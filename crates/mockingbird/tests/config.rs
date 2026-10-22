#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use mb_store::{
    ContractStatus, ControlStore, DEFAULT_WORKSPACE, DataPlaneAuthConfig, DataPlaneAuthMode,
    LlmProfile, NewContractVersion, NewMock, ResourceSource, Scenario, ScenarioRules,
};
use mockingbird::config::{
    App, ServeArgs, apply_flags, bind_admin, bind_grpc, build_app, resolve_config,
};
use mockingbird::file_config::{self, AuthConfig, AuthMode, FileConfig, OidcConfig, ServerConfig};
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
        status: 400
        latency: 250ms
        set: {message: Out of stock}
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
    assert_eq!(s.status, Some(400));
    assert_eq!(s.latency, Some(Duration::from_millis(250)));
    assert_eq!(s.set, Some(json!({"message": "Out of stock"})));
    assert_eq!(s.operation_ids(), ["createOrder"]);
    assert_eq!(
        m.scenarios["slow"].latency,
        Some(Duration::from_secs(2)),
        "scenario defaults"
    );
    assert_eq!(m.workspace, "default");
    assert_eq!(m.data_plane_auth, DataPlaneAuthConfig::default());
    assert_eq!(p.workspace, "default");
    assert_eq!(local.workspace, "default", "profile workspace default");

    let auth = &cfg.auth;
    assert_eq!(auth.mode, AuthMode::None);
    assert_eq!(auth.external_url, "http://127.0.0.1:9090");
    assert_eq!(auth.session_ttl, Duration::from_secs(12 * 3600));
    assert!(auth.sysadmin_subjects.is_empty());
    assert!(auth.public_metrics);
    let oidc = auth.oidc.as_ref().unwrap();
    assert_eq!(oidc.issuer, "http://localhost:8080/auth/v1");
    assert_eq!(oidc.client_id, "mockingbird");
    assert_eq!(
        oidc.client_secret.as_deref(),
        Some("env:MB_OIDC_CLIENT_SECRET")
    );
    assert_eq!(oidc.scopes, ["openid", "profile", "email", "groups"]);
    assert_eq!(oidc.roles_claim, "groups");
    assert_eq!(oidc.group_prefix, "mb");
    assert_eq!(oidc.audience.as_deref(), Some("mockingbird"));

    assert_eq!(cfg.audit.retention_days, 365);
    assert_eq!(
        cfg.audit.webhook_url.as_deref(),
        Some("https://audit.example.com/mockingbird")
    );
    assert_eq!(
        cfg.audit.webhook_secret.as_deref(),
        Some("env:MB_AUDIT_WEBHOOK_SECRET")
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

    assert_eq!(cfg.brokers.len(), 1);
    assert_eq!(cfg.brokers["local"].kind, "memory");
    let brokers = file_config::broker_configs(&cfg).unwrap();
    assert_eq!(brokers[0].name, "local");

    let ticks = &cfg.mocks["events"].scenarios["trickle"];
    assert_eq!(ticks.stream_items, Some(3));
    assert_eq!(ticks.stream_item_interval, Some(Duration::from_millis(500)));

    let ev = &cfg.mocks["order-events"];
    assert_eq!(ev.broker.as_deref(), Some("local"));
    let created = &ev.channels["orderCreated"];
    assert!(created.enabled);
    let triggers: Vec<mb_mcm::Trigger> = created
        .triggers
        .as_ref()
        .unwrap()
        .iter()
        .map(|t| t.to_trigger())
        .collect();
    assert_eq!(
        triggers,
        [
            mb_mcm::Trigger::Schedule {
                every: Duration::from_secs(30)
            },
            mb_mcm::Trigger::Cron {
                expr: "0 9 * * 1-5".into()
            },
            mb_mcm::Trigger::OnStateChange {
                resource_type: "orders".into(),
                kind: mb_mcm::StateChangeKind::Upserted
            },
            mb_mcm::Trigger::Manual,
        ]
    );
    assert!(!ev.channels["orderCancelled"].enabled);
}

#[tokio::test]
async fn example_config_reconciles() {
    let cfg = file_config::load(&repo().join("docs/examples/mockingbird.yaml")).unwrap();
    let store = store().await;
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    let mock = store
        .get_mock(DEFAULT_WORKSPACE, "order-events")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.list_channel_settings(mock.id).await.unwrap().len(), 2);
}

#[test]
fn auth_defaults() {
    let ws = Workspace::new();
    let cfg = ws.load("mocks:\n  orders:\n    contract: orders.yaml\n");
    assert_eq!(cfg.auth.mode, AuthMode::None);
    assert_eq!(cfg.auth.external_url, "http://127.0.0.1:9090");
    assert_eq!(cfg.auth.session_ttl, Duration::from_secs(12 * 3600));
    assert!(cfg.auth.sysadmin_subjects.is_empty());
    assert!(cfg.auth.public_metrics);
    assert_eq!(cfg.auth.oidc, None);
    assert_eq!(cfg.audit.retention_days, 365);
    assert_eq!(cfg.audit.webhook_url, None);
    assert_eq!(cfg.audit.webhook_secret, None);
    assert_eq!(cfg.mocks["orders"].workspace, "default");
    assert_eq!(
        cfg.mocks["orders"].data_plane_auth,
        DataPlaneAuthConfig::default()
    );

    // OIDC defaults.
    let cfg = ws.load("auth:\n  mode: oidc\n  oidc:\n    issuer: http://idp\n    client_id: mb\n");
    assert_eq!(cfg.auth.mode, AuthMode::Oidc);
    let oidc = cfg.auth.oidc.unwrap();
    assert_eq!(oidc.client_secret, None);
    assert_eq!(oidc.scopes, ["openid", "profile", "email", "groups"]);
    assert_eq!(oidc.roles_claim, "groups");
    assert_eq!(oidc.group_prefix, "mb");
    assert_eq!(oidc.audience, None);

    // Unknown keys are rejected in the new sections too.
    for yaml in [
        "auth:\n  mdoe: oidc\n",
        "auth:\n  oidc: {issuer: x, client_id: y, scope: [openid]}\n",
        "audit:\n  retention: 3\n",
        "mocks:\n  orders: {contract: orders.yaml, data_plane_auth: {mod: api_key}}\n",
        "auth:\n  mode: saml\n",
    ] {
        ws.write(yaml);
        assert!(file_config::load(&ws.config_path()).is_err(), "{yaml}");
    }
}

#[test]
fn session_ttl_and_retention_days_are_bounded() {
    let ws = Workspace::new();
    for yaml in [
        "auth:\n  session_ttl: 31d\n",
        "audit:\n  retention_days: 0\n",
        "audit:\n  retention_days: 3651\n",
        "audit:\n  retention_days: 4294967295\n",
    ] {
        ws.write(yaml);
        let err = file_config::load(&ws.config_path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("auth.session_ttl") || msg.contains("audit.retention_days"),
            "{yaml}: {msg}"
        );
    }
    let cfg = ws.load("auth:\n  session_ttl: 30d\naudit:\n  retention_days: 3650\n");
    assert_eq!(cfg.auth.session_ttl, Duration::from_secs(30 * 24 * 60 * 60));
    assert_eq!(cfg.audit.retention_days, 3650);
    let cfg = ws.load("audit:\n  retention_days: 1\n");
    assert_eq!(cfg.audit.retention_days, 1);
}

fn oidc_args(ws: &Workspace) -> ServeArgs {
    ServeArgs {
        config: Some(ws.config_path()),
        offline: true,
        ..ServeArgs::default()
    }
}

