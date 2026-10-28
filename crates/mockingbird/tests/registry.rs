#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mb_engine::StreamConfig;
use mb_ext::{ExtError, Extensions, SecretStore};
use mb_protocols::ProtocolSettings;
use mb_protocols::events::EventDeps;
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    Consistency, ContractStatus, ControlStore, DEFAULT_WORKSPACE, LlmProfile, Mock, MockBehavior,
    NewContractVersion, NewMock, ResourceSource, SqliteControlStore, UnknownIds,
};
use mockingbird::registry::MockRegistry;
use mockingbird::runtime::{RuntimeDeps, build_provider};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{MockServer, ResponseTemplate};

const PING_PATH: &str = r#"  /ping:
    get:
      operationId: ping
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                type: object
                properties:
                  ok:
                    type: boolean
"#;

fn fixture_source() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/openapi/orders-3.0.yaml");
    std::fs::read_to_string(path).unwrap()
}

/// The fixture with an extra `GET /ping` operation.
fn fixture_v2_source() -> String {
    fixture_source().replacen("paths:\n", &format!("paths:\n{PING_PATH}"), 1)
}

async fn store() -> Arc<dyn ControlStore> {
    Arc::new(
        SqliteControlStore::connect("sqlite::memory:")
            .await
            .unwrap(),
    )
}

fn deps(store: Arc<dyn ControlStore>) -> RuntimeDeps {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    RuntimeDeps {
        store,
        state,
        ext: Extensions::default(),
        recorder: None,
        offline: true,
        default_profile: None.into(),
        data_plane_keys: None,
        protocols: ProtocolSettings::default(),
        stream: StreamConfig::default(),
        events: EventDeps::default(),
        limiter: mb_engine::GenerationLimiter::new(64),
        scrubber: None,
        limits: Default::default(),
        http: Default::default(),
        mode: mockingbird::runtime::ServeMode::Full,
        webhook_log: Default::default(),
    }
}

async fn create_mock(
    store: &Arc<dyn ControlStore>,
    name: &str,
    prefix: &str,
    profile: Option<&str>,
) -> Mock {
    store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: name.into(),
                route_prefix: Some(prefix.into()),
                persona: None,
                llm_profile: profile.map(str::to_string),
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
        .unwrap()
}

async fn create_hosted(
    store: &Arc<dyn ControlStore>,
    name: &str,
    prefix: &str,
    hosts: &[&str],
) -> Mock {
    store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: name.into(),
                route_prefix: Some(prefix.into()),
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: Default::default(),
                hosts: hosts.iter().map(|h| h.to_string()).collect(),
                behavior: Default::default(),
                webhooks: Vec::new(),
            },
        )
        .await
        .unwrap()
}

