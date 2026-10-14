#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mb_ext::Extensions;
use mb_protocols::{RequestRecord, RequestRecorder};
use mb_state::{
    MemoryStore, SessionInfo, SessionKey, StateChanged, StateError, StateStore, StoredResource,
};
use mb_store::{
    ContractStatus, ContractVersion, ControlChange, ControlStore, DEFAULT_WORKSPACE, LlmProfile,
    Mock, NewContractVersion, NewMock, NewRequestLogEntry, RequestLogEntry, RequestLogFilter,
    ResourceSource, Scenario, SqliteControlStore, StoreError,
};
use mockingbird::ops::{self, OpsState};
use mockingbird::recorder::StoreRecorder;
use mockingbird::registry::MockRegistry;
use mockingbird::runtime::RuntimeDeps;
use mockingbird::telemetry;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

/// A SQLite control store whose `ping` can fail and whose
/// `append_request_log` can stall forever.
struct StubControl {
    inner: SqliteControlStore,
    ping_fails: AtomicBool,
    stall_append: AtomicBool,
}

#[async_trait]
impl ControlStore for StubControl {
    async fn ping(&self) -> Result<(), StoreError> {
        if self.ping_fails.load(Ordering::Relaxed) {
            return Err(StoreError::Db("connection refused".into()));
        }
        self.inner.ping().await
    }
    async fn list_mocks(&self, ws: &str) -> Result<Vec<Mock>, StoreError> {
        self.inner.list_mocks(ws).await
    }
    async fn get_mock(&self, ws: &str, name: &str) -> Result<Option<Mock>, StoreError> {
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
        if self.stall_append.load(Ordering::Relaxed) {
            std::future::pending::<()>().await;
        }
        self.inner.append_request_log(e, retention).await
    }
    async fn list_request_log(
        &self,
        mock_id: Uuid,
        f: &RequestLogFilter,
    ) -> Result<Vec<RequestLogEntry>, StoreError> {
        self.inner.list_request_log(mock_id, f).await
    }
    fn subscribe(&self) -> broadcast::Receiver<ControlChange> {
        self.inner.subscribe()
    }
}

/// A state store whose every call fails.
struct FailingState {
    changes: broadcast::Sender<StateChanged>,
}

fn down() -> StateError {
    StateError::Backend("state backend down".into())
}

#[async_trait]
impl StateStore for FailingState {
    async fn get(
        &self,
        _: &SessionKey,
        _: &str,
        _: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        Err(down())
    }
    async fn list(
        &self,
        _: &SessionKey,
        _: &str,
        _: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        Err(down())
    }
    async fn upsert(&self, _: &SessionKey, _: StoredResource) -> Result<(), StateError> {
        Err(down())
    }
    async fn delete(&self, _: &SessionKey, _: &str, _: &str) -> Result<bool, StateError> {
        Err(down())
    }
    async fn reset(&self, _: &SessionKey) -> Result<(), StateError> {
        Err(down())
    }
    async fn sessions(&self, _: &str, _: &str) -> Result<Vec<SessionInfo>, StateError> {
        Err(down())
    }
    async fn export(&self, _: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        Err(down())
    }
    async fn seed(&self, _: &SessionKey, _: Vec<StoredResource>) -> Result<(), StateError> {
        Err(down())
    }
    async fn ping(&self) -> Result<(), StateError> {
        Err(down())
    }
    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.changes.subscribe()
    }
}

fn failing_state() -> Arc<dyn StateStore> {
    Arc::new(FailingState {
        changes: broadcast::channel(16).0,
    })
}

fn memory_state() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new(Duration::from_secs(3600)))
}

async fn stub_store() -> Arc<StubControl> {
    Arc::new(StubControl {
        inner: SqliteControlStore::connect("sqlite::memory:")
            .await
            .unwrap(),
        ping_fails: AtomicBool::new(false),
        stall_append: AtomicBool::new(false),
    })
}

fn fixture_source() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/openapi/orders-3.0.yaml");
    std::fs::read_to_string(path).unwrap()
}

