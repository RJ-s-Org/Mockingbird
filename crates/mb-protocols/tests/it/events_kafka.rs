#![cfg(feature = "kafka")]

use std::time::{Duration, Instant};

use futures::StreamExt;
use mb_mcm::ChannelBinding;
use mb_protocols::events::{
    BrokerClient, BrokerConfig, BrokerError, BrokerKind, KafkaClient, OutMessage,
};

fn kafka_config(url: &str) -> BrokerConfig {
    BrokerConfig {
        name: "kafka".into(),
        kind: BrokerKind::Kafka,
        url: url.into(),
        tls: false,
        ca_file: None,
        sasl: None,
        client_id_prefix: "mockingbird".into(),
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn kafka_round_trip() {
    let url = match std::env::var("MB_TEST_KAFKA_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!("skipped: MB_TEST_KAFKA_URL not set");
            return;
        }
    };
    let client = KafkaClient::connect(&kafka_config(&url), None)
        .await
        .unwrap();
    let topic = format!("mb-test-{}", uuid::Uuid::new_v4().simple());
    let binding = ChannelBinding::Kafka {
        topic: topic.clone(),
        partitions_hint: None,
    };
    let mut stream = client.subscribe(&binding, "mb-test-group").await.unwrap();
    // Let the consumer group assign partitions.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let msg = OutMessage {
        key: Some(b"k1".to_vec()),
        headers: vec![("x-mockingbird-hop".into(), "0".into())],
        payload: br#"{"lumens":3}"#.to_vec(),
        content_type: "application/json".into(),
    };
    // The subscription created the topic, so the group can take a few
    // more seconds to be assigned (brokers delay the first rebalance by
    // 3 s by default): publish again each second until one arrives.
    let deadline = Instant::now() + Duration::from_secs(30);
    let received = loop {
        client.publish(&binding, msg.clone()).await.unwrap();
        match tokio::time::timeout(Duration::from_secs(1), stream.next()).await {
            Ok(item) => break item.unwrap().unwrap(),
            Err(_) => assert!(Instant::now() < deadline, "nothing received within 30 s"),
        }
    };
    assert_eq!(received.address, topic);
    assert_eq!(received.key, Some(b"k1".to_vec()));
    assert_eq!(received.payload, br#"{"lumens":3}"#.to_vec());
    assert_eq!(header(&received.headers, "x-mockingbird-hop"), Some("0"));
}

#[tokio::test]
async fn kafka_unreachable_connect_error() {
    let started = Instant::now();
    let result = KafkaClient::connect(&kafka_config("127.0.0.1:1"), None).await;
    assert!(
        matches!(result, Err(BrokerError::Connect(_))),
        "expected a Connect error"
    );
    assert!(started.elapsed() < Duration::from_secs(15));
}
