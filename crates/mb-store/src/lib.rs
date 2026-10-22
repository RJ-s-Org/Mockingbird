//! Control-plane persistence: workspaces, mocks, contract versions, scenarios,
//! LLM profiles, the request log, API keys, admin sessions, the audit log and
//! the import job queue, behind the backend-neutral [`ControlStore`].

mod error;
mod model;
mod postgres;
mod sqlite;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};
use mb_mcm::Bundle;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use uuid::Uuid;

pub use error::StoreError;
pub use model::*;
pub use postgres::PgStore;
pub use sqlite::SqliteControlStore;

/// Capacity of the [`ControlChange`] broadcast channel.
pub const CHANGE_CHANNEL_CAPACITY: usize = 256;

/// Request-log pruning is amortised: a mock's log is pruned on the first
/// insert for it this process makes, and then on every
/// `RETENTION_PRUNE_INTERVAL`-th insert for that mock (counted per mock, so
/// any interleaving of mocks stays bounded). A mock therefore never holds
/// more than `retention + RETENTION_PRUNE_INTERVAL` entries.
pub const RETENTION_PRUNE_INTERVAL: i64 = 100;

/// How long a store call waits for a pooled connection before failing, so
/// an unreachable database fails fast instead of stalling every request.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound of [`ControlStore::list_import_jobs`]'s `limit`.
pub const MAX_IMPORT_JOB_LIST: u32 = 200;

/// The error of a job failed by [`ControlStore::fail_stale_import_jobs`].
pub const STALE_IMPORT_ERROR: &str = "interrupted: the import worker stopped responding";

