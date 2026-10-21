//! The live request tail: `GET …/mocks/{mock}/requests/stream` over SSE.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use mb_admin::{AdminState, LiveSubscribe, router};
use mb_auth::{AuthError, Authenticate, Principal, PrincipalKind, Role};
use mb_protocols::{RequestKind, RequestRecord};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower::ServiceExt;

/// `Bearer viewer` → viewer of `default`; `Bearer team-editor` → editor of
/// `team-a` only; `Bearer sysadmin` → sysadmin. A token in `revoked` is
/// rejected.
#[derive(Clone, Default)]
struct TestAuth {
    revoked: Arc<Mutex<HashSet<String>>>,
}

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
        if self.revoked.lock().unwrap().contains(name) {
            return Err(AuthError::Unauthenticated("revoked".into()));
        }
        let p = match name {
            "viewer" => principal(name, false, &[("default", Role::Viewer)]),
            "team-editor" => principal(name, false, &[("team-a", Role::Editor)]),
            "sysadmin" => principal(name, true, &[]),
            _ => return Err(AuthError::Unauthenticated("unknown token".into())),
        };
        Ok(Some(p))
    }
}

struct Harness {
    app: Router,
    tx: broadcast::Sender<RequestRecord>,
    auth: TestAuth,
}

/// Mocks `orders` and `other` in `default`, and `orders` in `team-a`; the
/// live feed is a broadcast channel of `capacity` (`None`: no live feed).
async fn harness(capacity: Option<usize>) -> Harness {
    harness_with(capacity, Duration::from_secs(30)).await
}

/// [`harness`] with streams re-checking their caller every `recheck`.
async fn harness_with(capacity: Option<usize>, recheck: Duration) -> Harness {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let (tx, _) = broadcast::channel(capacity.unwrap_or(16));
    let live: Option<LiveSubscribe> = capacity.map(|_| {
        let tx = tx.clone();
        Arc::new(move || tx.subscribe()) as LiveSubscribe
    });
    let auth = TestAuth::default();
    let app = router(AdminState {
        auth: Arc::new(auth.clone()),
        live,
        live_recheck: recheck,
        ..AdminState::new(store, state)
    });
    let h = Harness { app, tx, auth };
    for (uri, body) in [
        ("/api/v1/workspaces", json!({ "name": "team-a" })),
        ("/api/v1/mocks", json!({ "name": "orders" })),
        ("/api/v1/mocks", json!({ "name": "other" })),
        (
            "/api/v1/workspaces/team-a/mocks",
            json!({ "name": "orders", "route_prefix": "/team-a-orders" }),
        ),
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(
                Request::post(uri)
                    .header(header::AUTHORIZATION, "Bearer sysadmin")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "POST {uri}");
    }
    h
}

impl Harness {
    async fn get(&self, who: &str, uri: &str) -> Response<Body> {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(uri)
                    .header(header::AUTHORIZATION, format!("Bearer {who}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    fn send(&self, r: RequestRecord) {
        self.tx.send(r).unwrap();
    }

    fn revoke(&self, token: &str) {
        self.auth.revoked.lock().unwrap().insert(token.to_string());
    }
}

/// Reads `body` until it ends, failing after 2 s; returns the `event:`
/// names seen.
async fn read_to_end(body: &mut Body) -> Vec<String> {
    let mut seen = Vec::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap_or_else(|_| panic!("stream still open; saw {seen:?}"));
        let Some(frame) = frame else {
            return seen;
        };
        if let Ok(data) = frame.unwrap().into_data() {
            for line in std::str::from_utf8(&data).unwrap().lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    seen.push(v.trim().to_string());
                }
            }
        }
    }
}

fn record(workspace: &str, mock: &str, op: &str, session: &str, source: &str) -> RequestRecord {
    RequestRecord {
        workspace: workspace.into(),
        mock: mock.into(),
        operation_id: Some(op.into()),
        session: session.into(),
        method: "GET".into(),
        path: "/orders".into(),
        status: 200,
        source: Some(source.into()),
        fallback_reason: None,
        latency: Duration::from_millis(12),
        prompt_tokens: 3,
        completion_tokens: 4,
        request: json!({ "query": {}, "headers": {}, "body": null }),
        request_body_bytes: None,
        response: Some(json!(["a"])),
        kind: RequestKind::Http,
    }
}

