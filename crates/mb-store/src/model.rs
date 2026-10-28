//! Control-plane entities persisted by a [`crate::ControlStore`].

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use mb_mcm::{Bundle, Trigger};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// The workspace that always exists and cannot be deleted.
pub const DEFAULT_WORKSPACE: &str = "default";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub display_name: String,
    pub created_at: DateTime<Utc>,
}

/// How a mock's data plane authenticates callers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DataPlaneAuthMode {
    #[default]
    None,
    ApiKey,
    PassthroughValidate,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DataPlaneAuthConfig {
    #[serde(default)]
    pub mode: DataPlaneAuthMode,
    /// Lowercase header carrying the key in `api_key` mode.
    #[serde(default = "DataPlaneAuthConfig::default_header")]
    pub header: String,
}

impl DataPlaneAuthConfig {
    pub const DEFAULT_HEADER: &str = "x-api-key";

    fn default_header() -> String {
        Self::DEFAULT_HEADER.to_string()
    }
}

impl Default for DataPlaneAuthConfig {
    fn default() -> Self {
        Self {
            mode: DataPlaneAuthMode::None,
            header: Self::default_header(),
        }
    }
}

/// Where a resource is managed. `File` resources are read-only through the API.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceSource {
    File,
    Api,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Mock {
    pub id: Uuid,
    pub workspace: String,
    pub name: String,
    pub route_prefix: String,
    pub persona: Option<String>,
    pub llm_profile: Option<String>,
    pub header_overrides: bool,
    pub source: ResourceSource,
    #[serde(default)]
    pub data_plane_auth: DataPlaneAuthConfig,
    /// Fault injection; `None` injects nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chaos: Option<ChaosSettings>,
    #[serde(default)]
    pub limits: MockLimits,
    /// Response consistency, unknown-id handling and the default scenario.
    #[serde(default, skip_serializing_if = "MockBehavior::is_default")]
    pub behavior: MockBehavior,
    /// Host names the mock answers for; empty answers for any host.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Where the mock POSTs an event after each successful write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<WebhookTarget>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NewMock {
    pub name: String,
    /// Defaults to `/<name>`.
    pub route_prefix: Option<String>,
    pub persona: Option<String>,
    pub llm_profile: Option<String>,
    pub header_overrides: bool,
    pub source: ResourceSource,
    #[serde(default)]
    pub data_plane_auth: DataPlaneAuthConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chaos: Option<ChaosSettings>,
    #[serde(default)]
    pub limits: MockLimits,
    #[serde(default, skip_serializing_if = "MockBehavior::is_default")]
    pub behavior: MockBehavior,
    /// Host names the mock answers for; empty answers for any host.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Where the mock POSTs an event after each successful write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<WebhookTarget>,
}

/// How a webhook delivery is signed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookSigning {
    /// `Stripe-Signature: t=<ts>,v1=<hex HMAC-SHA256(secret, "<ts>.<body>")>`.
    #[serde(rename = "stripe")]
    Stripe,
    /// `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>`.
    #[serde(rename = "hmac-sha256")]
    HmacSha256,
    #[serde(rename = "none")]
    None,
}

