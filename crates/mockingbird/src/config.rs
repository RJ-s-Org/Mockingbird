//! Command-line flags and LLM environment configuration.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use axum::Router;
use clap::{Parser, Subcommand};
use mb_admin::channels::SharedChannelControl;
use mb_engine::GenerationLimiter;
use mb_engine::pii::Scrubber;
use mb_ext::{ExtError, Extensions, SecretStore};
use mb_protocols::events::{Brokers, EventDeps};
use mb_state::{Bus, LeaderBackend, Leadership, StateStore};
use mb_store::ControlStore;

use crate::client::ClientCommand;
use crate::dp_keys::StoreDataPlaneKeys;
use crate::events::{RegistryChannels, StoreLease};
use crate::file_config::{
    self, AuthConfig, AuthMode, ENV_API_KEY_REF, ENV_PROFILE, FileConfig, FileMock, FileProfile,
    Role,
};
use crate::ha::Ha;
use crate::host_guard::HostPolicy;
use crate::import_llm::RuntimeLlm;
use crate::maintenance::{LeaderJobs, LeaderWork, Maintenance, StateGauge};
use crate::ops::{self, OpsState};
use crate::recorder::StoreRecorder;
use crate::registry::{MockRegistry, mock_label};
use crate::runtime::{DefaultProfile, RuntimeDeps, ServeMode};
use crate::watch;
use crate::{reconcile, telemetry};

/// Health check path; reserved so it cannot collide with contract paths.
pub const HEALTH_PATH: &str = "/__mockingbird/healthz";

/// The leader lock control nodes contend for.
pub const LEADER_LOCK: &str = "control";

#[derive(Parser, Debug)]
#[command(name = "mockingbird", version, about = "LLM-backed mock endpoints")]
pub struct Cli {
    /// Admin API base URL used by the client subcommands.
    #[arg(
        long,
        global = true,
        env = "MB_ADMIN_URL",
        default_value = crate::client::DEFAULT_ADMIN_URL
    )]
    pub admin_url: String,
    /// Bearer token (an API key) the client subcommands send to the Admin
    /// API.
    #[arg(long, global = true, env = "MB_ADMIN_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// Workspace the client subcommands act on.
    #[arg(
        long,
        global = true,
        env = "MB_WORKSPACE",
        default_value = mb_store::DEFAULT_WORKSPACE
    )]
    pub workspace: String,
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Serve one or more contracts as live mocks.
    Serve(ServeArgs),
    /// Compile a contract and print an operation summary.
    Validate { contract: PathBuf },
    /// Print the Admin API's OpenAPI document as JSON.
    Openapi,
    #[command(flatten)]
    Client(ClientCommand),
}

// The doc comments are clap help text, not Markdown.
#[allow(rustdoc::invalid_html_tags)]
#[derive(clap::Args, Debug, Clone, Default)]
pub struct ServeArgs {
    /// Config file (`mockingbird.yaml`); without it the contracts are served
    /// zero-config.
    #[arg(short = 'c', long, conflicts_with = "contracts")]
    pub config: Option<PathBuf>,
    /// Bind address of both ports [default: 127.0.0.1].
    #[arg(long)]
    pub host: Option<IpAddr>,
    /// Data-plane port [default: 8080].
    #[arg(long)]
    pub port: Option<u16>,
    /// Admin API and ops port [default: 9090].
    #[arg(long)]
    pub admin_port: Option<u16>,
    /// gRPC port [default: 50051].
    #[arg(long)]
    pub grpc_port: Option<u16>,
    /// Control-plane storage: sqlite::memory:, sqlite://<path> or postgres://….
    #[arg(long)]
    pub storage_url: Option<String>,
    /// Allow binding a non-loopback address without authentication
    /// (`auth.mode: none`).
    #[arg(long)]
    pub insecure_no_auth: bool,
    /// Faker only, no LLM.
    #[arg(long)]
    pub offline: bool,
    /// LLM timeout of the default profile [default: 3s].
    #[arg(long)]
    pub llm_timeout: Option<humantime::Duration>,
    /// Which planes this node serves [default: all]; `control` and `data`
    /// need shared Postgres (and Postgres or Redis state) stores.
    #[arg(long, value_enum)]
    pub role: Option<file_config::Role>,
    /// Reconcile again whenever the config file or a contract changes.
    #[arg(long, requires = "config")]
    pub watch: bool,
    #[arg(required_unless_present = "config")]
    pub contracts: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct LlmEnv {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub json_schema: bool,
}

impl LlmEnv {
    /// Reads `MB_LLM_*`, applying the documented defaults.
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            base_url: get("MB_LLM_BASE_URL").unwrap_or_else(|| "https://api.deepseek.com".into()),
            model: get("MB_LLM_MODEL").unwrap_or_else(|| "deepseek-chat".into()),
            api_key: get("MB_LLM_API_KEY"),
            json_schema: get("MB_LLM_JSON_SCHEMA")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        }
    }
}

