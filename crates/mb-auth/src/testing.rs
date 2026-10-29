//! A wiremock OpenID provider for tests (feature `testing`).
//!
//! It serves discovery, a JWKS with one RSA key at a time, a token endpoint
//! that answers codes registered with [`MockOidcProvider::issue_code`] and a
//! userinfo endpoint for the access tokens it handed out. This is test
//! support: it panics on internal failures instead of returning errors.
#![expect(
    clippy::expect_used,
    reason = "test-support fixture (feature `testing`, dev-dependencies only) whose public API is documented to panic on internal failures; returning errors would break the published 0.1 signatures"
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use openidconnect::url::Url;
use rsa::RsaPrivateKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

struct SigningKey {
    kid: String,
    private: RsaPrivateKey,
    encoding: EncodingKey,
}

/// Test-only RSA keys (PKCS#8 PEM), checked in so tests do not pay for
/// 2048-bit key generation on every provider start. Under nextest every
/// test runs in its own process, so an in-process cache alone would not
/// help. These keys sign nothing but test tokens.
const POOL_PEMS: [&str; 4] = [
    include_str!("testing_keys/key-1.pem"),
    include_str!("testing_keys/key-2.pem"),
    include_str!("testing_keys/key-3.pem"),
    include_str!("testing_keys/key-4.pem"),
];

/// The parsed pool keys, each filled on first use.
static POOL: [OnceLock<(RsaPrivateKey, EncodingKey)>; 4] = [
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
];

/// Gives each provider started in this process its own starting slot in
/// the pool, so two providers in one test sign with different keys.
static NEXT_PROVIDER: AtomicUsize = AtomicUsize::new(0);

fn pool_key(slot: usize) -> (RsaPrivateKey, EncodingKey) {
    POOL[slot]
        .get_or_init(|| {
            let pem = POOL_PEMS[slot];
            let private =
                RsaPrivateKey::from_pkcs8_pem(pem).expect("parsing a pooled test RSA key");
            let encoding =
                EncodingKey::from_rsa_pem(pem.as_bytes()).expect("loading a pooled test RSA key");
            (private, encoding)
        })
        .clone()
}

impl SigningKey {
    /// A provider's `n`th key (1-based). Its first `POOL_PEMS.len()` keys
    /// come from the pool, starting at its slot `base`, so they differ
    /// from each other; later rotations generate fresh keys.
    fn for_provider(base: usize, n: usize) -> Self {
        let (private, encoding) = if (1..=POOL_PEMS.len()).contains(&n) {
            pool_key((base + n - 1) % POOL_PEMS.len())
        } else {
            Self::generate()
        };
        SigningKey {
            kid: format!("key-{n}"),
            private,
            encoding,
        }
    }

    fn generate() -> (RsaPrivateKey, EncodingKey) {
        let private =
            RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("generating an RSA key");
        let pem = private
            .to_pkcs8_pem(LineEnding::LF)
            .expect("encoding the RSA key as PKCS#8");
        let encoding =
            EncodingKey::from_rsa_pem(pem.as_bytes()).expect("loading the RSA key for signing");
        (private, encoding)
    }

    fn jwk(&self) -> Value {
        let public = self.private.to_public_key();
        json!({
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": self.kid,
            "n": URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            "e": URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
        })
    }
}

#[derive(Default)]
struct State {
    /// Signing keys in creation order; the last one is current and the
    /// only one the JWKS serves.
    keys: Vec<SigningKey>,
    /// Authorization code → ID token.
    codes: HashMap<String, String>,
    /// Access token → userinfo claims.
    userinfo: HashMap<String, Value>,
}

type Shared = Arc<Mutex<State>>;

fn lock(state: &Shared) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The discovery document of a provider at `issuer`.
fn discovery(issuer: &str) -> Value {
    json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "userinfo_endpoint": format!("{issuer}/userinfo"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "scopes_supported": ["openid", "profile", "email", "groups"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic"],
        "code_challenge_methods_supported": ["S256"],
    })
}

pub struct MockOidcProvider {
    pub server: MockServer,
    state: Shared,
    /// This provider's starting slot in the key pool.
    pool_base: usize,
}

impl MockOidcProvider {
    pub const CLIENT_ID: &'static str = "mockingbird";
    pub const CLIENT_SECRET: &'static str = "test-secret";

