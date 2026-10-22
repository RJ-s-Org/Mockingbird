//! Mocks from every workspace: isolation, per-workspace profiles and
//! data-plane API keys.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use mb_ext::Extensions;
use mb_protocols::{RequestRecord, RequestRecorder};
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    ApiKey, ContractStatus, ControlStore, DEFAULT_WORKSPACE, DataPlaneAuthConfig,
    DataPlaneAuthMode, LlmProfile, Mock, NewContractVersion, NewMock, RequestLogFilter,
    ResourceSource, SqliteControlStore,
};
use mockingbird::dp_keys::StoreDataPlaneKeys;
use mockingbird::recorder::StoreRecorder;
use mockingbird::registry::{MockRegistry, mock_label};
use mockingbird::runtime::RuntimeDeps;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{MockServer, ResponseTemplate};

const TEAM_A: &str = "team-a";

fn fixture_source() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/openapi/orders-3.0.yaml");
    std::fs::read_to_string(path).unwrap()
}

async fn store() -> Arc<dyn ControlStore> {
    let store: Arc<dyn ControlStore> = Arc::new(
        SqliteControlStore::connect("sqlite::memory:")
            .await
            .unwrap(),
    );
    store.create_workspace(TEAM_A, "Team A").await.unwrap();
    store
}

fn deps(store: Arc<dyn ControlStore>, recorder: Option<Arc<dyn RequestRecorder>>) -> RuntimeDeps {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    RuntimeDeps {
        data_plane_keys: Some(StoreDataPlaneKeys::new(store.clone())),
        protocols: mb_protocols::ProtocolSettings::default(),
        stream: mb_engine::StreamConfig::default(),
        store,
        state,
        ext: Extensions::default(),
        recorder,
        offline: true,
        default_profile: None.into(),
        events: mb_protocols::events::EventDeps::default(),
        limiter: mb_engine::GenerationLimiter::new(64),
        scrubber: None,
        limits: Default::default(),
        http: Default::default(),
        mode: mockingbird::runtime::ServeMode::Full,
    }
}

