//! SQL [`StateStore`] over SQLite or Postgres.
//!
//! Timestamps are Unix epoch milliseconds (wall clock). Each session row keeps
//! a counter that hands every upserted resource a recency sequence, which
//! orders `list` (newest first) and `export` (oldest first). Resource data is
//! TEXT holding JSON on SQLite and JSONB on Postgres. Reads check that the
//! session is live but never refresh it; `touch` and writes do.

use std::str::FromStr;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePoolOptions};
use sqlx::types::Json;
use sqlx::{ConnectOptions, PgPool, Row, SqlitePool};
use tokio::sync::broadcast;

use crate::{
    CHANGE_CHANNEL_CAPACITY, ChangeKind, SWEEP_INTERVAL, SessionInfo, SessionKey, StateChanged,
    StateError, StateStore, StoredResource,
};

/// Migrations table name, distinct from other crates' migrators that may share
/// the same database.
const MIGRATIONS_TABLE: &str = "_mb_state_migrations";

/// How long a call waits for a pooled connection before failing, so an
/// unreachable database fails fast instead of stalling every request.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(2);

const KEY_WHERE: &str = "workspace = $1 AND mock = $2 AND session = $3";

/// Limits a `state_resources` query keyed by [`KEY_WHERE`] to a live
/// session; `{cutoff}` is replaced with the cutoff's placeholder.
const LIVE_SESSION: &str = "EXISTS (SELECT 1 FROM state_sessions ss \
     WHERE ss.workspace = $1 AND ss.mock = $2 AND ss.session = $3 \
     AND ss.last_touched > {cutoff})";

/// [`LIVE_SESSION`] with its cutoff bound to `$N`.
fn live_session(n: u8) -> String {
    LIVE_SESSION.replace("{cutoff}", &format!("${n}"))
}

impl From<sqlx::Error> for StateError {
    fn from(e: sqlx::Error) -> Self {
        StateError::Backend(e.to_string())
    }
}

impl From<sqlx::migrate::MigrateError> for StateError {
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        StateError::Backend(format!("migration: {e}"))
    }
}

enum Pool {
    Sqlite(SqlitePool),
    Pg(PgPool),
}

/// Evaluates `$body` with `$p` bound to whichever pool backs the store. The
/// body is type-checked once per backend; the SQL must be portable (`$N`
/// placeholders work on both).
macro_rules! on_pool {
    ($pool:expr, |$p:ident| $body:expr) => {
        match $pool {
            Pool::Sqlite($p) => $body,
            Pool::Pg($p) => $body,
        }
    };
}

/// Builds a [`StoredResource`] from a row with `resource_type`, `id` and `data`.
macro_rules! resource_from_row {
    ($row:expr) => {{
        let row = $row;
        let Json(data): Json<Value> = row.try_get("data")?;
        StoredResource {
            resource_type: row.try_get("resource_type")?,
            id: row.try_get("id")?,
            data,
        }
    }};
}

struct Inner {
    pool: Pool,
    ttl_ms: i64,
    /// See [`Keeper`].
    _keeper: Option<Keeper>,
}

