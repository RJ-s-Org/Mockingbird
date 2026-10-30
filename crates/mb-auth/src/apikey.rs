use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use mb_store::{ApiKey, ControlStore};
use rand::Rng;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::apikey_cache::{self, KeyCache};
use crate::error::AuthError;
use crate::principal::{Principal, PrincipalKind};
use crate::role::Role;

/// Every API key token starts with this.
pub const API_KEY_PREFIX: &str = "mbk_";

const KEY_ID_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const KEY_ID_LEN: usize = 12;
const SECRET_LEN: usize = 32;
/// base64url without padding of [`SECRET_LEN`] bytes.
const SECRET_B64_LEN: usize = 43;
const MAX_NAME_CHARS: usize = 128;
/// `last_used_at` is written at most this often.
const TOUCH_INTERVAL_SECS: i64 = 60;

/// How long [`ApiKeys::with_cache`] keeps a key's stored row by default.
/// A key revoked on another node (or in the store directly) keeps working
/// on this one for up to this long; see [`ApiKeys::invalidate_caches`].
pub const DEFAULT_API_KEY_CACHE_TTL: Duration = Duration::from_secs(5);

/// How many keys an [`ApiKeys::with_cache`] cache holds at most.
pub const API_KEY_CACHE_CAPACITY: usize = 10_000;

/// What an API key may be used for.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum KeyScope {
    Admin,
    DataPlane,
}

impl KeyScope {
    /// The wire string (`admin`, `data-plane`).
    pub fn as_str(self) -> &'static str {
        match self {
            KeyScope::Admin => "admin",
            KeyScope::DataPlane => "data-plane",
        }
    }
}

/// A freshly generated key. `token` is shown once; only `secret_hash` is stored.
/// Its `Debug` output redacts `token`.
#[derive(Clone)]
pub struct GeneratedKey {
    pub key_id: String,
    pub token: String,
    pub secret_hash: String,
}

impl std::fmt::Debug for GeneratedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedKey")
            .field("key_id", &self.key_id)
            .field("token", &"<redacted>")
            .field("secret_hash", &self.secret_hash)
            .finish()
    }
}

/// Generates `mbk_<key_id>_<secret>` from a CSPRNG.
pub fn generate_key() -> GeneratedKey {
    let mut rng = rand::rng();
    let mut id_bytes = [0u8; KEY_ID_LEN];
    rng.fill_bytes(&mut id_bytes);
    // 256 is a multiple of 32, so masking is unbiased.
    let key_id: String = id_bytes
        .iter()
        .map(|b| char::from(KEY_ID_ALPHABET[usize::from(b & 31)]))
        .collect();
    let mut secret = [0u8; SECRET_LEN];
    rng.fill_bytes(&mut secret);
    let token = format!(
        "{API_KEY_PREFIX}{key_id}_{}",
        URL_SAFE_NO_PAD.encode(secret)
    );
    GeneratedKey {
        key_id,
        token,
        secret_hash: hash_secret(&secret),
    }
}

/// Splits a token into its key id and secret bytes; `None` when malformed.
pub fn parse_token(token: &str) -> Option<(String, [u8; 32])> {
    let rest = token.strip_prefix(API_KEY_PREFIX)?;
    if !rest.is_ascii() || rest.len() != KEY_ID_LEN + 1 + SECRET_B64_LEN {
        return None;
    }
    let (id, tail) = rest.split_at(KEY_ID_LEN);
    if !id.bytes().all(|b| KEY_ID_ALPHABET.contains(&b)) {
        return None;
    }
    let secret_b64 = tail.strip_prefix('_')?;
    let decoded = URL_SAFE_NO_PAD.decode(secret_b64).ok()?;
    let secret: [u8; SECRET_LEN] = decoded.try_into().ok()?;
    Some((id.to_string(), secret))
}

/// Hex sha256 of the secret bytes: the only form of the secret that is stored.
pub fn hash_secret(secret: &[u8; 32]) -> String {
    crate::sha256_hex(secret)
}

/// A request to create an API key in a workspace.
#[derive(Clone, Debug)]
pub struct NewKeyRequest {
    pub name: String,
    pub role: Role,
    /// Empty means `[Admin]`.
    pub scopes: Vec<KeyScope>,
    pub expires_at: Option<DateTime<Utc>>,
    pub sysadmin: bool,
}