    /// Starts the provider with one signing key and every endpoint mounted.
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        let pool_base = NEXT_PROVIDER.fetch_add(1, Ordering::Relaxed) % POOL_PEMS.len();
        let key = SigningKey::for_provider(pool_base, 1);
        let state: Shared = Arc::new(Mutex::new(State {
            keys: vec![key],
            ..State::default()
        }));
        let issuer = server.uri();

        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(discovery(&issuer)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(Jwks(Arc::clone(&state)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(Token(Arc::clone(&state)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/userinfo"))
            .respond_with(UserInfo(Arc::clone(&state)))
            .mount(&server)
            .await;

        MockOidcProvider {
            server,
            state,
            pool_base,
        }
    }

    /// Makes discovery advertise `end_session_endpoint` `<issuer>/logout`
    /// (RP-initiated logout) to clients discovered from now on.
    pub async fn advertise_end_session(&self) {
        let issuer = self.issuer();
        let mut doc = discovery(&issuer);
        if let Some(m) = doc.as_object_mut() {
            m.insert(
                "end_session_endpoint".to_string(),
                json!(format!("{issuer}/logout")),
            );
        }
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(doc))
            .with_priority(1)
            .mount(&self.server)
            .await;
    }

    /// The issuer URL (the server's base URI, no trailing slash).
    pub fn issuer(&self) -> String {
        self.server.uri()
    }

    /// Signs `claims` with RS256 and the current key's `kid`.
    pub fn sign(&self, claims: Value) -> String {
        sign_current(&self.state, &claims)
    }

    /// Signs `claims` with an arbitrary header and key.
    pub fn sign_with(&self, header: Header, key: &EncodingKey, claims: Value) -> String {
        encode(&header, &claims, key).expect("signing a JWT")
    }

    /// Replaces the signing key; the JWKS serves only the new key from now
    /// on. Returns the new `kid`.
    pub async fn rotate_key(&self) -> String {
        let n = lock(&self.state).keys.len() + 1;
        let base = self.pool_base;
        let key = tokio::task::spawn_blocking(move || SigningKey::for_provider(base, n))
            .await
            .expect("key generation task");
        let kid = key.kid.clone();
        lock(&self.state).keys.push(key);
        kid
    }

    /// Makes the token endpoint answer `code` (once) with an ID token for
    /// `claims`; the access token it returns unlocks the same claims at
    /// the userinfo endpoint.
    pub async fn issue_code(&self, code: &str, claims: Value) {
        let id_token = self.sign(claims.clone());
        let mut info = claims;
        if let Some(m) = info.as_object_mut() {
            for c in ["iss", "aud", "exp", "iat", "nbf", "nonce", "auth_time"] {
                m.remove(c);
            }
        }
        let mut state = lock(&self.state);
        state.codes.insert(code.to_string(), id_token);
        state.userinfo.insert(access_token(code), info);
    }

    /// Standard claims for `sub`: `iss`, `aud = CLIENT_ID`, `iat` now,
    /// `exp` now + 300 s, `groups`, and `nonce` when given.
    pub fn claims(&self, sub: &str, groups: &[&str], nonce: Option<&str>) -> Value {
        let now = chrono::Utc::now().timestamp();
        let mut claims = json!({
            "iss": self.issuer(),
            "aud": Self::CLIENT_ID,
            "sub": sub,
            "iat": now,
            "exp": now + 300,
            "name": sub,
            "groups": groups,
        });
        if let (Some(nonce), Some(m)) = (nonce, claims.as_object_mut()) {
            m.insert("nonce".to_string(), json!(nonce));
        }
        claims
    }

    /// The first value of query parameter `name` in `url`.
    pub fn query_param(url: &str, name: &str) -> Option<String> {
        Url::parse(url)
            .ok()?
            .query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    }
}

fn access_token(code: &str) -> String {
    format!("access-{code}")
}

fn sign_current(state: &Shared, claims: &Value) -> String {
    let state = lock(state);
    let key = state.keys.last().expect("the provider always has a key");
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(key.kid.clone());
    encode(&header, claims, &key.encoding).expect("signing a JWT")
}

struct Jwks(Shared);

impl Respond for Jwks {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let state = lock(&self.0);
        let keys: Vec<Value> = state.keys.last().map(SigningKey::jwk).into_iter().collect();
        ResponseTemplate::new(200).set_body_json(json!({ "keys": keys }))
    }
}

struct Token(Shared);

impl Respond for Token {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let form: HashMap<String, String> =
            openidconnect::url::form_urlencoded::parse(&request.body)
                .into_owned()
                .collect();
        let error = |status: u16, code: &str| {
            ResponseTemplate::new(status).set_body_json(json!({ "error": code }))
        };

        let expected_auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!(
                "{}:{}",
                MockOidcProvider::CLIENT_ID,
                MockOidcProvider::CLIENT_SECRET
            ))
        );
        let auth_ok = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == expected_auth);
        if !auth_ok {
            return error(401, "invalid_client");
        }
        if form.get("grant_type").map(String::as_str) != Some("authorization_code")
            || form.get("code_verifier").is_none_or(String::is_empty)
        {
            return error(400, "invalid_request");
        }
        let Some(code) = form.get("code") else {
            return error(400, "invalid_request");
        };
        let Some(id_token) = lock(&self.0).codes.remove(code) else {
            return error(400, "invalid_grant");
        };
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access_token(code),
            "token_type": "Bearer",
            "expires_in": 300,
            "id_token": id_token,
        }))
    }
}

struct UserInfo(Shared);

impl Respond for UserInfo {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let token = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match token.and_then(|t| lock(&self.0).userinfo.get(t).cloned()) {
            Some(claims) => ResponseTemplate::new(200).set_body_json(claims),
            None => ResponseTemplate::new(401).set_body_json(json!({ "error": "invalid_token" })),
        }
    }
}
