#![cfg(test)]
//! The public API's common traits: `Display`/`FromStr` of the named enums,
//! and `Debug` output that never shows a secret.

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_protocols::events::{BrokerConfig, BrokerKind, Brokers, SaslConfig};
use mb_protocols::webhooks::deliver::{
    DispatchSettings, ResolvedTarget, SecretBytes, WebhookDispatcher,
};
use mb_protocols::webhooks::log::{DeliveryLog, DeliveryStatus};
use mb_protocols::webhooks::sign::Signing;
use mb_protocols::{MockRuntime, ParseNameError, RequestKind};
use mb_state::MemoryStore;

/// A value no `Debug` output may contain.
const SECRET: &str = "s3cr3t-Pa55word";

#[test]
fn named_enums_display_and_parse_their_names() {
    for kind in [
        BrokerKind::Kafka,
        BrokerKind::Amqp,
        BrokerKind::Mqtt,
        BrokerKind::Memory,
    ] {
        assert_eq!(kind.to_string(), kind.as_str());
        assert_eq!(kind.as_str().parse::<BrokerKind>(), Ok(kind));
    }
    for kind in [
        RequestKind::Http,
        RequestKind::Graphql,
        RequestKind::Grpc,
        RequestKind::Soap,
        RequestKind::Event,
    ] {
        assert_eq!(kind.to_string(), kind.as_str());
        assert_eq!(kind.as_str().parse::<RequestKind>(), Ok(kind));
    }
    for status in [
        DeliveryStatus::Pending,
        DeliveryStatus::Delivered,
        DeliveryStatus::Failed,
        DeliveryStatus::Dropped,
    ] {
        assert_eq!(status.to_string(), status.as_str());
        assert_eq!(status.as_str().parse::<DeliveryStatus>(), Ok(status));
    }
}

#[test]
fn unknown_names_are_typed_errors() {
    let err: ParseNameError = "nats".parse::<BrokerKind>().unwrap_err();
    assert_eq!(err.name(), "nats");
    assert_eq!(err.to_string(), "unknown broker kind \"nats\"");
    assert!(
        "HTTP".parse::<RequestKind>().is_err(),
        "names are lowercase"
    );
    assert!("".parse::<DeliveryStatus>().is_err());
}

fn broker(url: String) -> BrokerConfig {
    BrokerConfig {
        name: "events".into(),
        kind: BrokerKind::Amqp,
        url,
        tls: false,
        ca_file: None,
        sasl: Some(SaslConfig {
            mechanism: "PLAIN".into(),
            username: "app".into(),
            password_ref: "env:BROKER_PASSWORD".into(),
        }),
        client_id_prefix: "mb".into(),
    }
}

#[test]
fn broker_debug_hides_url_credentials() {
    let config = broker(format!("amqp://app:{SECRET}@broker:5672/vhost"));
    let shown = format!("{config:?}");
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(shown.contains("amqp://broker:5672/vhost"), "{shown}");
    let brokers = Brokers::new(vec![config], Arc::new(mb_ext::EnvFileSecrets));
    let shown = format!("{brokers:?}");
    assert!(!shown.contains(SECRET), "{shown}");
}

fn target() -> ResolvedTarget {
    ResolvedTarget {
        url: format!("https://hook:{SECRET}@example.com/in?token={SECRET}#{SECRET}"),
        events: vec!["*".into()],
        signing: Signing::Stripe,
        secret: Ok(Some(SecretBytes::new(SECRET.into()))),
        delay: Duration::ZERO,
    }
}

#[test]
fn webhook_debug_hides_url_credentials_and_the_secret() {
    let shown = format!("{:?}", target());
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(shown.contains("https://example.com/in"), "{shown}");
    let dispatcher = WebhookDispatcher::new(
        "default".into(),
        "m".into(),
        vec![target()],
        reqwest::Client::new(),
        Arc::new(DeliveryLog::default()),
        DispatchSettings::default(),
    );
    let shown = format!("{dispatcher:?}");
    assert!(!shown.contains(SECRET), "{shown}");
}

#[tokio::test]
async fn runtime_debug_hides_webhook_secrets() {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let bundle = Arc::new(compile(&std::fs::read(path).unwrap()).unwrap());
    let mut rt = MockRuntime::new(
        "orders",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(60))),
    );
    rt.webhooks = Some(Arc::new(WebhookDispatcher::new(
        "default".into(),
        "orders".into(),
        vec![target()],
        reqwest::Client::new(),
        Arc::new(DeliveryLog::default()),
        DispatchSettings::default(),
    )));
    let shown = format!("{rt:?}");
    assert!(shown.contains("\"orders\""), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
}

#[test]
fn retry_delay_doubles_from_the_backoff_base() {
    let settings = DispatchSettings {
        backoff_base: Duration::from_millis(100),
        ..DispatchSettings::default()
    };
    assert_eq!(settings.retry_delay(1), Duration::from_millis(100));
    assert_eq!(settings.retry_delay(3), Duration::from_millis(400));
    assert_eq!(
        settings.retry_delay(u32::MAX),
        Duration::from_millis(100) * u32::MAX
    );
}
