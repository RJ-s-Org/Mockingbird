//! The runtime serves its active contract version's effective world
//! description: loaded when the mock is built, rebuilt when a new revision is
//! stored, and kept (or tier 0) with a retry while the store cannot read it.
#![cfg(test)]

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mb_engine::StreamConfig;
use mb_ext::Extensions;
use mb_mcm::Bundle;
use mb_mcm::world::Author;
use mb_protocols::ProtocolSettings;
use mb_protocols::events::EventDeps;
use mb_protocols::{RequestRecord, RequestRecorder};
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    AdminSession, ApiKey, AuditEvent, AuditFilter, ChannelSettings, ContractStatus,
    ContractVersion, ControlChange, ControlStore, DEFAULT_WORKSPACE, ImportJob, ImportOutcome,
    ImportProgress, LlmProfile, Mock, MockEventSettings, NewAuditEvent, NewContractVersion,
    NewImportJob, NewMock, NewRequestLogEntry, NewWorldRevision, RequestLogEntry, RequestLogFilter,
    ResourceSource, Scenario, SqliteControlStore, StoreError, Workspace, WorldRevision,
    WorldRevisionInfo,
};
use mockingbird::ha::HubEvent;
use mockingbird::registry::MockRegistry;
use mockingbird::runtime::RuntimeDeps;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::support::serve;