/// One URL a mock POSTs webhook events to.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WebhookTarget {
    /// Absolute `http` or `https` URL.
    pub url: String,
    /// Operation ids, event types or `"*"`; default `["*"]`.
    #[serde(default = "WebhookTarget::all_events")]
    pub events: Vec<String>,
    /// A secret reference (`env:`, `file:`, `vault:`), never a value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Default: `hmac-sha256` with a secret, `none` without.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<WebhookSigning>,
    /// Wait before the first attempt, at most [`Self::MAX_DELAY_MS`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delay_ms: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl WebhookTarget {
    pub const MAX_TARGETS: usize = 16;
    pub const MAX_DELAY_MS: u64 = 60_000;

    pub fn all_events() -> Vec<String> {
        vec!["*".into()]
    }

    /// `signing`, else `hmac-sha256` with a secret and `none` without.
    pub fn effective_signing(&self) -> WebhookSigning {
        match (self.signing, &self.secret) {
            (Some(s), _) => s,
            (None, Some(_)) => WebhookSigning::HmacSha256,
            (None, None) => WebhookSigning::None,
        }
    }

    /// The first problem with this target, naming the field without the
    /// `webhooks[i].` prefix the caller adds. Never echoes the secret.
    pub fn validate(&self) -> Result<(), String> {
        let Some(url) = url::Url::parse(&self.url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
        else {
            return Err("url must be an absolute http or https URL".into());
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(
                "url must not carry credentials (user:password@); sign with a secret instead"
                    .into(),
            );
        }
        if self.events.is_empty() || self.events.iter().any(|e| e.trim().is_empty()) {
            return Err("events must list operation ids, event types or \"*\" (none empty)".into());
        }
        if let Some(secret) = &self.secret
            && !is_secret_reference(secret)
        {
            return Err(
                "secret must be a secret reference (env:NAME, file:/path or vault:path#key), not a value"
                    .into(),
            );
        }
        let signs = matches!(
            self.effective_signing(),
            WebhookSigning::Stripe | WebhookSigning::HmacSha256
        );
        if signs && self.secret.is_none() {
            return Err("signing stripe or hmac-sha256 needs a secret".into());
        }
        if self.delay_ms > Self::MAX_DELAY_MS {
            return Err(format!("delay_ms must be at most {}", Self::MAX_DELAY_MS));
        }
        Ok(())
    }
}

/// `env:`, `file:` or `vault:` followed by a non-blank remainder.
fn is_secret_reference(s: &str) -> bool {
    ["env:", "file:", "vault:"].iter().any(|p| {
        s.strip_prefix(p)
            .is_some_and(|rest| !rest.trim().is_empty() && !rest.contains(char::is_whitespace))
    })
}

/// At most [`WebhookTarget::MAX_TARGETS`] targets, each valid; messages
/// name the offending `webhooks[i].<field>`.
pub fn validate_webhooks(targets: &[WebhookTarget]) -> Result<(), String> {
    if targets.len() > WebhookTarget::MAX_TARGETS {
        return Err(format!(
            "webhooks: at most {} targets per mock",
            WebhookTarget::MAX_TARGETS
        ));
    }
    for (i, t) in targets.iter().enumerate() {
        t.validate().map_err(|e| format!("webhooks[{i}].{e}"))?;
    }
    Ok(())
}

/// Whether a mock answers repeated reads from a per-session cache and seeds
/// list items into session state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Consistency {
    #[default]
    On,
    Off,
}

impl Serialize for Consistency {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            Consistency::On => "on",
            Consistency::Off => "off",
        })
    }
}

impl<'de> Deserialize<'de> for Consistency {
    /// `"on"`/`"off"`, or a boolean (`true` is on).
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Bool(true) => Ok(Consistency::On),
            Value::Bool(false) => Ok(Consistency::Off),
            Value::String(s) if s == "on" => Ok(Consistency::On),
            Value::String(s) if s == "off" => Ok(Consistency::Off),
            other => Err(serde::de::Error::custom(format!(
                "consistency must be \"on\" or \"off\", not {other}"
            ))),
        }
    }
}

/// How a mock answers a read of an id its session does not hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnknownIds {
    /// Generate a resource for the id.
    #[default]
    Generate,
    /// Answer the operation's 404.
    NotFound,
}

impl Serialize for UnknownIds {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            UnknownIds::Generate => "generate",
            UnknownIds::NotFound => "404",
        })
    }
}

impl<'de> Deserialize<'de> for UnknownIds {
    /// `"generate"`, `"404"` or the number `404` (YAML `unknown_ids: 404`).
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) if s == "generate" => Ok(UnknownIds::Generate),
            Value::String(s) if s == "404" => Ok(UnknownIds::NotFound),
            Value::Number(n) if n.as_u64() == Some(404) => Ok(UnknownIds::NotFound),
            other => Err(serde::de::Error::custom(format!(
                "unknown_ids must be \"generate\" or 404, not {other}"
            ))),
        }
    }
}

