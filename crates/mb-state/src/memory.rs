use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use dashmap::mapref::one::{Ref, RefMut};
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::{
    CHANGE_CHANNEL_CAPACITY, ChangeKind, SWEEP_INTERVAL, SessionInfo, SessionKey, StateChanged,
    StateError, StateStore, StoredResource,
};

/// One resource type of a session.
#[derive(Default)]
struct TypeData {
    /// id -> (recency sequence, resource)
    by_id: HashMap<String, (u64, StoredResource)>,
    /// recency sequence -> id, so a list reads only the newest `limit`.
    order: BTreeMap<u64, String>,
}

struct SessionData {
    /// Monotonic clock, for expiry (pausable in tests).
    last_touched: Instant,
    /// Wall clock, for reporting.
    last_touched_at: DateTime<Utc>,
    seq: u64,
    /// resource_type -> its resources
    resources: HashMap<String, TypeData>,
}

impl SessionData {
    fn new() -> Self {
        Self {
            last_touched: Instant::now(),
            last_touched_at: Utc::now(),
            seq: 0,
            resources: HashMap::new(),
        }
    }

    fn touch(&mut self) {
        self.last_touched = Instant::now();
        self.last_touched_at = Utc::now();
    }

    fn insert(&mut self, r: StoredResource) {
        self.seq += 1;
        let seq = self.seq;
        let data = self.resources.entry(r.resource_type.clone()).or_default();
        let id = r.id.clone();
        data.order.insert(seq, id.clone());
        if let Some((old, _)) = data.by_id.insert(id, (seq, r)) {
            data.order.remove(&old);
        }
    }

    fn get(&self, resource_type: &str, id: &str) -> Option<&StoredResource> {
        self.resources
            .get(resource_type)?
            .by_id
            .get(id)
            .map(|(_, r)| r)
    }

    fn contains(&self, resource_type: &str, id: &str) -> bool {
        self.get(resource_type, id).is_some()
    }

    fn remove(&mut self, resource_type: &str, id: &str) -> bool {
        let Some(data) = self.resources.get_mut(resource_type) else {
            return false;
        };
        let Some((seq, _)) = data.by_id.remove(id) else {
            return false;
        };
        data.order.remove(&seq);
        if data.by_id.is_empty() {
            self.resources.remove(resource_type);
        }
        true
    }

    fn len(&self) -> usize {
        self.resources.values().map(|d| d.by_id.len()).sum()
    }

    /// Up to `limit` resources of `resource_type`, most recent first.
    fn list(&self, resource_type: &str, limit: usize) -> Vec<StoredResource> {
        let Some(data) = self.resources.get(resource_type) else {
            return Vec::new();
        };
        data.order
            .values()
            .rev()
            .filter_map(|id| data.by_id.get(id))
            .take(limit)
            .map(|(_, r)| r.clone())
            .collect()
    }

    /// Every resource, oldest first.
    fn export(&self) -> Vec<StoredResource> {
        let mut all: Vec<&(u64, StoredResource)> = self
            .resources
            .values()
            .flat_map(|d| d.by_id.values())
            .collect();
        all.sort_unstable_by_key(|(seq, _)| *seq);
        all.into_iter().map(|(_, r)| r.clone()).collect()
    }
}

type Entry<'a> = RefMut<'a, SessionKey, SessionData>;
type ReadEntry<'a> = Ref<'a, SessionKey, SessionData>;

struct Inner {
    sessions: DashMap<SessionKey, SessionData>,
    ttl: Duration,
    tx: broadcast::Sender<StateChanged>,
}

impl Inner {
    fn expired(&self, d: &SessionData, now: Instant) -> bool {
        now.saturating_duration_since(d.last_touched) >= self.ttl
    }

    /// Fetches a live session for reading, under a shared shard lock, without
    /// refreshing it. Never creates one; an expired session is purged and
    /// reported as absent.
    fn live(&self, s: &SessionKey) -> Option<ReadEntry<'_>> {
        let now = Instant::now();
        let entry = self.sessions.get(s)?;
        if self.expired(&entry, now) {
            drop(entry);
            self.purge_expired(s);
            return None;
        }
        Some(entry)
    }

    /// Fetches a live session for writing and refreshes it. Never creates
    /// one; an expired session is purged and reported as absent.
    fn touch_existing(&self, s: &SessionKey) -> Option<Entry<'_>> {
        let now = Instant::now();
        let mut entry = self.sessions.get_mut(s)?;
        if self.expired(&entry, now) {
            drop(entry);
            self.purge_expired(s);
            return None;
        }
        entry.touch();
        Some(entry)
    }

    fn purge_expired(&self, s: &SessionKey) {
        self.sessions
            .remove_if(s, |_, d| self.expired(d, Instant::now()));
    }

    /// Fetches a live session, creating it (or restarting an expired one),
    /// and refreshes it.
    fn touch_or_create(&self, s: &SessionKey) -> Entry<'_> {
        let now = Instant::now();
        let mut entry = self
            .sessions
            .entry(s.clone())
            .or_insert_with(SessionData::new);
        if self.expired(&entry, now) {
            *entry = SessionData::new();
        }
        entry.touch();
        entry
    }

    fn sweep(&self) {
        let now = Instant::now();
        self.sessions.retain(|_, d| !self.expired(d, now));
    }

    fn publish(&self, s: &SessionKey, resource_type: &str, id: &str, kind: ChangeKind) {
        // No subscribers is fine.
        let _ = self.tx.send(StateChanged {
            session: s.clone(),
            resource_type: resource_type.to_string(),
            id: id.to_string(),
            kind,
            origin: crate::ChangeOrigin::Local,
        });
    }
}

