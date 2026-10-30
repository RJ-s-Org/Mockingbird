//! Cross-node change bus.
//!
//! Nodes announce *what* changed (ids only); receivers reload from the stores.
//! A bus never delivers a node's own events back to it. After a lost
//! connection is re-established, a bus delivers [`BusEvent::Resync`] locally,
//! because notifications sent while it was away are gone: receivers then
//! reload everything.
//!
//! A connection that goes half-open (a partition, or a NAT or load balancer
//! dropping an idle flow) never reports EOF, so a networked bus also
//! heartbeats its subscriber connection every [`HEARTBEAT_INTERVAL`] and
//! treats a heartbeat that fails or times out as a lost connection. As a
//! last line of defence it delivers a local `Resync` about every
//! [`SAFETY_RESYNC_INTERVAL`] (jittered), and after a failed publish it
//! announces `Resync` to the other nodes once the bus works again.

use std::fmt;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgConnectOptions, PgListener, PgPool, PgPoolOptions};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{Instant, Interval, MissedTickBehavior};

use crate::{CHANGE_CHANNEL_CAPACITY, StateError};

/// A change announced to the other nodes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BusEvent {
    /// A mock was created, changed or deleted.
    Mock {
        /// The mock's workspace.
        workspace: String,
        /// The mock's name.
        name: String,
    },
    /// An LLM profile was created, changed or deleted.
    Profile {
        /// The profile's workspace.
        workspace: String,
        /// The profile's name.
        name: String,
    },
    /// A workspace was created, changed or deleted.
    Workspace {
        /// The workspace's name.
        name: String,
    },
    /// A stored resource was upserted or deleted.
    State {
        /// The session's workspace.
        workspace: String,
        /// The session's mock.
        mock: String,
        /// The session id.
        session: String,
        /// The resource's type.
        resource_type: String,
        /// The resource's id.
        id: String,
        /// `true` for a delete, `false` for an upsert.
        deleted: bool,
    },
    /// A request-log entry was written.
    RequestLogged {
        /// The entry's id.
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
    /// The node id of the sender.
    pub origin: String,
    /// The event.
    pub event: BusEvent,
}

/// Largest encoded envelope sent as-is (below Postgres' 8000-byte NOTIFY
/// limit). A larger event is sent as [`BusEvent::Resync`].
pub const MAX_BUS_PAYLOAD: usize = 7900;
/// The Postgres `NOTIFY` channel of [`PgBus`].
pub const PG_BUS_CHANNEL: &str = "mockingbird_bus";
/// The Redis pub/sub channel of [`RedisBus`].
pub const REDIS_BUS_CHANNEL: &str = "mockingbird:bus";

/// First reconnect delay; doubles on each failure up to [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Bound on connecting, on each publish and on each heartbeat.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a networked bus checks its subscriber connection (`SELECT 1`
/// on the Postgres listener, `PING` on the Redis pub/sub connection).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// Mean interval of the local safety `Resync` (jittered by ±25 %).
pub const SAFETY_RESYNC_INTERVAL: Duration = Duration::from_secs(300);

/// How a networked bus watches its connection (tests shorten it).
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct BusTiming {
    /// First reconnect delay; doubles on each failure up to 30 s.
    pub initial_backoff: Duration,
    /// See [`HEARTBEAT_INTERVAL`].
    pub heartbeat: Duration,
    /// See [`SAFETY_RESYNC_INTERVAL`].
    pub safety_resync: Duration,
}

impl Default for BusTiming {
    fn default() -> Self {
        Self {
            initial_backoff: INITIAL_BACKOFF,
            heartbeat: HEARTBEAT_INTERVAL,
            safety_resync: SAFETY_RESYNC_INTERVAL,
        }
    }
}

impl BusTiming {
    fn with_backoff(initial_backoff: Duration) -> Self {
        Self {
            initial_backoff,
            ..Self::default()
        }
    }

