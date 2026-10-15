#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use mb_engine::{Engine, EngineConfig, ScenarioSpec};
use mb_ext::{CallInfo, QuotaGuard, Rejection, Tenancy, UsageEvent, UsageSink};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider, Usage};
use mb_mcm::Bundle;
use mb_protocols::http_rest;
use mb_protocols::{MockRuntime, RequestRecord, RequestRecorder, ScenarioRuntime, StateHealth};
use mb_state::{
    MemoryStore, SessionInfo, SessionKey, StateChanged, StateError, StateStore, StoredResource,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower::ServiceExt;

fn orders() -> Arc<Bundle> {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

fn order(id: &str) -> Value {
    json!({"id": id, "customerId": "c1", "status": "pending", "total": 10})
}

fn runtime(provider: Option<Arc<dyn Provider>>, state: Arc<dyn StateStore>) -> MockRuntime {
    MockRuntime::new(
        "orders",
        orders(),
        Arc::new(Engine::new(provider, EngineConfig::default())),
        state,
    )
}

fn memory() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new(Duration::from_secs(3600)))
}

fn offline() -> MockRuntime {
    runtime(None, memory())
}

async fn send(router: &Router, req: Request<Body>) -> Response {
    router.clone().oneshot(req).await.unwrap()
}

fn get_with(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::get(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn shipped(operations: &[&str]) -> ScenarioRuntime {
    ScenarioRuntime {
        spec: ScenarioSpec {
            name: "shipped".into(),
            instructions: Some("Every order has already shipped.".into()),
            set: Some(json!({"status": "shipped"})),
        },
        status: None,
        latency: None,
        operations: operations.iter().map(|s| s.to_string()).collect(),
    }
}

#[tokio::test]
async fn invalid_session_header_400() {
    let router = http_rest::router(Arc::new(offline()));
    let long = "a".repeat(129);
    for bad in ["../x", long.as_str()] {
        let resp = send(
            &router,
            get_with("/orders/o1", &[("x-mockingbird-session", bad)]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "session {bad:?}");
        assert_eq!(
            resp.headers()["content-type"],
            "application/problem+json",
            "session {bad:?}"
        );
    }
    let ok = "a".repeat(128);
    for good in ["A-z_0.9", ok.as_str()] {
        let resp = send(
            &router,
            get_with("/orders/o1", &[("x-mockingbird-session", good)]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "session {good:?}");
    }
}

/// An empty (or blank) session header means the default session, as in P1.
#[tokio::test]
async fn empty_session_header_is_default_session() {
    let collect = Arc::new(Collect::default());
    let mut rt = offline();
    rt.recorder = Some(collect.clone());
    let router = http_rest::router(Arc::new(rt));
    for blank in ["", "  "] {
        let resp = send(
            &router,
            get_with("/orders/o1", &[("x-mockingbird-session", blank)]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "session {blank:?}");
    }
    let records = collect.records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|r| r.session == "default"));
}

#[tokio::test(start_paused = true)]
async fn scenario_header_applies_instructions_status_latency_and_set() {
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(order("o1").to_string())]));
    let mut rt = runtime(Some(provider.clone() as Arc<dyn Provider>), memory());
    let mut scenario = shipped(&[]);
    scenario.status = Some(200);
    scenario.latency = Some(Duration::from_millis(400));
    rt.scenarios.insert("shipped".into(), scenario);
    let router = http_rest::router(Arc::new(rt));

    let start = tokio::time::Instant::now();
    let resp = send(
        &router,
        get_with("/orders/o1", &[("x-mockingbird-scenario", "shipped")]),
    )
    .await;
    assert!(start.elapsed() >= Duration::from_millis(400));
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status"], "shipped");

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let prompt: String = requests[0]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(prompt.contains("<scenario name=\"shipped\">"), "{prompt}");
    assert!(prompt.contains("Every order has already shipped."));
}