#[async_trait]
pub trait ControlStore: Send + Sync {
    async fn ping(&self) -> Result<(), StoreError>;
    /// Name ascending.
    async fn list_workspaces(&self) -> Result<Vec<Workspace>, StoreError>;
    async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>, StoreError>;
    /// `Invalid` for a bad name, `Conflict` if it already exists.
    async fn create_workspace(
        &self,
        name: &str,
        display_name: &str,
    ) -> Result<Workspace, StoreError>;
    /// `NotFound` if the workspace does not exist.
    async fn update_workspace(
        &self,
        name: &str,
        display_name: &str,
    ) -> Result<Workspace, StoreError>;
    /// `false` if it does not exist. The default workspace is a `Conflict`,
    /// as is a non-empty one (any mock or profile) without `force`; with
    /// `force` its mocks (cascading) and profiles go in the same transaction.
    /// Its API keys are always deleted with it; audit events are kept.
    async fn delete_workspace(&self, name: &str, force: bool) -> Result<bool, StoreError>;
    /// Every workspace's mocks, `(workspace, name)` ascending.
    async fn list_all_mocks(&self) -> Result<Vec<Mock>, StoreError>;
    async fn list_mocks(&self, ws: &str) -> Result<Vec<Mock>, StoreError>;
    async fn get_mock(&self, ws: &str, name: &str) -> Result<Option<Mock>, StoreError>;
    /// `Conflict` if the name already exists in the workspace, `NotFound` if
    /// the workspace does not exist, `Invalid` when `chaos` or `limits` is
    /// out of range.
    async fn create_mock(&self, ws: &str, m: NewMock) -> Result<Mock, StoreError>;
    /// Updates the mock with `m.id`, bumping `updated_at`. `Invalid` when
    /// `chaos` or `limits` is out of range.
    async fn update_mock(&self, m: &Mock) -> Result<Mock, StoreError>;
    /// Deletes the mock with its versions, scenarios and request log.
    async fn delete_mock(&self, ws: &str, name: &str) -> Result<bool, StoreError>;
    /// The new version number is the mock's current maximum plus one.
    async fn add_contract_version(
        &self,
        mock_id: Uuid,
        v: NewContractVersion,
    ) -> Result<ContractVersion, StoreError>;
    /// Newest version first.
    async fn list_contract_versions(
        &self,
        mock_id: Uuid,
    ) -> Result<Vec<ContractVersion>, StoreError>;
    async fn get_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<Option<ContractVersion>, StoreError>;
    /// Makes `version` the only `Active` one (the previous becomes `Approved`),
    /// atomically. Activating a `Draft` is `Invalid`.
    async fn activate_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<ContractVersion, StoreError>;
    async fn active_contract(&self, mock_id: Uuid) -> Result<Option<ContractVersion>, StoreError>;
    async fn list_scenarios(&self, mock_id: Uuid) -> Result<Vec<Scenario>, StoreError>;
    /// Upserts by `(mock_id, name)`. `Invalid` when `rules.chaos` is out of
    /// range.
    async fn put_scenario(&self, s: &Scenario) -> Result<(), StoreError>;
    async fn delete_scenario(&self, mock_id: Uuid, name: &str) -> Result<bool, StoreError>;
    async fn list_profiles(&self, ws: &str) -> Result<Vec<LlmProfile>, StoreError>;
    async fn get_profile(&self, ws: &str, name: &str) -> Result<Option<LlmProfile>, StoreError>;
    /// Upserts by `(workspace, name)`. `NotFound` if the workspace does not
    /// exist.
    async fn put_profile(&self, p: &LlmProfile) -> Result<(), StoreError>;
    async fn delete_profile(&self, ws: &str, name: &str) -> Result<bool, StoreError>;
    /// Returns the new row id. The mock's rows beyond the newest `retention`
    /// are deleted on its first insert and then every
    /// [`RETENTION_PRUNE_INTERVAL`] inserts for that mock.
    async fn append_request_log(
        &self,
        e: NewRequestLogEntry,
        retention: u32,
    ) -> Result<i64, StoreError>;
    /// Newest (highest id) first.
    async fn list_request_log(
        &self,
        mock_id: Uuid,
        f: &RequestLogFilter,
    ) -> Result<Vec<RequestLogEntry>, StoreError>;
    /// The entry with `id`, of any mock.
    async fn get_request_log(&self, id: i64) -> Result<Option<RequestLogEntry>, StoreError>;
    /// Adds `tokens` to the mock's usage for `day` (UTC) and returns the new total.
    async fn add_token_usage(
        &self,
        mock_id: Uuid,
        day: chrono::NaiveDate,
        tokens: u64,
    ) -> Result<u64, StoreError>;
    /// The mock's usage for `day` (UTC); 0 when nothing was recorded.
    async fn token_usage(&self, mock_id: Uuid, day: chrono::NaiveDate) -> Result<u64, StoreError>;
    /// Deletes every mock's usage of the days before `before` (UTC);
    /// returns how many rows were deleted.
    async fn prune_token_usage(&self, before: chrono::NaiveDate) -> Result<u64, StoreError>;
    /// Sorted by `channel_id`.
    async fn list_channel_settings(
        &self,
        mock_id: Uuid,
    ) -> Result<Vec<ChannelSettings>, StoreError>;
    /// Upserts by `(mock_id, channel_id)`; publishes the owning mock.
    async fn put_channel_settings(&self, s: &ChannelSettings) -> Result<(), StoreError>;
    /// Publishes the owning mock when a row was deleted.
    async fn delete_channel_settings(
        &self,
        mock_id: Uuid,
        channel_id: &str,
    ) -> Result<bool, StoreError>;
    async fn get_event_settings(
        &self,
        mock_id: Uuid,
    ) -> Result<Option<MockEventSettings>, StoreError>;
    /// Upserts by `mock_id`; publishes the owning mock.
    async fn put_event_settings(&self, s: &MockEventSettings) -> Result<(), StoreError>;
    /// Takes or renews the mock's scheduler lease for `ttl`. `true` when
    /// `holder` now holds it: it was free, expired, or already `holder`'s.
    async fn acquire_scheduler_lease(
        &self,
        mock_id: Uuid,
        holder: &str,
        ttl: Duration,
    ) -> Result<bool, StoreError>;
    /// `NotFound` if the key's workspace does not exist, `Conflict` if the
    /// `key_id` is taken. A workspace's keys go with it when it is deleted.
    async fn create_api_key(&self, k: &ApiKey) -> Result<(), StoreError>;
    async fn get_api_key(&self, key_id: &str) -> Result<Option<ApiKey>, StoreError>;
    /// Newest (`created_at`) first, revoked keys included.
    async fn list_api_keys(&self, ws: &str) -> Result<Vec<ApiKey>, StoreError>;
    /// Sets `revoked_at`; `false` if the key is not in `ws` or already revoked.
    async fn revoke_api_key(&self, ws: &str, key_id: &str) -> Result<bool, StoreError>;
    /// Sets `last_used_at`; an unknown key is ignored.
    async fn touch_api_key(&self, key_id: &str, at: DateTime<Utc>) -> Result<(), StoreError>;
    /// `Conflict` if `id_hash` is taken.
    async fn create_admin_session(&self, s: &AdminSession) -> Result<(), StoreError>;
    async fn get_admin_session(&self, id_hash: &str) -> Result<Option<AdminSession>, StoreError>;
    /// Sets `last_seen`; an unknown session is ignored.
    async fn touch_admin_session(&self, id_hash: &str, at: DateTime<Utc>)
    -> Result<(), StoreError>;
    async fn delete_admin_session(&self, id_hash: &str) -> Result<bool, StoreError>;
    /// Deletes sessions with `expires_at <= now`; returns how many.
    async fn delete_expired_admin_sessions(&self, now: DateTime<Utc>) -> Result<u64, StoreError>;
    /// Assigns the next id.
    async fn append_audit(&self, e: NewAuditEvent) -> Result<AuditEvent, StoreError>;
    /// Newest (highest id) first.
    async fn list_audit(&self, f: &AuditFilter) -> Result<Vec<AuditEvent>, StoreError>;
    /// Deletes events with `at < older_than`; returns how many.
    async fn prune_audit(&self, older_than: DateTime<Utc>) -> Result<u64, StoreError>;
    /// Adds a `Draft` version (numbered like [`Self::add_contract_version`])
    /// carrying `draft_meta`. `Invalid` unless `v.status` is `Draft`.
    /// Publishes no [`ControlChange`]: drafts are never served.
    async fn add_draft_contract_version(
        &self,
        mock_id: Uuid,
        v: NewContractVersion,
        draft_meta: Value,
    ) -> Result<ContractVersion, StoreError>;
    /// Replaces a draft's source (recomputing `source_digest`), bundle and
    /// metadata in place. `NotFound` for a missing version, `Conflict` unless
    /// it is a `Draft`. Publishes nothing.
    async fn update_draft_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
        source: Vec<u8>,
        bundle: Bundle,
        draft_meta: Value,
    ) -> Result<ContractVersion, StoreError>;
    /// Moves a `Draft` to `Approved`, keeping its `draft_meta`. `NotFound`
    /// for a missing version, `Conflict` unless it is a `Draft`. Publishes
    /// nothing.
    async fn approve_contract_version(
        &self,
        mock_id: Uuid,
        version: i64,
    ) -> Result<ContractVersion, StoreError>;
    /// Replaces the version's compilation warnings. `NotFound` for a missing
    /// version. Publishes nothing.
    async fn set_contract_version_warnings(
        &self,
        mock_id: Uuid,
        version: i64,
        warnings: Value,
    ) -> Result<ContractVersion, StoreError>;
    /// Queues a job with progress `{"queued", 0, 0}`, copying the mock's
    /// workspace and name. `NotFound` if the mock does not exist.
    async fn create_import_job(&self, j: NewImportJob) -> Result<ImportJob, StoreError>;
    async fn get_import_job(&self, id: Uuid) -> Result<Option<ImportJob>, StoreError>;
    /// The job's input; `None` for an unknown or finished job (finishing
    /// clears it).
    async fn import_job_input(&self, id: Uuid) -> Result<Option<Vec<u8>>, StoreError>;
    /// Newest (`created_at`) first; `limit` clamped to `1..=200`.
    async fn list_import_jobs(
        &self,
        mock_id: Uuid,
        limit: u32,
    ) -> Result<Vec<ImportJob>, StoreError>;
    /// Atomically moves the oldest queued job to `running` (`started_at` and
    /// `updated_at` = `now`) and returns it; `None` when nothing is queued.
    /// Concurrent callers never claim the same job.
    async fn claim_import_job(&self, now: DateTime<Utc>) -> Result<Option<ImportJob>, StoreError>;
    /// Records progress and bumps `updated_at`; `false` unless the job is
    /// `running`.
    async fn update_import_progress(
        &self,
        id: Uuid,
        p: &ImportProgress,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError>;
    /// Ends a `running` job as `done` or `failed` and clears its input;
    /// `false` unless the job is `running`.
    async fn finish_import_job(
        &self,
        id: Uuid,
        outcome: ImportOutcome,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError>;
    /// Adds `v` as a draft of `mock_id` (like
    /// [`Self::add_draft_contract_version`]) and ends job `id` as `done` with
    /// that version, atomically: `Ok(None)`, with nothing stored, unless the
    /// job is `running` and belongs to the mock. Backends override the
    /// default, which is not atomic.
    async fn complete_import_job(
        &self,
        id: Uuid,
        mock_id: Uuid,
        v: NewContractVersion,
        draft_meta: Value,
        now: DateTime<Utc>,
    ) -> Result<Option<ContractVersion>, StoreError> {
        let running = self
            .get_import_job(id)
            .await?
            .is_some_and(|j| j.mock_id == mock_id && j.status == ImportJobStatus::Running);
        if !running {
            return Ok(None);
        }
        let version = self
            .add_draft_contract_version(mock_id, v, draft_meta)
            .await?;
        let outcome = ImportOutcome::Done {
            version: version.version,
        };
        Ok(self
            .finish_import_job(id, outcome, now)
            .await?
            .then_some(version))
    }
    /// How many of the mock's jobs are `queued` or `running`.
    async fn count_active_import_jobs(&self, mock_id: Uuid) -> Result<u64, StoreError>;
    /// Moves a `queued` or `running` job to `canceled` and clears its input;
    /// `false` otherwise.
    async fn cancel_import_job(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, StoreError>;
    /// Fails every `running` job whose `updated_at` is before
    /// `updated_before` with [`STALE_IMPORT_ERROR`]; returns how many.
    async fn fail_stale_import_jobs(
        &self,
        updated_before: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError>;
    /// Deletes finished jobs with `finished_at < finished_before`; returns how
    /// many.
    async fn prune_import_jobs(&self, finished_before: DateTime<Utc>) -> Result<u64, StoreError>;
    fn subscribe(&self) -> broadcast::Receiver<ControlChange>;
}

/// Opens the store named by `url` and runs its migrations.
///
/// Supported: `sqlite::memory:`, `sqlite://<path>` (created if missing) and
/// `postgres://…` / `postgresql://…`.
pub async fn connect(url: &str) -> Result<Arc<dyn ControlStore>, StoreError> {
    if url.starts_with("sqlite:") {
        Ok(Arc::new(SqliteControlStore::connect(url).await?))
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(Arc::new(PgStore::connect(url).await?))
    } else {
        Err(StoreError::Invalid(format!(
            "unsupported storage url scheme (expected sqlite: or postgres://): {}",
            url.split(':').next().unwrap_or_default()
        )))
    }
}

/// `^[a-z0-9][a-z0-9-]{0,62}$`
pub fn validate_mock_name(name: &str) -> Result<(), StoreError> {
    let bytes = name.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
    if valid {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "mock name {name:?} must match ^[a-z0-9][a-z0-9-]{{0,62}}$"
        )))
    }
}

