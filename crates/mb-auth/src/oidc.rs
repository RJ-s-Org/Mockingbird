//! The OIDC relying party: Authorization Code + PKCE login against the
//! configured provider.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openidconnect::core::{
    CoreAuthDisplay, CoreAuthPrompt, CoreAuthenticationFlow, CoreErrorResponseType,
    CoreGenderClaim, CoreIdTokenVerifier, CoreJsonWebKey, CoreJsonWebKeySet,
    CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm, CoreProviderMetadata,
    CoreRevocableToken, CoreRevocationErrorResponse, CoreTokenIntrospectionResponse, CoreTokenType,
};
use openidconnect::{
    AdditionalClaims, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EmptyExtraTokenFields,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, IdTokenFields, IssuerUrl, JsonWebKeySetUrl,
    Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl,
    RequestTokenError, Scope, StandardErrorResponse, StandardTokenResponse, TokenResponse,
    UserInfoClaims,
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
/// The longest `return_to` accepted, in bytes.
pub const MAX_RETURN_TO: usize = 2048;
/// Minimum time between JWKS refetches for ID-token verification.
const ID_TOKEN_JWKS_REFETCH: Duration = Duration::from_secs(30);

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

/// The resolved `auth.oidc` configuration. Its `Debug` output redacts
/// `client_secret`.
#[derive(Clone)]
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

impl std::fmt::Debug for OidcSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcSettings")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("scopes", &self.scopes)
            .field("audience", &self.audience)
            .field("redirect_url", &self.redirect_url)
            .field("mapping", &self.mapping)
            .finish()
    }
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

/// The keys ID tokens are verified with: the JWKS from discovery, replaced
/// when an ID token fails verification and the last refetch (if any) is
/// older than [`ID_TOKEN_JWKS_REFETCH`], so a provider key rotation does not
/// break logins until a restart.
struct IdTokenKeys {
    jwks_uri: JsonWebKeySetUrl,
    client_id: ClientId,
    client_secret: Option<ClientSecret>,
    issuer: IssuerUrl,
    algs: Vec<CoreJwsSigningAlgorithm>,
    /// The key set and when it was last refetched (`None`: only discovery).
    /// Never held across a fetch.
    cache: tokio::sync::RwLock<(CoreJsonWebKeySet, Option<Instant>)>,
    /// Held for the whole of a refetch (single flight).
    fetch_guard: tokio::sync::Mutex<()>,
}

impl IdTokenKeys {
    fn verifier(&self, jwks: CoreJsonWebKeySet) -> CoreIdTokenVerifier<'static> {
        let v = match &self.client_secret {
            Some(secret) => CoreIdTokenVerifier::new_confidential_client(
                self.client_id.clone(),
                secret.clone(),
                self.issuer.clone(),
                jwks,
            ),
            None => CoreIdTokenVerifier::new_public_client(
                self.client_id.clone(),
                self.issuer.clone(),
                jwks,
            ),
        };
        v.set_allowed_algs(self.algs.clone())
    }

    /// Refetches the key set unless a refetch happened within the
    /// throttle window (or another caller just refetched); returns whether
    /// the set may have changed.
    async fn refetch(&self, http: &reqwest::Client, seen: &CoreJsonWebKeySet) -> bool {
        let _fetching = self.fetch_guard.lock().await;
        {
            let mut cache = self.cache.write().await;
            if !same_keys(&cache.0, seen) {
                return true;
            }
            if cache.1.is_some_and(|t| t.elapsed() < ID_TOKEN_JWKS_REFETCH) {
                return false;
            }
            cache.1 = Some(Instant::now());
        }
        match CoreJsonWebKeySet::fetch_async(&self.jwks_uri, &OidcHttp(http)).await {
            Ok(jwks) => {
                self.cache.write().await.0 = jwks;
                true
            }
            Err(e) => {
                tracing::warn!("refetching the OIDC JWKS failed: {}", error_chain(&e));
                false
            }
        }
    }
}

fn same_keys(a: &CoreJsonWebKeySet, b: &CoreJsonWebKeySet) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

pub struct OidcClient {
    client: Client,
    id_keys: IdTokenKeys,
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
        let id_keys = IdTokenKeys {
            jwks_uri: metadata.jwks_uri().clone(),
            client_id: ClientId::new(settings.client_id.clone()),
            client_secret: settings.client_secret.clone().map(ClientSecret::new),
            issuer: metadata.issuer().clone(),
            algs: metadata.id_token_signing_alg_values_supported().clone(),
            cache: tokio::sync::RwLock::new((metadata.jwks().clone(), None)),
            fetch_guard: tokio::sync::Mutex::new(()),
        };

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
            id_keys,
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
        let invalid = |e: openidconnect::ClaimsVerificationError| {
            AuthError::Unauthenticated(format!("invalid ID token: {e}"))
        };
        let jwks = self.id_keys.cache.read().await.0.clone();
        let id_claims = match id_token.claims(&self.id_keys.verifier(jwks.clone()), &pending.nonce)
        {
            Ok(claims) => claims,
            // The provider may have rotated its keys since discovery.
            Err(e) => {
                if !self.id_keys.refetch(&self.http, &jwks).await {
                    return Err(invalid(e));
                }
                let jwks = self.id_keys.cache.read().await.0.clone();
                id_token
                    .claims(&self.id_keys.verifier(jwks), &pending.nonce)
                    .map_err(invalid)?
            }
        };
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

    /// Whether `state` identifies a login that was started and has neither
    /// completed nor expired.
    pub fn is_pending(&self, state: &str) -> bool {
        self.lock_pending()
            .get(state)
            .is_some_and(|p| p.created.elapsed() < PENDING_TTL)
    }

    /// Ends the login identified by `state` without completing it (the
    /// provider answered with an error); returns whether it was pending.
    pub fn abandon_login(&self, state: &str) -> bool {
        self.lock_pending()
            .remove(state)
            .is_some_and(|p| p.created.elapsed() < PENDING_TTL)
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
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
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
/// which would turn `/\t/evil` into `//evil`), and must be at most
/// [`MAX_RETURN_TO`] bytes (it is held in memory until the login ends).
pub fn validate_return_to(s: &str) -> Result<String, AuthError> {
    let ok = s.len() <= MAX_RETURN_TO
        && s.starts_with('/')
        && !s.starts_with("//")
        && !s.starts_with("/\\")
        && !s.chars().any(char::is_control);
    if ok {
        Ok(s.to_string())
    } else {
        Err(AuthError::Invalid(
            "return_to must be a path of at most 2048 bytes starting with a single `/`".to_string(),
        ))
    }
}
