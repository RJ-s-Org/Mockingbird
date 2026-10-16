//! Cross-node change bus.
//!
//! Nodes announce *what* changed (ids only); receivers reload from the stores.
//! A bus never delivers a node's own events back to it. After a lost
//! connection is re-established, a bus delivers [`BusEvent::Resync`] locally,
//! because notifications sent while it was away are gone: receivers then
//! reload everything.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgConnectOptions, PgListener, PgPool, PgPoolOptions};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::{CHANGE_CHANNEL_CAPACITY, StateError};

/// A change announced to the other nodes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BusEvent {
    Mock {
        workspace: String,
        name: String,
    },
    Profile {
        workspace: String,
        name: String,
    },
    Workspace {
        name: String,
    },
    State {
        workspace: String,
        mock: String,
        session: String,
        resource_type: String,
        id: String,
        deleted: bool,
    },
    RequestLogged {
        id: i64,
    },
    /// Delivered locally after a reconnect, or sent instead of an event over MAX_BUS_PAYLOAD: receivers reload everything.
    Resync,
}

impl BusEvent {
    /// The event's wire `type`, also used as the metric label.
    pub fn type_name(&self) -> &'static str {
        match self {
            BusEvent::Mock { .. } => "mock",
            BusEvent::Profile { .. } => "profile",
            BusEvent::Workspace { .. } => "workspace",
            BusEvent::State { .. } => "state",
            BusEvent::RequestLogged { .. } => "request_logged",
            BusEvent::Resync => "resync",
        }
    }
}

/// What travels on the wire: the event and the node that sent it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BusEnvelope {
    pub origin: String,
    pub event: BusEvent,
}

/// Largest encoded envelope sent as-is (below Postgres' 8000-byte NOTIFY
/// limit). A larger event is sent as [`BusEvent::Resync`].
pub const MAX_BUS_PAYLOAD: usize = 7900;
pub const PG_BUS_CHANNEL: &str = "mockingbird_bus";
pub const REDIS_BUS_CHANNEL: &str = "mockingbird:bus";

/// First reconnect delay; doubles on each failure up to [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Bound on connecting and on each publish.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

const EVENTS_METRIC: &str = "mb_bus_events_total";
const CONNECTED_METRIC: &str = "mb_bus_connected";

#[async_trait]
pub trait Bus: Send + Sync {
    /// Announces `event` to the other nodes. Never delivered back to this node.
    async fn publish(&self, event: BusEvent) -> Result<(), StateError>;
    /// Events from other nodes, plus a local [`BusEvent::Resync`] after a reconnect.
    fn subscribe(&self) -> broadcast::Receiver<BusEvent>;
    /// `Err(reason)` while the bus is disconnected.
    fn health(&self) -> Result<(), String>;
    fn node_id(&self) -> &str;
    /// `true` for the in-process bus of a single node.
    fn is_local(&self) -> bool;
}

/// Encodes `event` from `origin`, substituting [`BusEvent::Resync`] when the
/// envelope would exceed [`MAX_BUS_PAYLOAD`]. Returns the payload and the
/// event actually sent.
fn encode(origin: &str, event: BusEvent) -> Result<(String, BusEvent), StateError> {
    let mut env = BusEnvelope {
        origin: origin.to_owned(),
        event,
    };
    let mut payload = serde_json::to_string(&env)
        .map_err(|e| StateError::Backend(format!("bus: encode event: {e}")))?;
    if payload.len() > MAX_BUS_PAYLOAD {
        tracing::warn!(
            r#type = env.event.type_name(),
            bytes = payload.len(),
            "bus event over the payload limit; sending resync instead"
        );
        env.event = BusEvent::Resync;
        payload = serde_json::to_string(&env)
            .map_err(|e| StateError::Backend(format!("bus: encode event: {e}")))?;
    }
    Ok((payload, env.event))
}

fn count(direction: &'static str, event: &BusEvent) {
    metrics::counter!(EVENTS_METRIC, "direction" => direction, "type" => event.type_name())
        .increment(1);
}

