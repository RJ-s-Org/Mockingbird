//! Redis [`StateStore`].
//!
//! Key layout, with `{s}` = `mb:{workspace}:{mock}:{session}` (names are
//! restricted to `[A-Za-z0-9._-]`, so `:` is a safe separator):
//!
//! - `{s}:res`: hash, field `{resource_type}\x1f{id}` → JSON [`StoredResource`].
//! - `{s}:order`: sorted set of the same fields, scored by the global
//!   counter `mb:seq`; it orders `list` (newest first) and `export` (oldest
//!   first).
//! - `{s}:meta`: hash with `last_touched` (RFC 3339). Its presence is what
//!   makes a session live.
//! - `mb:{workspace}:{mock}:sessions`: sorted set of session ids scored by
//!   `last_touched` in Unix milliseconds.
//!
//! Every write and every touch is applied atomically (a `MULTI` pipeline, or
//! a Lua script where the write is conditional on the session being live)
//! and `PEXPIRE`s the session's keys and the mock's session index to the
//! TTL, so Redis itself expires idle sessions.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{Pipeline, RedisError, Script};
use tokio::sync::broadcast;

use crate::{
    CHANGE_CHANNEL_CAPACITY, ChangeKind, SessionInfo, SessionKey, StateChanged, StateError,
    StateStore, StoredResource,
};

/// Prefix of every key this store writes.
const KEY_PREFIX: &str = "mb:";
/// Global recency counter.
const SEQ_KEY: &str = "mb:seq";
/// Separates resource type and id in hash fields and sorted-set members.
const FIELD_SEP: char = '\x1f';
const LAST_TOUCHED: &str = "last_touched";
/// Bound on `ping`.
const PING_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound on connecting and on each command.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Refreshes a live session: `meta`, its TTLs and its index entry.
/// KEYS: res, order, meta, sessions. ARGV: ttl_ms, last_touched, score_ms,
/// session id. Shared by [`TOUCH_SCRIPT`] and [`DELETE_SCRIPT`].
const REFRESH_LUA: &str = "
redis.call('HSET', KEYS[3], 'last_touched', ARGV[2])
redis.call('PEXPIRE', KEYS[1], ARGV[1])
redis.call('PEXPIRE', KEYS[2], ARGV[1])
redis.call('PEXPIRE', KEYS[3], ARGV[1])
redis.call('ZADD', KEYS[4], ARGV[3], ARGV[4])
redis.call('PEXPIRE', KEYS[4], ARGV[1])
";

/// Same KEYS/ARGV as [`REFRESH_LUA`]. Returns 1 if the session was live.
fn touch_script() -> Script {
    Script::new(&format!(
        "if redis.call('EXISTS', KEYS[3]) == 0 then return 0 end\n{REFRESH_LUA}\nreturn 1"
    ))
}

/// [`REFRESH_LUA`]'s KEYS/ARGV plus ARGV[5] = field. Returns 1 if the
/// resource existed; never creates a session.
fn delete_script() -> Script {
    Script::new(&format!(
        "if redis.call('EXISTS', KEYS[3]) == 0 then return 0 end\n\
         local removed = redis.call('HDEL', KEYS[1], ARGV[5])\n\
         redis.call('ZREM', KEYS[2], ARGV[5])\n\
         {REFRESH_LUA}\n\
         return removed"
    ))
}

impl From<redis::ParsingError> for StateError {
    fn from(e: redis::ParsingError) -> Self {
        StateError::Backend(format!("redis: unexpected reply: {e}"))
    }
}

impl From<RedisError> for StateError {
    fn from(e: RedisError) -> Self {
        StateError::Backend(format!("redis: {e}"))
    }
}

/// The keys of one session.
struct Keys {
    res: String,
    order: String,
    meta: String,
    sessions: String,
}

fn mock_prefix(workspace: &str, mock: &str) -> String {
    format!("{KEY_PREFIX}{workspace}:{mock}:")
}

fn sessions_key(workspace: &str, mock: &str) -> String {
    format!("{}sessions", mock_prefix(workspace, mock))
}

fn session_keys(workspace: &str, mock: &str, session: &str) -> [String; 3] {
    let s = format!("{}{session}", mock_prefix(workspace, mock));
    [
        format!("{s}:res"),
        format!("{s}:order"),
        format!("{s}:meta"),
    ]
}

