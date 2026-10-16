use std::sync::Arc;

use async_trait::async_trait;
use http::HeaderMap;
use http::header::{AUTHORIZATION, COOKIE};

use crate::apikey::{API_KEY_PREFIX, ApiKeys, KeyScope};
use crate::error::AuthError;
use crate::principal::Principal;
use crate::session::{SESSION_COOKIE, SessionManager};

/// Resolves the caller of an admin request. `Ok(None)` means "no credential".
#[async_trait]
pub trait Authenticate: Send + Sync {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Option<Principal>, AuthError>;
}

/// `auth.mode: none`: every request is [`Principal::local`].
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalAuth;

#[async_trait]
impl Authenticate for LocalAuth {
    async fn authenticate(&self, _headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
        Ok(Some(Principal::local()))
    }
}

/// Verifies a non-API-key bearer token (a JWT in `oidc` mode).
#[async_trait]
pub trait BearerVerifier: Send + Sync {
    async fn verify(&self, token: &str) -> Result<Principal, AuthError>;
}

/// `auth.mode: oidc`: API keys, bearer JWTs and session cookies.
#[derive(Clone)]
pub struct Authenticator {
    pub api_keys: ApiKeys,
    pub sessions: Option<SessionManager>,
    pub bearer: Option<Arc<dyn BearerVerifier>>,
}

#[async_trait]
impl Authenticate for Authenticator {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
        if let Some(token) = bearer_token(headers) {
            if token.starts_with(API_KEY_PREFIX) {
                let key = self.api_keys.verify(token, KeyScope::Admin).await?;
                return Ok(Some(ApiKeys::principal(&key)));
            }
            return match &self.bearer {
                Some(verifier) => verifier.verify(token).await.map(Some),
                None => Err(AuthError::Unauthenticated(
                    "bearer tokens are not accepted".to_string(),
                )),
            };
        }
        if let (Some(sessions), Some(cookie)) = (&self.sessions, session_cookie(headers)) {
            return sessions.lookup(cookie).await;
        }
        Ok(None)
    }
}

/// The token of an `Authorization: Bearer <token>` header (scheme is
/// case-insensitive).
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// The value of the first `mb_session` cookie across all `Cookie` headers.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.trim())
        .filter(|v| !v.is_empty())
}