/// Workspace names follow the mock-name rule `^[a-z0-9][a-z0-9-]{0,62}$`.
pub fn validate_workspace_name(name: &str) -> Result<(), StoreError> {
    validate_mock_name(name).map_err(|_| {
        StoreError::Invalid(format!(
            "workspace name {name:?} must match ^[a-z0-9][a-z0-9-]{{0,62}}$"
        ))
    })
}

/// A lowercase RFC 9110 token of 1–64 characters.
pub fn validate_header_name(name: &str) -> Result<(), StoreError> {
    let valid = (1..=64).contains(&name.len())
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b"!#$%&'*+-.^_`|~".contains(&b)
        });
    if valid {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "header name {name:?} must be a lowercase HTTP token of 1-64 characters"
        )))
    }
}

/// Header names an LLM profile may not set: the client sets them itself
/// (`authorization` comes from the profile's `secret_ref`).
const RESERVED_PROFILE_HEADERS: [&str; 5] = [
    "authorization",
    "content-type",
    "content-length",
    "host",
    "transfer-encoding",
];

/// The secret references embedded in a profile header value as
/// `${<reference>}` (`Bearer ${env:CF_AIG_TOKEN}` → `["env:CF_AIG_TOKEN"]`).
/// `Invalid` for an unterminated or empty placeholder.
pub fn header_secret_refs(value: &str) -> Result<Vec<&str>, StoreError> {
    let mut refs = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| {
            StoreError::Invalid("header value has an unterminated ${…} placeholder".into())
        })?;
        let reference = &after[..end];
        if reference.trim().is_empty() {
            return Err(StoreError::Invalid(
                "header value has an empty ${} placeholder".into(),
            ));
        }
        refs.push(reference);
        rest = &after[end + 1..];
    }
    Ok(refs)
}

