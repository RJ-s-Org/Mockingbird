use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use mb_store::{AdminSession, AdminSessionGrant, ControlStore};
use rand::Rng;
use serde_json::Value;

use crate::error::AuthError;
use crate::mapping::RoleMapping;
use crate::oidc::{FinishedLogin, OidcClient};
use crate::principal::Principal;

/// Name of the admin session cookie.
pub const SESSION_COOKIE: &str = "mb_session";

/// How often a session's roles are refreshed from the identity provider
/// with its refresh token, unless
/// [`SessionManager::with_role_refresh_interval`] says otherwise.
pub const DEFAULT_ROLE_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Domain separation for the key that seals a session's refresh token.
const SEAL_CONTEXT: &[u8] = b"mockingbird admin session refresh token v1\0";

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
///
/// With [`Self::with_role_refresh`], a session created by
/// [`Self::create_from_login`] keeps the provider's claims and refresh
/// token, and its roles are re-derived after login: from the stored claims
/// whenever the role mapping changes, and from the provider every
/// [`DEFAULT_ROLE_REFRESH_INTERVAL`] with the refresh token when the
/// provider issued one (a refused refresh ends the session). The refresh
/// token is stored sealed (ChaCha20-Poly1305) under a key derived from the
/// cookie value, which the store never sees, so the store alone cannot use
/// it. Without a refresh token the roles change at the next login.
#[derive(Clone)]
pub struct SessionManager {
    store: Arc<dyn ControlStore>,
    ttl: Duration,
    secure: bool,
    idle: Option<Duration>,
    roles: Option<Arc<OidcClient>>,
    refresh_every: Duration,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("ttl", &self.ttl)
            .field("secure", &self.secure)
            .field("idle", &self.idle)
            .field("role_refresh", &self.roles.is_some())
            .field("refresh_every", &self.refresh_every)
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
            roles: None,
            refresh_every: DEFAULT_ROLE_REFRESH_INTERVAL,
        }
    }

    /// Re-derives the roles of sessions created by
    /// [`Self::create_from_login`] with `oidc`'s role mapping and provider
    /// (see the type docs).
    #[must_use]
    pub fn with_role_refresh(mut self, oidc: Arc<OidcClient>) -> Self {
        self.roles = Some(oidc);
        self
    }

    /// How often roles are refreshed with the provider's refresh token
    /// ([`DEFAULT_ROLE_REFRESH_INTERVAL`] by default); `Duration::ZERO`
    /// refreshes on every request.
    #[must_use]
    pub fn with_role_refresh_interval(mut self, every: Duration) -> Self {
        self.refresh_every = every;
        self
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

    /// [`Self::create`] for a console login. With
    /// [`Self::with_role_refresh`], the session also keeps what the provider
    /// granted (its claims and, sealed, its refresh token), so its roles can
    /// be re-derived after login.
    ///
    /// # Errors
    ///
    /// As [`Self::create`]; [`AuthError::Store`] when the grant cannot be
    /// stored.
    pub async fn create_from_login(&self, login: &FinishedLogin) -> Result<String, AuthError> {
        let value = self.create(&login.principal).await?;
        let Some(oidc) = &self.roles else {
            return Ok(value);
        };
        let id_hash = hash_cookie(&value);
        let (claims, refresh_token) = login.grant.clone().into_parts();
        let principal = serde_json::to_value(&login.principal)
            .map_err(|e| AuthError::Invalid(format!("principal: {e}")))?;
        let grant = AdminSessionGrant {
            claims: Value::Object(claims),
            mapping_hash: mapping_hash(oidc.mapping()),
            refresh_token: refresh_token.and_then(|t| seal(&value, &id_hash, &t)),
            refreshed_at: Utc::now(),
        };
        self.store
            .put_admin_session_grant(&id_hash, &principal, &grant)
            .await?;
        Ok(value)
    }

    /// The session's principal, or `None` for an unknown, expired or idle
    /// session (an expired or idle one is deleted). A found session counts
    /// as used now. With [`Self::with_role_refresh`], the principal's roles
    /// are re-derived first when the role mapping changed or a refresh is
    /// due, and a session whose refresh the provider refuses ends.
    ///
    /// # Errors
    ///
    /// [`AuthError::Store`] when the store cannot be read or updated.
    pub async fn lookup(&self, cookie_value: &str) -> Result<Option<Principal>, AuthError> {
        let id_hash = hash_cookie(cookie_value);
        let Some((session, grant)) = self.store.get_admin_session_with_grant(&id_hash).await?
        else {
            return Ok(None);
        };
        let now = Utc::now();
        if !self.live(&session, now) {
            self.store.delete_admin_session(&id_hash).await?;
            return Ok(None);
        }
        let mut principal: Principal = match serde_json::from_value(session.principal) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(%err, "discarding admin session with an unreadable principal");
                self.store.delete_admin_session(&id_hash).await?;
                return Ok(None);
            }
        };
        if let (Some(oidc), Some(grant)) = (&self.roles, grant) {
            let seen = RederiveInput {
                cookie_value,
                id_hash: &id_hash,
                now,
            };
            match self.rederive(oidc, seen, principal, grant).await? {
                Some(p) => principal = p,
                None => {
                    self.store.delete_admin_session(&id_hash).await?;
                    return Ok(None);
                }
            }
        }
        if since(session.last_seen, now) >= self.touch_interval() {
            self.store.touch_admin_session(&id_hash, now).await?;
        }
        Ok(Some(principal))
    }

    /// `principal` with its roles re-derived from `grant`: refreshed from
    /// the provider when a refresh is due (and this caller won it), else
    /// re-mapped from the stored claims when the role mapping changed.
    /// `None` when the session must end (the provider refused the refresh).
    async fn rederive(
        &self,
        oidc: &OidcClient,
        at: RederiveInput<'_>,
        principal: Principal,
        grant: AdminSessionGrant,
    ) -> Result<Option<Principal>, AuthError> {
        let mapping = oidc.mapping();
        let hash = mapping_hash(mapping);
        let due = grant.refresh_token.is_some()
            && since(grant.refreshed_at, at.now) >= self.refresh_every;
        let claimed = due
            && self
                .store
                .claim_admin_session_refresh(at.id_hash, grant.refreshed_at, at.now)
                .await?;
        if claimed {
            let token = grant
                .refresh_token
                .as_deref()
                .and_then(|sealed| unseal(at.cookie_value, at.id_hash, sealed));
            match token {
                None => tracing::warn!(
                    "cannot unseal a console session's refresh token; its roles stay as they are"
                ),
                Some(token) => {
                    let previous = grant.claims.as_object().cloned().unwrap_or_default();
                    match oidc
                        .refresh_grant(&token, &principal.subject, &previous)
                        .await
                    {
                        Ok(fresh) => {
                            let (claims, rotated) = fresh.into_parts();
                            let Ok(refreshed) = mapping.principal(&claims) else {
                                tracing::info!(
                                    "the refreshed claims name no subject; ending the console \
                                     session"
                                );
                                return Ok(None);
                            };
                            if refreshed.roles != principal.roles
                                || refreshed.sysadmin != principal.sysadmin
                            {
                                tracing::info!(
                                    subject = %principal.subject,
                                    "console session roles refreshed from the identity provider"
                                );
                            }
                            let token = rotated.unwrap_or(token);
                            let new_grant = AdminSessionGrant {
                                claims: Value::Object(claims),
                                mapping_hash: hash,
                                refresh_token: seal(at.cookie_value, at.id_hash, &token),
                                refreshed_at: at.now,
                            };
                            self.store_grant(at.id_hash, &refreshed, &new_grant).await?;
                            return Ok(Some(refreshed));
                        }
                        Err(AuthError::Unauthenticated(err)) => {
                            tracing::info!(
                                %err,
                                subject = %principal.subject,
                                "the identity provider refused the refresh; ending the console \
                                 session"
                            );
                            return Ok(None);
                        }
                        Err(err) => tracing::warn!(
                            %err,
                            "refreshing console session roles failed; keeping them until the \
                             next refresh"
                        ),
                    }
                }
            }
        }
        if grant.mapping_hash == hash {
            return Ok(Some(principal));
        }
        let Some(claims) = grant.claims.as_object() else {
            return Ok(Some(principal));
        };
        let remapped = match mapping.principal(claims) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(%err, "cannot re-map a console session's claims; keeping its roles");
                return Ok(Some(principal));
            }
        };
        let new_grant = AdminSessionGrant {
            mapping_hash: hash,
            refreshed_at: if claimed { at.now } else { grant.refreshed_at },
            ..grant
        };
        self.store_grant(at.id_hash, &remapped, &new_grant).await?;
        Ok(Some(remapped))
    }

    async fn store_grant(
        &self,
        id_hash: &str,
        principal: &Principal,
        grant: &AdminSessionGrant,
    ) -> Result<(), AuthError> {
        let principal = serde_json::to_value(principal)
            .map_err(|e| AuthError::Invalid(format!("principal: {e}")))?;
        self.store
            .put_admin_session_grant(id_hash, &principal, grant)
            .await?;
        Ok(())
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

/// Which session a re-derivation is for, and when.
#[derive(Clone, Copy)]
struct RederiveInput<'a> {
    cookie_value: &'a str,
    id_hash: &'a str,
    now: DateTime<Utc>,
}