/// The mock name for a contract file: its stem.
pub fn mock_name(path: &Path) -> anyhow::Result<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .with_context(|| format!("cannot derive a mock name from {}", path.display()))
}

/// A built server: the routers of both ports and the mock registry.
pub struct App {
    /// The data plane (mocks and [`HEALTH_PATH`]).
    pub data: Router,
    /// The Admin API, the ops endpoints and any extension admin routes.
    pub admin: Router,
    /// Every gRPC mock, plus health and reflection (serve it as h2c).
    pub grpc: Router,
    pub registry: Arc<MockRegistry>,
    /// The effective configuration (file < env < flags).
    pub config: FileConfig,
    /// Which planes this node serves.
    pub role: Role,
    /// This node's claim on the [`LEADER_LOCK`]; a `data` node is never
    /// the leader.
    pub leadership: Arc<Leadership>,
    /// The leader-only work (reconcile on gaining leadership, `--watch`,
    /// [`Maintenance`]); `None` on a `data` node. Dropping it stops it.
    pub leader_work: Option<LeaderWork>,
    /// Refreshes the `mb_state_sessions` gauge on a node that serves the
    /// data plane (leader or not); `None` on a `control` node. Dropping it
    /// stops it.
    pub state_gauge: Option<StateGauge>,
    /// How many times this node reconciled the configuration into the
    /// control store (tests).
    #[doc(hidden)]
    pub reconciles: Arc<AtomicU64>,
    /// The request log writer.
    pub recorder: Arc<StoreRecorder>,
    /// The configured message brokers (the `brokers:` section).
    pub brokers: Arc<Brokers>,
    /// The import worker pool (not on a `data` node); dropping it stops
    /// the workers.
    pub import_workers: Option<mb_admin::ImportWorkers>,
    /// The cross-node bus and its bridges.
    pub ha: Arc<Ha>,
    /// The state store the mocks and the Admin API use, its change feed
    /// shared across nodes (see [`Ha::shared_state`]).
    pub state: Arc<dyn StateStore>,
    /// With `server.tls`: the certificates every listener serves, reloaded
    /// when the files change (see [`crate::tls::spawn_reloader`]).
    pub tls: Option<axum_server::tls_rustls::RustlsConfig>,
    /// Stops the certificate reloader when the app is dropped.
    pub tls_reloader: Option<crate::tls::ReloaderGuard>,
}

/// How long [`App::shutdown`] waits for the request log to drain.
pub const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl App {
    /// Graceful shutdown: stops the leader-only work (watching the config
    /// file, maintenance), flushes the brokers' queued messages and writes
    /// the queued request-log entries (each waiting at most
    /// [`SHUTDOWN_DRAIN_TIMEOUT`], concurrently).
    pub async fn shutdown(self) {
        drop(self.leader_work);
        drop(self.tls_reloader);
        tokio::join!(
            self.brokers.shutdown(SHUTDOWN_DRAIN_TIMEOUT),
            self.recorder.shutdown(SHUTDOWN_DRAIN_TIMEOUT)
        );
    }
}

/// The zero-config [`FileConfig`] for `serve a.yaml [b.yaml]`: SQLite in
/// memory; one mock per contract named after its file stem (lowercased,
/// other invalid characters replaced by `-`), at `/` for a single contract
/// or `/<mock name>` for several; and, unless `--offline`, a default profile
/// from `llm` whose key is the reference `env:MB_LLM_API_KEY`.
pub fn zero_config(args: &ServeArgs, llm: Option<&LlmEnv>) -> anyhow::Result<FileConfig> {
    let mut cfg = FileConfig::default();
    let multi = args.contracts.len() > 1;
    for path in &args.contracts {
        let stem = mock_name(path)?;
        let name = sanitize_mock_name(&stem);
        let mut mock = FileMock::new(
            std::path::absolute(path)
                .with_context(|| format!("cannot resolve {}", path.display()))?,
        );
        mock.route_prefix = Some(if multi {
            format!("/{name}")
        } else {
            "/".to_string()
        });
        if cfg.mocks.insert(name.clone(), mock).is_some() {
            bail!("duplicate mock name {name:?}; contract file stems must be unique");
        }
    }
    if let (false, Some(env)) = (args.offline, llm) {
        let mut profile = FileProfile::new(env.base_url.clone(), env.model.clone());
        profile.api_key = Some(ENV_API_KEY_REF.to_string());
        profile.json_schema = env.json_schema;
        cfg.llm.profiles.insert(ENV_PROFILE.to_string(), profile);
        cfg.llm.default_profile = Some(ENV_PROFILE.to_string());
    }
    Ok(cfg)
}

