//! The Kafka broker client (feature `kafka`), built on rdkafka.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use mb_mcm::ChannelBinding;
use rdkafka::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::error::{KafkaError, RDKafkaErrorCode};
use rdkafka::message::{Header, Headers, Message, OwnedHeaders};
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};

use super::address::param_names;
use super::broker::{BrokerClient, BrokerConfig, BrokerError, MessageStream};
use super::{InMessage, OutMessage};

/// How long reaching the cluster may take when connecting.
const METADATA_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a send may wait for room in the producer queue.
const QUEUE_TIMEOUT: Duration = Duration::from_secs(10);
/// What a `{p}` placeholder matches in a subscription pattern.
const PARAM_PATTERN: &str = "[A-Za-z0-9_-]+";
/// Received messages buffered ahead of the stream's reader.
const STREAM_CAPACITY: usize = 64;

/// A Kafka producer; each subscription builds a consumer of its own from
/// the same configuration.
pub struct KafkaClient {
    config: ClientConfig,
    producer: FutureProducer,
}

/// Shows nothing of the client configuration, which holds the SASL
/// password.
impl std::fmt::Debug for KafkaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaClient").finish_non_exhaustive()
    }
}

/// The client configuration of `cfg`, with `password` as the SASL
/// password. The password is kept out of the config's `Debug` output.
pub(crate) fn kafka_client_config(cfg: &BrokerConfig, password: Option<&str>) -> ClientConfig {
    let mut config = ClientConfig::new();
    config
        .set("bootstrap.servers", &cfg.url)
        .set("client.id", &cfg.client_id_prefix)
        .set("message.timeout.ms", "10000");
    let protocol = match (cfg.tls, cfg.sasl.is_some()) {
        (false, false) => "PLAINTEXT",
        (true, false) => "SSL",
        (false, true) => "SASL_PLAINTEXT",
        (true, true) => "SASL_SSL",
    };
    config.set("security.protocol", protocol);
    if let Some(sasl) = &cfg.sasl {
        config
            .set("sasl.mechanisms", &sasl.mechanism)
            .set("sasl.username", &sasl.username)
            .set("sasl.password", password.unwrap_or_default());
    }
    if let Some(ca_file) = &cfg.ca_file {
        config.set("ssl.ca.location", ca_file.to_string_lossy());
    }
    config
}

/// `regex` with its metacharacters escaped.
fn escape_regex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The subscription for `topic`: the topic itself without placeholders,
/// else an anchored regex (`^…$`) in which each `{p}` matches one
/// parameter value.
pub(crate) fn kafka_subscription(topic: &str) -> String {
    let names = param_names(topic);
    if names.is_empty() {
        return topic.to_string();
    }
    let mut pattern = String::from("^");
    let mut rest = topic;
    loop {
        // The earliest remaining placeholder.
        let next = names
            .iter()
            .filter_map(|name| {
                let placeholder = format!("{{{name}}}");
                rest.find(&placeholder).map(|at| (at, placeholder.len()))
            })
            .min();
        let Some((at, len)) = next else {
            pattern.push_str(&escape_regex(rest));
            break;
        };
        pattern.push_str(&escape_regex(&rest[..at]));
        pattern.push_str(PARAM_PATTERN);
        rest = &rest[at + len..];
    }
    pattern.push('$');
    pattern
}

/// The message a consumed record carries.
fn in_message<M: Message>(message: &M) -> InMessage {
    let headers = message
        .headers()
        .map(|headers| {
            (0..headers.count())
                .filter_map(|i| headers.try_get(i))
                .map(|header| {
                    let value = header
                        .value
                        .map(|v| String::from_utf8_lossy(v).into_owned())
                        .unwrap_or_default();
                    (header.key.to_string(), value)
                })
                .collect()
        })
        .unwrap_or_default();
    InMessage {
        address: message.topic().to_string(),
        key: message.key().map(<[u8]>::to_vec),
        headers,
        payload: message.payload().map(<[u8]>::to_vec).unwrap_or_default(),
    }
}