/// A mock's answer-consistency settings.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct MockBehavior {
    #[serde(default)]
    pub consistency: Consistency,
    #[serde(default)]
    pub unknown_ids: UnknownIds,
    /// The scenario applied when a call names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_scenario: Option<String>,
    /// Rewrites the path below the route prefix before it is matched
    /// against the contract.
    #[serde(default, skip_serializing_if = "PathRewrite::is_identity")]
    pub path_rewrite: PathRewrite,
    /// A GraphQL mock's endpoint below its route prefix, instead of the
    /// server-wide `protocols.graphql.path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphql_path: Option<String>,
}

/// A mock's path rewrite: `strip_prefix` is removed from the path below the
/// route prefix (when it starts with it, at a segment boundary), then
/// `add_prefix` is put in front. Lets one contract serve under another base
/// path, e.g. TMDB's `/3/...` contract at `movies-proxy.vercel.app/tmdb/...`
/// with `add_prefix: /3`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PathRewrite {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_prefix: Option<String>,
}

impl PathRewrite {
    pub fn is_identity(&self) -> bool {
        self.strip_prefix.is_none() && self.add_prefix.is_none()
    }

    /// Both prefixes are route-prefix shaped (`/seg(/seg)*`) and not `/`.
    pub fn validate(&self) -> Result<(), String> {
        for (field, value) in [
            ("strip_prefix", &self.strip_prefix),
            ("add_prefix", &self.add_prefix),
        ] {
            if let Some(v) = value
                && (v == "/" || crate::validate_route_prefix(v).is_err())
            {
                return Err(format!(
                    "path_rewrite.{field} {v:?} must look like /seg or /seg/seg (not /)"
                ));
            }
        }
        Ok(())
    }

    /// `path` (below the route prefix, starting with `/`) rewritten.
    pub fn apply(&self, path: &str) -> String {
        let rest = match &self.strip_prefix {
            Some(prefix) => match path.strip_prefix(prefix.as_str()) {
                Some("") => "/",
                Some(rest) if rest.starts_with('/') => rest,
                _ => path,
            },
            None => path,
        };
        match &self.add_prefix {
            Some(prefix) if rest == "/" => prefix.clone(),
            Some(prefix) => format!("{prefix}{rest}"),
            None => rest.to_owned(),
        }
    }
}

impl MockBehavior {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The path rewrite is valid and `graphql_path`, when set, is an
    /// absolute path.
    pub fn validate(&self) -> Result<(), String> {
        self.path_rewrite.validate()?;
        if let Some(path) = &self.graphql_path
            && (!path.starts_with('/')
                || path.contains(['?', '#'])
                || path.chars().any(char::is_whitespace))
        {
            return Err(format!(
                "graphql_path {path:?} must be a path starting with '/'"
            ));
        }
        Ok(())
    }
}

/// Bounds of a uniformly random added latency, in milliseconds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct LatencyJitter {
    pub min_ms: u64,
    pub max_ms: u64,
}

/// A mock's (or scenario's) fault injection.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ChaosSettings {
    /// Probability (0–1) that a call answers with an injected error.
    #[serde(default)]
    pub error_rate: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_jitter: Option<LatencyJitter>,
    /// Probability (0–1) that a streamed response is cut short.
    #[serde(default)]
    pub drop_stream_rate: f64,
}

impl ChaosSettings {
    /// Upper bound of `latency_jitter.max_ms`.
    pub const MAX_JITTER_MS: u64 = 30_000;

    /// Checks the ranges; the message names the offending field.
    pub fn validate(&self) -> Result<(), String> {
        unit_rate("chaos.error_rate", self.error_rate)?;
        unit_rate("chaos.drop_stream_rate", self.drop_stream_rate)?;
        if let Some(j) = &self.latency_jitter {
            if j.min_ms > j.max_ms {
                return Err(format!(
                    "chaos.latency_jitter.min_ms ({}) must not exceed chaos.latency_jitter.max_ms ({})",
                    j.min_ms, j.max_ms
                ));
            }
            if j.max_ms > Self::MAX_JITTER_MS {
                return Err(format!(
                    "chaos.latency_jitter.max_ms must be at most {} (30 s)",
                    Self::MAX_JITTER_MS
                ));
            }
        }
        Ok(())
    }

