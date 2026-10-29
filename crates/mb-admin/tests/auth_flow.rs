//! Console login over OIDC (Authorization Code + PKCE against a wiremock
//! provider), admin sessions, logout, `/api/v1/me`, bearer JWTs and the
//! console gate.
#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::get;
use mb_admin::{AdminState, LOGIN_STATE_COOKIE, WebAuth, console_gate, router};
use mb_auth::testing::MockOidcProvider;
use mb_auth::{
    ApiKeys, Authenticator, OidcClient, OidcSettings, RoleMapping, SESSION_COOKIE, SessionManager,
    http_client,
};
use mb_state::{MemoryStore, StateStore};
use mb_store::{AuditFilter, ControlStore};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    app: Router,
    st: AdminState,
    provider: MockOidcProvider,
    store: Arc<dyn ControlStore>,
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.text))
    }

    fn problem(&self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "body: {}", self.text);
        assert_eq!(
            self.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        self.json()
    }

    fn location(&self) -> String {
        assert_eq!(self.status, StatusCode::FOUND, "body: {}", self.text);
        self.headers[header::LOCATION].to_str().unwrap().to_string()
    }

    fn set_cookies(&self) -> Vec<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    }

    /// The `Set-Cookie` value for cookie `name`.
    fn set_cookie(&self, name: &str) -> Option<String> {
        self.set_cookies()
            .into_iter()
            .find(|c| c.starts_with(&format!("{name}=")))
    }
}

/// The value part of a `Set-Cookie` header (`name=value; ...`).
fn cookie_value(set_cookie: &str) -> String {
    let pair = set_cookie.split(';').next().unwrap();
    pair.split_once('=').unwrap().1.to_string()
}