/// Creates `name` in `ws` at `prefix` with the fixture contract active.
async fn mock_in(
    store: &Arc<dyn ControlStore>,
    ws: &str,
    name: &str,
    prefix: &str,
    profile: Option<&str>,
    auth: DataPlaneAuthMode,
) -> Mock {
    let mock = store
        .create_mock(
            ws,
            NewMock {
                name: name.into(),
                route_prefix: Some(prefix.into()),
                persona: None,
                llm_profile: profile.map(str::to_string),
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: DataPlaneAuthConfig {
                    mode: auth,
                    ..DataPlaneAuthConfig::default()
                },
                behavior: Default::default(),
                hosts: vec![],
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

async fn status_with_key(base: &str, p: &str, key: &str) -> u16 {
    reqwest::Client::new()
        .get(format!("{base}{p}"))
        .header("x-api-key", key)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// An LLM server answering every completion with `order`.
async fn llm_server(customer: &str) -> MockServer {
    let server = MockServer::start().await;
    let order = json!({"id": "6f1c0f0e-2f7b-4a59-9d55-3d1c2b1a0e9f", "customerId": customer,
                       "status": "shipped", "total": 1});
    wiremock::Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": order.to_string()}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;
    server
}

fn profile(ws: &str, name: &str, base_url: String) -> LlmProfile {
    LlmProfile {
        workspace: ws.into(),
        name: name.into(),
        base_url,
        model: "m".into(),
        secret_ref: None,
        json_schema: false,
        temperature: 0.2,
        timeout_ms: 3000,
        max_tokens: 512,
        fallback_profile: None,
        headers: Default::default(),
        extra_body: Default::default(),
        source: ResourceSource::Api,
    }
}

async fn customer_of(base: &str, p: &str) -> String {
    let r = reqwest::get(format!("{base}{p}")).await.unwrap();
    assert_eq!(r.status(), 200, "GET {p}");
    assert_eq!(r.headers()["x-mockingbird-source"], "llm");
    let body: Value = r.json().await.unwrap();
    body["customerId"].as_str().unwrap().to_string()
}

/// Stores a key in `ws` and returns its token.
async fn store_key(
    store: &Arc<dyn ControlStore>,
    ws: &str,
    scopes: &[&str],
    sysadmin: bool,
    expires_in_secs: Option<i64>,
) -> (String, String) {
    let generated = mb_auth::generate_key();
    store
        .create_api_key(&ApiKey {
            key_id: generated.key_id.clone(),
            workspace: ws.into(),
            name: "k".into(),
            role: "viewer".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            sysadmin,
            secret_hash: generated.secret_hash,
            created_by: "test".into(),
            created_at: Utc::now(),
            expires_at: expires_in_secs.map(|s| Utc::now() + chrono::Duration::seconds(s)),
            last_used_at: None,
            revoked_at: None,
        })
        .await
        .unwrap();
    (generated.key_id, generated.token)
}

/// A registry serving `store`'s mocks, and its data-plane base URL.
async fn served(deps: &RuntimeDeps) -> (Arc<MockRegistry>, String) {
    let registry = MockRegistry::new();
    registry.reload_all(deps).await.unwrap();
    let base = serve(registry.router()).await;
    (registry, base)
}

#[test]
fn labels_qualify_non_default_workspaces() {
    assert_eq!(mock_label(DEFAULT_WORKSPACE, "orders"), "orders");
    assert_eq!(mock_label(TEAM_A, "orders"), "team-a/orders");
}

#[tokio::test]
async fn same_name_two_workspaces_isolated() {
    let store = store().await;
    let default_mock = mock_in(
        &store,
        DEFAULT_WORKSPACE,
        "orders",
        "/a",
        None,
        DataPlaneAuthMode::None,
    )
    .await;
    let team_mock = mock_in(
        &store,
        TEAM_A,
        "orders",
        "/b",
        None,
        DataPlaneAuthMode::None,
    )
    .await;
    let recorder = StoreRecorder::new(store.clone(), 100, Vec::new());
    let mut live = recorder.live();
    let deps = deps(store.clone(), Some(recorder.clone()));
    let (registry, base) = served(&deps).await;
    assert_eq!(
        registry.mounted(),
        vec![
            (
                DEFAULT_WORKSPACE.to_string(),
                "orders".to_string(),
                "/a".to_string()
            ),
            (TEAM_A.to_string(), "orders".to_string(), "/b".to_string()),
        ]
    );

    let http = reqwest::Client::new();
    let created: Value = http
        .post(format!("{base}/a/orders"))
        .json(&json!({"customerId": "c1", "total": 10}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let listed: Value = reqwest::get(format!("{base}/b/orders"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !listed.to_string().contains(&id),
        "team-a list leaked {id}: {listed}"
    );

    // Live records carry their mock's workspace.
    let mut records: Vec<RequestRecord> = Vec::new();
    for _ in 0..2 {
        let r = tokio::time::timeout(Duration::from_secs(2), live.recv())
            .await
            .unwrap()
            .unwrap();
        records.push(r);
    }
    records.sort_by(|a, b| a.path.cmp(&b.path));
    let seen: Vec<(&str, &str, &str)> = records
        .iter()
        .map(|r| (r.workspace.as_str(), r.mock.as_str(), r.method.as_str()))
        .collect();
    assert!(
        seen.contains(&(DEFAULT_WORKSPACE, "orders", "POST")),
        "{seen:?}"
    );
    assert!(seen.contains(&(TEAM_A, "orders", "GET")), "{seen:?}");

    // Each mock's request log holds only its own entry.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let a = store
            .list_request_log(default_mock.id, &RequestLogFilter::default())
            .await
            .unwrap();
        let b = store
            .list_request_log(team_mock.id, &RequestLogFilter::default())
            .await
            .unwrap();
        if a.len() == 1 && b.len() == 1 {
            assert_eq!(a[0].method, "POST");
            assert_eq!(b[0].method, "GET");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "request logs: default {} entries, team-a {} entries",
            a.len(),
            b.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn profile_resolved_in_mock_workspace() {
    let in_default = llm_server("from-default").await;
    let in_team = llm_server("from-team-a").await;
    let store = store().await;
    store
        .put_profile(&profile(DEFAULT_WORKSPACE, "p", in_default.uri()))
        .await
        .unwrap();
    store
        .put_profile(&profile(TEAM_A, "p", in_team.uri()))
        .await
        .unwrap();
    mock_in(
        &store,
        DEFAULT_WORKSPACE,
        "orders",
        "/a",
        Some("p"),
        DataPlaneAuthMode::None,
    )
    .await;
    mock_in(
        &store,
        TEAM_A,
        "orders",
        "/b",
        Some("p"),
        DataPlaneAuthMode::None,
    )
    .await;
    let mut deps = deps(store, None);
    deps.offline = false;
    let (registry, base) = served(&deps).await;
    assert!(registry.failed_mocks().is_empty());

    assert_eq!(customer_of(&base, "/a/orders/o1").await, "from-default");
    assert_eq!(customer_of(&base, "/b/orders/o1").await, "from-team-a");
}

#[tokio::test]
async fn default_profile_falls_back_to_default_workspace() {
    let in_default = llm_server("from-default").await;
    let in_team_b = llm_server("from-team-b").await;
    let store = store().await;
    store.create_workspace("team-b", "Team B").await.unwrap();
    store
        .put_profile(&profile(DEFAULT_WORKSPACE, "dp", in_default.uri()))
        .await
        .unwrap();
    store
        .put_profile(&profile("team-b", "dp", in_team_b.uri()))
        .await
        .unwrap();
    // team-a has no `dp`: the default workspace's is used.
    mock_in(
        &store,
        TEAM_A,
        "orders",
        "/a",
        None,
        DataPlaneAuthMode::None,
    )
    .await;
    // team-b has its own `dp`, which wins.
    mock_in(
        &store,
        "team-b",
        "orders",
        "/b",
        None,
        DataPlaneAuthMode::None,
    )
    .await;
    let mut deps = deps(store, None);
    deps.offline = false;
    deps.default_profile = Some("dp".to_string()).into();
    let (registry, base) = served(&deps).await;
    assert!(registry.failed_mocks().is_empty());

    assert_eq!(customer_of(&base, "/a/orders/o1").await, "from-default");
    assert_eq!(customer_of(&base, "/b/orders/o1").await, "from-team-b");
}

#[tokio::test]
async fn api_key_mode_with_store_key() {
    let store = store().await;
    mock_in(
        &store,
        TEAM_A,
        "orders",
        "/b",
        None,
        DataPlaneAuthMode::ApiKey,
    )
    .await;
    let (key_id, token) = store_key(&store, TEAM_A, &["data-plane"], false, None).await;
    let (_, other_ws) = store_key(&store, DEFAULT_WORKSPACE, &["data-plane"], false, None).await;
    let (_, sysadmin) = store_key(&store, DEFAULT_WORKSPACE, &["data-plane"], true, None).await;
    let deps = deps(store.clone(), None);
    let (_registry, base) = served(&deps).await;

    assert_eq!(status_of(&base, "/b/orders").await, 401);
    assert_eq!(status_with_key(&base, "/b/orders", &token).await, 200);
    assert_eq!(
        status_of(&base, &format!("/b/orders?api_key={token}")).await,
        200
    );
    // A key of another workspace is rejected; a sysadmin key is not.
    assert_eq!(status_with_key(&base, "/b/orders", &other_ws).await, 401);
    assert_eq!(status_with_key(&base, "/b/orders", &sysadmin).await, 200);

    assert!(store.revoke_api_key(TEAM_A, &key_id).await.unwrap());
    assert_eq!(status_with_key(&base, "/b/orders", &token).await, 401);
}

#[tokio::test]
async fn admin_scoped_key_rejected_on_data_plane() {
    let store = store().await;
    mock_in(
        &store,
        TEAM_A,
        "orders",
        "/b",
        None,
        DataPlaneAuthMode::ApiKey,
    )
    .await;
    let (_, token) = store_key(&store, TEAM_A, &["admin"], false, None).await;
    let deps = deps(store, None);
    let (_registry, base) = served(&deps).await;
    assert_eq!(status_with_key(&base, "/b/orders", &token).await, 401);
}

#[tokio::test]
async fn expired_data_plane_key_401() {
    let store = store().await;
    mock_in(
        &store,
        TEAM_A,
        "orders",
        "/b",
        None,
        DataPlaneAuthMode::ApiKey,
    )
    .await;
    let (_, token) = store_key(&store, TEAM_A, &["data-plane"], false, Some(-60)).await;
    let deps = deps(store, None);
    let (_registry, base) = served(&deps).await;
    assert_eq!(status_with_key(&base, "/b/orders", &token).await, 401);
}

#[tokio::test]
async fn workspace_force_delete_unmounts() {
    let store = store().await;
    mock_in(
        &store,
        TEAM_A,
        "orders",
        "/b",
        None,
        DataPlaneAuthMode::None,
    )
    .await;
    mock_in(
        &store,
        DEFAULT_WORKSPACE,
        "orders",
        "/a",
        None,
        DataPlaneAuthMode::None,
    )
    .await;
    let deps = deps(store.clone(), None);
    let (registry, base) = served(&deps).await;
    let ha = mockingbird::ha::Ha::start_with_bus(
        mb_state::LocalBus::new("test".into()),
        deps.store.clone(),
    )
    .await;
    let _watcher = registry.spawn_watcher(deps, ha.changes());
    assert_eq!(status_of(&base, "/b/orders").await, 200);

    assert!(store.delete_workspace(TEAM_A, true).await.unwrap());
    let deadline = Instant::now() + Duration::from_secs(2);
    while status_of(&base, "/b/orders").await != 404 {
        assert!(Instant::now() < deadline, "/b still mounted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        registry.mounted(),
        vec![(
            DEFAULT_WORKSPACE.to_string(),
            "orders".to_string(),
            "/a".to_string()
        )]
    );
    assert_eq!(status_of(&base, "/a/orders").await, 200);
}
