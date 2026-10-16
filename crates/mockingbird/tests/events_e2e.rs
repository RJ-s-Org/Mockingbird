//! End to end: AsyncAPI mocks served by `build_app` with configured brokers.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mb_mcm::ChannelBinding;
use mb_protocols::events::InMessage;
use mockingbird::config::{App, ServeArgs, build_app};
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/asyncapi")
        .join(name)
}

/// A running app: its data and admin base URLs.
struct Running {
    app: App,
    data: String,
    admin: String,
    _dir: tempfile::TempDir,
}

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// `build_app` (offline) on a config file with `brokers` and `mocks`
/// (name → fixture file) sections.
async fn start(brokers: &str, mocks: &[(&str, &str)]) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let mut yaml = format!("brokers:\n{brokers}mocks:\n");
    for (name, file) in mocks {
        yaml.push_str(&format!(
            "  {name}:\n    contract: {}\n",
            fixture(file).display()
        ));
    }
    let path = dir.path().join("mockingbird.yaml");
    std::fs::write(&path, yaml).unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    let app = build_app(&args, None).await.unwrap();
    let data = serve(app.data.clone()).await;
    let admin = serve(app.admin.clone()).await;
    Running {
        app,
        data,
        admin,
        _dir: dir,
    }
}

const MEMORY: &str = "  mem:\n    kind: memory\n";

#[tokio::test]
async fn admin_publish_reaches_memory_broker() {
    let r = start(MEMORY, &[("orders", "orders-amqp-2.6.yaml")]).await;
    let http = reqwest::Client::new();
    let api = format!("{}/api/v1/mocks/orders", r.admin);

    let resp = http
        .put(format!("{api}/event-settings"))
        .json(&json!({"broker": "mem"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    let resp = http
        .post(format!("{api}/channels/orderCreated/publish"))
        .json(&json!({"count": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["messages"].as_array().unwrap().len(), 2, "{body}");

    let broker = r.app.brokers.memory("mem").unwrap();
    assert_eq!(broker.published().len(), 2);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let entries: Value = http
            .get(format!("{api}/requests?kind=event"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let publishes = entries
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["method"] == "PUBLISH")
            .count();
        if publishes == 2 {
            break;
        }
        assert!(Instant::now() < deadline, "{entries}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn consumed_message_replied_through_app() {
    let r = start(MEMORY, &[("orders", "orders-amqp-2.6.yaml")]).await;
    let bundle =
        mb_import::compile(&std::fs::read(fixture("orders-amqp-2.6.yaml")).unwrap()).unwrap();
    let binding = bundle.channel("cancelOrder").unwrap().binding.clone();
    let broker = r.app.brokers.memory("mem").unwrap();
    // Let the consumer subscribe.
    tokio::time::sleep(Duration::from_millis(200)).await;

    broker.inject(
        &binding,
        InMessage {
            address: binding.address().to_string(),
            key: None,
            headers: Vec::new(),
            payload: br#"{"orderId":"o1"}"#.to_vec(),
        },
    );
    let published = broker.wait_published(1, Duration::from_secs(2)).await;
    let reply = published.iter().find(|(b, _)| match b {
        ChannelBinding::Amqp { routing_key, .. } => routing_key == "orders.cancelled",
        _ => false,
    });
    assert!(reply.is_some(), "{published:?}");
}

#[tokio::test]
async fn ws_channel_served_at_mock_prefix() {
    let r = start(MEMORY, &[("chat", "chat-ws-2.6.yaml")]).await;
    let ws_url = format!("{}/chat/chat", r.data.replacen("http://", "ws://", 1));
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();

    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/v1/mocks/chat/channels/chatMessage/publish",
            r.admin
        ))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let frame = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = frame.into_text().unwrap();
    let message: Value = serde_json::from_str(&text).unwrap();
    assert!(message.is_object(), "{message}");
}

#[cfg(feature = "mqtt")]
#[tokio::test]
async fn unreachable_broker_degrades_readyz_but_mounts() {
    let r = start(
        "  down:\n    kind: mqtt\n    url: mqtt://127.0.0.1:1\n",
        &[("sensors", "sensors-mqtt-3.0.yaml")],
    )
    .await;
    let http = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let resp = http
            .get(format!("{}/readyz", r.admin))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        let degraded = body["status"] == "degraded"
            && body["reasons"].as_array().is_some_and(|reasons| {
                reasons.iter().any(|reason| {
                    reason
                        .as_str()
                        .is_some_and(|s| s.starts_with("mock sensors: broker down:"))
                })
            });
        if degraded {
            break;
        }
        assert!(Instant::now() < deadline, "{body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let resp = http
        .get(format!("{}/api/v1/mocks/sensors/channels", r.admin))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn shutdown_flushes_brokers() {
    let r = start(MEMORY, &[("orders", "orders-amqp-2.6.yaml")]).await;
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(6), r.app.shutdown())
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(6));
}
