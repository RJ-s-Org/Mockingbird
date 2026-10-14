//! `mockingbird.yaml`: server, storage, LLM profiles, observability and
//! mocks declared as code.
//!
//! Precedence: the file < `MB_*` environment variables ([`apply_env`]) <
//! command-line flags ([`crate::config::apply_flags`]).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow};
use serde::Deserialize;
use serde_json::Value;

/// Name of the profile the `MB_LLM_*` variables configure.
pub const ENV_PROFILE: &str = "default";

/// Secret reference the `MB_LLM_API_KEY` variable is stored as.
pub const ENV_API_KEY_REF: &str = "env:MB_LLM_API_KEY";

/// Default `MB_LLM_BASE_URL`.
pub const DEFAULT_LLM_BASE_URL: &str = "https://api.deepseek.com";

/// Default `MB_LLM_MODEL`.
pub const DEFAULT_LLM_MODEL: &str = "deepseek-chat";

#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct FileConfig {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub llm: LlmConfig,
    pub observability: ObservabilityConfig,
    pub mocks: BTreeMap<String, FileMock>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub host: IpAddr,
    /// Data plane.
    pub port: u16,
    /// Admin API and ops endpoints.
    pub admin_port: u16,
    pub insecure_no_auth: bool,
    /// Globs (`*` = any run of characters) an LLM profile's secret
    /// reference must match when it is created or updated through the
    /// Admin API. Profiles in this file are not restricted. Read at
    /// startup.
    pub api_secret_refs: Vec<String>,
    /// Whether `admin_port` was set explicitly (config file key,
    /// `MB_ADMIN_PORT` or `--admin-port`). When it was not, a busy default
    /// port is skipped with a warning instead of failing startup.
    #[serde(skip)]
    pub admin_port_explicit: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8080,
            admin_port: 9090,
            insecure_no_auth: false,
            api_secret_refs: mb_admin::DEFAULT_API_SECRET_REFS
                .iter()
                .map(|r| r.to_string())
                .collect(),
            admin_port_explicit: false,
        }
    }
}

impl ServerConfig {
    /// Checks the bind address against the auth rules (both ports share it).
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

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    /// Control-plane store: `sqlite::memory:`, `sqlite://<path>` or `postgres://…`.
    pub url: String,
    /// Session state store; `None` means the same as `url`, `memory` is in-process.
    pub state_url: Option<String>,
    #[serde(with = "humantime_serde")]
    pub session_ttl: Duration,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            url: "sqlite::memory:".into(),
            state_url: None,
            session_ttl: Duration::from_secs(24 * 60 * 60),
        }
    }
}

