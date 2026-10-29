//! P3 × P5: the channels Admin API under `/api/v1/workspaces/{ws}`, with
//! RBAC (viewers read, editors change and publish), one audit event per
//! change or publish, and the P2 aliases for `default`.
#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
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

const WS: &str = "team-a";
const ORDERS: &str = include_str!("../../../fixtures/asyncapi/orders-amqp-2.6.yaml");

/// `Bearer team-viewer|team-editor` → that role in `team-a`;
/// `Bearer sysadmin` → sysadmin.
struct TestAuth;

#[async_trait]
impl Authenticate for TestAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
        let Some(value) = headers.get(header::AUTHORIZATION) else {
            return Ok(None);
        };
        let name = value.to_str().unwrap().trim_start_matches("Bearer ");
        let (sysadmin, roles) = match name {
            "team-viewer" => (false, vec![(WS, Role::Viewer)]),
            "team-editor" => (false, vec![(WS, Role::Editor)]),
            "sysadmin" => (true, vec![]),
            _ => return Err(AuthError::Unauthenticated("unknown token".into())),
        };
        Ok(Some(Principal {
            subject: name.into(),
            display_name: name.into(),
            kind: PrincipalKind::User,
            sysadmin,
            roles: roles
                .into_iter()
                .map(|(ws, r)| (ws.to_string(), r))
                .collect::<BTreeMap<_, _>>(),
        }))
    }
}

/// Records `(workspace, mock, channel)` of every publish.
#[derive(Default)]
struct FakeControl {
    calls: Mutex<Vec<(String, String, String)>>,
    /// When set, a publish delivers this many messages and then fails.
    fail_after: Mutex<Option<usize>>,
}

#[async_trait]
impl ChannelControl for FakeControl {
    async fn publish(
        &self,
        ws: &str,
        mock: &str,
        channel: &str,
        count: usize,
        _context: Option<Value>,
    ) -> Result<Vec<PublishedMessage>, ChannelControlError> {
        self.calls
            .lock()
            .unwrap()
            .push((ws.into(), mock.into(), channel.into()));
        let fail_after = *self.fail_after.lock().unwrap();
        let messages = (0..fail_after.unwrap_or(count).min(count))
            .map(|i| PublishedMessage {
                address: "orders.created".into(),
                key: None,
                headers: BTreeMap::new(),
                payload: json!({ "id": format!("o{i}") }),
                source: "fallback".into(),
            })
            .collect();
        if fail_after.is_some() {
            return Err(ChannelControlError::Partial {
                published: messages,
                error: Box::new(ChannelControlError::Failed("broker unreachable".into())),
            });
        }
        Ok(messages)
    }

    fn activity(&self, ws: &str, mock: &str) -> BTreeMap<String, ChannelActivityView> {
        if (ws, mock) != (WS, "orders") {
            return BTreeMap::new();
        }
        BTreeMap::from([(
            "orderCreated".into(),
            ChannelActivityView {
                published: 3,
                received: 0,
                last_published_at: None,
                last_received_at: None,
                last_error: None,
            },
        )])
    }

    fn brokers(&self) -> Vec<BrokerView> {
        vec![BrokerView {
            name: "local".into(),
            kind: "memory".into(),
            url: "memory://".into(),
            status: "connected".into(),
        }]
    }
}