#[test]
fn oidc_mode_requires_oidc_section_and_external_url() {
    let ws = Workspace::new();
    ws.write("auth:\n  mode: oidc\n");
    let err = resolve_config(&oidc_args(&ws), None).unwrap_err();
    assert!(format!("{err:#}").contains("auth.oidc"), "{err:#}");

    for url in ["", "127.0.0.1:9090", "ftp://host", "http://", "not a url"] {
        ws.write(&format!(
            "auth:\n  mode: oidc\n  external_url: \"{url}\"\n  oidc: {{issuer: http://idp, client_id: mb}}\n"
        ));
        let err = resolve_config(&oidc_args(&ws), None).unwrap_err();
        assert!(
            format!("{err:#}").contains("external_url"),
            "{url}: {err:#}"
        );
        let cfg = file_config::load(&ws.config_path()).unwrap();
        assert!(cfg.server.check_bind(&cfg.auth).is_err(), "{url}");
    }

    ws.write(
        "auth:\n  mode: oidc\n  external_url: https://mb.example.com\n  oidc: {issuer: http://idp, client_id: mb}\n",
    );
    let cfg = resolve_config(&oidc_args(&ws), None).unwrap();
    assert!(cfg.server.check_bind(&cfg.auth).is_ok());

    // `none` mode ignores the OIDC settings.
    ws.write("auth:\n  mode: none\n  external_url: nonsense\n");
    resolve_config(&oidc_args(&ws), None).unwrap();
}

#[test]
fn none_mode_public_bind_requires_flag() {
    let auth = AuthConfig::default();
    let mut server = ServerConfig {
        host: "0.0.0.0".parse().unwrap(),
        ..ServerConfig::default()
    };
    let err = server.check_bind(&auth).unwrap_err();
    assert!(err.contains("--insecure-no-auth"), "{err}");
    server.insecure_no_auth = true;
    assert!(server.check_bind(&auth).is_ok());
    assert!(ServerConfig::default().check_bind(&auth).is_ok());
}

