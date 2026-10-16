//! Leader election among control nodes.
//!
//! A [`Leadership`] keeps trying to become (and stay) the single holder of a
//! named lock and publishes its current state on a `watch` channel:
//! - [`LeaderBackend::Always`]: a single node, always leader;
//! - [`LeaderBackend::Postgres`]: a session-level advisory lock on a dedicated
//!   connection, held for as long as that connection lives;
//! - [`LeaderBackend::Redis`]: a lease key renewed every
//!   [`LEADER_RENEW_INTERVAL`] and expiring after [`LEADER_LEASE_TTL`].
//!
//! A node that loses leadership skips its next attempt, so another contender
//! gets the first chance to take over.

use std::time::Duration;

use redis::Script;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use sha2::{Digest, Sha256};
use sqlx::Connection;
use sqlx::postgres::PgConnection;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::StateError;

pub const LEADER_LEASE_TTL: Duration = Duration::from_secs(15);
pub const LEADER_RENEW_INTERVAL: Duration = Duration::from_secs(5);

const LEADER_METRIC: &str = "mb_leader";

/// Where the lock lives.
pub enum LeaderBackend {
    /// No contention: this node is always the leader.
    Always,
    /// A Postgres URL.
    Postgres(String),
    /// A `redis://` or `rediss://` URL.
    Redis(String),
}

/// Renewal and lease timings; [`Default`] gives the production values.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct LeaderTimings {
    /// How often a leader renews and a follower retries.
    pub renew_interval: Duration,
    /// Redis lease lifetime.
    pub lease_ttl: Duration,
}

impl Default for LeaderTimings {
    fn default() -> Self {
        Self {
            renew_interval: LEADER_RENEW_INTERVAL,
            lease_ttl: LEADER_LEASE_TTL,
        }
    }
}

/// The Postgres advisory-lock key of lock `name`: the first 8 bytes
/// (big-endian) of sha256(`"mockingbird:<name>"`).
pub fn pg_advisory_key(name: &str) -> i64 {
    let digest = Sha256::digest(format!("mockingbird:{name}").as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(bytes)
}

/// The Redis lease key of lock `name`.
fn redis_key(name: &str) -> String {
    format!("mockingbird:leader:{name}")
}

/// This node's claim on a named lock. Dropping it stops contending and
/// releases the lock.
pub struct Leadership {
    rx: watch::Receiver<bool>,
    /// Keeps the channel open for [`LeaderBackend::Always`].
    _always: Option<watch::Sender<bool>>,
    task: Option<JoinHandle<()>>,
    /// Redis lease to release on drop.
    release: Option<RedisLease>,
}

impl Leadership {
    /// Makes the first attempt before returning, so a lone node is leader on return.
    pub async fn start(
        backend: LeaderBackend,
        name: &str,
        node_id: String,
    ) -> Result<Self, StateError> {
        Self::start_with(backend, name, node_id, LeaderTimings::default()).await
    }

    /// [`Leadership::start`] with custom timings (tests).
    #[doc(hidden)]
    pub async fn start_with(
        backend: LeaderBackend,
        name: &str,
        node_id: String,
        timings: LeaderTimings,
    ) -> Result<Self, StateError> {
        let mut lock = match backend {
            LeaderBackend::Always => {
                metrics::gauge!(LEADER_METRIC).set(1.0);
                let (tx, rx) = watch::channel(true);
                return Ok(Self {
                    rx,
                    _always: Some(tx),
                    task: None,
                    release: None,
                });
            }
            LeaderBackend::Postgres(url) => Lock::Postgres(PgLock::connect(url, name).await?),
            LeaderBackend::Redis(url) => {
                Lock::Redis(RedisLease::connect(&url, name, node_id, timings).await?)
            }
        };
        let op_timeout = timings.renew_interval;
        let leading = lock.try_acquire(op_timeout).await;
        metrics::gauge!(LEADER_METRIC).set(if leading { 1.0 } else { 0.0 });
        if leading {
            tracing::info!(lock = %name, "became leader");
        }
        let release = match &lock {
            Lock::Redis(lease) => Some(lease.clone()),
            Lock::Postgres(_) => None,
        };
        let (tx, rx) = watch::channel(leading);
        let task = tokio::spawn(run(lock, tx, name.to_owned(), timings.renew_interval));
        Ok(Self {
            rx,
            _always: None,
            task: Some(task),
            release,
        })
    }

    pub fn is_leader(&self) -> bool {
        *self.rx.borrow()
    }

    pub fn watch(&self) -> watch::Receiver<bool> {
        self.rx.clone()
    }
}

impl Drop for Leadership {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Drops the Postgres connection, which ends the session and with
            // it the advisory lock.
            task.abort();
        }
        if self.is_leader() {
            metrics::gauge!(LEADER_METRIC).set(0.0);
        }
        if let Some(lease) = self.release.take()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            rt.spawn(async move { lease.release().await });
        }
    }
}

/// Renews while leading, retries while not; publishes every change.
async fn run(mut lock: Lock, tx: watch::Sender<bool>, name: String, interval: Duration) {
    let mut leading = *tx.borrow();
    loop {
        tokio::time::sleep(interval).await;
        if leading {
            if !lock.renew(interval).await {
                leading = false;
                tracing::warn!(lock = %name, "lost leadership");
                metrics::gauge!(LEADER_METRIC).set(0.0);
                let _ = tx.send(false);
                // Give the other contenders the first chance to take over.
                tokio::time::sleep(interval).await;
            }
        } else if lock.try_acquire(interval).await {
            leading = true;
            tracing::info!(lock = %name, "became leader");
            metrics::gauge!(LEADER_METRIC).set(1.0);
            let _ = tx.send(true);
        }
    }
}

