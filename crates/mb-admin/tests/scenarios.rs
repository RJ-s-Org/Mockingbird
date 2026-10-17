#![allow(clippy::unwrap_used)]
//! Chaos on scenarios.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use mb_admin::{AdminState, router};
use mb_state::{MemoryStore, StateStore};
use mb_store::{AuditFilter, ControlStore, DEFAULT_WORKSPACE};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    app: Router,
    store: Arc<dyn ControlStore>,
}

async fn harness() -> Harness {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let app = router(AdminState::new(store.clone(), state));
    Harness { app, store }
}

impl Harness {
    async fn send(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let resp = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }
}

#[tokio::test]
async fn put_scenario_with_chaos() {
    let h = harness().await;
    let (status, body) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(json!({ "name": "orders" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let chaos = json!({
        "error_rate": 0.5,
        "latency_jitter": { "min_ms": 0, "max_ms": 100 },
        "drop_stream_rate": 0.0
    });
    let (status, view) = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/scenarios/flaky",
            Some(json!({ "chaos": chaos })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["chaos"], chaos);

    let (status, list) = h
        .send(Method::GET, "/api/v1/mocks/orders/scenarios", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["chaos"], chaos);

    let mock = h
        .store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    let stored = h.store.list_scenarios(mock.id).await.unwrap();
    assert_eq!(
        stored[0].rules.chaos.as_ref().map(|c| c.error_rate),
        Some(0.5)
    );

    let events = h
        .store
        .list_audit(&AuditFilter {
            action: Some("scenario.put".into()),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].after.as_ref().unwrap()["chaos"], chaos);

    // Absent chaos clears it (PUT replaces the scenario).
    let (status, view) = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/scenarios/flaky",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert!(view.get("chaos").is_none_or(Value::is_null), "{view}");

    let (status, problem) = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/scenarios/flaky",
            Some(json!({ "chaos": { "latency_jitter": { "min_ms": 10, "max_ms": 5 } } })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("chaos.latency_jitter"),
        "{problem}"
    );
}

#[tokio::test]
async fn put_scenario_interval_over_30s_422() {
    let h = harness().await;
    let (status, body) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(json!({ "name": "orders" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, problem) = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/scenarios/slow",
            Some(json!({ "stream_item_interval_ms": 30_001 })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("stream_item_interval_ms"),
        "{problem}"
    );

    let (status, view) = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/scenarios/slow",
            Some(json!({ "stream_item_interval_ms": 30_000 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["stream_item_interval_ms"], 30_000);
}
