//! The broker client abstraction and the registry of configured brokers.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use mb_ext::{EnvFileSecrets, SecretStore};
use mb_mcm::ChannelBinding;

use super::memory::MemoryBroker;
use super::{InMessage, OutMessage};

/// How long connecting to a broker may take.
#[cfg_attr(
    not(any(feature = "kafka", feature = "amqp", feature = "mqtt")),
    allow(dead_code)
)]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(thiserror::Error, Debug, Clone)]
pub enum BrokerError {
    #[error("cannot connect: {0}")]
    Connect(String),
    #[error("publish failed: {0}")]
    Publish(String),
    #[error("subscribe failed: {0}")]
    Subscribe(String),
    #[error("{0}")]
    Unsupported(String),
}

/// Messages received on a subscription.
pub type MessageStream = BoxStream<'static, Result<InMessage, BrokerError>>;

/// Messages queued per subscription of the memory and MQTT brokers; past
/// it new messages are dropped, so a slow consumer never grows memory.
pub(crate) const SUBSCRIPTION_QUEUE: usize = 64;

/// The sending side of one subscription's bounded queue. It never blocks:
/// a message that does not fit is dropped and counted in
/// `mb_broker_messages_dropped_total{kind, address}`.
pub(crate) struct SubscriptionSender {
    tx: futures::channel::mpsc::Sender<Result<InMessage, BrokerError>>,
    kind: &'static str,
    template: String,
    /// Whether the last offer was dropped (warn once per run of drops).
    dropping: bool,
}

/// A bounded subscription queue of broker `kind` for `template`.
pub(crate) fn subscription(
    kind: &'static str,
    template: &str,
) -> (SubscriptionSender, MessageStream) {
    use futures::StreamExt;
    // The channel holds `buffer + 1` (one slot per sender).
    let (tx, rx) = futures::channel::mpsc::channel(SUBSCRIPTION_QUEUE - 1);
    let sender = SubscriptionSender {
        tx,
        kind,
        template: template.to_string(),
        dropping: false,
    };
    (sender, rx.boxed())
}

impl SubscriptionSender {
    /// Whether the subscription's stream is gone.
    pub(crate) fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Queue `msg`, dropping it when the queue is full. `false` once the
    /// stream is gone.
    pub(crate) fn offer(&mut self, msg: InMessage) -> bool {
        match self.tx.try_send(Ok(msg)) {
            Ok(()) => {
                self.dropping = false;
                true
            }
            Err(e) if e.is_full() => {
                metrics::counter!(
                    "mb_broker_messages_dropped_total",
                    "kind" => self.kind,
                    "address" => self.template.clone(),
                )
                .increment(1);
                if !self.dropping {
                    tracing::warn!(kind = self.kind, address = %self.template,
                        "consumer is falling behind; dropping messages");
                    self.dropping = true;
                }
                true
            }
            Err(_) => false,
        }
    }
}