/// Sends one request through `router` with a `host` header.
async fn call(
    router: &axum::Router,
    method: &str,
    host: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    use tower::ServiceExt as _;
    let mut req = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", host);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            axum::body::Body::from(b.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let resp = router
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Adds `source` as a new approved version and activates it.
async fn activate(store: &Arc<dyn ControlStore>, mock: &Mock, source: &str) {
    let bundle = mb_import::compile(source.as_bytes()).unwrap();
    let v = store
        .add_contract_version(
            mock.id,
            NewContractVersion {
                source: source.as_bytes().to_vec(),
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
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn status_of(base: &str, p: &str) -> u16 {
    reqwest::get(format!("{base}{p}"))
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn wait_for_status(base: &str, p: &str, want: u16) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let got = status_of(base, p).await;
        if got == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "GET {p}: expected {want} within 2s, last saw {got}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn serves_active_contract_at_prefix() {
    let store = store().await;
    let mock = create_mock(&store, "orders", "/orders", None).await;
    activate(&store, &mock, &fixture_source()).await;
    let deps = deps(store);
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert_eq!(
        registry.mounted(),
        vec![(
            "default".to_string(),
            "orders".to_string(),
            "/orders".to_string()
        )]
    );

    let base = serve(registry.router()).await;
    assert_eq!(status_of(&base, "/orders/orders").await, 200);

    let r = reqwest::get(format!("{base}/nowhere")).await.unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(r.headers()["content-type"], "application/problem+json");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["title"], "No mock mounted at this path");
}

#[tokio::test]
async fn mock_without_active_contract_not_mounted() {
    let store = store().await;
    create_mock(&store, "orders", "/orders", None).await;
    let deps = deps(store);
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert!(registry.mounted().is_empty());
    let base = serve(registry.router()).await;
    assert_eq!(status_of(&base, "/orders/orders").await, 404);
}

#[tokio::test]
async fn activation_change_reloads() {
    let store = store().await;
    let mock = create_mock(&store, "orders", "/orders", None).await;
    activate(&store, &mock, &fixture_source()).await;
    let deps = deps(store.clone());
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let ha = mockingbird::ha::Ha::start_with_bus(
        mb_state::LocalBus::new("test".into()),
        deps.store.clone(),
    )
    .await;
    let _watcher = registry.spawn_watcher(deps, ha.changes());
    let base = serve(registry.router()).await;
    assert_eq!(status_of(&base, "/orders/ping").await, 404);

    activate(&store, &mock, &fixture_v2_source()).await;
    wait_for_status(&base, "/orders/ping", 200).await;
}

#[tokio::test]
async fn delete_unmounts() {
    let store = store().await;
    let mock = create_mock(&store, "orders", "/orders", None).await;
    activate(&store, &mock, &fixture_source()).await;
    let deps = deps(store.clone());
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let ha = mockingbird::ha::Ha::start_with_bus(
        mb_state::LocalBus::new("test".into()),
        deps.store.clone(),
    )
    .await;
    let _watcher = registry.spawn_watcher(deps, ha.changes());
    let base = serve(registry.router()).await;
    assert_eq!(status_of(&base, "/orders/orders").await, 200);

    assert!(
        store
            .delete_mock(DEFAULT_WORKSPACE, "orders")
            .await
            .unwrap()
    );
    wait_for_status(&base, "/orders/orders", 404).await;
    assert!(registry.mounted().is_empty());
}

#[tokio::test]
async fn longest_prefix_wins() {
    let store = store().await;
    let root = create_mock(&store, "root", "/", None).await;
    activate(&store, &root, &fixture_source()).await;
    let a = create_mock(&store, "a", "/a", None).await;
    activate(&store, &a, &fixture_v2_source()).await;
    let deps = deps(store);
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert_eq!(
        registry.mounted(),
        vec![
            ("default".to_string(), "a".to_string(), "/a".to_string()),
            ("default".to_string(), "root".to_string(), "/".to_string()),
        ]
    );
    let base = serve(registry.router()).await;

    // `/a/ping` exists only in `a`; `/ping` is not in `root`.
    assert_eq!(status_of(&base, "/a/ping").await, 200);
    assert_eq!(status_of(&base, "/ping").await, 404);
    // `/orders` goes to `root`; `/a/orders` goes to `a` (stripped to `/orders`).
    assert_eq!(status_of(&base, "/orders").await, 200);
    assert_eq!(status_of(&base, "/a/orders").await, 200);
    // `/ab/...` is not under `/a` (segment boundary), so `root` gets it.
    assert_eq!(status_of(&base, "/ab/orders").await, 404);
}

#[tokio::test]
async fn swap_during_inflight_requests() {
    let store = store().await;
    let mock = create_mock(&store, "orders", "/orders", None).await;
    activate(&store, &mock, &fixture_source()).await;
    let deps = deps(store.clone());
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let base = serve(registry.router()).await;
    let http = reqwest::Client::new();

    let mut inflight = Vec::new();
    for _ in 0..20 {
        let http = http.clone();
        let url = format!("{base}/orders/orders");
        inflight.push(tokio::spawn(async move {
            http.get(url)
                .header("X-Mockingbird-Latency", "200ms")
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    activate(&store, &mock, &fixture_v2_source()).await;
    registry
        .reload_mock(&deps, DEFAULT_WORKSPACE, "orders")
        .await;

    for h in inflight {
        assert_eq!(h.await.unwrap(), 200);
    }
    assert_eq!(status_of(&base, "/orders/ping").await, 200);
}

/// Resolves secret references from a fixed map.
struct FixedSecrets(HashMap<String, String>);

#[async_trait::async_trait]
impl SecretStore for FixedSecrets {
    async fn resolve(&self, reference: &str) -> Result<String, ExtError> {
        self.0
            .get(reference)
            .cloned()
            .ok_or_else(|| ExtError::SecretNotFound(reference.to_string()))
    }
}

fn profile(name: &str, base_url: String, fallback: Option<&str>) -> LlmProfile {
    LlmProfile {
        workspace: DEFAULT_WORKSPACE.into(),
        name: name.into(),
        base_url,
        model: "m".into(),
        secret_ref: Some("env:MB_TEST_REGISTRY_LLM_KEY".into()),
        json_schema: false,
        temperature: 0.2,
        timeout_ms: 3000,
        max_tokens: 512,
        fallback_profile: fallback.map(str::to_string),
        headers: Default::default(),
        extra_body: Default::default(),
        source: ResourceSource::Api,
    }
}

#[tokio::test]
async fn profile_secret_resolved_and_fallback_chain() {
    let primary = MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&primary)
        .await;
    let secondary = MockServer::start().await;
    let order = json!({"id": "6f1c0f0e-2f7b-4a59-9d55-3d1c2b1a0e9f", "customerId": "from-secondary",
                       "status": "shipped", "total": 42});
    wiremock::Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer sekrit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": order.to_string()}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&secondary)
        .await;

    let store = store().await;
    store
        .put_profile(&profile("secondary", secondary.uri(), None))
        .await
        .unwrap();
    store
        .put_profile(&profile("primary", primary.uri(), Some("secondary")))
        .await
        .unwrap();
    let mock = create_mock(&store, "orders", "/orders", Some("primary")).await;
    activate(&store, &mock, &fixture_source()).await;

    let mut deps = deps(store);
    deps.offline = false;
    // The secret is injected rather than set in the process environment.
    deps.ext.secrets = Arc::new(FixedSecrets(HashMap::from([(
        "env:MB_TEST_REGISTRY_LLM_KEY".to_string(),
        "sekrit".to_string(),
    )])));
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let base = serve(registry.router()).await;

    let r = reqwest::get(format!("{base}/orders/orders/o1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-mockingbird-source"], "llm");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["customerId"], "from-secondary");
    assert_eq!(body["total"], 42);
}

#[tokio::test]
async fn fallback_profile_cycle_errors() {
    let store = store().await;
    store
        .put_profile(&profile("a", "http://127.0.0.1:9".into(), Some("b")))
        .await
        .unwrap();
    store
        .put_profile(&profile("b", "http://127.0.0.1:9".into(), Some("a")))
        .await
        .unwrap();
    let mut deps = deps(store);
    deps.offline = false;
    let err = build_provider(&deps, "a")
        .await
        .err()
        .expect("a cyclic fallback chain must fail");
    assert!(err.to_string().contains("cycle"), "{err:#}");
}

/// Two mocks on `/x`: `a` sorts first and holds it, `b` is skipped.
async fn colliding_pair() -> (Arc<dyn ControlStore>, RuntimeDeps, Arc<MockRegistry>, Mock) {
    let store = store().await;
    let a = create_mock(&store, "a", "/x", None).await;
    activate(&store, &a, &fixture_source()).await;
    let b = create_mock(&store, "b", "/x", None).await;
    activate(&store, &b, &fixture_v2_source()).await;
    let deps = deps(store.clone());
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert_eq!(
        registry.mounted(),
        vec![("default".to_string(), "a".to_string(), "/x".to_string())]
    );
    (store, deps, registry, a)
}

#[tokio::test]
async fn deleting_prefix_holder_mounts_skipped_mock() {
    let (store, deps, registry, _a) = colliding_pair().await;
    let base = serve(registry.router()).await;
    // `/ping` exists only in `b`'s contract.
    assert_eq!(status_of(&base, "/x/ping").await, 404);

    assert!(store.delete_mock(DEFAULT_WORKSPACE, "a").await.unwrap());
    registry.reload_mock(&deps, DEFAULT_WORKSPACE, "a").await;

    assert_eq!(
        registry.mounted(),
        vec![("default".to_string(), "b".to_string(), "/x".to_string())]
    );
    assert_eq!(status_of(&base, "/x/ping").await, 200);
}

#[tokio::test]
async fn moving_prefix_holder_mounts_skipped_mock() {
    let (store, deps, registry, a) = colliding_pair().await;
    let base = serve(registry.router()).await;

    let mut moved = a.clone();
    moved.route_prefix = "/y".into();
    store.update_mock(&moved).await.unwrap();
    registry.reload_mock(&deps, DEFAULT_WORKSPACE, "a").await;

    assert_eq!(
        registry.mounted(),
        vec![
            ("default".to_string(), "a".to_string(), "/y".to_string()),
            ("default".to_string(), "b".to_string(), "/x".to_string()),
        ]
    );
    assert_eq!(status_of(&base, "/x/ping").await, 200);
    assert_eq!(status_of(&base, "/y/orders").await, 200);
}

/// A mock whose rebuild fails keeps serving its previous version, and is
/// reported as failed (for `/readyz`) until a rebuild succeeds.
#[tokio::test]
async fn failed_rebuild_keeps_serving_previous_version() {
    let store = store().await;
    let mut q = profile("q", "http://127.0.0.1:9".into(), None);
    q.secret_ref = None;
    store.put_profile(&q).await.unwrap();
    let c = create_mock(&store, "c", "/c", Some("q")).await;
    activate(&store, &c, &fixture_source()).await;
    let mut deps = deps(store.clone());
    deps.offline = false;
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let base = serve(registry.router()).await;
    assert_eq!(status_of(&base, "/c/orders").await, 200);
    assert!(registry.failed_mocks().is_empty());

    q.secret_ref = Some("env:MB_TEST_REGISTRY_NOT_SET_5D2B".into());
    store.put_profile(&q).await.unwrap();
    registry.reload_all(&deps).await.unwrap();
    assert_eq!(status_of(&base, "/c/orders").await, 200);
    registry.reload_mock(&deps, DEFAULT_WORKSPACE, "c").await;
    assert_eq!(status_of(&base, "/c/orders").await, 200);
    let failed = registry.failed_mocks();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0, "c");
    assert!(failed[0].1.contains("secret"), "{failed:?}");
    let issues = registry.mock_issues();
    assert!(
        issues[0].starts_with("mock c: stale, serving the previous version: "),
        "{issues:?}"
    );

    // Fixing the profile clears the failure.
    q.secret_ref = None;
    store.put_profile(&q).await.unwrap();
    registry.reload_all(&deps).await.unwrap();
    assert!(registry.failed_mocks().is_empty());
    assert_eq!(status_of(&base, "/c/orders").await, 200);

    // Deleting the mock unmounts it.
    assert!(store.delete_mock(DEFAULT_WORKSPACE, "c").await.unwrap());
    registry.reload_mock(&deps, DEFAULT_WORKSPACE, "c").await;
    assert_eq!(status_of(&base, "/c/orders").await, 404);
}

/// A primary that times out falls back to the secondary profile, which gets
/// its own timeout: the engine waits for the whole chain.
#[tokio::test]
async fn fallback_profile_used_when_primary_times_out() {
    let primary = MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&primary)
        .await;
    let secondary = MockServer::start().await;
    let order = json!({"id": "6f1c0f0e-2f7b-4a59-9d55-3d1c2b1a0e9f", "customerId": "from-secondary",
                       "status": "shipped", "total": 7});
    wiremock::Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": order.to_string()}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .expect(1)
        .mount(&secondary)
        .await;

    let store = store().await;
    let mut s = profile("secondary", secondary.uri(), None);
    s.secret_ref = None;
    s.timeout_ms = 2000;
    store.put_profile(&s).await.unwrap();
    let mut p = profile("primary", primary.uri(), Some("secondary"));
    p.secret_ref = None;
    p.timeout_ms = 300;
    store.put_profile(&p).await.unwrap();
    let mock = create_mock(&store, "orders", "/orders", Some("primary")).await;
    activate(&store, &mock, &fixture_source()).await;

    let mut deps = deps(store);
    deps.offline = false;
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let base = serve(registry.router()).await;

    let r = reqwest::get(format!("{base}/orders/orders/o1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-mockingbird-source"], "llm");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["customerId"], "from-secondary");
    secondary.verify().await;
}

// ---- protocol families ----

fn fixture_text(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(rel);
    std::fs::read_to_string(path).unwrap()
}

/// `(service, mock)` of every service the shared gRPC server publishes.
fn grpc_services(registry: &MockRegistry) -> Vec<(String, String)> {
    registry.grpc().services()
}

fn services(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(s, m)| (s.to_string(), m.to_string()))
        .collect()
}

#[tokio::test]
async fn graphql_mock_served_at_prefix() {
    let store = store().await;
    let mock = create_mock(&store, "gh", "/gh", None).await;
    activate(&store, &mock, &fixture_text("graphql/github.graphql")).await;
    let deps = deps(store);
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert_eq!(
        registry.mounted(),
        vec![("default".to_string(), "gh".to_string(), "/gh".to_string())]
    );
    let base = serve(registry.router()).await;

    let r = reqwest::Client::new()
        .post(format!("{base}/gh/graphql"))
        .json(&json!({"query": "{ viewer { login } }"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["data"]["viewer"]["login"].is_string(), "{body}");
    assert_eq!(status_of(&base, "/gh/other").await, 404);
}

/// `graphql_path` moves one mock's endpoint (Shopify's `graphql.json`,
/// whose schema is published as introspection JSON) while the others keep
/// `protocols.graphql.path`.
#[tokio::test]
async fn graphql_path_is_per_mock() {
    let store = store().await;
    let mut shop = create_mock(&store, "shop", "/api/2024-10", None).await;
    shop.behavior.graphql_path = Some("/graphql.json".into());
    let shop = store.update_mock(&shop).await.unwrap();
    activate(&store, &shop, &fixture_text("graphql/starwars-introspection.json")).await;
    let gh = create_mock(&store, "gh", "/gh", None).await;
    activate(&store, &gh, &fixture_text("graphql/github.graphql")).await;
    let registry = MockRegistry::new();
    registry.reload_all(&deps(store)).await.unwrap();
    let base = serve(registry.router()).await;
    let http = reqwest::Client::new();
    for (path, query, pointer) in [
        ("/api/2024-10/graphql.json", "{ hero { name } }", "/data/hero/name"),
        ("/gh/graphql", "{ viewer { login } }", "/data/viewer/login"),
    ] {
        let r = http
            .post(format!("{base}{path}"))
            .json(&json!({ "query": query }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let body: Value = r.json().await.unwrap();
        assert!(body.pointer(pointer).is_some_and(Value::is_string), "{path}: {body}");
    }
    assert_eq!(status_of(&base, "/api/2024-10/graphql").await, 404);
}

#[tokio::test]
async fn soap_wsdl_uses_mount_prefix() {
    let store = store().await;
    let mock = create_mock(&store, "calc", "/calc", None).await;
    activate(&store, &mock, &fixture_text("wsdl/calculator.wsdl")).await;
    let deps = deps(store);
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let base = serve(registry.router()).await;

    let r = reqwest::Client::new()
        .get(format!("{base}/calc/calculator.asmx?wsdl"))
        .header("host", "127.0.0.1:1234")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert_eq!(
        text.matches(r#"location="http://127.0.0.1:1234/calc/calculator.asmx""#)
            .count(),
        2,
        "{text}"
    );
}

#[tokio::test]
async fn grpc_mock_published_to_grpc_server() {
    let store = store().await;
    let mock = create_mock(&store, "routes", "/routes", None).await;
    activate(&store, &mock, &fixture_text("proto/route_guide.proto")).await;
    let deps = deps(store);
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();

    assert_eq!(
        grpc_services(&registry),
        services(&[("routeguide.RouteGuide", "routes")])
    );
    // A gRPC mock holds no route prefix on the data plane.
    assert!(registry.mounted().is_empty());
    let base = serve(registry.router()).await;
    assert_eq!(status_of(&base, "/routes").await, 404);
    assert!(registry.failed_mocks().is_empty());
}

/// A service already served by a mock stays with it: a newcomer serving
/// the same service is skipped and reported as failed, whatever the names'
/// order, and takes over only when the holder goes away.
#[tokio::test]
async fn grpc_service_collision_keeps_the_holder() {
    let store = store().await;
    let b = create_mock(&store, "b-routes", "/b", None).await;
    activate(&store, &b, &fixture_text("proto/route_guide.proto")).await;
    let deps = deps(store.clone());
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert_eq!(
        grpc_services(&registry),
        services(&[("routeguide.RouteGuide", "b-routes")])
    );

    // `a-routes` sorts first but arrives second: `b-routes` keeps it.
    let a = create_mock(&store, "a-routes", "/a", None).await;
    activate(&store, &a, &fixture_text("proto/route_guide.proto")).await;
    registry.reload_mock(&deps, "default", "a-routes").await;
    assert_eq!(
        grpc_services(&registry),
        services(&[("routeguide.RouteGuide", "b-routes")])
    );
    let failed = registry.failed_mocks();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0, "a-routes");
    assert_eq!(
        failed[0].1,
        "service routeguide.RouteGuide is already served by mock b-routes"
    );

    // Neither a full reload nor a rebuild of the holder moves it.
    registry.reload_all(&deps).await.unwrap();
    registry.reload_mock(&deps, "default", "b-routes").await;
    registry.reload_mock(&deps, "default", "a-routes").await;
    assert_eq!(
        grpc_services(&registry),
        services(&[("routeguide.RouteGuide", "b-routes")])
    );
    assert_eq!(registry.failed_mocks().len(), 1);

    // A mock serving other services is unaffected.
    let hello = create_mock(&store, "hello", "/hello", None).await;
    activate(&store, &hello, &fixture_text("proto/helloworld.proto")).await;
    registry.reload_mock(&deps, "default", "hello").await;
    assert_eq!(
        grpc_services(&registry),
        services(&[
            ("helloworld.Greeter", "hello"),
            ("routeguide.RouteGuide", "b-routes"),
        ])
    );

    assert!(
        store
            .delete_mock(DEFAULT_WORKSPACE, "b-routes")
            .await
            .unwrap()
    );
    registry.reload_mock(&deps, "default", "b-routes").await;
    assert_eq!(
        grpc_services(&registry),
        services(&[
            ("helloworld.Greeter", "hello"),
            ("routeguide.RouteGuide", "a-routes"),
        ])
    );
    assert!(registry.failed_mocks().is_empty());
}

/// A mock in another workspace cannot take a service over either, even
/// when its workspace sorts first.
#[tokio::test]
async fn grpc_service_holder_survives_cross_workspace_newcomer() {
    let store = store().await;
    let holder = create_mock(&store, "routes", "/routes", None).await;
    activate(&store, &holder, &fixture_text("proto/route_guide.proto")).await;
    let deps = deps(store.clone());
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();

    store.create_workspace("aaa", "aaa").await.unwrap();
    let thief = store
        .create_mock(
            "aaa",
            NewMock {
                name: "routes".into(),
                route_prefix: Some("/aaa-routes".into()),
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
    activate(&store, &thief, &fixture_text("proto/route_guide.proto")).await;
    registry.reload_mock(&deps, "aaa", "routes").await;
    registry.reload_all(&deps).await.unwrap();
    assert_eq!(
        grpc_services(&registry),
        services(&[("routeguide.RouteGuide", "routes")])
    );
    let failed = registry.failed_mocks();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0, "aaa/routes");
}

#[tokio::test]
async fn grpc_mock_removed_on_delete() {
    let store = store().await;
    let mock = create_mock(&store, "routes", "/routes", None).await;
    activate(&store, &mock, &fixture_text("proto/route_guide.proto")).await;
    let deps = deps(store.clone());
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let ha = mockingbird::ha::Ha::start_with_bus(
        mb_state::LocalBus::new("test".into()),
        deps.store.clone(),
    )
    .await;
    let _watcher = registry.spawn_watcher(deps, ha.changes());
    assert_eq!(grpc_services(&registry).len(), 1);

    assert!(
        store
            .delete_mock(DEFAULT_WORKSPACE, "routes")
            .await
            .unwrap()
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while !grpc_services(&registry).is_empty() {
        assert!(Instant::now() < deadline, "service still published");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn orders_asyncapi_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/asyncapi/orders-amqp-2.6.yaml");
    std::fs::read_to_string(path).unwrap()
}

/// [`deps`] with one memory broker, `mem`.
fn event_deps(store: Arc<dyn ControlStore>) -> RuntimeDeps {
    use mb_protocols::events::{BrokerConfig, BrokerKind, Brokers};
    let mut deps = deps(store);
    deps.events.brokers = Brokers::new(
        vec![BrokerConfig {
            name: "mem".into(),
            kind: BrokerKind::Memory,
            url: String::new(),
            tls: false,
            ca_file: None,
            sasl: None,
            client_id_prefix: "mockingbird".into(),
        }],
        Arc::new(mb_ext::EnvFileSecrets),
    );
    deps
}

#[tokio::test]
async fn asyncapi_mock_mounts_with_channel_runtime() {
    let store = store().await;
    let mock = create_mock(&store, "orders", "/orders", None).await;
    activate(&store, &mock, &orders_asyncapi_source()).await;
    let deps = event_deps(store);
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert!(
        registry.failed_mocks().is_empty(),
        "{:?}",
        registry.failed_mocks()
    );
    let channels = registry
        .channel_runtime(DEFAULT_WORKSPACE, "orders")
        .expect("a channel runtime");
    assert_eq!(channels.broker_name().as_deref(), Some("mem"));
    assert!(
        registry
            .channel_runtime(DEFAULT_WORKSPACE, "nobody")
            .is_none()
    );
    assert!(
        registry.event_issues().is_empty(),
        "{:?}",
        registry.event_issues()
    );

    let base = serve(registry.router()).await;
    let r = reqwest::get(format!("{base}/orders/anything"))
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(r.headers()["content-type"], "application/problem+json");
}

#[tokio::test]
async fn control_only_mode_starts_no_consumers() {
    let store = store().await;
    let mock = create_mock(&store, "orders", "/orders", None).await;
    activate(&store, &mock, &orders_asyncapi_source()).await;
    let mut deps = event_deps(store);
    deps.mode = mockingbird::runtime::ServeMode::ControlOnly;
    let broker = deps.events.brokers.memory("mem").unwrap();
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let channels = registry
        .channel_runtime(DEFAULT_WORKSPACE, "orders")
        .expect("a channel runtime for manual publish");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(broker.subscribed_groups().is_empty());
    assert_eq!(
        channels
            .publish_manual("orderCreated", 1, None)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn settings_change_restarts_runtime() {
    use mb_protocols::events::PublishError;
    let store = store().await;
    let mock = create_mock(&store, "orders", "/orders", None).await;
    activate(&store, &mock, &orders_asyncapi_source()).await;
    let deps = event_deps(store.clone());
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let before = registry
        .channel_runtime(DEFAULT_WORKSPACE, "orders")
        .unwrap();
    assert_eq!(
        before
            .publish_manual("orderCreated", 1, None)
            .await
            .unwrap()
            .len(),
        1
    );

    store
        .put_channel_settings(&mb_store::ChannelSettings {
            mock_id: mock.id,
            channel_id: "orderCreated".into(),
            enabled: false,
            triggers: None,
            source: ResourceSource::Api,
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    registry
        .reload_mock(&deps, DEFAULT_WORKSPACE, "orders")
        .await;

    let after = registry
        .channel_runtime(DEFAULT_WORKSPACE, "orders")
        .unwrap();
    assert!(!Arc::ptr_eq(&before, &after));
    assert_eq!(
        after
            .publish_manual("orderCreated", 1, None)
            .await
            .map_err(|f| f.error),
        Err(PublishError::Disabled("orderCreated".into()))
    );
}

#[tokio::test]
async fn mocks_share_a_prefix_on_different_hosts() {
    let store = store().await;
    let a = create_hosted(&store, "a", "/", &["a.test"]).await;
    activate(&store, &a, &fixture_v2_source()).await; // has GET /ping
    let b = create_hosted(&store, "b", "/", &["b.test"]).await;
    activate(&store, &b, &fixture_source()).await;
    let registry = MockRegistry::new();
    registry.reload_all(&deps(store.clone())).await.unwrap();
    let r = registry.router();
    assert_eq!(call(&r, "GET", "a.test", "/ping", None).await.0, 200);
    assert_eq!(call(&r, "GET", "B.TEST:443", "/orders", None).await.0, 200);
    assert_ne!(call(&r, "GET", "b.test", "/ping", None).await.0, 200);
    let (status, body) = call(&r, "GET", "localhost:8080", "/orders", None).await;
    assert_eq!(status, 404);
    assert!(
        body["detail"].as_str().unwrap().contains("localhost"),
        "{body}"
    );
    assert_eq!(registry.claimed_hosts(), ["a.test", "b.test"]);
    assert!(registry.claims_host("a.test") && !registry.claims_host("c.test"));
}

#[tokio::test]
async fn rest_at_root_and_graphql_below_on_one_host() {
    let store = store().await;
    let rest = create_hosted(&store, "rest", "/", &["api.github.test"]).await;
    activate(&store, &rest, &fixture_source()).await;
    let gql = create_hosted(&store, "gql", "/graphql", &["api.github.test"]).await;
    let sdl = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/graphql/github.graphql"),
    )
    .unwrap();
    activate(&store, &gql, &sdl).await;
    let registry = MockRegistry::new();
    registry.reload_all(&deps(store.clone())).await.unwrap();
    let r = registry.router();
    assert_eq!(
        call(&r, "GET", "api.github.test", "/orders", None).await.0,
        200
    );
    let (status, body) = call(
        &r,
        "POST",
        "api.github.test",
        "/graphql",
        Some(json!({"query": "{ __typename }"})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("data").is_some(), "{body}");
}

#[tokio::test]
async fn host_specific_mock_wins_over_hostless_at_same_prefix() {
    let store = store().await;
    let any = create_mock(&store, "any", "/", None).await;
    activate(&store, &any, &fixture_source()).await;
    let x = create_hosted(&store, "x", "/", &["x.test"]).await;
    activate(&store, &x, &fixture_v2_source()).await;
    let registry = MockRegistry::new();
    registry.reload_all(&deps(store.clone())).await.unwrap();
    let r = registry.router();
    assert_eq!(call(&r, "GET", "x.test", "/ping", None).await.0, 200);
    assert_ne!(call(&r, "GET", "localhost", "/ping", None).await.0, 200);
    assert_eq!(call(&r, "GET", "localhost", "/orders", None).await.0, 200);
}

#[tokio::test]
async fn overlapping_host_sets_collide() {
    let store = store().await;
    let a = create_hosted(&store, "a", "/", &["a.test"]).await;
    activate(&store, &a, &fixture_source()).await;
    let b = create_hosted(&store, "b", "/", &["b.test", "a.test"]).await;
    activate(&store, &b, &fixture_source()).await;
    let registry = MockRegistry::new();
    registry.reload_all(&deps(store.clone())).await.unwrap();
    assert_eq!(
        registry.mounted(),
        [("default".to_string(), "a".to_string(), "/".to_string())]
    );
    registry
        .reload_mock(&deps(store.clone()), DEFAULT_WORKSPACE, "b")
        .await;
    assert_eq!(
        registry.mounted().len(),
        1,
        "a sorts first and keeps the route"
    );
}

#[tokio::test]
async fn failed_first_build_is_reported_failed() {
    let store = store().await;
    let mut q = profile("q", "http://127.0.0.1:9".into(), None);
    q.secret_ref = Some("env:MB_TEST_REGISTRY_NOT_SET_7A1C".into());
    store.put_profile(&q).await.unwrap();
    let d = create_mock(&store, "d", "/d", Some("q")).await;
    activate(&store, &d, &fixture_source()).await;
    let mut deps = deps(store.clone());
    deps.offline = false;
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert!(registry.mock_issues()[0].starts_with("mock d: failed: "));
}

#[tokio::test]
async fn hosted_mock_that_failed_to_build_keeps_its_host_and_answers_503() {
    let store = store().await;
    let mut q = profile("q", "http://127.0.0.1:9".into(), None);
    q.secret_ref = Some("env:MB_TEST_REGISTRY_NOT_SET_7A1C".into());
    store.put_profile(&q).await.unwrap();
    let mut m = create_hosted(&store, "stripe", "/", &["api.stripe.test"]).await;
    m.llm_profile = Some("q".into());
    let m = store.update_mock(&m).await.unwrap();
    activate(&store, &m, &fixture_source()).await;
    let mut deps = deps(store.clone());
    deps.offline = false;
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert!(registry.mounted().is_empty());
    assert!(
        registry.claims_host("api.stripe.test"),
        "fails closed, never passes through"
    );
    assert_eq!(registry.claimed_hosts(), ["api.stripe.test"]);
    let (status, body) = call(
        &registry.router(),
        "GET",
        "api.stripe.test",
        "/orders",
        None,
    )
    .await;
    assert_eq!(status, 503, "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("stripe"),
        "{body}"
    );
    registry
        .reload_mock(&deps, DEFAULT_WORKSPACE, "stripe")
        .await;
    assert!(registry.claims_host("api.stripe.test"));
    store
        .delete_mock(DEFAULT_WORKSPACE, "stripe")
        .await
        .unwrap();
    registry
        .reload_mock(&deps, DEFAULT_WORKSPACE, "stripe")
        .await;
    assert!(!registry.claims_host("api.stripe.test"));
}

#[tokio::test]
async fn runtime_carries_mock_behavior() {
    let store = store().await;
    let mut mock = create_mock(&store, "orders", "/orders", None).await;
    activate(&store, &mock, &fixture_source()).await;
    mock.behavior = MockBehavior {
        consistency: Consistency::Off,
        unknown_ids: UnknownIds::NotFound,
        default_scenario: Some("gift".into()),
        ..Default::default()
    };
    let mock = store.update_mock(&mock).await.unwrap();
    let rt = mockingbird::runtime::build_runtime(&deps(store.clone()), &mock)
        .await
        .unwrap()
        .unwrap();
    assert!(!rt.consistency);
    assert_eq!(rt.unknown_ids, mb_protocols::UnknownIds::NotFound);
    assert_eq!(rt.default_scenario.as_deref(), Some("gift"));
}