    /// `true` when these settings never inject anything.
    pub fn is_noop(&self) -> bool {
        self.error_rate <= 0.0
            && self.drop_stream_rate <= 0.0
            && self.latency_jitter.as_ref().is_none_or(|j| j.max_ms == 0)
    }
}

fn unit_rate(field: &str, rate: f64) -> Result<(), String> {
    if (0.0..=1.0).contains(&rate) {
        Ok(())
    } else {
        Err(format!("{field} must be between 0 and 1"))
    }
}

/// A token bucket: `burst` calls at once, refilled at `requests_per_second`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct RateLimit {
    pub requests_per_second: f64,
    pub burst: u32,
}

/// A mock's admission limits. Unset fields fall back to the global ones
/// (or to no limit).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct MockLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// Tokens per UTC day, across all replicas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_token_budget: Option<u64>,
    /// In bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_bytes: Option<u64>,
    /// In bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_response_bytes: Option<u64>,
}

impl MockLimits {
    pub const MAX_REQUESTS_PER_SECOND: f64 = 100_000.0;
    pub const MAX_BURST: u32 = 100_000;
    pub const MIN_BYTES: u64 = 1024;
    pub const MAX_REQUEST_BYTES: u64 = 67_108_864;
    pub const MAX_RESPONSE_BYTES: u64 = 16_777_216;

    /// Checks the ranges; the message names the offending field.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(r) = &self.rate_limit {
            // Written so that NaN fails too.
            if !(r.requests_per_second > 0.0
                && r.requests_per_second <= Self::MAX_REQUESTS_PER_SECOND)
            {
                return Err(format!(
                    "limits.rate_limit.requests_per_second must be greater than 0 and at most {}",
                    Self::MAX_REQUESTS_PER_SECOND
                ));
            }
            if !(1..=Self::MAX_BURST).contains(&r.burst) {
                return Err(format!(
                    "limits.rate_limit.burst must be between 1 and {}",
                    Self::MAX_BURST
                ));
            }
        }
        if self.daily_token_budget == Some(0) {
            return Err("limits.daily_token_budget must be at least 1".into());
        }
        byte_cap(
            "limits.max_request_bytes",
            self.max_request_bytes,
            Self::MAX_REQUEST_BYTES,
        )?;
        byte_cap(
            "limits.max_response_bytes",
            self.max_response_bytes,
            Self::MAX_RESPONSE_BYTES,
        )
    }
}