    /// A ticker for heartbeats; the first tick is one period from now.
    fn heartbeat_timer(&self) -> Interval {
        let mut timer = tokio::time::interval_at(Instant::now() + self.heartbeat, self.heartbeat);
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        timer
    }

    /// When the next safety resync is due: the mean interval, ±25 %.
    fn next_safety_resync(&self) -> Instant {
        // Spread nodes apart without a random-number dependency: std's
        // `RandomState` is randomly keyed per instance.
        let permille = RandomState::new().hash_one(Instant::now()) % 501;
        let base = self.safety_resync.mul_f64(0.75);
        let spread = self.safety_resync.mul_f64(0.5 * permille as f64 / 500.0);
        Instant::now() + base + spread
    }
}

/// What woke a receive loop.
enum Wake<T> {
    Message(T),
    Heartbeat,
    SafetyResync,
}

const EVENTS_METRIC: &str = "mb_bus_events_total";
const CONNECTED_METRIC: &str = "mb_bus_connected";

/// Carries [`BusEvent`]s between the nodes of a cluster.
#[async_trait]
pub trait Bus: Send + Sync {
    /// Announces `event` to the other nodes. Never delivered back to this node.
    async fn publish(&self, event: BusEvent) -> Result<(), StateError>;
    /// Events from other nodes, plus a local [`BusEvent::Resync`] after a reconnect.
    fn subscribe(&self) -> broadcast::Receiver<BusEvent>;
    /// `Err(reason)` while the bus is disconnected.
    fn health(&self) -> Result<(), String>;
    /// This node's id, the [`BusEnvelope::origin`] of what it publishes.
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

/// Locks one of the bus mutexes, ignoring poison. Every critical section
/// under these locks is a single assignment, push, clone or read, so a
/// panic elsewhere cannot leave the guarded value half-updated.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
    /// A shared local bus of node `node_id`.
    #[deprecated(
        since = "0.1.1",
        note = "use `LocalBus::new_shared`; in 0.2.0 `new` returns `Self`"
    )]
    pub fn new(node_id: String) -> Arc<Self> {
        Self::new_shared(node_id)
    }

    /// A shared local bus of node `node_id`.
    #[must_use]
    pub fn new_shared(node_id: String) -> Arc<Self> {
        let (tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Arc::new(Self { node_id, tx })
    }
}

impl fmt::Debug for LocalBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalBus")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
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
    /// An empty shared hub.
    #[deprecated(
        since = "0.1.1",
        note = "use `MemoryBusHub::new_shared`; in 0.2.0 `new` returns `Self`"
    )]
    pub fn new() -> Arc<Self> {
        Self::new_shared()
    }

    /// An empty shared hub.
    #[must_use]
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self {
            nodes: Mutex::new(Vec::new()),
        })
    }

    /// A bus of node `node_id` joined to this hub: it delivers to every
    /// other node of the hub, never back to itself.
    pub fn join(self: &Arc<Self>, node_id: String) -> Arc<dyn Bus> {
        let (tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        lock(&self.nodes).push((node_id.clone(), tx.clone()));
        Arc::new(MemoryBus {
            hub: Arc::clone(self),
            node_id,
            tx,
        })
    }

    fn deliver(&self, payload: &str) -> Result<(), StateError> {
        let env: BusEnvelope = serde_json::from_str(payload)
            .map_err(|e| StateError::Backend(format!("bus: decode event: {e}")))?;
        let nodes = lock(&self.nodes);
        for (id, tx) in nodes.iter().filter(|(id, _)| *id != env.origin) {
            tracing::trace!(node = %id, "memory bus delivery");
            // No subscribers is fine.
            let _ = tx.send(env.event.clone());
        }
        Ok(())
    }
}

impl fmt::Debug for MemoryBusHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let nodes: Vec<String> = lock(&self.nodes).iter().map(|(id, _)| id.clone()).collect();
        f.debug_struct("MemoryBusHub")
            .field("nodes", &nodes)
            .finish()
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
    /// A publish failed: the other nodes may have missed an event, so a
    /// `Resync` is announced at the next heartbeat that can send one.
    resync_owed: Arc<AtomicBool>,
}

