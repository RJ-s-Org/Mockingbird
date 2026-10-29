#![cfg(test)]
#![cfg(feature = "events")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use mb_engine::{Engine, EngineConfig};
use mb_ext::{ExtError, SecretStore};
use mb_mcm::ChannelBinding;
use mb_protocols::events::{
    BrokerClient, BrokerConfig, BrokerError, BrokerKind, Brokers, InMessage, MemoryBroker,
    OutMessage, SaslConfig, param_values,
};
use serde_json::json;

fn mqtt(topic: &str) -> ChannelBinding {
    ChannelBinding::Mqtt {
        topic: topic.into(),
        qos: 1,
        retain: false,
    }
}

fn out(payload: &str) -> OutMessage {
    OutMessage {
        key: None,
        headers: vec![("x-mockingbird-hop".into(), "0".into())],
        payload: payload.as_bytes().to_vec(),
        content_type: "application/json".into(),
    }
}

fn config(name: &str, kind: BrokerKind, url: &str) -> BrokerConfig {
    BrokerConfig {
        name: name.into(),
        kind,
        url: url.into(),
        tls: false,
        ca_file: None,
        sasl: None,
        client_id_prefix: "mockingbird".into(),
    }
}

#[derive(Default)]
struct RecordingSecrets {
    requested: Mutex<Vec<String>>,
}

#[async_trait]
impl SecretStore for RecordingSecrets {
    async fn resolve(&self, reference: &str) -> Result<String, ExtError> {
        self.requested.lock().unwrap().push(reference.to_string());
        Ok("hunter2".into())
    }
}

fn secrets() -> Arc<dyn SecretStore> {
    Arc::new(RecordingSecrets::default())
}

#[tokio::test]
async fn memory_publish_delivers_to_matching_subscriber() {
    let broker = MemoryBroker::new();
    let mut matching = broker
        .subscribe(&mqtt("lights/{id}/on"), "g1")
        .await
        .unwrap();
    let mut other = broker
        .subscribe(&mqtt("lights/{id}/off"), "g1")
        .await
        .unwrap();

    broker
        .publish(&mqtt("lights/42/on"), out(r#"{"on":true}"#))
        .await
        .unwrap();

    let got = tokio::time::timeout(Duration::from_secs(1), matching.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        got,
        InMessage {
            address: "lights/42/on".into(),
            key: None,
            headers: vec![("x-mockingbird-hop".into(), "0".into())],
            payload: br#"{"on":true}"#.to_vec(),
        }
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), other.next())
            .await
            .is_err()
    );
    let published = broker.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].0, mqtt("lights/42/on"));

    // Injected messages are delivered but not recorded.
    broker.inject(
        &mqtt("lights/7/off"),
        InMessage {
            address: "lights/7/off".into(),
            key: None,
            headers: Vec::new(),
            payload: b"{}".to_vec(),
        },
    );
    let got = tokio::time::timeout(Duration::from_secs(1), other.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(got.address, "lights/7/off");
    assert_eq!(broker.published().len(), 1);
}

#[tokio::test]
async fn memory_fail_publishes_counts_down() {
    let broker = MemoryBroker::new();
    broker.fail_publishes(2);
    let target = mqtt("a/b");
    assert!(matches!(
        broker.publish(&target, out("1")).await,
        Err(BrokerError::Publish(_))
    ));
    assert!(matches!(
        broker.publish(&target, out("2")).await,
        Err(BrokerError::Publish(_))
    ));
    broker.publish(&target, out("3")).await.unwrap();
    let published = broker.wait_published(1, Duration::from_secs(1)).await;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].1.payload, b"3".to_vec());
}

