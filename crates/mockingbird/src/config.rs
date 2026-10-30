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

use crate::client::{ClientCommand, MocksUpdateArgs, WorkflowCommand, WorkflowTop};
use crate::dp_keys::StoreDataPlaneKeys;
use crate::events::{RegistryChannels, StoreLease};
use crate::file_config::{
    self, AuthConfig, AuthMode, DEFAULT_LLM_BASE_URL, DEFAULT_LLM_MODEL, ENV_API_KEY_REF,
    ENV_PROFILE, FileConfig, FileMock, FileProfile, Role,
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
        default_value = mb_store::DEFAULT_WORKSPACE,
        value_parser = parse_workspace
    )]
    pub workspace: String,
    #[command(subcommand)]
    pub cmd: Command,
}

/// A parsed command line: a [`Cli`] command, or one of the
/// [`WorkflowCommand`]s that [`command`] adds to it.
#[derive(Debug)]
#[non_exhaustive]
pub enum Invocation {
    /// A [`Command`] with the global flags.
    Cli(Box<Cli>),
    /// A workflow command with the global flags it runs with.
    Workflow {
        /// `--admin-url` (`MB_ADMIN_URL`).
        admin_url: String,
        /// `--token` (`MB_ADMIN_TOKEN`).
        token: Option<String>,
        /// `--workspace` (`MB_WORKSPACE`).
        workspace: String,
        /// The command.
        cmd: WorkflowCommand,
    },
}

/// The whole `mockingbird` command line: [`Cli`] with `mocks update`,
/// `drafts`, `contracts` and `api` added (the [`WorkflowCommand`]s).
#[must_use]
pub fn command() -> clap::Command {
    use clap::{Args as _, CommandFactory as _, Subcommand as _};
    let cmd = Cli::command().mut_subcommand("mocks", |mocks| {
        mocks.subcommand(
            MocksUpdateArgs::augment_args(clap::Command::new("update"))
                .about("Change some fields of a mock; the others keep their values"),
        )
    });
    WorkflowTop::augment_subcommands(cmd)
}

/// Parses `args` (the program name first) against [`command`].
///
/// # Errors
///
/// The [`clap::Error`] to print and exit with (help and `--version`
/// included).
pub fn parse_from<I, T>(args: I) -> Result<Invocation, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    use clap::{FromArgMatches as _, Subcommand as _};
    let mut matches = command().try_get_matches_from(args)?;
    let workflow = match matches.subcommand() {
        Some(("mocks", mocks)) => match mocks.subcommand() {
            Some(("update", update)) => Some(WorkflowCommand::MocksUpdate(
                MocksUpdateArgs::from_arg_matches(update).map_err(formatted)?,
            )),
            _ => None,
        },
        Some((name, _)) if WorkflowTop::has_subcommand(name) => Some(
            WorkflowTop::from_arg_matches(&matches)
                .map_err(formatted)?
                .into(),
        ),
        _ => None,
    };
    let Some(cmd) = workflow else {
        return Cli::from_arg_matches_mut(&mut matches)
            .map(|cli| Invocation::Cli(Box::new(cli)))
            .map_err(formatted);
    };
    let global = |id: &str| matches.get_one::<String>(id).cloned();
    Ok(Invocation::Workflow {
        admin_url: global("admin_url")
            .unwrap_or_else(|| crate::client::DEFAULT_ADMIN_URL.to_string()),
        token: global("token"),
        workspace: global("workspace").unwrap_or_else(|| mb_store::DEFAULT_WORKSPACE.to_string()),
        cmd,
    })
}

/// [`parse_from`] the process arguments; prints the error (or help) and
/// exits on failure.
#[must_use]
pub fn parse() -> Invocation {
    parse_from(std::env::args_os()).unwrap_or_else(|e| e.exit())
}

/// An error from turning matches into types, formatted like a parse error.
fn formatted(e: clap::Error) -> clap::Error {
    e.format(&mut command())
}