/// Names a role mapping: the hex sha256 of its JSON form.
fn mapping_hash(mapping: &RoleMapping) -> String {
    let json = serde_json::to_string(mapping).unwrap_or_default();
    crate::sha256_hex(json.as_bytes())
}

/// The key that seals the refresh token of the session whose cookie value
/// is `cookie_value`. The store keys sessions by a different hash of the
/// cookie value, so it cannot derive this key.
fn seal_key(cookie_value: &str) -> Option<aws_lc_rs::aead::LessSafeKey> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(SEAL_CONTEXT);
    h.update(cookie_value.as_bytes());
    let key = h.finalize();
    aws_lc_rs::aead::UnboundKey::new(&aws_lc_rs::aead::CHACHA20_POLY1305, &key)
        .ok()
        .map(aws_lc_rs::aead::LessSafeKey::new)
}

/// `token` sealed for the session `id_hash` (bound as associated data):
/// base64url of a random nonce and the ciphertext. `None` if sealing fails.
fn seal(cookie_value: &str, id_hash: &str, token: &str) -> Option<String> {
    use aws_lc_rs::aead::{Aad, NONCE_LEN, Nonce};
    let key = seal_key(cookie_value)?;
    let mut nonce = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce);
    let mut data = token.as_bytes().to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(id_hash.as_bytes()),
        &mut data,
    )
    .ok()?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&data);
    Some(URL_SAFE_NO_PAD.encode(out))
}

