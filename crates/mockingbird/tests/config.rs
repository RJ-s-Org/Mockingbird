#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use mb_store::{
    ContractStatus, ControlStore, DEFAULT_WORKSPACE, LlmProfile, NewContractVersion, NewMock,
    ResourceSource, Scenario, ScenarioRules,
};
use mockingbird::config::{App, ServeArgs, apply_flags, bind_admin, bind_grpc, build_app};
use mockingbird::file_config::{self, FileConfig};
use mockingbird::reconcile::reconcile;
use serde_json::{Value, json};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture() -> PathBuf {
    repo().join("fixtures/openapi/orders-3.0.yaml")
}

async fn store() -> Arc<dyn ControlStore> {
    mb_store::connect("sqlite::memory:").await.unwrap()
}

/// A state store for reconciles whose state does not matter.
fn no_state() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new(Duration::from_secs(3600)))
}

/// A temp dir holding `orders.yaml` (the fixture) and `mockingbird.yaml`.
struct Workspace {
    dir: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(fixture(), dir.path().join("orders.yaml")).unwrap();
        std::fs::copy(fixture(), dir.path().join("billing.yaml")).unwrap();
        Self { dir }
    }

    fn config_path(&self) -> PathBuf {
        self.dir.path().join("mockingbird.yaml")
    }

    fn write(&self, yaml: &str) {
        write_atomically(&self.config_path(), yaml);
    }

    fn load(&self, yaml: &str) -> FileConfig {
        self.write(yaml);
        file_config::load(&self.config_path()).unwrap()
    }
}

