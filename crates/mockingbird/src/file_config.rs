//! `mockingbird.yaml`: server, storage, authentication, audit, LLM
//! profiles, observability, protocol settings, message brokers and mocks
//! (with their channels and broker) declared as code.
//!
//! Precedence: the file < `MB_*` environment variables ([`apply_env`]) <
//! command-line flags ([`crate::config::apply_flags`]).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use mb_engine::StreamConfig;
use mb_engine::pii::{BuiltinPattern, CustomPattern, Scrubber, ScrubberConfig};
use mb_mcm::{StateChangeKind, Trigger};
use mb_protocols::ProtocolSettings;
use mb_protocols::events::{BrokerConfig, BrokerKind, MQTT_TLS_UNSUPPORTED, SaslConfig};
use mb_store::{
    ChaosSettings, Consistency, DEFAULT_WORKSPACE, DataPlaneAuthConfig, DataPlaneAuthMode,
    LatencyJitter, MockLimits, RateLimit, UnknownIds, WebhookSigning, WebhookTarget,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Name of the profile the `MB_LLM_*` variables configure.
pub const ENV_PROFILE: &str = "default";

/// Secret reference the `MB_LLM_API_KEY` variable is stored as.
pub const ENV_API_KEY_REF: &str = "env:MB_LLM_API_KEY";

/// Default `MB_LLM_BASE_URL`.
pub const DEFAULT_LLM_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Default `MB_LLM_MODEL`.
pub const DEFAULT_LLM_MODEL: &str = "deepseek/deepseek-v4.1-flash";

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct FileConfig {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub llm: LlmConfig,
    pub observability: ObservabilityConfig,
    /// Protocol knobs shared by every mock. Read at startup.
    pub protocols: ProtocolsConfig,
    /// Message brokers the mocks' channels use, by name. Read at startup.
    pub brokers: BTreeMap<String, FileBroker>,
    pub mocks: BTreeMap<String, FileMock>,
    pub auth: AuthConfig,
    pub audit: AuditConfig,
    /// Outbound HTTP (LLM providers, webhooks, OIDC): extra CA and proxy.
    pub outbound: OutboundConfig,
    /// The cross-node change bus.
    pub bus: BusConfig,
    /// Global admission limits.
    pub limits: LimitsConfig,
    /// The output PII scrubber.
    pub pii: PiiConfig,
    /// Smart imports (documentation, HAR, Postman). Read at startup.
    pub import: ImportConfig,
    /// The intercepting HTTP proxy `serve` may start. Read at startup.
    pub proxy: ProxyConfig,
}

/// The `proxy:` section.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ProxyConfig {
    /// Whether `serve` starts the proxy (`--proxy`).
    pub enabled: bool,
    /// The proxy's bind address. Loopback by default: off loopback,
    /// `unmapped` must be `block`, or the port would be an open relay.
    pub host: IpAddr,
    /// The proxy's port (`--proxy-port`).
    pub port: u16,
    /// What happens to hosts no mock claims.
    pub unmapped: Unmapped,
    /// Hosts answered with generated placeholder images.
    pub images: Vec<String>,
    /// Where the local CA lives; unset uses `MB_CA_DIR` or
    /// `~/.mockingbird/ca`. Relative paths are resolved against the config
    /// file's directory by [`load`].
    pub ca_dir: Option<PathBuf>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: crate::proxy::DEFAULT_PROXY_PORT,
            unmapped: Unmapped::Pass,
            images: Vec::new(),
            ca_dir: None,
        }
    }
}

impl ProxyConfig {
    /// Refuses `unmapped: pass` on a non-loopback `host`: anyone who can
    /// reach the port could tunnel to any address through it.
    pub fn check_bind(&self, host: IpAddr) -> Result<(), String> {
        if host.is_loopback() || self.unmapped == Unmapped::Block {
            Ok(())
        } else {
            Err(format!(
                "proxy.unmapped: pass on the non-loopback proxy address {host} would let anyone who reaches it tunnel to any host; set proxy.unmapped: block, or bind proxy.host to a loopback address"
            ))
        }
    }

    /// `ca_dir`, else [`default_ca_dir`](crate::proxy::ca::default_ca_dir).
    pub fn effective_ca_dir(&self) -> anyhow::Result<PathBuf> {
        match &self.ca_dir {
            Some(dir) => Ok(dir.clone()),
            None => crate::proxy::ca::default_ca_dir(),
        }
    }
}

/// What the proxy does with a host no mock claims (`proxy.unmapped`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Unmapped {
    /// Tunnel `CONNECT` and forward plain requests to the real host.
    #[default]
    Pass,
    /// Answer 502 problem+json naming the host.
    Block,
}

impl Unmapped {
    pub fn as_str(self) -> &'static str {
        match self {
            Unmapped::Pass => "pass",
            Unmapped::Block => "block",
        }
    }
}

/// Which planes a node serves (`server.role`, `MB_ROLE`, `--role`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Control and data plane in one process.
    #[default]
    All,
    /// Admin API, console, reconcile and maintenance only.
    Control,
    /// Mocks only.
    Data,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::All => "all",
            Role::Control => "control",
            Role::Data => "data",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "all" => Ok(Role::All),
            "control" => Ok(Role::Control),
            "data" => Ok(Role::Data),
            other => Err(format!(
                "unknown role {other:?}; expected all, control or data"
            )),
        }
    }
}

/// `server.tls`: Mockingbird terminates TLS itself. Both files or none.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TlsConfig {
    /// PEM certificate chain.
    pub cert_file: PathBuf,
    /// PEM private key.
    pub key_file: PathBuf,
}

/// The `outbound:` section.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct OutboundConfig {
    /// PEM bundle trusted in addition to the default roots.
    pub ca_file: Option<PathBuf>,
    /// `http(s)://` proxy for outbound requests.
    pub proxy: Option<String>,
    /// Hosts (or `.domain` suffixes) reached without the proxy.
    pub no_proxy: Vec<String>,
}

/// The `bus:` section.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct BusConfig {
    /// `redis://`, `rediss://`, `postgres://` or `local`; unset follows
    /// `storage.url` (see [`FileConfig::bus_target`]).
    pub url: Option<String>,
}

/// Where cross-node change notifications travel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusTarget {
    /// In-process only (a single node).
    Local,
    /// Postgres `LISTEN/NOTIFY` at this URL.
    Postgres(String),
    /// Redis pub/sub at this URL.
    Redis(String),
}