#[tokio::test]
async fn unknown_scenario_400() {
    let router = http_rest::router(Arc::new(offline()));
    let resp = send(
        &router,
        get_with("/orders/o1", &[("x-mockingbird-scenario", "nope")]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["title"], "Unknown scenario");
}

#[tokio::test]
async fn scenario_scoped_to_operations() {
    let mut rt = offline();
    let mut scenario = shipped(&["listOrders"]);
    scenario.status = Some(404);
    rt.scenarios.insert("shipped".into(), scenario);
    let router = http_rest::router(Arc::new(rt));

    // getOrder is outside the scenario's operations: it is ignored.
    let resp = send(
        &router,
        get_with("/orders/o1", &[("x-mockingbird-scenario", "shipped")]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // listOrders is in scope but declares no 404: the scenario's status is
    // ignored for it.
    let resp = send(
        &router,
        get_with("/orders", &[("x-mockingbird-scenario", "shipped")]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// A scenario's status applies only to operations declaring it; elsewhere
/// the call is served normally, with the scenario's latency.
#[tokio::test(start_paused = true)]
async fn scenario_status_applies_only_where_declared() {
    let mut rt = offline();
    let mut scenario = shipped(&[]);
    scenario.status = Some(404);
    scenario.latency = Some(Duration::from_millis(300));
    rt.scenarios.insert("outage".into(), scenario);
    let router = http_rest::router(Arc::new(rt));
    let headers = [("x-mockingbird-scenario", "outage")];

    // getOrder declares 404.
    let start = tokio::time::Instant::now();
    let resp = send(&router, get_with("/orders/o1", &headers)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(start.elapsed() >= Duration::from_millis(300));

    // listOrders does not: normal 2xx, latency still applied.
    let start = tokio::time::Instant::now();
    let resp = send(&router, get_with("/orders", &headers)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(start.elapsed() >= Duration::from_millis(300));

    // The header override still rejects an undeclared status.
    let resp = send(
        &router,
        get_with("/orders", &[("x-mockingbird-status", "404")]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["title"], "Undeclared status override");
}

#[tokio::test(start_paused = true)]
async fn header_overrides_beat_scenario() {
    let mut rt = offline();
    let mut scenario = shipped(&[]);
    scenario.status = Some(404);
    scenario.latency = Some(Duration::from_secs(10));
    rt.scenarios.insert("shipped".into(), scenario);
    let router = http_rest::router(Arc::new(rt));

    let start = tokio::time::Instant::now();
    let resp = send(
        &router,
        get_with(
            "/orders/o1",
            &[
                ("x-mockingbird-scenario", "shipped"),
                ("x-mockingbird-status", "200"),
                ("x-mockingbird-latency", "50ms"),
            ],
        ),
    )
    .await;
    let elapsed = start.elapsed();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(elapsed >= Duration::from_millis(50));
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    // The scenario's `set` still applies.
    assert_eq!(body_json(resp).await["status"], "shipped");
}

struct SlowDown;

#[async_trait]
impl QuotaGuard for SlowDown {
    async fn admit(&self, _t: &Tenancy, _call: &CallInfo<'_>) -> Result<(), Rejection> {
        Err(Rejection {
            status: 429,
            title: "Too Many Requests".into(),
            detail: "quota exhausted".into(),
            retry_after: Some(Duration::from_secs(2)),
        })
    }
}

#[tokio::test]
async fn quota_rejection_maps_to_problem() {
    let mut rt = offline();
    rt.ext.quota = Arc::new(SlowDown);
    let router = http_rest::router(Arc::new(rt));
    let resp = send(&router, get_with("/orders/o1", &[])).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(resp.headers()["retry-after"], "2");
    assert_eq!(resp.headers()["content-type"], "application/problem+json");
    let body = body_json(resp).await;
    assert_eq!(body["title"], "Too Many Requests");
    assert_eq!(body["detail"], "quota exhausted");
}

/// Delegates to a memory store, failing every call while `failing` is set.
struct FlakyStore {
    inner: MemoryStore,
    failing: AtomicBool,
    calls: AtomicUsize,
}

impl FlakyStore {
    fn check(&self) -> Result<(), StateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.failing.load(Ordering::SeqCst) {
            Err(StateError::Backend("database is down".into()))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl StateStore for FlakyStore {
    async fn get(
        &self,
        s: &SessionKey,
        t: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        self.check()?;
        self.inner.get(s, t, id).await
    }
    async fn list(
        &self,
        s: &SessionKey,
        t: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        self.check()?;
        self.inner.list(s, t, limit).await
    }
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        self.check()?;
        self.inner.upsert(s, r).await
    }
    async fn delete(&self, s: &SessionKey, t: &str, id: &str) -> Result<bool, StateError> {
        self.check()?;
        self.inner.delete(s, t, id).await
    }
    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        self.check()?;
        self.inner.reset(s).await
    }
    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.check()?;
        self.inner.touch(s).await
    }
    async fn sessions(&self, w: &str, m: &str) -> Result<Vec<SessionInfo>, StateError> {
        self.check()?;
        self.inner.sessions(w, m).await
    }
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        self.check()?;
        self.inner.export(s).await
    }
    async fn seed(&self, s: &SessionKey, r: Vec<StoredResource>) -> Result<(), StateError> {
        self.check()?;
        self.inner.seed(s, r).await
    }
    async fn ping(&self) -> Result<(), StateError> {
        self.check()
    }
    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.inner.subscribe()
    }
}

#[tokio::test]
async fn state_errors_degrade_to_stateless() {
    let store = Arc::new(FlakyStore {
        inner: MemoryStore::new(Duration::from_secs(3600)),
        failing: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    });
    let mut rt = runtime(None, store.clone());
    // Retry the store on every call, so recovery is seen at once.
    rt.state_health = Arc::new(StateHealth::new(Duration::ZERO));
    let rt = Arc::new(rt);
    let router = http_rest::router(Arc::clone(&rt));

    let resp = send(&router, get_with("/orders/o1", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(rt.state_health.is_degraded());

    // A failing write is not a 5xx either.
    let req = Request::post("/orders")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"customerId":"c1","total":10}"#))
        .unwrap();
    assert_eq!(send(&router, req).await.status(), StatusCode::CREATED);
    assert!(rt.state_health.is_degraded());

    store.failing.store(false, Ordering::SeqCst);
    let resp = send(&router, get_with("/orders/o1", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!rt.state_health.is_degraded());
}

/// While degraded, the store is tried at most once per retry interval: the
/// other calls are served statelessly without touching it.
#[tokio::test]
async fn degraded_state_is_retried_once_per_interval() {
    let store = Arc::new(FlakyStore {
        inner: MemoryStore::new(Duration::from_secs(3600)),
        failing: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    });
    let rt = Arc::new(runtime(None, store.clone()));
    let router = http_rest::router(Arc::clone(&rt));

    let resp = send(&router, get_with("/orders/o1", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(rt.state_health.is_degraded());
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);

    for _ in 0..5 {
        let resp = send(&router, get_with("/orders/o1", &[])).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    assert_eq!(
        store.calls.load(Ordering::SeqCst),
        1,
        "calls within the retry interval must not touch the store"
    );
    assert!(rt.state_health.is_degraded());
}

/// A degraded mock's probe goes to the first call that actually uses the
/// store: a stateless call in the same interval does not consume it.
#[tokio::test]
async fn stateless_calls_do_not_consume_the_state_probe() {
    let store = Arc::new(FlakyStore {
        inner: MemoryStore::new(Duration::from_secs(3600)),
        failing: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    });
    // listOrders made stateless: no resource type, so nothing to list.
    let mut bundle = (*orders()).clone();
    for op in &mut bundle.operations {
        if op.id == "listOrders" {
            op.resource_type = None;
        }
    }
    let mut rt = MockRuntime::new(
        "orders",
        Arc::new(bundle),
        Arc::new(Engine::new(None, EngineConfig::default())),
        store.clone() as Arc<dyn StateStore>,
    );
    rt.state_health = Arc::new(StateHealth::new(Duration::from_millis(100)));
    let rt = Arc::new(rt);
    let router = http_rest::router(Arc::clone(&rt));

    let resp = send(&router, get_with("/orders/o1", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(rt.state_health.is_degraded());
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    store.failing.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // A stateless call: nothing to read or write.
    let resp = send(&router, get_with("/orders", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);

    // The stateful call in the same interval gets the probe.
    let resp = send(&router, get_with("/orders/o1", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(store.calls.load(Ordering::SeqCst) >= 2);
    assert!(!rt.state_health.is_degraded());
}

/// Counts `touch` calls on a memory store.
struct TouchSpy {
    inner: MemoryStore,
    touches: AtomicUsize,
}

#[async_trait]
impl StateStore for TouchSpy {
    async fn get(
        &self,
        s: &SessionKey,
        t: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        self.inner.get(s, t, id).await
    }
    async fn list(
        &self,
        s: &SessionKey,
        t: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        self.inner.list(s, t, limit).await
    }
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        self.inner.upsert(s, r).await
    }
    async fn delete(&self, s: &SessionKey, t: &str, id: &str) -> Result<bool, StateError> {
        self.inner.delete(s, t, id).await
    }
    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.reset(s).await
    }
    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.touches.fetch_add(1, Ordering::SeqCst);
        self.inner.touch(s).await
    }
    async fn sessions(&self, w: &str, m: &str) -> Result<Vec<SessionInfo>, StateError> {
        self.inner.sessions(w, m).await
    }
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        self.inner.export(s).await
    }
    async fn seed(&self, s: &SessionKey, r: Vec<StoredResource>) -> Result<(), StateError> {
        self.inner.seed(s, r).await
    }
    async fn ping(&self) -> Result<(), StateError> {
        Ok(())
    }
    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.inner.subscribe()
    }
}

/// A request that reads state touches its session exactly once (reads
/// themselves no longer refresh it); a write refreshes it itself, so a
/// request touches at most once.
#[tokio::test]
async fn a_request_touches_its_session_at_most_once() {
    let store = Arc::new(TouchSpy {
        inner: MemoryStore::new(Duration::from_secs(3600)),
        touches: AtomicUsize::new(0),
    });
    let router = http_rest::router(Arc::new(runtime(None, store.clone())));

    let req = Request::post("/orders")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"customerId":"c1","total":10}"#))
        .unwrap();
    assert_eq!(send(&router, req).await.status(), StatusCode::CREATED);
    assert!(store.touches.load(Ordering::SeqCst) <= 1);

    store.touches.store(0, Ordering::SeqCst);
    let resp = send(&router, get_with("/orders", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(store.touches.load(Ordering::SeqCst), 1);

    store.touches.store(0, Ordering::SeqCst);
    let resp = send(&router, get_with("/orders/o1", &[])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(store.touches.load(Ordering::SeqCst), 1);
}

#[derive(Default)]
struct Collect {
    records: Mutex<Vec<RequestRecord>>,
    usage: Mutex<Vec<UsageEvent>>,
}

impl RequestRecorder for Collect {
    fn record(&self, r: RequestRecord) {
        self.records.lock().unwrap().push(r);
    }
}

impl UsageSink for Collect {
    fn record(&self, event: &UsageEvent) {
        self.usage.lock().unwrap().push(event.clone());
    }
}

#[tokio::test]
async fn recorder_and_usage_receive_events() {
    let provider = Arc::new(ScriptedProvider::with_usage(vec![Ok((
        order("o1").to_string(),
        Usage {
            prompt_tokens: 120,
            completion_tokens: 30,
        },
    ))]));
    let collect = Arc::new(Collect::default());
    let mut rt = runtime(Some(provider as Arc<dyn Provider>), memory());
    rt.ext.usage = collect.clone();
    rt.recorder = Some(collect.clone());
    let router = http_rest::router(Arc::new(rt));

    let resp = send(
        &router,
        get_with(
            "/orders/o1?expand=items",
            &[
                ("x-mockingbird-session", "s1"),
                ("authorization", "Bearer hunter2"),
                ("cookie", "sid=1"),
                ("x-trace", "abc"),
            ],
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let records = collect.records.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r.mock, "orders");
    assert_eq!(r.operation_id.as_deref(), Some("getOrder"));
    assert_eq!(r.session, "s1");
    assert_eq!(r.method, "GET");
    assert_eq!(r.path, "/orders/o1");
    assert_eq!(r.status, 200);
    assert_eq!(r.source.as_deref(), Some("llm"));
    assert_eq!(r.prompt_tokens, 120);
    assert_eq!(r.completion_tokens, 30);
    assert_eq!(r.request["query"], json!({"expand": "items"}));
    let headers = r.request["headers"].as_object().unwrap();
    assert_eq!(headers["x-trace"], "abc");
    assert!(!headers.contains_key("authorization"));
    assert!(!headers.contains_key("cookie"));
    assert_eq!(r.response.as_ref().unwrap()["id"], "o1");

    let usage = collect.usage.lock().unwrap().clone();
    assert_eq!(usage.len(), 1);
    let u = &usage[0];
    assert_eq!(u.mock, "orders");
    assert_eq!(u.operation_id, "getOrder");
    assert_eq!(u.status, 200);
    assert_eq!(u.source, "llm");
    assert_eq!(u.prompt_tokens, 120);
    assert_eq!(u.completion_tokens, 30);
    assert_eq!(u.tenancy.tenant, "default");
}

#[tokio::test]
async fn unrouted_requests_are_recorded() {
    let collect = Arc::new(Collect::default());
    let mut rt = offline();
    rt.ext.usage = collect.clone();
    rt.recorder = Some(collect.clone());
    let router = http_rest::router(Arc::new(rt));

    let resp = send(&router, get_with("/nowhere", &[])).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let records = collect.records.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].operation_id, None);
    assert_eq!(records[0].status, 404);
    assert_eq!(records[0].session, "default");
    assert_eq!(records[0].source, None);
    assert_eq!(collect.usage.lock().unwrap().len(), 1);
}

#[test]
fn new_runtime_defaults() {
    let rt = offline();
    assert!(rt.header_overrides);
    assert!(rt.scenarios.is_empty());
    assert!(rt.recorder.is_none());
    assert!(!rt.state_health.is_degraded());
    let _: &HashMap<String, ScenarioRuntime> = &rt.scenarios;
}