async fn send(app: &Router, req: Request<Body>) -> Resp {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    Resp {
        status,
        headers,
        text: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn get_with(app: &Router, uri: &str, headers: &[(&str, &str)]) -> Resp {
    let mut req = Request::get(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    send(app, req.body(Body::empty()).unwrap()).await
}

fn session_header(value: &str) -> String {
    format!("{SESSION_COOKIE}={value}")
}

async fn stores() -> (Arc<dyn ControlStore>, Arc<dyn StateStore>) {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    (store, state)
}

/// An `oidc`-mode admin router against a fresh [`MockOidcProvider`];
/// `secure` stands for an `https://` external URL.
async fn harness_with(secure: bool) -> Harness {
    let provider = MockOidcProvider::start().await;
    let (store, state) = stores().await;
    let settings = OidcSettings {
        issuer: provider.issuer(),
        client_id: MockOidcProvider::CLIENT_ID.to_string(),
        client_secret: Some(MockOidcProvider::CLIENT_SECRET.to_string()),
        scopes: vec!["openid".into(), "profile".into(), "groups".into()],
        audience: None,
        redirect_url: "http://127.0.0.1:9090/auth/callback".to_string(),
        mapping: RoleMapping::default(),
    };
    let oidc = Arc::new(OidcClient::discover(settings, http_client()).await.unwrap());
    let sessions = SessionManager::new(store.clone(), Duration::from_secs(3600), secure);
    let auth = Authenticator {
        api_keys: ApiKeys::new(store.clone()),
        sessions: Some(sessions.clone()),
        bearer: Some(oidc.bearer()),
    };
    let st = AdminState {
        auth: Arc::new(auth),
        web_auth: Some(WebAuth {
            oidc,
            sessions,
            secure_cookies: secure,
        }),
        ..AdminState::new(store.clone(), state)
    };
    Harness {
        app: router(st.clone()),
        st,
        provider,
        store,
    }
}

async fn harness() -> Harness {
    harness_with(false).await
}

static CODES: AtomicUsize = AtomicUsize::new(0);

/// A started login: the IdP URL and the `mb_login` cookie it set.
struct Started {
    url: String,
    state_cookie: String,
}

impl Harness {
    async fn start_login(&self, return_to: &str) -> Started {
        let r = get_with(
            &self.app,
            &format!("/auth/login?return_to={return_to}"),
            &[],
        )
        .await;
        let url = r.location();
        let state_cookie = r.set_cookie(LOGIN_STATE_COOKIE).expect("mb_login cookie");
        Started { url, state_cookie }
    }

    /// Registers a code for `sub` with `groups` (using `nonce`) and calls the
    /// callback with it.
    async fn callback(
        &self,
        started: &Started,
        sub: &str,
        groups: &[&str],
        nonce: &str,
        cookies: &str,
    ) -> Resp {
        let code = format!("code-{}", CODES.fetch_add(1, Ordering::Relaxed));
        self.provider
            .issue_code(&code, self.provider.claims(sub, groups, Some(nonce)))
            .await;
        let state = MockOidcProvider::query_param(&started.url, "state").unwrap();
        get_with(
            &self.app,
            &format!("/auth/callback?code={code}&state={state}"),
            &[("cookie", cookies)],
        )
        .await
    }

    /// Logs `sub` in with `groups`, sending `extra_cookie` along with the
    /// callback; returns the new session cookie value.
    async fn login(&self, sub: &str, groups: &[&str], extra_cookie: Option<&str>) -> String {
        let started = self.start_login("/mocks").await;
        let nonce = MockOidcProvider::query_param(&started.url, "nonce").unwrap();
        let mut cookies = format!(
            "{LOGIN_STATE_COOKIE}={}",
            cookie_value(&started.state_cookie)
        );
        if let Some(c) = extra_cookie {
            cookies.push_str("; ");
            cookies.push_str(&session_header(c));
        }
        let r = self.callback(&started, sub, groups, &nonce, &cookies).await;
        assert_eq!(r.location(), "/mocks");
        let cleared = r.set_cookie(LOGIN_STATE_COOKIE).expect("mb_login cleared");
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
        assert!(cleared.contains("Path=/auth"), "{cleared}");
        cookie_value(&r.set_cookie(SESSION_COOKIE).expect("session cookie"))
    }

    async fn me(&self, session: &str) -> Resp {
        get_with(
            &self.app,
            "/api/v1/me",
            &[("cookie", &session_header(session))],
        )
        .await
    }

    async fn audit(&self, action: &str) -> Vec<mb_store::AuditEvent> {
        self.store
            .list_audit(&AuditFilter {
                workspace: None,
                action: Some(action.to_string()),
                actor: None,
                limit: AuditFilter::DEFAULT_LIMIT,
                before_id: None,
            })
            .await
            .unwrap()
    }

    async fn logout(&self, cookie: Option<&str>, json: bool) -> Resp {
        let mut req = Request::post("/auth/logout");
        if json {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, session_header(c));
        }
        send(&self.app, req.body(Body::from("{}")).unwrap()).await
    }
}

#[tokio::test]
async fn login_redirects_to_idp_with_pkce_and_sets_state_cookie() {
    let h = harness().await;
    let started = h.start_login("/w/default/mocks").await;
    assert!(
        started
            .url
            .starts_with(&format!("{}/authorize?", h.provider.issuer())),
        "{}",
        started.url
    );
    let param = |n| MockOidcProvider::query_param(&started.url, n);
    assert_eq!(param("response_type").as_deref(), Some("code"));
    assert_eq!(param("code_challenge_method").as_deref(), Some("S256"));
    assert!(param("code_challenge").is_some_and(|c| c.len() >= 43));
    assert_eq!(
        param("client_id").as_deref(),
        Some(MockOidcProvider::CLIENT_ID)
    );
    assert!(param("nonce").is_some());
    let state = param("state").unwrap();
    assert_eq!(
        started.state_cookie,
        format!("mb_login={state}; HttpOnly; SameSite=Lax; Path=/auth; Max-Age=600")
    );

    // The default return_to is `/`.
    let r = get_with(&h.app, "/auth/login", &[]).await;
    assert!(r.location().starts_with(&h.provider.issuer()));
}

#[tokio::test]
async fn full_login_sets_session_and_me_reflects_roles() {
    let h = harness().await;
    h.store.create_workspace("other", "Other").await.unwrap();
    h.store.create_workspace("team-b", "Team B").await.unwrap();

    let started = h.start_login("/mocks").await;
    let nonce = MockOidcProvider::query_param(&started.url, "nonce").unwrap();
    let cookies = format!("mb_login={}", cookie_value(&started.state_cookie));
    let r = h
        .callback(
            &started,
            "alice",
            &["mb:default:editor", "mb:team-b:viewer", "mb:gone:owner"],
            &nonce,
            &cookies,
        )
        .await;
    assert_eq!(r.location(), "/mocks");
    let set = r.set_cookie(SESSION_COOKIE).unwrap();
    let session = cookie_value(&set);
    assert_eq!(
        set,
        format!("mb_session={session}; HttpOnly; SameSite=Lax; Path=/; Max-Age=3600")
    );

    let me = h.me(&session).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.text);
    assert_eq!(
        me.json(),
        json!({
            "subject": "alice",
            "display_name": "alice",
            "kind": "user",
            "sysadmin": false,
            "workspaces": [
                { "name": "default", "role": "editor" },
                { "name": "team-b", "role": "viewer" },
            ],
        })
    );

    // The session works for a mutation an editor may make...
    let cookie = session_header(&session);
    let create = |ws: &str| {
        Request::post(format!("/api/v1/workspaces/{ws}/mocks"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, &cookie)
            .body(Body::from(json!({ "name": "orders" }).to_string()))
            .unwrap()
    };
    let r = send(&h.app, create("default")).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    // ...and not for one a viewer may not.
    send(&h.app, create("team-b"))
        .await
        .problem(StatusCode::FORBIDDEN);

    let events = h.audit("auth.login").await;
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.workspace, None);
    assert_eq!(e.actor_subject, "alice");
    assert_eq!(e.actor_kind, "user");
    assert_eq!(e.target, "user:alice");
    assert_eq!(
        e.after,
        Some(json!({
            "display_name": "alice",
            "sysadmin": false,
            "roles": { "default": "editor", "gone": "owner", "team-b": "viewer" },
        }))
    );
    assert!(!serde_json::to_string(e).unwrap().contains(&session));
}

#[tokio::test]
async fn return_to_open_redirect_rejected() {
    let h = harness().await;
    for bad in [
        "%2F%2Fevil.example",
        "%2F%5Cevil.example",
        "https%3A%2F%2Fevil.example",
        "javascript%3Aalert(1)",
    ] {
        let r = get_with(&h.app, &format!("/auth/login?return_to={bad}"), &[]).await;
        r.problem(StatusCode::BAD_REQUEST);
        assert!(r.set_cookie(LOGIN_STATE_COOKIE).is_none(), "{bad}");
    }
}

#[tokio::test]
async fn callback_state_mismatch_rejected_not_audited() {
    let h = harness().await;
    let started = h.start_login("/").await;
    let nonce = MockOidcProvider::query_param(&started.url, "nonce").unwrap();
    let r = h
        .callback(&started, "alice", &[], &nonce, "mb_login=not-the-state")
        .await;
    let p = r.problem(StatusCode::BAD_REQUEST);
    assert_eq!(p["title"], "Login failed");
    assert!(r.set_cookie(SESSION_COOKIE).is_none());
    // Not attributable to this browser's login: no audit event.
    assert!(h.audit("auth.login_failed").await.is_empty());
    assert!(h.audit("auth.login").await.is_empty());

    // An error from the provider without a login in progress fails the
    // same way, unaudited.
    let r = get_with(&h.app, "/auth/callback?error=access_denied", &[]).await;
    assert_eq!(r.problem(StatusCode::BAD_REQUEST)["title"], "Login failed");
    assert!(h.audit("auth.login_failed").await.is_empty());
}

#[tokio::test]
async fn provider_error_for_pending_login_audited_once() {
    let h = harness().await;
    let started = h.start_login("/").await;
    let state = MockOidcProvider::query_param(&started.url, "state").unwrap();
    let cookies = format!("mb_login={}", cookie_value(&started.state_cookie));
    let uri = format!("/auth/callback?error=access_denied&state={state}");
    for _ in 0..3 {
        let r = get_with(&h.app, &uri, &[("cookie", &cookies)]).await;
        assert_eq!(r.problem(StatusCode::BAD_REQUEST)["title"], "Login failed");
    }
    let events = h.audit("auth.login_failed").await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].actor_subject, "anonymous");
    assert_eq!(events[0].target, "user:unknown");
    assert_eq!(events[0].workspace, None);
    assert_eq!(events[0].after, Some(json!({ "reason": "provider_error" })));
}

