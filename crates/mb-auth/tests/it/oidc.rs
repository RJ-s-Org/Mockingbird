#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use mb_auth::testing::MockOidcProvider;
use mb_auth::{
    AuthError, BearerVerifier, FinishedLogin, JwtVerifier, OidcClient, OidcSettings, Principal,
    PrincipalKind, Role, RoleMapping, SessionManager, http_client, validate_return_to,
};
use mb_store::ControlStore;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn settings(p: &MockOidcProvider) -> OidcSettings {
    OidcSettings {
        issuer: p.issuer(),
        client_id: MockOidcProvider::CLIENT_ID.to_string(),
        client_secret: Some(MockOidcProvider::CLIENT_SECRET.to_string()),
        scopes: vec!["openid".into(), "profile".into(), "email".into()],
        audience: None,
        redirect_url: "http://127.0.0.1:9999/auth/callback".to_string(),
        mapping: RoleMapping::default(),
    }
}

async fn store() -> Arc<dyn ControlStore> {
    mb_store::connect("sqlite::memory:").await.unwrap()
}

/// A client keeping its pending logins in `store`, as every node does.
async fn store_client(p: &MockOidcProvider, store: &Arc<dyn ControlStore>) -> OidcClient {
    client(p).await.with_store(store.clone())
}

async fn client(p: &MockOidcProvider) -> OidcClient {
    OidcClient::discover(settings(p), http_client())
        .await
        .unwrap()
}