fn byte_cap(field: &str, value: Option<u64>, max: u64) -> Result<(), String> {
    match value {
        Some(v) if !(MockLimits::MIN_BYTES..=max).contains(&v) => Err(format!(
            "{field} must be between {} and {max} bytes",
            MockLimits::MIN_BYTES
        )),
        _ => Ok(()),
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ContractStatus {
    Draft,
    Approved,
    Active,
    Archived,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ContractVersion {
    pub id: Uuid,
    pub mock_id: Uuid,
    /// 1-based, increasing per mock.
    pub version: i64,
    /// Hex sha256 of `source`.
    pub source_digest: String,
    pub source: Vec<u8>,
    pub bundle: Bundle,
    pub status: ContractStatus,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Review metadata of a version produced by an import; `None` for
    /// versions created any other way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_meta: Option<Value>,
    /// Compilation warnings (a JSON array of `mb_import::Warning`); `None`
    /// when none were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NewContractVersion {
    pub source: Vec<u8>,
    pub bundle: Bundle,
    pub status: ContractStatus,
    pub created_by: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ScenarioRules {
    pub status: Option<u16>,
    pub latency_ms: Option<u64>,
    pub set: Option<Value>,
    #[serde(default)]
    pub operations: Vec<String>,
    /// Items per streamed response unless `X-Mockingbird-Stream-Items`
    /// overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_items: Option<u32>,
    /// Pause between streamed items, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_item_interval_ms: Option<u64>,
    /// Replaces the mock's chaos while the scenario applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chaos: Option<ChaosSettings>,
    /// Instructions replacing the scenario's for one operation, by
    /// operation id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operation_instructions: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Scenario {
    pub mock_id: Uuid,
    pub name: String,
    pub instructions: Option<String>,
    pub rules: ScenarioRules,
    pub source: ResourceSource,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LlmProfile {
    pub workspace: String,
    pub name: String,
    pub base_url: String,
    pub model: String,
    /// `env:NAME` or `file:/path`; never a raw secret.
    pub secret_ref: Option<String>,
    pub json_schema: bool,
    pub temperature: f32,
    pub timeout_ms: u64,
    pub max_tokens: u32,
    pub fallback_profile: Option<String>,
    /// Extra request headers sent to the provider, by name. A value may
    /// embed secret references as `${env:NAME}` (see
    /// [`crate::header_secret_refs`]); a raw secret is never stored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// JSON merged into every request body sent to the provider, under the
    /// fields Mockingbird sets itself (see [`crate::validate_extra_body`]):
    /// OpenRouter's `provider` routing, `reasoning` settings and the like.
    /// Not for secrets, which belong in `headers` as references.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra_body: serde_json::Map<String, Value>,
    pub source: ResourceSource,
}

/// What kind of call a request-log entry records.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    #[default]
    Http,
    Graphql,
    Grpc,
    Soap,
    /// A produced or consumed event-channel message.
    Event,
}

impl RequestKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestKind::Http => "http",
            RequestKind::Graphql => "graphql",
            RequestKind::Grpc => "grpc",
            RequestKind::Soap => "soap",
            RequestKind::Event => "event",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "http" => Some(RequestKind::Http),
            "graphql" => Some(RequestKind::Graphql),
            "grpc" => Some(RequestKind::Grpc),
            "soap" => Some(RequestKind::Soap),
            "event" => Some(RequestKind::Event),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RequestLogEntry {
    pub id: i64,
    pub mock_id: Uuid,
    pub operation_id: Option<String>,
    pub session: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub source: Option<String>,
    /// Why the faker answered, when an LLM call failed or none is configured.
    #[serde(default)]
    pub fallback_reason: Option<String>,
    pub latency_ms: u64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub request: Value,
    pub response: Option<Value>,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub kind: RequestKind,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NewRequestLogEntry {
    pub mock_id: Uuid,
    pub operation_id: Option<String>,
    pub session: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub source: Option<String>,
    /// Why the faker answered, when an LLM call failed or none is configured.
    #[serde(default)]
    pub fallback_reason: Option<String>,
    pub latency_ms: u64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub request: Value,
    pub response: Option<Value>,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub kind: RequestKind,
}

/// Request-log query. Results are ordered by id, newest first.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RequestLogFilter {
    pub operation_id: Option<String>,
    pub session: Option<String>,
    pub source: Option<String>,
    /// Clamped to `1..=500`; defaults to 100.
    #[serde(default = "RequestLogFilter::default_limit")]
    pub limit: u32,
    /// Only entries with `id < before_id` (keyset pagination).
    pub before_id: Option<i64>,
    /// Only entries of this kind.
    #[serde(default)]
    pub kind: Option<RequestKind>,
}

impl RequestLogFilter {
    pub const DEFAULT_LIMIT: u32 = 100;
    pub const MAX_LIMIT: u32 = 500;

    fn default_limit() -> u32 {
        Self::DEFAULT_LIMIT
    }

    /// `limit` clamped to `1..=500`.
    pub fn effective_limit(&self) -> u32 {
        self.limit.clamp(1, Self::MAX_LIMIT)
    }
}

impl Default for RequestLogFilter {
    fn default() -> Self {
        Self {
            operation_id: None,
            session: None,
            source: None,
            limit: Self::DEFAULT_LIMIT,
            before_id: None,
            kind: None,
        }
    }
}

/// Per-channel overrides of a mock's event channel.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChannelSettings {
    pub mock_id: Uuid,
    pub channel_id: String,
    pub enabled: bool,
    /// `None` = the contract's triggers.
    pub triggers: Option<Vec<Trigger>>,
    pub source: ResourceSource,
    pub updated_at: DateTime<Utc>,
}

/// A mock's event settings: the broker its channels use.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MockEventSettings {
    pub mock_id: Uuid,
    /// Broker name; `None` picks automatically.
    pub broker: Option<String>,
    pub source: ResourceSource,
    pub updated_at: DateTime<Utc>,
}

/// A stored API key. Only the hex sha256 of its secret is kept.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ApiKey {
    pub key_id: String,
    pub workspace: String,
    pub name: String,
    /// `viewer`, `editor`, `admin` or `owner`.
    pub role: String,
    /// `admin` and/or `data-plane`.
    pub scopes: Vec<String>,
    pub sysadmin: bool,
    /// Hex sha256 of the secret bytes.
    pub secret_hash: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// A server-side console session, keyed by the hex sha256 of its cookie value.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AdminSession {
    pub id_hash: String,
    pub principal: Value,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuditEvent {
    pub id: i64,
    /// `None` for events outside any workspace (e.g. logins).
    pub workspace: Option<String>,
    pub actor_subject: String,
    pub actor_kind: String,
    pub action: String,
    pub target: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub request_id: Option<String>,
    pub at: DateTime<Utc>,
}

/// An [`AuditEvent`] before the store assigns its id.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NewAuditEvent {
    pub workspace: Option<String>,
    pub actor_subject: String,
    pub actor_kind: String,
    pub action: String,
    pub target: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub request_id: Option<String>,
    pub at: DateTime<Utc>,
}

/// Audit-log query. Results are ordered by id, newest first.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuditFilter {
    /// `None` lists every workspace's events (and those without one).
    pub workspace: Option<String>,
    pub action: Option<String>,
    /// Matches `actor_subject` exactly.
    pub actor: Option<String>,
    /// Clamped to `1..=1000`; defaults to 100.
    #[serde(default = "AuditFilter::default_limit")]
    pub limit: u32,
    /// Only events with `id < before_id` (keyset pagination).
    pub before_id: Option<i64>,
}