/// `stem` lowercased with every character outside `[a-z0-9-]` replaced by
/// `-`, as mock names require.
fn sanitize_mock_name(stem: &str) -> String {
    let mut name: String = stem
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if name.starts_with('-') {
        name.insert(0, 'm');
    }
    name.truncate(63);
    name
}

/// Applies the command-line flags, which override the file and the
/// environment.
pub fn apply_flags(cfg: &mut FileConfig, args: &ServeArgs) {
    if let Some(host) = args.host {
        cfg.server.host = host;
    }
    if let Some(port) = args.port {
        cfg.server.port = port;
    }
    if let Some(port) = args.admin_port {
        cfg.server.admin_port = port;
        cfg.server.admin_port_explicit = true;
    }
    if let Some(port) = args.grpc_port {
        cfg.server.grpc_port = port;
        cfg.server.grpc_port_explicit = true;
    }
    if args.insecure_no_auth {
        cfg.server.insecure_no_auth = true;
    }
    if let Some(role) = args.role {
        cfg.server.role = role;
    }
    if let Some(url) = &args.storage_url {
        cfg.storage.url = url.clone();
    }
    if let Some(timeout) = args.llm_timeout
        && let Some(name) = cfg.llm.default_profile.clone()
        && let Some(profile) = cfg.llm.profiles.get_mut(&name)
    {
        profile.timeout = timeout.into();
    }
}

/// The effective configuration: the config file (or the zero-config one)
/// with the `MB_*` environment and then the flags applied. In zero-config
/// mode `llm` stands in for the `MB_LLM_*` variables.
pub fn resolve_config(args: &ServeArgs, llm: Option<&LlmEnv>) -> anyhow::Result<FileConfig> {
    let mut cfg = match &args.config {
        Some(path) => {
            let mut cfg = file_config::load(path)?;
            file_config::apply_env(&mut cfg)?;
            cfg
        }
        None => {
            let mut cfg = zero_config(args, llm)?;
            // MB_LLM_* are already covered by `llm`.
            file_config::apply_env_from(&mut cfg, |k| {
                if k.starts_with("MB_LLM_") {
                    None
                } else {
                    std::env::var(k).ok()
                }
            })?;
            cfg
        }
    };
    apply_flags(&mut cfg, args);
    cfg.validate_ranges().map_err(|e| anyhow!(e))?;
    cfg.auth.validate().map_err(|e| anyhow!(e))?;
    crate::tls::check_external_url(&cfg).map_err(|e| anyhow!(e))?;
    Ok(cfg)
}

/// Whether some profile's key is read from an unset `MB_LLM_API_KEY`
/// (always false with `--offline`, which builds no provider).
pub fn missing_env_api_key(cfg: &FileConfig, offline: bool) -> bool {
    !offline
        && cfg
            .llm
            .profiles
            .values()
            .any(|p| p.api_key.as_deref() == Some(ENV_API_KEY_REF))
        && std::env::var("MB_LLM_API_KEY")
            .ok()
            .filter(|v| !v.is_empty())
            .is_none()
}

/// Binds the admin listener. When the port was not set explicitly and is
/// already in use, logs a warning and returns `None`: the data plane still
/// serves, without the Admin API and ops endpoints. Any other failure, or
/// a busy explicitly set port, is an error.
pub async fn bind_admin(
    addr: std::net::SocketAddr,
    explicit: bool,
) -> anyhow::Result<Option<tokio::net::TcpListener>> {
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => Ok(Some(listener)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && !explicit => {
            tracing::warn!(
                "admin port {addr} is in use; continuing without the Admin API and ops endpoints \
                 (set --admin-port, MB_ADMIN_PORT or server.admin_port to choose another)"
            );
            Ok(None)
        }
        Err(e) => Err(anyhow!("cannot bind {addr}: {e}")),
    }
}

/// Binds the gRPC listener, with the same policy as [`bind_admin`]: a busy
/// port that was not set explicitly is skipped with a warning (`None`).
pub async fn bind_grpc(
    addr: std::net::SocketAddr,
    explicit: bool,
) -> anyhow::Result<Option<tokio::net::TcpListener>> {
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => Ok(Some(listener)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && !explicit => {
            tracing::warn!(
                "gRPC port {addr} is in use; continuing without gRPC mocks \
                 (set --grpc-port, MB_GRPC_PORT or server.grpc_port to choose another)"
            );
            Ok(None)
        }
        Err(e) => Err(anyhow!("cannot bind {addr}: {e}")),
    }
}