/// The in-process bus of a single node: publishing is a no-op and nothing is
/// ever received.
pub struct LocalBus {
    node_id: String,
    tx: broadcast::Sender<BusEvent>,
}

impl LocalBus {
    pub fn new(node_id: String) -> Arc<Self> {
        let (tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Arc::new(Self { node_id, tx })
    }
}

#[async_trait]
impl Bus for LocalBus {
    async fn publish(&self, _event: BusEvent) -> Result<(), StateError> {
        Ok(())
    }
    fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }
    fn health(&self) -> Result<(), String> {
        Ok(())
    }
    fn node_id(&self) -> &str {
        &self.node_id
    }
    fn is_local(&self) -> bool {
        true
    }
}

/// An in-process hub joining several simulated nodes (for tests). Payloads go
/// through the same encoding and size rule as the real buses.
pub struct MemoryBusHub {
    nodes: Mutex<Vec<(String, broadcast::Sender<BusEvent>)>>,
}

impl MemoryBusHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            nodes: Mutex::new(Vec::new()),
        })
    }

    pub fn join(self: &Arc<Self>, node_id: String) -> Arc<dyn Bus> {
        let (tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        self.nodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((node_id.clone(), tx.clone()));
        Arc::new(MemoryBus {
            hub: Arc::clone(self),
            node_id,
            tx,
        })
    }

    fn deliver(&self, payload: &str) -> Result<(), StateError> {
        let env: BusEnvelope = serde_json::from_str(payload)
            .map_err(|e| StateError::Backend(format!("bus: decode event: {e}")))?;
        let nodes = self.nodes.lock().unwrap_or_else(PoisonError::into_inner);
        for (id, tx) in nodes.iter().filter(|(id, _)| *id != env.origin) {
            tracing::trace!(node = %id, "memory bus delivery");
            // No subscribers is fine.
            let _ = tx.send(env.event.clone());
        }
        Ok(())
    }
}

struct MemoryBus {
    hub: Arc<MemoryBusHub>,
    node_id: String,
    tx: broadcast::Sender<BusEvent>,
}

#[async_trait]
impl Bus for MemoryBus {
    async fn publish(&self, event: BusEvent) -> Result<(), StateError> {
        let (payload, _) = encode(&self.node_id, event)?;
        self.hub.deliver(&payload)
    }
    fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }
    fn health(&self) -> Result<(), String> {
        Ok(())
    }
    fn node_id(&self) -> &str {
        &self.node_id
    }
    fn is_local(&self) -> bool {
        false
    }
}

/// State shared between a networked bus and its receive task.
#[derive(Clone)]
struct Receiver {
    node_id: String,
    tx: broadcast::Sender<BusEvent>,
    health: Arc<Mutex<Result<(), String>>>,
}