#[tokio::test]
async fn anonymous_callback_flood_not_audited() {
    let h = harness().await;
    for uri in [
        "/auth/callback?error=x",
        "/auth/callback",
        "/auth/callback?code=a&state=b",
        "/auth/callback?error=x&state=b",
    ] {
        for cookie in ["", "mb_login=b"] {
            let r = get_with(&h.app, uri, &[("cookie", cookie)]).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}");
        }
    }
    assert!(h.audit("auth.login_failed").await.is_empty());
}

#[tokio::test]
async fn overlong_return_to_rejected() {
    let h = harness().await;
    let long = format!("%2F{}", "a".repeat(4096));
    let r = get_with(&h.app, &format!("/auth/login?return_to={long}"), &[]).await;
    r.problem(StatusCode::BAD_REQUEST);
    assert!(r.set_cookie(LOGIN_STATE_COOKIE).is_none());
}

#[tokio::test]
async fn callback_without_state_cookie_rejected() {
    let h = harness().await;
    let started = h.start_login("/").await;
    let nonce = MockOidcProvider::query_param(&started.url, "nonce").unwrap();
    let r = h.callback(&started, "alice", &[], &nonce, "other=1").await;
    assert_eq!(r.problem(StatusCode::BAD_REQUEST)["title"], "Login failed");
    assert!(r.set_cookie(SESSION_COOKIE).is_none());
    assert!(h.audit("auth.login_failed").await.is_empty());
}