/// Whether a header of this (lowercase) name likely carries a credential,
/// so its value must come from a `${…}` secret reference.
pub fn is_sensitive_header(name: &str) -> bool {
    ["auth", "key", "token", "secret", "password", "cookie"]
        .iter()
        .any(|part| name.contains(part))
}

/// Checks an LLM profile's `headers`: lowercase HTTP token names that the
/// client does not set itself, values without line breaks, well-formed
/// `${…}` placeholders, and no literal value for a credential-bearing name
/// (see [`is_sensitive_header`]).
pub fn validate_profile_headers(
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<(), StoreError> {
    for (name, value) in headers {
        validate_header_name(name)?;
        if RESERVED_PROFILE_HEADERS.contains(&name.as_str()) {
            return Err(StoreError::Invalid(format!(
                "header {name:?} is set by the client; use secret_ref for the provider key"
            )));
        }
        if value.contains(['\r', '\n']) || value.len() > 4096 {
            return Err(StoreError::Invalid(format!(
                "header {name:?} must be one line of at most 4096 bytes"
            )));
        }
        let refs = header_secret_refs(value)?;
        if refs.is_empty() && is_sensitive_header(name) {
            return Err(StoreError::Invalid(format!(
                "header {name:?} carries a credential: write it as a secret reference, \
                 for example \"Bearer ${{env:NAME}}\", never as a raw value"
            )));
        }
    }
    Ok(())
}

/// Request-body fields the LLM client sets itself, which `extra_body` may
/// not override.
pub const RESERVED_EXTRA_BODY_FIELDS: [&str; 6] = [
    "model",
    "messages",
    "response_format",
    "temperature",
    "max_tokens",
    "stream",
];

/// Largest accepted `extra_body`, serialized.
const MAX_EXTRA_BODY_BYTES: usize = 8192;

/// Checks an LLM profile's `extra_body`: no field the client sets itself
/// (see [`RESERVED_EXTRA_BODY_FIELDS`]) and at most 8 KiB of JSON.
pub fn validate_extra_body(body: &serde_json::Map<String, Value>) -> Result<(), StoreError> {
    if let Some(field) = RESERVED_EXTRA_BODY_FIELDS
        .iter()
        .find(|f| body.contains_key(**f))
    {
        return Err(StoreError::Invalid(format!(
            "extra_body field {field:?} is set by the client; use the profile's own setting"
        )));
    }
    let size = serde_json::to_vec(body).map_or(usize::MAX, |b| b.len());
    if size > MAX_EXTRA_BODY_BYTES {
        return Err(StoreError::Invalid(format!(
            "extra_body is {size} bytes; at most {MAX_EXTRA_BODY_BYTES} are accepted"
        )));
    }
    Ok(())
}

/// `Invalid` when a mock's chaos or limits are out of range.
pub(crate) fn validate_mock_settings(
    chaos: Option<&ChaosSettings>,
    limits: &MockLimits,
) -> Result<(), StoreError> {
    if let Some(c) = chaos {
        c.validate().map_err(StoreError::Invalid)?;
    }
    limits.validate().map_err(StoreError::Invalid)
}

/// Error for a write into a workspace that does not exist.
pub(crate) fn missing_workspace(name: &str) -> StoreError {
    StoreError::NotFound(format!("workspace {name:?}"))
}

/// `"/"` or `"/seg(/seg)*"` where each segment is `[A-Za-z0-9._-]+`.
pub fn validate_route_prefix(prefix: &str) -> Result<(), StoreError> {
    let valid = prefix == "/"
        || prefix.strip_prefix('/').is_some_and(|rest| {
            rest.split('/').all(|seg| {
                !seg.is_empty()
                    && seg
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            })
        });
    if valid {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "route prefix {prefix:?} must be \"/\" or \"/seg(/seg)*\" with segments of [A-Za-z0-9._-]"
        )))
    }
}

