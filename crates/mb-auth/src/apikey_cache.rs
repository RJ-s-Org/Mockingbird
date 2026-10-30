//! A bounded, short-lived cache of API key rows in front of the control
//! store, so verifying a key does not read the store on every request.
//!
//! Entries are keyed by key id (never by token) and hold the stored row; the
//! caller still compares the secret hash and checks revocation, expiry and
//! scope on every hit. Only rows that exist are cached: an unknown key id
//! reads the store every time.
//!
//! Invalidation is process-wide: [`invalidate_all`] bumps a global epoch, and
//! an entry loaded in an older epoch is a miss. A read that races an
//! invalidation is not cached, because the entry is tagged with the epoch seen
//! before the store read.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use mb_store::ApiKey;

/// Bumped by [`invalidate_all`]; entries of an older epoch are stale.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// The current invalidation epoch.
pub(crate) fn epoch() -> u64 {
    EPOCH.load(Ordering::Acquire)
}

/// Makes every cache in this process miss on its next lookup of any key.
pub(crate) fn invalidate_all() {
    EPOCH.fetch_add(1, Ordering::AcqRel);
}

struct Entry {
    key: ApiKey,
    epoch: u64,
    loaded: Instant,
}

pub(crate) struct KeyCache {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<HashMap<String, Entry>>,
    /// Store reads made through this cache (misses), for tests.
    store_reads: AtomicU64,
}

impl KeyCache {
    pub(crate) fn new(ttl: Duration, capacity: usize) -> Self {
        KeyCache {
            ttl,
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
            store_reads: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        // A panic while holding the lock cannot leave a map half-updated in
        // a way that matters: every entry is self-contained.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fresh(&self, entry: &Entry, now: Instant, current: u64) -> bool {
        entry.epoch == current && now.duration_since(entry.loaded) < self.ttl
    }

    /// The cached row of `key_id`, if it is fresh.
    pub(crate) fn get(&self, key_id: &str) -> Option<ApiKey> {
        let now = Instant::now();
        let current = epoch();
        let mut entries = self.lock();
        let entry = entries.get(key_id)?;
        if self.fresh(entry, now, current) {
            return Some(entry.key.clone());
        }
        entries.remove(key_id);
        None
    }

    /// Counts one store read.
    pub(crate) fn note_store_read(&self) {
        self.store_reads.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn store_reads(&self) -> u64 {
        self.store_reads.load(Ordering::Relaxed)
    }

    /// Caches `key`, read from the store after observing `read_epoch`.
    /// Nothing is cached when an invalidation happened since.
    pub(crate) fn insert(&self, key: ApiKey, read_epoch: u64) {
        let now = Instant::now();
        let current = epoch();
        if read_epoch != current {
            return;
        }
        let mut entries = self.lock();
        if entries.len() >= self.capacity && !entries.contains_key(&key.key_id) {
            entries.retain(|_, e| self.fresh(e, now, current));
            if entries.len() >= self.capacity {
                let oldest = entries
                    .iter()
                    .min_by_key(|(_, e)| e.loaded)
                    .map(|(id, _)| id.clone());
                if let Some(id) = oldest {
                    entries.remove(&id);
                }
            }
        }
        entries.insert(
            key.key_id.clone(),
            Entry {
                key,
                epoch: read_epoch,
                loaded: now,
            },
        );
    }

    /// Whether this caller should write `last_used_at`: the cached row's
    /// value is older than `interval` (or the row is not cached). The
    /// cached value is set to `now` here, so concurrent callers that share
    /// the entry write it once.
    pub(crate) fn claim_touch(&self, key_id: &str, now: DateTime<Utc>, interval_secs: i64) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(key_id) else {
            return true;
        };
        let stale = entry
            .key
            .last_used_at
            .is_none_or(|t| (now - t).num_seconds() >= interval_secs);
        if stale {
            entry.key.last_used_at = Some(now);
        }
        stale
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}
