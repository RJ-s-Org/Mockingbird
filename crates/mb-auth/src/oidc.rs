//! The OIDC relying party: Authorization Code + PKCE login against the
//! configured provider.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openidconnect::core::{
    CoreAuthDisplay, CoreAuthPrompt, CoreAuthenticationFlow, CoreErrorResponseType,
    CoreGenderClaim, CoreJsonWebKey, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm,
    CoreProviderMetadata, CoreRevocableToken, CoreRevocationErrorResponse,
    CoreTokenIntrospectionResponse, CoreTokenType,
};
use openidconnect::{
    AdditionalClaims, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EmptyExtraTokenFields,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, IdTokenFields, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RequestTokenError,
    Scope, StandardErrorResponse, StandardTokenResponse, TokenResponse, UserInfoClaims,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::AuthError;
use crate::http::OidcHttp;
use crate::jwt::JwtVerifier;
use crate::mapping::RoleMapping;
use crate::principal::Principal;

/// How long a started login may take to come back to the callback.
const PENDING_TTL: Duration = Duration::from_secs(600);
/// The most logins in flight at once; the oldest is evicted beyond this.
const MAX_PENDING: usize = 10_000;

/// Every claim that is not a standard OIDC claim (for example `groups`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ExtraClaims(HashMap<String, Value>);
impl AdditionalClaims for ExtraClaims {}

type IdFields = IdTokenFields<
    ExtraClaims,
    EmptyExtraTokenFields,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
>;
type TokenResp = StandardTokenResponse<IdFields, CoreTokenType>;
type Client = openidconnect::Client<
    ExtraClaims,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    TokenResp,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// The resolved `auth.oidc` configuration.
#[derive(Clone, Debug)]
pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    /// The resolved secret value (not an `env:`/`file:` reference).
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    /// Required `aud` of bearer JWTs; defaults to `client_id`.
    pub audience: Option<String>,
    pub redirect_url: String,
    pub mapping: RoleMapping,
}

/// Where to send the browser to log in, and the `state` that identifies
/// this login (stored in the `mb_login` cookie).
#[derive(Clone, Debug)]
pub struct LoginRedirect {
    pub url: String,
    pub state: String,
}

/// The outcome of a successful callback.
#[derive(Clone, Debug)]
pub struct CompletedLogin {
    pub principal: Principal,
    pub return_to: String,
}

struct Pending {
    pkce_verifier: PkceCodeVerifier,
    nonce: Nonce,
    return_to: String,
    created: Instant,
}

pub struct OidcClient {
    client: Client,
    http: reqwest::Client,
    scopes: Vec<String>,
    mapping: RoleMapping,
    bearer: Arc<JwtVerifier>,
    pending: Mutex<HashMap<String, Pending>>,
}

fn upstream(context: &str, e: impl std::fmt::Display) -> AuthError {
    AuthError::Upstream(format!("{context}: {e}"))
}

