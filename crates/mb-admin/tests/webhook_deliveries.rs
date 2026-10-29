//! `GET …/mocks/{mock}/webhooks/deliveries`: the in-memory delivery log.
#![cfg(test)]

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::{Extension, Router};
use mb_admin::{AdminState, router};
use mb_auth::{AuthError, Authenticate, Principal, PrincipalKind, Role};
use mb_protocols::webhooks::log::{DeliveryRecord, DeliveryStatus, SharedDeliveryLog};
use mb_state::{MemoryStore, StateStore};
use mb_store::DEFAULT_WORKSPACE;
use serde_json::{Value, json};
use tower::ServiceExt;

/// No header → the Local principal; `Bearer viewer` → a viewer of `default`.
struct TestAuth;

#[async_trait::async_trait]
impl Authenticate for TestAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
        let Some(value) = headers.get(header::AUTHORIZATION) else {
            return Ok(Some(Principal::local()));
        };
        match value.to_str().unwrap() {
            "Bearer viewer" => Ok(Some(Principal {
                subject: "viewer".into(),
                display_name: "viewer".into(),
                kind: PrincipalKind::User,
                sysadmin: false,
                roles: [("default".to_string(), Role::Viewer)].into(),
            })),
            _ => Err(AuthError::Unauthenticated("unknown token".into())),
        }
    }
}

fn record(id: &str, status: DeliveryStatus, attempts: u32, code: u16) -> DeliveryRecord {
    DeliveryRecord {
        id: id.into(),
        event_id: format!("evt_{id}"),
        event_type: "order.created".into(),
        operation: "createOrder".into(),
        session: "default".into(),
        url: "https://h.test/x".into(),
        status,
        attempts,
        response_status: Some(code),
        error: (code >= 300).then(|| format!("receiver answered {code}")),
        created_at: SystemTime::now(),
        finished_at: Some(SystemTime::now()),
    }
}

async fn app(with_log: bool) -> Router {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let mut st = AdminState::new(store, state);
    st.auth = Arc::new(TestAuth);
    let app = router(st);
    let (status, _) = send(
        &app,
        "POST",
        "/api/v1/mocks",
        Some(json!({"name": "shop"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    if !with_log {
        return app;
    }
    let log = SharedDeliveryLog::default();
    log.push(
        DEFAULT_WORKSPACE,
        "shop",
        record("whd_1", DeliveryStatus::Delivered, 1, 200),
    );
    log.push(
        DEFAULT_WORKSPACE,
        "shop",
        record("whd_2", DeliveryStatus::Failed, 4, 503),
    );
    app.layer(Extension(log))
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let ct = resp.headers().get("content-type").cloned();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status.is_client_error() {
        assert_eq!(ct.unwrap(), "application/problem+json", "{v}");
    }
    (status, v)
}

#[tokio::test]
async fn lists_deliveries_newest_first() {
    let app = app(true).await;
    let base = "/api/v1/workspaces/default/mocks";
    let (status, body) = send(
        &app,
        "GET",
        &format!("{base}/shop/webhooks/deliveries?limit=1"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "whd_2");
    assert_eq!(items[0]["status"], "failed");
    assert_eq!(items[0]["attempts"], 4);
    assert_eq!(items[0]["response_status"], 503);
    assert_eq!(items[0]["url"], "https://h.test/x");
    let created = items[0]["created_at"].as_str().unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(created).is_ok(),
        "{created}"
    );
    // The default alias and the default limit.
    let (status, body) = send(
        &app,
        "GET",
        "/api/v1/mocks/shop/webhooks/deliveries",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert_eq!(body[1]["status"], "delivered");
    // A viewer may read it.
    let (status, _) = send(
        &app,
        "GET",
        &format!("{base}/shop/webhooks/deliveries"),
        None,
        Some("viewer"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(
        &app,
        "GET",
        &format!("{base}/nope/webhooks/deliveries"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    for bad in ["0", "201"] {
        let (status, _) = send(
            &app,
            "GET",
            &format!("{base}/shop/webhooks/deliveries?limit={bad}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "limit {bad}");
    }
}

#[tokio::test]
async fn no_log_lists_nothing() {
    let app = app(false).await;
    let (status, body) = send(
        &app,
        "GET",
        "/api/v1/mocks/shop/webhooks/deliveries",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}