/// [`build_app_with`] with the self-hosted [`Extensions`].
pub async fn build_app(args: &ServeArgs, llm: Option<LlmEnv>) -> anyhow::Result<App> {
    build_app_with(args, llm, Extensions::default()).await
}

/// Opens the stores, sets up Admin API authentication (`auth.mode`; in
/// `oidc` mode a failed provider discovery is an error) and the audit log,
/// joins leader election for [`LEADER_LOCK`] (see [`Ha::leader_backend`]),
/// mounts the mocks and builds the routers (the admin one serving the
/// console as its fallback behind [`mb_admin::console_gate`], and behind
/// the host guard, see [`HostPolicy`]). The leader reconciles the
/// configuration into the control store before the mocks are mounted; on
/// every node but a `data` one, [`LeaderWork`] then runs the leader-only
/// work (the [`Maintenance`] jobs and, with `--watch`, watching the config
/// file) while this node leads. `server.role` decides the rest: a `data`
/// node serves only the ops endpoints on its admin port, authenticates
/// API keys only and runs no import workers; a `control` node builds its
/// mocks for manual publish only ([`ServeMode::ControlOnly`]). `llm: None`
/// (or `--offline`) means faker only in zero-config mode; `llm.api_key`,
/// when set, is what the reference `env:MB_LLM_API_KEY` resolves to.
///
/// Must be called inside a Tokio runtime.
pub async fn build_app_with(
    args: &ServeArgs,
    llm: Option<LlmEnv>,
    ext: Extensions,
) -> anyhow::Result<App> {
    build_app_on(args, llm, ext, None, None).await
}

/// [`build_app_with`] on the given cross-node bus instead of the one the
/// configuration names; the node id is the bus's (tests join several nodes
/// to one [`mb_state::MemoryBusHub`]).
#[doc(hidden)]
pub async fn build_app_with_bus(
    args: &ServeArgs,
    llm: Option<LlmEnv>,
    ext: Extensions,
    bus: Arc<dyn Bus>,
) -> anyhow::Result<App> {
    build_app_on(args, llm, ext, Some(bus), None).await
}

/// [`build_app_with_bus`] with leadership from `leader` instead of
/// [`Ha::leader_backend`] (tests flip a [`LeaderBackend::Manual`]).
#[doc(hidden)]
pub async fn build_app_with_leader(
    args: &ServeArgs,
    llm: Option<LlmEnv>,
    ext: Extensions,
    bus: Arc<dyn Bus>,
    leader: LeaderBackend,
) -> anyhow::Result<App> {
    build_app_on(args, llm, ext, Some(bus), Some(leader)).await
}