fn verifier(p: &MockOidcProvider) -> JwtVerifier {
    JwtVerifier::new(
        p.issuer(),
        MockOidcProvider::CLIENT_ID.to_string(),
        format!("{}/jwks", p.issuer()),
        RoleMapping::default(),
        http_client(),
    )
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A syntactically valid JWT whose signature is garbage.
fn forge(header: Value, claims: &Value) -> String {
    let h = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
    let c = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
    format!("{h}.{c}.c2lnbmF0dXJl")
}

fn assert_unauthenticated<T: std::fmt::Debug>(r: Result<T, AuthError>) {
    match r {
        Err(AuthError::Unauthenticated(_)) => {}
        other => panic!("expected Unauthenticated, got {other:?}"),
    }
}

async fn jwks_requests(p: &MockOidcProvider) -> usize {
    p.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/jwks")
        .count()
}

/// Waits (at most 5 s) until the provider has received `n` JWKS requests.
/// wiremock records a request when it arrives, before any response delay.
async fn wait_for_jwks_requests(p: &MockOidcProvider, n: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while jwks_requests(p).await < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {n} JWKS requests (saw {})",
            jwks_requests(p).await
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn discovery_and_login_round_trip() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    let login = c.start_login("/mocks").await.unwrap();

    assert!(login.url.starts_with(&format!("{}/authorize?", p.issuer())));
    assert_eq!(
        MockOidcProvider::query_param(&login.url, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert!(MockOidcProvider::query_param(&login.url, "code_challenge").is_some());
    assert_eq!(
        MockOidcProvider::query_param(&login.url, "state").as_deref(),
        Some(login.state.as_str())
    );
    assert_eq!(
        MockOidcProvider::query_param(&login.url, "client_id").as_deref(),
        Some(MockOidcProvider::CLIENT_ID)
    );
    let scope = MockOidcProvider::query_param(&login.url, "scope").unwrap();
    assert_eq!(scope.split(' ').filter(|s| *s == "openid").count(), 1);
    assert!(scope.split(' ').any(|s| s == "profile"));
    let nonce = MockOidcProvider::query_param(&login.url, "nonce").unwrap();

    p.issue_code(
        "c1",
        p.claims(
            "alice",
            &["mb:default:editor", "mb:team-a:viewer"],
            Some(&nonce),
        ),
    )
    .await;
    let done = c.complete_login(&login.state, "c1").await.unwrap();

    assert_eq!(done.return_to, "/mocks");
    assert_eq!(done.principal.subject, "alice");
    assert_eq!(done.principal.kind, PrincipalKind::User);
    assert!(!done.principal.sysadmin);
    assert_eq!(done.principal.roles.get("default"), Some(&Role::Editor));
    assert_eq!(done.principal.roles.get("team-a"), Some(&Role::Viewer));
}

#[tokio::test]
async fn roles_from_userinfo_when_absent_in_id_token() {
    let p = MockOidcProvider::start().await;
    Mock::given(method("GET"))
        .and(path("/userinfo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"sub": "bob", "groups": ["mb:default:admin"]})),
        )
        .with_priority(1)
        .mount(&p.server)
        .await;
    let c = client(&p).await;
    let login = c.start_login("/").await.unwrap();
    let nonce = MockOidcProvider::query_param(&login.url, "nonce").unwrap();
    let mut claims = p.claims("bob", &[], Some(&nonce));
    claims.as_object_mut().unwrap().remove("groups");
    p.issue_code("c1", claims).await;

    let done = c.complete_login(&login.state, "c1").await.unwrap();
    assert_eq!(done.principal.roles.get("default"), Some(&Role::Admin));
}

#[tokio::test]
async fn state_mismatch_rejected() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    let login = c.start_login("/").await.unwrap();
    let nonce = MockOidcProvider::query_param(&login.url, "nonce").unwrap();
    p.issue_code("c1", p.claims("alice", &[], Some(&nonce)))
        .await;
    assert_unauthenticated(c.complete_login("not-the-state", "c1").await);
}

#[tokio::test]
async fn state_is_single_use() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    let login = c.start_login("/").await.unwrap();
    let nonce = MockOidcProvider::query_param(&login.url, "nonce").unwrap();
    p.issue_code("c1", p.claims("alice", &[], Some(&nonce)))
        .await;
    p.issue_code("c2", p.claims("alice", &[], Some(&nonce)))
        .await;
    c.complete_login(&login.state, "c1").await.unwrap();
    assert_unauthenticated(c.complete_login(&login.state, "c2").await);
}

#[tokio::test]
async fn nonce_mismatch_rejected() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    let login = c.start_login("/").await.unwrap();
    p.issue_code("c1", p.claims("alice", &[], Some("another-nonce")))
        .await;
    assert_unauthenticated(c.complete_login(&login.state, "c1").await);
}

#[tokio::test]
async fn return_to_validation() {
    assert_eq!(validate_return_to("/").unwrap(), "/");
    assert_eq!(
        validate_return_to("/w/default/mocks?x=1").unwrap(),
        "/w/default/mocks?x=1"
    );
    for bad in [
        "//evil.example",
        "/\\evil.example",
        "https://evil.example",
        "javascript:alert(1)",
        "",
        "/\t/evil.example",
        &format!("/{}", "a".repeat(2048)),
    ] {
        assert!(
            matches!(validate_return_to(bad), Err(AuthError::Invalid(_))),
            "{bad:?} accepted"
        );
    }

    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    assert!(matches!(
        c.start_login("//evil.example").await,
        Err(AuthError::Invalid(_))
    ));
}

#[test]
fn return_to_length_bound() {
    let max = format!("/{}", "a".repeat(2047));
    assert_eq!(validate_return_to(&max).unwrap(), max);
    let long = format!("/{}", "a".repeat(2048));
    assert!(matches!(
        validate_return_to(&long),
        Err(AuthError::Invalid(_))
    ));
}

/// Runs one login with `sub` and returns its outcome.
async fn login(
    p: &MockOidcProvider,
    c: &OidcClient,
    code: &str,
) -> Result<mb_auth::CompletedLogin, AuthError> {
    let login = c.start_login("/").await.unwrap();
    let nonce = MockOidcProvider::query_param(&login.url, "nonce").unwrap();
    p.issue_code(code, p.claims("alice", &[], Some(&nonce)))
        .await;
    c.complete_login(&login.state, code).await
}

#[tokio::test]
async fn login_after_key_rotation() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    login(&p, &c, "c1").await.unwrap();
    p.rotate_key().await;
    let done = login(&p, &c, "c2").await.unwrap();
    assert_eq!(done.principal.subject, "alice");
    // The new key is cached: the next login fetches nothing.
    let before = jwks_requests(&p).await;
    login(&p, &c, "c3").await.unwrap();
    assert_eq!(jwks_requests(&p).await, before);
}