impl OidcClient {
    /// Fetches the provider's discovery document (and JWKS) from
    /// `<issuer>/.well-known/openid-configuration`.
    pub async fn discover(
        settings: OidcSettings,
        http: reqwest::Client,
    ) -> Result<Self, AuthError> {
        let issuer = IssuerUrl::new(settings.issuer.clone())
            .map_err(|e| AuthError::Invalid(format!("invalid OIDC issuer: {e}")))?;
        let redirect = RedirectUrl::new(settings.redirect_url.clone())
            .map_err(|e| AuthError::Invalid(format!("invalid OIDC redirect URL: {e}")))?;
        let call = OidcHttp(&http);
        let metadata = CoreProviderMetadata::discover_async(issuer, &call)
            .await
            .map_err(|e| upstream("OIDC discovery failed", error_chain(&e)))?;
        let jwks_uri = metadata.jwks_uri().url().to_string();

        let client = Client::from_provider_metadata(
            metadata,
            ClientId::new(settings.client_id.clone()),
            settings.client_secret.map(ClientSecret::new),
        )
        .set_redirect_uri(redirect);

        let audience = settings
            .audience
            .unwrap_or_else(|| settings.client_id.clone());
        let bearer = Arc::new(JwtVerifier::new(
            settings.issuer,
            audience,
            jwks_uri,
            settings.mapping.clone(),
            http.clone(),
        ));

        Ok(OidcClient {
            client,
            http,
            scopes: settings.scopes,
            mapping: settings.mapping,
            bearer,
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Starts an Authorization Code + PKCE (S256) login that returns the
    /// browser to `return_to` once it completes.
    pub fn begin_login(&self, return_to: &str) -> Result<LoginRedirect, AuthError> {
        let return_to = validate_return_to(return_to)?;
        let (challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let scopes = self
            .scopes
            .iter()
            // openidconnect always requests `openid` itself.
            .filter(|s| s.as_str() != "openid")
            .map(|s| Scope::new(s.clone()));
        let (url, state, nonce) = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scopes(scopes)
            .set_pkce_challenge(challenge)
            .url();
        let state = state.secret().clone();

        let now = Instant::now();
        let mut pending = self.lock_pending();
        pending.retain(|_, p| now.duration_since(p.created) < PENDING_TTL);
        while pending.len() >= MAX_PENDING {
            let oldest = pending
                .iter()
                .min_by_key(|(_, p)| p.created)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => pending.remove(&k),
                None => break,
            };
        }
        pending.insert(
            state.clone(),
            Pending {
                pkce_verifier,
                nonce,
                return_to,
                created: now,
            },
        );

        Ok(LoginRedirect {
            url: url.to_string(),
            state,
        })
    }

    /// Finishes the login identified by `state`: exchanges `code`, verifies
    /// the ID token and maps its claims to a principal. A state can be
    /// completed at most once.
    pub async fn complete_login(
        &self,
        state: &str,
        code: &str,
    ) -> Result<CompletedLogin, AuthError> {
        let pending = self
            .lock_pending()
            .remove(state)
            .filter(|p| p.created.elapsed() < PENDING_TTL)
            .ok_or_else(|| AuthError::Unauthenticated("unknown login state".to_string()))?;

        let call = OidcHttp(&self.http);
        let token = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .map_err(|e| upstream("OIDC provider has no token endpoint", e))?
            .set_pkce_verifier(pending.pkce_verifier)
            .request_async(&call)
            .await
            .map_err(|e| match e {
                RequestTokenError::ServerResponse(r) => {
                    AuthError::Unauthenticated(format!("authorization code rejected: {r}"))
                }
                other => upstream("token exchange failed", error_chain(&other)),
            })?;

        let id_token = token
            .id_token()
            .ok_or_else(|| AuthError::Upstream("token response has no id_token".to_string()))?;
        let verifier = self.client.id_token_verifier();
        let id_claims = id_token
            .claims(&verifier, &pending.nonce)
            .map_err(|e| AuthError::Unauthenticated(format!("invalid ID token: {e}")))?;
        let mut claims = to_map(id_claims)?;

        if !claims.contains_key(&self.mapping.roles_claim) && self.client.user_info_url().is_some()
        {
            let info: UserInfoClaims<ExtraClaims, CoreGenderClaim> = self
                .client
                .user_info(
                    token.access_token().clone(),
                    Some(id_claims.subject().clone()),
                )
                .map_err(|e| upstream("OIDC userinfo", e))?
                .request_async(&call)
                .await
                .map_err(|e| upstream("OIDC userinfo failed", error_chain(&e)))?;
            for (k, v) in to_map(&info)? {
                claims.entry(k).or_insert(v);
            }
        }

        Ok(CompletedLogin {
            principal: self.mapping.principal(&claims)?,
            return_to: pending.return_to,
        })
    }

    /// The bearer-JWT verifier for this provider (JWKS URI from discovery).
    pub fn bearer(&self) -> Arc<JwtVerifier> {
        Arc::clone(&self.bearer)
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        // A panic while holding the lock cannot leave the map inconsistent.
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn to_map<T: Serialize>(v: &T) -> Result<Map<String, Value>, AuthError> {
    match serde_json::to_value(v) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(AuthError::Upstream(
            "claims are not a JSON object".to_string(),
        )),
        Err(e) => Err(upstream("serializing claims", e)),
    }
}

/// An error and all its sources, `: `-separated.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// Accepts a same-origin path for the post-login redirect: it must start
/// with `/`, must not start with `//` or `/\` (protocol-relative URLs) and
/// must contain no control characters (browsers strip tabs and newlines,
/// which would turn `/\t/evil` into `//evil`).
pub fn validate_return_to(s: &str) -> Result<String, AuthError> {
    let ok = s.starts_with('/')
        && !s.starts_with("//")
        && !s.starts_with("/\\")
        && !s.chars().any(char::is_control);
    if ok {
        Ok(s.to_string())
    } else {
        Err(AuthError::Invalid(
            "return_to must be a path starting with a single `/`".to_string(),
        ))
    }
}
