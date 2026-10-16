//! Rate limit for `auth.login_failed` audit events, so a script replaying
//! failed logins cannot grow the audit log or flood the webhook.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// At most one audited failure per source address in this window.
pub const PER_SOURCE_WINDOW: Duration = Duration::from_secs(10);
/// At most [`GLOBAL_LIMIT`] audited failures in this sliding window.
pub const GLOBAL_WINDOW: Duration = Duration::from_secs(60);
pub const GLOBAL_LIMIT: usize = 100;

/// Counter of failed logins that were not audited.
pub const SUPPRESSED_METRIC: &str = "mb_auth_login_failed_suppressed_total";

/// Decides whether a failed login is written to the audit log: at most one
/// per source address per [`PER_SOURCE_WINDOW`] and at most
/// [`GLOBAL_LIMIT`] per [`GLOBAL_WINDOW`] overall. Each refusal increments
/// [`SUPPRESSED_METRIC`]. Requests without a known address share one source.
#[derive(Debug, Default)]
pub struct LoginFailureLimiter {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// When each source last had a failure audited.
    last_by_source: HashMap<Option<IpAddr>, Instant>,
    /// When the audited failures of the last [`GLOBAL_WINDOW`] happened.
    recent: VecDeque<Instant>,
}

impl LoginFailureLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a failure from `source` may be audited now.
    pub fn allow(&self, source: Option<IpAddr>) -> bool {
        self.allow_at(source, Instant::now())
    }

    /// [`allow`](Self::allow) at the given time (times must not go back).
    pub fn allow_at(&self, source: Option<IpAddr>, now: Instant) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let allowed = inner.admit(source, now);
        drop(inner);
        if !allowed {
            metrics::counter!(SUPPRESSED_METRIC).increment(1);
        }
        allowed
    }
}

impl Inner {
    fn admit(&mut self, source: Option<IpAddr>, now: Instant) -> bool {
        // Entries are only added when admitted (at most GLOBAL_LIMIT per
        // GLOBAL_WINDOW), and expired ones are dropped here: both
        // collections stay small.
        self.last_by_source
            .retain(|_, at| now.saturating_duration_since(*at) < PER_SOURCE_WINDOW);
        while self
            .recent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= GLOBAL_WINDOW)
        {
            self.recent.pop_front();
        }
        if self.last_by_source.contains_key(&source) || self.recent.len() >= GLOBAL_LIMIT {
            return false;
        }
        self.last_by_source.insert(source, now);
        self.recent.push_back(now);
        true
    }
}
