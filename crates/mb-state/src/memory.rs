use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::{ChangeKind, SessionKey, StateChanged, StateStore, StoredResource};

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct SessionData {
    last_touched: Option<Instant>,
    seq: u64,
    /// (resource_type, id) -> (recency sequence, resource)
    resources: HashMap<(String, String), (u64, StoredResource)>,
}

struct Inner {
    sessions: DashMap<SessionKey, SessionData>,
    ttl: Duration,
    tx: broadcast::Sender<StateChanged>,
}

impl Inner {
    fn expired(&self, d: &SessionData, now: Instant) -> bool {
        d.last_touched
            .is_some_and(|t| now.saturating_duration_since(t) >= self.ttl)
    }

    /// Fetch a live session entry, purging it first if expired, and refresh it.
    fn touch(&self, s: &SessionKey) -> dashmap::mapref::one::RefMut<'_, SessionKey, SessionData> {
        let now = Instant::now();
        let mut entry = self.sessions.entry(s.clone()).or_default();
        if self.expired(&entry, now) {
            *entry = SessionData::default();
        }
        entry.last_touched = Some(now);
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
        });
    }
}

pub struct MemoryStore {
    inner: Arc<Inner>,
}

impl MemoryStore {
    pub fn new(ttl: Duration) -> Self {
        let (tx, _) = broadcast::channel(1024);
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
    async fn get(&self, s: &SessionKey, resource_type: &str, id: &str) -> Option<StoredResource> {
        let entry = self.inner.touch(s);
        entry
            .resources
            .get(&(resource_type.to_string(), id.to_string()))
            .map(|(_, r)| r.clone())
    }

    async fn list(&self, s: &SessionKey, resource_type: &str, limit: usize) -> Vec<StoredResource> {
        let entry = self.inner.touch(s);
        let mut items: Vec<&(u64, StoredResource)> = entry
            .resources
            .iter()
            .filter(|((t, _), _)| t == resource_type)
            .map(|(_, v)| v)
            .collect();
        items.sort_by_key(|i| std::cmp::Reverse(i.0));
        items
            .into_iter()
            .take(limit)
            .map(|(_, r)| r.clone())
            .collect()
    }

    async fn upsert(&self, s: &SessionKey, r: StoredResource) {
        let (t, id) = (r.resource_type.clone(), r.id.clone());
        {
            let mut entry = self.inner.touch(s);
            entry.seq += 1;
            let seq = entry.seq;
            entry.resources.insert((t.clone(), id.clone()), (seq, r));
        }
        self.inner.publish(s, &t, &id, ChangeKind::Upserted);
    }

    async fn delete(&self, s: &SessionKey, resource_type: &str, id: &str) -> bool {
        let removed = {
            let mut entry = self.inner.touch(s);
            entry
                .resources
                .remove(&(resource_type.to_string(), id.to_string()))
                .is_some()
        };
        if removed {
            self.inner
                .publish(s, resource_type, id, ChangeKind::Deleted);
        }
        removed
    }

    async fn reset(&self, s: &SessionKey) {
        self.inner.sessions.remove(s);
    }

    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.inner.tx.subscribe()
    }
}