async fn build_app_on(
    args: &ServeArgs,
    llm: Option<LlmEnv>,
    mut ext: Extensions,
    bus: Option<Arc<dyn Bus>>,
    leader: Option<LeaderBackend>,
) -> anyhow::Result<App> {
    let cfg = resolve_config(args, llm.as_ref())?;
    let role = cfg.server.role;
    if role == Role::Data && args.watch {
        bail!("{WATCH_ON_DATA_NODE}");
    }
    // The certificate files and the CA bundle are read before anything is
    // opened, so a bad file fails startup at once.
    let tls = match &cfg.server.tls {
        Some(tls) => Some((tls.clone(), crate::tls::load_rustls(tls).await?)),
        None => None,
    };
    let http = crate::tls::outbound_options(&cfg.outbound)?;
    if let Some(key) = llm.as_ref().and_then(|l| l.api_key.clone()) {
        ext.secrets = Arc::new(EnvKeyOverlay {
            key,
            inner: ext.secrets.clone(),
        });
    }

    let store: Arc<dyn ControlStore> = mb_store::connect(&cfg.storage.url)
        .await
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| "opening the control store")?;
    let state = mb_state::connect(cfg.storage.effective_state_url(), cfg.storage.session_ttl)
        .await
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| "opening the state store")?;
    // One id names this node on the bus and as the scheduler lease holder.
    let ha = match bus {
        Some(bus) => Ha::start_with_bus(bus, store.clone()).await,
        None => Ha::start(&cfg, store.clone(), uuid::Uuid::new_v4().to_string())
            .await
            .context("connecting the cross-node bus")?,
    };
    let state = ha.shared_state(state);

    let broker_configs = file_config::broker_configs(&cfg).context("applying the configuration")?;
    // Before anything is applied, so a bad auth setup changes nothing.
    let (auth, web_auth) = match role {
        // Data nodes take API keys only: no console login, no OIDC discovery.
        Role::Data => data_node_auth(&cfg.auth, &store),
        Role::All | Role::Control => admin_auth(&cfg.auth, &store, &ext, &http).await?,
    };
    let audit_sink = match role {
        Role::Data => None,
        Role::All | Role::Control => audit_webhook(&cfg, &ext, &http).await?,
    };
    let audit = mb_admin::Auditor::new(store.clone(), audit_sink);

    // A data node never leads: it never reconciles.
    let backend = match (role, leader) {
        (Role::Data, _) => LeaderBackend::Manual(tokio::sync::watch::channel(false).1),
        (_, Some(leader)) => leader,
        (_, None) => Ha::leader_backend(&cfg),
    };
    let leadership = Arc::new(
        Leadership::start(backend, LEADER_LOCK, ha.node_id.clone())
            .await
            .map_err(|e| anyhow!("{e}"))
            .context("starting leader election")?,
    );
    let reconciles = Arc::new(AtomicU64::new(0));
    // The leader reconciles before the registry loads; the others serve
    // what the leader stored.
    let reconciled = leadership.is_leader();
    if reconciled {
        reconciles.fetch_add(1, Ordering::SeqCst);
        let report = reconcile::reconcile(store.as_ref(), state.as_ref(), &cfg)
            .await
            .context("applying the configuration")?;
        tracing::debug!(?report, "configuration applied");
    } else {
        tracing::info!(
            role = role.as_str(),
            "not the leader; serving the configuration the leader applies"
        );
    }

    let recorder = StoreRecorder::new(
        store.clone(),
        cfg.observability.request_log_retention,
        cfg.observability.redact_fields.clone(),
    );
    recorder.clone().with_bus(ha.clone());
    let (protocols, stream) = cfg.protocol_settings();
    let scrubber = match cfg.scrubber_config() {
        Some(pii) => Some(Arc::new(
            Scrubber::new(&pii)
                .map_err(|e| anyhow!("{e}"))
                .context("building the PII scrubber")?,
        )),
        None => None,
    };
    let deps = RuntimeDeps {
        store: store.clone(),
        state: state.clone(),
        ext: ext.clone(),
        recorder: Some(recorder.clone()),
        offline: args.offline,
        default_profile: DefaultProfile::new(cfg.llm.default_profile.clone()),
        data_plane_keys: Some(StoreDataPlaneKeys::new(store.clone())),
        protocols,
        stream,
        events: EventDeps {
            brokers: Brokers::new(broker_configs.clone(), ext.secrets.clone()),
            lease: Arc::new(StoreLease {
                store: store.clone(),
            }),
            holder: ha.node_id.clone(),
            shared_subscriptions: !ha.bus.is_local(),
        },
        limiter: GenerationLimiter::new(cfg.limits.max_concurrent_generations),
        scrubber,
        limits: cfg.limits.clone(),
        http: http.clone(),
        mode: match role {
            Role::Control => ServeMode::ControlOnly,
            Role::All | Role::Data => ServeMode::Full,
        },
    };
    let brokers = deps.events.brokers.clone();
    let registry = MockRegistry::new();
    // Subscribe before the first load, so no change after it is missed.
    let changes = ha.changes();
    registry.reload_all(&deps).await?;
    registry.spawn_watcher(deps.clone(), changes);
    for (workspace, name, prefix) in registry.mounted() {
        let name = mock_label(&workspace, &name);
        tracing::info!(
            "serving {name} at http://{}:{}{prefix}",
            cfg.server.host,
            cfg.server.port
        );
    }
    for c in registry.served_channels() {
        let address = match c.protocol {
            "websocket" => format!("ws://{}:{}{}", cfg.server.host, cfg.server.port, c.address),
            _ => c.address,
        };
        tracing::info!(
            "serving {} {} channel {} ({}) at {address}",
            c.mock,
            c.protocol,
            c.id,
            c.direction
        );
    }
    let grpc = registry.grpc();
    for (service, name) in grpc.services() {
        tracing::info!(
            "serving {name} gRPC {service} at {}:{}",
            cfg.server.host,
            cfg.server.grpc_port
        );
    }

    let data = Router::new()
        .route(
            HEALTH_PATH,
            axum::routing::get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        )
        .merge(registry.router());

    let sessions = web_auth.as_ref().map(|w| w.sessions.clone());
    let prometheus = telemetry::install_prometheus()?;
    let admin_state = mb_admin::AdminState {
        api_secret_refs: Arc::new(cfg.server.api_secret_refs.clone()),
        live: Some({
            let recorder = recorder.clone();
            Arc::new(move || recorder.live())
        }),
        auth,
        audit,
        web_auth,
        ext: deps.ext.clone(),
        llm: Arc::new(RuntimeLlm {
            deps: deps.clone(),
            import_profile: cfg.import.llm_profile.clone(),
        }),
        imports: mb_admin::Imports {
            settings: {
                let mut settings = cfg.import.settings().map_err(|e| anyhow!(e))?;
                // CA bundle only: the SSRF-guarded fetch never uses a proxy.
                settings.fetch.extra_roots_pem = http.extra_roots_pem.clone();
                settings
            },
            ..Default::default()
        },
        ..mb_admin::AdminState::new(store.clone(), state.clone())
    };
    let import_workers = (role != Role::Data).then(|| mb_admin::spawn_import_workers(&admin_state));
    let metrics_gate = (!cfg.auth.public_metrics).then(|| admin_state.clone());
    let hooks_auth = admin_state.clone();
    // In `oidc` mode only authenticated callers see why `/readyz` is not ready.
    let readyz_auth = (cfg.auth.mode == AuthMode::Oidc).then(|| admin_state.auth.clone());
    let ops = ops::router_with_metrics_gate(
        OpsState {
            store: store.clone(),
            state: state.clone(),
            registry: registry.clone(),
            prometheus,
            readyz_auth,
            bus: Some(ha.bus.clone()),
            role,
            leader: leadership.watch(),
        },
        metrics_gate,
    );
    let mut admin = match role {
        // A data node's admin port serves the ops endpoints only.
        Role::Data => ops,
        Role::All | Role::Control => {
            let console = mb_console::router().layer(axum::middleware::from_fn_with_state(
                admin_state.clone(),
                mb_admin::console_gate,
            ));
            let mut admin = mb_admin::router(admin_state)
                .merge(ops)
                .layer(axum::Extension::<SharedChannelControl>(Arc::new(
                    RegistryChannels {
                        registry: registry.clone(),
                        brokers: brokers.clone(),
                    },
                )));
            if let Some(routes) = ext.admin_hooks.routes() {
                // Extension routes get the same authentication as the Admin API.
                admin = admin.merge(routes.layer(axum::middleware::from_fn_with_state(
                    hooks_auth,
                    mb_admin::authenticate,
                )));
            }
            // Any other path is the console, behind the same credentials as the API.
            admin.fallback_service(console)
        }
    };
    if let Some(policy) = host_policy(&cfg)? {
        admin = admin.layer(axum::middleware::from_fn_with_state(
            policy,
            crate::host_guard::host_guard,
        ));
    }

    let leader_work = match role {
        Role::Data => None,
        Role::All | Role::Control => {
            let flags = args.clone();
            let initial_brokers = broker_configs.clone();
            let prepare: watch::Prepare = Arc::new(move |cfg: &mut FileConfig| {
                file_config::apply_env(cfg)?;
                apply_flags(cfg, &flags);
                cfg.validate_ranges().map_err(|e| anyhow!(e))?;
                // Brokers are connected at startup only: a reload that
                // changes them is rejected as a whole.
                ensure_brokers_unchanged(&initial_brokers, cfg)
            });
            let applied = applied_hook(registry.clone(), deps, recorder.clone());
            let jobs = LeaderJobs {
                reconcile: reconcile_once(ReconcileCtx {
                    config_path: args.config.clone(),
                    startup: cfg.clone(),
                    prepare: prepare.clone(),
                    applied: applied.clone(),
                    store: store.clone(),
                    state: state.clone(),
                    count: reconciles.clone(),
                }),
                watch: match (&args.config, args.watch) {
                    (Some(path), true) => {
                        let path = path.clone();
                        let (store, state) = (store.clone(), state.clone());
                        let start: crate::maintenance::StartWatch = Arc::new(move || {
                            watch::spawn_with_hook(
                                path.clone(),
                                store.clone(),
                                state.clone(),
                                prepare.clone(),
                                Some(applied.clone()),
                            )
                        });
                        Some(start)
                    }
                    _ => None,
                },
                maintenance: {
                    let store = store.clone();
                    let retention_days = cfg.audit.retention_days;
                    Arc::new(move || {
                        Maintenance::start(store.clone(), sessions.as_ref(), retention_days)
                    })
                },
            };
            Some(LeaderWork::spawn(leadership.watch(), reconciled, jobs))
        }
    };

    let state_gauge = match role {
        Role::Control => None,
        Role::All | Role::Data => Some(StateGauge::spawn(state.clone(), registry.clone())),
    };

    let (tls, tls_reloader) = match tls {
        Some((files, rustls)) => {
            let reloader = crate::tls::spawn_reloader(files, rustls.clone());
            (Some(rustls), Some(crate::tls::ReloaderGuard(reloader)))
        }
        None => (None, None),
    };
    Ok(App {
        data,
        admin,
        grpc: grpc.router(),
        registry,
        config: cfg,
        role,
        leadership,
        leader_work,
        state_gauge,
        reconciles,
        recorder,
        brokers,
        import_workers,
        ha,
        state,
        tls,
        tls_reloader,
    })
}