#[test]
fn oidc_mode_public_bind_allowed() {
    let auth = AuthConfig {
        mode: AuthMode::Oidc,
        external_url: "https://mb.example.com".into(),
        oidc: Some(OidcConfig {
            issuer: "https://idp.example.com".into(),
            client_id: "mb".into(),
            client_secret: None,
            scopes: vec!["openid".into()],
            roles_claim: "groups".into(),
            group_prefix: "mb".into(),
            audience: None,
        }),
        ..AuthConfig::default()
    };
    let server = ServerConfig {
        host: "0.0.0.0".parse().unwrap(),
        ..ServerConfig::default()
    };
    assert!(server.check_bind(&auth).is_ok());
    let missing = AuthConfig {
        oidc: None,
        ..auth.clone()
    };
    assert!(server.check_bind(&missing).is_err());
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
    assert_eq!(s.rules.status, Some(400));
    assert_eq!(s.rules.latency_ms, Some(250));
    assert_eq!(s.rules.operations, ["createOrder"]);

    let profile = store
        .get_profile(DEFAULT_WORKSPACE, "main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(profile.source, ResourceSource::File);
    assert_eq!(profile.secret_ref.as_deref(), Some("env:SOME_KEY"));
    assert_eq!(profile.timeout_ms, 60_000);

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
                chaos: None,
                limits: Default::default(),
                data_plane_auth: Default::default(),
                behavior: Default::default(),
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
        headers: Default::default(),
        extra_body: Default::default(),
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

/// gRPC services are unique across workspaces: two file mocks, or a file
/// mock and an API mock's active contract, may not serve the same one.
#[tokio::test]
async fn reconcile_rejects_grpc_service_clashes() {
    let ws = Workspace::new();
    let proto = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/proto/route_guide.proto"),
    )
    .unwrap();
    std::fs::write(ws.dir.path().join("routes.proto"), &proto).unwrap();
    let store = store().await;

    let cfg = ws.load(
        "mocks:\n  a:\n    contract: routes.proto\n  b:\n    workspace: team-b\n    contract: routes.proto\n",
    );
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("routeguide.RouteGuide"), "{text}");
    assert!(store.list_all_mocks().await.unwrap().is_empty());

    // An API mock's active contract holds the service.
    let api = store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: "api-routes".into(),
                route_prefix: None,
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: Default::default(),
                behavior: Default::default(),
            },
        )
        .await
        .unwrap();
    let v = store
        .add_contract_version(
            api.id,
            NewContractVersion {
                bundle: mb_import::compile(&proto).unwrap(),
                source: proto.clone(),
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await
        .unwrap();
    store
        .activate_contract_version(api.id, v.version)
        .await
        .unwrap();
    let cfg = ws.load("mocks:\n  a:\n    workspace: team-b\n    contract: routes.proto\n");
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("routeguide.RouteGuide") && text.contains("api-routes"),
        "{text}"
    );
    assert_eq!(store.list_all_mocks().await.unwrap().len(), 1);

    // Different services are fine.
    let cfg = ws.load("mocks:\n  a:\n    workspace: team-b\n    contract: orders.yaml\n");
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
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
async fn scenario_status_must_be_declared_like_the_admin_api() {
    let ws = Workspace::new();
    let store = store().await;
    let scenario = |rules: &str| {
        format!("mocks:\n  orders:\n    contract: orders.yaml\n    scenarios:\n      s: {rules}\n")
    };

    // createOrder declares 201 and 400: a 409 could never apply.
    let err = reconcile(
        store.as_ref(),
        no_state().as_ref(),
        &ws.load(&scenario("{status: 409, operations: [createOrder]}")),
    )
    .await
    .unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "mock \"orders\", scenario \"s\": status 409 is not declared by operation \
         createOrder of the active contract, so the scenario could never answer it; declare a \
         409 response, or use a status it declares (201, 400) or a 5xx"
    );
    // Without operations, no operation declares 409 either.
    reconcile(
        store.as_ref(),
        no_state().as_ref(),
        &ws.load(&scenario("{status: 409}")),
    )
    .await
    .unwrap_err();

    // A declared status, one declared by any operation, or a 5xx is fine.
    for rules in [
        "{status: 400, operations: [createOrder]}",
        "{status: 404}",
        "{status: 503, operations: [createOrder]}",
    ] {
        reconcile(
            store.as_ref(),
            no_state().as_ref(),
            &ws.load(&scenario(rules)),
        )
        .await
        .unwrap_or_else(|e| panic!("{rules}: {e:#}"));
    }
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
        // A scenario status its operation does not declare.
        "mocks:\n  orders:\n    contract: orders.yaml\n    scenarios:\n      s: {status: 409, operations: [createOrder]}\n",
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

/// Brokers are read at startup: a reload whose `brokers:` differ is
/// rejected as a whole (nothing else in it is applied), and the watch keeps
/// running.
#[tokio::test]
async fn reload_changing_brokers_is_rejected() {
    let with_brokers = |brokers: &str, scenario: &str| {
        format!(
            "brokers:\n{brokers}{}",
            BASIC.replace("out-of-stock", scenario)
        )
    };
    let local = "  local:\n    kind: memory\n";
    let ws = Workspace::new();
    ws.write(&with_brokers(local, "out-of-stock"));
    let app = start(config_args(&ws, true)).await;
    assert_eq!(app._app.brokers.names(), ["local"]);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Adding a broker (with another change) is not applied.
    ws.write(&with_brokers(
        &format!("{local}  extra:\n    kind: memory\n"),
        "sold-out",
    ));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(scenario_names(&app.admin).await, ["out-of-stock"]);
    assert_eq!(app._app.brokers.names(), ["local"]);

    // Changing a broker's settings is rejected too.
    ws.write(&with_brokers(
        "  local:\n    kind: memory\n    client_id_prefix: other\n",
        "sold-out",
    ));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(scenario_names(&app.admin).await, ["out-of-stock"]);

    // The same brokers again: the watch is still running and applies it.
    ws.write(&with_brokers(local, "sold-out"));
    let deadline = Instant::now() + Duration::from_secs(3);
    while scenario_names(&app.admin).await != ["sold-out"] {
        assert!(Instant::now() < deadline, "not reloaded after the fix");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[test]
fn broker_change_check_names_the_restart() {
    let ws = Workspace::new();
    let before = ws.load("brokers:\n  local:\n    kind: memory\n");
    let initial = file_config::broker_configs(&before).unwrap();
    assert!(mockingbird::config::ensure_brokers_unchanged(&initial, &before).is_ok());
    for changed in [
        "brokers:\n  local:\n    kind: memory\n  extra:\n    kind: memory\n",
        "brokers: {}\n",
        "brokers:\n  local:\n    kind: memory\n    client_id_prefix: x\n",
    ] {
        let cfg = ws.load(changed);
        let e = mockingbird::config::ensure_brokers_unchanged(&initial, &cfg).unwrap_err();
        assert!(
            format!("{e:#}").contains("brokers changed: restart required"),
            "{changed}: {e:#}"
        );
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
            .map(|(_, name, _)| name)
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

// ---- workspaces as code ----

const TEAM_A: &str = "team-a";

#[tokio::test]
async fn reconcile_creates_file_workspaces() {
    let ws = Workspace::new();
    let store = store().await;
    let cfg = ws.load(
        r#"
llm:
  profiles:
    main: {workspace: team-a, base_url: "http://127.0.0.1:1", model: m}
    shared: {base_url: "http://127.0.0.1:2", model: m}
mocks:
  orders: {workspace: team-a, contract: orders.yaml, llm_profile: main}
  billing: {workspace: team-b, contract: billing.yaml}
"#,
    );
    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    let mut created = report.created.clone();
    created.sort();
    assert_eq!(created, ["team-a/orders", "team-b/billing"]);

    let workspaces = store.list_workspaces().await.unwrap();
    let names: Vec<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(names, ["default", "team-a", "team-b"]);
    assert_eq!(workspaces[1].display_name, "team-a");
    let orders = store.get_mock(TEAM_A, "orders").await.unwrap().unwrap();
    assert_eq!(orders.source, ResourceSource::File);
    assert_eq!(orders.llm_profile.as_deref(), Some("main"));
    assert!(store.get_profile(TEAM_A, "main").await.unwrap().is_some());
    assert!(
        store
            .get_profile(DEFAULT_WORKSPACE, "main")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_profile(DEFAULT_WORKSPACE, "shared")
            .await
            .unwrap()
            .is_some()
    );

    // Removing everything deletes the file resources but keeps the workspaces.
    let report = reconcile(store.as_ref(), no_state().as_ref(), &ws.load(""))
        .await
        .unwrap();
    assert_eq!(report.deleted, ["team-a/orders", "team-b/billing"]);
    assert_eq!(store.list_workspaces().await.unwrap().len(), 3);
    assert!(store.list_profiles(TEAM_A).await.unwrap().is_empty());

    // An invalid workspace name is rejected before anything is applied.
    let cfg = ws.load("mocks:\n  orders: {workspace: Team_A, contract: orders.yaml}\n");
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("Team_A"), "{err:#}");
    assert_eq!(store.list_workspaces().await.unwrap().len(), 3);

    // A mock's profile must exist in the mock's own workspace.
    let cfg = ws.load(
        r#"
llm:
  profiles:
    main: {base_url: "http://127.0.0.1:1", model: m}
mocks:
  orders: {workspace: team-c, contract: orders.yaml, llm_profile: main}
"#,
    );
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("main"), "{err:#}");
    assert!(store.get_workspace("team-c").await.unwrap().is_none());
}

/// Mocks are keyed by (workspace, name): a file mock does not clash with a
/// same-named mock in another workspace, and moving a file mock to another
/// workspace replaces it there.
#[tokio::test]
async fn reconcile_same_name_two_workspaces() {
    let ws = Workspace::new();
    let store = store().await;
    store.create_workspace(TEAM_A, "Team A").await.unwrap();
    // An API mock `orders` in team-a and an API profile `main` there.
    store
        .create_mock(
            TEAM_A,
            NewMock {
                name: "orders".into(),
                route_prefix: Some("/a/orders".into()),
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: DataPlaneAuthConfig::default(),
                behavior: Default::default(),
            },
        )
        .await
        .unwrap();
    store
        .put_profile(&LlmProfile {
            workspace: TEAM_A.into(),
            ..api_profile("main")
        })
        .await
        .unwrap();

    // File `orders` and file profile `main` in `default` are fine.
    reconcile(store.as_ref(), no_state().as_ref(), &ws.load(BASIC))
        .await
        .unwrap();
    let file = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(file.source, ResourceSource::File);
    let api = store.get_mock(TEAM_A, "orders").await.unwrap().unwrap();
    assert_eq!(api.source, ResourceSource::Api);
    assert_eq!(
        store
            .get_profile(TEAM_A, "main")
            .await
            .unwrap()
            .unwrap()
            .source,
        ResourceSource::Api
    );

    // But declaring `orders` in team-a clashes with the API mock there.
    let cfg =
        ws.load("mocks:\n  orders: {workspace: team-a, contract: orders.yaml, route_prefix: /x}\n");
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("API"), "{err:#}");

    // Moving the file mock to team-b deletes it from default only.
    let cfg = ws.load("mocks:\n  orders: {workspace: team-b, contract: orders.yaml}\n");
    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert_eq!(report.created, ["team-b/orders"]);
    assert_eq!(report.deleted, ["orders"]);
    assert!(
        store
            .get_mock(DEFAULT_WORKSPACE, "orders")
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.get_mock("team-b", "orders").await.unwrap().is_some());
    assert!(store.get_mock(TEAM_A, "orders").await.unwrap().is_some());
}

#[tokio::test]
async fn reconcile_prefix_collision_across_workspaces_errors() {
    let ws = Workspace::new();
    let store = store().await;
    store.create_workspace(TEAM_A, "Team A").await.unwrap();
    store
        .create_mock(
            TEAM_A,
            NewMock {
                name: "manual".into(),
                route_prefix: Some("/shared".into()),
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: DataPlaneAuthConfig::default(),
                behavior: Default::default(),
            },
        )
        .await
        .unwrap();

    // With an API mock in another workspace.
    let cfg = ws.load(
        "llm:\n  profiles:\n    extra: {base_url: http://x, model: m}\nmocks:\n  orders: {workspace: team-b, contract: orders.yaml, route_prefix: /shared}\n",
    );
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("/shared"), "{err:#}");
    assert!(store.get_workspace("team-b").await.unwrap().is_none());
    assert!(
        store
            .get_profile(DEFAULT_WORKSPACE, "extra")
            .await
            .unwrap()
            .is_none()
    );

    // Between two file mocks in different workspaces.
    let cfg = ws.load(
        "mocks:\n  orders: {workspace: team-b, contract: orders.yaml, route_prefix: /p}\n  billing: {contract: billing.yaml, route_prefix: /p}\n",
    );
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("/p"), "{err:#}");
    assert!(
        store
            .list_mocks(DEFAULT_WORKSPACE)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn reconcile_applies_data_plane_auth() {
    let ws = Workspace::new();
    let store = store().await;
    let cfg = ws.load(
        "mocks:\n  orders:\n    contract: orders.yaml\n    data_plane_auth: {mode: api_key, header: x-key}\n",
    );
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    let mock = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mock.data_plane_auth.mode, DataPlaneAuthMode::ApiKey);
    assert_eq!(mock.data_plane_auth.header, "x-key");

    // Changing it updates the mock.
    let cfg = ws.load(
        "mocks:\n  orders:\n    contract: orders.yaml\n    data_plane_auth: {mode: passthrough_validate}\n",
    );
    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert_eq!(report.updated, ["orders"]);
    let mock = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        mock.data_plane_auth.mode,
        DataPlaneAuthMode::PassthroughValidate
    );
    assert_eq!(mock.data_plane_auth.header, "x-api-key");

    // An invalid header name is rejected.
    let cfg = ws.load(
        "mocks:\n  orders:\n    contract: orders.yaml\n    data_plane_auth: {mode: api_key, header: \"bad header\"}\n",
    );
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("header"), "{err:#}");
}

