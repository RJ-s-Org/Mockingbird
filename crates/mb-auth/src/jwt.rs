//! Verification of bearer JWTs issued by the configured OIDC provider.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use jsonwebtoken::jwk::{Jwk, JwkSet, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde_json::{Map, Value};
use tokio::sync::RwLock;

use crate::authenticator::BearerVerifier;
use crate::error::AuthError;
use crate::mapping::RoleMapping;
use crate::principal::Principal;

/// The only signature algorithms a bearer JWT may use.
pub const ALLOWED_ALGS: [Algorithm; 3] = [Algorithm::RS256, Algorithm::ES256, Algorithm::EdDSA];

/// Leeway in seconds for `exp` and `nbf`.
const LEEWAY_SECS: u64 = 60;

/// Default minimum time between JWKS fetches triggered by an unknown `kid`.
const DEFAULT_REFETCH_INTERVAL: Duration = Duration::from_secs(30);

/// Verifies bearer JWTs against the provider's JWKS and maps their claims
/// to a [`Principal`].
pub struct JwtVerifier {
    issuer: String,
    audience: String,
    jwks_uri: String,
    mapping: RoleMapping,
    http: reqwest::Client,
    /// The cached key set and when it was last fetched (`None`: never).
    cache: RwLock<(JwkSet, Option<Instant>)>,
    refetch_interval: Duration,
}

impl JwtVerifier {
    pub fn new(
        issuer: String,
        audience: String,
        jwks_uri: String,
        mapping: RoleMapping,
        http: reqwest::Client,
    ) -> Self {
        JwtVerifier {
            issuer,
            audience,
            jwks_uri,
            mapping,
            http,
            cache: RwLock::new((JwkSet::default(), None)),
            refetch_interval: DEFAULT_REFETCH_INTERVAL,
        }
    }

    /// Sets the minimum time between JWKS fetches (default 30 s).
    pub fn with_refetch_interval(mut self, d: Duration) -> Self {
        self.refetch_interval = d;
        self
    }

    /// The decoding key for `kid`, refetching the JWKS when the key is
    /// unknown and the last fetch is older than the refetch interval.
    async fn key(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        {
            let cache = self.cache.read().await;
            if let Some(jwk) = cache.0.find(kid) {
                return decoding_key(jwk);
            }
            if self.throttled(cache.1) {
                return Err(unknown_kid(kid));
            }
        }
        let mut cache = self.cache.write().await;
        // Another request may have refetched while we waited for the lock.
        if let Some(jwk) = cache.0.find(kid) {
            return decoding_key(jwk);
        }
        if self.throttled(cache.1) {
            return Err(unknown_kid(kid));
        }
        // Record the attempt first so a failing provider is throttled too.
        cache.1 = Some(Instant::now());
        cache.0 = self.fetch_jwks().await?;
        match cache.0.find(kid) {
            Some(jwk) => decoding_key(jwk),
            None => Err(unknown_kid(kid)),
        }
    }

    fn throttled(&self, last_fetch: Option<Instant>) -> bool {
        last_fetch.is_some_and(|t| t.elapsed() < self.refetch_interval)
    }

    async fn fetch_jwks(&self) -> Result<JwkSet, AuthError> {
        let resp = self
            .http
            .get(&self.jwks_uri)
            .send()
            .await
            .map_err(|e| AuthError::Upstream(format!("fetching JWKS failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(AuthError::Upstream(format!(
                "fetching JWKS failed: HTTP {}",
                resp.status()
            )));
        }
        resp.json::<JwkSet>()
            .await
            .map_err(|e| AuthError::Upstream(format!("invalid JWKS: {e}")))
    }
}

fn unknown_kid(kid: &str) -> AuthError {
    AuthError::Unauthenticated(format!("unknown signing key `{kid}`"))
}

fn decoding_key(jwk: &Jwk) -> Result<DecodingKey, AuthError> {
    if matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption)) {
        return Err(AuthError::Unauthenticated(
            "signing key is an encryption key".to_string(),
        ));
    }
    DecodingKey::from_jwk(jwk)
        .map_err(|e| AuthError::Unauthenticated(format!("unusable signing key: {e}")))
}

fn invalid(e: jsonwebtoken::errors::Error) -> AuthError {
    AuthError::Unauthenticated(format!("invalid bearer token: {e}"))
}

#[async_trait]
impl BearerVerifier for JwtVerifier {
    async fn verify(&self, token: &str) -> Result<Principal, AuthError> {
        let header = decode_header(token).map_err(invalid)?;
        if !ALLOWED_ALGS.contains(&header.alg) {
            return Err(AuthError::Unauthenticated(format!(
                "bearer token algorithm {:?} is not allowed",
                header.alg
            )));
        }
        let kid = header
            .kid
            .as_deref()
            .ok_or_else(|| AuthError::Unauthenticated("bearer token has no `kid`".to_string()))?;
        let key = self.key(kid).await?;

        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.leeway = LEEWAY_SECS;
        validation.validate_nbf = true;

        let data = decode::<Map<String, Value>>(token, &key, &validation).map_err(invalid)?;
        self.mapping.principal(&data.claims)
    }
}