pub struct MemoryStore {
    inner: Arc<Inner>,
}

impl MemoryStore {
    pub fn new(ttl: Duration) -> Self {
        let (tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        let inner = Arc::new(Inner {
            sessions: DashMap::new(),
            ttl,
            tx,
        });
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let weak: Weak<Inner> = Arc::downgrade(&inner);
            handle.spawn(async move {
                let mut tick = tokio::time::interval(SWEEP_INTERVAL);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    match weak.upgrade() {
                        Some(inner) => inner.sweep(),
                        None => break,
                    }
                }
            });
        }
        Self { inner }
    }
}

#[async_trait]
impl StateStore for MemoryStore {
    async fn get(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        Ok(self
            .inner
            .live(s)
            .and_then(|entry| entry.get(resource_type, id).cloned()))
    }

    async fn list(
        &self,
        s: &SessionKey,
        resource_type: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        Ok(self
            .inner
            .live(s)
            .map(|entry| entry.list(resource_type, limit))
            .unwrap_or_default())
    }

    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        let (t, id) = (r.resource_type.clone(), r.id.clone());
        self.inner.touch_or_create(s).insert(r);
        self.inner.publish(s, &t, &id, ChangeKind::Upserted);
        Ok(())
    }

    async fn insert_seeded(&self, s: &SessionKey, r: StoredResource) -> Result<bool, StateError> {
        let mut entry = self.inner.touch_or_create(s);
        if entry.contains(&r.resource_type, &r.id) {
            return Ok(false);
        }
        entry.insert(r);
        Ok(true)
    }

    async fn delete(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<bool, StateError> {
        let removed = self
            .inner
            .touch_existing(s)
            .is_some_and(|mut entry| entry.remove(resource_type, id));
        if removed {
            self.inner
                .publish(s, resource_type, id, ChangeKind::Deleted);
        }
        Ok(removed)
    }

    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.sessions.remove(s);
        Ok(())
    }

    async fn reset_mock(&self, workspace: &str, mock: &str) -> Result<(), StateError> {
        self.inner
            .sessions
            .retain(|k, _| !(k.workspace == workspace && k.mock == mock));
        Ok(())
    }

    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.touch_existing(s);
        Ok(())
    }

    async fn session_count(&self, workspace: &str, mock: &str) -> Result<usize, StateError> {
        let now = Instant::now();
        Ok(self
            .inner
            .sessions
            .iter()
            .filter(|e| {
                let k = e.key();
                k.workspace == workspace && k.mock == mock && !self.inner.expired(e.value(), now)
            })
            .count())
    }

    async fn sessions(&self, workspace: &str, mock: &str) -> Result<Vec<SessionInfo>, StateError> {
        let now = Instant::now();
        let mut live: Vec<(Instant, SessionInfo)> = self
            .inner
            .sessions
            .iter()
            .filter(|e| {
                let k = e.key();
                k.workspace == workspace && k.mock == mock && !self.inner.expired(e.value(), now)
            })
            .map(|e| {
                let d = e.value();
                (
                    d.last_touched,
                    SessionInfo {
                        session: e.key().session.clone(),
                        resources: d.len(),
                        last_touched: d.last_touched_at,
                    },
                )
            })
            .collect();
        live.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.session.cmp(&b.1.session)));
        Ok(live.into_iter().map(|(_, info)| info).collect())
    }

    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        Ok(self
            .inner
            .live(s)
            .map(|entry| entry.export())
            .unwrap_or_default())
    }

    async fn seed(&self, s: &SessionKey, resources: Vec<StoredResource>) -> Result<(), StateError> {
        let mut data = SessionData::new();
        for r in resources {
            data.insert(r);
        }
        self.inner.sessions.insert(s.clone(), data);
        Ok(())
    }

    async fn ping(&self) -> Result<(), StateError> {
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.inner.tx.subscribe()
    }
}