/// Why `--watch` is refused on a `data` node.
pub const WATCH_ON_DATA_NODE: &str = "--watch cannot be used with role data: data nodes never apply the config file (run --watch on a control node)";

/// What a leader reconciles with.
struct ReconcileCtx {
    /// The config file, re-read on every reconcile; `None` in zero-config
    /// mode, which reconciles `startup`.
    config_path: Option<PathBuf>,
    startup: FileConfig,
    prepare: watch::Prepare,
    applied: watch::Applied,
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    count: Arc<AtomicU64>,
}

/// Reconciles the current configuration (the config file read afresh, as
/// the previous leader's `--watch` may have applied changes since startup)
/// into the store, then applies its out-of-store settings.
fn reconcile_once(ctx: ReconcileCtx) -> crate::maintenance::ReconcileOnce {
    let ctx = Arc::new(ctx);
    Arc::new(move || {
        let ctx = ctx.clone();
        Box::pin(async move {
            ctx.count.fetch_add(1, Ordering::SeqCst);
            let cfg = match &ctx.config_path {
                Some(path) => {
                    let mut cfg = file_config::load(path)?;
                    (ctx.prepare)(&mut cfg)?;
                    cfg
                }
                None => ctx.startup.clone(),
            };
            let report = reconcile::reconcile(ctx.store.as_ref(), ctx.state.as_ref(), &cfg)
                .await
                .context("applying the configuration")?;
            tracing::debug!(?report, "configuration applied");
            (ctx.applied)(cfg).await;
            Ok(())
        })
    })
}

