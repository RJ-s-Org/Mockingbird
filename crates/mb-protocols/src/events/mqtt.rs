//! The MQTT 5 broker client (feature `mqtt`), built on rumqttc.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use mb_mcm::ChannelBinding;
use rumqttc::tokio_rustls::rustls;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{Packet, Publish, PublishProperties};
use rumqttc::v5::{AsyncClient, Event, EventLoop, MqttOptions};
use rumqttc::{TlsConfiguration, Transport};

use super::address::param_names;
use super::broker::{BrokerClient, BrokerConfig, BrokerError, MessageStream};
use super::{InMessage, MAX_MESSAGE_BYTES, OutMessage};

/// How long the first connection may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The pause before polling again after a connection error.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const KEEP_ALIVE: Duration = Duration::from_secs(30);
/// Queued requests between the client handle and the event loop.
const REQUEST_CAPACITY: usize = 64;
/// The largest packet accepted: room for an over-limit message to arrive
/// and be recorded as such.
const MAX_PACKET_BYTES: u32 = 2 * MAX_MESSAGE_BYTES as u32;
/// The header carrying MQTT 5 correlation data.
const HEADER_CORRELATION: &str = "correlation_id";
/// The header carrying an MQTT 5 response topic.
const HEADER_RESPONSE_TOPIC: &str = "response_topic";
/// Where system CA bundles live when `SSL_CERT_FILE` is unset.
const SYSTEM_CA_BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
];

/// A live subscription: its MQTT filter and where to deliver.
struct Subscription {
    filter: String,
    qos: QoS,
    tx: UnboundedSender<Result<InMessage, BrokerError>>,
}

type Subscriptions = Arc<Mutex<Vec<Subscription>>>;

fn lock(subs: &Mutex<Vec<Subscription>>) -> MutexGuard<'_, Vec<Subscription>> {
    subs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A connection to an MQTT 5 broker. One task polls the event loop and
/// fans received messages out to the subscriptions; it is aborted when
/// the client drops.
pub struct MqttClient {
    client: AsyncClient,
    subs: Subscriptions,
    event_loop: tokio::task::JoinHandle<()>,
}

impl Drop for MqttClient {
    fn drop(&mut self) {
        self.event_loop.abort();
    }
}

/// `topic_template` with every segment holding a `{p}` placeholder
/// replaced by the single-level wildcard `+`.
pub(crate) fn mqtt_filter(topic_template: &str) -> String {
    topic_template
        .split('/')
        .map(|segment| {
            if param_names(segment).is_empty() {
                segment
            } else {
                "+"
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether `topic` matches the MQTT topic `filter` (`+` one level, `#`
/// the remaining levels).
fn filter_matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && (filter.starts_with('+') || filter.starts_with('#')) {
        return false;
    }
    let mut levels = topic.split('/');
    for part in filter.split('/') {
        if part == "#" {
            return true;
        }
        match levels.next() {
            Some(level) if part == "+" || part == level => {}
            _ => return false,
        }
    }
    levels.next().is_none()
}

fn qos(level: u8) -> QoS {
    match level {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        _ => QoS::ExactlyOnce,
    }
}

/// `(host, port, tls)` from `mqtt://host[:1883]` or `mqtts://host[:8883]`.
fn parse_url(url: &str) -> Result<(String, u16, bool), BrokerError> {
    let (rest, tls, default_port) = if let Some(rest) = url.strip_prefix("mqtt://") {
        (rest, false, 1883)
    } else if let Some(rest) = url.strip_prefix("mqtts://") {
        (rest, true, 8883)
    } else {
        return Err(BrokerError::Connect(
            "mqtt url must start with mqtt:// or mqtts://".into(),
        ));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    // `[v6]:port` keeps its colons inside the brackets.
    let (host, port) = match host_port.strip_prefix('[') {
        Some(bracketed) => match bracketed.split_once(']') {
            Some((host, tail)) => (host, tail.strip_prefix(':')),
            None => return Err(BrokerError::Connect(format!("invalid host in {url}"))),
        },
        None => match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        },
    };
    let port = match port {
        Some(port) => port
            .parse::<u16>()
            .map_err(|_| BrokerError::Connect(format!("invalid port in {url}")))?,
        None => default_port,
    };
    if host.is_empty() {
        return Err(BrokerError::Connect(format!("no host in {url}")));
    }
    Ok((host.to_string(), port, tls))
}

/// The CA certificates of `ca_file`, or of the system bundle.
fn root_store(ca_file: Option<&Path>) -> Result<rustls::RootCertStore, BrokerError> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;

    let path = match ca_file {
        Some(path) => path.to_path_buf(),
        None => std::env::var_os("SSL_CERT_FILE")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                SYSTEM_CA_BUNDLES
                    .iter()
                    .map(Path::new)
                    .find(|p| p.is_file())
                    .map(Path::to_path_buf)
            })
            .ok_or_else(|| BrokerError::Connect("no system CA bundle found; set ca_file".into()))?,
    };
    let pem = std::fs::read(&path)
        .map_err(|e| BrokerError::Connect(format!("ca_file {}: {e}", path.display())))?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(&pem) {
        let cert =
            cert.map_err(|e| BrokerError::Connect(format!("ca_file {}: {e}", path.display())))?;
        roots
            .add(cert)
            .map_err(|e| BrokerError::Connect(format!("ca_file {}: {e}", path.display())))?;
    }
    if roots.is_empty() {
        return Err(BrokerError::Connect(format!(
            "ca_file {}: no certificates",
            path.display()
        )));
    }
    Ok(roots)
}

/// A rustls client config with an explicit crypto provider.
fn tls_config(ca_file: Option<&Path>) -> Result<rustls::ClientConfig, BrokerError> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| BrokerError::Connect(format!("tls: {e}")))?
        .with_root_certificates(root_store(ca_file)?)
        .with_no_client_auth())
}