/// The `limits:` section: global admission limits.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    /// LLM generations running at once, across every mock.
    pub max_concurrent_generations: usize,
    /// Largest request body accepted, in bytes.
    pub max_request_bytes: u64,
    /// Largest response body generated, in bytes.
    pub max_response_bytes: u64,
    /// Completion tokens the LLM is asked to write per answer: a larger
    /// response is generated shorter and completed by the faker (0: off).
    pub output_budget_tokens: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_concurrent_generations: 64,
            max_request_bytes: 1_048_576,
            max_response_bytes: 1_048_576,
            output_budget_tokens: mb_engine::budget::DEFAULT_OUTPUT_BUDGET,
        }
    }
}

/// The `import:` section.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ImportConfig {
    /// Import worker tasks, 1..=16.
    pub workers: usize,
    /// Tokens one import job's LLM calls may spend, at least 1000.
    pub max_tokens: u64,
    /// Profile used for extraction instead of the mock's own (looked up in
    /// the mock's workspace, then `default`).
    pub llm_profile: Option<String>,
    /// Hosts (`docs.internal`, `.corp.example`) and CIDRs URL imports may
    /// reach although they are private.
    pub url_allowlist: Vec<String>,
    /// Most queued plus running jobs per mock, at least 1.
    pub max_active_jobs_per_mock: u32,
}

impl Default for ImportConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            max_tokens: mb_import::smart::DEFAULT_IMPORT_TOKENS,
            llm_profile: None,
            url_allowlist: Vec::new(),
            max_active_jobs_per_mock: mb_admin::DEFAULT_MAX_ACTIVE_JOBS_PER_MOCK,
        }
    }
}

/// The `pii:` section: the output PII scrubber.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PiiConfig {
    pub enabled: bool,
    /// Builtin patterns to replace.
    pub patterns: Vec<BuiltinPattern>,
    /// Extra patterns `{name, regex, replacement}`.
    #[serde(
        deserialize_with = "strict_custom_patterns",
        serialize_with = "serialize_custom_patterns"
    )]
    pub custom: Vec<CustomPattern>,
}

impl Default for PiiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            patterns: BuiltinPattern::ALL.to_vec(),
            custom: Vec::new(),
        }
    }
}

/// [`CustomPattern`]s, rejecting unknown keys like the rest of the file.
fn strict_custom_patterns<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<CustomPattern>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Strict {
        name: String,
        regex: String,
        replacement: String,
    }
    Ok(Vec::<Strict>::deserialize(d)?
        .into_iter()
        .map(|s| CustomPattern {
            name: s.name,
            regex: s.regex,
            replacement: s.replacement,
        })
        .collect())
}

/// [`CustomPattern`]s as `{name, regex, replacement}` maps.
fn serialize_custom_patterns<S: serde::Serializer>(
    patterns: &[CustomPattern],
    s: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Out<'a> {
        name: &'a str,
        regex: &'a str,
        replacement: &'a str,
    }
    s.collect_seq(patterns.iter().map(|p| Out {
        name: &p.name,
        regex: &p.regex,
        replacement: &p.replacement,
    }))
}

/// Accepted `import.workers`.
pub const IMPORT_WORKERS: std::ops::RangeInclusive<usize> = 1..=16;

/// Accepted `limits.output_budget_tokens` other than 0 (off).
const MIN_OUTPUT_BUDGET: u32 = 100;
const MAX_OUTPUT_BUDGET: u32 = 100_000;

/// Smallest accepted `import.max_tokens`.
pub const MIN_IMPORT_TOKENS: u64 = 1000;

impl ImportConfig {
    /// The worker pool's settings; `Err("import.<field>: <reason>")` when a
    /// value is out of range or an allowlist entry does not parse.
    pub fn settings(&self) -> Result<mb_admin::ImportSettings, String> {
        if !IMPORT_WORKERS.contains(&self.workers) {
            return Err(format!(
                "import.workers: {} must be between {} and {}",
                self.workers,
                IMPORT_WORKERS.start(),
                IMPORT_WORKERS.end()
            ));
        }
        if self.max_tokens < MIN_IMPORT_TOKENS {
            return Err(format!(
                "import.max_tokens: {} must be at least {MIN_IMPORT_TOKENS}",
                self.max_tokens
            ));
        }
        if self.max_active_jobs_per_mock == 0 {
            return Err("import.max_active_jobs_per_mock: 0 must be at least 1".to_string());
        }
        let fetch = mb_import::smart::fetch::FetchPolicy::from_allowlist(&self.url_allowlist)
            .map_err(|e| format!("import.url_allowlist: {e}"))?;
        Ok(mb_admin::ImportSettings {
            workers: self.workers,
            max_tokens: self.max_tokens,
            fetch,
            max_active_jobs_per_mock: self.max_active_jobs_per_mock,
            ..mb_admin::ImportSettings::default()
        })
    }
}

/// How the Admin API authenticates callers.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Every admin request is the local sysadmin.
    #[default]
    None,
    /// OIDC console login, bearer JWTs and API keys.
    Oidc,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    pub mode: AuthMode,
    /// The URL browsers reach the admin port at: the OIDC redirect URL is
    /// `<external_url>/auth/callback`, an `https` URL makes cookies
    /// `Secure`, and in `oidc` mode the admin port answers only to its host.
    pub external_url: String,
    /// Absolute lifetime of a console login session.
    #[serde(with = "humantime_serde")]
    pub session_ttl: Duration,
    /// OIDC `sub` values that are always sysadmin.
    pub sysadmin_subjects: Vec<String>,
    /// Whether `/metrics` is served without credentials.
    pub public_metrics: bool,
    /// Required in `oidc` mode.
    pub oidc: Option<OidcConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            mode: AuthMode::None,
            external_url: "http://127.0.0.1:9090".into(),
            session_ttl: Duration::from_secs(12 * 60 * 60),
            sysadmin_subjects: Vec::new(),
            public_metrics: true,
            oidc: None,
        }
    }
}

impl AuthConfig {
    /// In `oidc` mode: `auth.oidc` must be present and `external_url` an
    /// http(s) URL with a host. `none` mode has nothing to check.
    pub fn validate(&self) -> Result<(), String> {
        if self.mode == AuthMode::None {
            return Ok(());
        }
        if self.oidc.is_none() {
            return Err("auth.mode oidc requires an auth.oidc section".into());
        }
        self.external_host().map(|_| ())
    }

