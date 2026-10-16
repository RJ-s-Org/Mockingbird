//! The audit log: one event per successful mutation, filtering, JSONL export,
//! the signed webhook and retention.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use chrono::Utc;
use mb_admin::{AdminState, Auditor, WebhookSink, router, signature, spawn_retention};
use mb_auth::{AuthError, Authenticate, Principal, PrincipalKind, Role};
use mb_state::{MemoryStore, StateStore};
use mb_store::{AuditEvent, AuditFilter, ControlStore, NewAuditEvent};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// No header → the Local principal (sysadmin, subject `local`);
/// `Bearer viewer|editor` → that role in `default`.
struct TestAuth;

fn user(subject: &str, roles: &[(&str, Role)]) -> Principal {
    Principal {
        subject: subject.into(),
        display_name: subject.into(),
        kind: PrincipalKind::User,
        sysadmin: false,
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
            return Ok(Some(Principal::local()));
        };
        let name = value.to_str().unwrap().strip_prefix("Bearer ").unwrap();
        Ok(Some(match name {
            "viewer" => user(name, &[("default", Role::Viewer)]),
            "editor" => user(name, &[("default", Role::Editor)]),
            _ => return Err(AuthError::Unauthenticated("unknown token".into())),
        }))
    }
}

struct Harness {
    app: Router,
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

    fn ok(&self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "body: {}", self.text);
        if self.text.is_empty() {
            Value::Null
        } else {
            self.json()
        }
    }
}

async fn harness_with(webhook: Option<WebhookSink>) -> Harness {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let app = router(AdminState {
        auth: Arc::new(TestAuth),
        audit: Auditor::new(store.clone(), webhook),
        ..AdminState::new(store.clone(), state)
    });
    Harness { app, store }
}

async fn harness() -> Harness {
    harness_with(None).await
}

fn contract(title: &str) -> String {
    json!({
        "openapi": "3.0.3",
        "info": { "title": title, "version": "1.0" },
        "paths": { "/orders": { "get": {
            "operationId": "listOrders",
            "responses": { "200": { "description": "ok", "content": { "application/json": {
                "schema": { "type": "array", "items": { "type": "string" } }
            } } } }
        } } }
    })
    .to_string()
}

