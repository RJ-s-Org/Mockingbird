//! Background maintenance jobs: the admin session sweeper and audit
//! retention.

use std::sync::Arc;

use mb_auth::SessionManager;
use mb_store::ControlStore;
use tokio::task::JoinHandle;

/// The running maintenance jobs; dropping it stops them.
pub struct Maintenance {
    _handles: Vec<JoinHandle<()>>,
}

impl Maintenance {
    /// Starts the hourly sweep of expired admin sessions (when `sessions`
    /// is set, i.e. in `auth.mode: oidc`) and the daily pruning of audit
    /// events older than `retention_days`. Must be called inside a Tokio
    /// runtime.
    pub fn start(
        store: Arc<dyn ControlStore>,
        sessions: Option<&SessionManager>,
        retention_days: u32,
    ) -> Self {
        let mut handles = Vec::with_capacity(2);
        if let Some(sessions) = sessions {
            handles.push(sessions.spawn_sweeper());
        }
        handles.push(mb_admin::spawn_retention(store, retention_days));
        Self { _handles: handles }
    }
}

impl Drop for Maintenance {
    fn drop(&mut self) {
        for handle in &self._handles {
            handle.abort();
        }
    }
}