/// A `--workspace` (`MB_WORKSPACE`) value: a valid workspace name, so an
/// empty or malformed one is a usage error rather than a 404.
fn parse_workspace(name: &str) -> Result<String, String> {
    mb_store::validate_workspace_name(name)
        .map(|()| name.to_string())
        .map_err(|e| match e {
            mb_store::StoreError::Invalid(detail) => detail,
            other => other.to_string(),
        })
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Serve one or more contracts as live mocks.
    Serve(ServeArgs),
    /// Compile a contract and print an operation summary.
    Validate { contract: PathBuf },
    /// Print the Admin API's OpenAPI document as JSON.
    Openapi,
    /// Write a starter mockingbird.yaml: storage, an LLM profile from a
    /// provider preset, and mocks for the given contracts.
    Init(crate::onboard::InitArgs),
    /// LLM provider onboarding: list presets, configure a profile, test it.
    #[command(subcommand)]
    Llm(crate::onboard::LlmCommand),
    /// Run `serve` in the background: start, stop, status, logs.
    #[command(subcommand)]
    Server(crate::daemon::ServerCommand),
    /// Report the installation, configuration, provider keys and server
    /// state as JSON.
    Doctor(crate::daemon::DoctorArgs),
    /// Run the Model Context Protocol server that exposes Mockingbird to
    /// coding agents (stdio by default, or streamable HTTP).
    Mcp(crate::mcp::McpArgs),
    /// Set Mockingbird up in a coding agent: its MCP server entry and
    /// skills (Claude Code, Codex, Cursor, Gemini CLI, Kiro, VS Code,
    /// Windsurf, Claude Desktop).
    #[command(subcommand)]
    Agent(crate::agents::AgentCommand),
    /// Run the intercepting HTTP proxy in front of a running server, or
    /// print the variables that send an app through it (`proxy env`).
    Proxy(crate::proxy::ProxyArgs),
    /// Run a command against the mocks: starts the server and proxy unless
    /// one already answers, runs the command with the proxy environment for
    /// every runtime, stops what it started and exits with the command's
    /// code: `mockingbird run -c mockingbird.yaml -- npm test`.
    Run(crate::run::RunArgs),
    /// The local CA the proxy signs certificates with.
    #[command(subcommand)]
    Ca(crate::proxy::CaCommand),
    /// Store an LLM key in a private file through a local page, and point
    /// the profile at it.
    #[command(subcommand)]
    Keys(crate::keys::KeysCommand),
    /// Copy contracts and session state between a running server and a
    /// snapshot directory (`serve --record` / `--replay` use the same one).
    #[command(subcommand)]
    Snapshot(crate::snapshot::SnapshotCommand),
    /// Summarise what the running server answered: requests per mock and
    /// operation, response sources and fallbacks, error statuses, unmapped
    /// proxy hosts.
    Report(crate::report::ReportArgs),
    /// Check the report against assertions (no fallbacks, no unmapped
    /// hosts, operations called); exits 1 when one fails.
    Verify(crate::report::VerifyArgs),
    /// The catalog of ready-made API examples (Stripe, GitHub, Twilio, ...):
    /// list them, show one, or write one into a directory.
    #[command(subcommand)]
    Examples(crate::examples::ExamplesCommand),
    #[command(flatten)]
    Client(ClientCommand),
}

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
    /// Control-plane storage: `sqlite::memory:`, `sqlite://<path>` or
    /// `postgres://…`.
    #[arg(
        long,
        help = "Control-plane storage: sqlite::memory:, sqlite://<path> or postgres://…"
    )]
    pub storage_url: Option<String>,
    /// Allow binding a non-loopback address without authentication
    /// (`auth.mode: none`).
    #[arg(long)]
    pub insecure_no_auth: bool,
    /// Faker only, no LLM.
    #[arg(long)]
    pub offline: bool,
    /// LLM timeout of the default profile [default: 60s].
    #[arg(long)]
    pub llm_timeout: Option<humantime::Duration>,
    /// Which planes this node serves [default: all]; `control` and `data`
    /// need shared Postgres (and Postgres or Redis state) stores.
    #[arg(long, value_enum)]
    pub role: Option<file_config::Role>,
    /// Reconcile again whenever the config file or a contract changes.
    #[arg(long, requires = "config")]
    pub watch: bool,
    /// Start the intercepting HTTP proxy (`proxy.enabled`); see
    /// `mockingbird proxy env`.
    #[arg(long)]
    pub proxy: bool,
    /// The proxy's port [default: 8888].
    #[arg(long)]
    pub proxy_port: Option<u16>,
    /// Append every LLM answer to `<DIR>/answers.jsonl` for a later
    /// `--replay`.
    #[arg(long, value_name = "DIR")]
    pub record: Option<PathBuf>,
    /// Answer from the snapshot in `<DIR>` and never call an LLM (a prompt
    /// it holds no answer for falls back to the faker); with `--record` on
    /// the same directory, misses go to the LLM and are recorded.
    #[arg(long, value_name = "DIR")]
    pub replay: Option<PathBuf>,
    #[arg(required_unless_present = "config")]
    pub contracts: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct LlmEnv {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    /// `MB_LLM_OUTPUT_MODE`, else the deprecated `MB_LLM_JSON_SCHEMA`;
    /// `None` when neither is set or the value is not a mode.
    pub output_mode: Option<mb_store::OutputMode>,
}