/// One server-sent event: its `event:` name and its parsed `data:`.
#[derive(Debug)]
struct Sse {
    event: String,
    data: Value,
}

/// Reads SSE events from `body` until `n` events (keep-alive comments are
/// skipped) have arrived, failing after 2 s.
async fn read_events(body: &mut Body, n: usize) -> Vec<Sse> {
    let mut buf = String::new();
    let mut events = Vec::new();
    while events.len() < n {
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap_or_else(|_| panic!("timed out; got {events:?}, pending {buf:?}"))
            .expect("stream ended")
            .unwrap();
        let Ok(data) = frame.into_data() else {
            continue;
        };
        buf.push_str(std::str::from_utf8(&data).unwrap());
        while let Some(end) = buf.find("\n\n") {
            let block: String = buf.drain(..end + 2).collect();
            let mut event = String::from("message");
            let mut data = String::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push_str(v.trim_start());
                }
            }
            if !data.is_empty() {
                events.push(Sse {
                    event,
                    data: serde_json::from_str(&data).unwrap(),
                });
            }
        }
    }
    events
}

const STREAM: &str = "/api/v1/workspaces/default/mocks/orders/requests/stream";

#[tokio::test]
async fn stream_emits_matching_records() {
    let h = harness(Some(16)).await;
    let resp = h.get("viewer", STREAM).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let mut body = resp.into_body();
    h.send(record("default", "other", "listOrders", "s1", "llm"));
    h.send(record("team-a", "orders", "listOrders", "s1", "llm"));
    h.send(record("default", "orders", "listOrders", "s1", "llm"));
    let events = read_events(&mut body, 1).await;
    assert_eq!(events[0].event, "request");
    let d = &events[0].data;
    assert_eq!(d["workspace"], "default");
    assert_eq!(d["mock"], "orders");
    assert_eq!(d["operation_id"], "listOrders");
    assert_eq!(d["session"], "s1");
    assert_eq!(d["method"], "GET");
    assert_eq!(d["path"], "/orders");
    assert_eq!(d["status"], 200);
    assert_eq!(d["source"], "llm");
    assert_eq!(d["latency_ms"], 12);
    assert_eq!(d["prompt_tokens"], 3);
    assert_eq!(d["completion_tokens"], 4);
    assert_eq!(d["response"], json!(["a"]));
    assert!(d["at"].is_string());

    // The P2 alias streams `default` too.
    let mut alias = h
        .get("viewer", "/api/v1/mocks/orders/requests/stream")
        .await
        .into_body();
    h.send(record("default", "orders", "getOrder", "s2", "llm"));
    let events = read_events(&mut alias, 1).await;
    assert_eq!(events[0].data["operation_id"], "getOrder");
}

#[tokio::test]
async fn stream_filters_by_operation_session_source() {
    let h = harness(Some(16)).await;
    let mut body = h
        .get(
            "viewer",
            &format!("{STREAM}?operation=listOrders&session=s1&source=llm"),
        )
        .await
        .into_body();
    h.send(record("default", "orders", "getOrder", "s1", "llm"));
    h.send(record("default", "orders", "listOrders", "s2", "llm"));
    h.send(record("default", "orders", "listOrders", "s1", "fallback"));
    h.send(record("default", "orders", "listOrders", "s1", "llm"));
    let events = read_events(&mut body, 1).await;
    let d = &events[0].data;
    assert_eq!(
        (&d["operation_id"], &d["session"], &d["source"]),
        (&json!("listOrders"), &json!("s1"), &json!("llm"))
    );
    // Nothing else matched: the next event is the next matching record.
    h.send(record("default", "orders", "listOrders", "s1", "llm"));
    let events = read_events(&mut body, 1).await;
    assert_eq!(events[0].data["session"], "s1");
}

#[tokio::test]
async fn lagged_subscriber_gets_lagged_event() {
    let h = harness(Some(4)).await;
    let mut body = h.get("viewer", STREAM).await.into_body();
    for i in 0..10 {
        h.send(record(
            "default",
            "orders",
            "listOrders",
            &format!("s{i}"),
            "llm",
        ));
    }
    let events = read_events(&mut body, 2).await;
    assert_eq!(events[0].event, "lagged");
    assert!(events[0].data["dropped"].as_u64().unwrap() > 0);
    assert_eq!(events[1].event, "request");
    h.send(record("default", "orders", "listOrders", "later", "llm"));
    let mut rest = read_events(&mut body, 4).await;
    let last = rest.pop().unwrap();
    assert_eq!(last.event, "request");
    assert_eq!(last.data["session"], "later");
}