#[tokio::test]
async fn id_token_jwks_refetch_throttled() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    p.rotate_key().await;
    login(&p, &c, "c1").await.unwrap();
    let after_first = jwks_requests(&p).await;
    // A second rotation within the throttle window is not refetched.
    p.rotate_key().await;
    assert_unauthenticated(login(&p, &c, "c2").await);
    assert_unauthenticated(login(&p, &c, "c3").await);
    assert_eq!(jwks_requests(&p).await, after_first);
}

#[tokio::test]
async fn bearer_jwt_accepted() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    let token = p.sign(p.claims("carol", &["mb:sysadmin"], None));
    let principal = c.bearer().verify(&token).await.unwrap();
    assert_eq!(principal.subject, "carol");
    assert_eq!(principal.kind, PrincipalKind::User);
    assert!(principal.sysadmin);
}

#[tokio::test]
async fn bearer_wrong_issuer_or_audience_401() {
    let p = MockOidcProvider::start().await;
    let v = verifier(&p);

    let mut claims = p.claims("carol", &[], None);
    claims["iss"] = json!("https://other.example");
    assert_unauthenticated(v.verify(&p.sign(claims)).await);

    let mut claims = p.claims("carol", &[], None);
    claims["aud"] = json!("someone-else");
    assert_unauthenticated(v.verify(&p.sign(claims)).await);

    // A configured audience replaces the client id.
    let p2 = MockOidcProvider::start().await;
    let mut s = settings(&p2);
    s.audience = Some("mockingbird-api".to_string());
    let c = OidcClient::discover(s, http_client()).await.unwrap();
    assert_unauthenticated(c.bearer().verify(&p2.sign(p2.claims("x", &[], None))).await);
    let mut claims = p2.claims("x", &[], None);
    claims["aud"] = json!(["mockingbird-api", "other"]);
    c.bearer().verify(&p2.sign(claims)).await.unwrap();
}

#[tokio::test]
async fn expired_jwt_401() {
    let p = MockOidcProvider::start().await;
    let mut claims = p.claims("carol", &[], None);
    claims["exp"] = json!(now() - 120);
    claims["iat"] = json!(now() - 600);
    assert_unauthenticated(verifier(&p).verify(&p.sign(claims)).await);
}

#[tokio::test]
async fn jwt_within_leeway_ok() {
    let p = MockOidcProvider::start().await;
    let mut claims = p.claims("carol", &[], None);
    claims["exp"] = json!(now() - 30);
    claims["iat"] = json!(now() - 600);
    verifier(&p).verify(&p.sign(claims)).await.unwrap();
}

#[tokio::test]
async fn not_yet_valid_jwt_401() {
    let p = MockOidcProvider::start().await;
    let mut claims = p.claims("carol", &[], None);
    claims["nbf"] = json!(now() + 120);
    assert_unauthenticated(verifier(&p).verify(&p.sign(claims)).await);
}

#[tokio::test]
async fn alg_none_and_hs256_rejected() {
    let p = MockOidcProvider::start().await;
    let v = verifier(&p);
    let claims = p.claims("carol", &["mb:sysadmin"], None);

    let none = forge(json!({"alg": "none", "typ": "JWT", "kid": "k1"}), &claims);
    let none = none.rsplit_once('.').unwrap().0.to_string() + ".";
    assert_unauthenticated(v.verify(&none).await);

    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("k1".into());
    let hs = p.sign_with(header, &EncodingKey::from_secret(b"secret"), claims.clone());
    assert_unauthenticated(v.verify(&hs).await);

    // A token without `kid` is rejected too.
    let unkeyed = p.sign_with(
        Header::new(Algorithm::HS256),
        &EncodingKey::from_secret(b"secret"),
        claims,
    );
    assert_unauthenticated(v.verify(&unkeyed).await);
}