/// Each host is a lowercase DNS name or IPv4 address without a port: 1–253
/// bytes of `.`-separated labels made of `[a-z0-9-]`, not starting or ending
/// with `-`. No host may be listed twice.
pub fn validate_hosts(hosts: &[String]) -> Result<(), StoreError> {
    let mut seen = std::collections::HashSet::new();
    for h in hosts {
        let valid = (1..=253).contains(&h.len())
            && h.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            });
        if !valid {
            return Err(StoreError::Invalid(format!(
                "host {h:?} must be a lowercase DNS name or IPv4 address without a port"
            )));
        }
        if !seen.insert(h.as_str()) {
            return Err(StoreError::Invalid(format!("host {h:?} is listed twice")));
        }
    }
    Ok(())
}

/// Whether two mocks' routes clash: the same prefix, and either both answer
/// for any host or their host sets share a host.
pub fn routes_collide(
    a_prefix: &str,
    a_hosts: &[String],
    b_prefix: &str,
    b_hosts: &[String],
) -> bool {
    a_prefix == b_prefix
        && ((a_hosts.is_empty() && b_hosts.is_empty())
            || a_hosts.iter().any(|h| b_hosts.contains(h)))
}

/// Per-mock request-log insert counters deciding when to prune.
#[derive(Default)]
pub(crate) struct PruneCounter(Mutex<HashMap<Uuid, i64>>);

