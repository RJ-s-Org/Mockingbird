use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use mb_store::{AdminSession, ControlStore};
use rand::Rng;

use crate::error::AuthError;
use crate::principal::Principal;

/// Name of the admin session cookie.
pub const SESSION_COOKIE: &str = "mb_session";

/// `last_seen` is written at most this often (or every half idle timeout
/// when that is shorter).
const TOUCH_INTERVAL: Duration = Duration::from_secs(60);
/// How often the sweeper deletes expired sessions.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Whether session cookies carry the `Secure` attribute (sent over HTTPS
/// only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CookieSecurity {
    /// `Secure`: the console is served over HTTPS (or behind a TLS proxy).
    Secure,
    /// No `Secure` attribute, for a console served over plain HTTP.
    Insecure,
}

/// A live console session as [`SessionManager::list`] describes it. Its
/// `id` is the hex sha256 of the cookie value: it names the session (to
/// revoke it) but cannot be used to sign in.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionInfo {
    /// The session id ([`SessionManager::session_id`] of its cookie).
    pub id: String,
    /// The signed-in principal's subject.
    pub subject: String,
    /// The signed-in principal's display name.
    pub display_name: String,
    /// When the session was created (the login).
    pub created_at: DateTime<Utc>,
    /// When the session ends regardless of activity (login plus the TTL).
    pub expires_at: DateTime<Utc>,
    /// The last request seen with the session, written at most once a
    /// minute (every half idle timeout when that is shorter).
    pub last_seen: DateTime<Utc>,
    /// When the session ends unless it is used again (`last_seen` plus the
    /// idle timeout); `None` without an idle timeout.
    pub idle_expires_at: Option<DateTime<Utc>>,
}

/// Server-side console sessions. The cookie value is 32 random bytes
/// (base64url, no padding); the store only sees its hex sha256. The TTL is
/// absolute; with [`Self::with_idle_timeout`] a session also ends once it
/// has not been used for that long.
#[derive(Clone)]
pub struct SessionManager {
    store: Arc<dyn ControlStore>,
    ttl: Duration,
    secure: bool,
    idle: Option<Duration>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("ttl", &self.ttl)
            .field("secure", &self.secure)
            .field("idle", &self.idle)
            .finish_non_exhaustive()
    }
}

impl SessionManager {
    /// Sessions stored in `store` that last `ttl` from login; `secure`
    /// adds the `Secure` cookie attribute. Prefer
    /// [`Self::with_cookie_security`], which names that choice.
    pub fn new(store: Arc<dyn ControlStore>, ttl: Duration, secure: bool) -> Self {
        SessionManager {
            store,
            ttl,
            secure,
            idle: None,
        }
    }

    /// [`Self::new`] with the cookie's `Secure` attribute set by `security`.
    pub fn with_cookie_security(
        store: Arc<dyn ControlStore>,
        ttl: Duration,
        security: CookieSecurity,
    ) -> Self {
        Self::new(store, ttl, security == CookieSecurity::Secure)
    }