impl LlmEnv {
    /// Reads `MB_LLM_*`, applying the documented defaults.
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            base_url: get("MB_LLM_BASE_URL").unwrap_or_else(|| DEFAULT_LLM_BASE_URL.into()),
            model: get("MB_LLM_MODEL").unwrap_or_else(|| DEFAULT_LLM_MODEL.into()),
            api_key: get("MB_LLM_API_KEY"),
            output_mode: get("MB_LLM_OUTPUT_MODE")
                .and_then(|v| v.parse().ok())
                .or_else(|| {
                    get("MB_LLM_JSON_SCHEMA").map(|v| {
                        mb_store::OutputMode::from_json_schema(v.eq_ignore_ascii_case("true"))
                    })
                }),
        }
    }
}

/// The mock name for a contract file: its stem.
///
/// # Errors
///
/// Fails when `path` has no file stem usable as a mock name.
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
    /// The outcome of the last `--watch` reload, which `/readyz` reports.
    pub reload: watch::ReloadStatus,
    /// The intercepting proxy (`proxy.enabled` on a node serving the data
    /// plane); `None` otherwise.
    pub proxy: Option<Arc<crate::proxy::ProxyState>>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("role", &self.role)
            .field("leader_work", &self.leader_work.is_some())
            .field("tls", &self.tls.is_some())
            .field("proxy", &self.proxy.is_some())
            .finish_non_exhaustive()
    }
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

/// The snapshot `--record` / `--replay` name; both must name the same
/// directory.
fn open_snapshot(args: &ServeArgs) -> anyhow::Result<Option<Arc<crate::snapshot::Snapshot>>> {
    let dir = match (&args.replay, &args.record) {
        (None, None) => return Ok(None),
        (Some(a), Some(b)) if a != b => {
            bail!("--replay and --record must name the same directory")
        }
        (Some(dir), _) | (None, Some(dir)) => dir,
    };
    let snapshot = crate::snapshot::Snapshot::open_with(
        dir,
        crate::snapshot::SnapshotMode {
            replay: args.replay.is_some(),
            record: args.record.is_some(),
        },
    )?;
    Ok(Some(Arc::new(snapshot)))
}

/// Whether `serve` replays without recording: it then calls no LLM.
pub fn replay_only(args: &ServeArgs) -> bool {
    args.replay.is_some() && args.record.is_none()
}

