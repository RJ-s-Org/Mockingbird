//! A per-session cache of final responses, so a session asking the same
//! question twice gets the same answer.
//!
//! [`CachingStateStore`] wraps the shared state store and clears the cache
//! through the calls that change session state: a write of a resource type
//! drops that type's answers, a reset drops the session's. Seeded inserts
//! clear nothing, so paginated lists do not evict each other.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use crate::{
    SWEEP_INTERVAL, SessionInfo, SessionKey, StateChanged, StateError, StateStore, StoredResource,
};

/// Answers kept per session before the least recently used is evicted.
pub const DEFAULT_CACHE_ENTRIES: usize = 500;

/// A final response as the data plane sent it.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Option<Value>,
}

struct Entry {
    resource_type: Option<String>,
    value: CachedResponse,
    /// Recency: higher is more recent.
    used: u64,
}

struct SessionCache {
    entries: HashMap<String, Entry>,
    clock: u64,
    /// Bumped by every invalidation of the session.
    epoch: u64,
    /// Monotonic, for idleness (pausable in tests).
    last_used: Instant,
}

impl SessionCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            epoch: 0,
            last_used: Instant::now(),
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.last_used = Instant::now();
        self.clock
    }
}

/// When an answer's generation began, as far as invalidation goes (see
/// [`ResponseCache::put_if_current`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheEpoch {
    clears: u64,
    session: u64,
}

/// Per-session LRU of final responses, keyed by the caller's canonical key.
pub struct ResponseCache {
    sessions: DashMap<SessionKey, SessionCache>,
    /// Bumped by every clear spanning sessions (a mock reset, a lag).
    clears: AtomicU64,
    capacity: usize,
    idle_ttl: Duration,
}

impl ResponseCache {
    pub fn new(capacity: usize, idle_ttl: Duration) -> Self {
        Self {
            sessions: DashMap::new(),
            clears: AtomicU64::new(0),
            capacity: capacity.max(1),
            idle_ttl,
        }
    }

    fn idle(&self, cache: &SessionCache, now: Instant) -> bool {
        now.duration_since(cache.last_used) >= self.idle_ttl
    }

    /// The answer cached under `key`, marking it recently used. A session
    /// idle for the TTL is dropped and misses.
    pub fn get(&self, s: &SessionKey, key: &str) -> Option<CachedResponse> {
        let now = Instant::now();
        if self
            .sessions
            .remove_if(s, |_, cache| self.idle(cache, now))
            .is_some()
        {
            return None;
        }
        let mut cache = self.sessions.get_mut(s)?;
        let used = cache.tick();
        let entry = cache.entries.get_mut(key)?;
        entry.used = used;
        Some(entry.value.clone())
    }

    /// Caches `value` under `key`, tagged with the resource type whose
    /// writes invalidate it, evicting the least recently used entry over
    /// capacity.
    pub fn put(
        &self,
        s: &SessionKey,
        key: String,
        resource_type: Option<&str>,
        value: CachedResponse,
    ) {
        let now = Instant::now();
        let mut cache = self
            .sessions
            .entry(s.clone())
            .or_insert_with(SessionCache::new);
        if self.idle(&cache, now) {
            let epoch = cache.epoch;
            *cache = SessionCache::new();
            cache.epoch = epoch;
        }
        let used = cache.tick();
        cache.entries.insert(
            key,
            Entry {
                resource_type: resource_type.map(str::to_string),
                value,
                used,
            },
        );
        while cache.entries.len() > self.capacity {
            let Some(oldest) = cache
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            cache.entries.remove(&oldest);
        }
    }

    /// The session's invalidation epoch now: read it before generating an
    /// answer and pass it to [`ResponseCache::put_if_current`].
    pub fn epoch(&self, s: &SessionKey) -> CacheEpoch {
        CacheEpoch {
            clears: self.clears.load(Ordering::SeqCst),
            session: self.sessions.get(s).map_or(0, |c| c.epoch),
        }
    }

    /// [`ResponseCache::put`], unless the session was invalidated since
    /// `since`: an answer generated before a write or reset must not
    /// outlive it.
    pub fn put_if_current(
        &self,
        s: &SessionKey,
        key: String,
        resource_type: Option<&str>,
        value: CachedResponse,
        since: CacheEpoch,
    ) {
        if self.epoch(s) == since {
            self.put(s, key, resource_type, value);
        }
    }

    /// Drops the session's answers about `resource_type`.
    pub fn invalidate_type(&self, s: &SessionKey, resource_type: &str) {
        let mut cache = self
            .sessions
            .entry(s.clone())
            .or_insert_with(SessionCache::new);
        cache.epoch += 1;
        cache
            .entries
            .retain(|_, e| e.resource_type.as_deref() != Some(resource_type));
    }

