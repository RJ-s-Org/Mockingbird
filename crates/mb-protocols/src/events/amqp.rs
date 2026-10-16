//! The AMQP 0-9-1 broker client (feature `amqp`), built on lapin.

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use futures::StreamExt;
use lapin::options::{
    BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, ConfirmSelectOptions,
    ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions,
};
use lapin::tcp::OwnedTLSConfig;
use lapin::types::{AMQPValue, FieldTable, ShortString};
use lapin::{
    BasicProperties, Channel, Confirmation, Connection, ConnectionProperties, ExchangeKind,
};
use mb_mcm::ChannelBinding;

use super::address::{param_names, sanitize_param};
use super::broker::{BrokerClient, BrokerConfig, BrokerError, MessageStream};
use super::{InMessage, OutMessage};

/// The header mapped to the `correlation_id` property.
const HEADER_CORRELATION: &str = "correlation_id";
/// The header mapped to the `reply_to` property.
const HEADER_REPLY_TO: &str = "reply_to";

/// A connection to an AMQP 0-9-1 broker with one publish channel in
/// confirm mode. Each subscription consumes on a channel of its own.
pub struct AmqpClient {
    connection: Connection,
    /// The publish channel; replaced when the broker has closed it.
    publisher: tokio::sync::Mutex<Channel>,
    /// The exchanges this client has declared.
    declared: Mutex<HashSet<String>>,
}

fn lock(set: &Mutex<HashSet<String>>) -> MutexGuard<'_, HashSet<String>> {
    set.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Percent-encode everything but the RFC 3986 unreserved characters.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The connection URI: `cfg.url` with the SASL username and `password`