impl AuditFilter {
    pub const DEFAULT_LIMIT: u32 = 100;
    pub const MAX_LIMIT: u32 = 1000;

    fn default_limit() -> u32 {
        Self::DEFAULT_LIMIT
    }

    /// `limit` clamped to `1..=1000`.
    pub fn effective_limit(&self) -> u32 {
        self.limit.clamp(1, Self::MAX_LIMIT)
    }
}

impl Default for AuditFilter {
    fn default() -> Self {
        Self {
            workspace: None,
            action: None,
            actor: None,
            limit: Self::DEFAULT_LIMIT,
            before_id: None,
        }
    }
}

/// Lifecycle of an [`ImportJob`]: `queued` → `running` → `done` | `failed`,
/// or `canceled` from `queued` or `running`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ImportJobStatus {
    Queued,
    Running,
    Done,
    Failed,
    Canceled,
}

/// How far a running import has got. `phase` is one of the progress phase
/// wire strings (`queued`, `fetching`, `extracting`, …).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ImportProgress {
    pub phase: String,
    pub done: u32,
    pub total: u32,
}

/// A queued or processed import. The job's input is stored alongside it
/// (see [`crate::ControlStore::import_job_input`]) until it finishes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ImportJob {
    pub id: Uuid,
    pub workspace: String,
    pub mock_id: Uuid,
    /// The mock's name when the job was created.
    pub mock: String,
    pub kind: String,
    pub source_name: Option<String>,
    pub status: ImportJobStatus,
    pub progress: ImportProgress,
    /// The draft contract version a `done` job produced.
    pub result_version: Option<i64>,
    /// Why a `failed` job failed.
    pub error: Option<String>,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    /// `merge` or `replace`; `None` = merge when the mock has a draft or
    /// active version.
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NewImportJob {
    pub mock_id: Uuid,
    pub kind: String,
    pub source_name: Option<String>,
    pub input: Vec<u8>,
    pub created_by: Option<String>,
    /// `merge` or `replace`; `None` = merge when the mock has a draft or
    /// active version.
    #[serde(default)]
    pub mode: Option<String>,
}

