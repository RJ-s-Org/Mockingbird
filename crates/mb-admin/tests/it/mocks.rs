//! Chaos and limits on the mock resource.
#![cfg(test)]

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
                hosts: vec![],
                behavior: Default::default(),
                webhooks: Vec::new(),
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

#[tokio::test]
async fn hosts_let_mocks_share_a_prefix() {
    let h = harness().await;
    let create =
        |name: &str, hosts: Value| json!({"name": name, "route_prefix": "/", "hosts": hosts});
    let (s, body) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(create("a", json!(["a.test"]))),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(body["hosts"], json!(["a.test"]));
    let (s, _) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(create("b", json!(["b.test"]))),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, body) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(create("c", json!(["c.test", "b.test"]))),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(
        body["detail"].as_str().unwrap().contains("\"b.test\""),
        "{body}"
    );
    assert!(
        body["detail"].as_str().unwrap().contains("default/b"),
        "{body}"
    );
    let (s, body) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(create("d", json!(["D.test:80"]))),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn patch_hosts_replaces_and_checks_collisions() {
    let h = harness().await;
    for (name, host) in [("a", "a.test"), ("b", "b.test")] {
        let body = json!({"name": name, "route_prefix": "/", "hosts": [host]});
        assert_eq!(
            h.send(Method::POST, "/api/v1/mocks", Some(body)).await.0,
            StatusCode::CREATED
        );
    }
    let (s, _) = h.patch("b", json!({"hosts": ["a.test"]})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, body) = h.patch("b", json!({"hosts": ["c.test"]})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["hosts"], json!(["c.test"]));
    let (s, body) = h.patch("b", json!({"hosts": []})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["hosts"], json!([]));
}