enum Lock {
    Postgres(PgLock),
    Redis(RedisLease),
}

impl Lock {
    async fn try_acquire(&mut self, op_timeout: Duration) -> bool {
        let result = match self {
            Lock::Postgres(l) => bounded(op_timeout, l.try_acquire(op_timeout)).await,
            Lock::Redis(l) => bounded(op_timeout, l.try_acquire()).await,
        };
        result.unwrap_or_else(|e| {
            self.drop_connection();
            tracing::warn!(error = %e, "leader acquire attempt failed");
            false
        })
    }

    /// After a failed or interrupted Postgres call the session's state is
    /// unknown: drop it (releasing anything it held) and start afresh.
    fn drop_connection(&mut self) {
        if let Lock::Postgres(l) = self {
            l.conn = None;
        }
    }

    async fn renew(&mut self, op_timeout: Duration) -> bool {
        let result = match self {
            Lock::Postgres(l) => bounded(op_timeout, l.renew()).await,
            Lock::Redis(l) => bounded(op_timeout, l.renew()).await,
        };
        match result {
            Ok(held) => held,
            Err(e) => {
                self.drop_connection();
                tracing::warn!(error = %e, "leader renewal failed");
                false
            }
        }
    }
}

async fn bounded<T>(
    limit: Duration,
    fut: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(limit, fut)
        .await
        .unwrap_or_else(|_| Err("timed out".into()))
}

/// A session-level advisory lock on a dedicated connection.
struct PgLock {
    url: String,
    key: i64,
    conn: Option<PgConnection>,
}

impl PgLock {
    /// Connects once so a bad URL or unreachable server fails at startup.
    async fn connect(url: String, name: &str) -> Result<Self, StateError> {
        let conn = PgConnection::connect(&url)
            .await
            .map_err(|e| StateError::Backend(format!("leader: postgres: {e}")))?;
        Ok(Self {
            url,
            key: pg_advisory_key(name),
            conn: Some(conn),
        })
    }

    async fn try_acquire(&mut self, op_timeout: Duration) -> Result<bool, String> {
        if self.conn.is_none() {
            let conn = tokio::time::timeout(op_timeout, PgConnection::connect(&self.url))
                .await
                .map_err(|_| "postgres: connect timed out".to_owned())?
                .map_err(|e| format!("postgres: {e}"))?;
            self.conn = Some(conn);
        }
        let Some(conn) = self.conn.as_mut() else {
            return Ok(false);
        };
        sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(self.key)
            .fetch_one(conn)
            .await
            .map_err(|e| format!("postgres: {e}"))
    }

    /// The lock lives as long as the session: a live connection means it is
    /// still held. On failure the caller drops the connection (and so the lock).
    async fn renew(&mut self) -> Result<bool, String> {
        let Some(conn) = self.conn.as_mut() else {
            return Ok(false);
        };
        sqlx::query("SELECT 1")
            .execute(conn)
            .await
            .map(|_| true)
            .map_err(|e| format!("postgres: {e}"))
    }
}

/// `1` if `KEYS[1]` holds `ARGV[1]` and its expiry was reset to `ARGV[2]` ms.
const RENEW_LUA: &str = "if redis.call('GET', KEYS[1]) == ARGV[1] then \
     return redis.call('PEXPIRE', KEYS[1], ARGV[2]) else return 0 end";
/// `1` if `KEYS[1]` held `ARGV[1]` and was deleted.
const RELEASE_LUA: &str = "if redis.call('GET', KEYS[1]) == ARGV[1] then \
     return redis.call('DEL', KEYS[1]) else return 0 end";

/// A Redis lease key held while it contains this node's id.
#[derive(Clone)]
struct RedisLease {
    con: ConnectionManager,
    key: String,
    node_id: String,
    ttl_ms: u64,
}

impl RedisLease {
    async fn connect(
        url: &str,
        name: &str,
        node_id: String,
        timings: LeaderTimings,
    ) -> Result<Self, StateError> {
        if !(url.starts_with("redis://") || url.starts_with("rediss://")) {
            return Err(StateError::Backend(
                "leader: redis url must start with redis:// or rediss://".into(),
            ));
        }
        let client = redis::Client::open(url)?;
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(timings.renew_interval))
            .set_response_timeout(Some(timings.renew_interval))
            .set_number_of_retries(1);
        let con = client.get_connection_manager_with_config(config).await?;
        Ok(Self {
            con,
            key: redis_key(name),
            node_id,
            ttl_ms: u64::try_from(timings.lease_ttl.as_millis())
                .unwrap_or(u64::MAX)
                .max(1),
        })
    }

    async fn try_acquire(&mut self) -> Result<bool, String> {
        let reply: Option<String> = redis::cmd("SET")
            .arg(&self.key)
            .arg(&self.node_id)
            .arg("NX")
            .arg("PX")
            .arg(self.ttl_ms)
            .query_async(&mut self.con)
            .await
            .map_err(|e| format!("redis: {e}"))?;
        Ok(reply.is_some())
    }

    /// `false` when the key is gone or held by another node.
    async fn renew(&mut self) -> Result<bool, String> {
        let renewed: i64 = Script::new(RENEW_LUA)
            .key(&self.key)
            .arg(&self.node_id)
            .arg(self.ttl_ms)
            .invoke_async(&mut self.con)
            .await
            .map_err(|e| format!("redis: {e}"))?;
        Ok(renewed == 1)
    }

    async fn release(mut self) {
        let result: Result<i64, _> = Script::new(RELEASE_LUA)
            .key(&self.key)
            .arg(&self.node_id)
            .invoke_async(&mut self.con)
            .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, "releasing leader lease failed");
        }
    }
}