impl Receiver {
    fn new(node_id: String) -> Self {
        let (tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        metrics::gauge!(CONNECTED_METRIC).set(1.0);
        Self {
            node_id,
            tx,
            health: Arc::new(Mutex::new(Ok(()))),
            resync_owed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn owe_resync(&self) {
        self.resync_owed.store(true, Ordering::Relaxed);
    }

    fn resync_owed(&self) -> bool {
        self.resync_owed.load(Ordering::Relaxed)
    }

    /// The owed `Resync` went out.
    fn resync_announced(&self) {
        self.resync_owed.store(false, Ordering::Relaxed);
        tracing::info!("bus works again after a failed publish; asked the other nodes to resync");
        count("sent", &BusEvent::Resync);
    }

    /// The `Resync` announcement for the other nodes.
    fn resync_payload(&self) -> Result<String, String> {
        encode(&self.node_id, BusEvent::Resync)
            .map(|(payload, _)| payload)
            .map_err(|e| e.to_string())
    }

    /// The periodic safety net: reload everything even though nothing
    /// looked lost.
    fn safety_resync(&self) {
        tracing::debug!("periodic bus safety resync");
        let _ = self.tx.send(BusEvent::Resync);
    }

    fn health(&self) -> Result<(), String> {
        lock(&self.health).clone()
    }

    fn set_health(&self, h: Result<(), String>) {
        metrics::gauge!(CONNECTED_METRIC).set(if h.is_ok() { 1.0 } else { 0.0 });
        *lock(&self.health) = h;
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

impl fmt::Debug for PgBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgBus")
            .field("node_id", &self.recv.node_id)
            .field("health", &self.recv.health())
            .finish_non_exhaustive()
    }
}

impl PgBus {
    /// Connects to the Postgres at `url` and listens on [`PG_BUS_CHANNEL`]
    /// as node `node_id`; a lost connection is re-established in the
    /// background.
    ///
    /// # Errors
    ///
    /// [`StateError::Backend`] when the first connection or `LISTEN` fails.
    pub async fn connect(url: &str, node_id: String) -> Result<Arc<Self>, StateError> {
        Self::connect_with_timing(url, node_id, BusTiming::default()).await
    }

    /// [`PgBus::connect`] with a custom first reconnect delay (tests).
    #[doc(hidden)]
    pub async fn connect_with_backoff(
        url: &str,
        node_id: String,
        initial_backoff: Duration,
    ) -> Result<Arc<Self>, StateError> {
        Self::connect_with_timing(url, node_id, BusTiming::with_backoff(initial_backoff)).await
    }

    /// [`PgBus::connect`] with custom timings (tests).
    #[doc(hidden)]
    pub async fn connect_with_timing(
        url: &str,
        node_id: String,
        timing: BusTiming,
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
        let task = tokio::spawn(pg_receive(recv.clone(), options, listener, timing));
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
    timing: BusTiming,
) {
    let mut heartbeat = timing.heartbeat_timer();
    let safety = tokio::time::sleep_until(timing.next_safety_resync());
    tokio::pin!(safety);
    loop {
        let wake = tokio::select! {
            r = listener.try_recv() => Wake::Message(r),
            _ = heartbeat.tick() => Wake::Heartbeat,
            () = &mut safety => Wake::SafetyResync,
        };
        let reason = match wake {
            Wake::Message(Ok(Some(n))) => {
                recv.deliver(n.payload());
                continue;
            }
            Wake::Message(Ok(None)) => "postgres: listener connection lost".to_owned(),
            Wake::Message(Err(e)) => format!("postgres: {e}"),
            Wake::Heartbeat => match pg_heartbeat(&recv, &mut listener).await {
                Ok(()) => continue,
                Err(e) => e,
            },
            Wake::SafetyResync => {
                recv.safety_resync();
                safety.as_mut().reset(timing.next_safety_resync());
                continue;
            }
        };
        drop(listener);
        recv.lost(reason);
        listener = recv
            .reconnect(timing.initial_backoff, || pg_listen(&options))
            .await;
        heartbeat.reset();
    }
}

/// Proves the listener connection still answers: announces an owed
/// `Resync` when there is one, else `SELECT 1`. Notifications arriving
/// meanwhile are buffered by the listener.
async fn pg_heartbeat(recv: &Receiver, listener: &mut PgListener) -> Result<(), String> {
    if recv.resync_owed() {
        let payload = recv.resync_payload()?;
        with_timeout(
            "postgres heartbeat",
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(PG_BUS_CHANNEL)
                .bind(payload)
                .execute(&mut *listener),
        )
        .await?;
        recv.resync_announced();
        return Ok(());
    }
    with_timeout(
        "postgres heartbeat",
        sqlx::query("SELECT 1").execute(&mut *listener),
    )
    .await
    .map(|_| ())
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
        .map_err(|e| {
            self.recv.owe_resync();
            StateError::Backend(format!("bus: {e}"))
        })?;
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
    publisher: RedisPublisher,
    task: JoinHandle<()>,
}

/// The command connection publishes go out on. The connection manager only
/// reconnects a connection that closed, not one that went half-open and
/// times out, so a failed publish replaces it with a fresh one.
#[derive(Clone)]
struct RedisPublisher {
    client: redis::Client,
    con: Arc<Mutex<ConnectionManager>>,
}

impl RedisPublisher {
    fn config() -> ConnectionManagerConfig {
        ConnectionManagerConfig::new()
            .set_connection_timeout(Some(IO_TIMEOUT))
            .set_response_timeout(Some(IO_TIMEOUT))
            .set_number_of_retries(2)
    }

    async fn connect(client: redis::Client) -> Result<Self, StateError> {
        let con = client
            .get_connection_manager_with_config(Self::config())
            .await?;
        Ok(Self {
            client,
            con: Arc::new(Mutex::new(con)),
        })
    }

    async fn publish(&self, payload: &str) -> Result<(), String> {
        let mut con = lock(&self.con).clone();
        let published = with_timeout(
            "redis publish",
            redis::cmd("PUBLISH")
                .arg(REDIS_BUS_CHANNEL)
                .arg(payload)
                .query_async::<i64>(&mut con),
        )
        .await;
        if published.is_err() {
            self.replace();
        }
        published.map(|_| ())
    }

    /// Swaps in a fresh connection (made on its first use).
    fn replace(&self) {
        match ConnectionManager::new_lazy_with_config(self.client.clone(), Self::config()) {
            Ok(fresh) => *lock(&self.con) = fresh,
            Err(error) => tracing::warn!(%error, "cannot replace the bus publish connection"),
        }
    }
}

impl fmt::Debug for RedisBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisBus")
            .field("node_id", &self.recv.node_id)
            .field("health", &self.recv.health())
            .finish_non_exhaustive()
    }
}

impl RedisBus {
    /// Connects to the Redis at `url` (`redis://` or `rediss://`) and
    /// subscribes to [`REDIS_BUS_CHANNEL`] as node `node_id`; a lost
    /// connection is re-established in the background.
    ///
    /// # Errors
    ///
    /// [`StateError::Backend`] when the URL has another scheme, or the
    /// first connection or subscription fails.
    pub async fn connect(url: &str, node_id: String) -> Result<Arc<Self>, StateError> {
        Self::connect_with_timing(url, node_id, BusTiming::default()).await
    }

    /// [`RedisBus::connect`] with a custom first reconnect delay (tests).
    #[doc(hidden)]
    pub async fn connect_with_backoff(
        url: &str,
        node_id: String,
        initial_backoff: Duration,
    ) -> Result<Arc<Self>, StateError> {
        Self::connect_with_timing(url, node_id, BusTiming::with_backoff(initial_backoff)).await
    }

    /// [`RedisBus::connect`] with custom timings (tests).
    #[doc(hidden)]
    pub async fn connect_with_timing(
        url: &str,
        node_id: String,
        timing: BusTiming,
    ) -> Result<Arc<Self>, StateError> {
        if !(url.starts_with("redis://") || url.starts_with("rediss://")) {
            return Err(StateError::Backend(
                "bus: redis url must start with redis:// or rediss://".into(),
            ));
        }
        let client = redis::Client::open(url)?;
        let publisher = RedisPublisher::connect(client.clone()).await?;
        let subscription = redis_subscribe(&client)
            .await
            .map_err(StateError::Backend)?;
        let recv = Receiver::new(node_id);
        let task = tokio::spawn(redis_receive(
            recv.clone(),
            client,
            publisher.clone(),
            subscription,
            timing,
        ));
        Ok(Arc::new(Self {
            recv,
            publisher,
            task,
        }))
    }
}

/// A pub/sub connection: the sink for heartbeats, the stream for messages.
type Subscription = (redis::aio::PubSubSink, redis::aio::PubSubStream);

async fn redis_subscribe(client: &redis::Client) -> Result<Subscription, String> {
    let mut pubsub = with_timeout("redis subscribe", client.get_async_pubsub()).await?;
    with_timeout("redis subscribe", pubsub.subscribe(REDIS_BUS_CHANNEL)).await?;
    Ok(pubsub.split())
}

async fn redis_receive(
    recv: Receiver,
    client: redis::Client,
    publisher: RedisPublisher,
    (mut sink, mut stream): Subscription,
    timing: BusTiming,
) {
    let mut heartbeat = timing.heartbeat_timer();
    let safety = tokio::time::sleep_until(timing.next_safety_resync());
    tokio::pin!(safety);
    loop {
        let wake = tokio::select! {
            msg = stream.next() => Wake::Message(msg),
            _ = heartbeat.tick() => Wake::Heartbeat,
            () = &mut safety => Wake::SafetyResync,
        };
        let reason = match wake {
            Wake::Message(Some(msg)) => {
                match msg.get_payload::<String>() {
                    Ok(payload) => recv.deliver(&payload),
                    Err(e) => tracing::warn!(error = %e, "ignoring non-text bus payload"),
                }
                continue;
            }
            Wake::Message(None) => "redis: pub/sub connection lost".to_owned(),
            Wake::Heartbeat => match redis_heartbeat(&recv, &mut sink, &publisher).await {
                Ok(()) => continue,
                Err(e) => e,
            },
            Wake::SafetyResync => {
                recv.safety_resync();
                safety.as_mut().reset(timing.next_safety_resync());
                continue;
            }
        };
        drop((sink, stream));
        recv.lost(reason);
        (sink, stream) = recv
            .reconnect(timing.initial_backoff, || redis_subscribe(&client))
            .await;
        heartbeat.reset();
    }
}

/// Proves the pub/sub connection still answers (`PING`), then announces an
/// owed `Resync` over the command connection when there is one.
async fn redis_heartbeat(
    recv: &Receiver,
    sink: &mut redis::aio::PubSubSink,
    publisher: &RedisPublisher,
) -> Result<(), String> {
    with_timeout("redis heartbeat", sink.ping::<redis::Value>()).await?;
    if recv.resync_owed() {
        let payload = recv.resync_payload()?;
        match publisher.publish(&payload).await {
            Ok(_) => recv.resync_announced(),
            // The subscriber is fine; try again at the next heartbeat.
            Err(error) => tracing::warn!(%error, "cannot announce a resync on the bus yet"),
        }
    }
    Ok(())
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
        if let Err(e) = self.publisher.publish(&payload).await {
            self.recv.owe_resync();
            return Err(StateError::Backend(format!("bus: {e}")));
        }
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