/// A connection to one broker.
#[async_trait]
pub trait BrokerClient: Send + Sync {
    /// `target` has its parameters resolved.
    async fn publish(&self, target: &ChannelBinding, msg: OutMessage) -> Result<(), BrokerError>;
    /// `binding` keeps `{param}` placeholders, which match any one segment.
    /// `group` names the consumer group.
    async fn subscribe(
        &self,
        binding: &ChannelBinding,
        group: &str,
    ) -> Result<MessageStream, BrokerError>;
    /// [`BrokerClient::subscribe`] when several nodes consume the same
    /// channel and each message should reach one of them (default: the
    /// consumer group already shares the messages).
    async fn subscribe_shared(
        &self,
        binding: &ChannelBinding,
        group: &str,
    ) -> Result<MessageStream, BrokerError> {
        self.subscribe(binding, group).await
    }
    /// Flush queued messages (default: nothing to flush).
    async fn flush(&self, _timeout: Duration) {}
    /// Whether the client can still be used. A client that cannot (its
    /// connection is gone for good) is dropped and connected again.
    fn is_connected(&self) -> bool {
        true
    }
    /// The client's current connection problem, for a client that
    /// reconnects by itself (default: none).
    fn health(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Which kind of broker a configured connection talks to.
/// Why an MQTT broker cannot use TLS: this build's rumqttc has no TLS
/// stack (its rustls-webpki 0.102 has open advisories).
pub const MQTT_TLS_UNSUPPORTED: &str = "MQTT over TLS is not supported in this build (rumqttc TLS stack has open advisories); use a TLS-terminating proxy or plain MQTT";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrokerKind {
    Kafka,
    Amqp,
    Mqtt,
    /// The in-process [`MemoryBroker`].
    Memory,
}

impl BrokerKind {
    /// `kafka`, `amqp`, `mqtt` or `memory`.
    pub fn as_str(self) -> &'static str {
        match self {
            BrokerKind::Kafka => "kafka",
            BrokerKind::Amqp => "amqp",
            BrokerKind::Mqtt => "mqtt",
            BrokerKind::Memory => "memory",
        }
    }

    /// Whether this kind of broker can carry `binding`: a memory broker
    /// serves every non-WebSocket binding, the others their own protocol.
    pub fn serves(self, binding: &ChannelBinding) -> bool {
        match (self, binding) {
            (_, ChannelBinding::WebSocket { .. }) => false,
            (BrokerKind::Memory, _) => true,
            (BrokerKind::Kafka, ChannelBinding::Kafka { .. })
            | (BrokerKind::Amqp, ChannelBinding::Amqp { .. })
            | (BrokerKind::Mqtt, ChannelBinding::Mqtt { .. }) => true,
            _ => false,
        }
    }

    /// Whether support for this kind is compiled into this build.
    pub fn compiled(self) -> bool {
        match self {
            BrokerKind::Kafka => cfg!(feature = "kafka"),
            BrokerKind::Amqp => cfg!(feature = "amqp"),
            BrokerKind::Mqtt => cfg!(feature = "mqtt"),
            BrokerKind::Memory => true,
        }
    }
}

/// SASL credentials; the password is a secret reference.
#[derive(Clone, Debug, PartialEq)]
pub struct SaslConfig {
    pub mechanism: String,
    pub username: String,
    /// Resolved through the [`SecretStore`] when connecting.
    pub password_ref: String,
}

/// One configured broker connection (the `brokers:` config section).
#[derive(Clone, Debug, PartialEq)]
pub struct BrokerConfig {
    pub name: String,
    pub kind: BrokerKind,
    /// Kafka: the bootstrap servers.
    pub url: String,
    pub tls: bool,
    pub ca_file: Option<PathBuf>,
    pub sasl: Option<SaslConfig>,
    pub client_id_prefix: String,
}

impl BrokerConfig {
    /// `url` with any userinfo (`user:password@`) removed.
    pub fn display_url(&self) -> String {
        strip_userinfo(&self.url)
    }
}

fn strip_userinfo(url: &str) -> String {
    let (scheme, rest) = match url.find("://") {
        Some(i) => url.split_at(i + 3),
        None => ("", url),
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    match authority.rfind('@') {
        Some(at) => format!("{scheme}{}{tail}", &authority[at + 1..]),
        None => url.to_string(),
    }
}

/// A configured broker as the Admin API lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct BrokerInfo {
    pub name: String,
    pub kind: BrokerKind,
    /// Credentials stripped.
    pub url: String,
    /// `ok`, `not connected` or `error: <e>`.
    pub status: String,
}

/// The last reported outcome of a broker.
#[derive(Clone, Debug)]
enum Health {
    Ok,
    Error(String),
}

/// The configured brokers and their connected clients.
pub struct Brokers {
    configs: BTreeMap<String, BrokerConfig>,
    secrets: Arc<dyn SecretStore>,
    /// Connected clients, by broker name; only successes are cached.
    clients: Mutex<HashMap<String, Arc<dyn BrokerClient>>>,
    /// The shared client of each memory broker.
    memory: Mutex<HashMap<String, Arc<MemoryBroker>>>,
    /// Absent until the first outcome is reported.
    health: Mutex<HashMap<String, Health>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Brokers {
    pub fn new(configs: Vec<BrokerConfig>, secrets: Arc<dyn SecretStore>) -> Arc<Self> {
        Arc::new(Self {
            configs: configs.into_iter().map(|c| (c.name.clone(), c)).collect(),
            secrets,
            clients: Mutex::new(HashMap::new()),
            memory: Mutex::new(HashMap::new()),
            health: Mutex::new(HashMap::new()),
        })
    }

    /// No brokers configured.
    pub fn empty() -> Arc<Self> {
        Self::new(Vec::new(), Arc::new(EnvFileSecrets))
    }

    pub fn config(&self, name: &str) -> Option<&BrokerConfig> {
        self.configs.get(name)
    }

    /// The configured broker names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.configs.keys().cloned().collect()
    }