    /// Drops every answer of the session.
    pub fn clear_session(&self, s: &SessionKey) {
        let mut cache = self
            .sessions
            .entry(s.clone())
            .or_insert_with(SessionCache::new);
        cache.epoch += 1;
        cache.entries.clear();
    }

    /// Drops every answer of every session of a mock.
    pub fn clear_mock(&self, workspace: &str, mock: &str) {
        self.clears.fetch_add(1, Ordering::SeqCst);
        self.sessions
            .retain(|k, _| !(k.workspace == workspace && k.mock == mock));
    }

    /// How many answers the session holds.
    pub fn len(&self, s: &SessionKey) -> usize {
        self.sessions.get(s).map_or(0, |c| c.entries.len())
    }

    /// Whether the session holds no answer.
    pub fn is_empty(&self, s: &SessionKey) -> bool {
        self.len(s) == 0
    }

    fn sweep(&self) {
        let now = Instant::now();
        self.sessions.retain(|_, cache| !self.idle(cache, now));
    }

    fn clear(&self) {
        self.clears.fetch_add(1, Ordering::SeqCst);
        self.sessions.clear();
    }
}

/// A [`StateStore`] that owns a [`ResponseCache`] and clears it through
/// the calls that change session state.
pub struct CachingStateStore {
    inner: Arc<dyn StateStore>,
    cache: Arc<ResponseCache>,
}

impl CachingStateStore {
    /// Wraps `inner`. With a Tokio runtime, a task also invalidates on every
    /// change event of `inner` (including changes relayed from other nodes)
    /// and sweeps idle sessions.
    pub fn wrap(
        inner: Arc<dyn StateStore>,
        idle_ttl: Duration,
        capacity: usize,
    ) -> Arc<dyn StateStore> {
        let cache = Arc::new(ResponseCache::new(capacity, idle_ttl));
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let weak: Weak<ResponseCache> = Arc::downgrade(&cache);
            let changes = inner.subscribe();
            handle.spawn(watch(weak, changes));
        }
        Arc::new(Self { inner, cache })
    }
}

/// Invalidates on change events and sweeps idle sessions until the cache
/// is dropped.
async fn watch(weak: Weak<ResponseCache>, mut changes: broadcast::Receiver<StateChanged>) {
    let mut tick = tokio::time::interval(SWEEP_INTERVAL);
    tick.tick().await;
    loop {
        tokio::select! {
            change = changes.recv() => {
                let Some(cache) = weak.upgrade() else { break };
                match change {
                    Ok(c) => cache.invalidate_type(&c.session, &c.resource_type),
                    // Missed events may have changed anything.
                    Err(RecvError::Lagged(_)) => cache.clear(),
                    Err(RecvError::Closed) => break,
                }
            }
            _ = tick.tick() => {
                let Some(cache) = weak.upgrade() else { break };
                cache.sweep();
            }
        }
    }
}

#[async_trait]
impl StateStore for CachingStateStore {
    async fn get(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        self.inner.get(s, resource_type, id).await
    }

    async fn list(
        &self,
        s: &SessionKey,
        resource_type: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        self.inner.list(s, resource_type, limit).await
    }

    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        let resource_type = r.resource_type.clone();
        let result = self.inner.upsert(s, r).await;
        self.cache.invalidate_type(s, &resource_type);
        result
    }

    async fn insert_seeded(&self, s: &SessionKey, r: StoredResource) -> Result<bool, StateError> {
        self.inner.insert_seeded(s, r).await
    }

    async fn delete(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<bool, StateError> {
        let result = self.inner.delete(s, resource_type, id).await;
        self.cache.invalidate_type(s, resource_type);
        result
    }

    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        let result = self.inner.reset(s).await;
        self.cache.clear_session(s);
        result
    }

    async fn reset_mock(&self, workspace: &str, mock: &str) -> Result<(), StateError> {
        let result = self.inner.reset_mock(workspace, mock).await;
        self.cache.clear_mock(workspace, mock);
        result
    }

    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.touch(s).await
    }

    async fn sessions(&self, workspace: &str, mock: &str) -> Result<Vec<SessionInfo>, StateError> {
        self.inner.sessions(workspace, mock).await
    }

    async fn session_count(&self, workspace: &str, mock: &str) -> Result<usize, StateError> {
        self.inner.session_count(workspace, mock).await
    }

    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        self.inner.export(s).await
    }

    async fn seed(&self, s: &SessionKey, resources: Vec<StoredResource>) -> Result<(), StateError> {
        let result = self.inner.seed(s, resources).await;
        self.cache.clear_session(s);
        result
    }

    async fn ping(&self) -> Result<(), StateError> {
        self.inner.ping().await
    }

    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.inner.subscribe()
    }

    fn response_cache(&self) -> Option<&ResponseCache> {
        Some(&self.cache)
    }
}