    /// The `host[:port]` of `external_url`, lowercased (the port only when
    /// it is not the scheme's default). `Err` unless `external_url` is an
    /// http or https URL with a host.
    pub fn external_host(&self) -> Result<String, String> {
        let invalid = || {
            format!(
                "auth.external_url {:?} must be an http:// or https:// URL with a host",
                self.external_url
            )
        };
        let url = reqwest::Url::parse(&self.external_url).map_err(|_| invalid())?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(invalid());
        }
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(invalid)?
            .to_ascii_lowercase();
        Ok(match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        })
    }

    /// `external_url` without a trailing `/`.
    pub fn external_base(&self) -> &str {
        self.external_url.trim_end_matches('/')
    }

    /// Whether `external_url` is https (cookies then carry `Secure`).
    pub fn secure_cookies(&self) -> bool {
        self.external_url
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    /// A secret reference (`env:NAME` or `file:/path`); `None` for a
    /// public client.
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default = "OidcConfig::default_scopes")]
    pub scopes: Vec<String>,
    /// The claim holding `<group_prefix>:<workspace>:<role>` and
    /// `<group_prefix>:sysadmin` strings.
    #[serde(default = "OidcConfig::default_roles_claim")]
    pub roles_claim: String,
    #[serde(default = "OidcConfig::default_group_prefix")]
    pub group_prefix: String,
    /// The `aud` bearer JWTs must carry; defaults to `client_id`.
    #[serde(default)]
    pub audience: Option<String>,
}

impl OidcConfig {
    fn default_scopes() -> Vec<String> {
        ["openid", "profile", "email", "groups"]
            .map(String::from)
            .to_vec()
    }

    fn default_roles_claim() -> String {
        "groups".into()
    }

    fn default_group_prefix() -> String {
        "mb".into()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AuditConfig {
    /// Audit events older than this are pruned daily.
    pub retention_days: u32,
    /// Every audit event is POSTed here when set.
    pub webhook_url: Option<String>,
    /// A secret reference for the webhook's HMAC signature.
    pub webhook_secret: Option<String>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            retention_days: 365,
            webhook_url: None,
            webhook_secret: None,
        }
    }
}

fn default_ws() -> String {
    DEFAULT_WORKSPACE.to_string()
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub host: IpAddr,
    /// Data plane.
    pub port: u16,
    /// Admin API and ops endpoints.
    pub admin_port: u16,
    /// gRPC (h2c), shared by every gRPC mock.
    pub grpc_port: u16,
    pub insecure_no_auth: bool,
    /// Globs (`*` = any run of characters) an LLM profile's secret
    /// reference must match when it is created or updated through the
    /// Admin API. Profiles in this file are not restricted. Read at
    /// startup.
    pub api_secret_refs: Vec<String>,
    /// Globs a mock's webhook `secret` reference must match when set
    /// through the Admin API. Mocks in this file are not restricted. Read
    /// at startup.
    pub api_webhook_secret_refs: Vec<String>,
    /// Which planes this node serves.
    pub role: Role,
    /// Whether `X-Forwarded-*` request headers are trusted (only behind a
    /// reverse proxy that sets them).
    pub trust_forwarded_headers: bool,
    /// TLS termination; absent serves plain HTTP.
    pub tls: Option<TlsConfig>,
    /// Whether `admin_port` was set explicitly (config file key,
    /// `MB_ADMIN_PORT` or `--admin-port`). When it was not, a busy default
    /// port is skipped with a warning instead of failing startup.
    #[serde(skip)]
    pub admin_port_explicit: bool,
    /// Whether `grpc_port` was set explicitly (config file key,
    /// `MB_GRPC_PORT` or `--grpc-port`). When it was not, a busy default
    /// port is skipped with a warning instead of failing startup.
    #[serde(skip)]
    pub grpc_port_explicit: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8080,
            admin_port: 9090,
            grpc_port: 50051,
            insecure_no_auth: false,
            api_secret_refs: mb_admin::DEFAULT_API_SECRET_REFS
                .iter()
                .map(|r| r.to_string())
                .collect(),
            api_webhook_secret_refs: mb_admin::DEFAULT_API_WEBHOOK_SECRET_REFS
                .iter()
                .map(|r| r.to_string())
                .collect(),
            role: Role::All,
            trust_forwarded_headers: false,
            tls: None,
            admin_port_explicit: false,
            grpc_port_explicit: false,
        }
    }
}

