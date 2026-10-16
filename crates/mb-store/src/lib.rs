//! Control-plane persistence: workspaces, mocks, contract versions, scenarios,
//! LLM profiles and the request log, behind the backend-neutral [`ControlStore`].

mod error;
mod model;
mod postgres;
mod sqlite;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};
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
    async fn delete_workspace(&self, name: &str, force: bool) -> Result<bool, StoreError>;
    /// Every workspace's mocks, `(workspace, name)` ascending.
    async fn list_all_mocks(&self) -> Result<Vec<Mock>, StoreError>;
    async fn list_mocks(&self, ws: &str) -> Result<Vec<Mock>, StoreError>;
    async fn get_mock(&self, ws: &str, name: &str) -> Result<Option<Mock>, StoreError>;
    /// `Conflict` if the name already exists in the workspace, `NotFound` if
    /// the workspace does not exist.
    async fn create_mock(&self, ws: &str, m: NewMock) -> Result<Mock, StoreError>;
    /// Updates the mock with `m.id`, bumping `updated_at`.
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
    /// Upserts by `(mock_id, name)`.
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

/// Hex sha256.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn digest_is_hex_sha256() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