#[tokio::test]
async fn stream_ends_when_credential_revoked() {
    let h = harness_with(Some(16), Duration::from_millis(100)).await;
    let mut body = h.get("viewer", STREAM).await.into_body();
    h.send(record("default", "orders", "listOrders", "s1", "llm"));
    assert_eq!(read_events(&mut body, 1).await[0].event, "request");
    h.revoke("viewer");
    tokio::time::sleep(Duration::from_millis(250)).await;
    // The stream is closed (or closes) without delivering this call.
    let _ =
        h.tx.send(record("default", "orders", "listOrders", "s2", "llm"));
    let seen = read_to_end(&mut body).await;
    assert!(!seen.contains(&"request".to_string()), "{seen:?}");
}

#[tokio::test]
async fn stream_ends_when_workspace_deleted() {
    let h = harness_with(Some(16), Duration::from_millis(100)).await;
    let mut body = h
        .get(
            "sysadmin",
            "/api/v1/workspaces/team-a/mocks/orders/requests/stream",
        )
        .await
        .into_body();
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::delete("/api/v1/workspaces/team-a?force=true")
                .header(header::AUTHORIZATION, "Bearer sysadmin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let seen = read_to_end(&mut body).await;
    assert!(seen.is_empty(), "{seen:?}");
}

#[tokio::test]
async fn stream_stays_open_while_credential_valid() {
    let h = harness_with(Some(16), Duration::from_millis(50)).await;
    let mut body = h.get("viewer", STREAM).await.into_body();
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.send(record("default", "orders", "listOrders", "s1", "llm"));
    assert_eq!(read_events(&mut body, 1).await[0].event, "request");
}

#[tokio::test]
async fn stream_requires_read() {
    let h = harness(Some(16)).await;
    let resp = h.get("team-editor", STREAM).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = h
        .get("team-editor", "/api/v1/mocks/orders/requests/stream")
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn stream_unknown_mock_404() {
    let h = harness(Some(16)).await;
    let resp = h
        .get(
            "viewer",
            "/api/v1/workspaces/default/mocks/nope/requests/stream",
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
}

#[tokio::test]
async fn stream_without_live_feed_503() {
    let h = harness(None).await;
    let resp = h.get("viewer", STREAM).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
}

#[test]
fn openapi_documents_the_stream() {
    let doc = serde_json::to_value(mb_admin::openapi_document()).unwrap();
    for path in [
        "/api/v1/workspaces/{ws}/mocks/{mock}/requests/stream",
        "/api/v1/mocks/{mock}/requests/stream",
    ] {
        let ok = &doc["paths"][path]["get"]["responses"]["200"]["content"]["text/event-stream"];
        assert_eq!(
            ok["schema"]["$ref"], "#/components/schemas/LiveRequestView",
            "{path}: {ok}"
        );
    }
    assert_eq!(
        doc["paths"]["/api/v1/mocks/{mock}/requests/stream"]["get"]["deprecated"],
        true
    );
}

#[tokio::test]
async fn live_stream_filters_kind() {
    let h = harness(Some(16)).await;
    let mut body = h
        .get("viewer", &format!("{STREAM}?kind=event"))
        .await
        .into_body();
    h.send(record(
        "default",
        "orders",
        "listOrders",
        "http-call",
        "llm",
    ));
    h.send(RequestRecord {
        kind: RequestKind::Event,
        method: "PUBLISH".into(),
        ..record("default", "orders", "orderCreated", "event-call", "faker")
    });
    let events = read_events(&mut body, 1).await;
    let d = &events[0].data;
    assert_eq!(d["kind"], "event");
    assert_eq!(d["session"], "event-call");
    assert_eq!(d["method"], "PUBLISH");
}

#[tokio::test]
async fn live_stream_unknown_kind_400() {
    let h = harness(Some(16)).await;
    let resp = h.get("viewer", &format!("{STREAM}?kind=nope")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
}