impl ServerConfig {
    /// Checks the bind address against the auth rules (every port
    /// shares it): in `none` mode a non-loopback address needs
    /// `--insecure-no-auth`; `oidc` mode accepts any address once its
    /// settings are valid ([`AuthConfig::validate`]).
    pub fn check_bind(&self, auth: &AuthConfig) -> Result<(), String> {
        if auth.mode == AuthMode::Oidc {
            return auth.validate();
        }
        if !self.host.is_loopback() && !self.insecure_no_auth {
            return Err(format!(
                "refusing to bind {} without auth; pass --insecure-no-auth",
                self.host
            ));
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LlmConfig {
    /// Profile for mocks that name none.
    pub default_profile: Option<String>,
    pub profiles: BTreeMap<String, FileProfile>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileProfile {
    /// The workspace the profile belongs to (created if missing).
    #[serde(default = "default_ws")]
    pub workspace: String,
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
    /// Extra request headers sent to the provider. Values may embed secret
    /// references as `${env:NAME}` or `${file:/path}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// JSON merged into every provider request body (OpenRouter `provider`
    /// routing, `reasoning` settings). Not for secrets.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra_body: serde_json::Map<String, serde_json::Value>,
}

impl FileProfile {
    fn default_timeout() -> Duration {
        Duration::from_secs(60)
    }

    fn default_temperature() -> f32 {
        0.7
    }

    fn default_max_tokens() -> u32 {
        16_384
    }

    /// A profile with the given endpoint and every other field defaulted.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            workspace: default_ws(),
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
            json_schema: false,
            timeout: Self::default_timeout(),
            temperature: Self::default_temperature(),
            max_tokens: Self::default_max_tokens(),
            fallback: None,
            headers: BTreeMap::new(),
            extra_body: serde_json::Map::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ObservabilityConfig {
    /// Request-log entries kept per mock.
    pub request_log_retention: u32,
    /// Redacted keys: a recorded JSON, form or query key (any depth) is
    /// redacted when, lowercased and without `_`, `-` and `.`, it contains
    /// one of these normalized the same way (`token` matches `access_token`).
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

/// The `protocols:` section: knobs shared by every mock's adapter.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ProtocolsConfig {
    pub graphql: GraphqlConfig,
    pub stream: StreamFileConfig,
    pub sse: SseConfig,
    pub grpc: GrpcConfig,
}

impl ProtocolsConfig {
    /// The adapters' settings and the engine's stream limits.
    pub fn settings(&self) -> (ProtocolSettings, StreamConfig) {
        let settings = ProtocolSettings {
            graphql_path: self.graphql.path.clone(),
            graphql_introspection: self.graphql.introspection,
            graphql_max_depth: self.graphql.max_depth,
            graphql_max_complexity: self.graphql.max_complexity,
            sse_keepalive: self.sse.keepalive,
            grpc_max_message_bytes: self.grpc.max_message_bytes,
            graphql_max_root_fields: self.graphql.max_root_fields,
            graphql_max_subscriptions_per_connection: self.graphql.max_subscriptions_per_connection,
            graphql_max_connections_per_mock: self.graphql.max_connections_per_mock,
            graphql_connection_init_timeout: self.graphql.connection_init_timeout,
            ..ProtocolSettings::default()
        };
        let stream = StreamConfig {
            default_items: self.stream.default_items,
            max_items: self.stream.max_items,
            item_interval: self.stream.item_interval,
        };
        (settings, stream)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct GraphqlConfig {
    /// Endpoint path below each GraphQL mock's route prefix.
    pub path: String,
    /// Whether introspection queries are answered.
    pub introspection: bool,
    pub max_depth: usize,
    pub max_complexity: usize,
    /// Most root fields one operation may select.
    pub max_root_fields: usize,
    /// Most concurrent subscriptions on one WebSocket connection.
    pub max_subscriptions_per_connection: usize,
    /// Most concurrent WebSocket connections to one mock.
    pub max_connections_per_mock: usize,
    /// How long a WebSocket connection may wait before `connection_init`.
    #[serde(with = "humantime_serde")]
    pub connection_init_timeout: Duration,
}

impl Default for GraphqlConfig {
    fn default() -> Self {
        Self {
            path: "/graphql".into(),
            introspection: true,
            max_depth: 15,
            max_complexity: 1000,
            max_root_fields: 20,
            max_subscriptions_per_connection: 16,
            max_connections_per_mock: 1000,
            connection_init_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct StreamFileConfig {
    /// Items per streamed response when the request asks for no count.
    pub default_items: usize,
    /// Upper bound on items per streamed response.
    pub max_items: usize,
    /// Pause before every streamed item except the first.
    #[serde(with = "humantime_serde")]
    pub item_interval: Duration,
}

impl Default for StreamFileConfig {
    fn default() -> Self {
        Self {
            default_items: 5,
            max_items: 50,
            item_interval: Duration::ZERO,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SseConfig {
    /// Interval between keepalive comments.
    #[serde(with = "humantime_serde")]
    pub keepalive: Duration,
}

impl Default for SseConfig {
    fn default() -> Self {
        Self {
            keepalive: Duration::from_secs(15),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct GrpcConfig {
    /// Largest gRPC message decoded or encoded.
    pub max_message_bytes: usize,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            max_message_bytes: 4 * 1024 * 1024,
        }
    }
}

/// One `brokers:` entry: a connection to a message broker.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileBroker {
    /// `kafka`, `amqp`, `mqtt` or `memory`.
    pub kind: String,
    /// AMQP and MQTT: the broker URL, without a password.
    #[serde(default)]
    pub url: Option<String>,
    /// Kafka: comma-separated `host:port` list.
    #[serde(default)]
    pub bootstrap_servers: Option<String>,
    #[serde(default)]
    pub tls: bool,
    /// CA bundle (PEM) trusted for TLS, in addition to the system roots.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    #[serde(default)]
    pub sasl: Option<FileSasl>,
    /// Prefix of the client ids and consumer groups Mockingbird uses: a
    /// mock's Kafka consumer group (and AMQP shared-queue name) is
    /// `<client_id_prefix>-<mock>` (`<client_id_prefix>-<workspace>.<mock>`
    /// outside `default`).
    #[serde(default = "default_client_id_prefix")]
    pub client_id_prefix: String,
}

fn default_client_id_prefix() -> String {
    "mockingbird".into()
}

/// SASL credentials of a broker.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileSasl {
    pub mechanism: String,
    pub username: String,
    /// A secret reference (`env:NAME` or `file:/path`), never a password.
    pub password: String,
}

/// The broker connections of the `brokers:` section, validated: a known
/// `kind` compiled into this build, the address that kind needs, no
/// password in a `url`, a secret reference as the SASL password, and no
/// TLS for MQTT (`mqtts://`/`ssl://` URLs, `tls` or `ca_file`; see
/// [`MQTT_TLS_UNSUPPORTED`]).
pub fn broker_configs(cfg: &FileConfig) -> anyhow::Result<Vec<BrokerConfig>> {
    cfg.brokers
        .iter()
        .map(|(name, broker)| broker_config(name, broker))
        .collect()
}

fn broker_config(name: &str, broker: &FileBroker) -> anyhow::Result<BrokerConfig> {
    let kind = match broker.kind.as_str() {
        "kafka" => BrokerKind::Kafka,
        "amqp" => BrokerKind::Amqp,
        "mqtt" => BrokerKind::Mqtt,
        "memory" => BrokerKind::Memory,
        other => bail!("brokers.{name}.kind: unknown broker kind \"{other}\""),
    };
    let url = match kind {
        BrokerKind::Kafka => broker
            .bootstrap_servers
            .clone()
            .ok_or_else(|| anyhow!("brokers.{name}.bootstrap_servers: required for kafka"))?,
        BrokerKind::Amqp | BrokerKind::Mqtt => broker
            .url
            .clone()
            .ok_or_else(|| anyhow!("brokers.{name}.url: required for {}", kind.as_str()))?,
        BrokerKind::Memory => broker.url.clone().unwrap_or_default(),
    };
    if broker.url.as_deref().is_some_and(has_password) {
        bail!("brokers.{name}.url: put the password in sasl.password as a secret reference");
    }
    let sasl = match &broker.sasl {
        Some(sasl) => {
            if !crate::reconcile::valid_secret_ref(&sasl.password) {
                bail!(
                    "brokers.{name}.sasl.password: must be a secret reference (env:NAME or file:/path), not a password"
                );
            }
            Some(SaslConfig {
                mechanism: sasl.mechanism.clone(),
                username: sasl.username.clone(),
                password_ref: sasl.password.clone(),
            })
        }
        None => None,
    };
    if kind == BrokerKind::Mqtt
        && (broker.tls
            || broker.ca_file.is_some()
            || url
                .get(..url.find("://").unwrap_or(0))
                .is_some_and(|scheme| {
                    ["mqtts", "ssl"]
                        .iter()
                        .any(|tls| scheme.eq_ignore_ascii_case(tls))
                }))
    {
        bail!("brokers.{name}: {MQTT_TLS_UNSUPPORTED}");
    }
    if !kind.compiled() {
        let kind = kind.as_str();
        bail!(
            "brokers.{name}: this build has no {kind} support (use the full image or build with --features {kind})"
        );
    }
    Ok(BrokerConfig {
        name: name.to_string(),
        kind,
        url,
        tls: broker.tls,
        ca_file: broker.ca_file.clone(),
        sasl,
        client_id_prefix: broker.client_id_prefix.clone(),
    })
}

/// Whether `url`'s userinfo (`user:password@`) holds a password.
fn has_password(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'))
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileMock {
    /// The workspace the mock belongs to (created if missing).
    #[serde(default = "default_ws")]
    pub workspace: String,
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
    /// The `brokers:` entry the mock's channels use; absent picks one
    /// automatically.
    #[serde(default)]
    pub broker: Option<String>,
    /// Per-channel settings, by channel id.
    #[serde(default)]
    pub channels: BTreeMap<String, FileChannel>,
    #[serde(default, deserialize_with = "strict_data_plane_auth")]
    pub data_plane_auth: DataPlaneAuthConfig,
    /// Fault injection; absent injects nothing.
    #[serde(default)]
    pub chaos: Option<FileChaos>,
    /// Admission limits; unset fields use the global `limits:`.
    #[serde(default)]
    pub limits: FileMockLimits,
    /// `on` (default): repeated reads in a session get the same answer and
    /// listed items are served by id; `off` generates every answer anew.
    #[serde(default)]
    pub consistency: Consistency,
    /// `generate` (default) or `404`: how a read of an id the session does
    /// not hold is answered.
    #[serde(default)]
    pub unknown_ids: UnknownIds,
    /// A scenario of this mock applied when a call names none.
    #[serde(default)]
    pub default_scenario: Option<String>,
    /// Host names the mock answers for (by `Host`); empty answers for any
    /// host. With hosts, two mocks may share a route prefix.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Rewrites the path below `route_prefix` before it is matched against
    /// the contract (`strip_prefix`, then `add_prefix`).
    #[serde(default)]
    pub path_rewrite: mb_store::PathRewrite,
    /// A GraphQL mock's endpoint below `route_prefix` (Shopify's
    /// `/graphql.json`); default `protocols.graphql.path`.
    #[serde(default)]
    pub graphql_path: Option<String>,
    /// Where the mock POSTs an event after each successful write.
    #[serde(default)]
    pub webhooks: Vec<FileWebhook>,
}

/// One entry of a mock's `webhooks:`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileWebhook {
    /// Absolute `http`/`https` URL the events are POSTed to.
    pub url: String,
    /// Operation ids, event types (`customer.created`) or `"*"`.
    #[serde(default = "WebhookTarget::all_events")]
    pub events: Vec<String>,
    /// A secret reference (`env:NAME`, `file:/path`, `vault:path#key`).
    #[serde(default)]
    pub secret: Option<String>,
    /// `stripe`, `hmac-sha256` or `none`; default `hmac-sha256` with a
    /// secret, `none` without.
    #[serde(default)]
    pub signing: Option<WebhookSigning>,
    /// Wait before the first attempt (at most 60 000).
    #[serde(default)]
    pub delay_ms: u64,
}

impl FileWebhook {
    pub fn to_store(&self) -> WebhookTarget {
        WebhookTarget {
            url: self.url.clone(),
            events: self.events.clone(),
            secret: self.secret.clone(),
            signing: self.signing,
            delay_ms: self.delay_ms,
        }
    }
}

/// A mock's (or scenario's) `chaos:`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct FileChaos {
    /// Probability (0–1) that a call answers with an injected error.
    pub error_rate: f64,
    /// Uniformly random latency added to every call.
    pub latency_jitter: Option<FileJitter>,
    /// Probability (0–1) that a streamed response is cut short.
    pub drop_stream_rate: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct FileJitter {
    #[serde(with = "humantime_serde")]
    pub min: Duration,
    #[serde(with = "humantime_serde")]
    pub max: Duration,
}

impl FileChaos {
    /// The stored form (jitter in milliseconds).
    pub fn to_store(&self) -> ChaosSettings {
        ChaosSettings {
            error_rate: self.error_rate,
            latency_jitter: self.latency_jitter.as_ref().map(|j| LatencyJitter {
                min_ms: millis(j.min),
                max_ms: millis(j.max),
            }),
            drop_stream_rate: self.drop_stream_rate,
        }
    }

    /// Range checks; messages name the key below `prefix` (e.g.
    /// `mocks.orders.`).
    fn validate(&self, prefix: &str) -> Result<(), String> {
        if let Some(j) = &self.latency_jitter {
            if j.min > j.max {
                return Err(format!(
                    "{prefix}chaos.latency_jitter.min ({}) must not exceed {prefix}chaos.latency_jitter.max ({})",
                    humantime::format_duration(j.min),
                    humantime::format_duration(j.max)
                ));
            }
            if j.max > MAX_STREAM_INTERVAL {
                return Err(format!(
                    "{prefix}chaos.latency_jitter.max must be at most 30s"
                ));
            }
        }
        self.to_store()
            .validate()
            .map_err(|e| format!("{prefix}{e}"))
    }
}

/// Milliseconds, saturating.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// A mock's `limits.rate_limit`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct FileRateLimit {
    pub requests_per_second: f64,
    pub burst: u32,
}

/// A mock's `limits:`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct FileMockLimits {
    pub rate_limit: Option<FileRateLimit>,
    /// Tokens per UTC day, across all replicas.
    pub daily_token_budget: Option<u64>,
    /// Overrides `limits.max_request_bytes` for this mock.
    pub max_request_bytes: Option<u64>,
    /// Overrides `limits.max_response_bytes` for this mock.
    pub max_response_bytes: Option<u64>,
}

impl FileMockLimits {
    /// The stored form.
    pub fn to_store(&self) -> MockLimits {
        MockLimits {
            rate_limit: self.rate_limit.as_ref().map(|r| RateLimit {
                requests_per_second: r.requests_per_second,
                burst: r.burst,
            }),
            daily_token_budget: self.daily_token_budget,
            max_request_bytes: self.max_request_bytes,
            max_response_bytes: self.max_response_bytes,
        }
    }
}

/// [`DataPlaneAuthConfig`], rejecting unknown keys like the rest of the file.
fn strict_data_plane_auth<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<DataPlaneAuthConfig, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Strict {
        #[serde(default)]
        mode: DataPlaneAuthMode,
        #[serde(default)]
        header: Option<String>,
    }
    let strict = Strict::deserialize(d)?;
    Ok(DataPlaneAuthConfig {
        mode: strict.mode,
        header: strict
            .header
            .unwrap_or_else(|| DataPlaneAuthConfig::DEFAULT_HEADER.to_string()),
    })
}

impl FileMock {
    /// A mock serving `contract` with every other field defaulted.
    pub fn new(contract: PathBuf) -> Self {
        Self {
            workspace: default_ws(),
            contract,
            route_prefix: None,
            persona: None,
            llm_profile: None,
            header_overrides: true,
            scenarios: BTreeMap::new(),
            broker: None,
            channels: BTreeMap::new(),
            data_plane_auth: DataPlaneAuthConfig::default(),
            chaos: None,
            limits: FileMockLimits::default(),
            consistency: Consistency::default(),
            unknown_ids: UnknownIds::default(),
            default_scenario: None,
            hosts: Vec::new(),
            path_rewrite: mb_store::PathRewrite::default(),
            graphql_path: None,
            webhooks: Vec::new(),
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

/// A `channels:` entry of a mock: overrides of one event channel.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileChannel {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Absent uses the contract's triggers.
    #[serde(default)]
    pub triggers: Option<Vec<FileTrigger>>,
}

/// A channel trigger as written in the config file.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FileTrigger {
    /// `- every: 10s`
    Every(#[serde(with = "humantime_serde")] Duration),
    /// `- cron: "*/5 * * * *"`
    Cron(String),
    /// `- on_state_change: {resource_type: orders, kind: deleted}`
    OnStateChange {
        resource_type: String,
        #[serde(default)]
        kind: StateChangeKind,
    },
    /// `- manual`
    Manual,
}

impl FileTrigger {
    /// The MCM trigger.
    pub fn to_trigger(&self) -> Trigger {
        match self {
            FileTrigger::Every(every) => Trigger::Schedule { every: *every },
            FileTrigger::Cron(expr) => Trigger::Cron { expr: expr.clone() },
            FileTrigger::OnStateChange {
                resource_type,
                kind,
            } => Trigger::OnStateChange {
                resource_type: resource_type.clone(),
                kind: *kind,
            },
            FileTrigger::Manual => Trigger::Manual,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct FileScenario {
    pub instructions: Option<String>,
    pub status: Option<u16>,
    #[serde(with = "humantime_serde")]
    pub latency: Option<Duration>,
    /// A JSON object of fields forced into the response body.
    pub set: Option<Value>,
    /// Operations the scenario applies to (an id, or `{id, instructions}`);
    /// empty means all.
    pub operations: Vec<FileScenarioOperation>,
    /// Items per streamed response unless `X-Mockingbird-Stream-Items`
    /// overrides it.
    pub stream_items: Option<u32>,
    /// Pause between streamed items.
    #[serde(with = "humantime_serde")]
    pub stream_item_interval: Option<Duration>,
    /// Replaces the mock's chaos while the scenario applies.
    pub chaos: Option<FileChaos>,
    /// Response headers to send, by name (`Retry-After: "30"`); they
    /// replace any the contract declares.
    pub headers: BTreeMap<String, String>,
}

/// One `operations:` entry of a scenario: an operation id, or an id with
/// instructions replacing the scenario's for that operation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum FileScenarioOperation {
    Id(String),
    Entry(FileScenarioOperationEntry),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileScenarioOperationEntry {
    pub id: String,
    #[serde(default)]
    pub instructions: Option<String>,
}

impl FileScenarioOperation {
    pub fn id(&self) -> &str {
        match self {
            FileScenarioOperation::Id(id) => id,
            FileScenarioOperation::Entry(e) => &e.id,
        }
    }
}

impl FileScenario {
    /// The ids of `operations`, in order.
    pub fn operation_ids(&self) -> Vec<String> {
        self.operations.iter().map(|o| o.id().to_string()).collect()
    }

    /// Per-operation instructions from `operations`, by id.
    pub fn operation_instructions(&self) -> BTreeMap<String, String> {
        self.operations
            .iter()
            .filter_map(|o| match o {
                FileScenarioOperation::Entry(FileScenarioOperationEntry {
                    id,
                    instructions: Some(text),
                }) => Some((id.clone(), text.clone())),
                _ => None,
            })
            .collect()
    }
}

/// Longest accepted `auth.session_ttl`.
pub const MAX_SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Accepted `audit.retention_days`.
pub const RETENTION_DAYS: std::ops::RangeInclusive<u32> = 1..=3650;

/// Longest accepted stream item interval and latency jitter.
pub const MAX_STREAM_INTERVAL: Duration = Duration::from_secs(30);

/// Accepted `limits.max_concurrent_generations`.
pub const MAX_CONCURRENT_GENERATIONS: std::ops::RangeInclusive<usize> = 1..=4096;

/// Smallest accepted `protocols.grpc.max_message_bytes`.
pub const MIN_GRPC_MESSAGE_BYTES: usize = 1024;

impl FileConfig {
    /// Every range and consistency check that does not depend on the auth
    /// mode, each error naming its key: `auth.session_ttl` ≤ 30 days,
    /// `audit.retention_days` in 1..=3650, the `server.role` storage rule,
    /// `server.tls` (both files or neither), `bus.url`'s scheme,
    /// `outbound.proxy`, the `limits:` and `protocols:` ranges, every mock's
    /// and scenario's `chaos`, `limits` and `stream_item_interval`, the
    /// `pii:` patterns and the `import:` section ([`ImportConfig::settings`]).
    pub fn validate_ranges(&self) -> Result<(), String> {
        self.validate_file()?;
        self.validate_role()
    }

    /// [`Self::validate_ranges`] without the `server.role` storage rule,
    /// which the environment and flags may still satisfy (`MB_ROLE`,
    /// `MB_STORAGE_URL`, `MB_STATE_URL`, `--role`, `--storage-url`).
    fn validate_file(&self) -> Result<(), String> {
        if self.auth.session_ttl > MAX_SESSION_TTL {
            return Err(format!(
                "auth.session_ttl {} exceeds the maximum of 30d",
                humantime::format_duration(self.auth.session_ttl)
            ));
        }
        if !RETENTION_DAYS.contains(&self.audit.retention_days) {
            return Err(format!(
                "audit.retention_days {} must be between {} and {}",
                self.audit.retention_days,
                RETENTION_DAYS.start(),
                RETENTION_DAYS.end()
            ));
        }
        if let Some(tls) = &self.server.tls
            && (tls.cert_file.as_os_str().is_empty() || tls.key_file.as_os_str().is_empty())
        {
            return Err(
                "server.tls requires both cert_file and key_file (or omit server.tls)".into(),
            );
        }
        if let Some(url) = &self.bus.url
            && url != "local"
            && !matches!(
                scheme(url).as_deref(),
                Some("redis" | "rediss" | "postgres" | "postgresql")
            )
        {
            return Err(format!(
                "bus.url {url:?} must be a redis://, rediss:// or postgres:// URL, or local"
            ));
        }
        if let Some(proxy) = &self.outbound.proxy {
            let ok = reqwest::Url::parse(proxy).is_ok_and(|u| {
                matches!(u.scheme(), "http" | "https")
                    && u.host_str().is_some_and(|h| !h.is_empty())
            });
            if !ok {
                return Err(format!(
                    "outbound.proxy {proxy:?} must be an http:// or https:// URL"
                ));
            }
        }
        self.validate_limits()?;
        self.validate_protocols()?;
        crate::proxy::images::validate_image_entries(&self.proxy.images)
            .map_err(|e| format!("proxy.images: {e}"))?;
        self.proxy.check_bind(self.proxy.host)?;
        for (name, mock) in &self.mocks {
            let prefix = format!("mocks.{name}.");
            mb_store::validate_hosts(&mock.hosts).map_err(|e| format!("{prefix}hosts: {e}"))?;
            mb_store::MockBehavior {
                path_rewrite: mock.path_rewrite.clone(),
                graphql_path: mock.graphql_path.clone(),
                ..Default::default()
            }
            .validate()
            .map_err(|e| format!("mocks.{name}.{e}"))?;
            let webhooks: Vec<_> = mock.webhooks.iter().map(FileWebhook::to_store).collect();
            mb_store::validate_webhooks(&webhooks).map_err(|e| format!("{prefix}{e}"))?;
            if let Some(chaos) = &mock.chaos {
                chaos.validate(&prefix)?;
            }
            mock.limits
                .to_store()
                .validate()
                .map_err(|e| format!("{prefix}{e}"))?;
            if let Some(default) = &mock.default_scenario
                && !mock.scenarios.contains_key(default)
            {
                return Err(format!(
                    "{prefix}default_scenario: no scenario named {default:?}"
                ));
            }
            for (sname, scenario) in &mock.scenarios {
                let prefix = format!("mocks.{name}.scenarios.{sname}.");
                if let Some(chaos) = &scenario.chaos {
                    chaos.validate(&prefix)?;
                }
                if let Some(interval) = scenario.stream_item_interval
                    && interval > MAX_STREAM_INTERVAL
                {
                    return Err(format!(
                        "{prefix}stream_item_interval {} must be at most 30s",
                        humantime::format_duration(interval)
                    ));
                }
                mb_store::ScenarioRules {
                    headers: scenario.headers.clone(),
                    ..mb_store::ScenarioRules::default()
                }
                .check_headers()
                .map_err(|e| format!("{prefix}headers: {e}"))?;
            }
        }
        Scrubber::new(&ScrubberConfig {
            builtins: self.pii.patterns.clone(),
            custom: self.pii.custom.clone(),
        })
        .map_err(|e| e.to_string())?;
        self.import.settings()?;
        Ok(())
    }

    fn validate_limits(&self) -> Result<(), String> {
        let l = &self.limits;
        in_range(
            "limits.max_concurrent_generations",
            l.max_concurrent_generations,
            MAX_CONCURRENT_GENERATIONS,
        )?;
        in_range(
            "limits.max_request_bytes",
            l.max_request_bytes,
            MockLimits::MIN_BYTES..=MockLimits::MAX_REQUEST_BYTES,
        )?;
        in_range(
            "limits.max_response_bytes",
            l.max_response_bytes,
            MockLimits::MIN_BYTES..=MockLimits::MAX_RESPONSE_BYTES,
        )?;
        if l.output_budget_tokens != 0 {
            in_range(
                "limits.output_budget_tokens",
                l.output_budget_tokens,
                MIN_OUTPUT_BUDGET..=MAX_OUTPUT_BUDGET,
            )?;
        }
        Ok(())
    }

    fn validate_protocols(&self) -> Result<(), String> {
        let p = &self.protocols;
        if p.sse.keepalive < Duration::from_secs(1) {
            return Err(format!(
                "protocols.sse.keepalive must be at least 1s (got {})",
                humantime::format_duration(p.sse.keepalive)
            ));
        }
        if !p.graphql.path.starts_with('/') {
            return Err(format!(
                "protocols.graphql.path must start with '/' (got {:?})",
                p.graphql.path
            ));
        }
        if p.grpc.max_message_bytes < MIN_GRPC_MESSAGE_BYTES {
            return Err(format!(
                "protocols.grpc.max_message_bytes {} must be at least {MIN_GRPC_MESSAGE_BYTES}",
                p.grpc.max_message_bytes
            ));
        }
        if p.stream.item_interval > MAX_STREAM_INTERVAL {
            return Err(format!(
                "protocols.stream.item_interval {} must be at most 30s",
                humantime::format_duration(p.stream.item_interval)
            ));
        }
        in_range(
            "protocols.graphql.max_root_fields",
            p.graphql.max_root_fields,
            1..=1000,
        )?;
        in_range(
            "protocols.graphql.max_subscriptions_per_connection",
            p.graphql.max_subscriptions_per_connection,
            1..=1000,
        )?;
        let init_timeout = p.graphql.connection_init_timeout;
        if !(Duration::from_secs(1)..=Duration::from_secs(10)).contains(&init_timeout) {
            return Err(format!(
                "protocols.graphql.connection_init_timeout must be between 1s and 10s (got {})",
                humantime::format_duration(init_timeout)
            ));
        }
        in_range(
            "protocols.graphql.max_connections_per_mock",
            p.graphql.max_connections_per_mock,
            1..=100_000,
        )
    }

    /// `control` and `data` nodes share their stores with other nodes: the
    /// control store must be Postgres, the state store Postgres or Redis,
    /// and the bus not in-process.
    fn validate_role(&self) -> Result<(), String> {
        let role = self.server.role;
        if role == Role::All {
            return Ok(());
        }
        let role = role.as_str();
        if !is_postgres(&self.storage.url) {
            return Err(format!(
                "server.role {role} requires a postgres:// storage.url; SQLite deployments are single-node"
            ));
        }
        let state = self.storage.effective_state_url();
        if !is_postgres(state) && !is_redis(state) {
            return Err(format!(
                "server.role {role} requires a shared state store (storage.state_url postgres:// or redis://)"
            ));
        }
        // An in-process bus would leave the data nodes deaf to changes and
        // make every control node the leader.
        if self.bus_target() == BusTarget::Local {
            return Err(format!(
                "server.role {role} requires a shared bus; bus.url local is single-node (leave it unset, or use postgres:// or redis://)"
            ));
        }
        Ok(())
    }

    /// Where change notifications travel: `bus.url` `local` is in-process,
    /// `redis(s)://` Redis and `postgres(ql)://` Postgres; unset uses
    /// `storage.url` when it is Postgres and is in-process otherwise. (Any
    /// other `bus.url` fails [`Self::validate_ranges`]; it maps to `Local`.)
    pub fn bus_target(&self) -> BusTarget {
        match &self.bus.url {
            Some(url) if is_redis(url) => BusTarget::Redis(url.clone()),
            Some(url) if is_postgres(url) => BusTarget::Postgres(url.clone()),
            Some(_) => BusTarget::Local,
            None if is_postgres(&self.storage.url) => BusTarget::Postgres(self.storage.url.clone()),
            None => BusTarget::Local,
        }
    }

    /// The PII scrubber's patterns; `None` unless `pii.enabled`.
    pub fn scrubber_config(&self) -> Option<ScrubberConfig> {
        self.pii.enabled.then(|| ScrubberConfig {
            builtins: self.pii.patterns.clone(),
            custom: self.pii.custom.clone(),
        })
    }

    /// The adapters' settings ([`ProtocolsConfig::settings`] plus the
    /// `server:` keys they need) and the engine's stream limits.
    pub fn protocol_settings(&self) -> (ProtocolSettings, StreamConfig) {
        let (mut settings, stream) = self.protocols.settings();
        settings.trust_forwarded_headers = self.server.trust_forwarded_headers;
        settings.tls_terminated = self.server.tls.is_some();
        (settings, stream)
    }
}

/// `Err` naming `key` unless `value` is in `range`.
fn in_range<T: PartialOrd + std::fmt::Display>(
    key: &str,
    value: T,
    range: std::ops::RangeInclusive<T>,
) -> Result<(), String> {
    if range.contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "{key} {value} must be between {} and {}",
            range.start(),
            range.end()
        ))
    }
}

/// A URL's scheme, lowercased.
fn scheme(url: &str) -> Option<String> {
    url.split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
}

fn is_postgres(url: &str) -> bool {
    matches!(scheme(url).as_deref(), Some("postgres" | "postgresql"))
}

fn is_redis(url: &str) -> bool {
    matches!(scheme(url).as_deref(), Some("redis" | "rediss"))
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
    let explicit = explicit_ports(&text);
    cfg.server.admin_port_explicit = explicit.admin_port;
    cfg.server.grpc_port_explicit = explicit.grpc_port;
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
    resolve_relative_paths(&mut cfg, &dir);
    cfg.validate_file()
        .map_err(|e| anyhow!(e))
        .with_context(|| format!("invalid config file {}", path.display()))?;
    Ok(cfg)
}

/// Makes the relative file paths of a loaded config file relative to its
/// directory `dir`: `proxy.ca_dir`, `sqlite:` paths in `storage.url` and
/// `storage.state_url`, `server.tls`, `outbound.ca_file` and the brokers'
/// `ca_file`. Values from flags and environment variables are applied later
/// and stay relative to the working directory.
fn resolve_relative_paths(cfg: &mut FileConfig, dir: &Path) {
    let join = |p: &mut PathBuf| {
        if p.is_relative() && !p.as_os_str().is_empty() {
            *p = dir.join(p.strip_prefix(".").unwrap_or(p));
        }
    };
    if let Some(ca_dir) = &mut cfg.proxy.ca_dir {
        join(ca_dir);
    }
    if let Some(tls) = &mut cfg.server.tls {
        join(&mut tls.cert_file);
        join(&mut tls.key_file);
    }
    if let Some(ca) = &mut cfg.outbound.ca_file {
        join(ca);
    }
    for broker in cfg.brokers.values_mut() {
        if let Some(ca) = &mut broker.ca_file {
            join(ca);
        }
    }
    cfg.storage.url = resolve_sqlite_url(&cfg.storage.url, dir);
    if let Some(url) = &mut cfg.storage.state_url {
        *url = resolve_sqlite_url(url, dir);
    }
}

/// `sqlite://<relative path>` (or `sqlite:<relative path>`) as
/// `sqlite://<dir>/<path>`, keeping any `?` options; other URLs, absolute
/// paths and `sqlite::memory:` unchanged.
fn resolve_sqlite_url(url: &str, dir: &Path) -> String {
    let Some(rest) = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
    else {
        return url.to_string();
    };
    let (file, query) = match rest.split_once('?') {
        Some((file, query)) => (file, Some(query)),
        None => (rest, None),
    };
    let path = Path::new(file);
    if file.is_empty() || file.starts_with(':') || file.starts_with("file:") || path.is_absolute() {
        return url.to_string();
    }
    let path = dir.join(path.strip_prefix(".").unwrap_or(path));
    match query {
        Some(query) => format!("sqlite://{}?{query}", path.display()),
        None => format!("sqlite://{}", path.display()),
    }
}

/// Which optional ports a config document sets.
#[derive(Default)]
struct ExplicitPorts {
    admin_port: bool,
    grpc_port: bool,
}

/// Whether a config document sets `server.admin_port` and
/// `server.grpc_port`.
fn explicit_ports(text: &str) -> ExplicitPorts {
    #[derive(Deserialize, Default)]
    struct Probe {
        #[serde(default)]
        server: Option<ProbeServer>,
    }
    #[derive(Deserialize, Default)]
    struct ProbeServer {
        #[serde(default)]
        admin_port: Option<serde::de::IgnoredAny>,
        #[serde(default)]
        grpc_port: Option<serde::de::IgnoredAny>,
    }
    serde_saphyr::from_str::<Probe>(text)
        .ok()
        .and_then(|p| p.server)
        .map(|s| ExplicitPorts {
            admin_port: s.admin_port.is_some(),
            grpc_port: s.grpc_port.is_some(),
        })
        .unwrap_or_default()
}

/// Applies the `MB_*` environment variables (see [`apply_env_from`]).
pub fn apply_env(cfg: &mut FileConfig) -> anyhow::Result<()> {
    apply_env_from(cfg, |k| std::env::var(k).ok())
}

/// Applies `MB_HOST`, `MB_PORT`, `MB_ADMIN_PORT`, `MB_GRPC_PORT`,
/// `MB_STORAGE_URL`, `MB_STATE_URL` (`storage.state_url`), `MB_BUS_URL`
/// (`bus.url`) and `MB_ROLE` (`server.role`: `all`, `control` or `data`),
/// and the `MB_LLM_*` variables to the [`ENV_PROFILE`]
/// profile (created with
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
    if let Some(v) = get("MB_GRPC_PORT") {
        cfg.server.grpc_port = v.parse().with_context(|| format!("MB_GRPC_PORT={v:?}"))?;
        cfg.server.grpc_port_explicit = true;
    }
    if let Some(v) = get("MB_STORAGE_URL") {
        cfg.storage.url = v;
    }
    if let Some(v) = get("MB_STATE_URL") {
        cfg.storage.state_url = Some(v);
    }
    if let Some(v) = get("MB_BUS_URL") {
        cfg.bus.url = Some(v);
    }
    if let Some(v) = get("MB_ROLE") {
        cfg.server.role = v
            .parse()
            .map_err(|e: String| anyhow!(e))
            .with_context(|| format!("MB_ROLE={v:?}"))?;
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
