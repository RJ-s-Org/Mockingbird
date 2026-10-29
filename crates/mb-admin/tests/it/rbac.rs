//! Role-based access control on the admin API, with a test authenticator
//! that maps `Authorization: Bearer <name>` to fixed principals.
#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::{Extension, Router};
use mb_admin::channels::{
    BrokerView, ChannelActivityView, ChannelControl, ChannelControlError, PublishedMessage,
    SharedChannelControl,
};
use mb_admin::{AdminState, router};
use mb_auth::{AuthError, Authenticate, Principal, PrincipalKind, Role};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

/// `Bearer viewer|editor|admin|owner` → that role in `default`;
/// `Bearer team-editor` → editor of `team-a`; `Bearer sysadmin` → sysadmin;
/// no header → no principal; anything else → an invalid credential.
struct TestAuth;

fn principal(subject: &str, sysadmin: bool, roles: &[(&str, Role)]) -> Principal {
    Principal {
        subject: subject.into(),
        display_name: subject.into(),
        kind: PrincipalKind::User,
        sysadmin,
        roles: roles
            .iter()
            .map(|(ws, r)| (ws.to_string(), *r))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[async_trait::async_trait]
impl Authenticate for TestAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
        let Some(value) = headers.get(header::AUTHORIZATION) else {
            return Ok(None);
        };
        let name = value
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap_or_default();
        let p = match name {
            "viewer" => principal(name, false, &[("default", Role::Viewer)]),
            "editor" => principal(name, false, &[("default", Role::Editor)]),
            "admin" => principal(name, false, &[("default", Role::Admin)]),
            "owner" => principal(name, false, &[("default", Role::Owner)]),
            "team-editor" => principal(name, false, &[("team-a", Role::Editor)]),
            "sysadmin" => principal(name, true, &[]),
            _ => return Err(AuthError::Unauthenticated("unknown token".into())),
        };
        Ok(Some(p))
    }
}

/// A channel runtime that publishes one message per requested one.
struct FakeControl;

#[async_trait::async_trait]
impl ChannelControl for FakeControl {
    async fn publish(
        &self,
        _ws: &str,
        _mock: &str,
        _channel: &str,
        count: usize,
        _context: Option<Value>,
    ) -> Result<Vec<PublishedMessage>, ChannelControlError> {
        Ok((0..count)
            .map(|_| PublishedMessage {
                address: "orders.created".into(),
                key: None,
                headers: BTreeMap::new(),
                payload: json!({}),
                source: "fallback".into(),
            })
            .collect())
    }

    fn activity(&self, _ws: &str, _mock: &str) -> BTreeMap<String, ChannelActivityView> {
        BTreeMap::new()
    }

    fn brokers(&self) -> Vec<BrokerView> {
        Vec::new()
    }
}

const EVENTS: &str = include_str!("../../../../fixtures/asyncapi/orders-amqp-2.6.yaml");

struct Harness {
    app: Router,
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

    fn expect(&self, status: StatusCode, what: &str) {
        assert_eq!(self.status, status, "{what}: {}", self.text);
        if !status.is_success() {
            assert_eq!(
                self.headers[header::CONTENT_TYPE],
                "application/problem+json",
                "{what}"
            );
        }
    }
}

/// An OpenAPI 3.0 contract with one operation.
fn contract() -> String {
    json!({
        "openapi": "3.0.3",
        "info": { "title": "Orders", "version": "1.0" },
        "paths": { "/orders": { "get": {
            "operationId": "listOrders",
            "responses": { "200": { "description": "ok", "content": { "application/json": {
                "schema": { "type": "array", "items": { "type": "string" } }
            } } } }
        } } }
    })
    .to_string()
}

fn profile() -> Value {
    json!({ "base_url": "https://llm", "model": "m" })
}