/// Whether `error` only reports that a subscribed topic does not exist
/// yet; the consumer picks the topic up once it is created.
fn topic_not_there_yet(error: &KafkaError) -> bool {
    matches!(
        error,
        KafkaError::MessageConsumption(
            RDKafkaErrorCode::UnknownTopicOrPartition | RDKafkaErrorCode::UnknownTopic
        )
    )
}

/// Receive from `consumer` into `tx` until `tx` closes; skips the notices
/// that a subscribed topic does not exist yet.
async fn forward(
    consumer: StreamConsumer,
    tx: tokio::sync::mpsc::Sender<Result<InMessage, BrokerError>>,
) {
    loop {
        let item = tokio::select! {
            () = tx.closed() => return,
            received = consumer.recv() => match received {
                Ok(message) => Ok(in_message(&message)),
                Err(e) if topic_not_there_yet(&e) => {
                    tracing::debug!(error = %e, "kafka topic does not exist yet");
                    continue;
                }
                Err(e) => Err(BrokerError::Subscribe(e.to_string())),
            },
        };
        if tx.send(item).await.is_err() {
            return;
        }
    }
}

fn kafka_topic(binding: &ChannelBinding) -> Result<&str, BrokerError> {
    match binding {
        ChannelBinding::Kafka { topic, .. } => Ok(topic),
        _ => Err(BrokerError::Unsupported(
            "a kafka broker serves only kafka bindings".into(),
        )),
    }
}

impl KafkaClient {
    /// Create the producer and check that the cluster answers a metadata
    /// request within 10 s.
    ///
    /// # Errors
    ///
    /// A [`BrokerError`] when the producer cannot be created or the
    /// cluster does not answer in time.
    pub async fn connect(
        cfg: &BrokerConfig,
        password: Option<String>,
    ) -> Result<Self, BrokerError> {
        Self::connect_with_timeout(cfg, password, METADATA_TIMEOUT).await
    }

    /// [`KafkaClient::connect`], waiting at most `metadata_timeout` for the
    /// cluster to answer the metadata request. librdkafka retries an
    /// unreachable bootstrap server until then, so a refused connection
    /// fails only once the timeout has passed.
    ///
    /// # Errors
    ///
    /// As [`KafkaClient::connect`].
    pub async fn connect_with_timeout(
        cfg: &BrokerConfig,
        password: Option<String>,
        metadata_timeout: Duration,
    ) -> Result<Self, BrokerError> {
        let config = kafka_client_config(cfg, password.as_deref());
        let producer: FutureProducer = config
            .create()
            .map_err(|e| BrokerError::Connect(e.to_string()))?;
        let probe = producer.clone();
        tokio::task::spawn_blocking(move || {
            probe
                .client()
                .fetch_metadata(None, metadata_timeout)
                .map(|_| ())
        })
        .await
        .map_err(|e| BrokerError::Connect(e.to_string()))?
        .map_err(|e| BrokerError::Connect(e.to_string()))?;
        Ok(Self { config, producer })
    }
}

#[async_trait]
impl BrokerClient for KafkaClient {
    async fn publish(&self, target: &ChannelBinding, msg: OutMessage) -> Result<(), BrokerError> {
        let topic = kafka_topic(target)?;
        let mut headers = OwnedHeaders::new_with_capacity(msg.headers.len());
        for (name, value) in &msg.headers {
            headers = headers.insert(Header {
                key: name,
                value: Some(value.as_bytes()),
            });
        }
        let mut record = FutureRecord::to(topic)
            .payload(msg.payload.as_slice())
            .headers(headers);
        if let Some(key) = &msg.key {
            record = record.key(key.as_slice());
        }
        self.producer
            .send(record, QUEUE_TIMEOUT)
            .await
            .map(|_| ())
            .map_err(|(e, _)| BrokerError::Publish(e.to_string()))
    }

