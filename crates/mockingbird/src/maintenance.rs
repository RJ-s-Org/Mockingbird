//! Background maintenance jobs: the admin session sweeper and audit
//! retention, and the leader-only work ([`LeaderWork`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use mb_auth::SessionManager;
use mb_store::ControlStore;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::watch::WatchGuard;

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

/// Reconciles the configuration into the control store once (the result is
/// logged by the caller).
pub type ReconcileOnce =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;

/// Starts watching the config file (`--watch`).
pub type StartWatch = Arc<dyn Fn() -> anyhow::Result<WatchGuard> + Send + Sync>;

/// Starts the [`Maintenance`] jobs.
pub type StartMaintenance = Arc<dyn Fn() -> Maintenance + Send + Sync>;

/// The work only the leader does.
#[derive(Clone)]
pub struct LeaderJobs {
    pub reconcile: ReconcileOnce,
    /// `None` without `--watch`.
    pub watch: Option<StartWatch>,
    pub maintenance: StartMaintenance,
}

/// The leader-watch task: on every transition to leader it reconciles once
/// (logging errors), starts the `--watch` guard (when requested) and the
/// [`Maintenance`] jobs; when leadership is lost it drops both. Dropping it
/// stops the task and with it everything it started.
pub struct LeaderWork {
    task: JoinHandle<()>,
}

impl LeaderWork {
    /// Follows `leader`. `reconciled` says the caller already reconciled as
    /// the current leader (at startup), so the first leading state starts
    /// the jobs without reconciling again. Must be called inside a Tokio
    /// runtime.
    pub fn spawn(leader: watch::Receiver<bool>, reconciled: bool, jobs: LeaderJobs) -> Self {
        Self {
            task: tokio::spawn(follow(leader, reconciled, jobs)),
        }
    }
}

impl Drop for LeaderWork {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What a leader keeps running; dropping it stops them.
struct Running {
    _watch: Option<WatchGuard>,
    _maintenance: Maintenance,
}

async fn follow(mut leader: watch::Receiver<bool>, mut reconciled: bool, jobs: LeaderJobs) {
    let mut running: Option<Running> = None;
    loop {
        let leading = *leader.borrow_and_update();
        match (leading, running.is_some()) {
            (true, false) => {
                if !reconciled {
                    tracing::info!("became leader; reconciling the configuration");
                    if let Err(error) = (jobs.reconcile)().await {
                        tracing::error!(
                            error = %format!("{error:#}"),
                            "cannot apply the configuration"
                        );
                    }
                }
                let watch = jobs.watch.as_ref().and_then(|start| match start() {
                    Ok(guard) => Some(guard),
                    Err(error) => {
                        tracing::error!(
                            error = %format!("{error:#}"),
                            "cannot watch the config file"
                        );
                        None
                    }
                });
                running = Some(Running {
                    _watch: watch,
                    _maintenance: (jobs.maintenance)(),
                });
            }
            (false, true) => {
                tracing::warn!("lost leadership; stopping the config watch and maintenance");
                running = None;
            }
            _ => {}
        }
        reconciled = false;
        if leader.changed().await.is_err() {
            // Leadership is final: keep what runs until this task stops.
            std::future::pending::<()>().await;
        }
    }
}