/// For an in-memory SQLite database: a connection outside the pool, never
/// used for queries, that keeps the shared-cache database alive for the
/// store's whole lifetime. The pool closes a connection that a cancelled
/// query left unusable and opens a replacement; without this one the last
/// connection would close and take the database with it, and the
/// replacement would open a fresh, empty one.
struct Keeper {
    _conn: std::sync::Mutex<SqliteConnection>,
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl Inner {
    /// Sessions touched at or before this instant are expired.
    fn cutoff(&self, now: i64) -> i64 {
        now.saturating_sub(self.ttl_ms)
    }

    /// Refreshes a live session. `false` if it is unknown or expired; never
    /// creates one.
    async fn touch_existing(&self, s: &SessionKey) -> Result<bool, StateError> {
        let now = now_ms();
        let sql = format!(
            "UPDATE state_sessions SET last_touched = $4 WHERE {KEY_WHERE} AND last_touched > $5"
        );
        let n = on_pool!(&self.pool, |p| sqlx::query(sqlx::AssertSqlSafe(
            sql.as_str()
        ))
        .bind(&s.workspace)
        .bind(&s.mock)
        .bind(&s.session)
        .bind(now)
        .bind(self.cutoff(now))
        .execute(p)
        .await?
        .rows_affected());
        Ok(n > 0)
    }

    async fn sweep(&self) -> Result<u64, StateError> {
        let cutoff = self.cutoff(now_ms());
        Ok(on_pool!(&self.pool, |p| sqlx::query(
            "DELETE FROM state_sessions WHERE last_touched <= $1"
        )
        .bind(cutoff)
        .execute(p)
        .await?
        .rows_affected()))
    }
}

pub struct SqlStateStore {
    inner: Arc<Inner>,
    tx: broadcast::Sender<StateChanged>,
}

impl SqlStateStore {
    /// Opens `sqlite::memory:`, `sqlite://<path>` (created if missing) or
    /// `postgres://…`, runs migrations, and sweeps expired sessions every
    /// 60 s while the store lives.
    pub async fn connect(url: &str, ttl: Duration) -> Result<Self, StateError> {
        let (pool, keeper) = if url.starts_with("sqlite:") {
            let (pool, keeper) = connect_sqlite(url).await?;
            (Pool::Sqlite(pool), keeper)
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            (Pool::Pg(connect_postgres(url).await?), None)
        } else {
            return Err(StateError::Backend(format!(
                "unsupported state url scheme (expected memory, sqlite:, postgres://, redis:// or rediss://): {}",
                url.split(':').next().unwrap_or_default()
            )));
        };
        Ok(Self::from_pool(pool, keeper, ttl))
    }

    /// A Postgres store whose pool connects on first use and which runs no
    /// migrations (the schema must already exist). Unlike [`Self::connect`]
    /// it succeeds while the database is unreachable.
    pub fn connect_lazy(url: &str, ttl: Duration) -> Result<Self, StateError> {
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            return Err(StateError::Backend(
                "connect_lazy supports postgres:// urls only".into(),
            ));
        }
        let options = PgConnectOptions::from_str(url)
            .map_err(|e| StateError::Backend(format!("postgres url: {e}")))?;
        let pool = PgPoolOptions::new()
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_lazy_with(options);
        Ok(Self::from_pool(Pool::Pg(pool), None, ttl))
    }

    fn from_pool(pool: Pool, keeper: Option<Keeper>, ttl: Duration) -> Self {
        let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
        let inner = Arc::new(Inner {
            pool,
            ttl_ms,
            _keeper: keeper,
        });
        spawn_sweeper(Arc::downgrade(&inner));
        let (tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Self { inner, tx }
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

async fn connect_sqlite(url: &str) -> Result<(SqlitePool, Option<Keeper>), StateError> {
    let options = SqliteConnectOptions::from_str(url)
        .map_err(|e| StateError::Backend(format!("sqlite url: {e}")))?
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let in_memory = url.contains(":memory:") || url.contains("mode=memory");
    let (pool, keeper) = if in_memory {
        // A named shared-cache database (sqlx names `:memory:` uniquely),
        // held open by the keeper so pool connections may come and go.
        let options = options.shared_cache(true);
        let keeper = Keeper {
            _conn: std::sync::Mutex::new(options.connect().await?),
        };
        // One pooled connection that never expires, so writers are
        // serialised as before.
        let pool = SqlitePoolOptions::new()
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        (pool, Some(keeper))
    } else {
        let pool = SqlitePoolOptions::new()
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_with(options.journal_mode(SqliteJournalMode::Wal))
            .await?;
        (pool, None)
    };
    let mut migrator = sqlx::migrate!("./migrations/sqlite");
    migrator.dangerous_set_table_name(MIGRATIONS_TABLE);
    migrator.run(&pool).await?;
    Ok((pool, keeper))
}

async fn connect_postgres(url: &str) -> Result<PgPool, StateError> {
    let options = PgConnectOptions::from_str(url)
        .map_err(|e| StateError::Backend(format!("postgres url: {e}")))?;
    let pool = PgPoolOptions::new()
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await?;
    // Runs under Postgres's advisory lock.
    let mut migrator = sqlx::migrate!("./migrations/postgres");
    migrator.dangerous_set_table_name(MIGRATIONS_TABLE);
    migrator.run(&pool).await?;
    Ok(pool)
}

fn spawn_sweeper(weak: Weak<Inner>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(inner) = weak.upgrade() else { break };
            if let Err(e) = inner.sweep().await {
                tracing::warn!(error = %e, "state session sweep failed");
            }
        }
    });
}