/// How a running import ended.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ImportOutcome {
    Done { version: i64 },
    Failed { error: String },
}

/// Published on every control-plane mutation. Scenario and contract changes
/// publish the owning mock.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ControlChange {
    Mock { workspace: String, name: String },
    Profile { workspace: String, name: String },
    Workspace { name: String },
}

impl ResourceSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResourceSource::File => "file",
            ResourceSource::Api => "api",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, crate::StoreError> {
        match s {
            "file" => Ok(ResourceSource::File),
            "api" => Ok(ResourceSource::Api),
            other => Err(crate::StoreError::Db(format!(
                "unknown resource source {other:?}"
            ))),
        }
    }
}

impl ContractStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContractStatus::Draft => "draft",
            ContractStatus::Approved => "approved",
            ContractStatus::Active => "active",
            ContractStatus::Archived => "archived",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, crate::StoreError> {
        match s {
            "draft" => Ok(ContractStatus::Draft),
            "approved" => Ok(ContractStatus::Approved),
            "active" => Ok(ContractStatus::Active),
            "archived" => Ok(ContractStatus::Archived),
            other => Err(crate::StoreError::Db(format!(
                "unknown contract status {other:?}"
            ))),
        }
    }
}

impl ImportJobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ImportJobStatus::Queued => "queued",
            ImportJobStatus::Running => "running",
            ImportJobStatus::Done => "done",
            ImportJobStatus::Failed => "failed",
            ImportJobStatus::Canceled => "canceled",
        }
    }

    /// `done`, `failed` or `canceled`.
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            ImportJobStatus::Done | ImportJobStatus::Failed | ImportJobStatus::Canceled
        )
    }

    pub(crate) fn parse(s: &str) -> Result<Self, crate::StoreError> {
        match s {
            "queued" => Ok(ImportJobStatus::Queued),
            "running" => Ok(ImportJobStatus::Running),
            "done" => Ok(ImportJobStatus::Done),
            "failed" => Ok(ImportJobStatus::Failed),
            "canceled" => Ok(ImportJobStatus::Canceled),
            other => Err(crate::StoreError::Db(format!(
                "unknown import job status {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PathRewrite;

    fn rewrite(strip: Option<&str>, add: Option<&str>) -> PathRewrite {
        PathRewrite {
            strip_prefix: strip.map(String::from),
            add_prefix: add.map(String::from),
        }
    }

    #[test]
    fn path_rewrite_strips_then_adds_at_segment_boundaries() {
        let add = rewrite(None, Some("/3"));
        assert_eq!(add.apply("/movie/550"), "/3/movie/550");
        assert_eq!(add.apply("/"), "/3");
        let strip = rewrite(Some("/v1"), None);
        assert_eq!(strip.apply("/v1/orders"), "/orders");
        assert_eq!(strip.apply("/v1"), "/");
        assert_eq!(strip.apply("/v10/orders"), "/v10/orders");
        assert_eq!(strip.apply("/orders"), "/orders");
        let both = rewrite(Some("/api"), Some("/v2"));
        assert_eq!(both.apply("/api/orders"), "/v2/orders");
        assert!(PathRewrite::default().is_identity());
        assert_eq!(PathRewrite::default().apply("/x"), "/x");
    }

    #[test]
    fn path_rewrite_prefixes_are_validated() {
        assert!(rewrite(Some("/a/b"), Some("/3")).validate().is_ok());
        assert!(rewrite(Some("/"), None).validate().is_err());
        assert!(rewrite(None, Some("3")).validate().is_err());
        assert!(rewrite(None, Some("/a b")).validate().is_err());
    }
}
