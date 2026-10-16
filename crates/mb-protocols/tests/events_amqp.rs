#![cfg(feature = "amqp")]
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use futures::StreamExt;
use mb_mcm::ChannelBinding;
use mb_protocols::events::{
    AmqpClient, BrokerClient, BrokerConfig, BrokerError, BrokerKind, OutMessage, SaslConfig,
};

/// The broker URL, or `None` (and a skip note) when it is not set.
fn amqp_url() -> Option<String> {
    match std::env::var("MB_TEST_AMQP_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ => {
            eprintln!("skipped: MB_TEST_AMQP_URL not set");
            None
        }
    }
}

fn amqp_config(url: &str) -> BrokerConfig {
    BrokerConfig {
        name: "rabbit".into(),
        kind: BrokerKind::Amqp,
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

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

#[tokio::test]
async fn amqp_round_trip() {
    let Some(url) = amqp_url() else { return };
    let client = AmqpClient::connect(&amqp_config(&url), None).await.unwrap();
    let exchange = unique("mb-test");
    let binding = |key: &str| ChannelBinding::Amqp {
        exchange: exchange.clone(),
        exchange_type: "topic".into(),
        routing_key: key.into(),
        queue: None,
    };
    let mut stream = client
        .subscribe(&binding("orders.{id}"), "g1")
        .await
        .unwrap();

    let msg = OutMessage {
        key: None,
        headers: vec![
            ("x-mockingbird-hop".into(), "1".into()),
            ("correlation_id".into(), "corr-7".into()),
            ("reply_to".into(), "replies".into()),
        ],
        payload: br#"{"id":7}"#.to_vec(),
        content_type: "application/json".into(),
    };
    client.publish(&binding("orders.7"), msg).await.unwrap();

    let received = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(received.address, "orders.7");
    assert_eq!(received.payload, br#"{"id":7}"#.to_vec());
    assert_eq!(header(&received.headers, "x-mockingbird-hop"), Some("1"));
    assert_eq!(header(&received.headers, "correlation_id"), Some("corr-7"));
    assert_eq!(header(&received.headers, "reply_to"), Some("replies"));
}

#[tokio::test]
async fn amqp_queue_binding_round_trip() {
    let Some(url) = amqp_url() else { return };
    let client = AmqpClient::connect(&amqp_config(&url), None).await.unwrap();
    let exchange = unique("mb-test");
    let queue = unique("mb-test-q");
    let binding = ChannelBinding::Amqp {
        exchange: exchange.clone(),
        exchange_type: "direct".into(),
        routing_key: "created".into(),
        queue: Some(queue.clone()),
    };
    let mut stream = client.subscribe(&binding, "g2").await.unwrap();

    let msg = OutMessage {
        key: None,
        headers: Vec::new(),
        payload: b"hello".to_vec(),
        content_type: "text/plain".into(),
    };
    client.publish(&binding, msg).await.unwrap();

    let received = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(received.address, "created");
    assert_eq!(received.payload, b"hello".to_vec());
}

#[tokio::test]
async fn amqp_parameterized_key_needs_topic_exchange() {
    let Some(url) = amqp_url() else { return };
    let client = AmqpClient::connect(&amqp_config(&url), None).await.unwrap();
    let binding = ChannelBinding::Amqp {
        exchange: unique("mb-test"),
        exchange_type: "direct".into(),
        routing_key: "orders.{id}".into(),
        queue: None,
    };
    let err = client.subscribe(&binding, "g3").await.err().unwrap();
    assert_eq!(
        err.to_string(),
        "subscribe failed: parameterized routing keys need a topic exchange"
    );
}

#[tokio::test]
async fn amqp_sasl_credentials() {
    let Some(url) = amqp_url() else { return };
    let mut cfg = amqp_config(&url);
    let parsed = cfg.display_url();
    let (user, password) = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('@'))
        .and_then(|(userinfo, _)| userinfo.split_once(':'))
        .map(|(u, p)| (u.to_string(), p.to_string()))
        .unwrap_or_else(|| ("guest".into(), "guest".into()));
    cfg.url = parsed;
    cfg.sasl = Some(SaslConfig {
        mechanism: "PLAIN".into(),
        username: user,
        password_ref: "env:UNUSED".into(),
    });
    AmqpClient::connect(&cfg, Some(password)).await.unwrap();
    let err = AmqpClient::connect(&cfg, Some("wrong-password".into()))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, BrokerError::Connect(_)), "{err}");
}