/// The token [`seal`] sealed, or `None` for anything else.
fn unseal(cookie_value: &str, id_hash: &str, sealed: &str) -> Option<String> {
    use aws_lc_rs::aead::{Aad, NONCE_LEN, Nonce};
    let key = seal_key(cookie_value)?;
    let bytes = URL_SAFE_NO_PAD.decode(sealed).ok()?;
    if bytes.len() < NONCE_LEN {
        return None;
    }
    let (nonce, data) = bytes.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
    let mut data = data.to_vec();
    let plain = key
        .open_in_place(nonce, Aad::from(id_hash.as_bytes()), &mut data)
        .ok()?;
    String::from_utf8(plain.to_vec()).ok()
}

/// `now - then`, zero when `then` is in the future.
fn since(then: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (now - then).to_std().unwrap_or(Duration::ZERO)
}

fn hash_cookie(value: &str) -> String {
    crate::sha256_hex(value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{hash_cookie, seal, unseal};

    #[test]
    fn refresh_tokens_unseal_only_with_their_cookie_and_session() {
        let cookie = "cookie-value";
        let id = hash_cookie(cookie);
        let sealed = seal(cookie, &id, "refresh-token-secret").unwrap();
        assert!(!sealed.contains("refresh-token-secret"));
        // A fresh nonce each time.
        assert_ne!(seal(cookie, &id, "refresh-token-secret").unwrap(), sealed);
        assert_eq!(
            unseal(cookie, &id, &sealed).as_deref(),
            Some("refresh-token-secret")
        );
        // The store knows the id hash, never the cookie value.
        assert_eq!(unseal(&id, &id, &sealed), None);
        assert_eq!(unseal("other-cookie", &id, &sealed), None);
        assert_eq!(unseal(cookie, &hash_cookie("other"), &sealed), None);
        assert_eq!(unseal(cookie, &id, "not base64 !"), None);
        assert_eq!(unseal(cookie, &id, "AAAA"), None);
    }
}