impl PruneCounter {
    /// Counts an insert for `mock`; `true` when it should prune: the first
    /// insert this process makes for the mock, then every
    /// [`RETENTION_PRUNE_INTERVAL`]-th.
    pub(crate) fn tick(&self, mock: Uuid) -> bool {
        let Ok(mut counts) = self.0.lock() else {
            return true;
        };
        let n = counts.entry(mock).or_insert(0);
        let prune = *n == 0;
        *n = (*n + 1) % RETENTION_PRUNE_INTERVAL;
        prune
    }

    /// Forgets the counters (e.g. after mocks are deleted).
    pub(crate) fn clear(&self) {
        if let Ok(mut counts) = self.0.lock() {
            counts.clear();
        }
    }
}

/// Current time at microsecond precision, which every backend round-trips.
pub(crate) fn now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

/// The progress of a newly queued job.
pub(crate) fn queued_progress() -> ImportProgress {
    ImportProgress {
        phase: "queued".into(),
        done: 0,
        total: 0,
    }
}

/// `Invalid` unless a draft write carries `Draft` status.
pub(crate) fn require_draft_status(v: &NewContractVersion) -> Result<(), StoreError> {
    if v.status == ContractStatus::Draft {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "a draft contract version must have status draft, not {}",
            v.status.as_str()
        )))
    }
}

/// Error for a draft-only write to a version that is not a draft.
pub(crate) fn not_a_draft(version: i64) -> StoreError {
    StoreError::Conflict(format!("contract version {version} is not a draft"))
}

/// `(now + ttl, now)` at microsecond precision, for a lease acquisition.
pub(crate) fn lease_times(ttl: Duration) -> Result<(DateTime<Utc>, DateTime<Utc>), StoreError> {
    let at = now();
    let expires = chrono::Duration::from_std(ttl)
        .ok()
        .and_then(|d| at.checked_add_signed(d))
        .ok_or_else(|| StoreError::Invalid("lease ttl is out of range".into()))?
        .trunc_subsecs(6);
    Ok((expires, at))
}

