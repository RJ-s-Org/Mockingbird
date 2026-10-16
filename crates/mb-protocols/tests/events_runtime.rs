#![cfg(feature = "events")]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mb_engine::{Engine, EngineConfig, Source};
use mb_ext::EnvFileSecrets;
use mb_mcm::{Bundle, ChannelBinding, Direction, Reply};
use mb_protocols::events::{
    BrokerConfig, BrokerKind, Brokers, ChannelMode, ChannelRuntime, ChannelSetup, EventDeps,
    EventSetup, InMessage, MemoryBroker, PublishError, PublishFailure, WS_PUBLISH_ON_DATA_NODES,
};
use mb_protocols::{MockRuntime, RequestKind, RequestRecord, RequestRecorder};
use mb_state::{MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};

#[derive(Default)]
struct Capture(Mutex<Vec<RequestRecord>>);

impl RequestRecorder for Capture {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

impl Capture {
    fn records(&self) -> Vec<RequestRecord> {
        self.0.lock().unwrap().clone()
    }

    async fn wait(&self, n: usize, timeout: Duration) -> Vec<RequestRecord> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let records = self.records();
            if records.len() >= n || tokio::time::Instant::now() >= deadline {
                return records;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn orders() -> Bundle {
    mb_import::compile(include_bytes!(
        "../../../fixtures/asyncapi/orders-amqp-2.6.yaml"
    ))
    .unwrap()
}

fn memory_brokers() -> Arc<Brokers> {
    memory_brokers_with_prefix("mockingbird")
}

fn memory_brokers_with_prefix(client_id_prefix: &str) -> Arc<Brokers> {
    Brokers::new(
        vec![BrokerConfig {
            name: "mem".into(),
            kind: BrokerKind::Memory,
            url: String::new(),
            tls: false,
            ca_file: None,
            sasl: None,
            client_id_prefix: client_id_prefix.into(),
        }],
        Arc::new(EnvFileSecrets),
    )
}

struct Harness {
    channels: Arc<ChannelRuntime>,
    records: Arc<Capture>,
    broker: Arc<MemoryBroker>,
    state: Arc<dyn StateStore>,
}

fn runtime_with_brokers(bundle: Bundle, setup: EventSetup, brokers: Arc<Brokers>) -> Harness {
    let records = Arc::new(Capture::default());
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "orders",
        Arc::new(bundle),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&state),
    );
    rt.recorder = Some(Arc::clone(&records) as Arc<dyn RequestRecorder>);
    let broker = brokers.memory("mem").unwrap_or_default();
    let deps = EventDeps {
        brokers,
        ..EventDeps::default()
    };
    let channels = ChannelRuntime::start(Arc::new(rt), deps, setup);
    Harness {
        channels,
        records,
        broker,
        state,
    }
}

fn runtime_with(bundle: Bundle, setup: EventSetup) -> Harness {
    runtime_with_brokers(bundle, setup, memory_brokers())
}

/// Let the consumer tasks subscribe.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

fn cancel_binding(bundle: &Bundle) -> ChannelBinding {
    bundle.channel("cancelOrder").unwrap().binding.clone()
}

fn incoming(payload: &[u8], headers: &[(&str, &str)]) -> InMessage {
    InMessage {
        address: "order-cancellations".into(),
        key: None,
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        payload: payload.to_vec(),
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn manual_publish_returns_and_publishes() {
    let h = runtime_with(orders(), EventSetup::default());
    assert_eq!(h.channels.broker_name().as_deref(), Some("mem"));
    let published = h
        .channels
        .publish_manual("orderCreated", 2, None)
        .await
        .unwrap();
    assert_eq!(published.len(), 2);
    for p in &published {
        assert_eq!(p.channel, "orderCreated");
        assert_eq!(p.address, "orders.created");
        assert_eq!(p.source, Source::Fallback);
        assert_eq!(p.headers["x-mockingbird-mock"], "orders");
        assert_eq!(p.headers["x-mockingbird-hop"], "0");
        assert_eq!(p.headers["x-mockingbird-source"], "fallback");
        assert!(p.payload.get("id").is_some());
    }

    let sent = h.broker.published();
    assert_eq!(sent.len(), 2);
    for (binding, msg) in &sent {
        match binding {
            ChannelBinding::Amqp { routing_key, .. } => assert_eq!(routing_key, "orders.created"),
            other => panic!("unexpected binding {other:?}"),
        }
        assert_eq!(header(&msg.headers, "x-mockingbird-mock"), Some("orders"));
        assert_eq!(header(&msg.headers, "x-mockingbird-hop"), Some("0"));
        assert_eq!(
            header(&msg.headers, "x-mockingbird-source"),
            Some("fallback")
        );
    }
    let first: Value = serde_json::from_slice(&sent[0].1.payload).unwrap();
    assert_eq!(first, published[0].payload);

    let records = h.records.records();
    assert_eq!(records.len(), 2);
    for r in &records {
        assert_eq!(r.kind, RequestKind::Event);
        assert_eq!(r.method, "PUBLISH");
        assert_eq!(r.path, "orders.created");
        assert_eq!(r.status, 200);
        assert_eq!(r.operation_id.as_deref(), Some("orderCreated"));
        assert_eq!(r.session, "default");
        assert_eq!(r.source.as_deref(), Some("fallback"));
    }
    let activity = h.channels.activity();
    assert_eq!(activity["orderCreated"].published, 2);
    assert!(activity["orderCreated"].last_published_at.is_some());
}

#[tokio::test]
async fn manual_publish_errors() {
    let mut channels = HashMap::new();
    channels.insert(
        "orderCancelled".to_string(),
        ChannelSetup {
            enabled: false,
            triggers: Vec::new(),
        },
    );
    let h = runtime_with(
        orders(),
        EventSetup {
            channels,
            ..EventSetup::default()
        },
    );
    assert_eq!(
        h.channels.publish_manual("nope", 1, None).await,
        Err(PublishFailure::from(PublishError::UnknownChannel(
            "nope".into()
        )))
    );
    assert_eq!(
        h.channels.publish_manual("cancelOrder", 1, None).await,
        Err(PublishFailure::from(PublishError::NotPublishable(
            "cancelOrder".into()
        )))
    );
    assert_eq!(
        h.channels.publish_manual("orderCancelled", 1, None).await,
        Err(PublishFailure::from(PublishError::Disabled(
            "orderCancelled".into()
        )))
    );
    assert!(h.broker.published().is_empty());
}

#[tokio::test]
async fn no_broker_reason_in_health() {
    let h = runtime_with_brokers(orders(), EventSetup::default(), Brokers::empty());
    assert_eq!(h.channels.broker_name(), None);
    let health = h.channels.health();
    assert!(
        health
            .iter()
            .any(|r| r.contains("no broker configured for amqp channels")),
        "{health:?}"
    );
    match h
        .channels
        .publish_manual("orderCreated", 1, None)
        .await
        .map_err(|f| f.error)
    {
        Err(PublishError::NoBroker(reason)) => {
            assert!(reason.contains("no broker configured"), "{reason}")
        }
        other => panic!("expected NoBroker, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_named_broker_in_health() {
    let h = runtime_with(
        orders(),
        EventSetup {
            broker: Some("ghost".into()),
            ..EventSetup::default()
        },
    );
    assert!(
        h.channels
            .health()
            .contains(&"broker \"ghost\" is not configured".to_string()),
        "{:?}",
        h.channels.health()
    );
}

#[tokio::test]
async fn consumed_message_validated_recorded_and_replied() {
    let bundle = orders();
    let binding = cancel_binding(&bundle);
    let h = runtime_with(bundle, EventSetup::default());
    settle().await;

    h.broker.inject(
        &binding,
        incoming(
            br#"{"orderId":"o1","reason":"x"}"#,
            &[("correlation_id", "c1"), ("other", "y")],
        ),
    );
    let replies = h.broker.wait_published(1, Duration::from_secs(5)).await;
    assert_eq!(replies.len(), 1);
    let (binding, msg) = &replies[0];
    match binding {
        ChannelBinding::Amqp { routing_key, .. } => assert_eq!(routing_key, "orders.cancelled"),
        other => panic!("unexpected binding {other:?}"),
    }
    assert_eq!(header(&msg.headers, "correlation_id"), Some("c1"));
    assert_eq!(header(&msg.headers, "other"), None);
    assert_eq!(header(&msg.headers, "x-mockingbird-hop"), Some("1"));

    let records = h.records.wait(2, Duration::from_secs(5)).await;
    let received = records.iter().find(|r| r.method == "RECEIVE").unwrap();
    assert_eq!(received.status, 200);
    assert_eq!(received.kind, RequestKind::Event);
    assert_eq!(received.path, "order-cancellations");
    assert_eq!(received.operation_id.as_deref(), Some("cancelOrder"));
    assert_eq!(
        received.request["body"],
        json!({"orderId":"o1","reason":"x"})
    );
    assert_eq!(received.request["headers"]["correlation_id"], "c1");
    assert_eq!(received.response.as_ref().unwrap()["valid"], true);
    assert!(
        records
            .iter()
            .any(|r| r.method == "PUBLISH" && r.operation_id.as_deref() == Some("orderCancelled"))
    );

    let key = SessionKey {
        workspace: "default".into(),
        mock: "orders".into(),
        session: "default".into(),
    };
    let stored = h.state.get(&key, "cancellations", "o1").await.unwrap();
    assert_eq!(stored.unwrap().data["reason"], "x");
    assert_eq!(h.channels.activity()["cancelOrder"].received, 1);
}

/// The consumer group is `<client_id_prefix>-<mock>` (`mockingbird-<mock>`
/// without a prefix); outside `default` the workspace is part of it.
#[tokio::test]
async fn consumer_group_uses_the_client_id_prefix() {
    let expect = |prefix: &str, workspace: &str, group: &str| {
        assert_eq!(
            mb_protocols::events::consumer_group(prefix, workspace, "orders"),
            group
        );
    };
    expect("acme", "default", "acme-orders");
    expect("mockingbird", "default", "mockingbird-orders");
    expect("", "default", "mockingbird-orders");
    expect("acme", "team-a", "acme-team-a.orders");

    let h = runtime_with_brokers(
        orders(),
        EventSetup::default(),
        memory_brokers_with_prefix("acme"),
    );
    settle().await;
    let groups = h.broker.subscribed_groups();
    assert!(!groups.is_empty());
    assert!(groups.iter().all(|g| g == "acme-orders"), "{groups:?}");
}

/// Consumed messages are recorded without credential-like headers, as on
/// the HTTP path; the reply still carries the correlation headers.
#[tokio::test]
async fn consumed_message_headers_are_redacted_in_the_record() {
    let bundle = orders();
    let binding = cancel_binding(&bundle);
    let h = runtime_with(bundle, EventSetup::default());
    settle().await;

    h.broker.inject(
        &binding,
        incoming(
            br#"{"orderId":"o9","reason":"x"}"#,
            &[
                ("correlation_id", "c9"),
                ("Authorization", "Bearer secret-token"),
                ("x-api-key", "mb_key"),
                ("apiKey", "k2"),
                ("cookie", "sid=1"),
                ("x-session-token", "t"),
                ("db_password", "p"),
                ("x-trace", "keep-me"),
            ],
        ),
    );
    h.broker.wait_published(1, Duration::from_secs(5)).await;
    let records = h.records.wait(2, Duration::from_secs(5)).await;
    let received = records.iter().find(|r| r.method == "RECEIVE").unwrap();
    let headers = received.request["headers"].as_object().unwrap();
    assert_eq!(headers["correlation_id"], "c9");
    assert_eq!(headers["x-trace"], "keep-me");
    for secret in [
        "Authorization",
        "x-api-key",
        "apiKey",
        "cookie",
        "x-session-token",
        "db_password",
    ] {
        assert!(!headers.contains_key(secret), "{secret} in {headers:?}");
    }
    let text = received.request.to_string();
    for value in ["secret-token", "mb_key", "k2", "sid=1"] {
        assert!(!text.contains(value), "{value} in {text}");
    }
}

#[tokio::test]
async fn poison_message_recorded_not_crash() {
    let bundle = orders();
    let binding = cancel_binding(&bundle);
    let h = runtime_with(bundle, EventSetup::default());
    settle().await;

    h.broker.inject(&binding, incoming(b"not json", &[]));
    h.broker
        .inject(&binding, incoming(br#"{"orderId":"o2"}"#, &[]));
    let replies = h.broker.wait_published(1, Duration::from_secs(5)).await;
    assert_eq!(replies.len(), 1);

    let records = h.records.wait(3, Duration::from_secs(5)).await;
    let received: Vec<&RequestRecord> = records.iter().filter(|r| r.method == "RECEIVE").collect();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].status, 400);
    assert_eq!(received[0].request["body"], json!({"raw": "not json"}));
    assert_eq!(received[1].status, 200);
}

#[tokio::test]
async fn schema_invalid_message_422_no_reply() {
    let bundle = orders();
    let binding = cancel_binding(&bundle);
    let h = runtime_with(bundle, EventSetup::default());
    settle().await;

    h.broker
        .inject(&binding, incoming(br#"{"reason":"no id"}"#, &[]));
    let records = h.records.wait(1, Duration::from_secs(5)).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, 422);
    let response = records[0].response.as_ref().unwrap();
    assert_eq!(response["valid"], false);
    assert!(!response["errors"].as_array().unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.broker.published().is_empty());
    assert_eq!(h.records.records().len(), 1);
}

#[tokio::test]
async fn oversized_consumed_message_recorded_413() {
    let bundle = orders();
    let binding = cancel_binding(&bundle);
    let h = runtime_with(bundle, EventSetup::default());
    settle().await;

    let big = format!(
        r#"{{"orderId":"o3","reason":"{}"}}"#,
        "x".repeat(1024 * 1024)
    );
    let n = big.len();
    h.broker.inject(&binding, incoming(big.as_bytes(), &[]));
    let records = h.records.wait(1, Duration::from_secs(5)).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, 413);
    assert_eq!(
        records[0].request["body"],
        json!({"truncated": true, "bytes": n})
    );
    assert_eq!(records[0].request_body_bytes, Some(n));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.broker.published().is_empty());
}

#[tokio::test]
async fn oversized_generated_message_not_published() {
    let mut bundle = orders();
    let mut huge = bundle.channel("orderCreated").unwrap().clone();
    huge.id = "huge".into();
    huge.message_schema = json!({"type": "string", "minLength": 1_153_434});
    bundle.channels.push(huge);
    let h = runtime_with(bundle, EventSetup::default());

    match h
        .channels
        .publish_manual("huge", 1, None)
        .await
        .map_err(|f| f.error)
    {
        Err(PublishError::TooLarge(n)) => assert!(n > 1024 * 1024),
        other => panic!("expected TooLarge, got {other:?}"),
    }
    assert!(h.broker.published().is_empty());
    let records = h.records.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, 413);
    assert_eq!(records[0].method, "PUBLISH");
}

#[tokio::test(start_paused = true)]
async fn publish_failure_retries_then_records_503() {
    let h = runtime_with(orders(), EventSetup::default());

    h.broker.fail_publishes(4);
    let started = tokio::time::Instant::now();
    match h.channels.publish_manual("orderCreated", 1, None).await {
        Err(PublishFailure {
            error: PublishError::Broker(e),
            published,
        }) => {
            assert!(e.contains("injected failure"), "{e}");
            assert!(published.is_empty());
        }
        other => panic!("expected Broker, got {other:?}"),
    }
    // 200 + 400 + 800 ms between four attempts.
    assert!(started.elapsed() >= Duration::from_millis(1400));
    assert!(h.broker.published().is_empty());
    let records = h.records.records();
    assert_eq!(records.last().unwrap().status, 503);
    assert!(h.channels.activity()["orderCreated"].last_error.is_some());

    h.broker.fail_publishes(2);
    let started = tokio::time::Instant::now();
    let published = h
        .channels
        .publish_manual("orderCreated", 1, None)
        .await
        .unwrap();
    assert_eq!(published.len(), 1);
    // Two failures, then success on the third attempt: 200 + 400 ms.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(600) && elapsed < Duration::from_millis(1400),
        "{elapsed:?}"
    );
    assert_eq!(h.broker.published().len(), 1);
    assert_eq!(h.records.records().last().unwrap().status, 200);
}

#[tokio::test(start_paused = true)]
async fn manual_publish_failing_partway_returns_what_was_published() {
    let h = runtime_with(orders(), EventSetup::default());

    // Three publishes go out, then the broker fails every attempt.
    h.broker.fail_publishes_after(3, usize::MAX);
    let failure = h
        .channels
        .publish_manual("orderCreated", 10, None)
        .await
        .unwrap_err();
    match &failure.error {
        PublishError::Broker(e) => assert!(e.contains("injected failure"), "{e}"),
        other => panic!("expected Broker, got {other:?}"),
    }
    assert_eq!(failure.published.len(), 3, "{failure:?}");
    assert_eq!(h.broker.published().len(), 3);
    for p in &failure.published {
        assert_eq!(p.channel, "orderCreated");
        assert_eq!(p.address, "orders.created");
    }
    assert_eq!(h.channels.activity()["orderCreated"].published, 3);
}

#[tokio::test]
async fn reply_hop_limit_stops_loop() {
    let mut bundle = orders();
    let consumed = bundle.channel("cancelOrder").unwrap().clone();
    // A publish channel that writes back to the queue the mock consumes.
    let mut echo = bundle.channel("orderCancelled").unwrap().clone();
    echo.id = "echo".into();
    echo.binding = consumed.binding.clone();
    echo.direction = Direction::Publish;
    echo.message_schema = consumed.message_schema.clone();
    bundle.channels.push(echo);
    for c in &mut bundle.channels {
        if c.id == "cancelOrder" {
            c.reply = Some(Reply {
                channel: "echo".into(),
                address_header: None,
            });
        }
    }
    let binding = consumed.binding.clone();
    let h = runtime_with(bundle, EventSetup::default());
    settle().await;

    h.broker
        .inject(&binding, incoming(br#"{"orderId":"o9"}"#, &[]));
    let replies = h.broker.wait_published(4, Duration::from_millis(500)).await;
    assert_eq!(replies.len(), 3);
    let hops: Vec<&str> = replies
        .iter()
        .map(|(_, m)| header(&m.headers, "x-mockingbird-hop").unwrap())
        .collect();
    assert_eq!(hops, vec!["1", "2", "3"]);
    let records = h.records.wait(7, Duration::from_millis(500)).await;
    let last_receive = records.iter().rfind(|r| r.method == "RECEIVE").unwrap();
    assert_eq!(
        last_receive.response.as_ref().unwrap()["reply"],
        "skipped: hop limit"
    );
}

#[tokio::test]
async fn dropping_runtime_stops_consumers() {
    let bundle = orders();
    let binding = cancel_binding(&bundle);
    let h = runtime_with(bundle, EventSetup::default());
    settle().await;

    drop(h.channels);
    tokio::task::yield_now().await;
    h.broker
        .inject(&binding, incoming(br#"{"orderId":"o5"}"#, &[]));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.records.records().is_empty());
}

#[tokio::test]
async fn publish_only_runtime_starts_nothing_but_publishes_to_brokers() {
    let bundle = orders();
    let binding = cancel_binding(&bundle);
    let h = runtime_with(
        bundle,
        EventSetup {
            mode: ChannelMode::PublishOnly,
            ..EventSetup::default()
        },
    );
    settle().await;
    assert!(h.broker.subscribed_groups().is_empty());
    h.broker
        .inject(&binding, incoming(br#"{"orderId":"o1"}"#, &[]));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.broker.published().is_empty());

    let published = h
        .channels
        .publish_manual("orderCancelled", 1, None)
        .await
        .unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(h.broker.published().len(), 1);
}

#[tokio::test]
async fn publish_only_runtime_refuses_websocket_publish() {
    let chat = mb_import::compile(include_bytes!(
        "../../../fixtures/asyncapi/chat-ws-2.6.yaml"
    ))
    .unwrap();
    let h = runtime_with(
        chat,
        EventSetup {
            mode: ChannelMode::PublishOnly,
            ..EventSetup::default()
        },
    );
    assert!(h.channels.ws_router().is_none());
    assert_eq!(
        h.channels.publish_manual("chatMessage", 1, None).await,
        Err(PublishFailure::from(PublishError::NotPublishable(
            WS_PUBLISH_ON_DATA_NODES.into()
        )))
    );
}
