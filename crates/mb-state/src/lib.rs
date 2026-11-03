//! Session-scoped state for Mockingbird mocks: the resources a mock's
//! clients created, the cross-node change bus, leader election and the
//! per-session response cache.
//!
//! This is an internal crate of Mockingbird, published so that
//! [`mockingbird-cli`](https://crates.io/crates/mockingbird-cli) can depend
//! on it; it makes no API stability promise. Extensions use
//! [`mb-ext`](https://crates.io/crates/mb-ext) instead.
//!
//! A [`StateStore`] keeps resources per [`SessionKey`] (workspace, mock and
//! session). [`connect`] opens one from a URL: `memory`, `redis://…`,
//! `sqlite:…` or `postgres://…`.
//!
//! ```
//! # async fn demo() -> Result<(), mb_state::StateError> {
//! use std::time::Duration;
//!
//! use mb_state::{SessionKey, StoredResource};
//!
//! let store = mb_state::connect("memory", Duration::from_secs(3600)).await?;
//! let session = SessionKey {
//!     workspace: "default".into(),
//!     mock: "petstore".into(),
//!     session: "s1".into(),
//! };
//! let pet = StoredResource {
//!     resource_type: "pet".into(),
//!     id: "1".into(),
//!     data: serde_json::json!({ "id": 1, "name": "Rex" }),
//! };
//! store.upsert(&session, pet.clone()).await?;
//! assert_eq!(store.get(&session, "pet", "1").await?, Some(pet));
//! # Ok(())
//! # }
//! ```
#![warn(missing_debug_implementations, missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

pub mod bus;
pub mod cache;
pub mod leader;
pub mod memory;
pub mod redis;
pub mod sql;

pub use self::redis::RedisStateStore;
pub use bus::{Bus, BusEnvelope, BusEvent, LocalBus, MemoryBusHub, PgBus, RedisBus};
pub use cache::{
    CacheEpoch, CachedResponse, CachingStateStore, DEFAULT_CACHE_ENTRIES, ResponseCache,
};
pub use leader::{LeaderBackend, Leadership};
pub use memory::MemoryStore;
pub use sql::SqlStateStore;

/// Capacity of the in-process change bus.
pub(crate) const CHANGE_CHANNEL_CAPACITY: usize = 1024;

/// How often expired sessions are swept.
pub(crate) const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// A failure of a state backend (a store, the bus or a leader lock).
#[derive(thiserror::Error, Debug)]
pub enum StateError {
    /// The backend failed or refused the operation; the text says why.
    #[error("state backend: {0}")]
    Backend(String),
}

/// The resource type under which the runtime keeps what is not a resource of the
/// session: the write memory and deleted markers of `mockingbird-protocols`. It lives and dies with
/// the session (TTL, reset, seed) and answers `get`, `upsert` and `delete` like
/// any type, but no backend exports it, counts it among
/// [`SessionInfo::resources`], publishes a change for it or seeds it from a
/// fixture. A contract cannot be expected to name it.
pub const INTERNAL_TYPE: &str = "__mockingbird.earlier_requests";

/// Identifies one session of one mock: the scope every stored resource
/// belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    /// The workspace of the mock.
    pub workspace: String,
    /// The mock's name.
    pub mock: String,
    /// The session id the client sent (or the default session).
    pub session: String,
}

/// One resource a mock's clients created or the mock was seeded with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredResource {
    /// The contract's name for the resource kind (`pet`, `order`...).
    pub resource_type: String,
    /// The resource id, unique within its type and session.
    pub id: String,
    /// The resource body.
    pub data: Value,
}

/// A live session of a mock, as [`StateStore::sessions`] lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    /// The session id.
    pub session: String,
    /// How many resources the session holds.
    pub resources: usize,
    /// When the session was last written or touched.
    pub last_touched: DateTime<Utc>,
}

/// What a [`StateChanged`] did to its resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// Created or replaced.
    Upserted,
    /// Removed.
    Deleted,
}