#[tokio::test]
async fn nonce_mismatch_rejected() {
    let h = harness().await;
    let started = h.start_login("/").await;
    let cookies = format!("mb_login={}", cookie_value(&started.state_cookie));
    let r = h
        .callback(&started, "alice", &[], "not-the-nonce", &cookies)
        .await;
    assert_eq!(r.problem(StatusCode::BAD_REQUEST)["title"], "Login failed");
    assert!(r.set_cookie(SESSION_COOKIE).is_none());
    assert_eq!(h.audit("auth.login_failed").await.len(), 1);
}

#[tokio::test]
async fn session_fixation_new_id_per_login() {
    let h = harness().await;
    let first = h.login("alice", &["mb:default:viewer"], None).await;
    assert_eq!(h.me(&first).await.status, StatusCode::OK);
    let second = h.login("alice", &["mb:default:viewer"], Some(&first)).await;
    assert_ne!(first, second);
    h.me(&first).await.problem(StatusCode::UNAUTHORIZED);
    assert_eq!(h.me(&second).await.status, StatusCode::OK);
}

#[tokio::test]
async fn logout_invalidates_session() {
    let h = harness().await;
    let session = h.login("alice", &["mb:default:viewer"], None).await;
    let r = h.logout(Some(&session), true).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    assert_eq!(
        r.set_cookie(SESSION_COOKIE).unwrap(),
        "mb_session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"
    );
    h.me(&session).await.problem(StatusCode::UNAUTHORIZED);

    let events = h.audit("auth.logout").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].actor_subject, "alice");
    assert_eq!(events[0].target, "user:alice");
    assert_eq!(events[0].workspace, None);

    // Without a (valid) session: 204 and no event.
    assert_eq!(h.logout(None, true).await.status, StatusCode::NO_CONTENT);
    assert_eq!(
        h.logout(Some(&session), true).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.audit("auth.logout").await.len(), 1);
}