    async fn subscribe(
        &self,
        binding: &ChannelBinding,
        group: &str,
    ) -> Result<MessageStream, BrokerError> {
        let topic = kafka_topic(binding)?;
        let mut config = self.config.clone();
        config
            .set("group.id", group)
            .set("enable.auto.commit", "true")
            .set("auto.offset.reset", "latest")
            // A mock may subscribe before anything has produced to the
            // topic; let the subscription create it (where the cluster
            // allows) so the group is assigned right away.
            .set("allow.auto.create.topics", "true");
        let consumer: StreamConsumer = config
            .create()
            .map_err(|e| BrokerError::Subscribe(e.to_string()))?;
        consumer
            .subscribe(&[&kafka_subscription(topic)])
            .map_err(|e| BrokerError::Subscribe(e.to_string()))?;
        // Poll the consumer right away, not only once the stream is first
        // polled: librdkafka completes the group assignment (and resolves
        // `latest` offsets) only while the consumer is polled. The task
        // ends when the stream is dropped.
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_CAPACITY);
        tokio::spawn(forward(consumer, tx));
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            let item = rx.recv().await?;
            Some((item, rx))
        });
        Ok(stream.boxed())
    }

    async fn flush(&self, timeout: Duration) {
        let producer = self.producer.clone();
        let flushed = tokio::task::spawn_blocking(move || producer.flush(timeout)).await;
        if let Ok(Err(e)) = flushed {
            tracing::warn!(error = %e, "kafka producer flush failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{BrokerKind, SaslConfig};

    fn config(tls: bool, sasl: bool) -> BrokerConfig {
        BrokerConfig {
            name: "kafka".into(),
            kind: BrokerKind::Kafka,
            url: "k1:9092,k2:9092".into(),
            tls,
            ca_file: tls.then(|| "/etc/ca.pem".into()),
            sasl: sasl.then(|| SaslConfig {
                mechanism: "SCRAM-SHA-512".into(),
                username: "svc".into(),
                password_ref: "env:KAFKA_PASSWORD".into(),
            }),
            client_id_prefix: "mockingbird".into(),
        }
    }

    #[test]
    fn kafka_config_security_protocols() {
        let cases = [
            (false, false, "PLAINTEXT"),
            (true, false, "SSL"),
            (false, true, "SASL_PLAINTEXT"),
            (true, true, "SASL_SSL"),
        ];
        for (tls, sasl, protocol) in cases {
            let cfg = config(tls, sasl);
            let password = sasl.then_some("s3cret-pw");
            let client = kafka_client_config(&cfg, password);
            assert_eq!(client.get("security.protocol"), Some(protocol));
            assert_eq!(client.get("bootstrap.servers"), Some("k1:9092,k2:9092"));
            assert_eq!(client.get("client.id"), Some("mockingbird"));
            assert_eq!(client.get("message.timeout.ms"), Some("10000"));
            if sasl {
                assert_eq!(client.get("sasl.mechanisms"), Some("SCRAM-SHA-512"));
                assert_eq!(client.get("sasl.username"), Some("svc"));
                assert_eq!(client.get("sasl.password"), Some("s3cret-pw"));
            } else {
                assert_eq!(client.get("sasl.mechanisms"), None);
                assert_eq!(client.get("sasl.password"), None);
            }
            if tls {
                assert_eq!(client.get("ssl.ca.location"), Some("/etc/ca.pem"));
            } else {
                assert_eq!(client.get("ssl.ca.location"), None);
            }
            assert!(!format!("{client:?}").contains("s3cret-pw"));
        }
    }

    #[test]
    fn kafka_subscription_regex() {
        assert_eq!(kafka_subscription("a.{id}.b"), r"^a\.[A-Za-z0-9_-]+\.b$");
        assert_eq!(kafka_subscription("plain.topic"), "plain.topic");
        assert_eq!(
            kafka_subscription("{a}-{b}"),
            "^[A-Za-z0-9_-]+-[A-Za-z0-9_-]+$"
        );
    }
}
