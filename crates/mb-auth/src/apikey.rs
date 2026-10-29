use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use mb_store::{ApiKey, ControlStore};
use rand::Rng;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

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
#[derive(Clone)]
pub struct ApiKeys {
    store: Arc<dyn ControlStore>,
}

impl ApiKeys {
    pub fn new(store: Arc<dyn ControlStore>) -> Self {
        ApiKeys { store }
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
        let mut key = self.store.get_api_key(&key_id).await?.ok_or_else(invalid)?;
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
            self.store.touch_api_key(&key.key_id, now).await?;
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