fn options(cfg: &BrokerConfig, password: Option<String>) -> Result<MqttOptions, BrokerError> {
    let (host, port, mqtts) = parse_url(&cfg.url)?;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let client_id = format!("{}-{}", cfg.client_id_prefix, &suffix[..8]);
    let mut options = MqttOptions::new(client_id, host, port);
    options
        .set_keep_alive(KEEP_ALIVE)
        .set_clean_start(true)
        .set_max_packet_size(Some(MAX_PACKET_BYTES));
    if let Some(sasl) = &cfg.sasl {
        if !sasl.mechanism.eq_ignore_ascii_case("PLAIN") {
            return Err(BrokerError::Connect(
                "mqtt supports only PLAIN credentials".into(),
            ));
        }
        options.set_credentials(sasl.username.clone(), password.unwrap_or_default());
    }
    if cfg.tls || mqtts {
        let config = tls_config(cfg.ca_file.as_deref())?;
        options.set_transport(Transport::Tls(TlsConfiguration::Rustls(Arc::new(config))));
    }
    Ok(options)
}

/// The message a received publish carries.
fn in_message(publish: &Publish) -> InMessage {
    let mut headers = Vec::new();
    if let Some(props) = &publish.properties {
        headers.extend(props.user_properties.iter().cloned());
        if let Some(data) = &props.correlation_data {
            headers.push((
                HEADER_CORRELATION.into(),
                String::from_utf8_lossy(data).into_owned(),
            ));
        }
        if let Some(topic) = &props.response_topic {
            headers.push((HEADER_RESPONSE_TOPIC.into(), topic.clone()));
        }
    }
    InMessage {
        address: String::from_utf8_lossy(&publish.topic).into_owned(),
        key: None,
        headers,
        payload: publish.payload.to_vec(),
    }
}

/// Deliver `publish` to every live subscription whose filter matches,
/// dropping subscriptions whose stream is gone.
fn fan_out(subs: &Mutex<Vec<Subscription>>, publish: &Publish) {
    let msg = in_message(publish);
    lock(subs).retain(|sub| {
        if sub.tx.is_closed() {
            return false;
        }
        if !filter_matches(&sub.filter, &msg.address) {
            return true;
        }
        sub.tx.unbounded_send(Ok(msg.clone())).is_ok()
    });
}

/// Subscribe again to every live filter after a reconnect (the session
/// starts clean).
fn resubscribe(client: &AsyncClient, subs: &Mutex<Vec<Subscription>>) {
    let mut filters: Vec<(String, QoS)> = lock(subs)
        .iter()
        .filter(|sub| !sub.tx.is_closed())
        .map(|sub| (sub.filter.clone(), sub.qos))
        .collect();
    filters.sort_by(|a, b| a.0.cmp(&b.0));
    filters.dedup_by(|a, b| a.0 == b.0);
    for (filter, qos) in filters {
        if let Err(e) = client.try_subscribe(filter.clone(), qos) {
            tracing::warn!(filter = %filter, error = %e, "mqtt resubscribe failed");
        }
    }
}