/// Where a [`StateChanged`] was made: on this node, or on another node and
/// relayed over the cross-node bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum ChangeOrigin {
    /// Made on this node.
    #[default]
    Local,
    /// Made on another node and relayed over the bus.
    Remote,
}

/// A change to one stored resource, as [`StateStore::subscribe`] delivers
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateChanged {
    /// The session of the resource.
    pub session: SessionKey,
    /// The resource's type.
    pub resource_type: String,
    /// The resource's id.
    pub id: String,
    /// Whether it was upserted or deleted.
    pub kind: ChangeKind,
    /// Stores publish [`ChangeOrigin::Local`]; a bus-aware wrapper marks
    /// changes relayed from other nodes [`ChangeOrigin::Remote`].
    pub origin: ChangeOrigin,
}

/// Session-scoped resource store.
///
/// Reads (`get`, `list`, `export`) never create a session and never refresh
/// its `last_touched`; [`StateStore::touch`] does that (callers touch once
/// per request that used state), as do writes. Only `upsert` and `seed`
/// create a session. A session untouched for longer than the store's TTL is
/// invisible and is swept; a later write starts it afresh.
///
/// Every method returns [`StateError::Backend`] when the backend fails.
#[async_trait]
pub trait StateStore: Send + Sync {
    /// The resource of `resource_type` with `id`, if the session holds it.
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
    /// Stores `r`, replacing a resource with its type and id, and creates or
    /// refreshes the session.
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError>;
    /// Stores `r` only when the session holds no resource with its type and
    /// id, creating or refreshing the session like `upsert`; `true` when
    /// stored. Backends publish no change event (seeding is not a write a
    /// client made). This default, for test doubles, does publish.
    async fn insert_seeded(&self, s: &SessionKey, r: StoredResource) -> Result<bool, StateError> {
        if self.get(s, &r.resource_type, &r.id).await?.is_some() {
            return Ok(false);
        }
        self.upsert(s, r).await?;
        Ok(true)
    }
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
    /// How many live sessions a mock has (the `mb_state_sessions` gauge):
    /// always the length of [`StateStore::sessions`]. Backends may count
    /// without listing.
    async fn session_count(&self, workspace: &str, mock: &str) -> Result<usize, StateError> {
        Ok(self.sessions(workspace, mock).await?.len())
    }
    /// All resources of a session, oldest first (so [`StateStore::seed`]
    /// restores the same recency order).
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError>;
    /// Replaces the session's contents with `resources` (later entries are
    /// more recent), creating the session if needed. Publishes no change
    /// events.
    async fn seed(&self, s: &SessionKey, resources: Vec<StoredResource>) -> Result<(), StateError>;
    /// Checks that the backend answers.
    async fn ping(&self) -> Result<(), StateError>;
    /// A receiver of this store's [`StateChanged`] events.
    fn subscribe(&self) -> broadcast::Receiver<StateChanged>;
    /// The per-session response cache, when this store keeps one (see
    /// [`CachingStateStore`]).
    fn response_cache(&self) -> Option<&ResponseCache> {
        None
    }
}

/// Opens a state store: `memory` gives a [`MemoryStore`]; `redis://…` and
/// `rediss://…` give a [`RedisStateStore`]; `sqlite:…` and `postgres://…`
/// give a [`SqlStateStore`].
///
/// # Errors
///
/// [`StateError::Backend`] when the URL names no supported backend, or the
/// backend cannot be reached or migrated.
pub async fn connect(url: &str, ttl: Duration) -> Result<Arc<dyn StateStore>, StateError> {
    if url == "memory" {
        Ok(Arc::new(MemoryStore::new(ttl)))
    } else if url.starts_with("redis://") || url.starts_with("rediss://") {
        Ok(Arc::new(RedisStateStore::connect(url, ttl).await?))
    } else {
        Ok(Arc::new(SqlStateStore::connect(url, ttl).await?))
    }
}