    /// Also ends a session once it has not been used for `idle` (its
    /// `last_seen` is that old), before its absolute TTL.
    /// `Duration::ZERO` disables the idle timeout, which is the default.
    #[must_use]
    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        self.idle = (!idle.is_zero()).then_some(idle);
        self
    }

    /// The idle timeout, or `None` when sessions end only at their TTL.
    pub fn idle_timeout(&self) -> Option<Duration> {
        self.idle
    }

    /// The id of the session whose cookie value is `cookie_value`: the hex
    /// sha256 the store keys it by, as [`SessionInfo::id`] shows it.
    pub fn session_id(cookie_value: &str) -> String {
        hash_cookie(cookie_value)
    }

    /// Stores a new session for `p` and returns the cookie value.
    ///
    /// # Errors
    ///
    /// [`AuthError::Invalid`] when the TTL is out of range or `p` cannot be
    /// serialised; [`AuthError::Store`] when the session cannot be stored.
    pub async fn create(&self, p: &Principal) -> Result<String, AuthError> {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let value = URL_SAFE_NO_PAD.encode(bytes);
        let now = Utc::now();
        let ttl = chrono::Duration::from_std(self.ttl)
            .map_err(|e| AuthError::Invalid(format!("session ttl out of range: {e}")))?;
        let principal =
            serde_json::to_value(p).map_err(|e| AuthError::Invalid(format!("principal: {e}")))?;
        let expires_at = now
            .checked_add_signed(ttl)
            .ok_or_else(|| AuthError::Invalid("session ttl out of range".into()))?;
        self.store
            .create_admin_session(&AdminSession {
                id_hash: hash_cookie(&value),
                principal,
                created_at: now,
                expires_at,
                last_seen: now,
            })
            .await?;
        Ok(value)
    }

    /// The session's principal, or `None` for an unknown, expired or idle
    /// session (an expired or idle one is deleted). A found session counts
    /// as used now.
    ///
    /// # Errors
    ///
    /// [`AuthError::Store`] when the store cannot be read or updated.
    pub async fn lookup(&self, cookie_value: &str) -> Result<Option<Principal>, AuthError> {
        let id_hash = hash_cookie(cookie_value);
        let Some(session) = self.store.get_admin_session(&id_hash).await? else {
            return Ok(None);
        };
        let now = Utc::now();
        if !self.live(&session, now) {
            self.store.delete_admin_session(&id_hash).await?;
            return Ok(None);
        }
        let principal: Principal = match serde_json::from_value(session.principal) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(%err, "discarding admin session with an unreadable principal");
                self.store.delete_admin_session(&id_hash).await?;
                return Ok(None);
            }
        };
        if since(session.last_seen, now) >= self.touch_interval() {
            self.store.touch_admin_session(&id_hash, now).await?;
        }
        Ok(Some(principal))
    }

    /// Whether `s` has neither expired nor (with an idle timeout) idled
    /// out at `now`.
    fn live(&self, s: &AdminSession, now: DateTime<Utc>) -> bool {
        s.expires_at > now && self.idle.is_none_or(|idle| since(s.last_seen, now) < idle)
    }

    /// How often `last_seen` is written: once a minute, or every half idle
    /// timeout when that is shorter, so a session in use never idles out.
    fn touch_interval(&self) -> Duration {
        self.idle
            .map_or(TOUCH_INTERVAL, |idle| TOUCH_INTERVAL.min(idle / 2))
    }

    /// The live sessions (neither expired nor idle), all of them or those
    /// of `subject`, newest first. Sessions whose principal cannot be read
    /// are left out.
    ///
    /// # Errors
    ///
    /// [`AuthError::Store`] when the store cannot list sessions.
    pub async fn list(&self, subject: Option<&str>) -> Result<Vec<SessionInfo>, AuthError> {
        let now = Utc::now();
        Ok(self
            .store
            .list_admin_sessions(subject)
            .await?
            .into_iter()
            .filter(|s| self.live(s, now))
            .filter_map(|s| self.info(s))
            .collect())
    }

    /// The live session with this id, if any.
    ///
    /// # Errors
    ///
    /// [`AuthError::Store`] when the store cannot be read.
    pub async fn get(&self, id: &str) -> Result<Option<SessionInfo>, AuthError> {
        let now = Utc::now();
        Ok(self
            .store
            .get_admin_session(id)
            .await?
            .filter(|s| self.live(s, now))
            .and_then(|s| self.info(s)))
    }

    /// Ends the session with this id; `false` when there is none.
    ///
    /// # Errors
    ///
    /// [`AuthError::Store`] when the store cannot delete it.
    pub async fn revoke(&self, id: &str) -> Result<bool, AuthError> {
        Ok(self.store.delete_admin_session(id).await?)
    }

    /// Ends every session of `subject` (sign out everywhere) and returns
    /// how many were stored, expired and idle ones included.
    ///
    /// # Errors
    ///
    /// [`AuthError::Store`] when the store cannot delete them.
    pub async fn revoke_all(&self, subject: &str) -> Result<u64, AuthError> {
        Ok(self.store.delete_admin_sessions_of(subject).await?)
    }

    fn info(&self, s: AdminSession) -> Option<SessionInfo> {
        let p: Principal = serde_json::from_value(s.principal).ok()?;
        let idle_expires_at = match self.idle {
            Some(idle) => {
                let idle = chrono::Duration::from_std(idle).ok()?;
                Some(s.last_seen.checked_add_signed(idle)?)
            }
            None => None,
        };
        Some(SessionInfo {
            id: s.id_hash,
            subject: p.subject,
            display_name: p.display_name,
            created_at: s.created_at,
            expires_at: s.expires_at,
            last_seen: s.last_seen,
            idle_expires_at,
        })
    }

    /// Deletes the session `cookie_value` identifies (logout); unknown
    /// sessions are ignored.
    ///
    /// # Errors
    ///
    /// [`AuthError::Store`] when the store cannot delete it.
    pub async fn destroy(&self, cookie_value: &str) -> Result<(), AuthError> {
        self.store
            .delete_admin_session(&hash_cookie(cookie_value))
            .await?;
        Ok(())
    }

    /// The `Set-Cookie` value that installs a session cookie.
    pub fn set_cookie(&self, value: &str) -> String {
        self.cookie(value, self.ttl.as_secs())
    }

    /// The `Set-Cookie` value that removes the session cookie.
    pub fn clear_cookie(&self) -> String {
        self.cookie("", 0)
    }

    fn cookie(&self, value: &str, max_age: u64) -> String {
        let mut c =
            format!("{SESSION_COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}");
        if self.secure {
            c.push_str("; Secure");
        }
        c
    }

    /// Deletes expired sessions (and, with an idle timeout, idle ones)
    /// every hour.
    ///
    /// # Panics
    ///
    /// When called outside a Tokio runtime.
    pub fn spawn_sweeper(&self) -> tokio::task::JoinHandle<()> {
        let store = self.store.clone();
        let idle = self.idle.and_then(|d| chrono::Duration::from_std(d).ok());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let now = Utc::now();
                match store.delete_expired_admin_sessions(now).await {
                    Ok(n) if n > 0 => tracing::debug!(deleted = n, "swept expired admin sessions"),
                    Ok(_) => {}
                    Err(err) => tracing::warn!(%err, "admin session sweep failed"),
                }
                let Some(before) = idle.and_then(|idle| now.checked_sub_signed(idle)) else {
                    continue;
                };
                match store.delete_idle_admin_sessions(before).await {
                    Ok(n) if n > 0 => tracing::debug!(deleted = n, "swept idle admin sessions"),
                    Ok(_) => {}
                    Err(err) => tracing::warn!(%err, "idle admin session sweep failed"),
                }
            }
        })
    }
}

/// `now - then`, zero when `then` is in the future.
fn since(then: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (now - then).to_std().unwrap_or(Duration::ZERO)
}

fn hash_cookie(value: &str) -> String {
    crate::sha256_hex(value.as_bytes())
}