    /// The connected client of broker `name`, connecting on first use.
    /// Every call reports its outcome through [`Brokers::report`].
    pub async fn client(&self, name: &str) -> Result<Arc<dyn BrokerClient>, BrokerError> {
        let Some(config) = self.configs.get(name) else {
            return Err(BrokerError::Connect(format!(
                "broker \"{name}\" is not configured"
            )));
        };
        let cached = {
            let mut clients = lock(&self.clients);
            match clients.get(name) {
                Some(client) if !client.is_connected() => {
                    tracing::warn!(broker = %name, "broker connection lost; reconnecting");
                    clients.remove(name);
                    None
                }
                other => other.cloned(),
            }
        };
        let result = match cached {
            Some(client) => Ok(client),
            None => self.connect(config).await.map(|client| {
                Arc::clone(
                    lock(&self.clients)
                        .entry(name.to_string())
                        .or_insert(client),
                )
            }),
        };
        self.report(name, result.as_ref().map(|_| ()));
        result
    }

    async fn connect(&self, config: &BrokerConfig) -> Result<Arc<dyn BrokerClient>, BrokerError> {
        if !config.kind.compiled() {
            return Err(BrokerError::Unsupported(format!(
                "{} support is not compiled into this build",
                config.kind.as_str()
            )));
        }
        let password = match &config.sasl {
            Some(sasl) => Some(
                self.secrets
                    .resolve(&sasl.password_ref)
                    .await
                    .map_err(|e| BrokerError::Connect(format!("sasl password: {e}")))?,
            ),
            None => None,
        };
        match config.kind {
            BrokerKind::Memory => {
                let _ = password;
                let memory = self.shared_memory(&config.name);
                Ok(memory as Arc<dyn BrokerClient>)
            }
            #[cfg(feature = "kafka")]
            BrokerKind::Kafka => {
                connect_within(super::kafka::KafkaClient::connect(config, password)).await
            }
            #[cfg(feature = "amqp")]
            BrokerKind::Amqp => {
                connect_within(super::amqp::AmqpClient::connect(config, password)).await
            }
            #[cfg(feature = "mqtt")]
            BrokerKind::Mqtt => {
                connect_within(super::mqtt::MqttClient::connect(config, password)).await
            }
            #[allow(unreachable_patterns)]
            other => Err(BrokerError::Unsupported(format!(
                "{} support is not compiled into this build",
                other.as_str()
            ))),
        }
    }

    fn shared_memory(&self, name: &str) -> Arc<MemoryBroker> {
        Arc::clone(lock(&self.memory).entry(name.to_string()).or_default())
    }

    /// The shared memory client of the Memory broker `name` (created on
    /// first use); `None` for other kinds and unknown names.
    pub fn memory(&self, name: &str) -> Option<Arc<MemoryBroker>> {
        match self.configs.get(name)?.kind {
            BrokerKind::Memory => Some(self.shared_memory(name)),
            _ => None,
        }
    }

    /// Record the outcome of a call to broker `name`. Unknown names are
    /// ignored.
    pub fn report(&self, name: &str, outcome: Result<(), &BrokerError>) {
        if !self.configs.contains_key(name) {
            return;
        }
        let health = match outcome {
            Ok(()) => Health::Ok,
            Err(e) => Health::Error(e.to_string()),
        };
        lock(&self.health).insert(name.to_string(), health);
    }

    /// `(broker, last error)` for every broker whose last outcome was an
    /// error, sorted by broker.
    pub fn issues(&self) -> Vec<(String, String)> {
        let mut issues: Vec<(String, String)> = self
            .current_health()
            .into_iter()
            .filter_map(|(name, h)| match h {
                Health::Error(e) => Some((name, e)),
                Health::Ok => None,
            })
            .collect();
        issues.sort();
        issues
    }

    /// The last reported outcome of each broker, overridden by the
    /// current problem of its connected client: a client that reconnects
    /// by itself may lose its broker without any call failing.
    fn current_health(&self) -> HashMap<String, Health> {
        let mut health = lock(&self.health).clone();
        let clients: Vec<(String, Arc<dyn BrokerClient>)> = lock(&self.clients)
            .iter()
            .map(|(name, client)| (name.clone(), Arc::clone(client)))
            .collect();
        for (name, client) in clients {
            if let Err(e) = client.health() {
                health.insert(name, Health::Error(e));
            }
        }
        health
    }

    /// Every configured broker with its status, sorted by name.
    pub fn infos(&self) -> Vec<BrokerInfo> {
        let health = self.current_health();
        self.configs
            .values()
            .map(|config| BrokerInfo {
                name: config.name.clone(),
                kind: config.kind,
                url: config.display_url(),
                status: match health.get(&config.name) {
                    None => "not connected".into(),
                    Some(Health::Ok) => "ok".into(),
                    Some(Health::Error(e)) => format!("error: {e}"),
                },
            })
            .collect()
    }

