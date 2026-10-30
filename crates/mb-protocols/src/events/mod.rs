//! Event channels (spec §5, §7.3): the broker abstraction, the in-memory
//! broker, the registry of configured broker connections and channel
//! address handling, and the per-mock channel runtime.

mod address;
#[cfg(feature = "amqp")]
mod amqp;
mod broker;
mod consume;
#[cfg(feature = "kafka")]
mod kafka;
mod memory;
#[cfg(feature = "mqtt")]
mod mqtt;
mod produce;
mod runtime;
mod triggers;
mod ws;

pub use address::{address_matches, param_names, param_values, resolve_binding, sanitize_param};
#[cfg(feature = "amqp")]
pub use amqp::AmqpClient;
pub use broker::{
    BrokerClient, BrokerConfig, BrokerError, BrokerInfo, BrokerKind, Brokers, MQTT_TLS_UNSUPPORTED,
    MessageStream, SaslConfig,
};
pub use consume::consumer_group;
#[cfg(feature = "kafka")]
pub use kafka::KafkaClient;
pub use memory::MemoryBroker;
#[cfg(feature = "mqtt")]
pub use mqtt::MqttClient;
pub use runtime::{
    ChannelActivity, ChannelMode, ChannelRuntime, ChannelSetup, EventDeps, EventSetup,
    PublishError, PublishFailure, Published, WS_PUBLISH_ON_DATA_NODES,
};
pub use triggers::{AlwaysLeader, LEASE_RENEW, LEASE_TTL, LeaseProvider};
pub use ws::CLOSE_RUNTIME_REPLACED;

/// Largest message payload produced or consumed (1 MiB).
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// No reply is sent to a message whose hop count is at least this.
pub const MAX_HOPS: u32 = 3;
/// Header naming how a produced payload was made (`llm|patched|repaired|fallback`).
pub const HEADER_SOURCE: &str = "x-mockingbird-source";
/// Header naming the mock that produced a message.
pub const HEADER_MOCK: &str = "x-mockingbird-mock";
/// Header carrying the reply hop count (decimal).
pub const HEADER_HOP: &str = "x-mockingbird-hop";

/// A message to publish.
#[derive(Clone, Debug, PartialEq)]
pub struct OutMessage {
    pub key: Option<Vec<u8>>,
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
    pub content_type: String,
}

/// A message received from a broker.
#[derive(Clone, Debug, PartialEq)]
pub struct InMessage {
    /// The concrete address it arrived on (parameters resolved).
    pub address: String,
    pub key: Option<Vec<u8>>,
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
}
