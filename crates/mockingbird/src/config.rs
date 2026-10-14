//! Command-line flags and LLM environment configuration.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::Router;
use clap::{Parser, Subcommand};
use mb_engine::{Engine, EngineConfig};
use mb_llm::{OpenAiCompatible, OpenAiConfig, Provider};
use mb_protocols::MockRuntime;
use mb_protocols::http_rest;
use mb_state::{MemoryStore, StateStore};

/// Health check path; reserved so it cannot collide with contract paths.
pub const HEALTH_PATH: &str = "/__mockingbird/healthz";

/// Default session time-to-live.
const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Parser, Debug)]
#[command(name = "mockingbird", version, about = "LLM-backed mock endpoints")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Serve one or more contracts as live mocks.
    Serve(ServeArgs),
    /// Compile a contract and print an operation summary.
    Validate { contract: PathBuf },
}

#[derive(clap::Args, Debug, Clone)]
pub struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1")]
    pub host: IpAddr,
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
    /// Allow binding a non-loopback address (P1 has no auth).
    #[arg(long)]
    pub insecure_no_auth: bool,
    /// Faker only, no LLM.
    #[arg(long)]
    pub offline: bool,
    #[arg(long, default_value = "3s")]
    pub llm_timeout: humantime::Duration,
    #[arg(required = true)]
    pub contracts: Vec<PathBuf>,
}

impl ServeArgs {
    /// Checks the bind address against the auth rules.
    pub fn check_bind(&self) -> Result<(), String> {
        if !self.host.is_loopback() && !self.insecure_no_auth {
            return Err(format!(
                "refusing to bind {} without auth; pass --insecure-no-auth",
                self.host
            ));
        }
        Ok(())
    }
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

/// Builds the router for all contracts. `llm: None` (or `--offline`) means faker only.
///
/// Must be called inside a tokio runtime.
pub fn build_app(args: &ServeArgs, llm: Option<LlmEnv>) -> anyhow::Result<Router> {
    let provider: Option<Arc<dyn Provider>> = match (args.offline, llm) {
        (false, Some(env)) => Some(Arc::new(OpenAiCompatible::new(OpenAiConfig {
            base_url: env.base_url,
            api_key: env.api_key,
            model: env.model,
            json_schema: env.json_schema,
        })?)),
        _ => None,
    };
    let engine = Arc::new(Engine::new(
        provider,
        EngineConfig {
            timeout: args.llm_timeout.into(),
            ..EngineConfig::default()
        },
    ));
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(SESSION_TTL));

    let multi = args.contracts.len() > 1;
    let mut seen = std::collections::HashSet::new();
    let mut app = Router::new().route(
        HEALTH_PATH,
        axum::routing::get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
    );
    for path in &args.contracts {
        let name = mock_name(path)?;
        if !seen.insert(name.clone()) {
            bail!("duplicate mock name {name:?}; contract file stems must be unique");
        }
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let bundle = mb_import::compile(&bytes)
            .with_context(|| format!("cannot compile {}", path.display()))?;
        let prefix = if multi {
            format!("/{name}")
        } else {
            String::new()
        };
        tracing::info!(
            "serving {name} ({} operations) at http://{}:{}{}",
            bundle.operations.len(),
            args.host,
            args.port,
            if prefix.is_empty() { "/" } else { &prefix }
        );
        let rt = Arc::new(MockRuntime::new(
            name,
            Arc::new(bundle),
            engine.clone(),
            state.clone(),
        ));
        let router = http_rest::try_router(rt)
            .with_context(|| format!("cannot route {}", path.display()))?;
        app = if multi {
            app.nest(&prefix, router)
        } else {
            app.merge(router)
        };
    }
    Ok(app)
}
