//! The MQTT 5 broker client (feature `mqtt`), built on rumqttc.
//!
//! rumqttc is built without its TLS features: its rustls stack depends on
//! rustls-webpki 0.102, which has open advisories. MQTT brokers are reached
//! over plain TCP only; `mqtts://` URLs, `tls` and `ca_file` are refused
//! (use a TLS-terminating proxy in front of the broker).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use mb_mcm::ChannelBinding;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{
    ConnAck, Packet, Publish, PublishProperties, SubscribeProperties,
};
use rumqttc::v5::{AsyncClient, Event, EventLoop, MqttOptions};

use super::address::param_names;
use super::broker::{
    BrokerClient, BrokerConfig, BrokerError, MQTT_TLS_UNSUPPORTED, MessageStream,
    SubscriptionSender, subscription,
};
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
/// A live subscription: its MQTT filter, the filter it was subscribed
/// with (a shared subscription's `$share/...` form), its MQTT 5
/// subscription identifier and where to deliver.
struct Subscription {
    filter: String,
    subscribed: String,
    /// Shared by every subscription with the same `subscribed` filter;
    /// `None` when the broker has no subscription identifiers.
    id: Option<usize>,
    qos: QoS,
    tx: SubscriptionSender,
}

/// The largest MQTT 5 subscription identifier (a variable byte integer).
const MAX_SUBSCRIPTION_ID: usize = 268_435_455;

type Subscriptions = Arc<Mutex<Vec<Subscription>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The connection state the event loop shares with the client handle.
#[derive(Default)]
struct Link {
    /// The last connection error, until the broker acknowledges a
    /// reconnect.
    problem: Mutex<Option<String>>,
    /// Set once `flush` has asked the event loop to disconnect.
    closing: AtomicBool,
    /// Set once the event loop has stopped.
    closed: tokio::sync::watch::Sender<bool>,
}

impl Link {
    fn problem(&self) -> Option<String> {
        lock(&self.problem).clone()
    }

    fn set_problem(&self, problem: Option<String>) {
        *lock(&self.problem) = problem;
    }
}