/// The Compose stack's config (deploy/compose) parses and is a valid `oidc`
/// config for an all-interfaces bind.
#[test]
fn compose_config_is_a_valid_oidc_config() {
    let cfg = file_config::load(&repo().join("deploy/compose/mockingbird.yaml")).unwrap();
    assert_eq!(cfg.auth.mode, AuthMode::Oidc);
    assert_eq!(cfg.auth.external_url, "http://localhost:9090");
    assert_eq!(
        cfg.storage.url,
        "postgres://postgres:postgres@postgres:5432/postgres"
    );
    assert_eq!(cfg.llm.default_profile, None);
    let oidc = cfg.auth.oidc.as_ref().unwrap();
    assert_eq!(oidc.issuer, "http://localhost:8081/auth/v1/");
    assert_eq!(oidc.client_id, "mockingbird");
    assert_eq!(
        oidc.client_secret.as_deref(),
        Some("file:/bootstrap/client_secret")
    );
    let server = ServerConfig {
        host: "0.0.0.0".parse().unwrap(),
        ..cfg.server.clone()
    };
    server.check_bind(&cfg.auth).unwrap();
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
  sse: {keepalive: 1s}
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

#[test]
fn import_section_defaults() {
    let ws = Workspace::new();
    let cfg = ws.load("mocks: {}\n");
    assert_eq!(cfg.import.workers, 2);
    assert_eq!(cfg.import.max_tokens, 200_000);
    assert_eq!(cfg.import.llm_profile, None);
    assert!(cfg.import.url_allowlist.is_empty());
    assert_eq!(cfg.import.max_active_jobs_per_mock, 10);
    let settings = cfg.import.settings().unwrap();
    assert_eq!(settings.workers, 2);
    assert_eq!(settings.max_tokens, 200_000);
    assert_eq!(settings.max_active_jobs_per_mock, 10);

    let cfg = ws.load(
        "import:\n  workers: 4\n  max_tokens: 50000\n  llm_profile: big\n  url_allowlist: [docs.internal, 10.0.0.0/8]\n  max_active_jobs_per_mock: 3\n",
    );
    assert_eq!(cfg.import.workers, 4);
    assert_eq!(cfg.import.llm_profile.as_deref(), Some("big"));
    let settings = cfg.import.settings().unwrap();
    assert_eq!(settings.workers, 4);
    assert_eq!(settings.max_tokens, 50_000);
    assert_eq!(settings.max_active_jobs_per_mock, 3);
    assert!(settings.fetch.host_allowed("docs.internal"));
}

#[test]
fn import_section_validation() {
    let ws = Workspace::new();
    for (yaml, field) in [
        ("import:\n  workers: 0\n", "import.workers"),
        ("import:\n  workers: 17\n", "import.workers"),
        ("import:\n  max_tokens: 999\n", "import.max_tokens"),
        (
            "import:\n  max_active_jobs_per_mock: 0\n",
            "import.max_active_jobs_per_mock",
        ),
        (
            "import:\n  url_allowlist: [\"10.0.0.0/33\"]\n",
            "import.url_allowlist",
        ),
    ] {
        ws.write(yaml);
        let err = file_config::load(&ws.config_path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(field), "{yaml}: {msg}");
    }
    let cfg = ws.load("import:\n  workers: 16\n  max_tokens: 1000\n");
    assert_eq!(cfg.import.workers, 16);
    assert_eq!(cfg.import.max_tokens, 1000);
}

#[test]
fn import_unknown_field_rejected() {
    let ws = Workspace::new();
    ws.write("import:\n  worker: 3\n");
    let err = file_config::load(&ws.config_path()).unwrap_err();
    assert!(format!("{err:#}").contains("worker"), "{err:#}");
}

/// `broker_configs` error for a config with one broker `b` declared as `body`.
fn broker_error(ws: &Workspace, body: &str) -> String {
    let cfg = ws.load(&format!("brokers:\n  b:\n{body}"));
    match file_config::broker_configs(&cfg) {
        Ok(configs) => panic!("expected an error, got {configs:?}"),
        Err(e) => format!("{e:#}"),
    }
}

#[test]
fn brokers_section_parses_and_validates() {
    let ws = Workspace::new();

    let cfg = ws.load("brokers:\n  local:\n    kind: memory\n");
    let configs = file_config::broker_configs(&cfg).unwrap();
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0].name, "local");
    assert_eq!(configs[0].kind, mb_protocols::events::BrokerKind::Memory);
    assert_eq!(configs[0].client_id_prefix, "mockingbird");
    assert!(!configs[0].tls);

    let e = broker_error(&ws, "    kind: nats\n");
    assert!(
        e.contains("brokers.b.kind: unknown broker kind \"nats\""),
        "{e}"
    );

    let e = broker_error(&ws, "    kind: kafka\n");
    assert!(e.contains("brokers.b.bootstrap_servers"), "{e}");
    let e = broker_error(&ws, "    kind: amqp\n");
    assert!(e.contains("brokers.b") && e.contains("url"), "{e}");
    let e = broker_error(&ws, "    kind: mqtt\n");
    assert!(e.contains("brokers.b") && e.contains("url"), "{e}");

    let e = broker_error(
        &ws,
        "    kind: amqp\n    url: amqp://guest:guest@127.0.0.1:5672/%2f\n",
    );
    assert!(
        e.contains("brokers.b.url: put the password in sasl.password as a secret reference"),
        "{e}"
    );

    let e = broker_error(
        &ws,
        "    kind: mqtt\n    url: mqtt://127.0.0.1:1883\n    sasl:\n      mechanism: PLAIN\n      username: u\n      password: hunter2\n",
    );
    assert!(e.contains("brokers.b.sasl.password"), "{e}");
    assert!(e.contains("secret reference"), "{e}");

    // A user without a password, and a secret-reference password, pass.
    #[cfg(feature = "mqtt")]
    {
        let cfg = ws.load(
            "brokers:\n  m:\n    kind: mqtt\n    url: mqtt://user@127.0.0.1:1883\n    client_id_prefix: mb\n    sasl:\n      mechanism: PLAIN\n      username: u\n      password: env:MQTT_PASSWORD\n",
        );
        let configs = file_config::broker_configs(&cfg).unwrap();
        assert_eq!(configs[0].url, "mqtt://user@127.0.0.1:1883");
        assert!(!configs[0].tls);
        assert_eq!(configs[0].client_id_prefix, "mb");
        assert_eq!(
            configs[0].sasl.as_ref().unwrap().password_ref,
            "env:MQTT_PASSWORD"
        );
    }

    // Unknown keys are rejected.
    ws.write("brokers:\n  b:\n    kind: memory\n    bogus: 1\n");
    assert!(file_config::load(&ws.config_path()).is_err());
}

/// rumqttc is built without its TLS stack (its rustls-webpki has open
/// advisories), so every way of asking for MQTT over TLS is refused.
#[test]
fn mqtt_over_tls_is_rejected() {
    const EXPECTED: &str = "brokers.b: MQTT over TLS is not supported in this build (rumqttc TLS stack has open advisories); use a TLS-terminating proxy or plain MQTT";
    let ws = Workspace::new();
    for body in [
        "    kind: mqtt\n    url: mqtts://127.0.0.1:8883\n",
        "    kind: mqtt\n    url: ssl://127.0.0.1:8883\n",
        "    kind: mqtt\n    url: MQTTS://127.0.0.1:8883\n",
        "    kind: mqtt\n    url: mqtt://127.0.0.1:1883\n    tls: true\n",
        "    kind: mqtt\n    url: mqtt://127.0.0.1:1883\n    ca_file: /etc/ssl/ca.pem\n",
    ] {
        let e = broker_error(&ws, body);
        assert!(e.contains(EXPECTED), "{body}: {e}");
    }
    // Plain MQTT, and TLS for the other kinds, stay accepted.
    let cfg = ws.load("brokers:\n  m:\n    kind: mqtt\n    url: mqtt://127.0.0.1:1883\n");
    #[cfg(feature = "mqtt")]
    assert!(file_config::broker_configs(&cfg).is_ok());
    let _ = cfg;
    let cfg = ws.load(
        "brokers:\n  r:\n    kind: amqp\n    url: amqps://127.0.0.1:5671/%2f\n    tls: true\n",
    );
    #[cfg(feature = "amqp")]
    assert!(file_config::broker_configs(&cfg).is_ok());
    let _ = cfg;
}