/// Creates mock `orders` at `/orders` with the fixture contract active.
async fn orders_mock(store: &Arc<dyn ControlStore>) -> Mock {
    let mock = store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: "orders".into(),
                route_prefix: Some("/orders".into()),
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
            },
        )
        .await
        .unwrap();
    let source = fixture_source();
    let bundle = mb_import::compile(source.as_bytes()).unwrap();
    let v = store
        .add_contract_version(
            mock.id,
            NewContractVersion {
                source: source.into_bytes(),
                bundle,
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await
        .unwrap();
    store
        .activate_contract_version(mock.id, v.version)
        .await
        .unwrap();
    mock
}

fn deps(
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    recorder: Option<Arc<dyn RequestRecorder>>,
) -> RuntimeDeps {
    RuntimeDeps {
        store,
        state,
        ext: Extensions::default(),
        recorder,
        offline: true,
        default_profile: None.into(),
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn ops_state(
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    registry: Arc<MockRegistry>,
) -> OpsState {
    OpsState {
        store,
        state,
        registry,
        prometheus: telemetry::install_prometheus().unwrap(),
    }
}

fn record(mock: &str, n: usize) -> RequestRecord {
    RequestRecord {
        mock: mock.into(),
        operation_id: Some("listOrders".into()),
        session: "default".into(),
        method: "GET".into(),
        path: format!("/orders/{n}"),
        status: 200,
        source: Some("faker".into()),
        latency: Duration::from_millis(1),
        prompt_tokens: 0,
        completion_tokens: 0,
        request: json!({"query": {}, "headers": {}, "body": null}),
        response: None,
    }
}

#[tokio::test]
async fn request_log_redacts_fields() {
    let store: Arc<dyn ControlStore> = stub_store().await;
    let mock = orders_mock(&store).await;
    let recorder = StoreRecorder::new(
        store.clone(),
        1000,
        [
            "password",
            "token",
            "secret",
            "authorization",
            "apiKey",
            "api_key",
        ]
        .map(String::from)
        .to_vec(),
    );
    let mut live = recorder.live();
    let deps = deps(store.clone(), memory_state(), Some(recorder.clone()));
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let base = serve(registry.router()).await;

    reqwest::Client::new()
        .post(format!("{base}/orders/orders"))
        .json(&json!({"password": "p", "nested": {"Token": "t"}, "ok": 1}))
        .send()
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    let entry = loop {
        let entries = store
            .list_request_log(
                mock.id,
                &RequestLogFilter {
                    operation_id: None,
                    session: None,
                    source: None,
                    limit: 10,
                    before_id: None,
                },
            )
            .await
            .unwrap();
        if let Some(e) = entries.into_iter().next() {
            break e;
        }
        assert!(Instant::now() < deadline, "no request log entry within 2s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let body = &entry.request["body"];
    assert_eq!(body["password"], "[redacted]");
    assert_eq!(body["nested"]["Token"], "[redacted]");
    assert_eq!(body["ok"], 1);
    assert_eq!(entry.method, "POST");

    let tailed = tokio::time::timeout(Duration::from_secs(2), live.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tailed.request["body"]["password"], "[redacted]");
}

#[test]
fn redact_matches_keys_case_insensitively_everywhere() {
    let mut v = json!({
        "API_KEY": "k",
        "list": [{"Secret": {"deep": 1}}, {"keep": "x"}],
        "keep": "y"
    });
    mockingbird::recorder::redact(&mut v, &["api_key".into(), "secret".into()]);
    assert_eq!(
        v,
        json!({
            "API_KEY": "[redacted]",
            "list": [{"Secret": "[redacted]"}, {"keep": "x"}],
            "keep": "y"
        })
    );
}

#[tokio::test]
async fn recorder_drops_when_full_without_blocking() {
    let stub = stub_store().await;
    stub.stall_append.store(true, Ordering::Relaxed);
    let store: Arc<dyn ControlStore> = stub.clone();
    orders_mock(&store).await;
    let recorder = StoreRecorder::new(store, 1000, vec!["password".into()]);

    let started = Instant::now();
    for n in 0..5000 {
        recorder.record(record("orders", n));
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "recording 5000 entries took {:?}",
        started.elapsed()
    );
    assert!(recorder.dropped() > 0, "nothing was dropped");
}

#[tokio::test]
async fn readyz_ready() {
    let store: Arc<dyn ControlStore> = stub_store().await;
    let base = serve(ops::router(ops_state(
        store,
        memory_state(),
        MockRegistry::new(),
    )))
    .await;

    let r = reqwest::get(format!("{base}/readyz")).await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "ready");

    let r = reqwest::get(format!("{base}/healthz")).await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body, json!({"status": "ok"}));
}

#[tokio::test]
async fn readyz_reports_degraded_state_store() {
    let store: Arc<dyn ControlStore> = stub_store().await;
    orders_mock(&store).await;
    let state = failing_state();
    let deps = deps(store.clone(), state.clone(), None);
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let data = serve(registry.router()).await;
    // A stateful call marks the mock's runtime degraded.
    reqwest::get(format!("{data}/orders/orders")).await.unwrap();
    assert_eq!(registry.degraded_mocks(), vec!["orders".to_string()]);

    let base = serve(ops::router(ops_state(store, state, registry))).await;
    let r = reqwest::get(format!("{base}/readyz")).await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "degraded");
    let reasons = body["reasons"].as_array().unwrap();
    assert!(
        reasons
            .iter()
            .any(|r| r.as_str().unwrap().contains("state store")),
        "{body}"
    );
    assert!(
        reasons
            .iter()
            .any(|r| r.as_str().unwrap().contains("orders")),
        "{body}"
    );
}

#[tokio::test]
async fn readyz_unavailable_when_control_store_down() {
    let stub = stub_store().await;
    stub.ping_fails.store(true, Ordering::Relaxed);
    let base = serve(ops::router(ops_state(
        stub,
        memory_state(),
        MockRegistry::new(),
    )))
    .await;

    let r = reqwest::get(format!("{base}/readyz")).await.unwrap();
    assert_eq!(r.status(), 503);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "unavailable");
    assert!(!body["reasons"].as_array().unwrap().is_empty(), "{body}");
}

#[tokio::test]
async fn metrics_exposes_request_counters() {
    let prometheus = telemetry::install_prometheus().unwrap();
    let store: Arc<dyn ControlStore> = stub_store().await;
    orders_mock(&store).await;
    let state = memory_state();
    let deps = deps(store.clone(), state.clone(), None);
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let data = serve(registry.router()).await;
    assert_eq!(
        reqwest::get(format!("{data}/orders/orders"))
            .await
            .unwrap()
            .status(),
        200
    );

    let base = serve(ops::router(OpsState {
        store,
        state,
        registry,
        prometheus,
    }))
    .await;
    let r = reqwest::get(format!("{base}/metrics")).await.unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.contains("mb_requests_total"), "{text}");
}

#[test]
fn otlp_disabled_without_endpoint() {
    // SAFETY: no other test in this binary reads or writes this variable.
    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
    let guard = telemetry::init("mockingbird-test").unwrap();
    assert!(!guard.otlp_enabled());
}