/// A connection to an MQTT 5 broker. One task polls the event loop and
/// fans received messages out to the subscriptions; it is aborted when
/// the client drops.
pub struct MqttClient {
    client: AsyncClient,
    /// Whether the broker supports subscription identifiers, which tell
    /// apart the copies of one message sent for different share groups.
    sub_ids: bool,
    subs: Subscriptions,
    link: Arc<Link>,
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

/// The MQTT 5 shared subscription of `filter` in `group`:
/// `$share/<group>/<filter>`; the broker delivers each message to one
/// member of the group.
pub(crate) fn shared_filter(group: &str, filter: &str) -> String {
    format!("$share/{group}/{filter}")
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

/// `(host, port)` from `mqtt://host[:1883]`; `mqtts://` and `ssl://` are
/// refused (see [`MQTT_TLS_UNSUPPORTED`]).
fn parse_url(url: &str) -> Result<(String, u16), BrokerError> {
    let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
    if matches!(scheme.as_deref(), Some("mqtts" | "ssl")) {
        return Err(BrokerError::Connect(MQTT_TLS_UNSUPPORTED.into()));
    }
    let (rest, default_port) = match url.strip_prefix("mqtt://") {
        Some(rest) => (rest, 1883),
        None => {
            return Err(BrokerError::Connect(
                "mqtt url must start with mqtt://".into(),
            ));
        }
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
    Ok((host.to_string(), port))
}

fn options(cfg: &BrokerConfig, password: Option<String>) -> Result<MqttOptions, BrokerError> {
    if cfg.tls || cfg.ca_file.is_some() {
        return Err(BrokerError::Connect(MQTT_TLS_UNSUPPORTED.into()));
    }
    let (host, port) = parse_url(&cfg.url)?;
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

/// Deliver `publish` to every live subscription it was sent for,
/// dropping subscriptions whose stream is gone. A publish carrying
/// subscription identifiers goes to the subscriptions with those ids
/// (each share group gets its own copy); one without goes to every
/// subscription whose filter matches.
fn fan_out(subs: &Mutex<Vec<Subscription>>, publish: &Publish) {
    let msg = in_message(publish);
    let ids = publish
        .properties
        .as_ref()
        .map(|p| p.subscription_identifiers.as_slice())
        .unwrap_or_default();
    lock(subs).retain_mut(|sub| {
        if sub.tx.is_closed() {
            return false;
        }
        let matches = if ids.is_empty() {
            filter_matches(&sub.filter, &msg.address)
        } else {
            sub.id.is_some_and(|id| ids.contains(&id))
        };
        if !matches {
            return true;
        }
        sub.tx.offer(msg.clone())
    });
}

/// The properties to subscribe with: the subscription identifier, if any.
fn subscribe_properties(id: Option<usize>) -> Option<SubscribeProperties> {
    id.map(|id| SubscribeProperties {
        id: Some(id),
        user_properties: Vec::new(),
    })
}

/// Subscribe again to every live filter after a reconnect (the session
/// starts clean).
fn resubscribe(client: &AsyncClient, subs: &Mutex<Vec<Subscription>>) {
    let mut filters: Vec<(String, QoS, Option<usize>)> = lock(subs)
        .iter()
        .filter(|sub| !sub.tx.is_closed())
        .map(|sub| (sub.subscribed.clone(), sub.qos, sub.id))
        .collect();
    filters.sort_by(|a, b| a.0.cmp(&b.0));
    filters.dedup_by(|a, b| a.0 == b.0);
    for (filter, qos, id) in filters {
        let subscribed = match subscribe_properties(id) {
            Some(properties) => {
                client.try_subscribe_with_properties(filter.clone(), qos, properties)
            }
            None => client.try_subscribe(filter.clone(), qos),
        };
        if let Err(e) = subscribed {
            tracing::warn!(filter = %filter, error = %e, "mqtt resubscribe failed");
        }
    }
}

/// Poll the event loop until aborted or disconnected by `flush`,
/// reconnecting after errors. The connection state is kept in `link`.
///
/// After `flush` has sent `Disconnect`, polling goes on until the broker
/// closes the connection: dropping the socket while its acks are still
/// unread makes the kernel reset the connection, and the broker then
/// discards the publishes it has not read yet.
async fn run_event_loop(
    mut event_loop: EventLoop,
    client: AsyncClient,
    subs: Subscriptions,
    link: Arc<Link>,
) {
    loop {
        match event_loop.poll().await {
            Ok(Event::Incoming(Packet::Publish(publish))) => fan_out(&subs, &publish),
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                link.set_problem(None);
                resubscribe(&client, &subs);
            }
            Ok(_) => {}
            Err(e) => {
                if link.closing.load(Ordering::SeqCst) {
                    break;
                }
                tracing::warn!(error = %e, "mqtt connection error; reconnecting");
                link.set_problem(Some(e.to_string()));
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
    link.closed.send_replace(true);
}

/// Whether the broker supports subscription identifiers (an absent
/// property means it does).
fn subscription_ids_available(ack: &ConnAck) -> bool {
    ack.properties
        .as_ref()
        .and_then(|p| p.subscription_identifiers_available)
        != Some(0)
}

/// Poll `event_loop` until the broker acknowledges the connection.
async fn first_connack(event_loop: &mut EventLoop) -> Result<ConnAck, BrokerError> {
    loop {
        match event_loop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(ack))) => return Ok(ack),
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
        let sub_ids =
            match tokio::time::timeout(CONNECT_TIMEOUT, first_connack(&mut event_loop)).await {
                Ok(Ok(ack)) => subscription_ids_available(&ack),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(BrokerError::Connect(format!(
                        "timed out after {}s",
                        CONNECT_TIMEOUT.as_secs()
                    )));
                }
            };
        let subs: Subscriptions = Arc::new(Mutex::new(Vec::new()));
        let link = Arc::new(Link::default());
        let event_loop = tokio::spawn(run_event_loop(
            event_loop,
            client.clone(),
            Arc::clone(&subs),
            Arc::clone(&link),
        ));
        Ok(Self {
            client,
            sub_ids,
            subs,
            link,
            event_loop,
        })
    }
}

impl MqttClient {
    /// Subscribe to `binding`; `group` is the share group of a shared
    /// subscription.
    async fn subscribe_as(
        &self,
        binding: &ChannelBinding,
        group: Option<&str>,
    ) -> Result<MessageStream, BrokerError> {
        let Some((topic, level, _)) = mqtt_binding(binding) else {
            return Err(BrokerError::Unsupported(
                "an mqtt broker serves only mqtt bindings".into(),
            ));
        };
        let filter = mqtt_filter(topic);
        let subscribed = match group {
            Some(group) => shared_filter(group, &filter),
            None => filter.clone(),
        };
        let qos = qos(level);
        let (tx, rx) = subscription("mqtt", &filter);
        let id = {
            let mut subs = lock(&self.subs);
            let id = if self.sub_ids {
                next_subscription_id(&subs, &subscribed)
            } else {
                None
            };
            subs.push(Subscription {
                filter,
                subscribed: subscribed.clone(),
                id,
                qos,
                tx,
            });
            id
        };
        let result = match subscribe_properties(id) {
            Some(properties) => {
                self.client
                    .subscribe_with_properties(subscribed, qos, properties)
                    .await
            }
            None => self.client.subscribe(subscribed, qos).await,
        };
        result.map_err(|e| BrokerError::Subscribe(e.to_string()))?;
        Ok(rx)
    }
}

/// The subscription identifier for `subscribed`: that of a live
/// subscription to the same filter (subscribing again replaces the
/// broker's identifier), else the lowest one not in use. `None` once
/// every identifier is taken.
fn next_subscription_id(subs: &[Subscription], subscribed: &str) -> Option<usize> {
    let live = || subs.iter().filter(|sub| !sub.tx.is_closed());
    if let Some(id) = live()
        .find(|sub| sub.subscribed == subscribed)
        .and_then(|sub| sub.id)
    {
        return Some(id);
    }
    (1..=MAX_SUBSCRIPTION_ID).find(|candidate| live().all(|sub| sub.id != Some(*candidate)))
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
        // rumqttc would queue the message until it reconnects; report the
        // lost connection instead, so the publish is retried and recorded.
        if let Some(problem) = self.link.problem() {
            return Err(BrokerError::Publish(format!("not connected: {problem}")));
        }
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
        self.subscribe_as(binding, None).await
    }

    /// Subscribes to `$share/<group>/<topic>`, so the nodes running the
    /// mock each get some of the messages, while other mocks (other
    /// groups) consuming the topic get them all.
    async fn subscribe_shared(
        &self,
        binding: &ChannelBinding,
        group: &str,
    ) -> Result<MessageStream, BrokerError> {
        self.subscribe_as(binding, Some(group)).await
    }

    /// Send what is queued: a `Disconnect` is queued behind the pending
    /// publishes, then the event loop is awaited until the broker has
    /// read it and closed the connection, all within `timeout`. Nothing
    /// is waited for while disconnected.
    async fn flush(&self, timeout: Duration) {
        if self.link.problem().is_some() || *self.link.closed.borrow() {
            return;
        }
        self.link.closing.store(true, Ordering::SeqCst);
        let mut closed = self.link.closed.subscribe();
        let drained = async {
            if self.client.disconnect().await.is_err() {
                return;
            }
            let _ = closed.wait_for(|closed| *closed).await;
        };
        if tokio::time::timeout(timeout, drained).await.is_err() {
            tracing::warn!("mqtt flush timed out after {}s", timeout.as_secs());
        }
    }

    /// The client reconnects by itself until `flush` disconnects it.
    fn is_connected(&self) -> bool {
        !*self.link.closed.borrow()
    }

    fn health(&self) -> Result<(), String> {
        match self.link.problem() {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
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
    fn mqtt_shared_subscription_topic() {
        assert_eq!(
            shared_filter("mockingbird", &mqtt_filter("sensors/{id}/t")),
            "$share/mockingbird/sensors/+/t"
        );
        assert_eq!(shared_filter("mb", "plain/topic"), "$share/mb/plain/topic");
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

    fn sub(filter: &str, subscribed: &str, id: Option<usize>) -> (Subscription, MessageStream) {
        let (tx, rx) = subscription("mqtt", filter);
        let sub = Subscription {
            filter: filter.into(),
            subscribed: subscribed.into(),
            id,
            qos: QoS::AtLeastOnce,
            tx,
        };
        (sub, rx)
    }

    fn publish_with_ids(topic: &str, ids: Vec<usize>) -> Publish {
        let properties = PublishProperties {
            subscription_identifiers: ids,
            ..Default::default()
        };
        Publish::new(topic, QoS::AtLeastOnce, "{}", Some(properties))
    }

    fn received(stream: &mut MessageStream) -> usize {
        let mut n = 0;
        use futures::{FutureExt, StreamExt};
        while let Some(Some(_)) = stream.next().now_or_never() {
            n += 1;
        }
        n
    }

    /// Two share groups on one connection: the broker sends one copy per
    /// group, tagged with that group's subscription id, and each copy
    /// reaches only its group (MB-PROTOCOLS-STUB-6).
    #[test]
    fn fan_out_follows_subscription_ids() {
        let (a, mut a_rx) = sub("t", "$share/g-a/t", Some(1));
        let (b, mut b_rx) = sub("t", "$share/g-b/t", Some(2));
        let subs = Mutex::new(vec![a, b]);
        fan_out(&subs, &publish_with_ids("t", vec![1]));
        fan_out(&subs, &publish_with_ids("t", vec![2]));
        assert_eq!(received(&mut a_rx), 1);
        assert_eq!(received(&mut b_rx), 1);
        // Without identifiers, the topic filter decides.
        fan_out(&subs, &publish_with_ids("t", Vec::new()));
        assert_eq!(received(&mut a_rx), 1);
        assert_eq!(received(&mut b_rx), 1);
    }

    #[test]
    fn subscription_ids_are_reused_per_filter() {
        let (a, _a_rx) = sub("t", "$share/g-a/t", Some(1));
        let (b, _b_rx) = sub("t", "$share/g-b/t", Some(2));
        let subs = vec![a, b];
        assert_eq!(next_subscription_id(&subs, "$share/g-b/t"), Some(2));
        assert_eq!(next_subscription_id(&subs, "$share/g-c/t"), Some(3));
        let (c, c_rx) = sub("t", "$share/g-c/t", Some(3));
        drop(c_rx);
        let subs = vec![c];
        // A closed subscription frees its id.
        assert_eq!(next_subscription_id(&subs, "x"), Some(1));
    }

    #[test]
    fn url_parsing() {
        assert_eq!(parse_url("mqtt://h").ok(), Some(("h".to_string(), 1883)));
        assert_eq!(
            parse_url("mqtt://u:p@127.0.0.1:1999/x").ok(),
            Some(("127.0.0.1".to_string(), 1999))
        );
        for tls in ["mqtts://h", "ssl://h:8883", "MQTTS://h"] {
            match parse_url(tls) {
                Err(BrokerError::Connect(e)) => assert_eq!(e, MQTT_TLS_UNSUPPORTED),
                other => panic!("{tls}: {other:?}"),
            }
        }
        assert!(parse_url("tcp://h").is_err());
        assert!(parse_url("mqtt://h:x").is_err());
        assert!(parse_url("mqtt://").is_err());
    }
}
