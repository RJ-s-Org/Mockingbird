//! The per-mock channel runtime: broker resolution, the consumer tasks and
//! the shared state producers, consumers and triggers use.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use mb_engine::Source;
use mb_mcm::{Bundle, Channel, ChannelBinding, Direction, Trigger};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::task::JoinSet;

use super::Brokers;
use super::triggers::{AlwaysLeader, LeaseProvider};
use crate::MockRuntime;

/// The session produced messages and state effects use by default.
pub(crate) const DEFAULT_SESSION: &str = "default";
/// The workspace of every session key.
pub(crate) const WORKSPACE: &str = "default";
/// Most messages one manual publish generates.
const MAX_MANUAL_COUNT: usize = 100;
/// Manual publishes generated at once.
const MANUAL_CONCURRENCY: usize = 4;
/// Capacity of each WebSocket channel's broadcast queue.
const WS_QUEUE: usize = 256;

/// The stored settings of one channel.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelSetup {
    pub enabled: bool,
    pub triggers: Vec<Trigger>,
}

/// Process-wide dependencies of every channel runtime.
#[derive(Clone)]
pub struct EventDeps {
    pub brokers: Arc<Brokers>,
    pub lease: Arc<dyn LeaseProvider>,
    /// This replica's scheduler-lease holder id.
    pub holder: String,
}

impl Default for EventDeps {
    /// No brokers, a single replica that always leads, a random holder id.
    fn default() -> Self {
        Self {
            brokers: Brokers::empty(),
            lease: Arc::new(AlwaysLeader),
            holder: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// The event settings of one mock.
#[derive(Clone, Debug, Default)]
pub struct EventSetup {
    /// The broker the mock uses; `None` picks the single broker serving
    /// every broker channel.
    pub broker: Option<String>,
    /// By channel id; a missing id is enabled with the contract triggers.
    pub channels: HashMap<String, ChannelSetup>,
    /// The scheduler lease key of the mock.
    pub lease_key: String,
}

impl EventSetup {
    /// The settings of `channel`: stored, else enabled with the contract
    /// triggers.
    pub(crate) fn channel(&self, channel: &Channel) -> ChannelSetup {
        self.channels
            .get(&channel.id)
            .cloned()
            .unwrap_or_else(|| ChannelSetup {
                enabled: true,
                triggers: channel.effective_triggers(),
            })
    }
}

/// One produced message.
#[derive(Clone, Debug, PartialEq)]
pub struct Published {
    pub channel: String,
    /// The resolved address it was published to.
    pub address: String,
    pub key: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub payload: Value,
    pub source: Source,
}

#[derive(thiserror::Error, Debug, Clone, PartialEq)]
pub enum PublishError {
    #[error("channel {0:?} does not exist")]
    UnknownChannel(String),
    #[error("channel {0:?} is consumed by this mock, not published")]
    NotPublishable(String),
    #[error("channel {0:?} is disabled")]
    Disabled(String),
    #[error("{0}")]
    NoBroker(String),
    #[error("broker publish failed: {0}")]
    Broker(String),
    #[error("generated message is {0} bytes, over the 1 MiB limit")]
    TooLarge(usize),
}

/// Message counts and the latest outcome of one channel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChannelActivity {
    pub published: u64,
    pub received: u64,
    pub last_published_at: Option<DateTime<Utc>>,
    pub last_received_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// Which way a message went, for activity and metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flow {
    Publish,
    Receive,
}

impl Flow {
    fn as_str(self) -> &'static str {
        match self {
            Flow::Publish => "publish",
            Flow::Receive => "receive",
        }
    }
}

/// What made a channel produce a message.
#[derive(Clone, Debug)]
pub(crate) enum Origin {
    /// A trigger firing; carries the trigger JSON (with `"session"` for
    /// a state change).
    Trigger(Value),
    /// A manual publish, with its optional context.
    Manual(Option<Value>),
    /// A reply to a consumed message.
    Reply {
        received: Value,
        headers: Vec<(String, String)>,
        hop: u32,
    },
}

impl Origin {
    /// The session the message belongs to: a trigger's `"session"`, else
    /// `default`.
    pub(crate) fn session(&self) -> String {
        match self {
            Origin::Trigger(trigger) => trigger
                .get("session")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_SESSION)
                .to_string(),
            _ => DEFAULT_SESSION.to_string(),
        }
    }
}

