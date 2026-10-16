//! Session-scoped state store for mock resources.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

pub mod bus;
pub mod leader;
pub mod memory;
pub mod redis;
pub mod sql;

pub use self::redis::RedisStateStore;
pub use bus::{Bus, BusEnvelope, BusEvent, LocalBus, MemoryBusHub, PgBus, RedisBus};
pub use leader::{LeaderBackend, Leadership};
pub use memory::MemoryStore;
pub use sql::SqlStateStore;

/// Capacity of the in-process change bus.
pub(crate) const CHANGE_CHANNEL_CAPACITY: usize = 1024;

/// How often expired sessions are swept.
pub(crate) const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(thiserror::Error, Debug)]
pub enum StateError {
    #[error("state backend: {0}")]
    Backend(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub workspace: String,
    pub mock: String,
    pub session: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredResource {
    pub resource_type: String,
    pub id: String,
    pub data: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub session: String,
    pub resources: usize,
    pub last_touched: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Upserted,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateChanged {
    pub session: SessionKey,
    pub resource_type: String,
    pub id: String,
    pub kind: ChangeKind,
}

/// Session-scoped resource store.
///
/// Reads (`get`, `list`, `export`) never create a session and never refresh
/// its `last_touched`; [`StateStore::touch`] does that (callers touch once
/// per request that used state), as do writes. Only `upsert` and `seed`
/// create a session. A session untouched for longer than the store's TTL is
/// invisible and is swept; a later write starts it afresh.
#[async_trait]
pub trait StateStore: Send + Sync {
    async fn get(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError>;
    /// Most recent first.
    async fn list(
        &self,
        s: &SessionKey,
        resource_type: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError>;
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError>;
    /// `true` if the resource existed. Never creates a session.
    async fn delete(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<bool, StateError>;
    /// Removes the session and all its resources. Publishes no change events.
    async fn reset(&self, s: &SessionKey) -> Result<(), StateError>;
    /// Removes every session of a mock with all its resources (the mock was
    /// deleted). Publishes no change events.
    async fn reset_mock(&self, workspace: &str, mock: &str) -> Result<(), StateError>;
    /// Refreshes a live session's `last_touched`. A no-op for an unknown or
    /// expired session: never creates or revives one.
    async fn touch(&self, s: &SessionKey) -> Result<(), StateError>;
    /// Live sessions of a mock, most recently touched first.
    async fn sessions(&self, workspace: &str, mock: &str) -> Result<Vec<SessionInfo>, StateError>;
    /// All resources of a session, oldest first (so [`StateStore::seed`]
    /// restores the same recency order).
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError>;
    /// Replaces the session's contents with `resources` (later entries are
    /// more recent), creating the session if needed. Publishes no change
    /// events.
    async fn seed(&self, s: &SessionKey, resources: Vec<StoredResource>) -> Result<(), StateError>;
    async fn ping(&self) -> Result<(), StateError>;
    fn subscribe(&self) -> broadcast::Receiver<StateChanged>;
}

/// Opens a state store: `memory` gives a [`MemoryStore`]; `redis://…` and
/// `rediss://…` give a [`RedisStateStore`]; `sqlite:…` and `postgres://…`
/// give a [`SqlStateStore`].
pub async fn connect(url: &str, ttl: Duration) -> Result<Arc<dyn StateStore>, StateError> {
    if url == "memory" {
        Ok(Arc::new(MemoryStore::new(ttl)))
    } else if url.starts_with("redis://") || url.starts_with("rediss://") {
        Ok(Arc::new(RedisStateStore::connect(url, ttl).await?))
    } else {
        Ok(Arc::new(SqlStateStore::connect(url, ttl).await?))
    }
}