#[tokio::test]
async fn jwks_rotation_refetches() {
    let p = MockOidcProvider::start().await;
    let v = verifier(&p).with_refetch_interval(Duration::from_millis(100));
    v.verify(&p.sign(p.claims("carol", &[], None)))
        .await
        .unwrap();

    let kid2 = p.rotate_key().await;
    let token = p.sign(p.claims("carol", &[], None));
    let header = jsonwebtoken::decode_header(&token).unwrap();
    assert_eq!(header.kid.as_deref(), Some(kid2.as_str()));

    tokio::time::sleep(Duration::from_millis(150)).await;
    v.verify(&token).await.unwrap();
    assert_eq!(jwks_requests(&p).await, 2);
}

#[tokio::test]
async fn unknown_kid_refetch_throttled() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await; // discovery fetches JWKS once
    let v: Arc<JwtVerifier> = c.bearer();
    let claims = p.claims("carol", &[], None);
    for kid in ["unknown-1", "unknown-2"] {
        let token = forge(json!({"alg": "RS256", "typ": "JWT", "kid": kid}), &claims);
        assert_unauthenticated(v.verify(&token).await);
    }
    assert_eq!(jwks_requests(&p).await, 2);
}

#[tokio::test]
async fn jwks_refetch_does_not_block_cached_kids() {
    let p = MockOidcProvider::start().await;
    let v = Arc::new(verifier(&p).with_refetch_interval(Duration::ZERO));
    let cached = p.sign(p.claims("carol", &[], None));
    v.verify(&cached).await.unwrap();

    // From now on the JWKS endpoint takes 30 s to answer; the test never
    // waits for it.
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(500).set_delay(Duration::from_secs(30)))
        .with_priority(1)
        .mount(&p.server)
        .await;
    assert_eq!(jwks_requests(&p).await, 1);
    p.rotate_key().await;
    let unknown = p.sign(p.claims("carol", &[], None));
    let slow = {
        let v = Arc::clone(&v);
        tokio::spawn(async move { v.verify(&unknown).await })
    };
    // The refetch has reached the provider and is waiting for its answer.
    wait_for_jwks_requests(&p, 2).await;
    assert!(!slow.is_finished());

    let started = std::time::Instant::now();
    for _ in 0..5 {
        v.verify(&cached).await.unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cached-kid verifications waited {:?} for the refetch",
        started.elapsed()
    );
    // They ran while the refetch was still in flight, and started no other.
    assert!(!slow.is_finished());
    assert_eq!(jwks_requests(&p).await, 2);
    slow.abort();
}

#[tokio::test]
async fn failed_jwks_refetch_is_upstream_error() {
    let p = MockOidcProvider::start().await;
    let v = verifier(&p).with_refetch_interval(Duration::ZERO);
    v.verify(&p.sign(p.claims("carol", &[], None)))
        .await
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&p.server)
        .await;
    p.rotate_key().await;
    let unknown = p.sign(p.claims("carol", &[], None));
    assert!(matches!(
        v.verify(&unknown).await,
        Err(AuthError::Upstream(_))
    ));
    assert_eq!(jwks_requests(&p).await, 2);
}

#[tokio::test]
async fn providers_and_rotations_sign_with_distinct_keys() {
    let jwk = |p: &MockOidcProvider| {
        let token = p.sign(p.claims("carol", &[], None));
        let p_uri = p.issuer();
        async move {
            let jwks: Value = reqwest::get(format!("{p_uri}/jwks"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(jwks["keys"].as_array().map(Vec::len), Some(1));
            let kid = jsonwebtoken::decode_header(&token).unwrap().kid.unwrap();
            assert_eq!(jwks["keys"][0]["kid"], json!(kid));
            jwks["keys"][0]["n"].as_str().unwrap().to_string()
        }
    };
    let a = MockOidcProvider::start().await;
    let b = MockOidcProvider::start().await;
    let mut moduli = vec![jwk(&a).await, jwk(&b).await];
    for _ in 0..4 {
        a.rotate_key().await;
        moduli.push(jwk(&a).await);
    }
    // Two providers in one process, and a provider across its first five
    // keys (the fifth is generated, not pooled), never share a key.
    let a_keys = [&moduli[..1], &moduli[2..]].concat();
    let mut unique = a_keys.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), a_keys.len(), "provider reused a key");
    assert_ne!(moduli[0], moduli[1], "two providers share a key");
}