#[tokio::test]
async fn invalid_brokers_fail_build_app() {
    let ws = Workspace::new();
    ws.write("brokers:\n  b:\n    kind: nats\n");
    let e = match build_app(&config_args(&ws, false), None).await {
        Ok(_) => panic!("expected an error"),
        Err(e) => format!("{e:#}"),
    };
    assert!(e.starts_with("applying the configuration"), "{e}");
    assert!(e.contains("unknown broker kind \"nats\""), "{e}");
}

#[cfg(not(feature = "kafka"))]
#[test]
fn slim_build_rejects_kafka_broker() {
    let ws = Workspace::new();
    let e = broker_error(
        &ws,
        "    kind: kafka\n    bootstrap_servers: 127.0.0.1:9092\n",
    );
    assert!(
        e.contains(
            "brokers.b: this build has no kafka support (use the full image or build with --features kafka)"
        ),
        "{e}"
    );
}

// ---- channels and broker as code ----

fn asyncapi_fixture() -> PathBuf {
    repo().join("fixtures/asyncapi/orders-amqp-2.6.yaml")
}

/// A config with a `mem` broker and an `orders` AsyncAPI mock whose body
/// (`broker:`/`channels:` lines, indented four spaces) is `extra`.
fn channels_config(extra: &str) -> String {
    format!(
        "brokers:\n  mem:\n    kind: memory\nmocks:\n  orders:\n    contract: {}\n{extra}",
        asyncapi_fixture().display()
    )
}

const CHANNEL_BLOCK: &str = "    broker: mem\n    channels:\n      orderCreated:\n        triggers:\n          - every: 10s\n          - manual\n";

#[test]
fn file_trigger_forms_parse() {
    let ws = Workspace::new();
    let cfg = ws.load(&channels_config(
        "    channels:\n      orderCreated:\n        enabled: false\n        triggers:\n          - every: 250ms\n          - cron: \"*/5 * * * *\"\n          - on_state_change: {resource_type: orders, kind: deleted}\n          - on_state_change: {resource_type: carts}\n          - manual\n      orderCancelled: {}\n",
    ));
    let m = &cfg.mocks["orders"];
    assert_eq!(m.broker, None);
    let c = &m.channels["orderCreated"];
    assert!(!c.enabled);
    let triggers: Vec<mb_mcm::Trigger> = c
        .triggers
        .as_ref()
        .unwrap()
        .iter()
        .map(|t| t.to_trigger())
        .collect();
    assert_eq!(
        triggers,
        [
            mb_mcm::Trigger::Schedule {
                every: Duration::from_millis(250)
            },
            mb_mcm::Trigger::Cron {
                expr: "*/5 * * * *".into()
            },
            mb_mcm::Trigger::OnStateChange {
                resource_type: "orders".into(),
                kind: mb_mcm::StateChangeKind::Deleted
            },
            mb_mcm::Trigger::OnStateChange {
                resource_type: "carts".into(),
                kind: mb_mcm::StateChangeKind::Any
            },
            mb_mcm::Trigger::Manual,
        ]
    );
    let other = &m.channels["orderCancelled"];
    assert!(other.enabled);
    assert_eq!(other.triggers, None);

    ws.write(&channels_config(
        "    channels:\n      orderCreated:\n        bogus: 1\n",
    ));
    assert!(file_config::load(&ws.config_path()).is_err());
}

#[tokio::test]
async fn file_channel_settings_reconciled() {
    let ws = Workspace::new();
    let store = store().await;
    let cfg = ws.load(&channels_config(CHANNEL_BLOCK));
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    let mock = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    let rows = store.list_channel_settings(mock.id).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].channel_id, "orderCreated");
    assert!(rows[0].enabled);
    assert_eq!(rows[0].source, ResourceSource::File);
    assert_eq!(
        rows[0].triggers,
        Some(vec![
            mb_mcm::Trigger::Schedule {
                every: Duration::from_secs(10)
            },
            mb_mcm::Trigger::Manual
        ])
    );
    let events = store.get_event_settings(mock.id).await.unwrap().unwrap();
    assert_eq!(events.broker.as_deref(), Some("mem"));
    assert_eq!(events.source, ResourceSource::File);

    // Reconciling the same file again changes nothing.
    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert!(report.is_empty(), "{report:?}");

    // Removing the block deletes the row and clears the broker.
    let cfg = ws.load(&channels_config(""));
    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert_eq!(report.updated, ["orders"]);
    assert!(
        store
            .list_channel_settings(mock.id)
            .await
            .unwrap()
            .is_empty()
    );
    let events = store.get_event_settings(mock.id).await.unwrap().unwrap();
    assert_eq!(events.broker, None);
}

async fn channel_reconcile_error(extra: &str) -> (String, Arc<dyn ControlStore>) {
    let ws = Workspace::new();
    let store = store().await;
    let cfg = ws.load(&channels_config(extra));
    let err = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap_err();
    (format!("{err:#}"), store)
}