/// A store holding workspace `team-a`, and in `default` a mock `orders` with
/// one contract version, a scenario `s` and a profile `p`, and an event
/// mock `stream` (AsyncAPI, channel `orderCreated`).
async fn harness() -> Harness {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let control: SharedChannelControl = Arc::new(FakeControl);
    let app = router(AdminState {
        auth: Arc::new(TestAuth),
        ..AdminState::new(store, state)
    })
    .layer(Extension(control));
    let h = Harness { app };
    let sys = Some("sysadmin");
    h.call(
        sys,
        Method::POST,
        "/api/v1/workspaces",
        Some(json!({ "name": "team-a" })),
    )
    .await
    .expect(StatusCode::CREATED, "create team-a");
    h.call(
        sys,
        Method::POST,
        "/api/v1/mocks",
        Some(json!({ "name": "orders" })),
    )
    .await
    .expect(StatusCode::CREATED, "create mock");
    h.upload(sys, "/api/v1/mocks/orders/contracts")
        .await
        .expect(StatusCode::CREATED, "upload");
    h.call(
        sys,
        Method::PUT,
        "/api/v1/mocks/orders/scenarios/s",
        Some(json!({})),
    )
    .await
    .expect(StatusCode::OK, "scenario");
    h.call(sys, Method::PUT, "/api/v1/llm-profiles/p", Some(profile()))
        .await
        .expect(StatusCode::OK, "profile");
    h.call(
        sys,
        Method::POST,
        "/api/v1/mocks",
        Some(json!({ "name": "stream" })),
    )
    .await
    .expect(StatusCode::CREATED, "create event mock");
    let req = Request::post("/api/v1/mocks/stream/contracts")
        .header(header::AUTHORIZATION, "Bearer sysadmin")
        .header(header::CONTENT_TYPE, "application/vnd.aai.asyncapi+yaml")
        .body(Body::from(EVENTS))
        .unwrap();
    h.send(req)
        .await
        .expect(StatusCode::CREATED, "upload AsyncAPI");
    h
}