async fn send(
    app: &Router,
    who: Option<&str>,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
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
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn setup() -> (Router, Arc<FakeControl>) {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let control = Arc::new(FakeControl::default());
    let shared: SharedChannelControl = control.clone();
    let app = router(AdminState {
        auth: Arc::new(TestAuth),
        ..AdminState::new(store, state)
    })
    .layer(Extension(shared));
    let (s, body) = send(
        &app,
        Some("sysadmin"),
        Method::POST,
        "/api/v1/workspaces",
        Some(json!({"name": WS})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let (s, body) = send(
        &app,
        Some("team-editor"),
        Method::POST,
        &format!("/api/v1/workspaces/{WS}/mocks"),
        Some(json!({"name": "orders"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let req = Request::post(format!("/api/v1/workspaces/{WS}/mocks/orders/contracts"))
        .header(header::AUTHORIZATION, "Bearer team-editor")
        .header(header::CONTENT_TYPE, "application/vnd.aai.asyncapi+yaml")
        .body(Body::from(ORDERS))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    (app, control)
}

async fn audit(app: &Router, action: &str) -> Vec<Value> {
    let (s, events) = send(
        app,
        Some("team-editor"),
        Method::GET,
        &format!("/api/v1/workspaces/{WS}/audit?action={action}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{events}");
    events.as_array().unwrap().clone()
}

#[tokio::test]
async fn channels_are_workspace_scoped_rbac_checked_and_audited() {
    let (app, control) = setup().await;
    let base = format!("/api/v1/workspaces/{WS}/mocks/orders");

    // Viewers read, with the workspace's activity.
    let (s, list) = send(
        &app,
        Some("team-viewer"),
        Method::GET,
        &format!("{base}/channels"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let created = list
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "orderCreated")
        .unwrap()
        .clone();
    assert_eq!(created["activity"]["published"], 3);
    let (s, _) = send(
        &app,
        Some("team-viewer"),
        Method::GET,
        &format!("{base}/channels/orderCreated"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(
        &app,
        Some("team-viewer"),
        Method::GET,
        &format!("{base}/event-settings"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // A viewer may not publish or change settings; nothing runs or is audited.
    let publish = format!("{base}/channels/orderCreated/publish");
    let (s, body) = send(
        &app,
        Some("team-viewer"),
        Method::POST,
        &publish,
        Some(json!({"count": 1})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    let (s, _) = send(
        &app,
        Some("team-viewer"),
        Method::PUT,
        &format!("{base}/channels/orderCreated"),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = send(
        &app,
        Some("team-viewer"),
        Method::PUT,
        &format!("{base}/event-settings"),
        Some(json!({"broker": "local"})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(control.calls.lock().unwrap().is_empty());
    assert!(audit(&app, "channel.publish").await.is_empty());

    // An editor publishes: the control is told the workspace; one audit
    // event per publish.
    for n in [2, 1] {
        let (s, body) = send(
            &app,
            Some("team-editor"),
            Method::POST,
            &publish,
            Some(json!({"count": n, "context": {"customer": {"email": "ada@example.com"}, "token": "s3cret"}})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(body["messages"].as_array().unwrap().len(), n);
    }
    assert_eq!(
        *control.calls.lock().unwrap(),
        vec![(WS.into(), "orders".into(), "orderCreated".into()); 2]
    );
    let events = audit(&app, "channel.publish").await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["actor_subject"], "team-editor");
    assert_eq!(events[0]["workspace"], WS);
    assert_eq!(events[0]["target"], "channel:orders/orderCreated");
    assert_eq!(events[0]["after"]["count"], 1);
    assert_eq!(events[1]["after"]["count"], 2);
    assert_eq!(events[0]["after"]["published"], 1);
    // The caller's context is not stored, only its top-level keys.
    assert_eq!(
        events[0]["after"]["context_keys"],
        json!(["customer", "token"])
    );
    let stored = serde_json::to_string(&events).unwrap();
    assert!(!stored.contains("s3cret"), "{stored}");
    assert!(!stored.contains("ada@example.com"), "{stored}");

    // Editors change channel and event settings, each audited once.
    let (s, body) = send(
        &app,
        Some("team-editor"),
        Method::PUT,
        &format!("{base}/channels/orderCreated"),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let events = audit(&app, "channel.update").await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["after"]["enabled"], false);
    let (s, body) = send(
        &app,
        Some("team-editor"),
        Method::PUT,
        &format!("{base}/event-settings"),
        Some(json!({"broker": "local"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let events = audit(&app, "event_settings.update").await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["after"]["broker"], "local");
    assert_eq!(events[0]["target"], "mock:orders");

    // The P2 alias addresses `default`, where this mock does not exist.
    let (s, _) = send(
        &app,
        Some("sysadmin"),
        Method::GET,
        "/api/v1/mocks/orders/channels",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn brokers_need_a_principal() {
    let (app, _) = setup().await;
    let (s, body) = send(&app, None, Method::GET, "/api/v1/brokers", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
    let (s, body) = send(
        &app,
        Some("team-viewer"),
        Method::GET,
        "/api/v1/brokers",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body[0]["name"], "local");
}

#[tokio::test]
async fn publish_failing_partway_is_audited_with_what_went_out() {
    let (app, control) = setup().await;
    let publish = format!("/api/v1/workspaces/{WS}/mocks/orders/channels/orderCreated/publish");

    *control.fail_after.lock().unwrap() = Some(2);
    let (s, body) = send(
        &app,
        Some("team-editor"),
        Method::POST,
        &publish,
        Some(json!({"count": 5})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{body}");
    let detail = body["detail"].as_str().unwrap();
    assert!(detail.contains("broker unreachable"), "{detail}");
    assert!(detail.contains("2 of 5 messages"), "{detail}");

    let events = audit(&app, "channel.publish").await;
    assert_eq!(events.len(), 1, "{events:?}");
    let after = &events[0]["after"];
    assert_eq!(after["count"], 5);
    assert_eq!(after["published"], 2);
    assert_eq!(after["error"], "broker unreachable");
    assert_eq!(after["addresses"].as_array().unwrap().len(), 2);

    // A publish that delivers nothing is not audited.
    *control.fail_after.lock().unwrap() = Some(0);
    let (s, body) = send(
        &app,
        Some("team-editor"),
        Method::POST,
        &publish,
        Some(json!({"count": 3})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(audit(&app, "channel.publish").await.len(), 1);
}

#[tokio::test]
async fn team_a_principals_cannot_reach_team_b_channels() {
    let (app, control) = setup().await;
    let (s, body) = send(
        &app,
        Some("sysadmin"),
        Method::POST,
        "/api/v1/workspaces",
        Some(json!({"name": "team-b"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let (s, body) = send(
        &app,
        Some("sysadmin"),
        Method::POST,
        "/api/v1/workspaces/team-b/mocks",
        Some(json!({"name": "orders", "route_prefix": "/orders-b"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let req = Request::post("/api/v1/workspaces/team-b/mocks/orders/contracts")
        .header(header::AUTHORIZATION, "Bearer sysadmin")
        .header(header::CONTENT_TYPE, "application/vnd.aai.asyncapi+yaml")
        .body(Body::from(ORDERS))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::CREATED
    );

    // Same-named mock in another workspace, and the `default` aliases:
    // every channel route is a 403 for team-a's principals.
    for base in [
        "/api/v1/workspaces/team-b/mocks/orders",
        "/api/v1/mocks/orders",
    ] {
        for who in ["team-viewer", "team-editor"] {
            for (method, uri, body) in [
                (Method::GET, format!("{base}/channels"), None),
                (Method::GET, format!("{base}/channels/orderCreated"), None),
                (Method::GET, format!("{base}/event-settings"), None),
                (
                    Method::PUT,
                    format!("{base}/channels/orderCreated"),
                    Some(json!({"enabled": false})),
                ),
                (
                    Method::POST,
                    format!("{base}/channels/orderCreated/publish"),
                    Some(json!({"count": 1})),
                ),
                (
                    Method::PUT,
                    format!("{base}/event-settings"),
                    Some(json!({"broker": "local"})),
                ),
            ] {
                let (s, body) = send(&app, Some(who), method.clone(), &uri, body).await;
                assert_eq!(s, StatusCode::FORBIDDEN, "{who}: {method} {uri}: {body}");
            }
        }
    }
    assert!(control.calls.lock().unwrap().is_empty());
    let (s, events) = send(
        &app,
        Some("sysadmin"),
        Method::GET,
        "/api/v1/workspaces/team-b/audit?action=channel.publish",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{events}");
    assert!(events.as_array().unwrap().is_empty());
}