#[tokio::test]
async fn memory_wait_published_waits_for_later_publishes() {
    let broker = MemoryBroker::new();
    let publisher = Arc::clone(&broker);
    tokio::spawn(async move {
        for i in 0..3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            publisher
                .publish(&mqtt("x"), out(&i.to_string()))
                .await
                .unwrap();
        }
    });
    let published = broker.wait_published(3, Duration::from_secs(2)).await;
    assert_eq!(published.len(), 3);
    // A timeout returns whatever was published.
    let published = broker.wait_published(5, Duration::from_millis(20)).await;
    assert_eq!(published.len(), 3);
}

#[tokio::test]
async fn brokers_memory_client_shared() {
    let brokers = Brokers::new(
        vec![config("mem", BrokerKind::Memory, "memory://")],
        secrets(),
    );
    assert_eq!(brokers.infos()[0].status, "not connected");
    let a = brokers.client("mem").await.unwrap();
    let b = brokers.client("mem").await.unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    let mem = brokers.memory("mem").unwrap();
    let again = brokers.memory("mem").unwrap();
    assert!(Arc::ptr_eq(&mem, &again));

    // Publishing through the client lands in the shared memory broker.
    a.publish(&mqtt("t"), out("{}")).await.unwrap();
    assert_eq!(mem.published().len(), 1);
    assert_eq!(brokers.infos()[0].status, "ok");
    assert!(brokers.issues().is_empty());

    brokers.report("mem", Err(&BrokerError::Publish("boom".into())));
    assert_eq!(brokers.infos()[0].status, "error: publish failed: boom");
    assert_eq!(
        brokers.issues(),
        vec![("mem".to_string(), "publish failed: boom".to_string())]
    );
    brokers.report("mem", Ok(()));
    assert!(brokers.issues().is_empty());
    brokers.shutdown(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn unknown_broker_errors() {
    let brokers = Brokers::empty();
    assert!(brokers.client("nope").await.is_err());
    assert!(brokers.memory("nope").is_none());
    assert!(brokers.config("nope").is_none());
    assert!(brokers.names().is_empty());
    assert!(brokers.issues().is_empty());
}

#[cfg(not(feature = "kafka"))]
#[tokio::test]
async fn uncompiled_kind_unsupported() {
    let brokers = Brokers::new(
        vec![config("k", BrokerKind::Kafka, "127.0.0.1:9092")],
        secrets(),
    );
    assert!(!BrokerKind::Kafka.compiled());
    match brokers.client("k").await {
        Err(BrokerError::Unsupported(msg)) => {
            assert_eq!(msg, "kafka support is not compiled into this build")
        }
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("expected Unsupported"),
    }
    assert_eq!(
        brokers.issues(),
        vec![(
            "k".to_string(),
            "kafka support is not compiled into this build".to_string()
        )]
    );
}

#[tokio::test]
async fn password_resolved_from_secret_ref() {
    let store = Arc::new(RecordingSecrets::default());
    let mut cfg = config("mem", BrokerKind::Memory, "memory://");
    cfg.sasl = Some(SaslConfig {
        mechanism: "PLAIN".into(),
        username: "u".into(),
        password_ref: "env:MB_BROKER_PASSWORD".into(),
    });
    let brokers = Brokers::new(vec![cfg], Arc::clone(&store) as Arc<dyn SecretStore>);
    brokers.client("mem").await.unwrap();
    assert_eq!(
        *store.requested.lock().unwrap(),
        vec!["env:MB_BROKER_PASSWORD".to_string()]
    );
}

#[tokio::test]
async fn infos_strip_credentials() {
    let brokers = Brokers::new(
        vec![
            config("rabbit", BrokerKind::Amqp, "amqp://u:p@h:5672/%2f"),
            config("mem", BrokerKind::Memory, "memory://"),
        ],
        secrets(),
    );
    assert_eq!(
        brokers.names(),
        vec!["mem".to_string(), "rabbit".to_string()]
    );
    let infos = brokers.infos();
    let rabbit = infos.iter().find(|i| i.name == "rabbit").unwrap();
    assert_eq!(rabbit.url, "amqp://h:5672/%2f");
    assert_eq!(rabbit.kind, BrokerKind::Amqp);
    assert_eq!(rabbit.status, "not connected");
    assert_eq!(
        brokers.config("rabbit").unwrap().url,
        "amqp://u:p@h:5672/%2f"
    );
}

#[test]
fn broker_kind_serves() {
    let ws = ChannelBinding::WebSocket { path: "/ws".into() };
    let kafka = ChannelBinding::Kafka {
        topic: "t".into(),
        partitions_hint: None,
    };
    assert!(BrokerKind::Memory.serves(&kafka));
    assert!(BrokerKind::Memory.serves(&mqtt("t")));
    assert!(!BrokerKind::Memory.serves(&ws));
    assert!(BrokerKind::Kafka.serves(&kafka));
    assert!(!BrokerKind::Kafka.serves(&mqtt("t")));
    assert!(BrokerKind::Mqtt.serves(&mqtt("t")));
    assert!(!BrokerKind::Amqp.serves(&ws));
    assert!(BrokerKind::Memory.compiled());
    assert_eq!(BrokerKind::Memory.as_str(), "memory");
}

#[test]
fn param_values_from_context_then_faker() {
    let bundle = mb_import::compile(include_bytes!(
        "../../../fixtures/asyncapi/streetlights-kafka-2.6.yaml"
    ))
    .unwrap();
    let channel = bundle
        .channels
        .iter()
        .find(|c| c.address.contains("lighting.measured"))
        .unwrap();
    let engine = Engine::new(None, EngineConfig::default());

    let context = json!({"streetlightId": "lamp 7"});
    let values = param_values(&engine, &bundle, channel, &[&context]);
    assert_eq!(
        values,
        BTreeMap::from([("streetlightId".to_string(), "lamp_7".to_string())])
    );

    // A number field counts; a later source does not override an earlier one.
    let first = json!({"other": 1});
    let second = json!({"streetlightId": 12});
    let third = json!({"streetlightId": "ignored"});
    let values = param_values(&engine, &bundle, channel, &[&first, &second, &third]);
    assert_eq!(values["streetlightId"], "12");

    // No source: the faker's string, sanitized.
    let values = param_values(&engine, &bundle, channel, &[]);
    let faked = &values["streetlightId"];
    assert!(!faked.is_empty());
    assert!(
        faked
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "{faked}"
    );

    // A parameter without a schema falls back to "1".
    let mut bare = channel.clone();
    bare.parameters.clear();
    let values = param_values(&engine, &bundle, &bare, &[]);
    assert_eq!(values["streetlightId"], "1");
}

#[tokio::test]
async fn memory_keeps_a_bounded_history() {
    let broker = MemoryBroker::new();
    for i in 0..1500 {
        broker
            .publish(&mqtt("a/b"), out(&format!("{{\"i\":{i}}}")))
            .await
            .unwrap();
    }
    let published = broker.published();
    assert_eq!(published.len(), 1000);
    assert_eq!(published[0].1.payload, br#"{"i":500}"#.to_vec());
    assert_eq!(published[999].1.payload, br#"{"i":1499}"#.to_vec());
    assert_eq!(broker.published_count(), 1500);
}

#[tokio::test]
async fn memory_slow_subscriber_queue_is_bounded() {
    let broker = MemoryBroker::new();
    let mut slow = broker.subscribe(&mqtt("a/{x}"), "g").await.unwrap();
    for i in 0..500 {
        broker
            .publish(&mqtt("a/b"), out(&format!("{{\"i\":{i}}}")))
            .await
            .unwrap();
    }
    let mut queued = 0;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(50), slow.next()).await {
        queued += 1;
    }
    assert!(queued > 0 && queued <= 64, "queued {queued}");
    // The subscription keeps working once drained.
    broker.publish(&mqtt("a/c"), out("{}")).await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), slow.next())
        .await
        .unwrap();
    assert!(matches!(next, Some(Ok(_))));
}
