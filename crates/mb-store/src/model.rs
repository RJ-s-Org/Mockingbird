//! Control-plane entities persisted by a [`crate::ControlStore`].

use chrono::{DateTime, Utc};
use mb_mcm::Bundle;
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
    pub source: ResourceSource,
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
    pub latency_ms: u64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub request: Value,
    pub response: Option<Value>,
    pub at: DateTime<Utc>,
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
    pub latency_ms: u64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub request: Value,
    pub response: Option<Value>,
    pub at: DateTime<Utc>,
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
        }
    }
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