/// Writes through a temp file and a rename, as editors do.
fn write_atomically(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

const BASIC: &str = r#"
llm:
  profiles:
    main:
      base_url: http://127.0.0.1:1
      model: m
      api_key: env:SOME_KEY
mocks:
  orders:
    contract: orders.yaml
    llm_profile: main
    scenarios:
      out-of-stock:
        instructions: Everything is out of stock.
        status: 409
        latency: 250ms
        set: {status: pending}
        operations: [createOrder]
"#;

const TWO_MOCKS: &str = r#"
llm:
  profiles:
    main:
      base_url: http://127.0.0.1:1
      model: m
    spare:
      base_url: http://127.0.0.1:2
      model: m
mocks:
  orders:
    contract: orders.yaml
    llm_profile: main
  billing:
    contract: billing.yaml
"#;

#[test]
fn loads_full_example_config() {
    let cfg = file_config::load(&repo().join("docs/examples/mockingbird.yaml")).unwrap();

    assert_eq!(cfg.server.host.to_string(), "127.0.0.1");
    assert_eq!(cfg.server.port, 8080);
    assert_eq!(cfg.server.admin_port, 9090);
    assert_eq!(cfg.server.grpc_port, 50051);
    assert!(cfg.server.grpc_port_explicit);
    assert!(!cfg.server.insecure_no_auth);
    assert_eq!(
        cfg.server.api_secret_refs,
        ["env:MB_LLM_*", "env:*_API_KEY"]
    );

    assert_eq!(cfg.storage.url, "sqlite::memory:");
    assert_eq!(cfg.storage.state_url.as_deref(), Some("memory"));
    assert_eq!(cfg.storage.session_ttl, Duration::from_secs(24 * 3600));

    assert_eq!(cfg.llm.default_profile.as_deref(), Some("deepseek"));
    assert_eq!(cfg.llm.profiles.len(), 2);
    let p = &cfg.llm.profiles["deepseek"];
    assert_eq!(p.base_url, "https://api.deepseek.com");
    assert_eq!(p.model, "deepseek-chat");
    assert_eq!(p.api_key.as_deref(), Some("env:DEEPSEEK_API_KEY"));
    assert!(!p.json_schema);
    assert_eq!(p.timeout, Duration::from_secs(3));
    assert!((p.temperature - 0.7).abs() < f32::EPSILON);
    assert_eq!(p.max_tokens, 2048);
    assert_eq!(p.fallback.as_deref(), Some("local"));
    let local = &cfg.llm.profiles["local"];
    assert!(local.json_schema);
    assert_eq!(local.api_key, None);
    assert_eq!(local.fallback, None);
    assert_eq!(local.timeout, Duration::from_secs(10));

    assert_eq!(cfg.observability.request_log_retention, 1000);
    assert_eq!(
        cfg.observability.redact_fields,
        [
            "password",
            "token",
            "secret",
            "authorization",
            "apiKey",
            "api_key"
        ]
    );

    let m = &cfg.mocks["orders"];
    assert!(m.contract.is_absolute(), "{}", m.contract.display());
    assert!(m.contract.exists(), "{}", m.contract.display());
    assert_eq!(m.route_prefix.as_deref(), Some("/orders-api"));
    assert!(m.persona.as_deref().unwrap().contains("hiking"));
    assert_eq!(m.llm_profile.as_deref(), Some("deepseek"));
    assert!(m.header_overrides);
    let s = &m.scenarios["out-of-stock"];
    assert_eq!(s.status, Some(409));
    assert_eq!(s.latency, Some(Duration::from_millis(250)));
    assert_eq!(s.set, Some(json!({"status": "pending"})));
    assert_eq!(s.operations, ["createOrder"]);
    assert_eq!(
        m.scenarios["slow"].latency,
        Some(Duration::from_secs(2)),
        "scenario defaults"
    );
    assert_eq!(s.stream_items, None);

    let p = &cfg.protocols;
    assert_eq!(p.graphql.path, "/graphql");
    assert!(p.graphql.introspection);
    assert_eq!(p.graphql.max_depth, 15);
    assert_eq!(p.graphql.max_complexity, 1000);
    assert_eq!(p.stream.default_items, 5);
    assert_eq!(p.stream.max_items, 50);
    assert_eq!(p.stream.item_interval, Duration::ZERO);
    assert_eq!(p.sse.keepalive, Duration::from_secs(15));
    assert_eq!(p.grpc.max_message_bytes, 4 * 1024 * 1024);

    let ticks = &cfg.mocks["events"].scenarios["trickle"];
    assert_eq!(ticks.stream_items, Some(3));
    assert_eq!(ticks.stream_item_interval, Some(Duration::from_millis(500)));
}

#[test]
fn defaults_and_unknown_keys() {
    let ws = Workspace::new();
    let cfg = ws.load("mocks:\n  orders:\n    contract: orders.yaml\n");
    assert_eq!(cfg.server.port, 8080);
    assert_eq!(cfg.server.admin_port, 9090);
    assert_eq!(
        cfg.server.api_secret_refs,
        mb_admin::DEFAULT_API_SECRET_REFS
    );
    assert_eq!(cfg.storage.url, "sqlite::memory:");
    assert_eq!(cfg.storage.state_url, None);
    assert_eq!(cfg.observability.request_log_retention, 1000);
    assert_eq!(cfg.observability.redact_fields.len(), 6);
    assert!(cfg.mocks["orders"].header_overrides);

    ws.write("server:\n  prot: 1\n");
    let err = file_config::load(&ws.config_path()).unwrap_err();
    assert!(format!("{err:#}").contains("prot"), "{err:#}");
}

#[test]
fn env_overrides_file_and_flags_override_env() {
    let ws = Workspace::new();
    let mut cfg = ws.load(
        r#"
server: {host: 127.0.0.1, port: 1000, admin_port: 1001}
storage: {url: "sqlite::memory:"}
llm:
  default_profile: default
  profiles:
    default: {base_url: "http://file", model: file-model, api_key: "file:/run/key"}
"#,
    );
    let env: HashMap<&str, &str> = HashMap::from([
        ("MB_HOST", "127.0.0.2"),
        ("MB_PORT", "2000"),
        ("MB_ADMIN_PORT", "2001"),
        ("MB_STORAGE_URL", "sqlite://env.db"),
        ("MB_LLM_MODEL", "env-model"),
        ("MB_LLM_API_KEY", "raw-secret-value"),
    ]);
    file_config::apply_env_from(&mut cfg, |k| env.get(k).map(|v| v.to_string())).unwrap();
    assert_eq!(cfg.server.host.to_string(), "127.0.0.2");
    assert_eq!(cfg.server.port, 2000);
    assert_eq!(cfg.server.admin_port, 2001);
    assert_eq!(cfg.storage.url, "sqlite://env.db");
    let p = &cfg.llm.profiles["default"];
    assert_eq!(p.model, "env-model");
    assert_eq!(
        p.base_url, "http://file",
        "unset MB_LLM_* keep the file value"
    );
    // Only the reference is kept, never the raw key.
    assert_eq!(p.api_key.as_deref(), Some("env:MB_LLM_API_KEY"));

    let args = ServeArgs {
        config: Some(ws.config_path()),
        port: Some(3000),
        storage_url: Some("sqlite://flag.db".into()),
        llm_timeout: Some("9s".parse().unwrap()),
        ..ServeArgs::default()
    };
    apply_flags(&mut cfg, &args);
    assert_eq!(cfg.server.port, 3000);
    assert_eq!(cfg.server.admin_port, 2001, "env value kept without a flag");
    assert_eq!(cfg.server.host.to_string(), "127.0.0.2");
    assert_eq!(cfg.storage.url, "sqlite://flag.db");
    assert_eq!(cfg.llm.profiles["default"].timeout, Duration::from_secs(9));

    let mut bad = FileConfig::default();
    let err = file_config::apply_env_from(&mut bad, |k| (k == "MB_PORT").then(|| "x".into()))
        .unwrap_err();
    assert!(format!("{err:#}").contains("MB_PORT"), "{err:#}");
}

#[test]
fn env_llm_vars_create_default_profile() {
    let mut cfg = FileConfig::default();
    file_config::apply_env_from(&mut cfg, |k| {
        (k == "MB_LLM_BASE_URL").then(|| "http://env".to_string())
    })
    .unwrap();
    let p = &cfg.llm.profiles["default"];
    assert_eq!(p.base_url, "http://env");
    assert_eq!(p.model, "deepseek-chat");
    assert_eq!(cfg.llm.default_profile.as_deref(), Some("default"));
}

#[tokio::test]
async fn reconcile_creates_and_activates() {
    let ws = Workspace::new();
    let cfg = ws.load(BASIC);
    let store = store().await;

    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert_eq!(report.created, ["orders"]);
    assert!(report.updated.is_empty() && report.deleted.is_empty());
    assert_eq!(report.new_versions, [("orders".to_string(), 1)]);

    let mock = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mock.source, ResourceSource::File);
    assert_eq!(mock.route_prefix, "/orders");
    assert_eq!(mock.llm_profile.as_deref(), Some("main"));
    let active = store.active_contract(mock.id).await.unwrap().unwrap();
    assert_eq!(active.version, 1);
    assert_eq!(active.status, ContractStatus::Active);

    let scenarios = store.list_scenarios(mock.id).await.unwrap();
    assert_eq!(scenarios.len(), 1);
    let s = &scenarios[0];
    assert_eq!(s.name, "out-of-stock");
    assert_eq!(s.source, ResourceSource::File);
    assert_eq!(s.rules.status, Some(409));
    assert_eq!(s.rules.latency_ms, Some(250));
    assert_eq!(s.rules.operations, ["createOrder"]);

    let profile = store
        .get_profile(DEFAULT_WORKSPACE, "main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(profile.source, ResourceSource::File);
    assert_eq!(profile.secret_ref.as_deref(), Some("env:SOME_KEY"));
    assert_eq!(profile.timeout_ms, 3000);

    // Applying the same file again changes nothing.
    let again = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert!(again.created.is_empty(), "{again:?}");
    assert!(again.updated.is_empty(), "{again:?}");
    assert!(again.new_versions.is_empty(), "{again:?}");
}

#[tokio::test]
async fn reconcile_new_contract_digest_new_version() {
    let ws = Workspace::new();
    let cfg = ws.load(BASIC);
    let store = store().await;
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();

    let mut source = std::fs::read_to_string(fixture()).unwrap();
    source.push_str("\n# edited\n");
    std::fs::write(ws.dir.path().join("orders.yaml"), source).unwrap();

    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert_eq!(report.updated, ["orders"]);
    assert_eq!(report.new_versions, [("orders".to_string(), 2)]);
    let mock = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    let active = store.active_contract(mock.id).await.unwrap().unwrap();
    assert_eq!(active.version, 2);
    assert_eq!(
        store.list_contract_versions(mock.id).await.unwrap().len(),
        2
    );
}

async fn api_mock(store: &Arc<dyn ControlStore>, name: &str) -> mb_store::Mock {
    let mock = store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: name.into(),
                route_prefix: None,
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
            },
        )
        .await
        .unwrap();
    let source = std::fs::read(fixture()).unwrap();
    let bundle = mb_import::compile(&source).unwrap();
    let v = store
        .add_contract_version(
            mock.id,
            NewContractVersion {
                source,
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

fn api_profile(name: &str) -> LlmProfile {
    LlmProfile {
        workspace: DEFAULT_WORKSPACE.into(),
        name: name.into(),
        base_url: "http://api".into(),
        model: "m".into(),
        secret_ref: None,
        json_schema: false,
        temperature: 0.7,
        timeout_ms: 3000,
        max_tokens: 2048,
        fallback_profile: None,
        source: ResourceSource::Api,
    }
}

#[tokio::test]
async fn reconcile_keeps_api_resources() {
    let ws = Workspace::new();
    let store = store().await;
    api_mock(&store, "manual").await;
    store.put_profile(&api_profile("api-prof")).await.unwrap();

    // A file mock may use an API-owned profile.
    let cfg = ws.load("mocks:\n  orders:\n    contract: orders.yaml\n    llm_profile: api-prof\n");
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();

    // An API scenario added to the file mock survives reconciles.
    let orders = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    store
        .put_scenario(&Scenario {
            mock_id: orders.id,
            name: "api-scenario".into(),
            instructions: None,
            rules: ScenarioRules::default(),
            source: ResourceSource::Api,
        })
        .await
        .unwrap();

    let cfg = ws.load("mocks: {}\n");
    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert_eq!(report.deleted, ["orders"]);
    assert!(
        store
            .get_mock(DEFAULT_WORKSPACE, "manual")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .get_profile(DEFAULT_WORKSPACE, "api-prof")
            .await
            .unwrap()
            .is_some()
    );

    // Declaring a mock or profile whose name an API resource holds fails.
    let cfg = ws.load("mocks:\n  manual:\n    contract: orders.yaml\n");
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("manual"), "{err:#}");
    let cfg = ws.load("llm:\n  profiles:\n    api-prof: {base_url: http://x, model: m}\n");
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("api-prof"), "{err:#}");
    let manual = store
        .get_mock(DEFAULT_WORKSPACE, "manual")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(manual.source, ResourceSource::Api);
    assert_eq!(
        store
            .get_profile(DEFAULT_WORKSPACE, "api-prof")
            .await
            .unwrap()
            .unwrap()
            .base_url,
        "http://api"
    );
}