impl StorageConfig {
    /// The session state store URL.
    pub fn effective_state_url(&self) -> &str {
        self.state_url.as_deref().unwrap_or(&self.url)
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LlmConfig {
    /// Profile for mocks that name none.
    pub default_profile: Option<String>,
    pub profiles: BTreeMap<String, FileProfile>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileProfile {
    pub base_url: String,
    pub model: String,
    /// A secret reference (`env:NAME` or `file:/path`), never a raw key.
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub json_schema: bool,
    #[serde(with = "humantime_serde", default = "FileProfile::default_timeout")]
    pub timeout: Duration,
    #[serde(default = "FileProfile::default_temperature")]
    pub temperature: f32,
    #[serde(default = "FileProfile::default_max_tokens")]
    pub max_tokens: u32,
    /// Profile tried when this one's provider fails.
    #[serde(default)]
    pub fallback: Option<String>,
}

impl FileProfile {
    fn default_timeout() -> Duration {
        Duration::from_secs(3)
    }

    fn default_temperature() -> f32 {
        0.7
    }

    fn default_max_tokens() -> u32 {
        2048
    }

    /// A profile with the given endpoint and every other field defaulted.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
            json_schema: false,
            timeout: Self::default_timeout(),
            temperature: Self::default_temperature(),
            max_tokens: Self::default_max_tokens(),
            fallback: None,
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ObservabilityConfig {
    /// Request-log entries kept per mock.
    pub request_log_retention: u32,
    /// JSON keys (any depth, case-insensitive) whose values are redacted.
    pub redact_fields: Vec<String>,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            request_log_retention: 1000,
            redact_fields: [
                "password",
                "token",
                "secret",
                "authorization",
                "apiKey",
                "api_key",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileMock {
    /// Relative paths are resolved against the config file's directory by [`load`].
    pub contract: PathBuf,
    /// Defaults to `/<name>`.
    #[serde(default)]
    pub route_prefix: Option<String>,
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub llm_profile: Option<String>,
    #[serde(default = "default_true")]
    pub header_overrides: bool,
    #[serde(default)]
    pub scenarios: BTreeMap<String, FileScenario>,
}

impl FileMock {
    /// A mock serving `contract` with every other field defaulted.
    pub fn new(contract: PathBuf) -> Self {
        Self {
            contract,
            route_prefix: None,
            persona: None,
            llm_profile: None,
            header_overrides: true,
            scenarios: BTreeMap::new(),
        }
    }

    /// The route prefix, defaulting to `/<name>`.
    pub fn effective_prefix(&self, name: &str) -> String {
        self.route_prefix
            .clone()
            .unwrap_or_else(|| format!("/{name}"))
    }
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct FileScenario {
    pub instructions: Option<String>,
    pub status: Option<u16>,
    #[serde(with = "humantime_serde")]
    pub latency: Option<Duration>,
    /// A JSON object of fields forced into the response body.
    pub set: Option<Value>,
    /// Operation ids the scenario applies to; empty means all.
    pub operations: Vec<String>,
}

/// Reads and parses `path`, resolving contract paths against its directory.
pub fn load(path: &Path) -> anyhow::Result<FileConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config file {}", path.display()))?;
    // An empty document is an all-defaults config.
    let mut cfg: FileConfig = if text.trim().is_empty() {
        FileConfig::default()
    } else {
        serde_saphyr::from_str(&text)
            .map_err(|e| anyhow!("{e}"))
            .with_context(|| format!("cannot parse config file {}", path.display()))?
    };
    cfg.server.admin_port_explicit = admin_port_in(&text);
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir
    };
    let dir = std::path::absolute(&dir)
        .with_context(|| format!("cannot resolve directory {}", dir.display()))?;
    for mock in cfg.mocks.values_mut() {
        if mock.contract.is_relative() {
            mock.contract = dir.join(&mock.contract);
        }
    }
    Ok(cfg)
}

/// Whether a config document sets `server.admin_port`.
fn admin_port_in(text: &str) -> bool {
    #[derive(Deserialize, Default)]
    struct Probe {
        #[serde(default)]
        server: Option<ProbeServer>,
    }
    #[derive(Deserialize, Default)]
    struct ProbeServer {
        #[serde(default)]
        admin_port: Option<serde::de::IgnoredAny>,
    }
    serde_saphyr::from_str::<Probe>(text)
        .ok()
        .and_then(|p| p.server)
        .is_some_and(|s| s.admin_port.is_some())
}

/// Applies the `MB_*` environment variables (see [`apply_env_from`]).
pub fn apply_env(cfg: &mut FileConfig) -> anyhow::Result<()> {
    apply_env_from(cfg, |k| std::env::var(k).ok())
}

/// Applies `MB_HOST`, `MB_PORT`, `MB_ADMIN_PORT` and `MB_STORAGE_URL`, and
/// the `MB_LLM_*` variables to the [`ENV_PROFILE`] profile (created with
/// the documented defaults if absent, and made the default profile when
/// none is set). `MB_LLM_API_KEY` is stored as the reference
/// [`ENV_API_KEY_REF`], never as its value. Empty values count as unset.
pub fn apply_env_from(
    cfg: &mut FileConfig,
    lookup: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<()> {
    let get = |k: &str| lookup(k).filter(|v| !v.is_empty());
    if let Some(v) = get("MB_HOST") {
        cfg.server.host = v.parse().with_context(|| format!("MB_HOST={v:?}"))?;
    }
    if let Some(v) = get("MB_PORT") {
        cfg.server.port = v.parse().with_context(|| format!("MB_PORT={v:?}"))?;
    }
    if let Some(v) = get("MB_ADMIN_PORT") {
        cfg.server.admin_port = v.parse().with_context(|| format!("MB_ADMIN_PORT={v:?}"))?;
        cfg.server.admin_port_explicit = true;
    }
    if let Some(v) = get("MB_STORAGE_URL") {
        cfg.storage.url = v;
    }

    let base_url = get("MB_LLM_BASE_URL");
    let model = get("MB_LLM_MODEL");
    let api_key = get("MB_LLM_API_KEY");
    let json_schema = get("MB_LLM_JSON_SCHEMA");
    if base_url.is_none() && model.is_none() && api_key.is_none() && json_schema.is_none() {
        return Ok(());
    }
    let profile = cfg
        .llm
        .profiles
        .entry(ENV_PROFILE.to_string())
        .or_insert_with(|| FileProfile::new(DEFAULT_LLM_BASE_URL, DEFAULT_LLM_MODEL));
    if let Some(v) = base_url {
        profile.base_url = v;
    }
    if let Some(v) = model {
        profile.model = v;
    }
    if api_key.is_some() {
        profile.api_key = Some(ENV_API_KEY_REF.to_string());
    }
    if let Some(v) = json_schema {
        profile.json_schema = v.eq_ignore_ascii_case("true");
    }
    if cfg.llm.default_profile.is_none() {
        cfg.llm.default_profile = Some(ENV_PROFILE.to_string());
    }
    Ok(())
}