#[async_trait]
impl StateStore for SqlStateStore {
    async fn get(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        let sql = format!(
            "SELECT resource_type, id, data FROM state_resources \
             WHERE {KEY_WHERE} AND resource_type = $4 AND id = $5 AND {}",
            live_session(6)
        );
        let cutoff = self.inner.cutoff(now_ms());
        Ok(on_pool!(&self.inner.pool, |p| {
            let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(&s.workspace)
                .bind(&s.mock)
                .bind(&s.session)
                .bind(resource_type)
                .bind(id)
                .bind(cutoff)
                .fetch_optional(p)
                .await?;
            match row {
                Some(row) => Some(resource_from_row!(&row)),
                None => None,
            }
        }))
    }

    async fn list(
        &self,
        s: &SessionKey,
        resource_type: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        let sql = format!(
            "SELECT resource_type, id, data FROM state_resources \
             WHERE {KEY_WHERE} AND resource_type = $4 AND {} ORDER BY seq DESC LIMIT $5",
            live_session(6)
        );
        let cutoff = self.inner.cutoff(now_ms());
        Ok(on_pool!(&self.inner.pool, |p| {
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(&s.workspace)
                .bind(&s.mock)
                .bind(&s.session)
                .bind(resource_type)
                .bind(to_i64(limit))
                .bind(cutoff)
                .fetch_all(p)
                .await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                out.push(resource_from_row!(row));
            }
            out
        }))
    }

    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        let now = now_ms();
        let cutoff = self.inner.cutoff(now);
        let purge = format!("DELETE FROM state_sessions WHERE {KEY_WHERE} AND last_touched <= $4");
        on_pool!(&self.inner.pool, |p| {
            let mut tx = p.begin().await?;
            // An expired session restarts empty.
            sqlx::query(sqlx::AssertSqlSafe(purge.as_str()))
                .bind(&s.workspace)
                .bind(&s.mock)
                .bind(&s.session)
                .bind(cutoff)
                .execute(&mut *tx)
                .await?;
            let seq: i64 = sqlx::query_scalar(
                "INSERT INTO state_sessions (workspace, mock, session, last_touched, seq) \
                 VALUES ($1, $2, $3, $4, 1) \
                 ON CONFLICT (workspace, mock, session) DO UPDATE \
                 SET last_touched = excluded.last_touched, seq = state_sessions.seq + 1 \
                 RETURNING seq",
            )
            .bind(&s.workspace)
            .bind(&s.mock)
            .bind(&s.session)
            .bind(now)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO state_resources \
                 (workspace, mock, session, resource_type, id, seq, data) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (workspace, mock, session, resource_type, id) DO UPDATE \
                 SET seq = excluded.seq, data = excluded.data",
            )
            .bind(&s.workspace)
            .bind(&s.mock)
            .bind(&s.session)
            .bind(&r.resource_type)
            .bind(&r.id)
            .bind(seq)
            .bind(Json(&r.data))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        });
        self.publish(s, &r.resource_type, &r.id, ChangeKind::Upserted);
        Ok(())
    }

    async fn delete(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<bool, StateError> {
        if !self.inner.touch_existing(s).await? {
            return Ok(false);
        }
        let sql = format!(
            "DELETE FROM state_resources WHERE {KEY_WHERE} AND resource_type = $4 AND id = $5"
        );
        let n = on_pool!(&self.inner.pool, |p| sqlx::query(sqlx::AssertSqlSafe(
            sql.as_str()
        ))
        .bind(&s.workspace)
        .bind(&s.mock)
        .bind(&s.session)
        .bind(resource_type)
        .bind(id)
        .execute(p)
        .await?
        .rows_affected());
        let removed = n > 0;
        if removed {
            self.publish(s, resource_type, id, ChangeKind::Deleted);
        }
        Ok(removed)
    }

    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        let sql = format!("DELETE FROM state_sessions WHERE {KEY_WHERE}");
        on_pool!(&self.inner.pool, |p| {
            sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(&s.workspace)
                .bind(&s.mock)
                .bind(&s.session)
                .execute(p)
                .await?;
        });
        Ok(())
    }

    async fn reset_mock(&self, workspace: &str, mock: &str) -> Result<(), StateError> {
        on_pool!(&self.inner.pool, |p| {
            sqlx::query("DELETE FROM state_sessions WHERE workspace = $1 AND mock = $2")
                .bind(workspace)
                .bind(mock)
                .execute(p)
                .await?;
        });
        Ok(())
    }

    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.touch_existing(s).await.map(|_| ())
    }

    async fn session_count(&self, workspace: &str, mock: &str) -> Result<usize, StateError> {
        let cutoff = self.inner.cutoff(now_ms());
        let n: i64 = on_pool!(&self.inner.pool, |p| sqlx::query_scalar(
            "SELECT COUNT(*) FROM state_sessions \
             WHERE workspace = $1 AND mock = $2 AND last_touched > $3"
        )
        .bind(workspace)
        .bind(mock)
        .bind(cutoff)
        .fetch_one(p)
        .await?);
        usize::try_from(n)
            .map_err(|_| StateError::Backend(format!("session count {n} is out of range")))
    }

    async fn sessions(&self, workspace: &str, mock: &str) -> Result<Vec<SessionInfo>, StateError> {
        let cutoff = self.inner.cutoff(now_ms());
        let sql = "SELECT s.session AS session, s.last_touched AS last_touched, \
                   COUNT(r.id) AS resources \
                   FROM state_sessions s LEFT JOIN state_resources r \
                   ON r.workspace = s.workspace AND r.mock = s.mock AND r.session = s.session \
                   WHERE s.workspace = $1 AND s.mock = $2 AND s.last_touched > $3 \
                   GROUP BY s.session, s.last_touched \
                   ORDER BY s.last_touched DESC, s.session";
        let raw: Vec<(String, i64, i64)> = on_pool!(&self.inner.pool, |p| {
            let rows = sqlx::query(sql)
                .bind(workspace)
                .bind(mock)
                .bind(cutoff)
                .fetch_all(p)
                .await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                out.push((
                    row.try_get("session")?,
                    row.try_get("last_touched")?,
                    row.try_get("resources")?,
                ));
            }
            out
        });
        raw.into_iter()
            .map(|(session, last_touched, resources)| {
                Ok(SessionInfo {
                    session,
                    resources: usize::try_from(resources).map_err(|_| {
                        StateError::Backend(format!("resource count {resources} is out of range"))
                    })?,
                    last_touched: DateTime::from_timestamp_millis(last_touched).ok_or_else(
                        || StateError::Backend(format!("stored timestamp {last_touched}")),
                    )?,
                })
            })
            .collect()
    }

    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        let sql = format!(
            "SELECT resource_type, id, data FROM state_resources \
             WHERE {KEY_WHERE} AND {} ORDER BY seq ASC",
            live_session(4)
        );
        let cutoff = self.inner.cutoff(now_ms());
        Ok(on_pool!(&self.inner.pool, |p| {
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(&s.workspace)
                .bind(&s.mock)
                .bind(&s.session)
                .bind(cutoff)
                .fetch_all(p)
                .await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                out.push(resource_from_row!(row));
            }
            out
        }))
    }

    async fn seed(&self, s: &SessionKey, resources: Vec<StoredResource>) -> Result<(), StateError> {
        let now = now_ms();
        let clear = format!("DELETE FROM state_sessions WHERE {KEY_WHERE}");
        on_pool!(&self.inner.pool, |p| {
            let mut tx = p.begin().await?;
            sqlx::query(sqlx::AssertSqlSafe(clear.as_str()))
                .bind(&s.workspace)
                .bind(&s.mock)
                .bind(&s.session)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO state_sessions (workspace, mock, session, last_touched, seq) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&s.workspace)
            .bind(&s.mock)
            .bind(&s.session)
            .bind(now)
            .bind(to_i64(resources.len()))
            .execute(&mut *tx)
            .await?;
            for (i, r) in resources.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO state_resources \
                     (workspace, mock, session, resource_type, id, seq, data) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7) \
                     ON CONFLICT (workspace, mock, session, resource_type, id) DO UPDATE \
                     SET seq = excluded.seq, data = excluded.data",
                )
                .bind(&s.workspace)
                .bind(&s.mock)
                .bind(&s.session)
                .bind(&r.resource_type)
                .bind(&r.id)
                .bind(to_i64(i).saturating_add(1))
                .bind(Json(&r.data))
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
        });
        Ok(())
    }

    async fn ping(&self) -> Result<(), StateError> {
        on_pool!(&self.inner.pool, |p| {
            sqlx::query("SELECT 1").execute(p).await?;
        });
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.tx.subscribe()
    }
}
