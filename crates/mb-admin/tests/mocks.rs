#![allow(clippy::unwrap_used)]
//! Chaos and limits on the mock resource.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use mb_admin::{AdminState, router};
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    AuditFilter, ChaosSettings, ControlStore, DEFAULT_WORKSPACE, MockLimits, NewMock, RateLimit,
    ResourceSource,
};
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

    async fn create(&self, name: &str) {
        let (status, body) = self
            .send(Method::POST, "/api/v1/mocks", Some(json!({ "name": name })))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    async fn patch(&self, name: &str, body: Value) -> (StatusCode, Value) {
        self.send(Method::PATCH, &format!("/api/v1/mocks/{name}"), Some(body))
            .await
    }
}

fn chaos_json() -> Value {
    json!({
        "error_rate": 0.2,
        "latency_jitter": { "min_ms": 5, "max_ms": 50 },
        "drop_stream_rate": 0.1
    })
}

fn limits_json() -> Value {
    json!({
        "rate_limit": { "requests_per_second": 10.0, "burst": 20 },
        "daily_token_budget": 5000,
        "max_request_bytes": 2048
    })
}

#[tokio::test]
async fn patch_chaos_and_limits() {
    let h = harness().await;
    h.create("orders").await;
    let (_, plain) = h.send(Method::GET, "/api/v1/mocks/orders", None).await;
    assert_eq!(plain["limits"], json!({}));
    assert!(plain.get("chaos").is_none_or(Value::is_null), "{plain}");

    let (status, view) = h
        .patch(
            "orders",
            json!({ "chaos": chaos_json(), "limits": limits_json() }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["chaos"], chaos_json());
    assert_eq!(view["limits"], limits_json());

    let (_, got) = h.send(Method::GET, "/api/v1/mocks/orders", None).await;
    assert_eq!(got["chaos"], chaos_json());
    assert_eq!(got["limits"], limits_json());

    let stored = h
        .store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.chaos.as_ref().map(|c| c.error_rate), Some(0.2));
    assert_eq!(stored.limits.daily_token_budget, Some(5000));

    // `limits` replaces the whole object.
    let (status, view) = h
        .patch(
            "orders",
            json!({ "limits": { "max_response_bytes": 4096 } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["limits"], json!({ "max_response_bytes": 4096 }));
    assert_eq!(view["chaos"], chaos_json(), "absent chaos is kept");

    let events = h
        .store
        .list_audit(&AuditFilter {
            action: Some("mock.update".into()),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].after.as_ref().unwrap()["limits"],
        json!({ "max_response_bytes": 4096 })
    );
}

#[tokio::test]
async fn create_with_chaos_and_limits() {
    let h = harness().await;
    let (status, view) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(json!({ "name": "made", "chaos": chaos_json(), "limits": limits_json() })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{view}");
    assert_eq!(view["chaos"], chaos_json());
    assert_eq!(view["limits"], limits_json());

    let (status, problem) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(json!({ "name": "bad", "chaos": { "error_rate": 1.5 } })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("chaos.error_rate must be between 0 and 1"),
        "{problem}"
    );
}

#[tokio::test]
async fn patch_invalid_rate_limit_422() {
    let h = harness().await;
    h.create("orders").await;
    let (status, problem) = h
        .patch(
            "orders",
            json!({ "limits": { "rate_limit": { "requests_per_second": 5.0, "burst": 0 } } }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("limits.rate_limit.burst"),
        "{problem}"
    );
    let stored = h
        .store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.limits, MockLimits::default());
}

#[tokio::test]
async fn patch_null_chaos_clears() {
    let h = harness().await;
    h.create("orders").await;
    let (status, _) = h.patch("orders", json!({ "chaos": chaos_json() })).await;
    assert_eq!(status, StatusCode::OK);
    let (status, view) = h.patch("orders", json!({ "chaos": null })).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert!(view.get("chaos").is_none_or(Value::is_null), "{view}");
    let stored = h
        .store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.chaos, None);
}

#[tokio::test]
async fn file_managed_mock_rejects_chaos_patch() {
    let h = harness().await;
    h.store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: "filed".into(),
                route_prefix: None,
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::File,
                data_plane_auth: Default::default(),
                chaos: Some(ChaosSettings {
                    error_rate: 0.5,
                    ..ChaosSettings::default()
                }),
                limits: MockLimits {
                    rate_limit: Some(RateLimit {
                        requests_per_second: 1.0,
                        burst: 1,
                    }),
                    ..MockLimits::default()
                },
            },
        )
        .await
        .unwrap();
    let (status, problem) = h.patch("filed", json!({ "chaos": null })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["title"], "Managed by config file", "{problem}");
}

#[tokio::test]
async fn patch_limits_null_clears() {
    let h = harness().await;
    h.create("orders").await;
    let (status, view) = h.patch("orders", json!({ "limits": limits_json() })).await;
    assert_eq!(status, StatusCode::OK, "{view}");

    // As the console sends cleared fields: each one `null`.
    let cleared = json!({ "limits": {
        "rate_limit": null,
        "daily_token_budget": null,
        "max_request_bytes": 4096,
        "max_response_bytes": null,
    } });
    let (status, view) = h.patch("orders", cleared).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["limits"], json!({ "max_request_bytes": 4096 }));

    // `limits: null` clears every limit.
    let (status, view) = h.patch("orders", json!({ "limits": null })).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["limits"], json!({}));
    let stored = h
        .store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.limits, MockLimits::default());
}
