#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mb_engine::StreamConfig;
use mb_ext::{ExtError, Extensions, SecretStore};
use mb_protocols::ProtocolSettings;
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    ContractStatus, ControlStore, DEFAULT_WORKSPACE, LlmProfile, Mock, NewContractVersion, NewMock,
    ResourceSource, SqliteControlStore,
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
        protocols: ProtocolSettings::default(),
        stream: StreamConfig::default(),
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
            },
        )
        .await
        .unwrap()
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
        vec![("orders".to_string(), "/orders".to_string())]
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
    let _watcher = registry.spawn_watcher(deps);
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
    let _watcher = registry.spawn_watcher(deps);
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
            ("a".to_string(), "/a".to_string()),
            ("root".to_string(), "/".to_string()),
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
    registry.reload_mock(&deps, "orders").await;

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
        vec![("a".to_string(), "/x".to_string())]
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
    registry.reload_mock(&deps, "a").await;

    assert_eq!(
        registry.mounted(),
        vec![("b".to_string(), "/x".to_string())]
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
    registry.reload_mock(&deps, "a").await;

    assert_eq!(
        registry.mounted(),
        vec![
            ("a".to_string(), "/y".to_string()),
            ("b".to_string(), "/x".to_string()),
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
    registry.reload_mock(&deps, "c").await;
    assert_eq!(status_of(&base, "/c/orders").await, 200);
    let failed = registry.failed_mocks();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0, "c");
    assert!(failed[0].1.contains("secret"), "{failed:?}");

    // Fixing the profile clears the failure.
    q.secret_ref = None;
    store.put_profile(&q).await.unwrap();
    registry.reload_all(&deps).await.unwrap();
    assert!(registry.failed_mocks().is_empty());
    assert_eq!(status_of(&base, "/c/orders").await, 200);

    // Deleting the mock unmounts it.
    assert!(store.delete_mock(DEFAULT_WORKSPACE, "c").await.unwrap());
    registry.reload_mock(&deps, "c").await;
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
        vec![("gh".to_string(), "/gh".to_string())]
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

/// A service already served by a mock that sorts first by name skips the
/// later mock, which is reported as failed; the skipped mock takes over
/// when the holder goes away.
#[tokio::test]
async fn grpc_service_collision_skips_later_mock() {
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

    // `a-routes` sorts first, so it takes the service over.
    let a = create_mock(&store, "a-routes", "/a", None).await;
    activate(&store, &a, &fixture_text("proto/route_guide.proto")).await;
    registry.reload_mock(&deps, "a-routes").await;
    assert_eq!(
        grpc_services(&registry),
        services(&[("routeguide.RouteGuide", "a-routes")])
    );
    let failed = registry.failed_mocks();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0, "b-routes");
    assert_eq!(
        failed[0].1,
        "service routeguide.RouteGuide is already served by mock a-routes"
    );

    // A full reload keeps the same winner.
    registry.reload_all(&deps).await.unwrap();
    assert_eq!(
        grpc_services(&registry),
        services(&[("routeguide.RouteGuide", "a-routes")])
    );
    assert_eq!(registry.failed_mocks().len(), 1);

    // A mock serving other services is unaffected.
    let hello = create_mock(&store, "hello", "/hello", None).await;
    activate(&store, &hello, &fixture_text("proto/helloworld.proto")).await;
    registry.reload_mock(&deps, "hello").await;
    assert_eq!(
        grpc_services(&registry),
        services(&[
            ("helloworld.Greeter", "hello"),
            ("routeguide.RouteGuide", "a-routes"),
        ])
    );

    assert!(
        store
            .delete_mock(DEFAULT_WORKSPACE, "a-routes")
            .await
            .unwrap()
    );
    registry.reload_mock(&deps, "a-routes").await;
    assert_eq!(
        grpc_services(&registry),
        services(&[
            ("helloworld.Greeter", "hello"),
            ("routeguide.RouteGuide", "b-routes"),
        ])
    );
    assert!(registry.failed_mocks().is_empty());
}

#[tokio::test]
async fn grpc_mock_removed_on_delete() {
    let store = store().await;
    let mock = create_mock(&store, "routes", "/routes", None).await;
    activate(&store, &mock, &fixture_text("proto/route_guide.proto")).await;
    let deps = deps(store.clone());
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    let _watcher = registry.spawn_watcher(deps);
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
