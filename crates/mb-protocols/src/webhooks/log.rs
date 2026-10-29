//! The in-memory log of webhook deliveries: the newest
//! [`DELIVERY_LOG_CAPACITY`] per mock, shared by the process.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use serde::Serialize;

/// Deliveries kept per `(workspace, mock)`; older ones are forgotten.
pub const DELIVERY_LOG_CAPACITY: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// Waiting for its delay or between attempts.
    Pending,
    Delivered,
    /// Every attempt failed, or the target's secret could not be resolved.
    Failed,
    /// Not attempted: too many deliveries were pending.
    Dropped,
}

impl DeliveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Dropped => "dropped",
        }
    }
}

/// One event sent (or to be sent) to one target.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeliveryRecord {
    /// `whd_` + 16 alphanumerics, one per (event, target).
    pub id: String,
    pub event_id: String,
    pub event_type: String,
    pub operation: String,
    pub session: String,
    /// The target URL without userinfo, query or fragment ([`redacted_url`]).
    pub url: String,
    pub status: DeliveryStatus,
    /// Attempts made so far.
    pub attempts: u32,
    /// The last attempt's HTTP status.
    pub response_status: Option<u16>,
    /// Why the last attempt (or the delivery) failed.
    pub error: Option<String>,
    pub created_at: SystemTime,
    pub finished_at: Option<SystemTime>,
}

type Key = (String, String);

#[derive(Default, Debug)]
pub struct DeliveryLog {
    by_mock: Mutex<HashMap<Key, VecDeque<DeliveryRecord>>>,
}

pub type SharedDeliveryLog = Arc<DeliveryLog>;

impl DeliveryLog {
    fn lock(&self) -> MutexGuard<'_, HashMap<Key, VecDeque<DeliveryRecord>>> {
        // A panic while holding the lock leaves whole records behind.
        self.by_mock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds `record` as the mock's newest, forgetting the oldest beyond
    /// [`DELIVERY_LOG_CAPACITY`].
    pub fn push(&self, workspace: &str, mock: &str, record: DeliveryRecord) {
        let mut map = self.lock();
        let records = map
            .entry((workspace.to_string(), mock.to_string()))
            .or_default();
        records.push_front(record);
        records.truncate(DELIVERY_LOG_CAPACITY);
    }

    /// Applies `f` to the record `id`, if it is still kept.
    pub fn update(
        &self,
        workspace: &str,
        mock: &str,
        id: &str,
        f: impl FnOnce(&mut DeliveryRecord),
    ) {
        let mut map = self.lock();
        if let Some(r) = map
            .get_mut(&(workspace.to_string(), mock.to_string()))
            .and_then(|records| records.iter_mut().find(|r| r.id == id))
        {
            f(r);
        }
    }

    /// Forgets every delivery of mock `mock` (a deleted mock). Deliveries
    /// still in flight for it are no longer updated.
    pub fn forget(&self, workspace: &str, mock: &str) {
        self.lock()
            .remove(&(workspace.to_string(), mock.to_string()));
    }

    /// Forgets the deliveries of every mock of `workspace` (a deleted
    /// workspace).
    pub fn forget_workspace(&self, workspace: &str) {
        self.lock().retain(|(ws, _), _| ws != workspace);
    }

    /// The number of mocks with deliveries kept.
    pub fn mocks(&self) -> usize {
        self.lock().len()
    }

    /// Newest first, at most `limit`.
    pub fn list(&self, workspace: &str, mock: &str, limit: usize) -> Vec<DeliveryRecord> {
        self.lock()
            .get(&(workspace.to_string(), mock.to_string()))
            .map(|records| records.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    }
}

/// `url` without userinfo, query and fragment, so credentials in a target
/// URL never reach the log; `<invalid url>` when it does not parse.
pub fn redacted_url(url: &str) -> String {
    let Ok(mut u) = reqwest::Url::parse(url) else {
        return "<invalid url>".into();
    };
    // Both fail only for URLs that cannot carry credentials.
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.set_query(None);
    u.set_fragment(None);
    u.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> DeliveryRecord {
        DeliveryRecord {
            id: id.into(),
            event_id: "evt".into(),
            event_type: "order.created".into(),
            operation: "createOrder".into(),
            session: "default".into(),
            url: "https://h.test/x".into(),
            status: DeliveryStatus::Delivered,
            attempts: 1,
            response_status: Some(200),
            error: None,
            created_at: SystemTime::now(),
            finished_at: None,
        }
    }

    /// Mocks created and deleted over and over leave nothing behind.
    #[test]
    fn forgotten_mocks_leave_no_entries() {
        let log = DeliveryLog::default();
        for n in [100usize, 1000] {
            for i in 0..n {
                let mock = format!("m{i}");
                log.push("ws", &mock, record("whd_1"));
                log.forget("ws", &mock);
            }
            assert_eq!(log.mocks(), 0, "after {n} create/delete cycles");
        }
    }

    #[test]
    fn forgetting_a_workspace_keeps_the_others() {
        let log = DeliveryLog::default();
        for (ws, mock) in [("a", "x"), ("a", "y"), ("b", "x")] {
            log.push(ws, mock, record("whd_1"));
        }
        log.forget_workspace("a");
        assert_eq!(log.mocks(), 1);
        assert_eq!(log.list("b", "x", 10).len(), 1);
        log.forget("b", "nope");
        assert_eq!(log.mocks(), 1);
    }
}
