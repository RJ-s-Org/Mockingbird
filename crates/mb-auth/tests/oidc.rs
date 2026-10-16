#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use mb_auth::testing::MockOidcProvider;
use mb_auth::{
    AuthError, BearerVerifier, JwtVerifier, OidcClient, OidcSettings, PrincipalKind, Role,
    RoleMapping, http_client, validate_return_to,
};
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

#[tokio::test]
async fn discovery_and_login_round_trip() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    let login = c.begin_login("/mocks").unwrap();

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
    let login = c.begin_login("/").unwrap();
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
    let login = c.begin_login("/").unwrap();
    let nonce = MockOidcProvider::query_param(&login.url, "nonce").unwrap();
    p.issue_code("c1", p.claims("alice", &[], Some(&nonce)))
        .await;
    assert_unauthenticated(c.complete_login("not-the-state", "c1").await);
}

#[tokio::test]
async fn state_is_single_use() {
    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    let login = c.begin_login("/").unwrap();
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
    let login = c.begin_login("/").unwrap();
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
    ] {
        assert!(
            matches!(validate_return_to(bad), Err(AuthError::Invalid(_))),
            "{bad:?} accepted"
        );
    }

    let p = MockOidcProvider::start().await;
    let c = client(&p).await;
    assert!(matches!(
        c.begin_login("//evil.example"),
        Err(AuthError::Invalid(_))
    ));
}

/// Runs one login with `sub` and returns its outcome.
async fn login(
    p: &MockOidcProvider,
    c: &OidcClient,
    code: &str,
) -> Result<mb_auth::CompletedLogin, AuthError> {
    let login = c.begin_login("/").unwrap();
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