/// Hex sha256.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn header_secret_refs_parse() {
        assert_eq!(
            header_secret_refs("Bearer ${env:CF_AIG_API_KEY}").unwrap(),
            ["env:CF_AIG_API_KEY"]
        );
        assert_eq!(
            header_secret_refs("${env:A}:${file:/run/b}").unwrap(),
            ["env:A", "file:/run/b"]
        );
        assert!(header_secret_refs("Mockingbird").unwrap().is_empty());
        assert!(header_secret_refs("Bearer ${env:A").is_err());
        assert!(header_secret_refs("${ }").is_err());
    }

    #[test]
    fn profile_headers() {
        let ok = |pairs: &[(&str, &str)]| {
            let map = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            validate_profile_headers(&map)
        };
        assert!(ok(&[("cf-aig-authorization", "Bearer ${env:CF_AIG_API_KEY}")]).is_ok());
        assert!(ok(&[("x-title", "Mockingbird"), ("http-referer", "https://x")]).is_ok());
        // A raw credential, a reserved or uppercase name, a line break.
        assert!(ok(&[("cf-aig-authorization", "Bearer abc123")]).is_err());
        assert!(ok(&[("api-key", "abc123")]).is_err());
        assert!(ok(&[("authorization", "Bearer ${env:X_API_KEY}")]).is_err());
        assert!(ok(&[("X-Title", "Mockingbird")]).is_err());
        assert!(ok(&[("x-title", "a\r\nb")]).is_err());
    }

    #[test]
    fn profile_extra_body() {
        let ok = |v: serde_json::Value| validate_extra_body(v.as_object().unwrap());
        assert!(ok(serde_json::json!({})).is_ok());
        assert!(
            ok(serde_json::json!({
                "provider": {"order": ["atlas-cloud"], "allow_fallbacks": false},
                "reasoning": {"enabled": false}
            }))
            .is_ok()
        );
        // Fields the client sets itself, and oversized bodies.
        for reserved in [
            "model",
            "messages",
            "response_format",
            "temperature",
            "max_tokens",
            "stream",
        ] {
            assert!(
                ok(serde_json::json!({ reserved: 1 })).is_err(),
                "{reserved}"
            );
        }
        assert!(ok(serde_json::json!({"x": "a".repeat(9000)})).is_err());
    }

    #[test]
    fn mock_names() {
        for ok in ["a", "orders", "orders-v2", "0x", &"a".repeat(63)] {
            assert!(validate_mock_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Bad Name",
            "-lead",
            "UPPER",
            "under_score",
            "dot.ted",
            &"a".repeat(64),
        ] {
            assert!(
                matches!(validate_mock_name(bad), Err(StoreError::Invalid(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn workspace_names() {
        assert!(validate_workspace_name("team-a").is_ok());
        for bad in ["Team A", "", "-x", &"a".repeat(64)] {
            assert!(
                matches!(validate_workspace_name(bad), Err(StoreError::Invalid(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn header_names() {
        for ok in [
            "x-api-key",
            "x-key",
            "authorization",
            "a",
            "x_1.~!",
            &"h".repeat(64),
        ] {
            assert!(validate_header_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Bad Header",
            "X-Api-Key",
            "x key",
            "x:y",
            "x\"/y",
            "é",
            &"h".repeat(65),
        ] {
            assert!(
                matches!(validate_header_name(bad), Err(StoreError::Invalid(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn route_prefixes() {
        for ok in ["/", "/orders", "/api/v2", "/A.b_c-d/e"] {
            assert!(validate_route_prefix(ok).is_ok(), "{ok}");
        }
        for bad in ["", "orders", "/orders/", "//", "/a//b", "/sp ace", "/q?x"] {
            assert!(
                matches!(validate_route_prefix(bad), Err(StoreError::Invalid(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn hosts_are_validated() {
        let ok = |h: &[&str]| validate_hosts(&h.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(ok(&[]).is_ok());
        assert!(ok(&["api.github.com", "image.tmdb.org", "localhost", "127.0.0.1"]).is_ok());
        for bad in [
            &["API.github.com"][..],
            &["api.github.com:443"],
            &["-a.test"],
            &["a..test"],
            &[""],
            &["a.test", "a.test"],
            &["*.github.com"],
        ] {
            assert!(matches!(ok(bad), Err(StoreError::Invalid(_))), "{bad:?}");
        }
    }

    #[test]
    fn routes_collide_by_prefix_and_host_set() {
        let h = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(routes_collide("/", &h(&[]), "/", &h(&[])));
        assert!(routes_collide(
            "/",
            &h(&["a.test"]),
            "/",
            &h(&["b.test", "a.test"])
        ));
        assert!(!routes_collide("/", &h(&["a.test"]), "/", &h(&["b.test"])));
        assert!(
            !routes_collide("/", &h(&["a.test"]), "/", &h(&[])),
            "host-specific and hostless coexist"
        );
        assert!(!routes_collide("/", &h(&[]), "/graphql", &h(&[])));
    }

    #[test]
    fn chaos_ranges() {
        let ok = ChaosSettings {
            error_rate: 1.0,
            latency_jitter: Some(LatencyJitter {
                min_ms: 30_000,
                max_ms: 30_000,
            }),
            drop_stream_rate: 0.0,
        };
        assert_eq!(ok.validate(), Ok(()));
        assert!(!ok.is_noop());
        assert!(ChaosSettings::default().is_noop());
        let zero_jitter = ChaosSettings {
            latency_jitter: Some(LatencyJitter::default()),
            ..ChaosSettings::default()
        };
        assert!(zero_jitter.is_noop());
        let cases = [
            (
                ChaosSettings {
                    error_rate: 1.5,
                    ..ChaosSettings::default()
                },
                "chaos.error_rate must be between 0 and 1",
            ),
            (
                ChaosSettings {
                    error_rate: f64::NAN,
                    ..ChaosSettings::default()
                },
                "chaos.error_rate",
            ),
            (
                ChaosSettings {
                    drop_stream_rate: -0.1,
                    ..ChaosSettings::default()
                },
                "chaos.drop_stream_rate",
            ),
            (
                ChaosSettings {
                    latency_jitter: Some(LatencyJitter {
                        min_ms: 5,
                        max_ms: 4,
                    }),
                    ..ChaosSettings::default()
                },
                "chaos.latency_jitter.min_ms",
            ),
            (
                ChaosSettings {
                    latency_jitter: Some(LatencyJitter {
                        min_ms: 0,
                        max_ms: 30_001,
                    }),
                    ..ChaosSettings::default()
                },
                "chaos.latency_jitter.max_ms",
            ),
        ];
        for (c, msg) in cases {
            let err = c.validate().expect_err(msg);
            assert!(err.contains(msg), "{err}");
        }
    }

    #[test]
    fn limit_ranges() {
        let ok = MockLimits {
            rate_limit: Some(RateLimit {
                requests_per_second: 100_000.0,
                burst: 100_000,
            }),
            daily_token_budget: Some(1),
            max_request_bytes: Some(67_108_864),
            max_response_bytes: Some(1024),
        };
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(MockLimits::default().validate(), Ok(()));
        let rate = |rps: f64, burst: u32| MockLimits {
            rate_limit: Some(RateLimit {
                requests_per_second: rps,
                burst,
            }),
            ..MockLimits::default()
        };
        let cases = [
            (rate(0.0, 1), "limits.rate_limit.requests_per_second"),
            (rate(f64::NAN, 1), "limits.rate_limit.requests_per_second"),
            (rate(100_000.1, 1), "limits.rate_limit.requests_per_second"),
            (rate(1.0, 0), "limits.rate_limit.burst"),
            (rate(1.0, 100_001), "limits.rate_limit.burst"),
            (
                MockLimits {
                    daily_token_budget: Some(0),
                    ..MockLimits::default()
                },
                "limits.daily_token_budget",
            ),
            (
                MockLimits {
                    max_request_bytes: Some(1023),
                    ..MockLimits::default()
                },
                "limits.max_request_bytes",
            ),
            (
                MockLimits {
                    max_request_bytes: Some(67_108_865),
                    ..MockLimits::default()
                },
                "limits.max_request_bytes",
            ),
            (
                MockLimits {
                    max_response_bytes: Some(16_777_217),
                    ..MockLimits::default()
                },
                "limits.max_response_bytes",
            ),
        ];
        for (l, msg) in cases {
            let err = l.validate().expect_err(msg);
            assert!(err.contains(msg), "{err}");
        }
    }

    #[test]
    fn digest_is_hex_sha256() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