impl Receiver {
    fn new(node_id: String) -> Self {
        let (tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        metrics::gauge!(CONNECTED_METRIC).set(1.0);
        Self {
            node_id,
            tx,
            health: Arc::new(Mutex::new(Ok(()))),
        }
    }

    fn health(&self) -> Result<(), String> {
        self.health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_health(&self, h: Result<(), String>) {
        metrics::gauge!(CONNECTED_METRIC).set(if h.is_ok() { 1.0 } else { 0.0 });
        *self.health.lock().unwrap_or_else(PoisonError::into_inner) = h;
    }

    /// Decodes a payload from the wire and hands it to subscribers unless this
    /// node sent it.
    fn deliver(&self, payload: &str) {
        let env: BusEnvelope = match serde_json::from_str(payload) {
            Ok(env) => env,
            Err(e) => {
                tracing::warn!(error = %e, "ignoring undecodable bus payload");
                return;
            }
        };
        if env.origin == self.node_id {
            return;
        }
        count("received", &env.event);
        // No subscribers is fine.
        let _ = self.tx.send(env.event);
    }

    fn lost(&self, reason: String) {
        tracing::warn!(%reason, "bus connection lost; reconnecting");
        self.set_health(Err(reason));
    }

    fn reconnected(&self) {
        tracing::info!("bus reconnected; requesting resync");
        self.set_health(Ok(()));
        let _ = self.tx.send(BusEvent::Resync);
    }

    /// Calls `connect` with backoff (`initial`, doubling to [`MAX_BACKOFF`])
    /// until it succeeds, then reports the reconnect.
    async fn reconnect<T, F, Fut>(&self, initial: Duration, mut connect: F) -> T
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        let mut backoff = initial;
        loop {
            tokio::time::sleep(backoff).await;
            match connect().await {
                Ok(c) => {
                    self.reconnected();
                    return c;
                }
                Err(e) => {
                    tracing::warn!(error = %e, retry_in = ?backoff, "bus reconnect failed");
                    self.set_health(Err(e));
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
}

async fn with_timeout<T, E: std::fmt::Display>(
    what: &str,
    fut: impl Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(IO_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{what}: {e}")),
        Err(_) => Err(format!("{what}: timed out")),
    }
}

/// The bus over Postgres `LISTEN/NOTIFY` on [`PG_BUS_CHANNEL`].
pub struct PgBus {
    recv: Receiver,
    pool: PgPool,
    task: JoinHandle<()>,
}

impl PgBus {
    pub async fn connect(url: &str, node_id: String) -> Result<Arc<Self>, StateError> {
        Self::connect_with_backoff(url, node_id, INITIAL_BACKOFF).await
    }

    /// [`PgBus::connect`] with a custom first reconnect delay (tests).
    #[doc(hidden)]
    pub async fn connect_with_backoff(
        url: &str,
        node_id: String,
        initial_backoff: Duration,
    ) -> Result<Arc<Self>, StateError> {
        let options: PgConnectOptions = url
            .parse()
            .map_err(|e| StateError::Backend(format!("bus: postgres url: {e}")))?;
        let options = options.application_name(&format!("mockingbird-bus:{node_id}"));
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(IO_TIMEOUT)
            .connect_lazy_with(options.clone());
        let listener = pg_listen(&options).await.map_err(StateError::Backend)?;
        let recv = Receiver::new(node_id);
        let task = tokio::spawn(pg_receive(recv.clone(), options, listener, initial_backoff));
        Ok(Arc::new(Self { recv, pool, task }))
    }
}

async fn pg_listen(options: &PgConnectOptions) -> Result<PgListener, String> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(IO_TIMEOUT)
        .max_lifetime(None)
        .idle_timeout(None)
        .connect_lazy_with(options.clone());
    let mut listener = with_timeout("postgres listen", PgListener::connect_with(&pool)).await?;
    with_timeout("postgres listen", listener.listen(PG_BUS_CHANNEL)).await?;
    Ok(listener)
}

async fn pg_receive(
    recv: Receiver,
    options: PgConnectOptions,
    mut listener: PgListener,
    initial_backoff: Duration,
) {
    loop {
        let reason = match listener.try_recv().await {
            Ok(Some(n)) => {
                recv.deliver(n.payload());
                continue;
            }
            Ok(None) => "postgres: listener connection lost".to_owned(),
            Err(e) => format!("postgres: {e}"),
        };
        drop(listener);
        recv.lost(reason);
        listener = recv
            .reconnect(initial_backoff, || pg_listen(&options))
            .await;
    }
}

impl Drop for PgBus {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait]
impl Bus for PgBus {
    async fn publish(&self, event: BusEvent) -> Result<(), StateError> {
        let (payload, sent) = encode(&self.recv.node_id, event)?;
        with_timeout(
            "postgres notify",
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(PG_BUS_CHANNEL)
                .bind(&payload)
                .execute(&self.pool),
        )
        .await
        .map_err(|e| StateError::Backend(format!("bus: {e}")))?;
        count("sent", &sent);
        Ok(())
    }
    fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.recv.tx.subscribe()
    }
    fn health(&self) -> Result<(), String> {
        self.recv.health()
    }
    fn node_id(&self) -> &str {
        &self.recv.node_id
    }
    fn is_local(&self) -> bool {
        false
    }
}

/// The bus over Redis pub/sub on [`REDIS_BUS_CHANNEL`].
pub struct RedisBus {
    recv: Receiver,
    con: ConnectionManager,
    task: JoinHandle<()>,
}

impl RedisBus {
    pub async fn connect(url: &str, node_id: String) -> Result<Arc<Self>, StateError> {
        Self::connect_with_backoff(url, node_id, INITIAL_BACKOFF).await
    }

    /// [`RedisBus::connect`] with a custom first reconnect delay (tests).
    #[doc(hidden)]
    pub async fn connect_with_backoff(
        url: &str,
        node_id: String,
        initial_backoff: Duration,
    ) -> Result<Arc<Self>, StateError> {
        if !(url.starts_with("redis://") || url.starts_with("rediss://")) {
            return Err(StateError::Backend(
                "bus: redis url must start with redis:// or rediss://".into(),
            ));
        }
        let client = redis::Client::open(url)?;
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(IO_TIMEOUT))
            .set_response_timeout(Some(IO_TIMEOUT))
            .set_number_of_retries(2);
        let con = client.get_connection_manager_with_config(config).await?;
        let stream = redis_subscribe(&client)
            .await
            .map_err(StateError::Backend)?;
        let recv = Receiver::new(node_id);
        let task = tokio::spawn(redis_receive(recv.clone(), client, stream, initial_backoff));
        Ok(Arc::new(Self { recv, con, task }))
    }
}

async fn redis_subscribe(client: &redis::Client) -> Result<redis::aio::PubSubStream, String> {
    let mut pubsub = with_timeout("redis subscribe", client.get_async_pubsub()).await?;
    with_timeout("redis subscribe", pubsub.subscribe(REDIS_BUS_CHANNEL)).await?;
    Ok(pubsub.into_on_message())
}

async fn redis_receive(
    recv: Receiver,
    client: redis::Client,
    mut stream: redis::aio::PubSubStream,
    initial_backoff: Duration,
) {
    loop {
        match stream.next().await {
            Some(msg) => match msg.get_payload::<String>() {
                Ok(payload) => recv.deliver(&payload),
                Err(e) => tracing::warn!(error = %e, "ignoring non-text bus payload"),
            },
            None => {
                drop(stream);
                recv.lost("redis: pub/sub connection lost".into());
                stream = recv
                    .reconnect(initial_backoff, || redis_subscribe(&client))
                    .await;
            }
        }
    }
}

impl Drop for RedisBus {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait]
impl Bus for RedisBus {
    async fn publish(&self, event: BusEvent) -> Result<(), StateError> {
        let (payload, sent) = encode(&self.recv.node_id, event)?;
        let mut con = self.con.clone();
        let _: i64 = redis::cmd("PUBLISH")
            .arg(REDIS_BUS_CHANNEL)
            .arg(payload)
            .query_async(&mut con)
            .await?;
        count("sent", &sent);
        Ok(())
    }
    fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.recv.tx.subscribe()
    }
    fn health(&self) -> Result<(), String> {
        self.recv.health()
    }
    fn node_id(&self) -> &str {
        &self.recv.node_id
    }
    fn is_local(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_event_is_sent_as_is() {
        let ev = BusEvent::RequestLogged { id: 1 };
        let (payload, sent) = encode("n", ev.clone()).expect("encode");
        assert_eq!(sent, ev);
        assert!(payload.len() <= MAX_BUS_PAYLOAD);
    }

    #[test]
    fn oversized_event_is_encoded_as_resync() {
        let ev = BusEvent::Workspace {
            name: "x".repeat(MAX_BUS_PAYLOAD),
        };
        let (payload, sent) = encode("n", ev).expect("encode");
        assert_eq!(sent, BusEvent::Resync);
        let env: BusEnvelope = serde_json::from_str(&payload).expect("decode");
        assert_eq!(env.origin, "n");
        assert_eq!(env.event, BusEvent::Resync);
    }
}