/// The zero-config [`FileConfig`] for `serve a.yaml [b.yaml]`: SQLite in
/// memory; one mock per contract named after its file stem (lowercased,
/// other invalid characters replaced by `-`), at `/` for a single contract
/// or `/<mock name>` for several; and, unless `--offline`, a default profile
/// from `llm` whose key is the reference `env:MB_LLM_API_KEY`.
///
/// # Errors
///
/// Fails when a contract path has no usable file stem, or two contracts
/// map to the same mock name.
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
        profile.output_mode = env.output_mode;
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
    if args.proxy {
        cfg.proxy.enabled = true;
    }
    if let Some(port) = args.proxy_port {
        cfg.proxy.port = port;
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
///
/// # Errors
///
/// Fails when the config file cannot be read or parsed, an `MB_*` variable
/// is invalid, or the result breaks a range, auth or TLS rule.
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
///
/// # Errors
///
/// Fails when the address cannot be bound for any reason other than a busy
/// port that was not set explicitly.
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
///
/// # Errors
///
/// As [`bind_admin`]: any bind failure except a busy port that was not set
/// explicitly.
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
/// Its Vault client follows `outbound.proxy`, `outbound.no_proxy` and
/// `outbound.ca_file`.
///
/// # Errors
///
/// Fails as [`build_app_with`] does, and when the Vault client cannot be
/// built.
pub async fn build_app(args: &ServeArgs, llm: Option<LlmEnv>) -> anyhow::Result<App> {
    self_hosted_app(args, llm).await
}

/// [`build_app_with`] with the self-hosted [`Extensions`], whose
/// [`mb_ext::DefaultSecrets`] reaches Vault with `outbound.proxy`,
/// `outbound.no_proxy` and `outbound.ca_file`. (Embedders that pass their
/// own extensions set that up with [`mb_ext::DefaultSecrets::with_http`].)
pub(crate) async fn self_hosted_app(args: &ServeArgs, llm: Option<LlmEnv>) -> anyhow::Result<App> {
    build_app_inner(args, llm, Extensions::default(), None, None, true).await
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
///
/// # Errors
///
/// Fails when the configuration is invalid, a store or the bus cannot be
/// opened, OIDC discovery fails, the first reconcile is rejected, or the TLS
/// files cannot be loaded.
pub async fn build_app_with(
    args: &ServeArgs,
    llm: Option<LlmEnv>,
    ext: Extensions,
) -> anyhow::Result<App> {
    build_app_inner(args, llm, ext, None, None, false).await
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
    build_app_inner(args, llm, ext, Some(bus), None, false).await
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
    build_app_inner(args, llm, ext, Some(bus), Some(leader), false).await
}

/// `self_hosted_secrets`: `ext.secrets` is the default
/// [`mb_ext::DefaultSecrets`], rebuilt here with the outbound settings.
async fn build_app_inner(
    args: &ServeArgs,
    llm: Option<LlmEnv>,
    mut ext: Extensions,
    bus: Option<Arc<dyn Bus>>,
    leader: Option<LeaderBackend>,
    self_hosted_secrets: bool,
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
    if self_hosted_secrets {
        ext.secrets = Arc::new(mb_ext::DefaultSecrets::from_env().with_http(vault_http(&http)));
    }
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
    // The response cache wraps the shared store, so every write and a
    // session reset, seed or mock reset clear it, on every node.
    let state = ha.cached_shared_state(
        state,
        cfg.storage.session_ttl,
        mb_state::DEFAULT_CACHE_ENTRIES,
    );

    let broker_configs = file_config::broker_configs(&cfg).context("applying the configuration")?;
    // One key cache serves the Admin API and the data plane. A revocation
    // through this node's Admin API clears it at once; other nodes see it
    // within the TTL.
    let api_keys = mb_auth::ApiKeys::with_cache(store.clone(), mb_auth::DEFAULT_API_KEY_CACHE_TTL);
    // Before anything is applied, so a bad auth setup changes nothing.
    let (auth, web_auth) = match role {
        // Data nodes take API keys only: no console login, no OIDC discovery.
        Role::Data => data_node_auth(&cfg.auth, &api_keys),
        Role::All | Role::Control => admin_auth(&cfg.auth, &store, &api_keys, &ext, &http).await?,
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
        offline: args.offline || replay_only(args),
        default_profile: DefaultProfile::new(cfg.llm.default_profile.clone()),
        data_plane_keys: Some(StoreDataPlaneKeys::with_keys(api_keys)),
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
        limiter: GenerationLimiter::new_shared(cfg.limits.max_concurrent_generations),
        scrubber,
        limits: cfg.limits.clone(),
        http: http.clone(),
        mode: match role {
            Role::Control => ServeMode::ControlOnly,
            Role::All | Role::Data => ServeMode::Full,
        },
        webhook_log: Default::default(),
    };
    let snapshot = open_snapshot(args)?;
    crate::snapshot::install(snapshot.clone());
    let brokers = deps.events.brokers.clone();
    let registry = MockRegistry::new();
    // Subscribe before the first load, so no change after it is missed.
    let changes = ha.changes();
    registry.reload_all(&deps).await?;
    registry.spawn_watcher(deps.clone(), changes);
    if let Some(snapshot) = snapshot.as_deref().filter(|s| s.replays()) {
        let workspace = crate::snapshot::workspace_of(snapshot.dir());
        let seeded = snapshot.seed_sessions(state.as_ref(), &workspace).await?;
        tracing::info!(
            "replaying {} ({seeded} sessions seeded); no LLM is called{}",
            snapshot.dir().display(),
            if snapshot.records() {
                " except to record misses"
            } else {
                ""
            }
        );
    }
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
    let proxy = match (role, cfg.proxy.enabled) {
        (Role::All | Role::Data, true) => {
            let dir = cfg.proxy.effective_ca_dir().context("proxy.ca_dir")?;
            let ca = crate::proxy::ca::LocalCa::load_or_create(&dir)
                .with_context(|| format!("proxy.ca_dir {}", dir.display()))?;
            Some(crate::proxy::ProxyState::new(
                registry.clone(),
                data.clone(),
                Arc::new(ca),
                cfg.proxy.unmapped,
                cfg.proxy.images.clone(),
            )?)
        }
        _ => None,
    };

    let sessions = web_auth.as_ref().map(|w| w.sessions.clone());
    let prometheus = telemetry::install_prometheus()?;
    let admin_state = mb_admin::AdminState {
        api_secret_refs: Arc::new(cfg.server.api_secret_refs.clone()),
        api_webhook_secret_refs: Arc::new(cfg.server.api_webhook_secret_refs.clone()),
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
    let reload = watch::ReloadStatus::default();
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
            reload: reload.clone(),
        },
        metrics_gate,
    );
    let mut admin = match role {
        // A data node's admin port serves the ops endpoints only.
        Role::Data => ops,
        Role::All | Role::Control => {
            if let Some(warning) = console_placeholder_warning(mb_console::is_placeholder()) {
                tracing::warn!("{warning}");
            }
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
                )))
                .layer(axum::Extension(deps.webhook_log.clone()));
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
            let initial = cfg.clone();
            let prepare: watch::Prepare = Arc::new(move |cfg: &mut FileConfig| {
                file_config::apply_env(cfg)?;
                apply_flags(cfg, &flags);
                cfg.validate_ranges().map_err(|e| anyhow!(e))?;
                // The proxy and the other read-at-startup sections are
                // used once: say so instead of ignoring an edit silently.
                if cfg.proxy != initial.proxy {
                    tracing::warn!(
                        "config reload: the proxy: section changed; restart the server to apply it (mocks and their hosts are reloaded)"
                    );
                }
                for section in restart_only_changes(&initial, cfg) {
                    tracing::warn!(
                        "config reload: the {section}: section changed; restart the server to apply it"
                    );
                }
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
                        let reload = reload.clone();
                        let start: crate::maintenance::StartWatch = Arc::new(move || {
                            watch::spawn_with_status(
                                path.clone(),
                                store.clone(),
                                state.clone(),
                                prepare.clone(),
                                Some(applied.clone()),
                                reload.clone(),
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
        reload,
        proxy,
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
    api_keys: &mb_auth::ApiKeys,
) -> (Arc<dyn mb_auth::Authenticate>, Option<mb_admin::WebAuth>) {
    match auth.mode {
        AuthMode::None => (Arc::new(mb_auth::LocalAuth), None),
        AuthMode::Oidc => (
            Arc::new(mb_auth::Authenticator {
                api_keys: api_keys.clone(),
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
    api_keys: &mb_auth::ApiKeys,
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
        .with_context(|| format!("cannot set up OIDC with issuer {}", oidc.issuer))?
        // Every node shares started logins, and they survive restarts.
        .with_store(store.clone())
        .with_post_logout_redirect(&format!("{}/", auth.external_base()))
        .map_err(|e| anyhow!("{e}"))
        .context("auth.external_url")?;
    let client = Arc::new(client);
    let secure = auth.secure_cookies();
    let sessions = mb_auth::SessionManager::new(store.clone(), auth.session_ttl, secure)
        .with_idle_timeout(auth.session_idle_timeout)
        .with_role_refresh(client.clone());
    let authenticator = mb_auth::Authenticator {
        api_keys: api_keys.clone(),
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
///
/// # Errors
///
/// Fails, naming what changed, when the brokers in `cfg` differ from
/// `initial` or do not validate.
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

/// The Vault client's share of the outbound settings.
fn vault_http(http: &mb_llm::HttpOptions) -> mb_ext::VaultHttp {
    let mut vault = mb_ext::VaultHttp::new()
        .with_no_proxy(http.no_proxy.clone())
        .with_extra_roots_pem(http.extra_roots_pem.clone());
    if let Some(proxy) = &http.proxy {
        vault = vault.with_proxy(proxy.clone());
    }
    vault
}

/// The startup warning of a binary built without the console (`placeholder`:
/// [`mb_console::is_placeholder`]), which then serves a "Console not built"
/// page on the admin port.
fn console_placeholder_warning(placeholder: bool) -> Option<&'static str> {
    placeholder.then_some(
        "this binary was built without the console: the admin port serves a \
         \"Console not built\" page. Build it with `npm --prefix crates/mb-console/web ci && \
         npm --prefix crates/mb-console/web run build`, then rebuild mockingbird",
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

/// The read-at-startup sections (besides `proxy:` and `brokers:`, which a
/// reload handles itself) that differ between `initial`, the configuration
/// the process started with, and `next`, a reloaded one: a `--watch`
/// reload cannot apply them, so it warns about each.
fn restart_only_changes(initial: &FileConfig, next: &FileConfig) -> Vec<&'static str> {
    let sections = [
        ("server", initial.server != next.server),
        ("storage", initial.storage != next.storage),
        ("protocols", initial.protocols != next.protocols),
        ("auth", initial.auth != next.auth),
        ("audit", initial.audit != next.audit),
        ("outbound", initial.outbound != next.outbound),
        ("bus", initial.bus != next.bus),
        ("limits", initial.limits != next.limits),
        ("pii", initial.pii != next.pii),
        ("import", initial.import != next.import),
    ];
    sections
        .into_iter()
        .filter_map(|(name, changed)| changed.then_some(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{FileConfig, console_placeholder_warning, restart_only_changes, vault_http};

    #[test]
    fn the_vault_client_gets_the_outbound_proxy_and_roots() {
        let http = mb_llm::HttpOptions {
            extra_roots_pem: b"pem".to_vec(),
            proxy: Some("http://proxy.internal:3128".into()),
            no_proxy: vec![".corp".into()],
        };
        assert_eq!(
            vault_http(&http),
            mb_ext::VaultHttp::new()
                .with_proxy("http://proxy.internal:3128")
                .with_no_proxy(vec![".corp".into()])
                .with_extra_roots_pem(b"pem".to_vec())
        );
        assert_eq!(
            vault_http(&mb_llm::HttpOptions::default()),
            mb_ext::VaultHttp::new()
        );
    }

    #[test]
    fn a_placeholder_console_is_warned_about_with_the_build_command() {
        let warning = console_placeholder_warning(true).expect("a warning");
        assert!(
            warning.contains("npm --prefix crates/mb-console/web run build"),
            "{warning}"
        );
        assert_eq!(console_placeholder_warning(false), None);
    }

    /// MOCKINGBIRD-STUB-9: every read-at-startup section a reload changes
    /// is named; live sections (mocks, LLM profiles, the request log) are
    /// not.
    #[test]
    fn reload_names_the_sections_that_need_a_restart() {
        let initial = FileConfig::default();
        let mut next = initial.clone();
        assert!(restart_only_changes(&initial, &next).is_empty());
        next.observability.request_log_retention += 1;
        next.llm.default_profile = Some("other".into());
        assert!(restart_only_changes(&initial, &next).is_empty());
        next.server.port = next.server.port.wrapping_add(1);
        next.limits.max_request_bytes += 1;
        next.auth.public_metrics = !next.auth.public_metrics;
        assert_eq!(
            restart_only_changes(&initial, &next),
            ["server", "auth", "limits"]
        );
    }
}