/// Creates and verifies API keys.
///
/// [`ApiKeys::new`] reads the store on every [`verify`](ApiKeys::verify).
/// [`ApiKeys::with_cache`] keeps each key's stored row for a short TTL
/// (clones share the cache); the secret hash, revocation, expiry and scope
/// are still checked on every call, and unknown key ids are never cached.
#[derive(Clone)]
pub struct ApiKeys {
    store: Arc<dyn ControlStore>,
    cache: Option<Arc<KeyCache>>,
}

impl ApiKeys {
    pub fn new(store: Arc<dyn ControlStore>) -> Self {
        ApiKeys { store, cache: None }
    }

    /// Like [`ApiKeys::new`], with a cache of at most
    /// [`API_KEY_CACHE_CAPACITY`] stored rows, each kept for `ttl`
    /// ([`DEFAULT_API_KEY_CACHE_TTL`] is the server's). A zero `ttl` caches
    /// nothing.
    ///
    /// Revoking a key must be followed by [`ApiKeys::invalidate_caches`] so
    /// that this process refuses it at once; other processes see the
    /// revocation within `ttl`.
    pub fn with_cache(store: Arc<dyn ControlStore>, ttl: Duration) -> Self {
        let cache = (!ttl.is_zero()).then(|| Arc::new(KeyCache::new(ttl, API_KEY_CACHE_CAPACITY)));
        ApiKeys { store, cache }
    }

    /// Drops every cached key row of every [`ApiKeys`] in this process, so
    /// the next verification of any key reads the store. Call it after
    /// revoking or deleting keys (the Admin API does).
    pub fn invalidate_caches() {
        apikey_cache::invalidate_all();
    }

    /// The stored row of `key_id`, from the cache when it is fresh.
    async fn load(&self, key_id: &str) -> Result<Option<ApiKey>, AuthError> {
        let Some(cache) = &self.cache else {
            return Ok(self.store.get_api_key(key_id).await?);
        };
        if let Some(key) = cache.get(key_id) {
            return Ok(Some(key));
        }
        let read_epoch = apikey_cache::epoch();
        cache.note_store_read();
        let key = self.store.get_api_key(key_id).await?;
        if let Some(key) = &key {
            cache.insert(key.clone(), read_epoch);
        }
        Ok(key)
    }

    /// Creates a key in `ws` on behalf of `creator`; returns the stored record
    /// and the token, which is never retrievable again.
    pub async fn create(
        &self,
        creator: &Principal,
        ws: &str,
        req: NewKeyRequest,
    ) -> Result<(ApiKey, String), AuthError> {
        if creator.kind == PrincipalKind::ApiKey {
            return Err(AuthError::Forbidden(
                "an API key cannot create API keys; sign in as a user".to_string(),
            ));
        }
        let chars = req.name.chars().count();
        if chars == 0 || chars > MAX_NAME_CHARS {
            return Err(AuthError::Invalid(format!(
                "API key name must be 1 to {MAX_NAME_CHARS} characters"
            )));
        }
        if req.sysadmin && !creator.sysadmin {
            return Err(AuthError::Forbidden(
                "only a sysadmin can create a sysadmin API key".to_string(),
            ));
        }
        match creator.role_in(ws) {
            Some(own) if req.role <= own => {}
            _ => {
                return Err(AuthError::Forbidden(format!(
                    "cannot create an API key with role {} in workspace {ws:?}",
                    req.role.as_str()
                )));
            }
        }
        let scopes = if req.scopes.is_empty() {
            vec![KeyScope::Admin]
        } else {
            req.scopes
        };
        let mut scope_strings: Vec<String> = Vec::with_capacity(scopes.len());
        for s in scopes {
            if !scope_strings.iter().any(|x| x == s.as_str()) {
                scope_strings.push(s.as_str().to_string());
            }
        }
        let generated = generate_key();
        let record = ApiKey {
            key_id: generated.key_id,
            workspace: ws.to_string(),
            name: req.name,
            role: req.role.as_str().to_string(),
            scopes: scope_strings,
            sysadmin: req.sysadmin,
            secret_hash: generated.secret_hash,
            created_by: creator.subject.clone(),
            created_at: Utc::now(),
            expires_at: req.expires_at,
            last_used_at: None,
            revoked_at: None,
        };
        self.store.create_api_key(&record).await?;
        Ok((record, generated.token))
    }