impl Keys {
    fn of(s: &SessionKey) -> Self {
        let [res, order, meta] = session_keys(&s.workspace, &s.mock, &s.session);
        Self {
            res,
            order,
            meta,
            sessions: sessions_key(&s.workspace, &s.mock),
        }
    }
}

fn field(resource_type: &str, id: &str) -> String {
    format!("{resource_type}{FIELD_SEP}{id}")
}

fn decode(json: &str) -> Result<StoredResource, StateError> {
    serde_json::from_str(json)
        .map_err(|e| StateError::Backend(format!("redis: corrupt stored resource: {e}")))
}

fn encode(r: &StoredResource) -> Result<String, StateError> {
    serde_json::to_string(r).map_err(|e| StateError::Backend(format!("encode resource: {e}")))
}

/// A wall-clock instant as both the RFC 3339 `last_touched` and the
/// millisecond index score.
struct Stamp {
    rfc3339: String,
    ms: i64,
}

fn stamp() -> Stamp {
    let now = Utc::now();
    Stamp {
        rfc3339: now.to_rfc3339_opts(SecondsFormat::Micros, true),
        ms: now.timestamp_millis(),
    }
}

pub struct RedisStateStore {
    con: ConnectionManager,
    ttl_ms: i64,
    tx: broadcast::Sender<StateChanged>,
    touch: Script,
    delete: Script,
}

impl RedisStateStore {
    /// Connects to `redis://…` or `rediss://…`. The connection is
    /// re-established automatically after a drop.
    pub async fn connect(url: &str, ttl: Duration) -> Result<Self, StateError> {
        if !(url.starts_with("redis://") || url.starts_with("rediss://")) {
            return Err(StateError::Backend(
                "redis state url must start with redis:// or rediss://".into(),
            ));
        }
        let client = redis::Client::open(url)?;
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(IO_TIMEOUT))
            .set_response_timeout(Some(IO_TIMEOUT))
            .set_number_of_retries(2);
        let con = client.get_connection_manager_with_config(config).await?;
        let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX).max(1);
        let (tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Ok(Self {
            con,
            ttl_ms,
            tx,
            touch: touch_script(),
            delete: delete_script(),
        })
    }

    fn con(&self) -> ConnectionManager {
        self.con.clone()
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

    /// Reserves `n` consecutive recency sequence numbers; returns the last.
    async fn next_seq(&self, n: usize) -> Result<i64, StateError> {
        let n = i64::try_from(n).unwrap_or(i64::MAX);
        Ok(redis::cmd("INCRBY")
            .arg(SEQ_KEY)
            .arg(n)
            .query_async(&mut self.con())
            .await?)
    }

    /// Appends to `pipe` the commands that mark the session live and
    /// refresh every TTL.
    fn refresh(&self, pipe: &mut Pipeline, k: &Keys, session: &str) {
        let now = stamp();
        pipe.cmd("HSET")
            .arg(&k.meta)
            .arg(LAST_TOUCHED)
            .arg(&now.rfc3339)
            .ignore();
        for key in [&k.res, &k.order, &k.meta] {
            pipe.cmd("PEXPIRE").arg(key).arg(self.ttl_ms).ignore();
        }
        pipe.cmd("ZADD")
            .arg(&k.sessions)
            .arg(now.ms)
            .arg(session)
            .ignore();
        pipe.cmd("PEXPIRE")
            .arg(&k.sessions)
            .arg(self.ttl_ms)
            .ignore();
    }

    /// Runs one of the conditional refresh scripts for `s`.
    async fn run_refresh_script(
        &self,
        script: &Script,
        s: &SessionKey,
        extra: Option<&str>,
    ) -> Result<i64, StateError> {
        let k = Keys::of(s);
        let now = stamp();
        let mut inv = script.prepare_invoke();
        inv.key(&k.res)
            .key(&k.order)
            .key(&k.meta)
            .key(&k.sessions)
            .arg(self.ttl_ms)
            .arg(&now.rfc3339)
            .arg(now.ms)
            .arg(&s.session);
        if let Some(extra) = extra {
            inv.arg(extra);
        }
        Ok(inv.invoke_async(&mut self.con()).await?)
    }
}

