//! Command-line flags and LLM environment configuration.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use axum::Router;
use clap::{Parser, Subcommand};
use mb_ext::{ExtError, Extensions, SecretStore};
use mb_store::ControlStore;

use crate::client::ClientCommand;
use crate::file_config::{self, ENV_API_KEY_REF, ENV_PROFILE, FileConfig, FileMock, FileProfile};
use crate::ops::{self, OpsState};
use crate::recorder::StoreRecorder;
use crate::registry::MockRegistry;
use crate::runtime::{DefaultProfile, RuntimeDeps};
use crate::watch::{self, WatchGuard};
use crate::{reconcile, telemetry};

/// Health check path; reserved so it cannot collide with contract paths.
pub const HEALTH_PATH: &str = "/__mockingbird/healthz";

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
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Serve one or more contracts as live mocks.
    Serve(ServeArgs),
    /// Compile a contract and print an operation summary.
    Validate { contract: PathBuf },
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
    /// Control-plane storage: sqlite::memory:, sqlite://<path> or postgres://….
    #[arg(long)]
    pub storage_url: Option<String>,
    /// Allow binding a non-loopback address (there is no auth yet).
    #[arg(long)]
    pub insecure_no_auth: bool,
    /// Faker only, no LLM.
    #[arg(long)]
    pub offline: bool,
    /// LLM timeout of the default profile [default: 3s].
    #[arg(long)]
    pub llm_timeout: Option<humantime::Duration>,
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
    pub registry: Arc<MockRegistry>,
    /// The effective configuration (file < env < flags).
    pub config: FileConfig,
    /// Present with `--watch`; dropping it stops watching.
    pub watch: Option<WatchGuard>,
}

/// The zero-config [`FileConfig`] for `serve a.yaml [b.yaml]`: SQLite in
/// memory; one mock per contract named after its file stem (lowercased,
/// other invalid characters replaced by `-`), at `/` for a single contract
/// or `/<stem>` for several; and, unless `--offline`, a default profile
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
            format!("/{stem}")
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
    }
    if args.insecure_no_auth {
        cfg.server.insecure_no_auth = true;
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

/// [`build_app_with`] with the self-hosted [`Extensions`].
pub async fn build_app(args: &ServeArgs, llm: Option<LlmEnv>) -> anyhow::Result<App> {
    build_app_with(args, llm, Extensions::default()).await
}

/// Opens the stores, reconciles the configuration into the control store,
/// mounts the mocks and builds both routers; with `--watch`, starts
/// watching the config file. `llm: None` (or `--offline`) means faker only
/// in zero-config mode; `llm.api_key`, when set, is what the reference
/// `env:MB_LLM_API_KEY` resolves to.
///
/// Must be called inside a Tokio runtime.
pub async fn build_app_with(
    args: &ServeArgs,
    llm: Option<LlmEnv>,
    mut ext: Extensions,
) -> anyhow::Result<App> {
    let cfg = resolve_config(args, llm.as_ref())?;
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

    let report = reconcile::reconcile(store.as_ref(), &cfg)
        .await
        .context("applying the configuration")?;
    tracing::debug!(?report, "configuration applied");

    let recorder = StoreRecorder::new(
        store.clone(),
        cfg.observability.request_log_retention,
        cfg.observability.redact_fields.clone(),
    );
    let deps = RuntimeDeps {
        store: store.clone(),
        state: state.clone(),
        ext: ext.clone(),
        recorder: Some(recorder.clone()),
        offline: args.offline,
        default_profile: DefaultProfile::new(cfg.llm.default_profile.clone()),
    };
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await?;
    registry.spawn_watcher(deps.clone());
    for (name, prefix) in registry.mounted() {
        tracing::info!(
            "serving {name} at http://{}:{}{prefix}",
            cfg.server.host,
            cfg.server.port
        );
    }

    let data = Router::new()
        .route(
            HEALTH_PATH,
            axum::routing::get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        )
        .merge(registry.router());

    let prometheus = telemetry::install_prometheus()?;
    let mut admin = mb_admin::router(mb_admin::AdminState {
        store: store.clone(),
        state: state.clone(),
    })
    .merge(ops::router(OpsState {
        store: store.clone(),
        state,
        registry: registry.clone(),
        prometheus,
    }));
    if let Some(routes) = ext.admin_hooks.routes() {
        admin = admin.merge(routes);
    }

    let watch = match (&args.config, args.watch) {
        (Some(path), true) => {
            let flags = args.clone();
            let prepare: watch::Prepare = Arc::new(move |cfg: &mut FileConfig| {
                file_config::apply_env(cfg)?;
                apply_flags(cfg, &flags);
                Ok(())
            });
            let applied = applied_hook(registry.clone(), deps, recorder);
            Some(watch::spawn_with_hook(
                path.clone(),
                store.clone(),
                prepare,
                Some(applied),
            )?)
        }
        _ => None,
    };

    Ok(App {
        data,
        admin,
        registry,
        config: cfg,
        watch,
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