#[tokio::test]
async fn token_signed_by_another_provider_with_same_kid_401() {
    // Both providers' first key has kid `key-1`; only the key differs.
    let a = MockOidcProvider::start().await;
    let b = MockOidcProvider::start().await;
    let mut claims = b.claims("carol", &[], None);
    claims["iss"] = json!(b.issuer());
    let forged = a.sign(claims);
    assert_eq!(
        jsonwebtoken::decode_header(&forged).unwrap().kid.as_deref(),
        Some("key-1")
    );
    assert_unauthenticated(verifier(&b).verify(&forged).await);
    verifier(&b)
        .verify(&b.sign(b.claims("carol", &[], None)))
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_unknown_kids_share_one_refetch() {
    let p = MockOidcProvider::start().await;
    let v = Arc::new(verifier(&p).with_refetch_interval(Duration::from_secs(30)));
    v.verify(&p.sign(p.claims("carol", &[], None)))
        .await
        .unwrap();
    assert_eq!(jwks_requests(&p).await, 1);
    // Within the throttle window an unknown kid is rejected without a
    // fetch; many at once still cause none.
    p.rotate_key().await;
    let token = p.sign(p.claims("carol", &[], None));
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let v = Arc::clone(&v);
            let token = token.clone();
            tokio::spawn(async move { v.verify(&token).await })
        })
        .collect();
    for t in tasks {
        assert_unauthenticated(t.await.unwrap());
    }
    assert_eq!(jwks_requests(&p).await, 1);
}

#[tokio::test]
async fn store_backed_login_survives_a_restart_and_crosses_nodes() {
    let p = MockOidcProvider::start().await;
    let shared = store().await;
    // Node A starts the login, then goes away (a restart, or the callback
    // reaching another replica): node B shares only the store.
    let a = store_client(&p, &shared).await;
    let login = a.start_login("/w/default/mocks").await.unwrap();
    drop(a);
    let b = store_client(&p, &shared).await;
    assert!(b.login_pending(&login.state).await.unwrap());

    let nonce = MockOidcProvider::query_param(&login.url, "nonce").unwrap();
    p.issue_code(
        "c1",
        p.claims("alice", &["mb:default:viewer"], Some(&nonce)),
    )
    .await;
    let done = b.complete_login(&login.state, "c1").await.unwrap();
    assert_eq!(done.return_to, "/w/default/mocks");
    assert_eq!(done.principal.subject, "alice");
    assert!(!b.login_pending(&login.state).await.unwrap());
}

#[tokio::test]
async fn store_backed_state_is_single_use_across_nodes() {
    let p = MockOidcProvider::start().await;
    let shared = store().await;
    let a = store_client(&p, &shared).await;
    let b = store_client(&p, &shared).await;
    let login = a.start_login("/").await.unwrap();
    let nonce = MockOidcProvider::query_param(&login.url, "nonce").unwrap();
    p.issue_code("c1", p.claims("alice", &[], Some(&nonce)))
        .await;
    p.issue_code("c2", p.claims("alice", &[], Some(&nonce)))
        .await;
    b.complete_login(&login.state, "c1").await.unwrap();
    assert_unauthenticated(a.complete_login(&login.state, "c2").await);
    assert_unauthenticated(b.complete_login(&login.state, "c2").await);
}