impl Harness {
    async fn send(&self, req: Request<Body>) -> Resp {
        let resp = self.app.clone().oneshot(req).await.unwrap();
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

    async fn call(
        &self,
        who: Option<&str>,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> Resp {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(who) = who {
            req = req.header(header::AUTHORIZATION, format!("Bearer {who}"));
        }
        let body = match body {
            Some(b) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        self.send(req.body(body).unwrap()).await
    }

    async fn upload(&self, who: Option<&str>, uri: &str) -> Resp {
        let mut req = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
        if let Some(who) = who {
            req = req.header(header::AUTHORIZATION, format!("Bearer {who}"));
        }
        self.send(req.body(Body::from(contract())).unwrap()).await
    }

    /// Runs `who` against every Edit-level mutation of `base` (a workspace's
    /// resource root): each must succeed (`allowed`) or be a 403.
    async fn edits(&self, who: &str, base: &str, allowed: bool) {
        let w = Some(who);
        let m = format!("{base}/mocks/orders");
        let e = format!("{base}/mocks/stream");
        let calls: Vec<(Method, String, Option<Value>)> = vec![
            (
                Method::POST,
                format!("{base}/mocks"),
                Some(json!({ "name": "new" })),
            ),
            (Method::PATCH, m.clone(), Some(json!({ "persona": "p" }))),
            (
                Method::POST,
                format!("{m}/contracts/1/activate"),
                Some(json!({})),
            ),
            (Method::PUT, format!("{m}/scenarios/t"), Some(json!({}))),
            (Method::DELETE, format!("{m}/scenarios/s"), None),
            (Method::PUT, format!("{m}/sessions/s1"), Some(json!([]))),
            (Method::DELETE, format!("{m}/sessions/s1"), None),
            (
                Method::PUT,
                format!("{e}/channels/orderCreated"),
                Some(json!({ "enabled": true })),
            ),
            (
                Method::POST,
                format!("{e}/channels/orderCreated/publish"),
                Some(json!({ "count": 1 })),
            ),
            (
                Method::PUT,
                format!("{e}/event-settings"),
                Some(json!({ "broker": null })),
            ),
        ];
        let check = |r: Resp, what: String| {
            if allowed {
                assert!(r.status.is_success(), "{what}: {} {}", r.status, r.text);
            } else {
                r.expect(StatusCode::FORBIDDEN, &what);
            }
        };
        for (method, uri, body) in calls {
            let what = format!("{who}: {method} {uri}");
            check(self.call(w, method, &uri, body).await, what);
        }
        let what = format!("{who}: upload to {m}");
        check(self.upload(w, &format!("{m}/contracts")).await, what);
    }

    async fn reads(&self, who: &str, base: &str, expected: StatusCode) {
        let m = format!("{base}/mocks/orders");
        let e = format!("{base}/mocks/stream");
        for uri in [
            format!("{base}/mocks"),
            m.clone(),
            format!("{m}/contracts"),
            format!("{m}/contracts/1"),
            format!("{m}/contracts/1/source"),
            format!("{m}/scenarios"),
            format!("{m}/sessions"),
            format!("{m}/sessions/s1"),
            format!("{m}/requests"),
            format!("{e}/channels"),
            format!("{e}/channels/orderCreated"),
            format!("{e}/event-settings"),
            format!("{base}/llm-profiles"),
            format!("{base}/llm-profiles/p"),
        ] {
            let what = format!("{who}: GET {uri}");
            self.call(Some(who), Method::GET, &uri, None)
                .await
                .expect(expected, &what);
        }
    }
}

const DEFAULT: &str = "/api/v1/workspaces/default";

#[tokio::test]
async fn missing_credentials_401_with_www_authenticate() {
    let h = harness().await;
    for uri in [
        "/api/v1/mocks",
        "/api/v1/workspaces",
        "/api/v1/workspaces/default/mocks/orders",
        "/api/v1/llm-profiles",
    ] {
        let r = h.call(None, Method::GET, uri, None).await;
        r.expect(StatusCode::UNAUTHORIZED, uri);
        assert_eq!(
            r.headers[header::WWW_AUTHENTICATE],
            "Bearer realm=\"mockingbird\"",
            "{uri}"
        );
        assert_eq!(r.json()["status"], 401);
    }
    // A rejected credential is a 401 too.
    let r = h
        .call(Some("forged"), Method::GET, "/api/v1/mocks", None)
        .await;
    r.expect(StatusCode::UNAUTHORIZED, "forged");
    assert!(r.headers.contains_key(header::WWW_AUTHENTICATE));
    // Mutations without credentials never run.
    h.call(
        None,
        Method::POST,
        "/api/v1/mocks",
        Some(json!({ "name": "x" })),
    )
    .await
    .expect(StatusCode::UNAUTHORIZED, "anonymous create");
    h.call(Some("sysadmin"), Method::GET, "/api/v1/mocks/x", None)
        .await
        .expect(StatusCode::NOT_FOUND, "anonymous create had no effect");
}

#[tokio::test]
async fn viewer_can_read_cannot_mutate() {
    let h = harness().await;
    h.reads("viewer", DEFAULT, StatusCode::OK).await;
    h.reads("viewer", "/api/v1", StatusCode::OK).await;
    h.call(
        Some("viewer"),
        Method::GET,
        "/api/v1/workspaces/default",
        None,
    )
    .await
    .expect(StatusCode::OK, "workspace");

    h.edits("viewer", DEFAULT, false).await;
    h.edits("viewer", "/api/v1", false).await;
    let v = Some("viewer");
    for (method, uri, body) in [
        (Method::DELETE, format!("{DEFAULT}/mocks/orders"), None),
        (
            Method::PUT,
            format!("{DEFAULT}/llm-profiles/q"),
            Some(profile()),
        ),
        (Method::DELETE, format!("{DEFAULT}/llm-profiles/p"), None),
        (
            Method::PATCH,
            DEFAULT.to_string(),
            Some(json!({ "display_name": "x" })),
        ),
        (Method::DELETE, DEFAULT.to_string(), None),
        (
            Method::POST,
            "/api/v1/workspaces".to_string(),
            Some(json!({ "name": "mine" })),
        ),
    ] {
        let what = format!("viewer: {method} {uri}");
        h.call(v, method, &uri, body)
            .await
            .expect(StatusCode::FORBIDDEN, &what);
    }
    // Nothing changed.
    h.call(v, Method::GET, "/api/v1/mocks/new", None)
        .await
        .expect(StatusCode::NOT_FOUND, "no new mock");
    h.call(v, Method::GET, "/api/v1/mocks/orders/scenarios", None)
        .await
        .expect(StatusCode::OK, "scenarios");
}

#[tokio::test]
async fn editor_cannot_touch_profiles() {
    let h = harness().await;
    let e = Some("editor");
    h.edits("editor", DEFAULT, true).await;
    for base in [DEFAULT, "/api/v1"] {
        h.call(e, Method::GET, &format!("{base}/llm-profiles/p"), None)
            .await
            .expect(StatusCode::OK, "get profile");
        h.call(
            e,
            Method::PUT,
            &format!("{base}/llm-profiles/p"),
            Some(profile()),
        )
        .await
        .expect(StatusCode::FORBIDDEN, "put profile");
        h.call(e, Method::DELETE, &format!("{base}/llm-profiles/p"), None)
            .await
            .expect(StatusCode::FORBIDDEN, "delete profile");
    }
    h.call(e, Method::DELETE, &format!("{DEFAULT}/mocks/orders"), None)
        .await
        .expect(StatusCode::NO_CONTENT, "editor deletes a mock");
}

#[tokio::test]
async fn admin_can_manage_profiles() {
    let h = harness().await;
    let a = Some("admin");
    h.call(
        a,
        Method::PUT,
        &format!("{DEFAULT}/llm-profiles/q"),
        Some(profile()),
    )
    .await
    .expect(StatusCode::OK, "put");
    h.call(a, Method::PUT, "/api/v1/llm-profiles/r", Some(profile()))
        .await
        .expect(StatusCode::OK, "put alias");
    h.call(
        a,
        Method::DELETE,
        &format!("{DEFAULT}/llm-profiles/q"),
        None,
    )
    .await
    .expect(StatusCode::NO_CONTENT, "delete");
    h.call(a, Method::DELETE, "/api/v1/llm-profiles/r", None)
        .await
        .expect(StatusCode::NO_CONTENT, "delete alias");
    h.call(
        a,
        Method::PATCH,
        DEFAULT,
        Some(json!({ "display_name": "x" })),
    )
    .await
    .expect(StatusCode::FORBIDDEN, "admin cannot patch the workspace");
}

#[tokio::test]
async fn only_sysadmin_binds_a_secret_to_a_base_url() {
    let h = harness().await;
    let uri = |n: &str| format!("{DEFAULT}/llm-profiles/{n}");
    let with = |base: &str, secret: Option<&str>, model: &str| json!({ "base_url": base, "model": model, "secret_ref": secret });
    let key = Some("env:MB_LLM_API_KEY");
    for who in ["admin", "owner"] {
        let w = Some(who);
        // A new profile that sends the server's key to an attacker.
        h.call(
            w,
            Method::PUT,
            &uri("evil"),
            Some(with("https://attacker.example", key, "m")),
        )
        .await
        .expect(
            StatusCode::FORBIDDEN,
            &format!("{who}: new profile with a secret"),
        );
    }
    h.call(
        Some("sysadmin"),
        Method::PUT,
        &uri("s"),
        Some(with("https://llm", key, "m")),
    )
    .await
    .expect(StatusCode::OK, "sysadmin sets the secret");
    let a = Some("admin");
    for (body, what) in [
        (with("https://attacker.example", key, "m"), "new base_url"),
        (
            with("https://llm", Some("env:OTHER_API_KEY"), "m"),
            "new secret_ref",
        ),
    ] {
        h.call(a, Method::PUT, &uri("s"), Some(body))
            .await
            .expect(StatusCode::FORBIDDEN, what);
    }
    let r = h.call(Some("sysadmin"), Method::GET, &uri("s"), None).await;
    r.expect(StatusCode::OK, "unchanged");
    assert_eq!(r.json()["base_url"], "https://llm");
    assert_eq!(r.json()["secret_ref"], "env:MB_LLM_API_KEY");
    // Other fields stay the admin's to change, and so does a profile
    // without a secret.
    h.call(
        a,
        Method::PUT,
        &uri("s"),
        Some(with("https://llm", key, "m2")),
    )
    .await
    .expect(StatusCode::OK, "admin changes the model");
    h.call(
        a,
        Method::PUT,
        &uri("s"),
        Some(with("https://other", None, "m2")),
    )
    .await
    .expect(StatusCode::OK, "admin drops the secret");
    h.call(
        a,
        Method::PUT,
        &uri("plain"),
        Some(with("https://local", None, "m")),
    )
    .await
    .expect(StatusCode::OK, "admin creates a profile without a secret");
}

#[tokio::test]
async fn only_sysadmin_binds_a_header_secret_to_a_base_url() {
    let h = harness().await;
    let uri = format!("{DEFAULT}/llm-profiles/gw");
    let with = |base: &str, header: &str| {
        json!({
            "base_url": base,
            "model": "m",
            "headers": { "cf-aig-authorization": header, "x-title": "Mockingbird" }
        })
    };
    let secret = "Bearer ${env:CF_AIG_API_KEY}";
    let a = Some("admin");
    h.call(
        a,
        Method::PUT,
        &uri,
        Some(with("https://attacker.example", secret)),
    )
    .await
    .expect(StatusCode::FORBIDDEN, "admin binds a header secret");
    h.call(
        Some("sysadmin"),
        Method::PUT,
        &uri,
        Some(with("https://gw", secret)),
    )
    .await
    .expect(StatusCode::OK, "sysadmin binds a header secret");
    let r = h.call(a, Method::GET, &uri, None).await;
    r.expect(StatusCode::OK, "read back");
    assert_eq!(r.json()["headers"]["cf-aig-authorization"], secret);
    h.call(
        a,
        Method::PUT,
        &uri,
        Some(with("https://attacker.example", secret)),
    )
    .await
    .expect(StatusCode::FORBIDDEN, "admin moves the header secret");
    h.call(
        Some("sysadmin"),
        Method::PUT,
        &uri,
        Some(with("https://gw", "Bearer ${env:NOT_ALLOWED}")),
    )
    .await
    .expect(
        StatusCode::UNPROCESSABLE_ENTITY,
        "reference outside api_secret_refs",
    );
    h.call(
        Some("sysadmin"),
        Method::PUT,
        &uri,
        Some(with("https://gw", "Bearer raw-token")),
    )
    .await
    .expect(StatusCode::UNPROCESSABLE_ENTITY, "raw credential header");
}

#[tokio::test]
async fn owner_can_patch_and_delete_workspace_admin_cannot() {
    let h = harness().await;
    h.call(Some("admin"), Method::DELETE, DEFAULT, None)
        .await
        .expect(StatusCode::FORBIDDEN, "admin delete");
    let r = h
        .call(
            Some("owner"),
            Method::PATCH,
            DEFAULT,
            Some(json!({ "display_name": "Main" })),
        )
        .await;
    r.expect(StatusCode::OK, "owner patch");
    assert_eq!(r.json()["display_name"], "Main");
    assert_eq!(r.json()["role"], "owner");
    // The owner passes the permission check; `default` itself is undeletable.
    h.call(Some("owner"), Method::DELETE, DEFAULT, None)
        .await
        .expect(StatusCode::CONFLICT, "owner delete default");

    // A team-a editor cannot delete team-a; a sysadmin can.
    h.call(
        Some("team-editor"),
        Method::DELETE,
        "/api/v1/workspaces/team-a",
        None,
    )
    .await
    .expect(StatusCode::FORBIDDEN, "team editor delete");
    h.call(
        Some("sysadmin"),
        Method::DELETE,
        "/api/v1/workspaces/team-a",
        None,
    )
    .await
    .expect(StatusCode::NO_CONTENT, "sysadmin delete");
}

#[tokio::test]
async fn cross_workspace_access_403() {
    let h = harness().await;
    let t = Some("team-editor");
    // Its own workspace works.
    h.call(
        t,
        Method::POST,
        "/api/v1/workspaces/team-a/mocks",
        Some(json!({ "name": "inv" })),
    )
    .await
    .expect(StatusCode::CREATED, "own workspace");
    h.call(t, Method::GET, "/api/v1/workspaces/team-a", None)
        .await
        .expect(StatusCode::OK, "own workspace");

    h.reads("team-editor", DEFAULT, StatusCode::FORBIDDEN).await;
    // The aliases act on `default`, where it has no role.
    h.reads("team-editor", "/api/v1", StatusCode::FORBIDDEN)
        .await;
    h.edits("team-editor", DEFAULT, false).await;
    h.edits("team-editor", "/api/v1", false).await;
    h.call(t, Method::GET, DEFAULT, None)
        .await
        .expect(StatusCode::FORBIDDEN, "default workspace");

    // A workspace that does not exist looks the same as one it cannot see.
    for uri in [
        "/api/v1/workspaces/nope",
        "/api/v1/workspaces/nope/mocks",
        "/api/v1/workspaces/nope/mocks/orders",
    ] {
        h.call(t, Method::GET, uri, None)
            .await
            .expect(StatusCode::FORBIDDEN, uri);
    }

    let list = h.call(t, Method::GET, "/api/v1/workspaces", None).await;
    list.expect(StatusCode::OK, "list");
    let names: Vec<Value> = list
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["name"].clone())
        .collect();
    assert_eq!(names, [json!("team-a")]);
}

#[tokio::test]
async fn sysadmin_missing_workspace_404() {
    let h = harness().await;
    let s = Some("sysadmin");
    for uri in [
        "/api/v1/workspaces/nope",
        "/api/v1/workspaces/nope/mocks",
        "/api/v1/workspaces/nope/llm-profiles/p",
    ] {
        h.call(s, Method::GET, uri, None)
            .await
            .expect(StatusCode::NOT_FOUND, uri);
    }
    h.call(s, Method::DELETE, "/api/v1/workspaces/nope", None)
        .await
        .expect(StatusCode::NOT_FOUND, "delete");
    h.call(s, Method::GET, "/api/v1/workspaces/team-a/mocks", None)
        .await
        .expect(StatusCode::OK, "sysadmin reads any workspace");
}

#[tokio::test]
async fn only_sysadmin_creates_workspaces() {
    let h = harness().await;
    for who in ["viewer", "editor", "admin", "owner", "team-editor"] {
        h.call(
            Some(who),
            Method::POST,
            "/api/v1/workspaces",
            Some(json!({ "name": format!("by-{who}") })),
        )
        .await
        .expect(StatusCode::FORBIDDEN, who);
    }
    let r = h
        .call(
            Some("sysadmin"),
            Method::POST,
            "/api/v1/workspaces",
            Some(json!({ "name": "by-sysadmin" })),
        )
        .await;
    r.expect(StatusCode::CREATED, "sysadmin");
    assert_eq!(r.json()["role"], "owner");
}

#[tokio::test]
async fn openapi_json_is_public() {
    let h = harness().await;
    let r = h.call(None, Method::GET, "/api/openapi.json", None).await;
    r.expect(StatusCode::OK, "openapi");
    assert!(r.json()["paths"].is_object());
    // Unknown API paths are a 404 problem, also without credentials.
    let r = h.call(None, Method::GET, "/api/v2/anything", None).await;
    r.expect(StatusCode::NOT_FOUND, "unknown path");
}