    /// Checks `token` and that it carries `scope`. Every failure is
    /// `Unauthenticated`. Touches `last_used_at` at most once per 60 s.
    pub async fn verify(&self, token: &str, scope: KeyScope) -> Result<ApiKey, AuthError> {
        let invalid = || AuthError::Unauthenticated("invalid API key".to_string());
        let (key_id, secret) = parse_token(token).ok_or_else(invalid)?;
        let mut key = self.load(&key_id).await?.ok_or_else(invalid)?;
        let presented = hash_secret(&secret);
        if !bool::from(presented.as_bytes().ct_eq(key.secret_hash.as_bytes())) {
            return Err(invalid());
        }
        let now = Utc::now();
        if key.revoked_at.is_some() {
            return Err(AuthError::Unauthenticated(
                "API key has been revoked".to_string(),
            ));
        }
        if key.expires_at.is_some_and(|e| e <= now) {
            return Err(AuthError::Unauthenticated(
                "API key has expired".to_string(),
            ));
        }
        if !key.scopes.iter().any(|s| s == scope.as_str()) {
            return Err(AuthError::Unauthenticated(format!(
                "API key lacks the {} scope",
                scope.as_str()
            )));
        }
        let stale = key
            .last_used_at
            .is_none_or(|t| (now - t).num_seconds() >= TOUCH_INTERVAL_SECS);
        if stale {
            // With a cache, concurrent requests on one key write it once.
            let claimed = self
                .cache
                .as_ref()
                .is_none_or(|c| c.claim_touch(&key.key_id, now, TOUCH_INTERVAL_SECS));
            if claimed {
                self.store.touch_api_key(&key.key_id, now).await?;
            }
            key.last_used_at = Some(now);
        }
        Ok(key)
    }