/// A file mock whose route prefix an API mock holds is rejected, and
/// nothing from that config is applied.
#[tokio::test]
async fn reconcile_rejects_prefix_clash_with_api_mock() {
    let ws = Workspace::new();
    let store = store().await;
    api_mock(&store, "manual").await;

    let cfg = ws.load(
        "llm:\n  profiles:\n    extra: {base_url: http://x, model: m}\nmocks:\n  orders:\n    contract: orders.yaml\n    route_prefix: /manual\n",
    );
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("/manual") && text.contains("manual"),
        "{text}"
    );
    assert!(
        store
            .get_mock(DEFAULT_WORKSPACE, "orders")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_profile(DEFAULT_WORKSPACE, "extra")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn reconcile_deletes_removed_file_mocks() {
    let ws = Workspace::new();
    let store = store().await;
    let report = reconcile(store.as_ref(), no_state().as_ref(), &ws.load(TWO_MOCKS))
        .await
        .unwrap();
    let mut created = report.created.clone();
    created.sort();
    assert_eq!(created, ["billing", "orders"]);

    let cfg = ws.load(
        r#"
llm:
  profiles:
    main: {base_url: "http://127.0.0.1:1", model: m}
mocks:
  orders: {contract: orders.yaml, llm_profile: main}
"#,
    );
    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert_eq!(report.deleted, ["billing"]);
    let names: Vec<String> = store
        .list_mocks(DEFAULT_WORKSPACE)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert_eq!(names, ["orders"]);
    let profiles: Vec<String> = store
        .list_profiles(DEFAULT_WORKSPACE)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(profiles, ["main"], "file profile removed from the file");
}

#[tokio::test]
async fn invalid_config_applies_nothing() {
    let ws = Workspace::new();
    std::fs::write(
        ws.dir.path().join("broken.yaml"),
        "openapi: 3.0.0\npaths: 7\n",
    )
    .unwrap();
    let store = store().await;

    let invalid = [
        // An unknown operation in a scenario; the other mock is valid.
        "mocks:\n  orders: {contract: orders.yaml}\n  billing:\n    contract: billing.yaml\n    scenarios:\n      s: {operations: [noSuchOp]}\n",
        // A missing profile.
        "mocks:\n  orders: {contract: orders.yaml, llm_profile: nope}\n",
        // A missing default profile and fallback.
        "llm:\n  default_profile: nope\n",
        "llm:\n  profiles:\n    a: {base_url: http://x, model: m, fallback: nope}\n",
        // A bad name and a bad prefix.
        "mocks:\n  Orders: {contract: orders.yaml}\n",
        "mocks:\n  orders: {contract: orders.yaml, route_prefix: orders}\n",
        // A contract that does not compile, or does not exist.
        "mocks:\n  orders: {contract: orders.yaml}\n  billing: {contract: broken.yaml}\n",
        "mocks:\n  orders: {contract: missing.yaml}\n",
        // A raw secret instead of a reference.
        "llm:\n  profiles:\n    a: {base_url: http://x, model: m, api_key: sk-123}\n",
        // Two mocks on one prefix.
        "mocks:\n  orders: {contract: orders.yaml, route_prefix: /x}\n  billing: {contract: billing.yaml, route_prefix: /x}\n",
    ];
    for yaml in invalid {
        let cfg = ws.load(yaml);
        assert!(
            reconcile(store.as_ref(), no_state().as_ref(), &cfg)
                .await
                .is_err(),
            "accepted: {yaml}"
        );
        assert!(
            store
                .list_mocks(DEFAULT_WORKSPACE)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .list_profiles(DEFAULT_WORKSPACE)
                .await
                .unwrap()
                .is_empty()
        );
    }

    // A failed reconcile leaves the previous state alone.
    reconcile(store.as_ref(), no_state().as_ref(), &ws.load(BASIC))
        .await
        .unwrap();
    let cfg = ws.load(invalid[0]);
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    let names: Vec<String> = store
        .list_mocks(DEFAULT_WORKSPACE)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert_eq!(names, ["orders"]);
    let orders = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(orders.llm_profile.as_deref(), Some("main"));
}

// ---- through the app ----

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

struct Running {
    data: String,
    admin: String,
    _app: App,
}

async fn start(args: ServeArgs) -> Running {
    let app = build_app(&args, None).await.unwrap();
    Running {
        data: serve(app.data.clone()).await,
        admin: serve(app.admin.clone()).await,
        _app: app,
    }
}

fn config_args(ws: &Workspace, watch: bool) -> ServeArgs {
    ServeArgs {
        config: Some(ws.config_path()),
        offline: true,
        watch,
        ..ServeArgs::default()
    }
}

#[tokio::test]
async fn file_managed_resources_are_read_only() {
    let ws = Workspace::new();
    ws.write(BASIC);
    let app = start(config_args(&ws, false)).await;
    let http = reqwest::Client::new();
    let admin = &app.admin;

    let r = http
        .patch(format!("{admin}/api/v1/mocks/orders"))
        .json(&json!({"persona": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    assert_eq!(r.headers()["content-type"], "application/problem+json");

    let r = http
        .delete(format!("{admin}/api/v1/mocks/orders"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    let r = http
        .put(format!(
            "{admin}/api/v1/mocks/orders/scenarios/out-of-stock"
        ))
        .json(&json!({"status": 500}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    let r = http
        .delete(format!("{admin}/api/v1/llm-profiles/main"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    // The file mock is served by the data plane.
    let r = reqwest::get(format!("{}/orders/orders", app.data))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

async fn scenario_names(admin: &str) -> Vec<String> {
    let r = reqwest::get(format!("{admin}/api/v1/mocks/orders/scenarios"))
        .await
        .unwrap();
    if r.status() != 200 {
        return Vec::new();
    }
    let body: Value = r.json().await.unwrap();
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| body.as_array())
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn watch_reloads_on_change() {
    let ws = Workspace::new();
    ws.write(BASIC);
    let app = start(config_args(&ws, true)).await;
    assert_eq!(scenario_names(&app.admin).await, ["out-of-stock"]);

    // Give the watcher a moment to register before editing.
    tokio::time::sleep(Duration::from_millis(200)).await;
    ws.write(&BASIC.replace("out-of-stock", "sold-out"));

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let names = scenario_names(&app.admin).await;
        if names == ["sold-out"] {
            break;
        }
        assert!(Instant::now() < deadline, "not reloaded: {names:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn invalid_reload_keeps_previous_state() {
    let ws = Workspace::new();
    ws.write(BASIC);
    let app = start(config_args(&ws, true)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    ws.write("mocks: [this is: not: valid\n");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let r = reqwest::get(format!("{}/orders/orders", app.data))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(scenario_names(&app.admin).await, ["out-of-stock"]);

    // A valid edit afterwards is applied.
    ws.write(&BASIC.replace("out-of-stock", "sold-out"));
    let deadline = Instant::now() + Duration::from_secs(3);
    while scenario_names(&app.admin).await != ["sold-out"] {
        assert!(Instant::now() < deadline, "not reloaded after a fix");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn zero_config_matches_p1_behaviour() {
    let args = ServeArgs {
        offline: true,
        contracts: vec![fixture()],
        ..ServeArgs::default()
    };
    let app = start(args).await;

    let r = reqwest::get(format!("{}/orders", app.data)).await.unwrap();
    assert_eq!(r.status(), 200);
    let r = reqwest::get(format!("{}/__mockingbird/healthz", app.data))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = reqwest::get(format!("{}/api/v1/mocks", app.admin))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| body.as_array())
        .cloned()
        .unwrap();
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["route_prefix"], "/");
    assert_eq!(items[0]["source"], "file");

    for p in ["/healthz", "/readyz", "/metrics"] {
        let r = reqwest::get(format!("{}{p}", app.admin)).await.unwrap();
        assert_eq!(r.status(), 200, "{p}");
    }
}

/// Several zero-config contracts are mounted at `/<mock name>`, the
/// sanitized stem, so a file name with a space still serves (as in P1).
#[tokio::test]
async fn zero_config_prefixes_use_sanitized_names() {
    let dir = tempfile::tempdir().unwrap();
    let pet = dir.path().join("pet store.yaml");
    let orders = dir.path().join("orders.yaml");
    std::fs::copy(fixture(), &pet).unwrap();
    std::fs::copy(fixture(), &orders).unwrap();
    let app = start(ServeArgs {
        offline: true,
        contracts: vec![pet, orders],
        ..ServeArgs::default()
    })
    .await;

    for prefix in ["/pet-store", "/orders"] {
        let r = reqwest::get(format!("{}{prefix}/orders", app.data))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{prefix}");
    }
}

const RENAMED_DEFAULT: &str = r#"
llm:
  default_profile: deepseek
  profiles:
    deepseek:
      base_url: http://127.0.0.1:1
      model: m
mocks:
  orders:
    contract: orders.yaml
"#;

/// A reload that renames the default profile (deleting the old one) keeps
/// the mocks that name no profile mounted: they follow the new default.
#[tokio::test]
async fn watch_reload_changes_default_profile() {
    let ws = Workspace::new();
    ws.write(RENAMED_DEFAULT);
    // Online: offline mode would never load the profile.
    let app = start(ServeArgs {
        offline: false,
        ..config_args(&ws, true)
    })
    .await;
    let mounted = |app: &Running| -> Vec<String> {
        app._app
            .registry
            .mounted()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    };
    assert_eq!(mounted(&app), ["orders"]);

    tokio::time::sleep(Duration::from_millis(200)).await;
    ws.write(&RENAMED_DEFAULT.replace("deepseek", "ds"));

    let http = reqwest::Client::new();
    let admin = &app.admin;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let old = http
            .get(format!("{admin}/api/v1/llm-profiles/deepseek"))
            .send()
            .await
            .unwrap()
            .status();
        let new = http
            .get(format!("{admin}/api/v1/llm-profiles/ds"))
            .send()
            .await
            .unwrap()
            .status();
        if old == 404 && new == 200 {
            break;
        }
        assert!(Instant::now() < deadline, "not reloaded: {old} {new}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Let the registry apply the profile changes.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(mounted(&app), ["orders"]);
    let r = reqwest::get(format!("{}/orders/orders", app.data))
        .await
        .unwrap();
    assert_ne!(r.status(), 404);
}

const REDACT_ALPHA: &str = r#"
observability:
  redact_fields: [alpha]
mocks:
  orders:
    contract: orders.yaml
"#;

/// The newest request-log entry's request body after one POST.
async fn post_and_log(app: &Running) -> Value {
    let http = reqwest::Client::new();
    let marker = uuid::Uuid::new_v4().to_string();
    http.post(format!("{}/orders/orders", app.data))
        .json(&json!({"alpha": "a", "beta": "b", "marker": marker}))
        .send()
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let r = http
            .get(format!("{}/api/v1/mocks/orders/requests", app.admin))
            .send()
            .await
            .unwrap();
        let entries: Vec<Value> = r.json().await.unwrap();
        if let Some(e) = entries
            .iter()
            .find(|e| e["request"]["body"]["marker"] == marker.as_str())
        {
            return e["request"]["body"].clone();
        }
        assert!(Instant::now() < deadline, "no request log entry");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn watch_reload_changes_redact_fields() {
    let ws = Workspace::new();
    ws.write(REDACT_ALPHA);
    let app = start(config_args(&ws, true)).await;
    let body = post_and_log(&app).await;
    assert_eq!(body["alpha"], "[redacted]");
    assert_eq!(body["beta"], "b");

    tokio::time::sleep(Duration::from_millis(200)).await;
    ws.write(&REDACT_ALPHA.replace("[alpha]", "[beta]"));

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let body = post_and_log(&app).await;
        if body["beta"] == "[redacted]" {
            assert_eq!(body["alpha"], "a");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "redact_fields not reloaded: {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Only an explicitly chosen admin port must be free: a busy default one
/// is skipped (the data plane still serves).
#[tokio::test]
async fn busy_default_admin_port_is_skipped() {
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = held.local_addr().unwrap();
    assert!(bind_admin(addr, false).await.unwrap().is_none());
    let err = bind_admin(addr, true).await.unwrap_err();
    assert!(format!("{err:#}").contains(&addr.to_string()), "{err:#}");
    drop(held);
    assert!(bind_admin(addr, false).await.unwrap().is_some());
}

#[test]
fn admin_port_explicitness() {
    let ws = Workspace::new();
    assert!(!ws.load("mocks: {}\n").server.admin_port_explicit);
    assert!(!ws.load("server:\n  port: 1\n").server.admin_port_explicit);
    let cfg = ws.load("server:\n  admin_port: 9191\n");
    assert!(cfg.server.admin_port_explicit);
    assert_eq!(cfg.server.admin_port, 9191);

    let mut cfg = FileConfig::default();
    file_config::apply_env_from(&mut cfg, |k| {
        (k == "MB_ADMIN_PORT").then(|| "9192".to_string())
    })
    .unwrap();
    assert!(cfg.server.admin_port_explicit);

    let mut cfg = FileConfig::default();
    apply_flags(
        &mut cfg,
        &ServeArgs {
            admin_port: Some(9193),
            ..ServeArgs::default()
        },
    );
    assert!(cfg.server.admin_port_explicit);
}

/// Removing a file mock from the config drops its session state too.
#[tokio::test]
async fn reconcile_deleting_a_mock_resets_its_state() {
    let ws = Workspace::new();
    let store = store().await;
    let state = no_state();
    reconcile(store.as_ref(), state.as_ref(), &ws.load(BASIC))
        .await
        .unwrap();
    let key = SessionKey {
        workspace: DEFAULT_WORKSPACE.into(),
        mock: "orders".into(),
        session: "alice".into(),
    };
    let order = StoredResource {
        resource_type: "orders".into(),
        id: "1".into(),
        data: json!({"id": "1"}),
    };
    state.upsert(&key, order).await.unwrap();

    let report = reconcile(store.as_ref(), state.as_ref(), &ws.load("mocks: {}\n"))
        .await
        .unwrap();
    assert_eq!(report.deleted, ["orders"]);
    assert!(
        state
            .sessions(DEFAULT_WORKSPACE, "orders")
            .await
            .unwrap()
            .is_empty()
    );
}

/// Removing a file profile that API mocks still use succeeds, with a
/// warning naming them.
#[tokio::test]
async fn removing_file_profile_used_by_api_mock_warns() {
    let ws = Workspace::new();
    let store = store().await;
    let cfg = ws.load("llm:\n  profiles:\n    fp: {base_url: http://x, model: m}\n");
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    let mut manual = api_mock(&store, "manual").await;
    manual.llm_profile = Some("fp".into());
    store.update_mock(&manual).await.unwrap();
    api_mock(&store, "unrelated").await;

    let report = reconcile(store.as_ref(), no_state().as_ref(), &ws.load("mocks: {}\n"))
        .await
        .unwrap();
    assert_eq!(report.warnings.len(), 1, "{report:?}");
    assert!(report.warnings[0].contains("\"fp\""), "{report:?}");
    assert!(report.warnings[0].contains("manual"), "{report:?}");
    assert!(!report.warnings[0].contains("unrelated"), "{report:?}");
    assert!(
        store
            .get_profile(DEFAULT_WORKSPACE, "fp")
            .await
            .unwrap()
            .is_none()
    );
}

/// Copies `src` recursively into `dst`.
fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

#[tokio::test]
async fn file_config_contract_directory() {
    let ws = Workspace::new();
    copy_dir(
        &repo().join("fixtures/proto/multi"),
        &ws.dir.path().join("protos"),
    );
    let cfg = ws.load("mocks:\n  orders:\n    contract: protos\n");
    let store = store().await;

    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert_eq!(report.new_versions, [("orders".to_string(), 1)]);
    let again = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert!(again.new_versions.is_empty(), "{again:?}");

    let mock = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    let versions = store.list_contract_versions(mock.id).await.unwrap();
    assert_eq!(versions.len(), 1);
    let active = store.active_contract(mock.id).await.unwrap().unwrap();
    assert_eq!(
        active.source,
        mb_import::pack_dir(&ws.dir.path().join("protos")).unwrap()
    );
    assert!(
        active
            .bundle
            .operations
            .iter()
            .any(|o| o.id == "OrderService.GetOrder")
    );
}

// ---- gRPC port and the protocols section ----

#[test]
fn grpc_port_precedence() {
    let ws = Workspace::new();
    let cfg = ws.load("mocks: {}\n");
    assert_eq!(cfg.server.grpc_port, 50051);
    assert!(!cfg.server.grpc_port_explicit);

    let mut cfg = ws.load("server:\n  grpc_port: 6000\n");
    assert_eq!(cfg.server.grpc_port, 6000);
    assert!(cfg.server.grpc_port_explicit);

    let mut env_only = FileConfig::default();
    file_config::apply_env_from(&mut env_only, |k| {
        (k == "MB_GRPC_PORT").then(|| "6001".to_string())
    })
    .unwrap();
    assert_eq!(env_only.server.grpc_port, 6001);
    assert!(env_only.server.grpc_port_explicit);

    file_config::apply_env_from(&mut cfg, |k| {
        (k == "MB_GRPC_PORT").then(|| "6001".to_string())
    })
    .unwrap();
    assert_eq!(cfg.server.grpc_port, 6001, "env overrides the file");
    apply_flags(&mut cfg, &ServeArgs::default());
    assert_eq!(cfg.server.grpc_port, 6001, "no flag keeps the env value");
    apply_flags(
        &mut cfg,
        &ServeArgs {
            grpc_port: Some(6002),
            ..ServeArgs::default()
        },
    );
    assert_eq!(cfg.server.grpc_port, 6002, "the flag overrides the env");

    let mut flag_only = FileConfig::default();
    apply_flags(
        &mut flag_only,
        &ServeArgs {
            grpc_port: Some(6002),
            ..ServeArgs::default()
        },
    );
    assert!(flag_only.server.grpc_port_explicit);

    let mut bad = FileConfig::default();
    let err = file_config::apply_env_from(&mut bad, |k| (k == "MB_GRPC_PORT").then(|| "x".into()))
        .unwrap_err();
    assert!(format!("{err:#}").contains("MB_GRPC_PORT"), "{err:#}");
}

/// Only an explicitly chosen gRPC port must be free.
#[tokio::test]
async fn busy_default_grpc_port_is_skipped() {
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = held.local_addr().unwrap();
    assert!(bind_grpc(addr, false).await.unwrap().is_none());
    let err = bind_grpc(addr, true).await.unwrap_err();
    assert!(format!("{err:#}").contains(&addr.to_string()), "{err:#}");
    drop(held);
    assert!(bind_grpc(addr, false).await.unwrap().is_some());
}

#[test]
fn protocols_section_defaults_and_overrides() {
    let ws = Workspace::new();
    let cfg = ws.load("mocks: {}\n");
    let (settings, stream) = cfg.protocols.settings();
    assert_eq!(settings.graphql_path, "/graphql");
    assert!(settings.graphql_introspection);
    assert_eq!(settings.graphql_max_depth, 15);
    assert_eq!(settings.graphql_max_complexity, 1000);
    assert_eq!(settings.sse_keepalive, Duration::from_secs(15));
    assert_eq!(settings.ws_keepalive, Duration::from_secs(30));
    assert_eq!(settings.grpc_max_message_bytes, 4_194_304);
    assert_eq!(stream.default_items, 5);
    assert_eq!(stream.max_items, 50);
    assert_eq!(stream.item_interval, Duration::ZERO);

    let cfg = ws.load(
        r#"
protocols:
  graphql: {path: /gql, introspection: false, max_depth: 7, max_complexity: 300}
  stream: {default_items: 3, max_items: 9, item_interval: 40ms}
  sse: {keepalive: 2s}
  grpc: {max_message_bytes: 1024}
"#,
    );
    let (settings, stream) = cfg.protocols.settings();
    assert_eq!(settings.graphql_path, "/gql");
    assert!(!settings.graphql_introspection);
    assert_eq!(settings.graphql_max_depth, 7);
    assert_eq!(settings.graphql_max_complexity, 300);
    assert_eq!(settings.sse_keepalive, Duration::from_secs(2));
    assert_eq!(settings.grpc_max_message_bytes, 1024);
    assert_eq!(stream.default_items, 3);
    assert_eq!(stream.max_items, 9);
    assert_eq!(stream.item_interval, Duration::from_millis(40));

    // A partial section keeps the other defaults.
    let cfg = ws.load("protocols:\n  sse:\n    keepalive: 1s\n");
    assert_eq!(cfg.protocols.graphql.path, "/graphql");
    assert_eq!(cfg.protocols.sse.keepalive, Duration::from_secs(1));

    ws.write("protocols:\n  graphql:\n    depth: 3\n");
    let err = file_config::load(&ws.config_path()).unwrap_err();
    assert!(format!("{err:#}").contains("depth"), "{err:#}");
    ws.write("protocols:\n  soap: {}\n");
    let err = file_config::load(&ws.config_path()).unwrap_err();
    assert!(format!("{err:#}").contains("soap"), "{err:#}");
}

/// Reads an SSE body until the first keepalive comment, returning the text.
async fn sse_until_keepalive(mut resp: reqwest::Response) -> String {
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !text.contains("keepalive") {
            let chunk = resp.chunk().await.unwrap().unwrap();
            text.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no keepalive in {text:?}"));
    text
}

#[tokio::test]
async fn scenario_stream_settings_reach_runtime() {
    let ws = Workspace::new();
    std::fs::copy(
        repo().join("fixtures/openapi/events-3.1.yaml"),
        ws.dir.path().join("events.yaml"),
    )
    .unwrap();
    ws.write(
        r#"
protocols:
  sse: {keepalive: 300ms}
mocks:
  events:
    contract: events.yaml
    scenarios:
      pair:
        stream_items: 2
"#,
    );
    let app = start(config_args(&ws, false)).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/events/ticks", app.data))
        .header("X-Mockingbird-Scenario", "pair")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = sse_until_keepalive(resp).await;
    assert_eq!(text.matches("event: tick").count(), 2, "{text}");

    // Without the scenario, the configured default count applies.
    let resp = reqwest::get(format!("{}/events/ticks", app.data))
        .await
        .unwrap();
    let text = sse_until_keepalive(resp).await;
    assert_eq!(text.matches("event: tick").count(), 5, "{text}");
}