#[tokio::test]
async fn patch_consistency_settings() {
    let h = harness().await;
    let (status, created) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(json!({"name": "orders"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        (
            created["consistency"].as_str(),
            created["unknown_ids"].as_str()
        ),
        (Some("on"), Some("generate"))
    );
    assert!(created.get("default_scenario").is_none());
    let (status, body) = h
        .send(
            Method::PATCH,
            "/api/v1/mocks/orders",
            Some(json!({"default_scenario": "calm"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    h.send(
        Method::PUT,
        "/api/v1/mocks/orders/scenarios/calm",
        Some(json!({})),
    )
    .await;
    let (status, view) = h
        .send(
            Method::PATCH,
            "/api/v1/mocks/orders",
            Some(json!({"consistency": "off", "unknown_ids": "404", "default_scenario": "calm"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        (
            view["consistency"].as_str(),
            view["unknown_ids"].as_str(),
            view["default_scenario"].as_str()
        ),
        (Some("off"), Some("404"), Some("calm"))
    );
    let (_, view) = h
        .send(
            Method::PATCH,
            "/api/v1/mocks/orders",
            Some(json!({"default_scenario": null})),
        )
        .await;
    assert!(view.get("default_scenario").is_none());
}

#[tokio::test]
async fn faker_seed_round_trips_through_the_api() {
    let h = harness().await;
    let stored_seed = async |name: &str| {
        h.store
            .get_mock(DEFAULT_WORKSPACE, name)
            .await
            .unwrap()
            .unwrap()
            .behavior
            .faker_seed
    };

    // Unset by default: absent from the view.
    h.create("plain").await;
    let (_, view) = h.send(Method::GET, "/api/v1/mocks/plain", None).await;
    assert!(view.get("faker_seed").is_none(), "{view}");
    assert_eq!(stored_seed("plain").await, None);

    // Set on create.
    let (status, created) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(json!({ "name": "orders", "faker_seed": 42 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["faker_seed"], 42, "{created}");
    assert_eq!(stored_seed("orders").await, Some(42));

    // Changed by a patch (the whole u64 range), kept by a patch of another
    // field.
    let (status, view) = h.patch("orders", json!({ "faker_seed": u64::MAX })).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["faker_seed"], u64::MAX, "{view}");
    let (status, view) = h.patch("orders", json!({ "persona": "Terse" })).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["faker_seed"], u64::MAX, "{view}");
    assert_eq!(stored_seed("orders").await, Some(u64::MAX));

    // Not a u64: a 422 problem, and the stored seed is untouched.
    for bad in [json!(-1), json!(1.5), json!("7"), json!(1.8e19)] {
        let (status, problem) = h.patch("orders", json!({ "faker_seed": bad })).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {problem}");
        assert_eq!(problem["title"], "Invalid request body", "{problem}");
    }
    let (status, problem) = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(json!({ "name": "negative", "faker_seed": -3 })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(stored_seed("orders").await, Some(u64::MAX));

    // `null` clears it.
    let (status, view) = h.patch("orders", json!({ "faker_seed": null })).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert!(view.get("faker_seed").is_none(), "{view}");
    assert_eq!(stored_seed("orders").await, None);
}

#[tokio::test]
async fn webhooks_round_trip_through_the_api() {
    let h = harness().await;
    let (status, body) = h
        .send(
            Method::POST,
            "/api/v1/workspaces/default/mocks",
            Some(json!({
                "name": "shop",
                "webhooks": [{"url": "https://hooks.test/stripe", "events": ["*"],
                    "secret": "env:SHOP_WEBHOOK_SECRET", "signing": "stripe", "delay_ms": 10}]
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["webhooks"][0]["signing"], "stripe");
    assert_eq!(body["webhooks"][0]["secret"], "env:SHOP_WEBHOOK_SECRET");
    let stored = h
        .store
        .get_mock(DEFAULT_WORKSPACE, "shop")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.webhooks[0].delay_ms, 10);
    let (status, body) = h
        .send(
            Method::PATCH,
            "/api/v1/workspaces/default/mocks/shop",
            Some(json!({"webhooks": [{"url": "http://localhost:4242/hook"}]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["webhooks"][0]["events"], json!(["*"]));
    let (status, body) = h.patch("shop", json!({"webhooks": []})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("webhooks").is_none(), "{body}");
}

#[tokio::test]
async fn webhook_secret_refs_are_restricted() {
    let h = harness().await;
    for (secret, title) in [
        ("env:OPENROUTER_API_KEY", "Secret reference not allowed"),
        ("whsec_pasted_value", "Invalid webhooks"),
    ] {
        let (status, body) = h
            .send(
                Method::POST,
                "/api/v1/workspaces/default/mocks",
                Some(json!({"name": "shop",
                    "webhooks": [{"url": "https://hooks.test/h", "secret": secret}]})),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["title"], title, "{body}");
        assert!(!body.to_string().contains("whsec_pasted_value"), "{body}");
    }
    h.create("shop").await;
    let (status, body) = h
        .patch(
            "shop",
            json!({"webhooks": [{"url": "https://hooks.test/h", "secret": "env:OPENROUTER_API_KEY"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["title"], "Secret reference not allowed", "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("webhooks[0].secret"),
        "{body}"
    );
    let (status, body) = h
        .patch("shop", json!({"webhooks": [{"url": "nope"}]}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["title"], "Invalid webhooks", "{body}");
    let (status, _) = h
        .patch(
            "shop",
            json!({"webhooks": [{"url": "https://hooks.test/h", "secret": "env:MB_WEBHOOK_SHOP"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn webhook_url_credentials_never_leave_the_store() {
    let h = harness().await;
    // Userinfo is refused; the refusal does not echo it.
    let (status, body) = h
        .send(
            Method::POST,
            "/api/v1/workspaces/default/mocks",
            Some(json!({"name": "shop",
                "webhooks": [{"url": "https://user:pw-secret@hooks.test/in"}]})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["title"], "Invalid webhooks");
    assert!(!body.to_string().contains("pw-secret"), "{body}");
    // A query is kept in the store but redacted in views and audit events.
    h.create("shop").await;
    let (status, body) = h
        .patch(
            "shop",
            json!({"webhooks": [{"url": "https://hooks.test/in?token=abc123"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["webhooks"][0]["url"],
        "https://hooks.test/in?<redacted>"
    );
    let (_, got) = h
        .send(Method::GET, "/api/v1/workspaces/default/mocks/shop", None)
        .await;
    assert!(!got.to_string().contains("abc123"), "{got}");
    let events = h
        .store
        .list_audit(&AuditFilter {
            action: Some("mock.update".into()),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert!(!serde_json::to_string(&events).unwrap().contains("abc123"));
    let stored = |h: &Harness| {
        let store = h.store.clone();
        async move {
            store
                .get_mock(DEFAULT_WORKSPACE, "shop")
                .await
                .unwrap()
                .unwrap()
                .webhooks
        }
    };
    assert_eq!(
        stored(&h).await[0].url,
        "https://hooks.test/in?token=abc123"
    );
    // Writing back what a GET returned keeps the stored URL.
    let (status, body) = h
        .patch(
            "shop",
            json!({"webhooks": [{"url": "https://hooks.test/in?<redacted>", "delay_ms": 5}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let targets = stored(&h).await;
    assert_eq!(targets[0].url, "https://hooks.test/in?token=abc123");
    assert_eq!(targets[0].delay_ms, 5);
}