/// Fan-out of produced WebSocket channel messages, by path template.
/// Closing it drops every sender, which ends the subscribed sockets.
pub(crate) struct WsHub {
    /// `None` once closed.
    senders: Mutex<Option<HashMap<String, broadcast::Sender<String>>>>,
}

impl Default for WsHub {
    fn default() -> Self {
        Self {
            senders: Mutex::new(Some(HashMap::new())),
        }
    }
}

impl WsHub {
    /// Create the sender of `path` unless it exists or the hub is closed.
    pub(crate) fn open(&self, path: &str) {
        if let Some(senders) = lock(&self.senders).as_mut() {
            senders
                .entry(path.to_string())
                .or_insert_with(|| broadcast::channel(WS_QUEUE).0);
        }
    }

    /// Send `text` to every subscriber of `path`; the number reached.
    pub(crate) fn send(&self, path: &str, text: String) -> usize {
        match lock(&self.senders).as_ref().and_then(|s| s.get(path)) {
            Some(tx) => tx.send(text).unwrap_or(0),
            None => 0,
        }
    }

    /// Receive what is sent to `path` from now on. On a closed hub the
    /// receiver reports closed at once.
    pub(crate) fn subscribe(&self, path: &str) -> broadcast::Receiver<String> {
        match lock(&self.senders).as_mut() {
            Some(senders) => senders
                .entry(path.to_string())
                .or_insert_with(|| broadcast::channel(WS_QUEUE).0)
                .subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    /// Drop every sender; later subscribers see a closed hub.
    pub(crate) fn close(&self) {
        lock(&self.senders).take();
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What producers, consumers and triggers of one mock share. Tasks hold
/// only this, never the [`ChannelRuntime`] handle.
pub(crate) struct ChannelInner {
    pub rt: Arc<MockRuntime>,
    pub deps: EventDeps,
    pub setup: EventSetup,
    /// The resolved broker.
    pub broker: Option<String>,
    pub hub: WsHub,
    /// Why the mock cannot serve some channels (broker resolution).
    reasons: Vec<String>,
    /// Inactive channel id → reason.
    inactive: HashMap<String, String>,
    activity: Mutex<BTreeMap<String, ChannelActivity>>,
    /// Consumers currently failing: channel id → error.
    consumer_errors: Mutex<BTreeMap<String, String>>,
    /// Whether this replica holds the mock's scheduler lease.
    leader: AtomicBool,
}

impl ChannelInner {
    fn new(rt: Arc<MockRuntime>, deps: EventDeps, setup: EventSetup) -> Self {
        let resolution = resolve_broker(&rt.bundle, &deps.brokers, &setup);
        let activity = rt
            .bundle
            .channels
            .iter()
            .map(|c| (c.id.clone(), ChannelActivity::default()))
            .collect();
        Self {
            rt,
            deps,
            setup,
            broker: resolution.broker,
            hub: WsHub::default(),
            reasons: resolution.reasons,
            inactive: resolution.inactive,
            activity: Mutex::new(activity),
            consumer_errors: Mutex::new(BTreeMap::new()),
            leader: AtomicBool::new(false),
        }
    }

    /// Whether this replica holds the mock's scheduler lease.
    pub(crate) fn is_leader(&self) -> bool {
        self.leader.load(Ordering::SeqCst)
    }

    pub(crate) fn set_leader(&self, leader: bool) {
        self.leader.store(leader, Ordering::SeqCst);
    }

    /// Why `channel` cannot be served, when it cannot.
    pub(crate) fn inactive_reason(&self, channel: &str) -> Option<&str> {
        self.inactive.get(channel).map(String::as_str)
    }

    pub(crate) fn enabled(&self, channel: &Channel) -> bool {
        self.setup.channel(channel).enabled
    }

    /// Count one message of `channel` in its activity and the
    /// `mb_broker_messages_total` metric; `outcome` carries the error.
    pub(crate) fn note(&self, channel: &str, flow: Flow, outcome: Result<(), &str>) {
        metrics::counter!(
            "mb_broker_messages_total",
            "mock" => self.rt.name.clone(),
            "channel" => channel.to_string(),
            "direction" => flow.as_str(),
            "outcome" => if outcome.is_ok() { "ok" } else { "error" },
        )
        .increment(1);
        let mut activity = lock(&self.activity);
        let entry = activity.entry(channel.to_string()).or_default();
        let now = Utc::now();
        match flow {
            Flow::Receive => {
                entry.received += 1;
                entry.last_received_at = Some(now);
            }
            Flow::Publish if outcome.is_ok() => {
                entry.published += 1;
                entry.last_published_at = Some(now);
            }
            Flow::Publish => {}
        }
        if let Err(e) = outcome {
            entry.last_error = Some(e.to_string());
        }
    }

    /// Mark the consumer of `channel` failing (`Some`) or healthy (`None`).
    pub(crate) fn set_consumer_error(&self, channel: &str, error: Option<String>) {
        let mut errors = lock(&self.consumer_errors);
        match error {
            Some(e) => {
                errors.insert(channel.to_string(), e);
            }
            None => {
                errors.remove(channel);
            }
        }
    }
}

/// The outcome of broker resolution.
struct Resolution {
    broker: Option<String>,
    reasons: Vec<String>,
    inactive: HashMap<String, String>,
}

fn is_websocket(binding: &ChannelBinding) -> bool {
    matches!(binding, ChannelBinding::WebSocket { .. })
}

/// Pick the mock's broker and find the channels it cannot serve.
fn resolve_broker(bundle: &Bundle, brokers: &Brokers, setup: &EventSetup) -> Resolution {
    let broker_channels: Vec<&Channel> = bundle
        .channels
        .iter()
        .filter(|c| !is_websocket(&c.binding))
        .collect();
    let mut out = Resolution {
        broker: None,
        reasons: Vec::new(),
        inactive: HashMap::new(),
    };
    let chosen = match &setup.broker {
        Some(b) if brokers.config(b).is_some() => Ok(b.clone()),
        Some(b) => Err(format!("broker \"{b}\" is not configured")),
        None if broker_channels.is_empty() => return out,
        None => {
            let candidates: Vec<String> = brokers
                .names()
                .into_iter()
                .filter(|name| {
                    brokers.config(name).is_some_and(|config| {
                        broker_channels
                            .iter()
                            .all(|c| config.kind.serves(&c.binding))
                    })
                })
                .collect();
            match candidates.as_slice() {
                [only] => Ok(only.clone()),
                _ => {
                    let mut protocols: Vec<&str> = broker_channels
                        .iter()
                        .map(|c| c.binding.protocol())
                        .collect();
                    protocols.sort_unstable();
                    protocols.dedup();
                    Err(format!(
                        "no broker configured for {} channels (set the mock's broker)",
                        protocols.join("/")
                    ))
                }
            }
        }
    };
    match chosen {
        Ok(broker) => {
            if let Some(config) = brokers.config(&broker) {
                for channel in &broker_channels {
                    if !config.kind.serves(&channel.binding) || !config.kind.compiled() {
                        let reason = format!(
                            "broker \"{broker}\" ({}) cannot serve channel {} ({})",
                            config.kind.as_str(),
                            channel.id,
                            channel.binding.protocol()
                        );
                        out.reasons.push(reason.clone());
                        out.inactive.insert(channel.id.clone(), reason);
                    }
                }
            }
            out.broker = Some(broker);
        }
        Err(reason) => {
            for channel in &broker_channels {
                out.inactive.insert(channel.id.clone(), reason.clone());
            }
            out.reasons.push(reason);
        }
    }
    out
}

/// The channels of one mounted mock: its consumer tasks and the handle the
/// Admin API publishes through. Dropping it stops every task it started.
pub struct ChannelRuntime {
    inner: Arc<ChannelInner>,
    tasks: Mutex<JoinSet<()>>,
}

impl ChannelRuntime {
    /// Resolve the broker, open the hub of every WebSocket channel, start
    /// a consumer for every enabled, active broker channel the mock
    /// consumes and the triggers of every enabled, active channel it
    /// publishes. Must run inside a Tokio runtime.
    pub fn start(rt: Arc<MockRuntime>, deps: EventDeps, setup: EventSetup) -> Arc<Self> {
        let inner = Arc::new(ChannelInner::new(rt, deps, setup));
        for channel in &inner.rt.bundle.channels {
            if let ChannelBinding::WebSocket { path } = &channel.binding {
                inner.hub.open(path);
            }
        }
        let mut tasks = JoinSet::new();
        if let Some(broker) = &inner.broker {
            for channel in &inner.rt.bundle.channels {
                if channel.direction != Direction::Subscribe
                    || is_websocket(&channel.binding)
                    || !inner.enabled(channel)
                    || inner.inactive_reason(&channel.id).is_some()
                {
                    continue;
                }
                tasks.spawn(super::consume::run_consumer(
                    Arc::clone(&inner),
                    channel.clone(),
                    broker.clone(),
                ));
            }
        }
        super::triggers::spawn_triggers(&inner, &mut tasks);
        Arc::new(Self {
            inner,
            tasks: Mutex::new(tasks),
        })
    }

    /// The routes of the mock's enabled WebSocket channels; `None` when it
    /// has none. Dropping the runtime closes every socket they opened.
    pub fn ws_router(&self) -> Option<axum::Router> {
        super::ws::router(&self.inner)
    }

    /// Generate and publish `count` (clamped to 1..=100) messages on
    /// `channel`, in order, stopping at the first error.
    pub async fn publish_manual(
        &self,
        channel: &str,
        count: usize,
        context: Option<Value>,
    ) -> Result<Vec<Published>, PublishError> {
        let inner = &self.inner;
        let Some(ch) = inner.rt.bundle.channel(channel) else {
            return Err(PublishError::UnknownChannel(channel.to_string()));
        };
        if ch.direction == Direction::Subscribe {
            return Err(PublishError::NotPublishable(channel.to_string()));
        }
        if !inner.enabled(ch) {
            return Err(PublishError::Disabled(channel.to_string()));
        }
        if let Some(reason) = inner.inactive_reason(channel) {
            return Err(PublishError::NoBroker(reason.to_string()));
        }
        let count = count.clamp(1, MAX_MANUAL_COUNT);
        let mut results = futures::stream::iter(0..count)
            .map(|_| inner.produce(ch, Origin::Manual(context.clone()), Vec::new()))
            .buffered(MANUAL_CONCURRENCY);
        let mut published = Vec::with_capacity(count);
        while let Some(result) = results.next().await {
            published.push(result?);
        }
        Ok(published)
    }

    /// Per-channel activity, by channel id.
    pub fn activity(&self) -> BTreeMap<String, ChannelActivity> {
        lock(&self.inner.activity).clone()
    }

    /// Why the mock's channels are not all working: broker resolution
    /// reasons, its broker's last error and failing consumers.
    pub fn health(&self) -> Vec<String> {
        let inner = &self.inner;
        let mut out = inner.reasons.clone();
        if let Some(broker) = &inner.broker {
            out.extend(
                inner
                    .deps
                    .brokers
                    .issues()
                    .into_iter()
                    .filter(|(name, _)| name == broker)
                    .map(|(name, e)| format!("broker {name}: {e}")),
            );
        }
        out.extend(
            lock(&inner.consumer_errors)
                .iter()
                .map(|(id, e)| format!("channel {id}: {e}")),
        );
        out
    }

    /// Whether this replica holds the mock's scheduler lease, so its
    /// schedule and cron triggers fire.
    pub fn is_leader(&self) -> bool {
        self.inner.is_leader()
    }

    /// The broker the mock resolved to.
    pub fn broker_name(&self) -> Option<String> {
        self.inner.broker.clone()
    }
}

impl Drop for ChannelRuntime {
    fn drop(&mut self) {
        lock(&self.tasks).abort_all();
        self.inner.hub.close();
    }
}
