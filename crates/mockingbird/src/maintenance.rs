//! Background maintenance jobs: the admin session sweeper and audit
//! retention, the leader-only work ([`LeaderWork`]) and, on every node that
//! serves the data plane, the state-size gauge ([`StateGauge`]).

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use mb_auth::SessionManager;
use mb_state::StateStore;
use mb_store::ControlStore;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::registry::{MockRegistry, mock_label};
use crate::watch::WatchGuard;

/// Gauge of each mounted mock's live state sessions, labelled by `mock`
/// (see [`mock_label`]).
pub const STATE_SESSIONS_METRIC: &str = "mb_state_sessions";

/// How often [`StateGauge`] refreshes [`STATE_SESSIONS_METRIC`].
pub const STATE_GAUGE_INTERVAL: Duration = Duration::from_secs(60);

/// Refreshes [`STATE_SESSIONS_METRIC`] for every mounted mock every
/// [`STATE_GAUGE_INTERVAL`] (first right away); a mock no longer mounted
/// drops to 0. Runs on every node that serves the data plane, leader or
/// not. Dropping it stops the task.
pub struct StateGauge {
    task: JoinHandle<()>,
}

impl StateGauge {
    /// Must be called inside a Tokio runtime.
    pub fn spawn(state: Arc<dyn StateStore>, registry: Arc<MockRegistry>) -> Self {
        Self {
            task: tokio::spawn(async move {
                let mut tick = tokio::time::interval(STATE_GAUGE_INTERVAL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut labelled = BTreeSet::new();
                loop {
                    tick.tick().await;
                    let counts = session_counts(state.as_ref(), &registry.mounted_mocks()).await;
                    labelled = set_state_gauges(counts, labelled);
                }
            }),
        }
    }
}

impl Drop for StateGauge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `(label, live sessions)` of each of `mocks` (`(workspace, name)`); a
/// mock whose count fails is logged and left out (its gauge keeps its last
/// value).
pub async fn session_counts(
    state: &dyn StateStore,
    mocks: &[(String, String)],
) -> Vec<(String, usize)> {
    let mut out = Vec::with_capacity(mocks.len());
    for (workspace, name) in mocks {
        let label = mock_label(workspace, name);
        match state.session_count(workspace, name).await {
            Ok(n) => out.push((label, n)),
            Err(error) => {
                tracing::warn!(mock = %label, %error, "cannot count state sessions");
            }
        }
    }
    out
}

/// Sets the gauge of every counted mock and zeroes the ones `previous`
/// labelled that are no longer counted. Returns the labels now set.
fn set_state_gauges(counts: Vec<(String, usize)>, previous: BTreeSet<String>) -> BTreeSet<String> {
    let mut now = BTreeSet::new();
    for (label, n) in counts {
        metrics::gauge!(STATE_SESSIONS_METRIC, "mock" => label.clone()).set(n as f64);
        now.insert(label);
    }
    for gone in previous.difference(&now) {
        metrics::gauge!(STATE_SESSIONS_METRIC, "mock" => gone.clone()).set(0.0);
    }
    now
}

/// Days of per-mock token usage kept (today's counts the daily budget; the
/// rest is history). Older rows are deleted by [`Maintenance`].
pub const TOKEN_USAGE_RETENTION_DAYS: u64 = 90;

/// The running maintenance jobs; dropping it stops them.
pub struct Maintenance {
    _handles: Vec<JoinHandle<()>>,
}

impl Maintenance {
    /// Starts the hourly sweep of expired admin sessions (when `sessions`
    /// is set, i.e. in `auth.mode: oidc`), the daily pruning of audit
    /// events older than `retention_days` and the daily pruning of token
    /// usage older than [`TOKEN_USAGE_RETENTION_DAYS`]. Must be called
    /// inside a Tokio runtime.
    pub fn start(
        store: Arc<dyn ControlStore>,
        sessions: Option<&SessionManager>,
        retention_days: u32,
    ) -> Self {
        let mut handles = Vec::with_capacity(3);
        if let Some(sessions) = sessions {
            handles.push(sessions.spawn_sweeper());
        }
        handles.push(mb_admin::spawn_retention(store.clone(), retention_days));
        handles.push(spawn_token_usage_retention(store));
        Self { _handles: handles }
    }
}

/// How often [`spawn_token_usage_retention`] runs.
const TOKEN_USAGE_PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Deletes token usage of days more than [`TOKEN_USAGE_RETENTION_DAYS`]
/// before today (UTC) now, then every 24 hours.
fn spawn_token_usage_retention(store: Arc<dyn ControlStore>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TOKEN_USAGE_PRUNE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let today = chrono::Utc::now().date_naive();
            let Some(cutoff) =
                today.checked_sub_days(chrono::Days::new(TOKEN_USAGE_RETENTION_DAYS))
            else {
                continue;
            };
            match store.prune_token_usage(cutoff).await {
                Ok(n) => tracing::debug!(pruned = n, %cutoff, "token usage retention ran"),
                Err(e) => tracing::warn!(error = %e, "token usage retention failed"),
            }
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use mb_state::{MemoryStore, SessionKey, StoredResource};

    #[tokio::test]
    async fn session_counts_label_each_mock() {
        let state = MemoryStore::new(Duration::from_secs(60));
        for (workspace, mock, session) in [
            ("default", "orders", "a"),
            ("default", "orders", "b"),
            ("team", "pets", "a"),
        ] {
            let key = SessionKey {
                workspace: workspace.into(),
                mock: mock.into(),
                session: session.into(),
            };
            let resource = StoredResource {
                resource_type: "thing".into(),
                id: "1".into(),
                data: serde_json::json!({}),
            };
            state.upsert(&key, resource).await.expect("upsert");
        }
        let mocks = [
            ("default".to_string(), "orders".to_string()),
            ("team".to_string(), "pets".to_string()),
            ("default".to_string(), "empty".to_string()),
        ];
        assert_eq!(
            session_counts(&state, &mocks).await,
            [
                ("orders".to_string(), 2),
                ("team/pets".to_string(), 1),
                ("empty".to_string(), 0),
            ]
        );
    }

    #[tokio::test]
    async fn maintenance_prunes_old_token_usage() {
        let store = mb_store::connect("sqlite::memory:").await.expect("store");
        let mock = store
            .create_mock(
                mb_store::DEFAULT_WORKSPACE,
                mb_store::NewMock {
                    name: "tokens".into(),
                    route_prefix: None,
                    persona: None,
                    llm_profile: None,
                    header_overrides: false,
                    source: mb_store::ResourceSource::Api,
                    data_plane_auth: Default::default(),
                    chaos: None,
                    limits: Default::default(),
                },
            )
            .await
            .expect("mock");
        let today = chrono::Utc::now().date_naive();
        let expired = today - chrono::Days::new(TOKEN_USAGE_RETENTION_DAYS + 1);
        let last_kept = today - chrono::Days::new(TOKEN_USAGE_RETENTION_DAYS);
        for day in [expired, last_kept, today] {
            store.add_token_usage(mock.id, day, 5).await.expect("add");
        }
        let _jobs = Maintenance::start(store.clone(), None, 365);
        let mut pruned = false;
        for _ in 0..100 {
            if store.token_usage(mock.id, expired).await.expect("usage") == 0 {
                pruned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(pruned, "usage older than the retention is pruned");
        assert_eq!(
            store.token_usage(mock.id, last_kept).await.expect("usage"),
            5
        );
        assert_eq!(store.token_usage(mock.id, today).await.expect("usage"), 5);
    }

    #[test]
    fn vanished_mocks_are_dropped_from_the_labels() {
        let first = set_state_gauges(vec![("a".into(), 1), ("b".into(), 2)], BTreeSet::new());
        let second = set_state_gauges(vec![("b".into(), 3)], first);
        assert_eq!(second.into_iter().collect::<Vec<_>>(), ["b"]);
    }
}
