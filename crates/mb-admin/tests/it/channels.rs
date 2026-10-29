#![cfg(test)]
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::{Extension, Router};
use mb_admin::channels::{
    BrokerView, ChannelActivityView, ChannelControl, ChannelControlError, PublishedMessage,
    SharedChannelControl,
};
use mb_admin::{AdminState, router};
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    ContractStatus, ControlStore, DEFAULT_WORKSPACE, NewContractVersion, NewMock, ResourceSource,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const ORDERS: &str = include_str!("../../../../fixtures/asyncapi/orders-amqp-2.6.yaml");

/// `(workspace, mock, channel, count, context)`.
type Call = (String, String, String, usize, Option<Value>);

#[derive(Default)]
struct FakeControl {
    calls: Mutex<Vec<Call>>,
    fail: Mutex<Option<ChannelControlError>>,
}

#[async_trait]
impl ChannelControl for FakeControl {
    async fn publish(
        &self,
        ws: &str,
        mock: &str,
        channel: &str,
        count: usize,
        context: Option<Value>,
    ) -> Result<Vec<PublishedMessage>, ChannelControlError> {
        self.calls.lock().unwrap().push((
            ws.to_owned(),
            mock.to_owned(),
            channel.to_owned(),
            count,
            context.clone(),
        ));
        if let Some(e) = self.fail.lock().unwrap().clone() {
            return Err(e);
        }
        Ok((0..count)
            .map(|i| PublishedMessage {
                address: "orders.created".into(),
                key: None,
                headers: BTreeMap::from([("x-mockingbird-source".into(), "fallback".into())]),
                payload: json!({ "id": format!("o{i}"), "status": "placed", "total": 1 }),
                source: "fallback".into(),
            })
            .collect())
    }

    fn activity(&self, _ws: &str, _mock: &str) -> BTreeMap<String, ChannelActivityView> {
        BTreeMap::from([(
            "orderCreated".into(),
            ChannelActivityView {
                published: 7,
                received: 0,
                last_published_at: None,
                last_received_at: None,
                last_error: None,
            },
        )])
    }

    fn brokers(&self) -> Vec<BrokerView> {
        vec![
            BrokerView {
                name: "rabbit".into(),
                kind: "amqp".into(),
                url: "amqp://localhost:5672".into(),
                status: "connected".into(),
            },
            BrokerView {
                name: "local".into(),
                kind: "memory".into(),
                url: "memory://".into(),
                status: "connected".into(),
            },
        ]
    }
}

struct Harness {
    app: Router,
    store: Arc<dyn ControlStore>,
    control: Option<Arc<FakeControl>>,
}

struct Resp {
    status: StatusCode,
    content_type: String,
    text: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.text))
    }

    fn ok(&self) -> Value {
        assert_eq!(self.status, StatusCode::OK, "{}", self.text);
        self.json()
    }

    fn problem(&self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "{}", self.text);
        assert_eq!(
            self.content_type, "application/problem+json",
            "{}",
            self.text
        );
        self.json()
    }
}

async fn harness(with_control: bool) -> Harness {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let mut app = router(AdminState::new(store.clone(), state));
    let control = with_control.then(|| Arc::new(FakeControl::default()));
    if let Some(c) = &control {
        let shared: SharedChannelControl = c.clone();
        app = app.layer(Extension(shared));
    }
    Harness {
        app,
        store,
        control,
    }
}