#[tokio::test]
async fn store_backed_login_can_be_cancelled_once() {
    let p = MockOidcProvider::start().await;
    let shared = store().await;
    let a = store_client(&p, &shared).await;
    let b = store_client(&p, &shared).await;
    let login = a.start_login("/").await.unwrap();
    assert!(b.cancel_login(&login.state).await.unwrap());
    assert!(!a.cancel_login(&login.state).await.unwrap());
    assert!(!a.login_pending(&login.state).await.unwrap());
    assert!(!a.login_pending("never-started").await.unwrap());
}

#[tokio::test]
async fn store_backed_pending_login_keeps_no_plain_state() {
    let p = MockOidcProvider::start().await;
    let shared = store().await;
    let c = store_client(&p, &shared).await;
    let login = c.start_login("/").await.unwrap();
    // Rows are keyed by the state's hash, never the state itself.
    let now = chrono::Utc::now();
    assert!(
        shared
            .get_oidc_pending_login(&login.state, now)
            .await
            .unwrap()
            .is_none()
    );
    use sha2::Digest;
    let hash: String = sha2::Sha256::digest(login.state.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let row = shared
        .get_oidc_pending_login(&hash, now)
        .await
        .unwrap()
        .unwrap();
    let ttl = row.expires_at - row.created_at;
    assert_eq!(ttl.num_seconds(), 600);
    assert_eq!(row.data["return_to"], "/");
}

#[tokio::test]
#[expect(deprecated, reason = "checks the in-memory methods' refusal")]
async fn in_memory_methods_refuse_a_store_backed_client() {
    let p = MockOidcProvider::start().await;
    let shared = store().await;
    let c = store_client(&p, &shared).await;
    match c.begin_login("/") {
        Err(AuthError::Invalid(msg)) => assert!(msg.contains("start_login"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
    let login = c.start_login("/").await.unwrap();
    assert!(!c.is_pending(&login.state));
    assert!(!c.abandon_login(&login.state));
    assert!(c.login_pending(&login.state).await.unwrap());

    // Without a store, the old methods keep working in memory.
    let m = client(&p).await;
    let login = m.begin_login("/").unwrap();
    assert!(m.is_pending(&login.state));
    assert!(m.login_pending(&login.state).await.unwrap());
    assert!(m.abandon_login(&login.state));
}

#[tokio::test]
async fn end_session_url_from_discovery() {
    let p = MockOidcProvider::start().await;
    // The default mock provider advertises no end_session_endpoint.
    assert_eq!(client(&p).await.end_session_url(), None);

    p.advertise_end_session().await;
    let c = client(&p)
        .await
        .with_post_logout_redirect("http://127.0.0.1:9999/")
        .unwrap();
    let url = c.end_session_url().unwrap();
    assert!(url.starts_with(&format!("{}/logout?", p.issuer())), "{url}");
    assert_eq!(
        MockOidcProvider::query_param(&url, "client_id").as_deref(),
        Some(MockOidcProvider::CLIENT_ID)
    );
    assert_eq!(
        MockOidcProvider::query_param(&url, "post_logout_redirect_uri").as_deref(),
        Some("http://127.0.0.1:9999/")
    );

    let bare = client(&p).await.end_session_url().unwrap();
    assert_eq!(
        MockOidcProvider::query_param(&bare, "post_logout_redirect_uri"),
        None
    );
    assert!(matches!(
        client(&p).await.with_post_logout_redirect("not a url"),
        Err(AuthError::Invalid(_))
    ));
}

#[tokio::test]
async fn oidc_client_and_verifier_debug_show_no_secret() {
    let p = MockOidcProvider::start().await;
    let sentinel = "s3ntinel-oidc-client-secret";
    let mut s = settings(&p);
    s.client_secret = Some(sentinel.to_string());
    let c = OidcClient::discover(s, http_client()).await.unwrap();
    let login = c.start_login("/").await.unwrap();
    let shown = format!("{c:?} {:?} {p:?}", c.bearer());
    assert!(!shown.contains(sentinel), "client secret leaked: {shown}");
    assert!(
        !shown.contains(&login.state),
        "pending login leaked: {shown}"
    );
    assert!(shown.contains(MockOidcProvider::CLIENT_ID), "{shown}");
}

/// A finished login of `alice` with `groups`, the provider handing out
/// `refresh` as its refresh token when given.
async fn finished(
    p: &MockOidcProvider,
    c: &OidcClient,
    code: &str,
    groups: &[&str],
    refresh: Option<&str>,
) -> FinishedLogin {
    let login = c.start_login("/").await.unwrap();
    let nonce = MockOidcProvider::query_param(&login.url, "nonce").unwrap();
    let claims = p.claims("alice", groups, Some(&nonce));
    match refresh {
        Some(rt) => p.issue_code_with_refresh(code, claims, rt).await,
        None => p.issue_code(code, claims).await,
    }
    c.finish_login(&login.state, code).await.unwrap()
}

fn role(p: &Principal, ws: &str) -> Option<Role> {
    p.roles.get(ws).copied()
}

#[tokio::test]
async fn finish_login_returns_the_grant() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    let done = finished(&p, &c, "fin-1", &["mb:default:viewer"], Some("rt-fin")).await;
    assert_eq!(role(&done.principal, "default"), Some(Role::Viewer));
    assert_eq!(done.return_to, "/");
    assert!(done.grant.has_refresh_token());
    assert_eq!(done.grant.claims()["sub"], "alice");
    let shown = format!("{done:?}");
    assert!(!shown.contains("rt-fin"), "refresh token leaked: {shown}");
    let done = finished(&p, &c, "fin-2", &[], None).await;
    assert!(!done.grant.has_refresh_token());
}

#[tokio::test]
async fn session_roles_follow_a_mapping_change() {
    let p = MockOidcProvider::start().await;
    let store = store().await;
    let before = Arc::new(store_client(&p, &store).await);
    let sessions = SessionManager::new(store.clone(), Duration::from_secs(3600), false)
        .with_role_refresh(before.clone());
    let groups = ["mb:default:viewer", "team:default:admin"];
    let done = finished(&p, &before, "map-1", &groups, None).await;
    let cookie = sessions.create_from_login(&done).await.unwrap();
    let plain = sessions.create(&done.principal).await.unwrap();
    let got = sessions.lookup(&cookie).await.unwrap().unwrap();
    assert_eq!(role(&got, "default"), Some(Role::Viewer));

    // The operator switches the group prefix (a config change and restart).
    let mut s = settings(&p);
    s.mapping.group_prefix = "team".to_string();
    let after = Arc::new(OidcClient::discover(s, http_client()).await.unwrap());
    let restarted = SessionManager::new(store.clone(), Duration::from_secs(3600), false)
        .with_role_refresh(after);
    let got = restarted.lookup(&cookie).await.unwrap().unwrap();
    assert_eq!(role(&got, "default"), Some(Role::Admin));
    // Stored for every node, and the provider was not asked.
    let row = store
        .get_admin_session(&SessionManager::session_id(&cookie))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.principal["roles"]["default"], "admin");
    assert_eq!(p.refresh_requests(), 0);
    // A session without stored claims keeps the roles of its login.
    let got = restarted.lookup(&plain).await.unwrap().unwrap();
    assert_eq!(role(&got, "default"), Some(Role::Viewer));
}

#[tokio::test]
async fn session_roles_refresh_with_the_refresh_token() {
    let p = MockOidcProvider::start().await;
    let store = store().await;
    let c = Arc::new(store_client(&p, &store).await);
    let sessions = SessionManager::new(store.clone(), Duration::from_secs(3600), false)
        .with_role_refresh(c.clone())
        .with_role_refresh_interval(Duration::ZERO);
    let done = finished(&p, &c, "ref-1", &["mb:default:viewer"], Some("rt-alice")).await;
    let cookie = sessions.create_from_login(&done).await.unwrap();

    // The refresh token is stored sealed.
    let id = SessionManager::session_id(&cookie);
    let (_, grant) = store
        .get_admin_session_with_grant(&id)
        .await
        .unwrap()
        .unwrap();
    let sealed = grant.unwrap().refresh_token.unwrap();
    assert!(!sealed.contains("rt-alice"), "{sealed}");

    // The user's groups change at the provider.
    p.set_refresh_claims("rt-alice", p.claims("alice", &["mb:default:editor"], None));
    let got = sessions.lookup(&cookie).await.unwrap().unwrap();
    assert_eq!(role(&got, "default"), Some(Role::Editor));
    assert_eq!(p.refresh_requests(), 1);
    let got = sessions.lookup(&cookie).await.unwrap().unwrap();
    assert_eq!(role(&got, "default"), Some(Role::Editor));
    assert_eq!(p.refresh_requests(), 2);

    // The provider revokes the grant: the session ends.
    p.revoke_refresh("rt-alice");
    assert_eq!(sessions.lookup(&cookie).await.unwrap(), None);
    assert!(store.get_admin_session(&id).await.unwrap().is_none());
}

#[tokio::test]
async fn role_refresh_waits_for_its_interval_and_needs_a_refresh_token() {
    let p = MockOidcProvider::start().await;
    let store = store().await;
    let c = Arc::new(store_client(&p, &store).await);
    let hourly = SessionManager::new(store.clone(), Duration::from_secs(3600), false)
        .with_role_refresh(c.clone())
        .with_role_refresh_interval(Duration::from_secs(3600));
    let done = finished(&p, &c, "wait-1", &["mb:default:viewer"], Some("rt-wait")).await;
    let cookie = hourly.create_from_login(&done).await.unwrap();
    p.set_refresh_claims("rt-wait", p.claims("alice", &["mb:default:owner"], None));
    let got = hourly.lookup(&cookie).await.unwrap().unwrap();
    assert_eq!(role(&got, "default"), Some(Role::Viewer));
    assert_eq!(p.refresh_requests(), 0);

    // Without a refresh token there is nothing to refresh with.
    let always = SessionManager::new(store.clone(), Duration::from_secs(3600), false)
        .with_role_refresh(c.clone())
        .with_role_refresh_interval(Duration::ZERO);
    let done = finished(&p, &c, "wait-2", &["mb:default:viewer"], None).await;
    let cookie = always.create_from_login(&done).await.unwrap();
    let got = always.lookup(&cookie).await.unwrap().unwrap();
    assert_eq!(role(&got, "default"), Some(Role::Viewer));
    assert_eq!(p.refresh_requests(), 0);
}

#[tokio::test]
async fn refreshed_id_token_for_another_subject_ends_the_session() {
    let p = MockOidcProvider::start().await;
    let store = store().await;
    let c = Arc::new(store_client(&p, &store).await);
    let sessions = SessionManager::new(store.clone(), Duration::from_secs(3600), false)
        .with_role_refresh(c.clone())
        .with_role_refresh_interval(Duration::ZERO);
    let done = finished(&p, &c, "sub-1", &["mb:default:viewer"], Some("rt-sub")).await;
    let cookie = sessions.create_from_login(&done).await.unwrap();
    p.set_refresh_claims("rt-sub", p.claims("mallory", &["mb:sysadmin"], None));
    assert_eq!(sessions.lookup(&cookie).await.unwrap(), None);
}

#[tokio::test]
async fn unreachable_provider_keeps_the_roles() {
    let p = MockOidcProvider::start().await;
    let store = store().await;
    let c = Arc::new(store_client(&p, &store).await);
    let sessions = SessionManager::new(store.clone(), Duration::from_secs(3600), false)
        .with_role_refresh(c.clone())
        .with_role_refresh_interval(Duration::ZERO);
    let done = finished(&p, &c, "down-1", &["mb:default:viewer"], Some("rt-down")).await;
    let cookie = sessions.create_from_login(&done).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&p.server)
        .await;
    let got = sessions.lookup(&cookie).await.unwrap().unwrap();
    assert_eq!(role(&got, "default"), Some(Role::Viewer));
}