    /// The principal a verified key acts as.
    pub fn principal(key: &ApiKey) -> Principal {
        let roles: BTreeMap<String, Role> = Role::parse(&key.role)
            .map(|r| (key.workspace.clone(), r))
            .into_iter()
            .collect();
        Principal {
            subject: format!("api_key:{}", key.key_id),
            display_name: key.name.clone(),
            kind: PrincipalKind::ApiKey,
            sysadmin: key.sysadmin,
            roles,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Tests that count store reads or bump the process-wide epoch run one
    /// at a time, so one test's invalidation never shows up in another's
    /// count.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn store() -> Arc<dyn ControlStore> {
        mb_store::connect("sqlite::memory:").await.unwrap()
    }

    fn req(expires_at: Option<DateTime<Utc>>) -> NewKeyRequest {
        NewKeyRequest {
            name: "k".into(),
            role: Role::Viewer,
            scopes: vec![],
            expires_at,
            sysadmin: false,
        }
    }

    fn reads(keys: &ApiKeys) -> u64 {
        keys.cache.as_ref().map_or(0, |c| c.store_reads())
    }

    #[tokio::test]
    async fn repeated_verifies_read_the_store_once() {
        let _serial = SERIAL.lock().await;
        let keys = ApiKeys::with_cache(store().await, Duration::from_secs(60));
        let (rec, token) = keys
            .create(&Principal::local(), "default", req(None))
            .await
            .unwrap();
        for _ in 0..100 {
            let k = keys.verify(&token, KeyScope::Admin).await.unwrap();
            assert_eq!(k.key_id, rec.key_id);
        }
        assert_eq!(reads(&keys), 1);
        // A clone shares the cache.
        keys.clone().verify(&token, KeyScope::Admin).await.unwrap();
        assert_eq!(reads(&keys), 1);
        // The scope and the secret are still checked on a hit.
        assert!(keys.verify(&token, KeyScope::DataPlane).await.is_err());
        let forged = format!("{API_KEY_PREFIX}{}_{}", rec.key_id, "A".repeat(43));
        assert!(keys.verify(&forged, KeyScope::Admin).await.is_err());
        assert_eq!(reads(&keys), 1);
        // Unknown key ids are never cached.
        let unknown = generate_key().token;
        assert!(keys.verify(&unknown, KeyScope::Admin).await.is_err());
        assert!(keys.verify(&unknown, KeyScope::Admin).await.is_err());
        assert_eq!(reads(&keys), 3);
    }

    #[tokio::test]
    async fn invalidation_refuses_a_revoked_key_at_once() {
        let _serial = SERIAL.lock().await;
        let s = store().await;
        let keys = ApiKeys::with_cache(s.clone(), Duration::from_secs(60));
        let (rec, token) = keys
            .create(&Principal::local(), "default", req(None))
            .await
            .unwrap();
        keys.verify(&token, KeyScope::Admin).await.unwrap();
        assert!(s.revoke_api_key("default", &rec.key_id).await.unwrap());
        // Still cached: the revocation has not been announced.
        keys.verify(&token, KeyScope::Admin).await.unwrap();
        ApiKeys::invalidate_caches();
        assert!(matches!(
            keys.verify(&token, KeyScope::Admin).await,
            Err(AuthError::Unauthenticated(m)) if m.contains("revoked")
        ));
        assert_eq!(reads(&keys), 2);
    }

    #[tokio::test]
    async fn an_unannounced_revocation_applies_after_the_ttl() {
        let s = store().await;
        let keys = ApiKeys::with_cache(s.clone(), Duration::from_millis(100));
        let (rec, token) = keys
            .create(&Principal::local(), "default", req(None))
            .await
            .unwrap();
        keys.verify(&token, KeyScope::Admin).await.unwrap();
        assert!(s.revoke_api_key("default", &rec.key_id).await.unwrap());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(keys.verify(&token, KeyScope::Admin).await.is_err());
    }

    #[tokio::test]
    async fn expiry_is_checked_on_a_cache_hit() {
        let keys = ApiKeys::with_cache(store().await, Duration::from_secs(60));
        let expires = Utc::now() + chrono::Duration::milliseconds(300);
        let (_, token) = keys
            .create(&Principal::local(), "default", req(Some(expires)))
            .await
            .unwrap();
        keys.verify(&token, KeyScope::Admin).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(matches!(
            keys.verify(&token, KeyScope::Admin).await,
            Err(AuthError::Unauthenticated(m)) if m.contains("expired")
        ));
    }

    #[tokio::test]
    async fn a_zero_ttl_caches_nothing() {
        let keys = ApiKeys::with_cache(store().await, Duration::ZERO);
        assert!(keys.cache.is_none());
    }

    #[tokio::test]
    async fn last_used_is_written_once_per_interval_through_the_cache() {
        let s = store().await;
        let keys = ApiKeys::with_cache(s.clone(), Duration::from_secs(60));
        let (rec, token) = keys
            .create(&Principal::local(), "default", req(None))
            .await
            .unwrap();
        let first = keys.verify(&token, KeyScope::Admin).await.unwrap();
        let used = s
            .get_api_key(&rec.key_id)
            .await
            .unwrap()
            .unwrap()
            .last_used_at;
        assert!(used.is_some());
        assert_eq!(first.last_used_at, used);
        let again = keys.verify(&token, KeyScope::Admin).await.unwrap();
        assert_eq!(again.last_used_at, used);
        let stored = s.get_api_key(&rec.key_id).await.unwrap().unwrap();
        assert_eq!(stored.last_used_at, used);
    }

    fn row(id: &str) -> ApiKey {
        ApiKey {
            key_id: id.into(),
            workspace: "default".into(),
            name: "k".into(),
            role: "viewer".into(),
            scopes: vec!["admin".into()],
            sysadmin: false,
            secret_hash: String::new(),
            created_by: "t".into(),
            created_at: Utc::now(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        }
    }

    #[tokio::test]
    async fn the_cache_is_bounded_and_skips_racing_reads() {
        let _serial = SERIAL.lock().await;
        let cache = KeyCache::new(Duration::from_secs(60), 2);
        let e = apikey_cache::epoch();
        cache.insert(row("a"), e);
        cache.insert(row("b"), e);
        cache.insert(row("c"), e);
        assert_eq!(cache.len(), 2);
        assert!(cache.get("a").is_none(), "the oldest entry is evicted");
        assert!(cache.get("c").is_some());
        // A read that started before an invalidation is not cached.
        let before = apikey_cache::epoch();
        ApiKeys::invalidate_caches();
        cache.insert(row("d"), before);
        assert!(cache.get("d").is_none());
        assert!(cache.get("c").is_none(), "older entries are stale");
    }

    #[test]
    fn concurrent_touches_are_claimed_once() {
        let cache = KeyCache::new(Duration::from_secs(60), 8);
        cache.insert(row("a"), apikey_cache::epoch());
        let now = Utc::now();
        assert!(cache.claim_touch("a", now, TOUCH_INTERVAL_SECS));
        assert!(!cache.claim_touch("a", now, TOUCH_INTERVAL_SECS));
        let later = now + chrono::Duration::seconds(TOUCH_INTERVAL_SECS);
        assert!(cache.claim_touch("a", later, TOUCH_INTERVAL_SECS));
        assert!(cache.claim_touch("missing", now, TOUCH_INTERVAL_SECS));
    }
}