/// A `data` node's authenticator: `none` mode is [`mb_auth::LocalAuth`];
/// otherwise API keys only (no console sessions, no OIDC bearer tokens, so
/// no provider discovery).
fn data_node_auth(
    auth: &AuthConfig,
    store: &Arc<dyn ControlStore>,
) -> (Arc<dyn mb_auth::Authenticate>, Option<mb_admin::WebAuth>) {
    match auth.mode {
        AuthMode::None => (Arc::new(mb_auth::LocalAuth), None),
        AuthMode::Oidc => (
            Arc::new(mb_auth::Authenticator {
                api_keys: mb_auth::ApiKeys::new(store.clone()),
                sessions: None,
                bearer: None,
            }),
            None,
        ),
    }
}

/// The Admin API's authenticator and, in `oidc` mode, the browser login.
///
/// `none` mode is [`mb_auth::LocalAuth`]. `oidc` mode resolves the client
/// secret through `ext.secrets`, discovers the provider (a failure is an
/// error naming the issuer) with the redirect URL
/// `<external_url>/auth/callback`, and accepts API keys, console sessions
/// and the provider's bearer JWTs.
async fn admin_auth(
    auth: &AuthConfig,
    store: &Arc<dyn ControlStore>,
    ext: &Extensions,
    http: &mb_llm::HttpOptions,
) -> anyhow::Result<(Arc<dyn mb_auth::Authenticate>, Option<mb_admin::WebAuth>)> {
    let oidc = match (auth.mode, &auth.oidc) {
        (AuthMode::None, _) => return Ok((Arc::new(mb_auth::LocalAuth), None)),
        (AuthMode::Oidc, Some(oidc)) => oidc,
        (AuthMode::Oidc, None) => bail!("auth.mode oidc requires an auth.oidc section"),
    };
    let client_secret = match &oidc.client_secret {
        Some(reference) => Some(
            ext.secrets
                .resolve(reference)
                .await
                .map_err(|e| anyhow!("{e}"))
                .context("resolving auth.oidc.client_secret")?,
        ),
        None => None,
    };
    let settings = mb_auth::OidcSettings {
        issuer: oidc.issuer.clone(),
        client_id: oidc.client_id.clone(),
        client_secret,
        scopes: oidc.scopes.clone(),
        audience: oidc.audience.clone(),
        redirect_url: format!("{}/auth/callback", auth.external_base()),
        mapping: mb_auth::RoleMapping {
            roles_claim: oidc.roles_claim.clone(),
            group_prefix: oidc.group_prefix.clone(),
            sysadmin_subjects: auth.sysadmin_subjects.clone(),
        },
    };
    let http = mb_auth::http_client_with(http)
        .map_err(|e| anyhow!("{e}"))
        .context("building the OIDC HTTP client from the outbound settings")?;
    let client = mb_auth::OidcClient::discover(settings, http)
        .await
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("cannot set up OIDC with issuer {}", oidc.issuer))?;
    let client = Arc::new(client);
    let secure = auth.secure_cookies();
    let sessions = mb_auth::SessionManager::new(store.clone(), auth.session_ttl, secure);
    let authenticator = mb_auth::Authenticator {
        api_keys: mb_auth::ApiKeys::new(store.clone()),
        sessions: Some(sessions.clone()),
        bearer: Some(client.bearer()),
    };
    let web = mb_admin::WebAuth {
        oidc: client,
        sessions,
        secure_cookies: secure,
    };
    Ok((Arc::new(authenticator), Some(web)))
}