#[async_trait]
impl StateStore for RedisStateStore {
    async fn get(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        let k = Keys::of(s);
        let json: Option<String> = redis::cmd("HGET")
            .arg(&k.res)
            .arg(field(resource_type, id))
            .query_async(&mut self.con())
            .await?;
        json.as_deref().map(decode).transpose()
    }

    async fn list(
        &self,
        s: &SessionKey,
        resource_type: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let k = Keys::of(s);
        let prefix = field(resource_type, "");
        let members: Vec<String> = redis::cmd("ZREVRANGE")
            .arg(&k.order)
            .arg(0)
            .arg(-1)
            .query_async(&mut self.con())
            .await?;
        let fields: Vec<String> = members
            .into_iter()
            .filter(|m| m.starts_with(&prefix))
            .take(limit)
            .collect();
        if fields.is_empty() {
            return Ok(Vec::new());
        }
        let values: Vec<Option<String>> = redis::cmd("HMGET")
            .arg(&k.res)
            .arg(&fields)
            .query_async(&mut self.con())
            .await?;
        // A resource deleted between the two reads is simply skipped.
        values.iter().flatten().map(|j| decode(j)).collect()
    }

    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        let k = Keys::of(s);
        let f = field(&r.resource_type, &r.id);
        let json = encode(&r)?;
        let seq = self.next_seq(1).await?;
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.cmd("HSET").arg(&k.res).arg(&f).arg(json).ignore();
        pipe.cmd("ZADD").arg(&k.order).arg(seq).arg(&f).ignore();
        self.refresh(&mut pipe, &k, &s.session);
        pipe.query_async::<()>(&mut self.con()).await?;
        self.publish(s, &r.resource_type, &r.id, ChangeKind::Upserted);
        Ok(())
    }

    async fn delete(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<bool, StateError> {
        let removed = self
            .run_refresh_script(&self.delete, s, Some(&field(resource_type, id)))
            .await?
            > 0;
        if removed {
            self.publish(s, resource_type, id, ChangeKind::Deleted);
        }
        Ok(removed)
    }

    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        let k = Keys::of(s);
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.cmd("DEL")
            .arg(&k.res)
            .arg(&k.order)
            .arg(&k.meta)
            .ignore();
        pipe.cmd("ZREM").arg(&k.sessions).arg(&s.session).ignore();
        pipe.query_async::<()>(&mut self.con()).await?;
        Ok(())
    }

    async fn reset_mock(&self, workspace: &str, mock: &str) -> Result<(), StateError> {
        let index = sessions_key(workspace, mock);
        let sessions: Vec<String> = redis::cmd("ZRANGE")
            .arg(&index)
            .arg(0)
            .arg(-1)
            .query_async(&mut self.con())
            .await?;
        let mut pipe = redis::pipe();
        pipe.atomic();
        for chunk in sessions.chunks(256) {
            let del = pipe.cmd("DEL");
            for session in chunk {
                for key in session_keys(workspace, mock, session) {
                    del.arg(key);
                }
            }
            del.ignore();
        }
        pipe.cmd("DEL").arg(&index).ignore();
        pipe.query_async::<()>(&mut self.con()).await?;
        Ok(())
    }

    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.run_refresh_script(&self.touch, s, None).await?;
        Ok(())
    }

    /// Like [`StateStore::sessions`], without reading each session's
    /// resource count and timestamp: only whether its meta key still exists.
    async fn session_count(&self, workspace: &str, mock: &str) -> Result<usize, StateError> {
        let index = sessions_key(workspace, mock);
        let cutoff = Utc::now().timestamp_millis().saturating_sub(self.ttl_ms);
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.cmd("ZREMRANGEBYSCORE")
            .arg(&index)
            .arg("-inf")
            .arg(cutoff)
            .ignore();
        pipe.cmd("ZRANGE").arg(&index).arg(0).arg(-1);
        let (listed,): (Vec<String>,) = pipe.query_async(&mut self.con()).await?;
        if listed.is_empty() {
            return Ok(0);
        }
        let mut pipe = redis::pipe();
        for session in &listed {
            let [_, _, meta] = session_keys(workspace, mock, session);
            pipe.cmd("EXISTS").arg(meta);
        }
        let exists: Vec<bool> = pipe.query_async(&mut self.con()).await?;
        Ok(exists.into_iter().filter(|e| *e).count())
    }

    async fn sessions(&self, workspace: &str, mock: &str) -> Result<Vec<SessionInfo>, StateError> {
        let index = sessions_key(workspace, mock);
        let cutoff = Utc::now().timestamp_millis().saturating_sub(self.ttl_ms);
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.cmd("ZREMRANGEBYSCORE")
            .arg(&index)
            .arg("-inf")
            .arg(cutoff)
            .ignore();
        pipe.cmd("ZREVRANGE")
            .arg(&index)
            .arg(0)
            .arg(-1)
            .arg("WITHSCORES");
        let (listed,): (Vec<(String, f64)>,) = pipe.query_async(&mut self.con()).await?;
        if listed.is_empty() {
            return Ok(Vec::new());
        }
        let mut pipe = redis::pipe();
        for (session, _) in &listed {
            let [res, _, meta] = session_keys(workspace, mock, session);
            pipe.cmd("HLEN").arg(res);
            pipe.cmd("HGET").arg(meta).arg(LAST_TOUCHED);
        }
        let details: Vec<redis::Value> = pipe.query_async(&mut self.con()).await?;
        let mut live: Vec<(f64, SessionInfo)> = Vec::with_capacity(listed.len());
        for ((session, score), pair) in listed.into_iter().zip(details.chunks(2)) {
            let [count, touched] = pair else { continue };
            let touched: Option<String> = redis::from_redis_value_ref(touched)?;
            // No meta: the session expired after the index was read.
            let Some(touched) = touched else { continue };
            let resources: usize = redis::from_redis_value_ref(count)?;
            let last_touched = DateTime::parse_from_rfc3339(&touched)
                .map(|t| t.with_timezone(&Utc))
                .ok()
                .or_else(|| Utc.timestamp_millis_opt(score as i64).single())
                .unwrap_or_default();
            live.push((
                score,
                SessionInfo {
                    session,
                    resources,
                    last_touched,
                },
            ));
        }
        live.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| a.1.session.cmp(&b.1.session))
        });
        Ok(live.into_iter().map(|(_, info)| info).collect())
    }

    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        let k = Keys::of(s);
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.cmd("ZRANGE").arg(&k.order).arg(0).arg(-1);
        pipe.cmd("HGETALL").arg(&k.res);
        let (order, mut all): (Vec<String>, HashMap<String, String>) =
            pipe.query_async(&mut self.con()).await?;
        order
            .iter()
            .filter_map(|f| all.remove(f))
            .map(|j| decode(&j))
            .collect()
    }

    async fn seed(&self, s: &SessionKey, resources: Vec<StoredResource>) -> Result<(), StateError> {
        let k = Keys::of(s);
        let mut entries: Vec<(String, String)> = Vec::with_capacity(resources.len());
        for r in &resources {
            entries.push((field(&r.resource_type, &r.id), encode(r)?));
        }
        let last = if entries.is_empty() {
            0
        } else {
            self.next_seq(entries.len()).await?
        };
        let first = last.saturating_sub(to_i64(entries.len())).saturating_add(1);
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.cmd("DEL")
            .arg(&k.res)
            .arg(&k.order)
            .arg(&k.meta)
            .ignore();
        if !entries.is_empty() {
            let hset = pipe.cmd("HSET").arg(&k.res);
            for (f, json) in &entries {
                hset.arg(f).arg(json);
            }
            hset.ignore();
            let zadd = pipe.cmd("ZADD").arg(&k.order);
            // Later entries are more recent; a duplicate keeps its last
            // position, as repeated upserts would.
            for (i, (f, _)) in entries.iter().enumerate() {
                zadd.arg(first.saturating_add(to_i64(i))).arg(f);
            }
            zadd.ignore();
        }
        self.refresh(&mut pipe, &k, &s.session);
        pipe.query_async::<()>(&mut self.con()).await?;
        Ok(())
    }

    async fn ping(&self) -> Result<(), StateError> {
        let mut con = self.con();
        let pong = tokio::time::timeout(
            PING_TIMEOUT,
            redis::cmd("PING").query_async::<String>(&mut con),
        )
        .await
        .map_err(|_| StateError::Backend("redis: ping timed out".into()))??;
        if pong == "PONG" {
            Ok(())
        } else {
            Err(StateError::Backend(format!(
                "redis: unexpected ping reply {pong:?}"
            )))
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.tx.subscribe()
    }
}

fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}