/// Widgets with no identity member tier 0 can find: a create is marked
/// `no_identity` until a description names one.
const NO_ID_CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Widgets, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
        "429": {description: slow, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /widgets/{widgetRef}:
    get:
      operationId: getWidget
      parameters: [{name: widgetRef, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}}
components:
  schemas:
    Widget:
      type: object
      required: [label]
      properties: {label: {type: string}, color: {type: string}}
    Problem:
      type: object
      properties: {message: {type: string}}
"##;

/// Things with an `id`, which tier 0 finds.
const THINGS: &str = r##"
openapi: 3.0.0
info: {title: Things, version: "1"}
paths:
  /things:
    post:
      operationId: createThing
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/ThingInput"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
  /things/{thingId}:
    parameters: [{name: thingId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getThing
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
components:
  schemas:
    Thing:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    ThingInput:
      type: object
      properties: {name: {type: string}}
"##;

/// The revision that names the widgets' identity member.
fn widget_identity() -> Value {
    json!({"version": 1, "types": {"widgets": {"identity": {"member": "label"}}}})
}

/// Takes one failure from `budget`: a positive count is used up one call at
/// a time, a negative one never runs out, zero fails nothing.
fn take_failure(budget: &AtomicI64) -> bool {
    budget
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| match n {
            0 => None,
            n if n < 0 => Some(n),
            n => Some(n - 1),
        })
        .is_ok()
}

/// A SQLite control store whose world and contract reads can fail, and that
/// counts them.
struct FlakyStore {
    inner: SqliteControlStore,
    /// Failures left for `latest_world_revision` (negative: always).
    fail_latest: AtomicI64,
    /// Failures left for `active_contract` (negative: always).
    fail_active: AtomicI64,
    /// Failures left for `get_mock` (negative: always).
    fail_get: AtomicI64,
    /// Failures left for `list_all_mocks` (negative: always).
    fail_list: AtomicI64,
    /// `active_contract` answers that the mock has no active version.
    no_active: AtomicBool,
    /// `active_contract` calls: one per build.
    active_calls: AtomicUsize,
    latest_calls: AtomicUsize,
    revision_calls: AtomicUsize,
}

#[async_trait]
impl ControlStore for FlakyStore {
    async fn ping(&self) -> Result<(), StoreError> {
        self.inner.ping().await
    }
    async fn list_workspaces(&self) -> Result<Vec<Workspace>, StoreError> {
        self.inner.list_workspaces().await
    }
    async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>, StoreError> {
        self.inner.get_workspace(name).await
    }
    async fn create_workspace(
        &self,
        name: &str,
        display_name: &str,
    ) -> Result<Workspace, StoreError> {
        self.inner.create_workspace(name, display_name).await
    }
    async fn update_workspace(
        &self,
        name: &str,
        display_name: &str,
    ) -> Result<Workspace, StoreError> {
        self.inner.update_workspace(name, display_name).await
    }
    async fn delete_workspace(&self, name: &str, force: bool) -> Result<bool, StoreError> {
        self.inner.delete_workspace(name, force).await
    }
    async fn list_all_mocks(&self) -> Result<Vec<Mock>, StoreError> {
        if take_failure(&self.fail_list) {
            return Err(StoreError::Db("pool timed out".into()));
        }
        self.inner.list_all_mocks().await
    }
    async fn list_mocks(&self, ws: &str) -> Result<Vec<Mock>, StoreError> {
        self.inner.list_mocks(ws).await
    }
    async fn get_mock(&self, ws: &str, name: &str) -> Result<Option<Mock>, StoreError> {
        if take_failure(&self.fail_get) {
            return Err(StoreError::Db("pool timed out".into()));
        }
        self.inner.get_mock(ws, name).await
    }
    async fn create_mock(&self, ws: &str, m: NewMock) -> Result<Mock, StoreError> {
        self.inner.create_mock(ws, m).await
    }
    async fn update_mock(&self, m: &Mock) -> Result<Mock, StoreError> {
        self.inner.update_mock(m).await
    }
    async fn delete_mock(&self, ws: &str, name: &str) -> Result<bool, StoreError> {
        self.inner.delete_mock(ws, name).await
    }
    async fn add_contract_version(
        &self,
        mock_id: Uuid,
        v: NewContractVersion,
    ) -> Result<ContractVersion, StoreError> {
        self.inner.add_contract_version(mock_id, v).await
    }
    async fn list_contract_versions(
        &self,
        mock_id: Uuid,
    ) -> Result<Vec<ContractVersion>, StoreError> {
        self.inner.list_contract_versions(mock_id).await
    }
    async fn get_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<Option<ContractVersion>, StoreError> {
        self.inner.get_contract_version(mock_id, version).await
    }
    async fn activate_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<ContractVersion, StoreError> {
        self.inner.activate_contract_version(mock_id, version).await
    }
    async fn active_contract(&self, mock_id: Uuid) -> Result<Option<ContractVersion>, StoreError> {
        self.active_calls.fetch_add(1, Ordering::SeqCst);
        if take_failure(&self.fail_active) {
            return Err(StoreError::Db("connection refused".into()));
        }
        if self.no_active.load(Ordering::SeqCst) {
            return Ok(None);
        }
        self.inner.active_contract(mock_id).await
    }
    async fn list_scenarios(&self, mock_id: Uuid) -> Result<Vec<Scenario>, StoreError> {
        self.inner.list_scenarios(mock_id).await
    }
    async fn put_scenario(&self, s: &Scenario) -> Result<(), StoreError> {
        self.inner.put_scenario(s).await
    }
    async fn delete_scenario(&self, mock_id: Uuid, name: &str) -> Result<bool, StoreError> {
        self.inner.delete_scenario(mock_id, name).await
    }
    async fn list_profiles(&self, ws: &str) -> Result<Vec<LlmProfile>, StoreError> {
        self.inner.list_profiles(ws).await
    }
    async fn get_profile(&self, ws: &str, name: &str) -> Result<Option<LlmProfile>, StoreError> {
        self.inner.get_profile(ws, name).await
    }
    async fn put_profile(&self, p: &LlmProfile) -> Result<(), StoreError> {
        self.inner.put_profile(p).await
    }
    async fn delete_profile(&self, ws: &str, name: &str) -> Result<bool, StoreError> {
        self.inner.delete_profile(ws, name).await
    }
    async fn append_request_log(
        &self,
        e: NewRequestLogEntry,
        retention: u32,
    ) -> Result<i64, StoreError> {
        self.inner.append_request_log(e, retention).await
    }
    async fn list_request_log(
        &self,
        mock_id: Uuid,
        f: &RequestLogFilter,
    ) -> Result<Vec<RequestLogEntry>, StoreError> {
        self.inner.list_request_log(mock_id, f).await
    }
    async fn get_request_log(&self, id: i64) -> Result<Option<RequestLogEntry>, StoreError> {
        self.inner.get_request_log(id).await
    }
    async fn add_token_usage(
        &self,
        mock_id: Uuid,
        day: chrono::NaiveDate,
        tokens: u64,
    ) -> Result<u64, StoreError> {
        self.inner.add_token_usage(mock_id, day, tokens).await
    }
    async fn token_usage(&self, mock_id: Uuid, day: chrono::NaiveDate) -> Result<u64, StoreError> {
        self.inner.token_usage(mock_id, day).await
    }
    async fn prune_token_usage(&self, before: chrono::NaiveDate) -> Result<u64, StoreError> {
        self.inner.prune_token_usage(before).await
    }
    async fn list_channel_settings(
        &self,
        mock_id: Uuid,
    ) -> Result<Vec<ChannelSettings>, StoreError> {
        self.inner.list_channel_settings(mock_id).await
    }
    async fn put_channel_settings(&self, s: &ChannelSettings) -> Result<(), StoreError> {
        self.inner.put_channel_settings(s).await
    }
    async fn delete_channel_settings(
        &self,
        mock_id: Uuid,
        channel_id: &str,
    ) -> Result<bool, StoreError> {
        self.inner
            .delete_channel_settings(mock_id, channel_id)
            .await
    }
    async fn get_event_settings(
        &self,
        mock_id: Uuid,
    ) -> Result<Option<MockEventSettings>, StoreError> {
        self.inner.get_event_settings(mock_id).await
    }
    async fn put_event_settings(&self, s: &MockEventSettings) -> Result<(), StoreError> {
        self.inner.put_event_settings(s).await
    }
    async fn acquire_scheduler_lease(
        &self,
        mock_id: Uuid,
        holder: &str,
        ttl: Duration,
    ) -> Result<bool, StoreError> {
        self.inner
            .acquire_scheduler_lease(mock_id, holder, ttl)
            .await
    }
    async fn create_api_key(&self, k: &ApiKey) -> Result<(), StoreError> {
        self.inner.create_api_key(k).await
    }
    async fn get_api_key(&self, key_id: &str) -> Result<Option<ApiKey>, StoreError> {
        self.inner.get_api_key(key_id).await
    }
    async fn list_api_keys(&self, ws: &str) -> Result<Vec<ApiKey>, StoreError> {
        self.inner.list_api_keys(ws).await
    }
    async fn revoke_api_key(&self, ws: &str, key_id: &str) -> Result<bool, StoreError> {
        self.inner.revoke_api_key(ws, key_id).await
    }
    async fn touch_api_key(&self, key_id: &str, at: DateTime<Utc>) -> Result<(), StoreError> {
        self.inner.touch_api_key(key_id, at).await
    }
    async fn create_admin_session(&self, s: &AdminSession) -> Result<(), StoreError> {
        self.inner.create_admin_session(s).await
    }
    async fn get_admin_session(&self, id_hash: &str) -> Result<Option<AdminSession>, StoreError> {
        self.inner.get_admin_session(id_hash).await
    }
    async fn touch_admin_session(
        &self,
        id_hash: &str,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.inner.touch_admin_session(id_hash, at).await
    }
    async fn delete_admin_session(&self, id_hash: &str) -> Result<bool, StoreError> {
        self.inner.delete_admin_session(id_hash).await
    }
    async fn delete_expired_admin_sessions(&self, now: DateTime<Utc>) -> Result<u64, StoreError> {
        self.inner.delete_expired_admin_sessions(now).await
    }
    async fn append_audit(&self, e: NewAuditEvent) -> Result<AuditEvent, StoreError> {
        self.inner.append_audit(e).await
    }
    async fn list_audit(&self, f: &AuditFilter) -> Result<Vec<AuditEvent>, StoreError> {
        self.inner.list_audit(f).await
    }
    async fn prune_audit(&self, older_than: DateTime<Utc>) -> Result<u64, StoreError> {
        self.inner.prune_audit(older_than).await
    }
    async fn add_draft_contract_version(
        &self,
        mock_id: Uuid,
        v: NewContractVersion,
        draft_meta: Value,
    ) -> Result<ContractVersion, StoreError> {
        self.inner
            .add_draft_contract_version(mock_id, v, draft_meta)
            .await
    }
    async fn update_draft_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
        source: Vec<u8>,
        bundle: Bundle,
        draft_meta: Value,
    ) -> Result<ContractVersion, StoreError> {
        self.inner
            .update_draft_contract_version(mock_id, version, source, bundle, draft_meta)
            .await
    }
    async fn approve_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<ContractVersion, StoreError> {
        self.inner.approve_contract_version(mock_id, version).await
    }
    async fn set_contract_version_warnings(
        &self,
        mock_id: Uuid,
        version: i64,
        warnings: Value,
    ) -> Result<ContractVersion, StoreError> {
        self.inner
            .set_contract_version_warnings(mock_id, version, warnings)
            .await
    }
    async fn create_import_job(&self, j: NewImportJob) -> Result<ImportJob, StoreError> {
        self.inner.create_import_job(j).await
    }
    async fn get_import_job(&self, id: Uuid) -> Result<Option<ImportJob>, StoreError> {
        self.inner.get_import_job(id).await
    }
    async fn import_job_input(&self, id: Uuid) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.import_job_input(id).await
    }
    async fn count_active_import_jobs(&self, mock_id: Uuid) -> Result<u64, StoreError> {
        self.inner.count_active_import_jobs(mock_id).await
    }
    async fn list_import_jobs(
        &self,
        mock_id: Uuid,
        limit: u32,
    ) -> Result<Vec<ImportJob>, StoreError> {
        self.inner.list_import_jobs(mock_id, limit).await
    }
    async fn claim_import_job(&self, now: DateTime<Utc>) -> Result<Option<ImportJob>, StoreError> {
        self.inner.claim_import_job(now).await
    }
    async fn update_import_progress(
        &self,
        id: Uuid,
        p: &ImportProgress,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        self.inner.update_import_progress(id, p, now).await
    }
    async fn finish_import_job(
        &self,
        id: Uuid,
        outcome: ImportOutcome,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        self.inner.finish_import_job(id, outcome, now).await
    }
    async fn cancel_import_job(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, StoreError> {
        self.inner.cancel_import_job(id, now).await
    }
    async fn fail_stale_import_jobs(
        &self,
        updated_before: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.inner.fail_stale_import_jobs(updated_before, now).await
    }
    async fn prune_import_jobs(&self, finished_before: DateTime<Utc>) -> Result<u64, StoreError> {
        self.inner.prune_import_jobs(finished_before).await
    }
    fn subscribe(&self) -> broadcast::Receiver<ControlChange> {
        self.inner.subscribe()
    }
    async fn add_world_revision(
        &self,
        mock_id: Uuid,
        version: i64,
        r: NewWorldRevision,
    ) -> Result<WorldRevision, StoreError> {
        self.inner.add_world_revision(mock_id, version, r).await
    }
    async fn list_world_revisions(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<Vec<WorldRevisionInfo>, StoreError> {
        self.inner.list_world_revisions(mock_id, version).await
    }
    async fn get_world_revision(
        &self,
        mock_id: Uuid,
        version: i64,
        revision: i64,
    ) -> Result<WorldRevision, StoreError> {
        self.revision_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .get_world_revision(mock_id, version, revision)
            .await
    }
    async fn latest_world_revision(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<Option<WorldRevision>, StoreError> {
        self.latest_calls.fetch_add(1, Ordering::SeqCst);
        if take_failure(&self.fail_latest) {
            return Err(StoreError::Db("connection refused".into()));
        }
        self.inner.latest_world_revision(mock_id, version).await
    }
}
/// Keeps every request record.
#[derive(Default)]
struct Captured(Mutex<Vec<RequestRecord>>);

impl RequestRecorder for Captured {
    fn record(&self, r: RequestRecord) {
        if let Ok(mut records) = self.0.lock() {
            records.push(r);
        }
    }
}

impl Captured {
    /// The `world_gap` of the newest record of `operation`, waiting for it.
    async fn last_gap(&self, operation: &str) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let found = self.0.lock().ok().and_then(|records| {
                records
                    .iter()
                    .rev()
                    .find(|r| r.operation_id.as_deref() == Some(operation))
                    .map(|r| r.request.get("world_gap").cloned())
            });
            if let Some(gap) = found {
                return gap;
            }
            assert!(Instant::now() < deadline, "no record of {operation}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn clear(&self) {
        if let Ok(mut records) = self.0.lock() {
            records.clear();
        }
    }
}

/// A mock `m` at `/m` over a [`FlakyStore`].
struct Env {
    store: Arc<FlakyStore>,
    registry: Arc<MockRegistry>,
    deps: RuntimeDeps,
    records: Arc<Captured>,
    mock: Mock,
    data: String,
    _watcher: Option<tokio::task::JoinHandle<()>>,
}

impl Env {
    /// The mock with `contract` active, its world revisions `revisions`
    /// stored, nothing built yet.
    async fn stored(contract: &str, revisions: &[Value]) -> Self {
        let store = Arc::new(FlakyStore {
            inner: SqliteControlStore::connect("sqlite::memory:")
                .await
                .unwrap(),
            fail_latest: AtomicI64::new(0),
            fail_active: AtomicI64::new(0),
            fail_get: AtomicI64::new(0),
            fail_list: AtomicI64::new(0),
            no_active: AtomicBool::new(false),
            active_calls: AtomicUsize::new(0),
            latest_calls: AtomicUsize::new(0),
            revision_calls: AtomicUsize::new(0),
        });
        let dyn_store: Arc<dyn ControlStore> = store.clone();
        let mock = dyn_store
            .create_mock(
                DEFAULT_WORKSPACE,
                NewMock {
                    name: "m".into(),
                    route_prefix: Some("/m".into()),
                    persona: None,
                    llm_profile: None,
                    header_overrides: true,
                    source: ResourceSource::Api,
                    chaos: None,
                    limits: Default::default(),
                    data_plane_auth: Default::default(),
                    hosts: vec![],
                    behavior: Default::default(),
                    webhooks: Vec::new(),
                },
            )
            .await
            .unwrap();
        add_version(&dyn_store, &mock, contract, ContractStatus::Active).await;
        for doc in revisions {
            revise(&dyn_store, &mock, 1, doc.clone()).await;
        }
        let records = Arc::new(Captured::default());
        let recorder: Arc<dyn RequestRecorder> = records.clone();
        let deps = RuntimeDeps {
            store: dyn_store,
            state: Arc::new(MemoryStore::new(Duration::from_secs(3600))) as Arc<dyn StateStore>,
            ext: Extensions::default(),
            recorder: Some(recorder),
            offline: true,
            default_profile: None.into(),
            data_plane_keys: None,
            protocols: ProtocolSettings::default(),
            stream: StreamConfig::default(),
            events: EventDeps::default(),
            limiter: mb_engine::GenerationLimiter::new_shared(64),
            scrubber: None,
            limits: Default::default(),
            http: Default::default(),
            mode: mockingbird::runtime::ServeMode::Full,
            webhook_log: Default::default(),
            record_pools: Default::default(),
            shares_groups: Default::default(),
        };
        let registry = MockRegistry::new();
        let data = serve(registry.router()).await;
        Self {
            store,
            registry,
            deps,
            records,
            mock,
            data,
            _watcher: None,
        }
    }

    /// Builds every mock and rebuilds them on the store's changes.
    async fn start(mut self) -> Self {
        let (tx, rx) = broadcast::channel(256);
        let mut changes = self.deps.store.subscribe();
        tokio::spawn(async move {
            while let Ok(change) = changes.recv().await {
                if tx.send(HubEvent::Change(change)).is_err() {
                    break;
                }
            }
        });
        self.registry.reload_all(&self.deps).await.unwrap();
        self._watcher = Some(self.registry.spawn_watcher(self.deps.clone(), rx));
        self
    }

    fn store(&self) -> &Arc<dyn ControlStore> {
        &self.deps.store
    }

    fn status(&self) -> Option<(i64, bool)> {
        self.registry.world_status(DEFAULT_WORKSPACE, "m")
    }

    /// Waits until the mock's world status is `want`.
    async fn wait_status(&self, want: Option<(i64, bool)>, within: Duration) {
        let deadline = Instant::now() + within;
        while self.status() != want {
            assert!(
                Instant::now() < deadline,
                "world status {:?}, wanted {want:?} within {within:?}",
                self.status()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn builds(&self) -> usize {
        self.store.active_calls.load(Ordering::SeqCst)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        session: &str,
        body: Option<Value>,
        scenario: Option<&str>,
    ) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .request(method, format!("{}/m{path}", self.data))
            .header("x-mockingbird-session", session);
        if let Some(body) = body {
            req = req.json(&body);
        }
        if let Some(s) = scenario {
            req = req.header("x-mockingbird-scenario", s);
        }
        req.send().await.unwrap()
    }

    /// Creates a widget and answers the `world_gap` its record carries.
    async fn create_widget_gap(&self) -> Option<Value> {
        self.records.clear();
        let r = self
            .send(
                reqwest::Method::POST,
                "/widgets",
                "s",
                Some(json!({"label": "a"})),
                None,
            )
            .await;
        assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
        self.records.last_gap("createWidget").await
    }
}

async fn add_version(
    store: &Arc<dyn ControlStore>,
    mock: &Mock,
    src: &str,
    status: ContractStatus,
) {
    let bundle: Bundle = mb_import::compile(src.as_bytes()).unwrap();
    store
        .add_contract_version(
            mock.id,
            NewContractVersion {
                source: src.as_bytes().to_vec(),
                bundle,
                status,
                created_by: None,
            },
        )
        .await
        .unwrap();
}

async fn revise(store: &Arc<dyn ControlStore>, mock: &Mock, version: i64, document: Value) -> i64 {
    store
        .add_world_revision(
            mock.id,
            version,
            NewWorldRevision {
                author: Author::Agent,
                document,
                created_by: None,
                expect: None,
            },
        )
        .await
        .unwrap()
        .revision
}

const SOON: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread")]
async fn a_world_revision_applies_without_restart() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await.start().await;
    assert_eq!(env.status(), Some((0, false)));
    assert_eq!(env.create_widget_gap().await, Some(json!(["no_identity"])));

    revise(env.store(), &env.mock, 1, widget_identity()).await;
    env.wait_status(Some((1, false)), SOON).await;
    assert_eq!(env.create_widget_gap().await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_world_revision_mid_session_keeps_stored_records() {
    let env = Env::stored(THINGS, &[]).await.start().await;
    let r = env
        .send(
            reqwest::Method::POST,
            "/things",
            "s",
            Some(json!({"name": "a"})),
            None,
        )
        .await;
    assert_eq!(r.status(), 201);
    let created: Value = r.json().await.unwrap();
    let id = created["id"].as_str().unwrap().to_owned();

    revise(env.store(), &env.mock, 1, json!({"version": 1})).await;
    env.wait_status(Some((1, false)), SOON).await;

    let r = env
        .send(
            reqwest::Method::GET,
            &format!("/things/{id}"),
            "s",
            None,
            None,
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-mockingbird-source"], "state");
    let read: Value = r.json().await.unwrap();
    assert_eq!(read, created);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_error_on_the_first_build_serves_tier0_and_retries() {
    let env = Env::stored(NO_ID_CONTRACT, &[widget_identity()]).await;
    env.store.fail_latest.store(2, Ordering::SeqCst);
    let env = env.start().await;
    assert_eq!(env.status(), Some((0, true)));
    // The marks a tier-0 runtime gives.
    assert_eq!(env.create_widget_gap().await, Some(json!(["no_identity"])));
    // Retried after 1 s (which fails again), then after 2 s more.
    env.wait_status(Some((1, false)), Duration::from_secs(10))
        .await;
    assert_eq!(env.create_widget_gap().await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_error_on_a_rebuild_keeps_the_previous_world() {
    let env = Env::stored(NO_ID_CONTRACT, &[widget_identity()])
        .await
        .start()
        .await;
    assert_eq!(env.status(), Some((1, false)));
    env.store.fail_latest.store(-1, Ordering::SeqCst);
    env.store()
        .put_scenario(&Scenario {
            mock_id: env.mock.id,
            name: "slow".into(),
            instructions: None,
            rules: serde_json::from_value(json!({"status": 429, "operations": ["createWidget"]}))
                .unwrap(),
            source: ResourceSource::Api,
        })
        .await
        .unwrap();
    env.wait_status(Some((1, true)), SOON).await;
    // Revision 1's world is still served.
    assert_eq!(env.create_widget_gap().await, None);
    // The scenario change is in effect.
    let r = env
        .send(
            reqwest::Method::POST,
            "/widgets",
            "s",
            Some(json!({"label": "b"})),
            Some("slow"),
        )
        .await;
    assert_eq!(r.status(), 429);
    env.store.fail_latest.store(0, Ordering::SeqCst);
    env.wait_status(Some((1, false)), Duration::from_secs(10))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_whose_build_fails_is_rescheduled() {
    let env = Env::stored(NO_ID_CONTRACT, &[widget_identity()]).await;
    env.store.fail_latest.store(1, Ordering::SeqCst);
    let env = env.start().await;
    assert_eq!(env.status(), Some((0, true)));
    assert_eq!(env.builds(), 1);
    // The next two builds (the retries after 1 s and 2 s more) fail before
    // the world read; the pending runtime stays mounted.
    env.store.fail_active.store(2, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    while env.builds() < 2 {
        assert!(Instant::now() < deadline, "no retry within 5s");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(env.status(), Some((0, true)));
    env.wait_status(Some((1, false)), Duration::from_secs(15))
        .await;
    assert_eq!(env.builds(), 4, "two failed retries and one that loaded");
    assert_eq!(env.create_widget_gap().await, None);
}

/// A first build that fails mounts nothing; the retry mounts the mock
/// without a config change.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_first_build_is_retried() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await;
    env.store.fail_active.store(1, Ordering::SeqCst);
    let env = env.start().await;
    assert_eq!(env.status(), None);
    assert_eq!(env.registry.failed_mocks().len(), 1);
    env.wait_status(Some((0, false)), SOON).await;
    assert_eq!(env.builds(), 2, "one failed build and its retry");
    assert!(env.registry.failed_mocks().is_empty());
    assert_eq!(env.create_widget_gap().await, Some(json!(["no_identity"])));
}

/// A change whose mock cannot be loaded from the store (a transient store error) is
/// applied by a retry, without another change.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_load_is_retried() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await.start().await;
    assert_eq!(env.builds(), 1);
    env.store.fail_get.store(1, Ordering::SeqCst);
    revise(env.store(), &env.mock, 1, widget_identity()).await;
    env.wait_status(Some((1, false)), Duration::from_secs(10))
        .await;
    assert_eq!(
        env.store.fail_get.load(Ordering::SeqCst),
        0,
        "the load failed once"
    );
    assert_eq!(env.create_widget_gap().await, None);
}

/// A reload of every mock that fails (a transient store error) is tried again
/// without another change.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_reload_of_all_is_retried() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await;
    env.registry.reload_all(&env.deps).await.unwrap();
    let (tx, rx) = broadcast::channel(16);
    let _watcher = env.registry.spawn_watcher(env.deps.clone(), rx);
    // A change the watcher is not told of, then a resync whose reload fails.
    revise(env.store(), &env.mock, 1, widget_identity()).await;
    env.store.fail_list.store(1, Ordering::SeqCst);
    tx.send(HubEvent::Resync).unwrap();
    env.wait_status(Some((1, false)), Duration::from_secs(10))
        .await;
    assert_eq!(
        env.store.fail_list.load(Ordering::SeqCst),
        0,
        "the first reload failed"
    );
}

/// A config reload that fails applying the config (a transient store error) is
/// tried again without another edit.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_config_reload_is_retried() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("w.yaml"), NO_ID_CONTRACT).unwrap();
    let config = dir.path().join("mockingbird.yaml");
    std::fs::write(
        &config,
        "mocks:\n  w:\n    contract: w.yaml\n    route_prefix: /w\n",
    )
    .unwrap();
    // The watch's first read of the config fails to apply it.
    env.store.fail_list.store(1, Ordering::SeqCst);
    let _watch = mockingbird::watch::spawn_with(
        config,
        env.deps.store.clone(),
        env.deps.state.clone(),
        Arc::new(|_: &mut mockingbird::file_config::FileConfig| Ok(())),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while env
        .store()
        .get_mock(DEFAULT_WORKSPACE, "w")
        .await
        .unwrap()
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "the failed reload was not retried"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        env.store.fail_list.load(Ordering::SeqCst),
        0,
        "the first apply failed"
    );
}

/// A rebuild that fails keeps the previous runtime and is retried until it
/// succeeds; then the retries stop.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_rebuild_is_retried() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await.start().await;
    assert_eq!(env.builds(), 1);
    env.store.fail_active.store(2, Ordering::SeqCst);
    revise(env.store(), &env.mock, 1, widget_identity()).await;
    let deadline = Instant::now() + SOON;
    while env.registry.failed_mocks().is_empty() {
        assert!(Instant::now() < deadline, "the rebuild did not fail");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The previous runtime still answers.
    assert_eq!(env.status(), Some((0, false)));
    // Retried after 1 s (which fails again), then after 2 s more.
    env.wait_status(Some((1, false)), Duration::from_secs(10))
        .await;
    assert!(env.registry.failed_mocks().is_empty());
    assert_eq!(env.builds(), 4);
    assert_eq!(env.create_widget_gap().await, None);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(env.builds(), 4, "no retry after a build succeeded");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_retry_stops_when_the_mock_is_deleted() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await;
    env.store.fail_latest.store(-1, Ordering::SeqCst);
    let env = env.start().await;
    assert_eq!(env.status(), Some((0, true)));
    env.store()
        .delete_mock(DEFAULT_WORKSPACE, "m")
        .await
        .unwrap();
    env.wait_status(None, SOON).await;
    let builds = env.builds();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(env.builds(), builds, "no retry after the mock is gone");
    assert_eq!(env.status(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_retry_stops_when_the_mock_has_no_active_version() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await;
    env.store.fail_latest.store(-1, Ordering::SeqCst);
    let env = env.start().await;
    assert_eq!(env.status(), Some((0, true)));
    env.store.no_active.store(true, Ordering::SeqCst);
    // Any change of the mock rebuilds it: the build finds no active version.
    env.store()
        .put_scenario(&Scenario {
            mock_id: env.mock.id,
            name: "any".into(),
            instructions: None,
            rules: serde_json::from_value(json!({"status": 429})).unwrap(),
            source: ResourceSource::Api,
        })
        .await
        .unwrap();
    env.wait_status(None, SOON).await;
    let builds = env.builds();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(env.builds(), builds, "no retry after the mock is unmounted");
    assert_eq!(env.status(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_version_change_replaces_the_pending_retry() {
    let env = Env::stored(NO_ID_CONTRACT, &[widget_identity()]).await;
    env.store.fail_latest.store(1, Ordering::SeqCst);
    let env = env.start().await;
    assert_eq!(env.status(), Some((0, true)));
    // Version 2 is added active (one change) while the store has recovered.
    add_version(
        env.store(),
        &env.mock,
        NO_ID_CONTRACT,
        ContractStatus::Active,
    )
    .await;
    env.wait_status(Some((0, false)), SOON).await;
    assert_eq!(env.builds(), 2);
    // Past the version-1 retry's time: it did nothing.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(env.builds(), 2, "the stale retry built nothing");
    assert_eq!(env.status(), Some((0, false)));
    revise(env.store(), &env.mock, 2, widget_identity()).await;
    env.wait_status(Some((1, false)), SOON).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revision_that_is_not_a_world_serves_tier0_without_retrying() {
    let env = Env::stored(NO_ID_CONTRACT, &[json!({"types": 5})])
        .await
        .start()
        .await;
    assert_eq!(env.status(), Some((1, false)));
    assert_eq!(env.create_widget_gap().await, Some(json!(["no_identity"])));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(env.builds(), 1, "no retry");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rebuild_reads_only_the_newest_revision() {
    let docs: Vec<Value> = (0..5).map(|_| widget_identity()).collect();
    let env = Env::stored(NO_ID_CONTRACT, &docs).await.start().await;
    assert_eq!(env.status(), Some((5, false)));
    assert_eq!(env.store.latest_calls.load(Ordering::SeqCst), 1);
    assert_eq!(env.store.revision_calls.load(Ordering::SeqCst), 0);
}

/// A world write can wait until the running mock serves its revision.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_for_a_revision_returns_once_it_is_mounted() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await.start().await;
    let revision = revise(env.store(), &env.mock, 1, widget_identity()).await;
    let mount = env
        .registry
        .wait_world(DEFAULT_WORKSPACE, "m", 1, revision, SOON)
        .await;
    assert_eq!(mount, mb_admin::WorldMount::Mounted);
    // Served at once: the next call already uses the new world.
    assert_eq!(env.status(), Some((revision, false)));
    assert_eq!(env.create_widget_gap().await, None);
}

/// A revision that does not land in time is pending; a mock that is not
/// mounted is not served.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_for_a_revision_that_does_not_land_times_out() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await.start().await;
    let short = Duration::from_millis(200);
    let mount = env
        .registry
        .wait_world(DEFAULT_WORKSPACE, "m", 1, 7, short)
        .await;
    assert_eq!(mount, mb_admin::WorldMount::Pending);
    let mount = env
        .registry
        .wait_world(DEFAULT_WORKSPACE, "other", 1, 1, short)
        .await;
    assert_eq!(mount, mb_admin::WorldMount::NotServed);
    // Another contract version is not this one.
    let mount = env
        .registry
        .wait_world(DEFAULT_WORKSPACE, "m", 2, 0, short)
        .await;
    assert_eq!(mount, mb_admin::WorldMount::Pending);
}

/// A world that failed to load is a fallback, not the revision: the wait
/// lasts until the retry loads it.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_for_a_revision_skips_a_fallback_build() {
    let env = Env::stored(NO_ID_CONTRACT, &[]).await.start().await;
    env.store.fail_latest.store(1, Ordering::SeqCst);
    let revision = revise(env.store(), &env.mock, 1, widget_identity()).await;
    let mount = env
        .registry
        .wait_world(DEFAULT_WORKSPACE, "m", 1, revision, Duration::from_secs(8))
        .await;
    assert_eq!(mount, mb_admin::WorldMount::Mounted);
    assert_eq!(env.status(), Some((revision, false)));
}