    /// Flush every connected client, each within `timeout`.
    pub async fn shutdown(&self, timeout: Duration) {
        let clients: Vec<Arc<dyn BrokerClient>> = lock(&self.clients).values().cloned().collect();
        futures::future::join_all(clients.iter().map(|client| async move {
            let _ = tokio::time::timeout(timeout, client.flush(timeout)).await;
        }))
        .await;
    }
}

/// Run a broker connect future under [`CONNECT_TIMEOUT`].
#[cfg(any(feature = "kafka", feature = "amqp", feature = "mqtt"))]
async fn connect_within<C, F>(connect: F) -> Result<Arc<dyn BrokerClient>, BrokerError>
where
    C: BrokerClient + 'static,
    F: std::future::Future<Output = Result<C, BrokerError>>,
{
    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(client)) => Ok(Arc::new(client)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(BrokerError::Connect(format!(
            "timed out after {}s",
            CONNECT_TIMEOUT.as_secs()
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, Ordering};

    /// A client whose connection state the test sets.
    #[derive(Default)]
    struct Fake {
        dead: AtomicBool,
        problem: Mutex<Option<String>>,
    }

    #[async_trait]
    impl BrokerClient for Fake {
        async fn publish(&self, _: &ChannelBinding, _: OutMessage) -> Result<(), BrokerError> {
            Ok(())
        }
        async fn subscribe(
            &self,
            _: &ChannelBinding,
            _: &str,
        ) -> Result<MessageStream, BrokerError> {
            Err(BrokerError::Subscribe("fake".into()))
        }
        fn is_connected(&self) -> bool {
            !self.dead.load(Ordering::SeqCst)
        }
        fn health(&self) -> Result<(), String> {
            match lock(&self.problem).clone() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
    }

    fn memory_brokers() -> Arc<Brokers> {
        Brokers::new(
            vec![BrokerConfig {
                name: "b".into(),
                kind: BrokerKind::Memory,
                url: String::new(),
                tls: false,
                ca_file: None,
                sasl: None,
                client_id_prefix: "mockingbird".into(),
            }],
            Arc::new(EnvFileSecrets),
        )
    }

    #[tokio::test]
    async fn dead_cached_client_is_replaced() {
        let brokers = memory_brokers();
        let fake = Arc::new(Fake::default());
        lock(&brokers.clients).insert("b".into(), Arc::clone(&fake) as Arc<dyn BrokerClient>);
        let client = brokers.client("b").await.unwrap();
        assert!(Arc::ptr_eq(
            &client,
            &(Arc::clone(&fake) as Arc<dyn BrokerClient>)
        ));

        fake.dead.store(true, Ordering::SeqCst);
        let client = brokers.client("b").await.unwrap();
        assert!(!Arc::ptr_eq(
            &client,
            &(Arc::clone(&fake) as Arc<dyn BrokerClient>)
        ));
        assert!(client.is_connected());
    }

    #[tokio::test]
    async fn client_health_reaches_issues_and_infos() {
        let brokers = memory_brokers();
        let fake = Arc::new(Fake::default());
        lock(&brokers.clients).insert("b".into(), Arc::clone(&fake) as Arc<dyn BrokerClient>);
        brokers.client("b").await.unwrap();
        assert!(brokers.issues().is_empty());
        assert_eq!(brokers.infos()[0].status, "ok");

        *lock(&fake.problem) = Some("connection refused".into());
        assert_eq!(
            brokers.issues(),
            vec![("b".to_string(), "connection refused".to_string())]
        );
        assert_eq!(brokers.infos()[0].status, "error: connection refused");

        *lock(&fake.problem) = None;
        assert!(brokers.issues().is_empty());
    }

    #[test]
    fn strip_userinfo_cases() {
        assert_eq!(strip_userinfo("amqp://u:p@h:5672/%2f"), "amqp://h:5672/%2f");
        assert_eq!(strip_userinfo("mqtt://h:1883"), "mqtt://h:1883");
        assert_eq!(strip_userinfo("mqtts://user@h"), "mqtts://h");
        assert_eq!(strip_userinfo("h1:9092,h2:9092"), "h1:9092,h2:9092");
        assert_eq!(strip_userinfo("amqp://h/v@host"), "amqp://h/v@host");
    }
}
