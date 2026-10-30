use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use mb_store::{AdminSession, ControlStore};
use rand::Rng;

use crate::error::AuthError;
use crate::principal::Principal;

/// Name of the admin session cookie.
pub const SESSION_COOKIE: &str = "mb_session";

/// `last_seen` is written at most this often.
const TOUCH_INTERVAL_SECS: i64 = 60;
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

/// Server-side console sessions. The cookie value is 32 random bytes
/// (base64url, no padding); the store only sees its hex sha256. The TTL is
/// absolute.
#[derive(Clone)]
pub struct SessionManager {
    store: Arc<dyn ControlStore>,
    ttl: Duration,
    secure: bool,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("ttl", &self.ttl)
            .field("secure", &self.secure)
            .finish_non_exhaustive()
    }
}

impl SessionManager {
    /// Sessions stored in `store` that last `ttl` from login; `secure`
    /// adds the `Secure` cookie attribute. Prefer
    /// [`Self::with_cookie_security`], which names that choice.
    pub fn new(store: Arc<dyn ControlStore>, ttl: Duration, secure: bool) -> Self {
        SessionManager { store, ttl, secure }
    }

    /// [`Self::new`] with the cookie's `Secure` attribute set by `security`.
    pub fn with_cookie_security(
        store: Arc<dyn ControlStore>,
        ttl: Duration,
        security: CookieSecurity,
    ) -> Self {
        Self::new(store, ttl, security == CookieSecurity::Secure)
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

    /// The session's principal, or `None` for an unknown or expired session
    /// (an expired one is deleted).
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
        if session.expires_at <= now {
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
        if (now - session.last_seen).num_seconds() >= TOUCH_INTERVAL_SECS {
            self.store.touch_admin_session(&id_hash, now).await?;
        }
        Ok(Some(principal))
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

    /// Deletes expired sessions every hour.
    ///
    /// # Panics
    ///
    /// When called outside a Tokio runtime.
    pub fn spawn_sweeper(&self) -> tokio::task::JoinHandle<()> {
        let store = self.store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                match store.delete_expired_admin_sessions(Utc::now()).await {
                    Ok(n) if n > 0 => tracing::debug!(deleted = n, "swept expired admin sessions"),
                    Ok(_) => {}
                    Err(err) => tracing::warn!(%err, "admin session sweep failed"),
                }
            }
        })
    }
}

fn hash_cookie(value: &str) -> String {
    crate::sha256_hex(value.as_bytes())
}