/// Poll the event loop until aborted, reconnecting after errors.
async fn run_event_loop(mut event_loop: EventLoop, client: AsyncClient, subs: Subscriptions) {
    loop {
        match event_loop.poll().await {
            Ok(Event::Incoming(Packet::Publish(publish))) => fan_out(&subs, &publish),
            Ok(Event::Incoming(Packet::ConnAck(_))) => resubscribe(&client, &subs),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "mqtt connection error; reconnecting");
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

/// Poll `event_loop` until the broker acknowledges the connection.
async fn first_connack(event_loop: &mut EventLoop) -> Result<(), BrokerError> {
    loop {
        match event_loop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(BrokerError::Connect(e.to_string())),
        }
    }
}

impl MqttClient {
    /// Connect to `cfg.url`, waiting for the broker's `ConnAck`.
    pub async fn connect(
        cfg: &BrokerConfig,
        password: Option<String>,
    ) -> Result<Self, BrokerError> {
        let options = options(cfg, password)?;
        let (client, mut event_loop) = AsyncClient::new(options, REQUEST_CAPACITY);
        match tokio::time::timeout(CONNECT_TIMEOUT, first_connack(&mut event_loop)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(BrokerError::Connect(format!(
                    "timed out after {}s",
                    CONNECT_TIMEOUT.as_secs()
                )));
            }
        }
        let subs: Subscriptions = Arc::new(Mutex::new(Vec::new()));
        let event_loop = tokio::spawn(run_event_loop(
            event_loop,
            client.clone(),
            Arc::clone(&subs),
        ));
        Ok(Self {
            client,
            subs,
            event_loop,
        })
    }
}

fn mqtt_binding(binding: &ChannelBinding) -> Option<(&str, u8, bool)> {
    match binding {
        ChannelBinding::Mqtt { topic, qos, retain } => Some((topic, *qos, *retain)),
        _ => None,
    }
}

#[async_trait]
impl BrokerClient for MqttClient {
    async fn publish(&self, target: &ChannelBinding, msg: OutMessage) -> Result<(), BrokerError> {
        let Some((topic, level, retain)) = mqtt_binding(target) else {
            return Err(BrokerError::Unsupported(
                "an mqtt broker serves only mqtt bindings".into(),
            ));
        };
        let mut properties = PublishProperties::default();
        for (name, value) in msg.headers {
            if name == HEADER_CORRELATION {
                properties.correlation_data = Some(value.into_bytes().into());
            } else {
                properties.user_properties.push((name, value));
            }
        }
        if !msg.content_type.is_empty() {
            properties.content_type = Some(msg.content_type);
        }
        self.client
            .publish_with_properties(topic, qos(level), retain, msg.payload, properties)
            .await
            .map_err(|e| BrokerError::Publish(e.to_string()))
    }

    async fn subscribe(
        &self,
        binding: &ChannelBinding,
        _group: &str,
    ) -> Result<MessageStream, BrokerError> {
        let Some((topic, level, _)) = mqtt_binding(binding) else {
            return Err(BrokerError::Unsupported(
                "an mqtt broker serves only mqtt bindings".into(),
            ));
        };
        let filter = mqtt_filter(topic);
        let qos = qos(level);
        let (tx, rx) = unbounded();
        lock(&self.subs).push(Subscription {
            filter: filter.clone(),
            qos,
            tx,
        });
        self.client
            .subscribe(filter, qos)
            .await
            .map_err(|e| BrokerError::Subscribe(e.to_string()))?;
        Ok(rx.boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mqtt_filter_wildcards() {
        assert_eq!(mqtt_filter("sensors/{id}/t"), "sensors/+/t");
        assert_eq!(mqtt_filter("{a}/{b}"), "+/+");
        assert_eq!(mqtt_filter("plain/topic"), "plain/topic");
    }

    #[test]
    fn filter_matching() {
        assert!(filter_matches("sensors/+/t", "sensors/s1/t"));
        assert!(!filter_matches("sensors/+/t", "sensors/s1/x"));
        assert!(!filter_matches("sensors/+/t", "sensors/s1/t/x"));
        assert!(filter_matches("sensors/#", "sensors/s1/t"));
        assert!(filter_matches("a/b", "a/b"));
        assert!(!filter_matches("a/b", "a"));
        assert!(!filter_matches("+/x", "$SYS/x"));
    }

    #[test]
    fn url_parsing() {
        assert_eq!(
            parse_url("mqtt://h").ok(),
            Some(("h".to_string(), 1883, false))
        );
        assert_eq!(
            parse_url("mqtts://h").ok(),
            Some(("h".to_string(), 8883, true))
        );
        assert_eq!(
            parse_url("mqtt://u:p@127.0.0.1:1999/x").ok(),
            Some(("127.0.0.1".to_string(), 1999, false))
        );
        assert!(parse_url("tcp://h").is_err());
        assert!(parse_url("mqtt://h:x").is_err());
        assert!(parse_url("mqtt://").is_err());
    }
}