#[tokio::test]
async fn file_channel_invalid_cron_rejected() {
    let (err, store) = channel_reconcile_error(
        "    channels:\n      orderCreated:\n        triggers:\n          - cron: \"not a cron\"\n",
    )
    .await;
    assert!(
        err.contains("mocks.orders.channels.orderCreated.triggers: invalid cron expression"),
        "{err}"
    );
    // Nothing was applied.
    assert!(
        store
            .list_mocks(DEFAULT_WORKSPACE)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn file_channel_unknown_id_rejected() {
    let (err, store) =
        channel_reconcile_error("    channels:\n      nope:\n        enabled: false\n").await;
    assert!(
        err.contains("mocks.orders.channels.nope: no such channel in the contract"),
        "{err}"
    );
    assert!(
        store
            .list_mocks(DEFAULT_WORKSPACE)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn file_unknown_broker_rejected() {
    let (err, store) = channel_reconcile_error("    broker: rabbit\n").await;
    assert!(
        err.contains("mocks.orders.broker: unknown broker \"rabbit\""),
        "{err}"
    );
    assert!(
        store
            .list_mocks(DEFAULT_WORKSPACE)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn file_triggers_on_subscribe_channel_rejected() {
    let (err, store) = channel_reconcile_error(
        "    channels:\n      cancelOrder:\n        triggers:\n          - manual\n",
    )
    .await;
    assert!(
        err.contains("mocks.orders.channels.cancelOrder.triggers: triggers apply only"),
        "{err}"
    );
    assert!(
        store
            .list_mocks(DEFAULT_WORKSPACE)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn api_put_on_file_channel_409() {
    let ws = Workspace::new();
    ws.write(&channels_config(CHANNEL_BLOCK));
    let app = start(config_args(&ws, false)).await;
    let http = reqwest::Client::new();
    let api = format!("{}/api/v1/mocks/orders", app.admin);

    let r = http
        .put(format!("{api}/channels/orderCreated"))
        .json(&json!({"enabled": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    assert_eq!(r.headers()["content-type"], "application/problem+json");

    let view: Value = http
        .get(format!("{api}/channels/orderCreated"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["settings_source"], "file", "{view}");
    assert_eq!(
        view["triggers"],
        json!([{"schedule": {"every_ms": 10000}}, "manual"]),
        "{view}"
    );
    let events: Value = http
        .get(format!("{api}/event-settings"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events, json!({"broker": "mem", "source": "file"}));
}

// ---- P7 config surface ----

/// The validation error of `yaml`, as `load` reports it.
fn load_err(ws: &Workspace, yaml: &str) -> String {
    ws.write(yaml);
    let err = file_config::load(&ws.config_path()).unwrap_err();
    format!("{err:#}")
}

#[test]
fn p7_sections_default() {
    use mb_engine::pii::BuiltinPattern;
    use mockingbird::file_config::{BusTarget, Role};

    let ws = Workspace::new();
    let cfg = ws.load("mocks:\n  orders:\n    contract: orders.yaml\n    scenarios: {s: {}}\n");
    assert_eq!(cfg.server.role, Role::All);
    assert!(!cfg.server.trust_forwarded_headers);
    assert_eq!(cfg.server.tls, None);
    assert_eq!(cfg.outbound.ca_file, None);
    assert_eq!(cfg.outbound.proxy, None);
    assert!(cfg.outbound.no_proxy.is_empty());
    assert_eq!(cfg.bus.url, None);
    assert_eq!(cfg.bus_target(), BusTarget::Local);
    assert_eq!(cfg.limits.max_concurrent_generations, 64);
    assert_eq!(cfg.limits.max_request_bytes, 1_048_576);
    assert_eq!(cfg.limits.max_response_bytes, 1_048_576);
    assert!(!cfg.pii.enabled);
    assert_eq!(cfg.pii.patterns, BuiltinPattern::ALL);
    assert!(cfg.pii.custom.is_empty());
    assert_eq!(cfg.scrubber_config(), None);
    assert_eq!(cfg.protocols.graphql.max_root_fields, 20);
    assert_eq!(cfg.protocols.graphql.max_subscriptions_per_connection, 16);
    assert_eq!(cfg.protocols.graphql.max_connections_per_mock, 1000);
    assert_eq!(
        cfg.protocols.graphql.connection_init_timeout,
        Duration::from_secs(5)
    );
    let m = &cfg.mocks["orders"];
    assert_eq!(m.chaos, None);
    assert_eq!(m.limits, file_config::FileMockLimits::default());
    assert_eq!(m.limits.to_store(), mb_store::MockLimits::default());
    assert_eq!(m.scenarios["s"].chaos, None);

    let (settings, _) = cfg.protocol_settings();
    assert_eq!(settings.graphql_max_root_fields, 20);
    assert_eq!(settings.graphql_max_subscriptions_per_connection, 16);
    assert_eq!(settings.graphql_max_connections_per_mock, 1000);
    assert_eq!(
        settings.graphql_connection_init_timeout,
        Duration::from_secs(5)
    );
    assert!(!settings.trust_forwarded_headers);
    assert!(!settings.tls_terminated);
    let d = mb_protocols::ProtocolSettings::default();
    assert_eq!(
        (
            d.graphql_max_root_fields,
            d.graphql_max_subscriptions_per_connection,
            d.graphql_max_connections_per_mock,
            d.trust_forwarded_headers,
            d.tls_terminated
        ),
        (20, 16, 1000, false, false)
    );

    let cfg = ws.load(
        r#"
server:
  trust_forwarded_headers: true
  tls: {cert_file: /etc/mb/cert.pem, key_file: /etc/mb/key.pem}
outbound:
  ca_file: /etc/mb/ca.pem
  proxy: http://proxy.internal:3128
  no_proxy: [localhost, .internal]
limits: {max_concurrent_generations: 8, max_request_bytes: 2048, max_response_bytes: 4096}
pii:
  enabled: true
  patterns: [email, ipv4]
  custom: [{name: order_ref, regex: "ORD-[0-9]{6}", replacement: "ORD-######"}]
protocols:
  graphql: {max_root_fields: 5, max_subscriptions_per_connection: 2, max_connections_per_mock: 10, connection_init_timeout: 2s}
"#,
    );
    let tls = cfg.server.tls.as_ref().unwrap();
    assert_eq!(tls.cert_file, PathBuf::from("/etc/mb/cert.pem"));
    assert_eq!(tls.key_file, PathBuf::from("/etc/mb/key.pem"));
    assert_eq!(cfg.outbound.ca_file, Some(PathBuf::from("/etc/mb/ca.pem")));
    assert_eq!(
        cfg.outbound.proxy.as_deref(),
        Some("http://proxy.internal:3128")
    );
    assert_eq!(cfg.outbound.no_proxy, ["localhost", ".internal"]);
    assert_eq!(cfg.limits.max_concurrent_generations, 8);
    assert_eq!(cfg.limits.max_request_bytes, 2048);
    assert_eq!(cfg.limits.max_response_bytes, 4096);
    let scrubber = cfg.scrubber_config().unwrap();
    assert_eq!(
        scrubber.builtins,
        [BuiltinPattern::Email, BuiltinPattern::Ipv4]
    );
    assert_eq!(scrubber.custom[0].name, "order_ref");
    let (settings, _) = cfg.protocol_settings();
    assert_eq!(settings.graphql_max_root_fields, 5);
    assert_eq!(settings.graphql_max_subscriptions_per_connection, 2);
    assert_eq!(settings.graphql_max_connections_per_mock, 10);
    assert_eq!(
        settings.graphql_connection_init_timeout,
        Duration::from_secs(2)
    );
    assert!(settings.trust_forwarded_headers);
    assert!(settings.tls_terminated);
}

#[test]
fn role_flag_env_file_precedence() {
    use mockingbird::file_config::Role;

    let ws = Workspace::new();
    ws.write(
        "server:\n  role: data\nstorage:\n  url: postgres://db/mb\n  state_url: redis://cache:6379\n",
    );
    let mut cfg = file_config::load(&ws.config_path()).unwrap();
    assert_eq!(cfg.server.role, Role::Data);

    file_config::apply_env_from(&mut cfg, |k| (k == "MB_ROLE").then(|| "control".into())).unwrap();
    assert_eq!(cfg.server.role, Role::Control);

    let args = ServeArgs {
        role: Some(Role::All),
        ..oidc_args(&ws)
    };
    apply_flags(&mut cfg, &args);
    assert_eq!(cfg.server.role, Role::All);

    // Through resolve_config: the flag wins over the file.
    let resolved = resolve_config(&args, None).unwrap();
    assert_eq!(resolved.server.role, Role::All);
    let resolved = resolve_config(&oidc_args(&ws), None).unwrap();
    assert_eq!(resolved.server.role, Role::Data);

    let err = file_config::apply_env_from(&mut cfg, |k| (k == "MB_ROLE").then(|| "edge".into()))
        .unwrap_err();
    assert!(format!("{err:#}").contains("MB_ROLE"), "{err:#}");

    // MB_STATE_URL and MB_BUS_URL.
    file_config::apply_env_from(&mut cfg, |k| match k {
        "MB_STATE_URL" => Some("rediss://cache:6380".into()),
        "MB_BUS_URL" => Some("local".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(
        cfg.storage.state_url.as_deref(),
        Some("rediss://cache:6380")
    );
    assert_eq!(cfg.bus.url.as_deref(), Some("local"));

    // The CLI parses --role.
    use clap::Parser;
    let cli = mockingbird::config::Cli::try_parse_from([
        "mockingbird",
        "serve",
        "-c",
        "x.yaml",
        "--role",
        "control",
    ])
    .unwrap();
    match cli.cmd {
        mockingbird::config::Command::Serve(args) => assert_eq!(args.role, Some(Role::Control)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn split_roles_require_shared_stores() {
    let ws = Workspace::new();
    for role in ["data", "control"] {
        // SQLite control store.
        let msg = load_resolve_err(&ws, &format!("server:\n  role: {role}\n"));
        assert!(
            msg.contains(&format!("server.role {role}")) && msg.contains("storage.url"),
            "{msg}"
        );
        assert!(msg.contains("single-node"), "{msg}");
        // In-process state.
        let msg = load_resolve_err(
            &ws,
            &format!(
                "server:\n  role: {role}\nstorage:\n  url: postgres://db/mb\n  state_url: memory\n"
            ),
        );
        assert!(
            msg.contains(&format!("server.role {role}")) && msg.contains("storage.state_url"),
            "{msg}"
        );
        // An in-process bus: the other nodes would never hear of changes,
        // and every control node would lead.
        let msg = load_resolve_err(
            &ws,
            &format!(
                "server:\n  role: {role}\nstorage:\n  url: postgres://db/mb\nbus:\n  url: local\n"
            ),
        );
        assert!(
            msg.contains(&format!("server.role {role}")) && msg.contains("bus.url local"),
            "{msg}"
        );
    }
    // Postgres with the default state store (the same URL), or Redis state, pass.
    for yaml in [
        "server:\n  role: data\nstorage:\n  url: postgres://db/mb\n",
        "server:\n  role: data\nstorage:\n  url: postgresql://db/mb\n  state_url: rediss://c:6380\n",
        "server:\n  role: all\n",
    ] {
        ws.write(yaml);
        resolve_config(&oidc_args(&ws), None).unwrap_or_else(|e| panic!("{yaml}: {e:#}"));
    }
}

/// The error `resolve_config` gives for `yaml` (the file, env and flags
/// combined).
fn load_resolve_err(ws: &Workspace, yaml: &str) -> String {
    ws.write(yaml);
    let err = resolve_config(&oidc_args(ws), None).unwrap_err();
    format!("{err:#}")
}

#[test]
fn bus_target_resolution() {
    use mockingbird::file_config::BusTarget;

    let mut cfg = FileConfig::default();
    cfg.bus.url = Some("local".into());
    cfg.storage.url = "postgres://db/mb".into();
    assert_eq!(cfg.bus_target(), BusTarget::Local);
    cfg.bus.url = Some("redis://cache:6379".into());
    assert_eq!(
        cfg.bus_target(),
        BusTarget::Redis("redis://cache:6379".into())
    );
    cfg.bus.url = Some("rediss://cache:6380".into());
    assert_eq!(
        cfg.bus_target(),
        BusTarget::Redis("rediss://cache:6380".into())
    );
    cfg.bus.url = Some("postgres://bus/mb".into());
    assert_eq!(
        cfg.bus_target(),
        BusTarget::Postgres("postgres://bus/mb".into())
    );
    cfg.bus.url = Some("postgresql://bus/mb".into());
    assert_eq!(
        cfg.bus_target(),
        BusTarget::Postgres("postgresql://bus/mb".into())
    );
    // Unset: the control store when it is Postgres, else in-process.
    cfg.bus.url = None;
    assert_eq!(
        cfg.bus_target(),
        BusTarget::Postgres("postgres://db/mb".into())
    );
    cfg.storage.url = "sqlite::memory:".into();
    assert_eq!(cfg.bus_target(), BusTarget::Local);

    // Any other scheme is rejected.
    let ws = Workspace::new();
    let msg = load_err(&ws, "bus:\n  url: amqp://rabbit\n");
    assert!(msg.contains("bus.url"), "{msg}");
}

#[test]
fn limits_ranges_named() {
    let ws = Workspace::new();
    let msg = load_err(&ws, "limits:\n  max_concurrent_generations: 0\n");
    assert!(
        msg.contains("limits.max_concurrent_generations 0 must be between 1 and 4096"),
        "{msg}"
    );
    for (yaml, key) in [
        (
            "limits:\n  max_concurrent_generations: 4097\n",
            "limits.max_concurrent_generations",
        ),
        (
            "limits:\n  max_request_bytes: 1023\n",
            "limits.max_request_bytes",
        ),
        (
            "limits:\n  max_request_bytes: 67108865\n",
            "limits.max_request_bytes",
        ),
        (
            "limits:\n  max_response_bytes: 16777217\n",
            "limits.max_response_bytes",
        ),
        (
            "mocks:\n  orders:\n    contract: orders.yaml\n    limits: {rate_limit: {requests_per_second: 0, burst: 1}}\n",
            "mocks.orders.limits.rate_limit.requests_per_second",
        ),
        (
            "mocks:\n  orders:\n    contract: orders.yaml\n    limits: {rate_limit: {requests_per_second: 5, burst: 0}}\n",
            "mocks.orders.limits.rate_limit.burst",
        ),
        (
            "mocks:\n  orders:\n    contract: orders.yaml\n    limits: {daily_token_budget: 0}\n",
            "mocks.orders.limits.daily_token_budget",
        ),
        (
            "mocks:\n  orders:\n    contract: orders.yaml\n    limits: {max_response_bytes: 100}\n",
            "mocks.orders.limits.max_response_bytes",
        ),
        (
            "mocks:\n  orders:\n    contract: orders.yaml\n    chaos: {error_rate: 1.5}\n",
            "mocks.orders.chaos.error_rate must be between 0 and 1",
        ),
        (
            "mocks:\n  orders:\n    contract: orders.yaml\n    chaos: {drop_stream_rate: -0.1}\n",
            "mocks.orders.chaos.drop_stream_rate",
        ),
        (
            "mocks:\n  orders:\n    contract: orders.yaml\n    chaos: {latency_jitter: {min: 2s, max: 1s}}\n",
            "mocks.orders.chaos.latency_jitter",
        ),
        (
            "mocks:\n  orders:\n    contract: orders.yaml\n    chaos: {latency_jitter: {min: 0s, max: 31s}}\n",
            "mocks.orders.chaos.latency_jitter.max",
        ),
        (
            "mocks:\n  orders:\n    contract: orders.yaml\n    scenarios: {flaky: {chaos: {error_rate: 2}}}\n",
            "mocks.orders.scenarios.flaky.chaos.error_rate",
        ),
    ] {
        let msg = load_err(&ws, yaml);
        assert!(msg.contains(key), "{yaml}: {msg}");
    }
    ws.write(
        "limits: {max_concurrent_generations: 4096, max_request_bytes: 67108864, max_response_bytes: 1024}\n",
    );
    file_config::load(&ws.config_path()).unwrap();
}

#[test]
fn protocols_validation() {
    let ws = Workspace::new();
    for (yaml, key) in [
        (
            "protocols:\n  sse: {keepalive: 500ms}\n",
            "protocols.sse.keepalive must be at least 1s",
        ),
        (
            "protocols:\n  graphql: {path: graphql}\n",
            "protocols.graphql.path must start with '/'",
        ),
        (
            "protocols:\n  grpc: {max_message_bytes: 100}\n",
            "protocols.grpc.max_message_bytes",
        ),
        (
            "protocols:\n  stream: {item_interval: 31s}\n",
            "protocols.stream.item_interval",
        ),
        (
            "mocks:\n  orders:\n    contract: orders.yaml\n    scenarios: {slow: {stream_item_interval: 31s}}\n",
            "mocks.orders.scenarios.slow.stream_item_interval",
        ),
        (
            "protocols:\n  graphql: {max_root_fields: 0}\n",
            "protocols.graphql.max_root_fields",
        ),
        (
            "protocols:\n  graphql: {max_root_fields: 1001}\n",
            "protocols.graphql.max_root_fields",
        ),
        (
            "protocols:\n  graphql: {max_subscriptions_per_connection: 0}\n",
            "protocols.graphql.max_subscriptions_per_connection",
        ),
        (
            "protocols:\n  graphql: {max_connections_per_mock: 100001}\n",
            "protocols.graphql.max_connections_per_mock",
        ),
        (
            "protocols:\n  graphql: {connection_init_timeout: 11s}\n",
            "protocols.graphql.connection_init_timeout",
        ),
        (
            "protocols:\n  graphql: {connection_init_timeout: 500ms}\n",
            "protocols.graphql.connection_init_timeout",
        ),
        ("outbound:\n  proxy: ftp://proxy\n", "outbound.proxy"),
        ("outbound:\n  proxy: not a url\n", "outbound.proxy"),
    ] {
        let msg = load_err(&ws, yaml);
        assert!(msg.contains(key), "{yaml}: {msg}");
    }
    ws.write("protocols:\n  stream: {item_interval: 30s}\n  sse: {keepalive: 1s}\noutbound:\n  proxy: https://proxy:3128\n");
    file_config::load(&ws.config_path()).unwrap();
}

const CHAOS_LIMITS: &str = r#"
mocks:
  orders:
    contract: orders.yaml
    chaos:
      error_rate: 0.25
      latency_jitter: {min: 10ms, max: 250ms}
      drop_stream_rate: 0.5
    limits:
      rate_limit: {requests_per_second: 5, burst: 10}
      daily_token_budget: 100000
      max_request_bytes: 2048
      max_response_bytes: 4096
    scenarios:
      flaky:
        chaos: {error_rate: 1}
"#;

#[tokio::test]
async fn mock_chaos_and_limits_reconciled() {
    let ws = Workspace::new();
    ws.write(CHAOS_LIMITS);
    let app = start(config_args(&ws, false)).await;
    let r = reqwest::get(format!("{}/api/v1/mocks/orders", app.admin))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mock: Value = r.json().await.unwrap();
    assert_eq!(
        mock["chaos"],
        json!({
            "error_rate": 0.25,
            "latency_jitter": {"min_ms": 10, "max_ms": 250},
            "drop_stream_rate": 0.5
        }),
        "{mock}"
    );
    assert_eq!(
        mock["limits"],
        json!({
            "rate_limit": {"requests_per_second": 5.0, "burst": 10},
            "daily_token_budget": 100000,
            "max_request_bytes": 2048,
            "max_response_bytes": 4096
        }),
        "{mock}"
    );

    // File-declared values replace what the API set; removing them clears.
    let store = store().await;
    let cfg = ws.load(CHAOS_LIMITS);
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    let mut m = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.chaos.as_ref().map(|c| c.error_rate), Some(0.25));
    m.chaos = None;
    m.limits = mb_store::MockLimits::default();
    store.update_mock(&m).await.unwrap();
    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert_eq!(report.updated, ["orders"]);
    let m = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        m.chaos,
        cfg.mocks["orders"].chaos.as_ref().map(|c| c.to_store())
    );
    assert_eq!(m.limits.daily_token_budget, Some(100_000));
    // Unchanged: nothing to do.
    let report = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert!(report.is_empty(), "{report:?}");

    let cfg = ws.load("mocks:\n  orders:\n    contract: orders.yaml\n");
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    let m = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.chaos, None);
    assert_eq!(m.limits, mb_store::MockLimits::default());
}

#[tokio::test]
async fn scenario_chaos_reconciled() {
    let ws = Workspace::new();
    let cfg = ws.load(CHAOS_LIMITS);
    let store = store().await;
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    let m = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    let scenarios = store.list_scenarios(m.id).await.unwrap();
    let flaky = scenarios.iter().find(|s| s.name == "flaky").unwrap();
    assert_eq!(
        flaky.rules.chaos,
        Some(mb_store::ChaosSettings {
            error_rate: 1.0,
            latency_jitter: None,
            drop_stream_rate: 0.0
        })
    );
}

#[test]
fn pii_custom_invalid_regex_rejected() {
    let ws = Workspace::new();
    let msg = load_err(
        &ws,
        "pii:\n  enabled: true\n  custom: [{name: bad, regex: \"(unclosed\", replacement: x}]\n",
    );
    assert!(msg.contains("pii.custom[0]"), "{msg}");
    let msg = load_err(
        &ws,
        "pii:\n  custom: [{name: Bad-Name, regex: x, replacement: y}]\n",
    );
    assert!(msg.contains("pii.custom[0]"), "{msg}");
    let msg = load_err(&ws, "pii:\n  patterns: [email, passport]\n");
    assert!(msg.contains("passport"), "{msg}");
}

#[test]
fn tls_requires_both_files() {
    let ws = Workspace::new();
    for yaml in [
        "server:\n  tls: {cert_file: /etc/mb/cert.pem}\n",
        "server:\n  tls: {key_file: /etc/mb/key.pem}\n",
        "server:\n  tls: {}\n",
    ] {
        let msg = load_err(&ws, yaml);
        assert!(msg.contains("server.tls"), "{yaml}: {msg}");
        assert!(
            msg.contains("cert_file") && msg.contains("key_file"),
            "{msg}"
        );
    }
}

#[test]
fn unknown_p7_key_rejected() {
    let ws = Workspace::new();
    for yaml in [
        "server:\n  rol: data\n",
        "server:\n  role: edge\n",
        "server:\n  tls: {cert: x, key_file: y}\n",
        "outbound:\n  proxies: [x]\n",
        "bus:\n  uri: local\n",
        "limits:\n  max_generations: 3\n",
        "pii:\n  enable: true\n",
        "pii:\n  custom: [{name: a, regex: b, replacement: c, flags: i}]\n",
        "protocols:\n  graphql: {max_roots: 3}\n",
        "mocks:\n  orders: {contract: orders.yaml, chaos: {error: 0.1}}\n",
        "mocks:\n  orders: {contract: orders.yaml, chaos: {latency_jitter: {min: 1s, max: 2s, avg: 1s}}}\n",
        "mocks:\n  orders: {contract: orders.yaml, limits: {rate: 3}}\n",
        "mocks:\n  orders: {contract: orders.yaml, limits: {rate_limit: {rps: 3, burst: 1}}}\n",
        "mocks:\n  orders: {contract: orders.yaml, scenarios: {s: {chaos: {errors: 1}}}}\n",
    ] {
        ws.write(yaml);
        assert!(file_config::load(&ws.config_path()).is_err(), "{yaml}");
    }
}

const BEHAVIOR: &str = r#"
mocks:
  orders:
    contract: orders.yaml
    consistency: off
    unknown_ids: 404
    default_scenario: gift
    scenarios:
      gift:
        operations:
          - listOrders
          - {id: getOrder, instructions: This order is a gift.}
"#;

#[tokio::test]
async fn reconcile_applies_mock_behavior_and_operation_instructions() {
    let ws = Workspace::new();
    let cfg = ws.load(BEHAVIOR);
    let store = store().await;
    reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    let mock = store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mock.behavior.consistency, mb_store::Consistency::Off);
    assert_eq!(mock.behavior.unknown_ids, mb_store::UnknownIds::NotFound);
    assert_eq!(mock.behavior.default_scenario.as_deref(), Some("gift"));
    let s = &store.list_scenarios(mock.id).await.unwrap()[0];
    assert_eq!(s.rules.operations, ["listOrders", "getOrder"]);
    assert_eq!(
        s.rules
            .operation_instructions
            .get("getOrder")
            .map(String::as_str),
        Some("This order is a gift.")
    );
    let again = reconcile(store.as_ref(), no_state().as_ref(), &cfg)
        .await
        .unwrap();
    assert!(again.updated.is_empty(), "{again:?}");
}

#[test]
fn default_scenario_must_name_a_scenario() {
    let ws = Workspace::new();
    ws.write("mocks:\n  orders:\n    contract: orders.yaml\n    default_scenario: missing\n");
    // `load` validates the file.
    let err = file_config::load(&ws.config_path()).unwrap_err();
    assert!(
        format!("{err:#}").contains("mocks.orders.default_scenario"),
        "{err:#}"
    );
}
