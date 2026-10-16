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
    /// Flush queued messages (default: nothing to flush).
    async fn flush(&self, _timeout: Duration) {}
}

/// Which kind of broker a configured connection talks to.
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
        let cached = lock(&self.clients).get(name).cloned();
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
        let health = lock(&self.health);
        let mut issues: Vec<(String, String)> = health
            .iter()
            .filter_map(|(name, h)| match h {
                Health::Error(e) => Some((name.clone(), e.clone())),
                Health::Ok => None,
            })
            .collect();
        issues.sort();
        issues
    }

    /// Every configured broker with its status, sorted by name.
    pub fn infos(&self) -> Vec<BrokerInfo> {
        let health = lock(&self.health);
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
mod tests {
    use super::*;

    #[test]
    fn strip_userinfo_cases() {
        assert_eq!(strip_userinfo("amqp://u:p@h:5672/%2f"), "amqp://h:5672/%2f");
        assert_eq!(strip_userinfo("mqtt://h:1883"), "mqtt://h:1883");
        assert_eq!(strip_userinfo("mqtts://user@h"), "mqtts://h");
        assert_eq!(strip_userinfo("h1:9092,h2:9092"), "h1:9092,h2:9092");
        assert_eq!(strip_userinfo("amqp://h/v@host"), "amqp://h/v@host");
    }
}