/// The audit webhook from `audit.webhook_url`, its secret resolved through
/// `ext.secrets`; `None` without a URL.
async fn audit_webhook(
    cfg: &FileConfig,
    ext: &Extensions,
    http: &mb_llm::HttpOptions,
) -> anyhow::Result<Option<mb_admin::WebhookSink>> {
    let Some(url) = &cfg.audit.webhook_url else {
        return Ok(None);
    };
    let secret = match &cfg.audit.webhook_secret {
        Some(reference) => Some(
            ext.secrets
                .resolve(reference)
                .await
                .map_err(|e| anyhow!("{e}"))
                .context("resolving audit.webhook_secret")?,
        ),
        None => None,
    };
    Ok(Some(mb_admin::WebhookSink::spawn(
        url.clone(),
        secret,
        mb_admin::WebhookSink::client(http)
            .map_err(|e| anyhow!("{e}"))
            .context("building the audit webhook client")?,
    )))
}

/// The admin port's [`HostPolicy`]: loopback names in `none` mode on a
/// loopback bind, the `auth.external_url` host in `oidc` mode, and no
/// check otherwise.
fn host_policy(cfg: &FileConfig) -> anyhow::Result<Option<HostPolicy>> {
    Ok(match cfg.auth.mode {
        AuthMode::Oidc => Some(HostPolicy::Exact(
            cfg.auth.external_host().map_err(|e| anyhow!(e))?,
        )),
        AuthMode::None if cfg.server.host.is_loopback() => Some(HostPolicy::Loopback),
        AuthMode::None => None,
    })
}

/// Applies the settings a reload changed that live outside the store: the
/// request-log settings, and the default LLM profile, rebuilding every mock
/// when it changed (mocks naming no profile resolve it at build time).
fn applied_hook(
    registry: Arc<MockRegistry>,
    deps: RuntimeDeps,
    recorder: Arc<StoreRecorder>,
) -> watch::Applied {
    Arc::new(move |cfg: FileConfig| {
        let registry = registry.clone();
        let deps = deps.clone();
        let recorder = recorder.clone();
        Box::pin(async move {
            recorder.set_settings(
                cfg.observability.request_log_retention,
                cfg.observability.redact_fields,
            );
            if deps.default_profile.set(cfg.llm.default_profile) {
                tracing::info!(
                    default_profile = ?deps.default_profile.get(),
                    "default LLM profile changed; rebuilding mocks"
                );
                if let Err(error) = registry.reload_all(&deps).await {
                    tracing::error!(error = %format!("{error:#}"), "cannot reload mocks");
                }
            }
        })
    })
}

/// `Ok` when `cfg`'s `brokers:` section validates to exactly `initial`
/// (the brokers this process started with); otherwise an error saying
/// "brokers changed: restart required" (and naming what changed), so a
/// `--watch` reload that changes brokers is rejected and the previous
/// state kept.
pub fn ensure_brokers_unchanged(
    initial: &[mb_protocols::events::BrokerConfig],
    cfg: &FileConfig,
) -> anyhow::Result<()> {
    let next = file_config::broker_configs(cfg)?;
    if next.as_slice() == initial {
        return Ok(());
    }
    let names = |configs: &[mb_protocols::events::BrokerConfig]| {
        configs
            .iter()
            .map(|c| c.name.clone())
            .collect::<std::collections::BTreeSet<_>>()
    };
    let (old, new) = (names(initial), names(&next));
    let added: Vec<_> = new.difference(&old).collect();
    let removed: Vec<_> = old.difference(&new).collect();
    let changed: Vec<_> = next
        .iter()
        .filter(|c| initial.iter().any(|i| i.name == c.name && i != *c))
        .map(|c| &c.name)
        .collect();
    bail!(
        "brokers changed: restart required (added {added:?}, removed {removed:?}, changed {changed:?}); the brokers are read at startup only"
    )
}

/// Resolves `env:MB_LLM_API_KEY` to the key handed to [`build_app`], and
/// everything else through the wrapped store.
struct EnvKeyOverlay {
    key: String,
    inner: Arc<dyn SecretStore>,
}

#[async_trait]
impl SecretStore for EnvKeyOverlay {
    async fn resolve(&self, reference: &str) -> Result<String, ExtError> {
        if reference == ENV_API_KEY_REF {
            Ok(self.key.clone())
        } else {
            self.inner.resolve(reference).await
        }
    }
}