/// (percent-encoded) as its userinfo, and `amqps` when `tls` is set.
pub(crate) fn amqp_uri(cfg: &BrokerConfig, password: Option<&str>) -> Result<String, BrokerError> {
    let (rest, secure) = if let Some(rest) = cfg.url.strip_prefix("amqp://") {
        (rest, false)
    } else if let Some(rest) = cfg.url.strip_prefix("amqps://") {
        (rest, true)
    } else {
        return Err(BrokerError::Connect(
            "amqp url must start with amqp:// or amqps://".into(),
        ));
    };
    let scheme = if secure || cfg.tls { "amqps" } else { "amqp" };
    let Some(sasl) = &cfg.sasl else {
        return Ok(format!("{scheme}://{rest}"));
    };
    if !sasl.mechanism.eq_ignore_ascii_case("PLAIN") {
        return Err(BrokerError::Connect(
            "amqp supports only PLAIN credentials".into(),
        ));
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let host = authority.rsplit('@').next().unwrap_or_default();
    Ok(format!(
        "{scheme}://{}:{}@{host}{tail}",
        percent_encode(&sasl.username),
        percent_encode(password.unwrap_or_default())
    ))
}

/// `routing_key` with every word holding a `{p}` placeholder replaced by
/// the one-word wildcard `*`.
pub(crate) fn amqp_binding_key(routing_key: &str) -> String {
    routing_key
        .split('.')
        .map(|word| {
            if param_names(word).is_empty() {
                word
            } else {
                "*"
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn exchange_kind(exchange_type: &str) -> ExchangeKind {
    match exchange_type {
        "direct" | "" => ExchangeKind::Direct,
        "topic" => ExchangeKind::Topic,
        "fanout" => ExchangeKind::Fanout,
        "headers" => ExchangeKind::Headers,
        other => ExchangeKind::Custom(other.to_string()),
    }
}

/// The CA certificates of `ca_file` as PEM text.
fn tls_config(cfg: &BrokerConfig) -> Result<OwnedTLSConfig, BrokerError> {
    let cert_chain = match &cfg.ca_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| BrokerError::Connect(format!("ca_file {}: {e}", path.display())))?,
        ),
        None => None,
    };
    Ok(OwnedTLSConfig {
        identity: None,
        cert_chain,
    })
}

/// A header value as text; `None` for tables, arrays and the like.
fn header_text(value: &AMQPValue) -> Option<String> {
    Some(match value {
        AMQPValue::LongString(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
        AMQPValue::ShortString(s) => s.as_str().to_string(),
        AMQPValue::Boolean(b) => b.to_string(),
        AMQPValue::ShortShortInt(n) => n.to_string(),
        AMQPValue::ShortShortUInt(n) => n.to_string(),
        AMQPValue::ShortInt(n) => n.to_string(),
        AMQPValue::ShortUInt(n) => n.to_string(),
        AMQPValue::LongInt(n) => n.to_string(),
        AMQPValue::LongUInt(n) => n.to_string(),
        AMQPValue::LongLongInt(n) => n.to_string(),
        AMQPValue::Float(n) => n.to_string(),
        AMQPValue::Double(n) => n.to_string(),
        _ => return None,
    })
}

/// The message a delivery carries.
fn in_message(delivery: &lapin::message::Delivery) -> InMessage {
    let props = &delivery.properties;
    let mut headers: Vec<(String, String)> = props
        .headers()
        .iter()
        .flat_map(|table| table.inner().iter())
        .filter_map(|(name, value)| Some((name.as_str().to_string(), header_text(value)?)))
        .collect();
    if let Some(id) = props.correlation_id() {
        headers.push((HEADER_CORRELATION.into(), id.as_str().to_string()));
    }
    if let Some(reply_to) = props.reply_to() {
        headers.push((HEADER_REPLY_TO.into(), reply_to.as_str().to_string()));
    }
    InMessage {
        address: delivery.routing_key.as_str().to_string(),
        key: None,
        headers,
        payload: delivery.data.clone(),
    }
}

/// The properties of a published message.
fn properties(msg: &OutMessage) -> BasicProperties {
    let mut props = BasicProperties::default();
    if !msg.content_type.is_empty() {
        props = props.with_content_type(msg.content_type.as_str().into());
    }
    let mut table = FieldTable::default();
    for (name, value) in &msg.headers {
        match name.as_str() {
            HEADER_CORRELATION => props = props.with_correlation_id(value.as_str().into()),
            HEADER_REPLY_TO => props = props.with_reply_to(value.as_str().into()),
            _ => {
                table.insert(
                    name.as_str().into(),
                    AMQPValue::LongString(value.as_str().into()),
                );
            }
        }
    }
    if !table.inner().is_empty() {
        props = props.with_headers(table);
    }
    props
}

fn amqp_binding(binding: &ChannelBinding) -> Result<(&str, &str, &str, Option<&str>), BrokerError> {
    match binding {
        ChannelBinding::Amqp {
            exchange,
            exchange_type,
            routing_key,
            queue,
        } => Ok((exchange, exchange_type, routing_key, queue.as_deref())),
        _ => Err(BrokerError::Unsupported(
            "an amqp broker serves only amqp bindings".into(),
        )),
    }
}

/// A channel in confirm mode.
async fn confirm_channel(connection: &Connection) -> Result<Channel, lapin::Error> {
    let channel = connection.create_channel().await?;
    channel
        .confirm_select(ConfirmSelectOptions::default())
        .await?;
    Ok(channel)
}

impl AmqpClient {
    /// Connect to `cfg.url` and open the publish channel.
    pub async fn connect(
        cfg: &BrokerConfig,
        password: Option<String>,
    ) -> Result<Self, BrokerError> {
        let uri = amqp_uri(cfg, password.as_deref())?;
        let properties = ConnectionProperties::default()
            .with_connection_name(cfg.client_id_prefix.as_str().into());
        let runtime =
            lapin::runtime::default_runtime().map_err(|e| BrokerError::Connect(e.to_string()))?;
        let connection =
            Connection::connect_with_config(&uri, properties, tls_config(cfg)?, runtime)
                .await
                .map_err(|e| BrokerError::Connect(e.to_string()))?;
        let publisher = confirm_channel(&connection)
            .await
            .map_err(|e| BrokerError::Connect(e.to_string()))?;
        Ok(Self {
            connection,
            publisher: tokio::sync::Mutex::new(publisher),
            declared: Mutex::new(HashSet::new()),
        })
    }

    /// Declare `exchange` (durable) unless this client already has. The
    /// declare runs on a channel of its own, so a refused declare cannot
    /// close the publish channel.
    async fn declare_exchange(&self, exchange: &str, exchange_type: &str) -> Result<(), String> {
        if exchange.is_empty() || lock(&self.declared).contains(exchange) {
            return Ok(());
        }
        let channel = self
            .connection
            .create_channel()
            .await
            .map_err(|e| e.to_string())?;
        let options = ExchangeDeclareOptions {
            durable: true,
            ..ExchangeDeclareOptions::default()
        };
        let declared = channel
            .exchange_declare(
                exchange.into(),
                exchange_kind(exchange_type),
                options,
                FieldTable::default(),
            )
            .await
            .map_err(|e| format!("declare exchange {exchange}: {e}"));
        let _ = channel.close(200, "OK".into()).await;
        declared?;
        lock(&self.declared).insert(exchange.to_string());
        Ok(())
    }

    /// The publish channel, reopened when the broker has closed it.
    async fn publisher(&self) -> Result<Channel, BrokerError> {
        let mut channel = self.publisher.lock().await;
        if !channel.status().connected() {
            *channel = confirm_channel(&self.connection)
                .await
                .map_err(|e| BrokerError::Publish(e.to_string()))?;
        }
        Ok(channel.clone())
    }
}

#[async_trait]
impl BrokerClient for AmqpClient {
    async fn publish(&self, target: &ChannelBinding, msg: OutMessage) -> Result<(), BrokerError> {
        let (exchange, exchange_type, routing_key, queue) = amqp_binding(target)?;
        let routing_key = match (routing_key, queue) {
            ("", Some(queue)) => queue,
            _ => routing_key,
        };
        self.declare_exchange(exchange, exchange_type)
            .await
            .map_err(BrokerError::Publish)?;
        let channel = self.publisher().await?;
        let confirm = channel
            .basic_publish(
                exchange.into(),
                routing_key.into(),
                BasicPublishOptions::default(),
                &msg.payload,
                properties(&msg),
            )
            .await
            .map_err(|e| BrokerError::Publish(e.to_string()))?
            .await
            .map_err(|e| BrokerError::Publish(e.to_string()))?;
        match confirm {
            Confirmation::Nack(_) => {
                Err(BrokerError::Publish("the broker nacked the message".into()))
            }
            Confirmation::Ack(_) | Confirmation::NotRequested => Ok(()),
        }
    }

    async fn subscribe(
        &self,
        binding: &ChannelBinding,
        group: &str,
    ) -> Result<MessageStream, BrokerError> {
        let (exchange, exchange_type, routing_key, queue) = amqp_binding(binding)?;
        let fail = |e: lapin::Error| BrokerError::Subscribe(e.to_string());
        let parameterized = !param_names(routing_key).is_empty();
        let binds = !exchange.is_empty();
        if binds && parameterized && exchange_type != "topic" {
            return Err(BrokerError::Subscribe(
                "parameterized routing keys need a topic exchange".into(),
            ));
        }
        self.declare_exchange(exchange, exchange_type)
            .await
            .map_err(BrokerError::Subscribe)?;
        let channel = self.connection.create_channel().await.map_err(fail)?;
        let queue_name = match queue {
            Some(queue) => {
                let options = QueueDeclareOptions {
                    durable: true,
                    ..QueueDeclareOptions::default()
                };
                channel
                    .queue_declare(queue.into(), options, FieldTable::default())
                    .await
                    .map_err(fail)?;
                queue.to_string()
            }
            None => {
                let name = format!("mockingbird-{group}-{}", sanitize_param(routing_key));
                let options = QueueDeclareOptions {
                    auto_delete: true,
                    ..QueueDeclareOptions::default()
                };
                channel
                    .queue_declare(name.as_str().into(), options, FieldTable::default())
                    .await
                    .map_err(fail)?;
                name
            }
        };
        if binds {
            channel
                .queue_bind(
                    queue_name.as_str().into(),
                    exchange.into(),
                    amqp_binding_key(routing_key).into(),
                    QueueBindOptions::default(),
                    FieldTable::default(),
                )
                .await
                .map_err(fail)?;
        }
        let tag: ShortString = format!("mockingbird-{}", uuid::Uuid::new_v4().simple()).into();
        let consumer = channel
            .basic_consume(
                queue_name.as_str().into(),
                tag,
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(fail)?;
        // The stream owns the channel: dropping the stream closes it.
        let stream =
            futures::stream::unfold((consumer, channel), |(mut consumer, channel)| async move {
                let item = match consumer.next().await? {
                    Ok(delivery) => {
                        let msg = in_message(&delivery);
                        match delivery.ack(BasicAckOptions::default()).await {
                            Ok(_) => Ok(msg),
                            Err(e) => Err(BrokerError::Subscribe(format!("ack: {e}"))),
                        }
                    }
                    Err(e) => Err(BrokerError::Subscribe(e.to_string())),
                };
                Some((item, (consumer, channel)))
            });
        Ok(stream.boxed())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::events::{BrokerKind, SaslConfig};

    fn config(url: &str, tls: bool, sasl: bool) -> BrokerConfig {
        BrokerConfig {
            name: "rabbit".into(),
            kind: BrokerKind::Amqp,
            url: url.into(),
            tls,
            ca_file: None,
            sasl: sasl.then(|| SaslConfig {
                mechanism: "PLAIN".into(),
                username: "u".into(),
                password_ref: "env:AMQP_PASSWORD".into(),
            }),
            client_id_prefix: "mockingbird".into(),
        }
    }

    #[test]
    fn amqp_uri_inserts_credentials() {
        let cfg = config("amqp://h:5672/%2f", false, true);
        assert_eq!(
            amqp_uri(&cfg, Some("p@ss")).unwrap(),
            "amqp://u:p%40ss@h:5672/%2f"
        );
        let tls = config("amqp://h:5672/%2f", true, true);
        assert_eq!(
            amqp_uri(&tls, Some("p@ss")).unwrap(),
            "amqps://u:p%40ss@h:5672/%2f"
        );
        // No credentials configured: the URL is kept as given.
        let plain = config("amqp://guest:guest@h:5672/%2f", false, false);
        assert_eq!(
            amqp_uri(&plain, None).unwrap(),
            "amqp://guest:guest@h:5672/%2f"
        );
        // Configured credentials replace any userinfo in the URL.
        let both = config("amqps://x:y@h/%2f", false, true);
        assert_eq!(amqp_uri(&both, Some("p")).unwrap(), "amqps://u:p@h/%2f");
    }

    #[test]
    fn amqp_uri_rejects_bad_input() {
        let mut cfg = config("amqp://h/%2f", false, true);
        if let Some(sasl) = cfg.sasl.as_mut() {
            sasl.mechanism = "SCRAM-SHA-256".into();
        }
        assert!(matches!(
            amqp_uri(&cfg, Some("p")),
            Err(BrokerError::Connect(_))
        ));
        let bad = config("http://h", false, false);
        assert!(matches!(amqp_uri(&bad, None), Err(BrokerError::Connect(_))));
    }

    #[test]
    fn amqp_binding_key_wildcards() {
        assert_eq!(amqp_binding_key("orders.{id}"), "orders.*");
        assert_eq!(amqp_binding_key("{a}.x.{b}"), "*.x.*");
        assert_eq!(amqp_binding_key("orders.created"), "orders.created");
        assert_eq!(amqp_binding_key("orders.#"), "orders.#");
    }
}