impl Harness {
    async fn send(&self, method: Method, uri: &str, body: Option<Value>) -> Resp {
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
        let content_type = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        Resp {
            status,
            content_type,
            text: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(&self, uri: &str) -> Resp {
        self.send(Method::GET, uri, None).await
    }

    /// Creates the `orders` mock through the API and uploads the AMQP
    /// orders contract, which becomes active.
    async fn orders(&self) {
        let r = self
            .send(
                Method::POST,
                "/api/v1/mocks",
                Some(json!({ "name": "orders" })),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::post("/api/v1/mocks/orders/contracts")
                    .header("content-type", "application/yaml")
                    .body(Body::from(ORDERS))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    fn fake(&self) -> &FakeControl {
        self.control.as_deref().unwrap()
    }
}

fn channel<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no channel {id} in {list}"))
}

#[tokio::test]
async fn list_channels_with_defaults() {
    let h = harness(true).await;
    h.orders().await;
    let list = h.get("/api/v1/mocks/orders/channels").await.ok();
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["cancelOrder", "orderCancelled", "orderCreated"]);

    let created = channel(&list, "orderCreated");
    assert_eq!(created["enabled"], true);
    assert_eq!(created["triggers"], json!(["manual"]));
    assert_eq!(created["settings_source"], "contract");
    assert_eq!(created["direction"], "publish");
    assert_eq!(created["protocol"], "amqp");
    assert_eq!(created["address"], "orders.created");
    assert_eq!(created["activity"]["published"], 7);

    let cancel = channel(&list, "cancelOrder");
    assert_eq!(cancel["triggers"], json!([]));
    assert_eq!(cancel["reply"], "orderCancelled");
    assert_eq!(cancel["direction"], "subscribe");
    assert!(cancel["activity"].is_null(), "{cancel}");
}

#[tokio::test]
async fn list_channels_without_contract_or_control() {
    let h = harness(false).await;
    let r = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(json!({ "name": "orders" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(h.get("/api/v1/mocks/orders/channels").await.ok(), json!([]));
    h.get("/api/v1/mocks/nope/channels")
        .await
        .problem(StatusCode::NOT_FOUND);

    let h = harness(false).await;
    h.orders().await;
    let list = h.get("/api/v1/mocks/orders/channels").await.ok();
    assert!(channel(&list, "orderCreated")["activity"].is_null());
}

#[tokio::test]
async fn get_channel_detail_includes_schema() {
    let h = harness(true).await;
    h.orders().await;
    let d = h
        .get("/api/v1/mocks/orders/channels/orderCreated")
        .await
        .ok();
    assert_eq!(d["id"], "orderCreated");
    assert_eq!(d["settings_source"], "contract");
    assert_eq!(d["message_names"], json!(["OrderCreated"]));
    let schema = d["message_schema"].to_string();
    assert!(schema.contains("\"total\""), "{schema}");
    assert!(d["parameters"].is_object(), "{d}");
    assert!(d.get("key_schema").is_some(), "{d}");

    h.get("/api/v1/mocks/orders/channels/nope")
        .await
        .problem(StatusCode::NOT_FOUND);
    let r = h
        .send(
            Method::POST,
            "/api/v1/mocks",
            Some(json!({ "name": "empty" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    h.get("/api/v1/mocks/empty/channels/orderCreated")
        .await
        .problem(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn put_channel_settings_round_trip() {
    let h = harness(true).await;
    h.orders().await;
    let body = json!({ "enabled": false, "triggers": [{ "cron": { "expr": "*/5 * * * *" } }] });
    let v = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/channels/orderCreated",
            Some(body),
        )
        .await
        .ok();
    assert_eq!(v["enabled"], false);
    assert_eq!(v["settings_source"], "api");
    assert_eq!(
        v["triggers"],
        json!([{ "cron": { "expr": "*/5 * * * *" } }])
    );

    let list = h.get("/api/v1/mocks/orders/channels").await.ok();
    let created = channel(&list, "orderCreated");
    assert_eq!(created["enabled"], false);
    assert_eq!(created["settings_source"], "api");
    assert_eq!(
        created["triggers"],
        json!([{ "cron": { "expr": "*/5 * * * *" } }])
    );

    let mock = h
        .store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    let rows = h.store.list_channel_settings(mock.id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel_id, "orderCreated");
    assert!(!rows[0].enabled);
    assert_eq!(rows[0].source, ResourceSource::Api);
    assert!(rows[0].triggers.is_some());

    // Absent triggers fall back to the contract's; enabled defaults to true.
    let v = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/channels/orderCreated",
            Some(json!({})),
        )
        .await
        .ok();
    assert_eq!(v["enabled"], true);
    assert_eq!(v["triggers"], json!(["manual"]));
    assert_eq!(v["settings_source"], "api");

    // Unknown channel, unknown field, non-JSON body.
    h.send(
        Method::PUT,
        "/api/v1/mocks/orders/channels/nope",
        Some(json!({})),
    )
    .await
    .problem(StatusCode::NOT_FOUND);
    h.send(
        Method::PUT,
        "/api/v1/mocks/orders/channels/orderCreated",
        Some(json!({ "bogus": 1 })),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY);
    h.send(
        Method::PUT,
        "/api/v1/mocks/orders/channels/orderCreated",
        None,
    )
    .await
    .problem(StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn put_invalid_cron_422() {
    let h = harness(true).await;
    h.orders().await;
    let p = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/channels/orderCreated",
            Some(json!({ "triggers": [{ "cron": { "expr": "not a cron" } }] })),
        )
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        p["detail"].as_str().unwrap().contains("invalid cron"),
        "{p}"
    );

    let p = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/channels/orderCreated",
            Some(json!({ "triggers": ["manual", { "nonsense": {} }] })),
        )
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(p["detail"].as_str().unwrap().contains("triggers[1]"), "{p}");
}

#[tokio::test]
async fn put_runaway_schedule_422() {
    let h = harness(true).await;
    h.orders().await;
    let p = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/channels/orderCreated",
            Some(json!({ "triggers": [{ "schedule": { "every_ms": 50 } }] })),
        )
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(p["detail"].as_str().unwrap().contains("100ms"), "{p}");
}

#[tokio::test]
async fn put_non_integer_schedule_422() {
    let h = harness(true).await;
    h.orders().await;
    for every in [json!(1500.5), json!(1500.0), json!("1500"), json!(-1)] {
        let p = h
            .send(
                Method::PUT,
                "/api/v1/mocks/orders/channels/orderCreated",
                Some(json!({ "triggers": [{ "schedule": { "every_ms": every } }] })),
            )
            .await
            .problem(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            p["detail"].as_str().unwrap().contains("triggers[0]"),
            "{every}: {p}"
        );
    }
}

#[tokio::test]
async fn put_triggers_on_subscribe_channel_422() {
    let h = harness(true).await;
    h.orders().await;
    let p = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/channels/cancelOrder",
            Some(json!({ "triggers": ["manual"] })),
        )
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(p["detail"].as_str().unwrap().contains("publishes"), "{p}");
}

#[tokio::test]
async fn put_on_file_mock_409() {
    let h = harness(true).await;
    let mock = h
        .store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: "filed".into(),
                route_prefix: None,
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::File,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: Default::default(),
                behavior: Default::default(),
                hosts: vec![],
                webhooks: Vec::new(),
            },
        )
        .await
        .unwrap();
    let bundle = mb_import::compile(ORDERS.as_bytes()).unwrap();
    h.store
        .add_contract_version(
            mock.id,
            NewContractVersion {
                source: ORDERS.as_bytes().to_vec(),
                bundle,
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await
        .unwrap();
    h.store.activate_contract_version(mock.id, 1).await.unwrap();

    let p = h
        .send(
            Method::PUT,
            "/api/v1/mocks/filed/channels/orderCreated",
            Some(json!({ "enabled": false })),
        )
        .await
        .problem(StatusCode::CONFLICT);
    assert_eq!(p["title"], "Managed by config file", "{p}");
    let p = h
        .send(
            Method::PUT,
            "/api/v1/mocks/filed/event-settings",
            Some(json!({ "broker": null })),
        )
        .await
        .problem(StatusCode::CONFLICT);
    assert_eq!(p["title"], "Managed by config file", "{p}");

    // A file-sourced row on an API mock is read-only too.
    h.orders().await;
    let orders = h
        .store
        .get_mock(DEFAULT_WORKSPACE, "orders")
        .await
        .unwrap()
        .unwrap();
    h.store
        .put_channel_settings(&mb_store::ChannelSettings {
            mock_id: orders.id,
            channel_id: "orderCreated".into(),
            enabled: false,
            triggers: None,
            source: ResourceSource::File,
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let list = h.get("/api/v1/mocks/orders/channels").await.ok();
    assert_eq!(channel(&list, "orderCreated")["settings_source"], "file");
    h.send(
        Method::PUT,
        "/api/v1/mocks/orders/channels/orderCreated",
        Some(json!({})),
    )
    .await
    .problem(StatusCode::CONFLICT);
}

#[tokio::test]
async fn publish_calls_control() {
    let h = harness(true).await;
    h.orders().await;
    let r = h
        .send(
            Method::POST,
            "/api/v1/mocks/orders/channels/orderCreated/publish",
            Some(json!({ "count": 2, "context": { "id": "o1" } })),
        )
        .await
        .ok();
    let messages = r["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["address"], "orders.created");
    assert_eq!(messages[0]["source"], "fallback");
    assert_eq!(
        *h.fake().calls.lock().unwrap(),
        vec![(
            DEFAULT_WORKSPACE.to_string(),
            "orders".to_string(),
            "orderCreated".to_string(),
            2,
            Some(json!({ "id": "o1" }))
        )]
    );

    // Count defaults to 1, context to none.
    let r = h
        .send(
            Method::POST,
            "/api/v1/mocks/orders/channels/orderCreated/publish",
            Some(json!({})),
        )
        .await
        .ok();
    assert_eq!(r["messages"].as_array().unwrap().len(), 1);
    assert_eq!(h.fake().calls.lock().unwrap()[1].3, 1);
    assert_eq!(h.fake().calls.lock().unwrap()[1].4, None);

    // A JSON content type is required; context must be an object.
    h.send(
        Method::POST,
        "/api/v1/mocks/orders/channels/orderCreated/publish",
        None,
    )
    .await
    .problem(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    h.send(
        Method::POST,
        "/api/v1/mocks/orders/channels/orderCreated/publish",
        Some(json!({ "context": [1] })),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY);
    h.send(
        Method::POST,
        "/api/v1/mocks/nope/channels/orderCreated/publish",
        Some(json!({})),
    )
    .await
    .problem(StatusCode::NOT_FOUND);
    assert_eq!(h.fake().calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn publish_count_out_of_range_422() {
    let h = harness(true).await;
    h.orders().await;
    for count in [0, 101] {
        let p = h
            .send(
                Method::POST,
                "/api/v1/mocks/orders/channels/orderCreated/publish",
                Some(json!({ "count": count })),
            )
            .await
            .problem(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(p["detail"].as_str().unwrap().contains("count"), "{p}");
    }
    assert!(h.fake().calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn publish_without_control_503() {
    let h = harness(false).await;
    h.orders().await;
    let p = h
        .send(
            Method::POST,
            "/api/v1/mocks/orders/channels/orderCreated/publish",
            Some(json!({})),
        )
        .await
        .problem(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(p["detail"], "Event runtime not available", "{p}");
}

#[tokio::test]
async fn publish_maps_control_errors() {
    let h = harness(true).await;
    h.orders().await;
    let cases = [
        (
            ChannelControlError::Unknown("no channel".into()),
            StatusCode::NOT_FOUND,
            None,
        ),
        (
            ChannelControlError::Conflict("subscribe channel".into()),
            StatusCode::CONFLICT,
            None,
        ),
        (
            ChannelControlError::NotRunning("not running".into()),
            StatusCode::SERVICE_UNAVAILABLE,
            None,
        ),
        (
            ChannelControlError::Failed("connection refused".into()),
            StatusCode::BAD_GATEWAY,
            Some("Broker publish failed"),
        ),
        (
            ChannelControlError::Failed("generated message is 2000000 bytes".into()),
            StatusCode::BAD_GATEWAY,
            Some("Generated message too large"),
        ),
    ];
    for (err, status, title) in cases {
        *h.fake().fail.lock().unwrap() = Some(err.clone());
        let p = h
            .send(
                Method::POST,
                "/api/v1/mocks/orders/channels/orderCreated/publish",
                Some(json!({})),
            )
            .await
            .problem(status);
        if let Some(title) = title {
            assert_eq!(p["title"], title, "{err:?}: {p}");
        }
    }
}

#[tokio::test]
async fn event_settings_round_trip_and_unknown_broker_422() {
    let h = harness(true).await;
    h.orders().await;
    assert_eq!(
        h.get("/api/v1/mocks/orders/event-settings").await.ok(),
        json!({ "broker": null, "source": "api" })
    );
    let v = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/event-settings",
            Some(json!({ "broker": "rabbit" })),
        )
        .await
        .ok();
    assert_eq!(v, json!({ "broker": "rabbit", "source": "api" }));
    assert_eq!(
        h.get("/api/v1/mocks/orders/event-settings").await.ok(),
        json!({ "broker": "rabbit", "source": "api" })
    );
    let p = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/event-settings",
            Some(json!({ "broker": "nope" })),
        )
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(p["detail"].as_str().unwrap().contains("nope"), "{p}");
    h.send(
        Method::PUT,
        "/api/v1/mocks/orders/event-settings",
        Some(json!({ "broker": null, "extra": 1 })),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY);
    let v = h
        .send(
            Method::PUT,
            "/api/v1/mocks/orders/event-settings",
            Some(json!({ "broker": null })),
        )
        .await
        .ok();
    assert_eq!(v, json!({ "broker": null, "source": "api" }));

    // Without a control any broker name is accepted.
    let h = harness(false).await;
    h.orders().await;
    h.send(
        Method::PUT,
        "/api/v1/mocks/orders/event-settings",
        Some(json!({ "broker": "anything" })),
    )
    .await
    .ok();
}

#[tokio::test]
async fn brokers_listed() {
    let h = harness(true).await;
    let list = h.get("/api/v1/brokers").await.ok();
    assert_eq!(
        list,
        json!([
            { "name": "local", "kind": "memory", "url": "memory://", "status": "connected" },
            { "name": "rabbit", "kind": "amqp", "url": "amqp://localhost:5672", "status": "connected" },
        ])
    );
    let h = harness(false).await;
    assert_eq!(h.get("/api/v1/brokers").await.ok(), json!([]));
}