#[tokio::test]
async fn logout_requires_json_content_type() {
    let h = harness().await;
    let session = h.login("alice", &[], None).await;
    h.logout(Some(&session), false)
        .await
        .problem(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(h.me(&session).await.status, StatusCode::OK);
}

#[tokio::test]
async fn missing_invalid_expired_token_401() {
    let h = harness().await;
    let check = |r: Resp| {
        r.problem(StatusCode::UNAUTHORIZED);
        assert_eq!(
            r.headers[header::WWW_AUTHENTICATE],
            "Bearer realm=\"mockingbird\""
        );
    };
    check(get_with(&h.app, "/api/v1/me", &[]).await);
    check(get_with(&h.app, "/api/v1/me", &[("authorization", "Bearer garbage")]).await);
    check(get_with(&h.app, "/api/v1/me", &[("cookie", "mb_session=unknown")]).await);

    let mut claims = h.provider.claims("bob", &["mb:default:admin"], None);
    let past = chrono::Utc::now().timestamp() - 600;
    claims["iat"] = json!(past - 300);
    claims["exp"] = json!(past);
    let expired = h.provider.sign(claims);
    check(
        get_with(
            &h.app,
            "/api/v1/me",
            &[("authorization", &format!("Bearer {expired}"))],
        )
        .await,
    );
}

#[tokio::test]
async fn bearer_jwt_principal_has_mapped_roles() {
    let h = harness().await;
    let token = h
        .provider
        .sign(h.provider.claims("bob", &["mb:default:admin"], None));
    let r = get_with(
        &h.app,
        "/api/v1/me",
        &[("authorization", &format!("Bearer {token}"))],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let me = r.json();
    assert_eq!(me["subject"], "bob");
    assert_eq!(me["kind"], "user");
    assert_eq!(me["sysadmin"], false);
    assert_eq!(
        me["workspaces"],
        json!([{ "name": "default", "role": "admin" }])
    );

    let sys = h
        .provider
        .sign(h.provider.claims("root", &["mb:sysadmin"], None));
    let r = get_with(
        &h.app,
        "/api/v1/me",
        &[("authorization", &format!("Bearer {sys}"))],
    )
    .await;
    let me = r.json();
    assert_eq!(me["sysadmin"], true);
    assert_eq!(
        me["workspaces"],
        json!([{ "name": "default", "role": "owner" }])
    );
}

fn gated(st: &AdminState) -> Router {
    Router::new()
        .route(
            "/{*path}",
            get(|| async { "console" }).post(|| async { "posted" }),
        )
        .layer(axum::middleware::from_fn_with_state(
            st.clone(),
            console_gate,
        ))
}

#[tokio::test]
async fn console_gate_redirects_html_and_401s_api_clients() {
    let h = harness().await;
    let app = gated(&h.st);
    let html = "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8";

    let r = get_with(&app, "/w/default/mocks?q=1", &[("accept", html)]).await;
    assert_eq!(
        r.location(),
        "/auth/login?return_to=%2Fw%2Fdefault%2Fmocks%3Fq%3D1"
    );
    let r = get_with(&app, "/metrics", &[("accept", "text/plain")]).await;
    r.problem(StatusCode::UNAUTHORIZED);
    assert_eq!(
        r.headers[header::WWW_AUTHENTICATE],
        "Bearer realm=\"mockingbird\""
    );
    get_with(&app, "/w/x", &[])
        .await
        .problem(StatusCode::UNAUTHORIZED);
    let post = Request::builder()
        .method(Method::POST)
        .uri("/w/x")
        .header(header::ACCEPT, html)
        .body(Body::empty())
        .unwrap();
    send(&app, post).await.problem(StatusCode::UNAUTHORIZED);
    // An invalid session navigates to the login page too.
    let r = get_with(
        &app,
        "/w/x",
        &[("accept", html), ("cookie", "mb_session=stale")],
    )
    .await;
    assert_eq!(r.location(), "/auth/login?return_to=%2Fw%2Fx");

    // A path too long to be a return_to sends the browser back to `/`.
    let long = format!("/w/{}", "a".repeat(3000));
    let r = get_with(&app, &long, &[("accept", html)]).await;
    assert_eq!(r.location(), "/auth/login?return_to=%2F");

    let session = h.login("alice", &[], None).await;
    let r = get_with(
        &app,
        "/w/default/mocks",
        &[("accept", html), ("cookie", &session_header(&session))],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text, "console");
}

#[tokio::test]
async fn secure_cookie_when_https_external_url() {
    let h = harness_with(true).await;
    let started = h.start_login("/").await;
    assert!(
        started.state_cookie.ends_with("; Secure"),
        "{}",
        started.state_cookie
    );
    let nonce = MockOidcProvider::query_param(&started.url, "nonce").unwrap();
    let cookies = format!("mb_login={}", cookie_value(&started.state_cookie));
    let r = h.callback(&started, "alice", &[], &nonce, &cookies).await;
    assert_eq!(r.location(), "/");
    for name in [SESSION_COOKIE, LOGIN_STATE_COOKIE] {
        let c = r.set_cookie(name).unwrap();
        assert!(c.ends_with("; Secure"), "{c}");
    }
    let session = cookie_value(&r.set_cookie(SESSION_COOKIE).unwrap());
    let r = h.logout(Some(&session), true).await;
    assert!(r.set_cookie(SESSION_COOKIE).unwrap().ends_with("; Secure"));

    // Plain http: no Secure attribute.
    let h = harness().await;
    let started = h.start_login("/").await;
    assert!(!started.state_cookie.contains("Secure"));
}

#[tokio::test]
async fn mode_none_login_redirects_back() {
    let (store, state) = stores().await;
    let st = AdminState::new(store, state);
    assert!(st.web_auth.is_none());
    let app = router(st.clone());

    let r = get_with(&app, "/auth/login?return_to=%2Fw%2Fdefault%3Fx%3D1", &[]).await;
    assert_eq!(r.location(), "/w/default?x=1");
    assert!(r.set_cookies().is_empty());
    assert_eq!(get_with(&app, "/auth/login", &[]).await.location(), "/");
    get_with(&app, "/auth/login?return_to=%2F%2Fevil.example", &[])
        .await
        .problem(StatusCode::BAD_REQUEST);

    let me = get_with(&app, "/api/v1/me", &[]).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.text);
    let me = me.json();
    assert_eq!(me["subject"], "local");
    assert_eq!(me["kind"], "local");
    assert_eq!(me["sysadmin"], true);
    assert_eq!(
        me["workspaces"],
        json!([{ "name": "default", "role": "owner" }])
    );

    // Logout is a no-op, and the console gate lets the Local principal in.
    let logout = Request::post("/auth/logout")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(send(&app, logout).await.status, StatusCode::NO_CONTENT);
    let r = get_with(&gated(&st), "/w/default", &[("accept", "text/html")]).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn openapi_documents_me_but_not_auth_routes() {
    let (store, state) = stores().await;
    let app = router(AdminState::new(store, state));
    let doc = get_with(&app, "/api/openapi.json", &[]).await.json();
    assert!(doc["paths"]["/api/v1/me"]["get"].is_object(), "{doc}");
    let schemas = doc["components"]["schemas"].as_object().unwrap();
    assert!(schemas.contains_key("MeView"));
    assert!(schemas.contains_key("MeWorkspace"));
    assert!(
        doc["paths"]
            .as_object()
            .unwrap()
            .keys()
            .all(|p| !p.starts_with("/auth"))
    );
}

/// Answers `path` on the provider with HTTP 500 from now on.
async fn break_endpoint(h: &Harness, endpoint: &str) {
    wiremock::Mock::given(wiremock::matchers::path(endpoint))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&h.provider.server)
        .await;
}

#[tokio::test]
async fn upstream_failure_is_a_generic_502() {
    let h = harness().await;
    break_endpoint(&h, "/jwks").await;
    // The bearer verifier has not fetched the JWKS yet: it must now.
    let token = h.provider.sign(h.provider.claims("alice", &[], None));
    let r = get_with(
        &h.app,
        "/api/v1/me",
        &[("authorization", &format!("Bearer {token}"))],
    )
    .await;
    let p = r.problem(StatusCode::BAD_GATEWAY);
    assert_eq!(p["detail"], "identity provider unavailable");
    assert!(!r.text.contains(&h.provider.issuer()), "{}", r.text);
    assert!(!r.text.contains("http"), "{}", r.text);
}

#[tokio::test]
async fn login_failure_detail_is_generic() {
    let h = harness().await;
    // The token exchange fails: the caller learns nothing about the IdP.
    break_endpoint(&h, "/token").await;
    let started = h.start_login("/").await;
    let nonce = MockOidcProvider::query_param(&started.url, "nonce").unwrap();
    let cookies = format!("mb_login={}", cookie_value(&started.state_cookie));
    let r = h.callback(&started, "alice", &[], &nonce, &cookies).await;
    let p = r.problem(StatusCode::BAD_REQUEST);
    assert_eq!(p["title"], "Login failed");
    assert_eq!(p["detail"], "the login could not be completed");
    assert!(!r.text.contains(&h.provider.issuer()), "{}", r.text);
    assert!(!r.text.contains("http"), "{}", r.text);

    // The provider's error text is not echoed back either.
    let started = h.start_login("/").await;
    let state = MockOidcProvider::query_param(&started.url, "state").unwrap();
    let cookies = format!("mb_login={}", cookie_value(&started.state_cookie));
    let r = get_with(
        &h.app,
        &format!("/auth/callback?error=see_https%3A%2F%2Fevil.example&state={state}"),
        &[("cookie", &cookies)],
    )
    .await;
    let p = r.problem(StatusCode::BAD_REQUEST);
    assert_eq!(p["detail"], "the identity provider reported an error");
    assert!(!r.text.contains("evil"), "{}", r.text);
}

#[tokio::test]
async fn login_failed_audit_is_rate_limited_per_source() {
    use axum::extract::ConnectInfo;
    let h = harness().await;
    // A failure of a login this browser started, from `peer`.
    let fail = |peer: std::net::SocketAddr| {
        let h = &h;
        async move {
            let started = h.start_login("/").await;
            let state = MockOidcProvider::query_param(&started.url, "state").unwrap();
            let mut req = Request::get(format!("/auth/callback?error=access_denied&state={state}"))
                .header(
                    "cookie",
                    format!("mb_login={}", cookie_value(&started.state_cookie)),
                )
                .body(Body::empty())
                .unwrap();
            req.extensions_mut().insert(ConnectInfo(peer));
            let r = send(&h.app, req).await;
            assert_eq!(r.problem(StatusCode::BAD_REQUEST)["title"], "Login failed");
        }
    };
    let a: std::net::SocketAddr = "192.0.2.1:5000".parse().unwrap();
    let b: std::net::SocketAddr = "192.0.2.2:5000".parse().unwrap();
    fail(a).await;
    // Same address, another port: still the same source.
    fail("192.0.2.1:6000".parse().unwrap()).await;
    fail(a).await;
    assert_eq!(h.audit("auth.login_failed").await.len(), 1);
    fail(b).await;
    assert_eq!(h.audit("auth.login_failed").await.len(), 2);
}