impl Harness {
    async fn raw(
        &self,
        token: Option<&str>,
        method: Method,
        uri: &str,
        content_type: Option<&str>,
        body: String,
    ) -> Resp {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if let Some(ct) = content_type {
            req = req.header(header::CONTENT_TYPE, ct);
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
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
        token: Option<&str>,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> Resp {
        match body {
            Some(b) => {
                self.raw(token, method, uri, Some("application/json"), b.to_string())
                    .await
            }
            None => self.raw(token, method, uri, None, String::new()).await,
        }
    }

    async fn events(&self) -> Vec<AuditEvent> {
        self.store
            .list_audit(&AuditFilter {
                limit: AuditFilter::MAX_LIMIT,
                ..AuditFilter::default()
            })
            .await
            .unwrap()
    }
}

/// One table row: a mutation and the event it must write.
struct Case {
    method: Method,
    uri: &'static str,
    content_type: Option<&'static str>,
    body: String,
    status: StatusCode,
    action: &'static str,
    target: &'static str,
    before: bool,
    after: bool,
}

fn case(
    method: Method,
    uri: &'static str,
    body: Option<Value>,
    status: StatusCode,
    action: &'static str,
    target: &'static str,
    (before, after): (bool, bool),
) -> Case {
    Case {
        method,
        uri,
        content_type: body.as_ref().map(|_| "application/json"),
        body: body.map(|b| b.to_string()).unwrap_or_default(),
        status,
        action,
        target,
        before,
        after,
    }
}

#[tokio::test]
async fn each_mutation_writes_exactly_one_event() {
    let h = harness().await;
    let cases = vec![
        case(
            Method::POST,
            "/api/v1/workspaces",
            Some(json!({"name": "team-a"})),
            StatusCode::CREATED,
            "workspace.create",
            "workspace:team-a",
            (false, true),
        ),
        case(
            Method::PATCH,
            "/api/v1/workspaces/team-a",
            Some(json!({"display_name": "Team A"})),
            StatusCode::OK,
            "workspace.update",
            "workspace:team-a",
            (true, true),
        ),
        case(
            Method::POST,
            "/api/v1/workspaces/default/mocks",
            Some(json!({"name": "orders"})),
            StatusCode::CREATED,
            "mock.create",
            "mock:orders",
            (false, true),
        ),
        case(
            Method::PATCH,
            "/api/v1/workspaces/default/mocks/orders",
            Some(json!({"persona": "terse"})),
            StatusCode::OK,
            "mock.update",
            "mock:orders",
            (true, true),
        ),
        Case {
            method: Method::POST,
            uri: "/api/v1/workspaces/default/mocks/orders/contracts",
            content_type: Some("application/json"),
            body: contract("v1"),
            status: StatusCode::CREATED,
            action: "contract.upload",
            target: "contract:orders@1",
            before: false,
            after: true,
        },
        Case {
            method: Method::POST,
            // The P2 alias path writes the same event.
            uri: "/api/v1/mocks/orders/contracts",
            content_type: Some("application/json"),
            body: contract("v2"),
            status: StatusCode::CREATED,
            action: "contract.upload",
            target: "contract:orders@2",
            before: false,
            after: true,
        },
        case(
            Method::POST,
            "/api/v1/workspaces/default/mocks/orders/contracts/2/activate",
            Some(json!({})),
            StatusCode::OK,
            "contract.activate",
            "contract:orders@2",
            (false, true),
        ),
        case(
            Method::PUT,
            "/api/v1/workspaces/default/mocks/orders/scenarios/slow",
            Some(json!({"latency_ms": 5})),
            StatusCode::OK,
            "scenario.put",
            "scenario:orders/slow",
            (false, true),
        ),
        case(
            Method::PUT,
            "/api/v1/workspaces/default/mocks/orders/scenarios/slow",
            Some(json!({"latency_ms": 9})),
            StatusCode::OK,
            "scenario.put",
            "scenario:orders/slow",
            (true, true),
        ),
        case(
            Method::DELETE,
            "/api/v1/workspaces/default/mocks/orders/scenarios/slow",
            None,
            StatusCode::NO_CONTENT,
            "scenario.delete",
            "scenario:orders/slow",
            (true, false),
        ),
        case(
            Method::PUT,
            "/api/v1/workspaces/default/mocks/orders/sessions/s1",
            Some(json!([{"resource_type": "order", "id": "1", "data": {"total": 3}}])),
            StatusCode::NO_CONTENT,
            "session.seed",
            "session:orders/s1",
            (false, true),
        ),
        case(
            Method::DELETE,
            "/api/v1/workspaces/default/mocks/orders/sessions/s1",
            None,
            StatusCode::NO_CONTENT,
            "session.reset",
            "session:orders/s1",
            (false, false),
        ),
        case(
            Method::PUT,
            "/api/v1/workspaces/default/llm-profiles/p",
            Some(json!({"base_url": "https://llm", "model": "m"})),
            StatusCode::OK,
            "profile.put",
            "profile:p",
            (false, true),
        ),
        case(
            Method::PUT,
            "/api/v1/llm-profiles/p",
            Some(json!({"base_url": "https://llm", "model": "m2"})),
            StatusCode::OK,
            "profile.put",
            "profile:p",
            (true, true),
        ),
        case(
            Method::DELETE,
            "/api/v1/workspaces/default/llm-profiles/p",
            None,
            StatusCode::NO_CONTENT,
            "profile.delete",
            "profile:p",
            (true, false),
        ),
        case(
            Method::DELETE,
            "/api/v1/workspaces/default/mocks/orders",
            None,
            StatusCode::NO_CONTENT,
            "mock.delete",
            "mock:orders",
            (true, false),
        ),
        case(
            Method::DELETE,
            "/api/v1/workspaces/team-a",
            None,
            StatusCode::NO_CONTENT,
            "workspace.delete",
            "workspace:team-a",
            (true, false),
        ),
    ];
    for c in cases {
        let what = format!("{} {}", c.method, c.uri);
        let before = h.events().await.len();
        let r = h
            .raw(None, c.method.clone(), c.uri, c.content_type, c.body)
            .await;
        assert_eq!(r.status, c.status, "{what}: {}", r.text);
        let events = h.events().await;
        assert_eq!(
            events.len(),
            before + 1,
            "{what} wrote {} events",
            events.len() - before
        );
        let e = &events[0];
        assert_eq!(e.action, c.action, "{what}");
        assert_eq!(e.target, c.target, "{what}");
        assert_eq!(e.actor_subject, "local", "{what}");
        assert_eq!(e.actor_kind, "local", "{what}");
        assert_eq!(
            e.before.is_some(),
            c.before,
            "{what}: before {:?}",
            e.before
        );
        assert_eq!(e.after.is_some(), c.after, "{what}: after {:?}", e.after);
        assert!(
            e.request_id.as_deref().is_some_and(|id| !id.is_empty()),
            "{what}"
        );
        let ws = if c.target.starts_with("workspace:") {
            "team-a"
        } else {
            "default"
        };
        assert_eq!(e.workspace.as_deref(), Some(ws), "{what}");
    }

    // The views are recorded, not the raw rows.
    let events = h.events().await;
    let seed = events.iter().find(|e| e.action == "session.seed").unwrap();
    assert_eq!(seed.after, Some(json!({"resources": 1})));
    let update = events.iter().find(|e| e.action == "mock.update").unwrap();
    assert_eq!(update.before.as_ref().unwrap()["persona"], Value::Null);
    assert_eq!(update.after.as_ref().unwrap()["persona"], "terse");
    let upload = events
        .iter()
        .find(|e| e.target == "contract:orders@1")
        .unwrap();
    assert_eq!(upload.after.as_ref().unwrap()["operations"], 1);

    // API keys: create and revoke.
    let created = h
        .call(
            None,
            Method::POST,
            "/api/v1/workspaces/default/api-keys",
            Some(json!({"name": "ci"})),
        )
        .await
        .ok(StatusCode::CREATED);
    let key_id = created["key"]["key_id"].as_str().unwrap().to_string();
    let e = &h.events().await[0];
    assert_eq!(e.action, "api_key.create");
    assert_eq!(e.target, format!("api_key:{key_id}"));
    assert!(e.before.is_none());
    assert_eq!(e.after.as_ref().unwrap()["key_id"], key_id.as_str());
    let count = h.events().await.len();
    h.call(
        None,
        Method::DELETE,
        &format!("/api/v1/workspaces/default/api-keys/{key_id}"),
        None,
    )
    .await
    .ok(StatusCode::NO_CONTENT);
    let events = h.events().await;
    assert_eq!(events.len(), count + 1);
    assert_eq!(events[0].action, "api_key.revoke");
    assert_eq!(events[0].target, format!("api_key:{key_id}"));
    assert!(events[0].before.is_some());

    // The x-request-id header is used when valid.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/workspaces/default/mocks")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-request-id", "req-42");
    let resp = h
        .app
        .clone()
        .oneshot(
            req.body(Body::from(json!({"name": "rid"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(h.events().await[0].request_id.as_deref(), Some("req-42"));
}

#[tokio::test]
async fn failed_mutation_writes_no_event() {
    let h = harness().await;
    h.call(
        None,
        Method::POST,
        "/api/v1/mocks",
        Some(json!({"name": "orders"})),
    )
    .await
    .ok(StatusCode::CREATED);
    let before = h.events().await.len();
    let r = h
        .raw(
            None,
            Method::POST,
            "/api/v1/mocks/orders/contracts",
            Some("application/json"),
            "{\"not\": \"a contract\"}".into(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
    // A forbidden and a not-found mutation write nothing either.
    h.call(Some("viewer"), Method::DELETE, "/api/v1/mocks/orders", None)
        .await;
    h.call(None, Method::DELETE, "/api/v1/mocks/nope", None)
        .await;
    assert_eq!(h.events().await.len(), before);
}

#[tokio::test]
async fn audit_never_contains_secrets() {
    // SAFETY: no other test in this binary reads this variable.
    unsafe { std::env::set_var("MB_LLM_AUDIT_TEST", "sk-123") };
    let h = harness().await;
    h.call(
        None,
        Method::PUT,
        "/api/v1/llm-profiles/p",
        Some(
            json!({"base_url": "https://llm", "model": "m", "secret_ref": "env:MB_LLM_AUDIT_TEST"}),
        ),
    )
    .await
    .ok(StatusCode::OK);
    let created = h
        .call(
            None,
            Method::POST,
            "/api/v1/workspaces/default/api-keys",
            Some(json!({"name": "ci", "role": "editor"})),
        )
        .await
        .ok(StatusCode::CREATED);
    let token = created["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("mbk_"));
    let secret = token.rsplit_once('_').unwrap().1.to_string();

    let r = h
        .call(
            None,
            Method::GET,
            "/api/v1/workspaces/default/audit?format=jsonl",
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text.lines().count(), 2);
    for text in [
        r.text.clone(),
        serde_json::to_string(&h.events().await).unwrap(),
    ] {
        assert!(!text.contains("sk-123"), "{text}");
        assert!(!text.contains(&token), "{text}");
        assert!(!text.contains(&secret), "{text}");
        assert!(!text.contains("secret_hash"), "{text}");
    }
}

#[tokio::test]
async fn audit_filters_and_paging() {
    let h = harness().await;
    for i in 1..=5 {
        h.call(
            None,
            Method::POST,
            "/api/v1/mocks",
            Some(json!({"name": format!("m{i}")})),
        )
        .await
        .ok(StatusCode::CREATED);
    }
    h.call(
        Some("editor"),
        Method::PATCH,
        "/api/v1/mocks/m1",
        Some(json!({"persona": "x"})),
    )
    .await
    .ok(StatusCode::OK);
    let audit = "/api/v1/workspaces/default/audit";

    let all = h
        .call(None, Method::GET, audit, None)
        .await
        .ok(StatusCode::OK);
    assert_eq!(all.as_array().unwrap().len(), 6);
    let creates = h
        .call(
            None,
            Method::GET,
            &format!("{audit}?action=mock.create"),
            None,
        )
        .await
        .ok(StatusCode::OK);
    let targets: Vec<&str> = creates
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["target"].as_str().unwrap())
        .collect();
    assert_eq!(
        targets,
        ["mock:m5", "mock:m4", "mock:m3", "mock:m2", "mock:m1"]
    );

    let by_editor = h
        .call(None, Method::GET, &format!("{audit}?actor=editor"), None)
        .await
        .ok(StatusCode::OK);
    let by_editor = by_editor.as_array().unwrap();
    assert_eq!(by_editor.len(), 1);
    assert_eq!(by_editor[0]["action"], "mock.update");
    assert_eq!(by_editor[0]["actor_subject"], "editor");
    assert_eq!(by_editor[0]["actor_kind"], "user");
    assert_eq!(by_editor[0]["workspace"], "default");
    assert!(by_editor[0]["at"].is_string());

    let page = h
        .call(
            None,
            Method::GET,
            &format!("{audit}?action=mock.create&limit=2"),
            None,
        )
        .await
        .ok(StatusCode::OK);
    let page = page.as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[1]["target"], "mock:m4");
    let cursor = page[1]["id"].as_i64().unwrap();
    let next = h
        .call(
            None,
            Method::GET,
            &format!("{audit}?action=mock.create&limit=2&before={cursor}"),
            None,
        )
        .await
        .ok(StatusCode::OK);
    let targets: Vec<&str> = next
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["mock:m3", "mock:m2"]);

    // Other workspaces' events are not listed.
    h.call(
        None,
        Method::POST,
        "/api/v1/workspaces",
        Some(json!({"name": "team-a"})),
    )
    .await
    .ok(StatusCode::CREATED);
    h.call(
        None,
        Method::POST,
        "/api/v1/workspaces/team-a/mocks",
        Some(json!({"name": "other"})),
    )
    .await
    .ok(StatusCode::CREATED);
    let all = h
        .call(None, Method::GET, audit, None)
        .await
        .ok(StatusCode::OK);
    assert_eq!(all.as_array().unwrap().len(), 6);

    let bad = h
        .call(None, Method::GET, &format!("{audit}?format=xml"), None)
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.text);
}

fn event(ws: &str, i: usize) -> NewAuditEvent {
    NewAuditEvent {
        workspace: Some(ws.into()),
        actor_subject: "seed".into(),
        actor_kind: "user".into(),
        action: "mock.create".into(),
        target: format!("mock:m{i}"),
        before: None,
        after: Some(json!({ "i": i })),
        request_id: Some(format!("r{i}")),
        at: Utc::now(),
    }
}

#[tokio::test]
async fn audit_jsonl_export() {
    let h = harness().await;
    // More than one page of 1000.
    for i in 0..1203 {
        h.store.append_audit(event("default", i)).await.unwrap();
    }
    h.store.append_audit(event("elsewhere", 0)).await.unwrap();
    let r = h
        .call(
            None,
            Method::GET,
            "/api/v1/workspaces/default/audit?format=jsonl",
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.headers[header::CONTENT_TYPE], "application/jsonl");
    let lines: Vec<Value> = r
        .text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1203);
    assert!(r.text.ends_with('\n'));
    assert_eq!(lines[0]["target"], "mock:m1202");
    assert_eq!(lines[1202]["target"], "mock:m0");
    let mut ids: Vec<i64> = lines.iter().map(|l| l["id"].as_i64().unwrap()).collect();
    ids.dedup();
    assert_eq!(ids.len(), 1203);

    // Filters apply to the export too.
    let r = h
        .call(
            None,
            Method::GET,
            "/api/v1/audit?format=jsonl&action=mock.create",
            None,
        )
        .await;
    assert_eq!(r.text.lines().count(), 1204);
}

#[tokio::test]
async fn global_audit_requires_sysadmin() {
    let h = harness().await;
    h.call(
        None,
        Method::POST,
        "/api/v1/workspaces",
        Some(json!({"name": "team-a"})),
    )
    .await
    .ok(StatusCode::CREATED);
    h.call(
        None,
        Method::POST,
        "/api/v1/mocks",
        Some(json!({"name": "orders"})),
    )
    .await
    .ok(StatusCode::CREATED);
    for token in ["viewer", "editor"] {
        let r = h
            .call(Some(token), Method::GET, "/api/v1/audit", None)
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{token}: {}", r.text);
        assert_eq!(r.headers[header::CONTENT_TYPE], "application/problem+json");
    }
    let all = h
        .call(None, Method::GET, "/api/v1/audit", None)
        .await
        .ok(StatusCode::OK);
    let actions: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["mock.create", "workspace.create"]);
}

#[tokio::test]
async fn viewer_reads_workspace_audit() {
    let h = harness().await;
    h.call(
        None,
        Method::POST,
        "/api/v1/mocks",
        Some(json!({"name": "orders"})),
    )
    .await
    .ok(StatusCode::CREATED);
    h.call(
        None,
        Method::POST,
        "/api/v1/workspaces",
        Some(json!({"name": "team-a"})),
    )
    .await
    .ok(StatusCode::CREATED);
    let list = h
        .call(
            Some("viewer"),
            Method::GET,
            "/api/v1/workspaces/default/audit",
            None,
        )
        .await
        .ok(StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    let r = h
        .call(
            Some("viewer"),
            Method::GET,
            "/api/v1/workspaces/team-a/audit",
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

fn stored(id: i64) -> AuditEvent {
    AuditEvent {
        id,
        workspace: Some("default".into()),
        actor_subject: "local".into(),
        actor_kind: "local".into(),
        action: "mock.create".into(),
        target: format!("mock:m{id}"),
        before: None,
        after: Some(json!({ "name": format!("m{id}") })),
        request_id: Some("r".into()),
        at: Utc::now(),
    }
}

/// Polls until `server` has received `n` requests (or 5 s pass).
async fn wait_for(server: &MockServer, n: usize) -> Vec<wiremock::Request> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let got = server.received_requests().await.unwrap_or_default();
        if got.len() >= n || Instant::now() > deadline {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn webhook_hmac_verified() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let sink = WebhookSink::spawn(
        format!("{}/hook", server.uri()),
        Some("s3cret".into()),
        reqwest::Client::new(),
    );
    sink.send(stored(7));
    let got = wait_for(&server, 1).await;
    assert_eq!(got.len(), 1);
    let req = &got[0];
    assert_eq!(req.headers["content-type"], "application/json");
    let sig = req.headers["x-mockingbird-signature"].to_str().unwrap();
    assert_eq!(sig, signature(b"s3cret", &req.body));
    assert!(sig.starts_with("sha256=") && sig.len() == 7 + 64, "{sig}");
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["id"], 7);
    assert_eq!(body["action"], "mock.create");
    assert_eq!(body["target"], "mock:m7");

    // The known-answer HMAC-SHA256 test vector (RFC 4231 case 2).
    assert_eq!(
        signature(b"Jefe", b"what do ya want for nothing?"),
        "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[tokio::test]
async fn webhook_retries_then_gives_up() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let sink = WebhookSink::spawn_with(
        server.uri(),
        None,
        reqwest::Client::new(),
        Duration::from_millis(10),
    );
    sink.send(stored(1));
    assert_eq!(wait_for(&server, 4).await.len(), 4);
    // No fifth attempt.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = server.received_requests().await.unwrap();
    assert_eq!(got.len(), 4);
    assert!(got[0].headers.get("x-mockingbird-signature").is_none());
}

#[tokio::test]
async fn webhook_failure_does_not_affect_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
        .mount(&server)
        .await;
    let sink = WebhookSink::spawn(server.uri(), Some("k".into()), reqwest::Client::new());
    let h = harness_with(Some(sink)).await;
    let start = Instant::now();
    for i in 0..3 {
        h.call(
            None,
            Method::POST,
            "/api/v1/mocks",
            Some(json!({"name": format!("m{i}")})),
        )
        .await
        .ok(StatusCode::CREATED);
    }
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(h.events().await.len(), 3);
}

#[tokio::test]
async fn webhook_queue_full_drops() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
        .mount(&server)
        .await;
    let sink = WebhookSink::spawn(server.uri(), None, reqwest::Client::new());
    let start = Instant::now();
    for i in 0..2000 {
        sink.send(stored(i));
    }
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn retention_prunes_old_events() {
    let h = harness().await;
    let mut old = event("default", 1);
    old.at = Utc::now() - chrono::Duration::days(400);
    h.store.append_audit(old).await.unwrap();
    let mut recent = event("default", 2);
    recent.at = Utc::now() - chrono::Duration::days(300);
    h.store.append_audit(recent).await.unwrap();
    let task = spawn_retention(h.store.clone(), 365);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut left = h.events().await;
    while left.len() != 1 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        left = h.events().await;
    }
    task.abort();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].target, "mock:m2");
}
