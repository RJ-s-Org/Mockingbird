#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mb_ext::Extensions;
use mb_protocols::{RequestRecord, RequestRecorder};
use mb_state::{
    MemoryStore, SessionInfo, SessionKey, SqlStateStore, StateChanged, StateError, StateStore,
    StoredResource,
};
use mb_store::{
    ContractStatus, ContractVersion, ControlChange, ControlStore, DEFAULT_WORKSPACE, LlmProfile,
    Mock, NewContractVersion, NewMock, NewRequestLogEntry, PgStore, RequestLogEntry,
    RequestLogFilter, ResourceSource, Scenario, SqliteControlStore, StoreError, Workspace,
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
        self.inner.list_all_mocks().await
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
    async fn reset_mock(&self, _: &str, _: &str) -> Result<(), StateError> {
        Err(down())
    }
    async fn touch(&self, _: &SessionKey) -> Result<(), StateError> {
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
                data_plane_auth: Default::default(),
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
        request_body_bytes: None,
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

/// The newest request-log entry of `mock`, waiting up to 2s for one.
async fn newest_entry(store: &Arc<dyn ControlStore>, mock: &Mock) -> RequestLogEntry {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let entries = store
            .list_request_log(
                mock.id,
                &RequestLogFilter {
                    limit: 1,
                    ..RequestLogFilter::default()
                },
            )
            .await
            .unwrap();
        if let Some(e) = entries.into_iter().next() {
            return e;
        }
        assert!(Instant::now() < deadline, "no request log entry within 2s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Serves `orders` with a [`StoreRecorder`] using the default settings.
async fn recorded_orders() -> (Arc<dyn ControlStore>, Mock, String) {
    let store: Arc<dyn ControlStore> = stub_store().await;
    let mock = orders_mock(&store).await;
    let recorder = StoreRecorder::new(store.clone(), 1000, vec!["password".into(), "token".into()]);
    let deps = deps(store.clone(), memory_state(), Some(recorder));
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let base = serve(registry.router()).await;
    (store, mock, base)
}

/// Form bodies are recorded decoded, so their fields are redacted too;
/// multipart bodies are not recorded.
#[tokio::test]
async fn request_log_redacts_form_bodies() {
    let (store, mock, base) = recorded_orders().await;
    let http = reqwest::Client::new();
    http.post(format!("{base}/orders/orders"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("customerId=c1&password=hunter2&tag=a&tag=b")
        .send()
        .await
        .unwrap();
    let entry = newest_entry(&store, &mock).await;
    let body = &entry.request["body"];
    assert_eq!(body["password"], "[redacted]", "{body}");
    assert_eq!(body["customerId"], "c1", "{body}");
    assert_eq!(body["tag"], json!(["a", "b"]), "{body}");
    assert!(!entry.request.to_string().contains("hunter2"));

    http.post(format!("{base}/orders/orders"))
        .header("content-type", "multipart/form-data; boundary=X")
        .body(
            "--X\r\nContent-Disposition: form-data; name=\"password\"\r\n\r\nhunter2\r\n--X--\r\n",
        )
        .send()
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let entry = loop {
        let e = newest_entry(&store, &mock).await;
        if e.request["body"] != body.clone() {
            break e;
        }
        assert!(Instant::now() < deadline, "multipart call not recorded");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(entry.request["body"], "[multipart omitted]");
    assert!(!entry.request.to_string().contains("hunter2"));
}

/// Request bodies over 64 KiB are replaced with a marker holding their
/// size and no (unredacted) preview.
#[tokio::test]
async fn request_log_truncates_large_bodies() {
    let (store, mock, base) = recorded_orders().await;
    let big = "x".repeat(70 * 1024);
    reqwest::Client::new()
        .post(format!("{base}/orders/orders"))
        .json(&json!({"password": "hunter2", "customerId": "c1", "total": 1, "notes": big}))
        .send()
        .await
        .unwrap();
    let entry = newest_entry(&store, &mock).await;
    let body = &entry.request["body"];
    assert_eq!(body["truncated"], true, "{}", &body.to_string()[..200]);
    assert!(body["bytes"].as_u64().unwrap() > 70 * 1024, "{body}");
    assert!(body.get("preview").is_none(), "{body}");
    assert!(!entry.request.to_string().contains("hunter2"));
}

/// `record` only queues: redacting and serializing a huge record happen on
/// the writer, which still stores and broadcasts it redacted and truncated.
#[tokio::test]
async fn record_does_no_heavy_work_on_the_request_path() {
    let store: Arc<dyn ControlStore> = stub_store().await;
    let mock = orders_mock(&store).await;
    let recorder = StoreRecorder::new(store.clone(), 1000, vec!["password".into(), "token".into()]);
    let mut live = recorder.live();

    let notes = "x".repeat(5 * 1024 * 1024);
    let raw_len = notes.len() + 40;
    let big_record = || {
        // A large response: many small objects, each with a redacted field.
        let response: Vec<Value> = (0..100_000)
            .map(|n| json!({"n": n, "token": "sekret"}))
            .collect();
        let mut r = record("orders", 1);
        r.request = json!({
            "query": {"access_token": "q"},
            "headers": {},
            "body": {"password": "hunter2", "notes": notes.clone()},
        });
        r.request_body_bytes = Some(raw_len);
        r.response = Some(Value::Array(response));
        r
    };

    // The fastest of a few calls, so a descheduled test thread under a
    // parallel test run does not fail it; serializing and redacting on
    // this path takes hundreds of milliseconds in a debug build.
    let mut fastest = Duration::MAX;
    for _ in 0..3 {
        let r = big_record();
        let started = Instant::now();
        recorder.record(r);
        fastest = fastest.min(started.elapsed());
    }
    assert!(
        fastest < Duration::from_millis(50),
        "record took {fastest:?}"
    );

    let entry = newest_entry(&store, &mock).await;
    let tailed = tokio::time::timeout(Duration::from_secs(5), live.recv())
        .await
        .unwrap()
        .unwrap();
    for (request, response) in [
        (&entry.request, entry.response.as_ref().unwrap()),
        (&tailed.request, tailed.response.as_ref().unwrap()),
    ] {
        assert_eq!(request["body"]["truncated"], true);
        assert_eq!(request["body"]["bytes"], raw_len);
        assert_eq!(request["query"]["access_token"], "[redacted]");
        assert!(!request.to_string().contains("hunter2"));
        assert_eq!(response["truncated"], true);
        let preview = response["preview"].as_str().unwrap();
        assert!(preview.len() <= 1024);
        assert!(preview.contains("[redacted]"), "{preview}");
        assert!(!response.to_string().contains("sekret"));
    }
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

/// A key is redacted when, normalized (lowercased, `_` `-` `.` removed),
/// it contains a normalized redact field.
#[test]
fn redact_matches_keys_containing_a_field() {
    let fields: Vec<String> = ["password", "token", "secret", "apiKey"]
        .map(String::from)
        .to_vec();
    let mut v = json!({
        "access_token": "a",
        "refresh_token": "r",
        "client_secret": "c",
        "new_password": "n",
        "x-api-key": "k",
        "API.KEY": "k2",
        "tokenizer_version": "v",
        "total": 3,
        "nested": [{"Access-Token": "t", "total": 4}]
    });
    mockingbird::recorder::redact(&mut v, &fields);
    assert_eq!(
        v,
        json!({
            "access_token": "[redacted]",
            "refresh_token": "[redacted]",
            "client_secret": "[redacted]",
            "new_password": "[redacted]",
            "x-api-key": "[redacted]",
            "API.KEY": "[redacted]",
            "tokenizer_version": "[redacted]",
            "total": 3,
            "nested": [{"Access-Token": "[redacted]", "total": 4}]
        })
    );
}

/// Form-body and query keys containing a redact field are redacted too.
#[tokio::test]
async fn request_log_redacts_compound_form_and_query_keys() {
    let (store, mock, base) = recorded_orders().await;
    reqwest::Client::new()
        .post(format!("{base}/orders/orders?access_token=q1&total=5"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("customerId=c1&new_password=hunter2&refresh_token=r1")
        .send()
        .await
        .unwrap();
    let entry = newest_entry(&store, &mock).await;
    let body = &entry.request["body"];
    assert_eq!(body["new_password"], "[redacted]", "{body}");
    assert_eq!(body["refresh_token"], "[redacted]", "{body}");
    assert_eq!(body["customerId"], "c1", "{body}");
    let query = &entry.request["query"];
    assert_eq!(query["access_token"], "[redacted]", "{query}");
    assert_eq!(query["total"], "5", "{query}");
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
    assert!(
        text.contains("# TYPE mb_request_duration_seconds histogram"),
        "{text}"
    );
    assert!(
        text.contains("mb_request_duration_seconds_bucket{")
            && text.contains("le=\"0.005\"")
            && text.contains("le=\"10\""),
        "{text}"
    );
}

#[test]
fn otlp_disabled_without_endpoint() {
    let guard = telemetry::init_with("mockingbird-test", None).unwrap();
    assert!(!guard.otlp_enabled());
}

/// A state store whose database is unreachable fails fast: a request is
/// served (statelessly) and `/readyz` answers within 3s, not after sqlx's
/// default 30s acquire timeout; later requests skip the store at once.
#[tokio::test]
async fn unreachable_state_database_fails_fast() {
    let store: Arc<dyn ControlStore> = stub_store().await;
    orders_mock(&store).await;
    let state: Arc<dyn StateStore> = Arc::new(
        SqlStateStore::connect_lazy("postgres://mb@127.0.0.1:1/x", Duration::from_secs(60))
            .unwrap(),
    );
    let deps = deps(store.clone(), state.clone(), None);
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let data = serve(registry.router()).await;
    let ops = serve(ops::router(ops_state(store, state, registry))).await;

    let started = Instant::now();
    let r = reqwest::get(format!("{data}/orders/orders/o1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "first request took {:?}",
        started.elapsed()
    );

    let started = Instant::now();
    let r = reqwest::get(format!("{data}/orders/orders/o2"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "a degraded mock must skip the store; took {:?}",
        started.elapsed()
    );

    let started = Instant::now();
    let r = reqwest::get(format!("{ops}/readyz")).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "readyz took {:?}",
        started.elapsed()
    );
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "degraded", "{body}");
}

/// `/readyz` reports an unreachable control database within 3s.
#[tokio::test]
async fn readyz_fails_fast_when_control_database_unreachable() {
    let store: Arc<dyn ControlStore> =
        Arc::new(PgStore::connect_lazy("postgres://mb@127.0.0.1:1/x").unwrap());
    let ops = serve(ops::router(ops_state(
        store,
        memory_state(),
        MockRegistry::new(),
    )))
    .await;
    let started = Instant::now();
    let r = reqwest::get(format!("{ops}/readyz")).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "readyz took {:?}",
        started.elapsed()
    );
    assert_eq!(r.status(), 503);
}

/// Graceful shutdown writes every queued request-log entry before it
/// returns, and gives up after its timeout when the store hangs.
#[tokio::test]
async fn recorder_shutdown_drains_queue() {
    let stub = stub_store().await;
    let store: Arc<dyn ControlStore> = stub.clone();
    let mock = orders_mock(&store).await;
    let recorder = StoreRecorder::new(store.clone(), 1000, Vec::new());
    for n in 0..200 {
        recorder.record(record("orders", n));
    }
    recorder.shutdown(Duration::from_secs(5)).await;
    let entries = store
        .list_request_log(
            mock.id,
            &RequestLogFilter {
                limit: 500,
                ..RequestLogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(entries.len(), 200);
    // Records after shutdown are dropped, not queued.
    recorder.record(record("orders", 999));
    assert_eq!(recorder.dropped(), 1);

    stub.stall_append.store(true, Ordering::Relaxed);
    let stalled = StoreRecorder::new(store, 1000, Vec::new());
    stalled.record(record("orders", 1));
    let started = Instant::now();
    stalled.shutdown(Duration::from_millis(200)).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}
