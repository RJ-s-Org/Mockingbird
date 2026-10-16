#![cfg(feature = "events")]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mb_engine::{Engine, EngineConfig};
use mb_ext::EnvFileSecrets;
use mb_mcm::{Bundle, StateChangeKind, Trigger};
use mb_protocols::events::{
    BrokerConfig, BrokerKind, Brokers, ChannelRuntime, ChannelSetup, EventDeps, EventSetup,
    LeaseProvider, MemoryBroker,
};
use mb_protocols::{MockRuntime, RequestRecord, RequestRecorder};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::json;

#[derive(Default)]
struct Capture(Mutex<Vec<RequestRecord>>);

impl RequestRecorder for Capture {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

impl Capture {
    fn records(&self) -> Vec<RequestRecord> {
        self.0.lock().unwrap().clone()
    }
}

/// A lease provider that answers from a script and counts calls: the
/// `n`th call gets `answers[n]`, or the last answer once they run out.
struct ScriptedLease {
    answers: Vec<Result<bool, String>>,
    calls: AtomicUsize,
}

impl ScriptedLease {
    fn new(answers: Vec<Result<bool, String>>) -> Arc<Self> {
        Arc::new(Self {
            answers,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LeaseProvider for ScriptedLease {
    async fn try_acquire(&self, _key: &str, _holder: &str, _ttl: Duration) -> Result<bool, String> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        self.answers
            .get(n)
            .or_else(|| self.answers.last())
            .cloned()
            .unwrap_or(Ok(true))
    }
}

fn orders() -> Bundle {
    mb_import::compile(include_bytes!(
        "../../../fixtures/asyncapi/orders-amqp-2.6.yaml"
    ))
    .unwrap()
}

fn memory_brokers() -> Arc<Brokers> {
    Brokers::new(
        vec![BrokerConfig {
            name: "mem".into(),
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

struct Harness {
    channels: Arc<ChannelRuntime>,
    records: Arc<Capture>,
    broker: Arc<MemoryBroker>,
    state: Arc<dyn StateStore>,
}

/// The orders mock with `orderCreated` set up as `enabled` with `triggers`.
fn start(enabled: bool, triggers: Vec<Trigger>, lease: Arc<dyn LeaseProvider>) -> Harness {
    let records = Arc::new(Capture::default());
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "orders",
        Arc::new(orders()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&state),
    );
    rt.recorder = Some(Arc::clone(&records) as Arc<dyn RequestRecorder>);
    let brokers = memory_brokers();
    let broker = brokers.memory("mem").unwrap_or_default();
    let deps = EventDeps {
        brokers,
        lease,
        holder: "replica-1".into(),
        shared_subscriptions: false,
    };
    let setup = EventSetup {
        broker: None,
        channels: HashMap::from([(
            "orderCreated".to_string(),
            ChannelSetup { enabled, triggers },
        )]),
        lease_key: "default/orders".into(),
        mode: Default::default(),
    };
    let channels = ChannelRuntime::start(Arc::new(rt), deps, setup);
    Harness {
        channels,
        records,
        broker,
        state,
    }
}

fn schedule(ms: u64) -> Trigger {
    Trigger::Schedule {
        every: Duration::from_millis(ms),
    }
}

fn session(name: &str) -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "orders".into(),
        session: name.into(),
    }
}

fn order(id: &str) -> StoredResource {
    StoredResource {
        resource_type: "orders".into(),
        id: id.into(),
        data: json!({"id": id, "status": "placed", "total": 10}),
    }
}

#[tokio::test(start_paused = true)]
async fn schedule_fires_while_leader() {
    let lease = ScriptedLease::new(vec![Ok(true)]);
    let h = start(true, vec![schedule(1000)], lease.clone());
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(h.broker.published().len(), 3);
    assert!(h.channels.is_leader());
    assert!(lease.calls() >= 1);
    let records = h.records.records();
    assert_eq!(records.len(), 3);
    assert_eq!(
        records[0].request["trigger"],
        json!({"kind": "schedule", "every_ms": 1000})
    );
}

#[tokio::test(start_paused = true)]
async fn not_leader_suppresses_schedule() {
    let lease = ScriptedLease::new(vec![Ok(false)]);
    let h = start(true, vec![schedule(1000)], lease.clone());
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(h.broker.published().len(), 0);
    assert!(!h.channels.is_leader());
    assert!(lease.calls() >= 1);
}

#[tokio::test(start_paused = true)]
async fn lease_error_keeps_leadership_until_ttl() {
    let lease = ScriptedLease::new(vec![Ok(true), Err("store down".into())]);
    let h = start(true, vec![schedule(60_000)], lease.clone());
    tokio::time::sleep(Duration::from_secs(25)).await;
    assert!(h.channels.is_leader());
    assert!(lease.calls() >= 3);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(!h.channels.is_leader());
}

#[tokio::test]
async fn cron_fires_every_second() {
    let lease = ScriptedLease::new(vec![Ok(true)]);
    let h = start(
        true,
        vec![Trigger::Cron {
            expr: "* * * * * *".into(),
        }],
        lease,
    );
    let published = h
        .broker
        .wait_published(2, Duration::from_millis(2500))
        .await;
    assert!(published.len() >= 2, "got {}", published.len());
    assert_eq!(
        h.records.records()[0].request["trigger"],
        json!({"kind": "cron", "expr": "* * * * * *"})
    );
}

#[tokio::test]
async fn state_change_trigger_publishes_with_key() {
    let lease = ScriptedLease::new(vec![Ok(false)]);
    let h = start(
        true,
        vec![Trigger::OnStateChange {
            resource_type: "orders".into(),
            kind: StateChangeKind::Upserted,
        }],
        lease.clone(),
    );
    h.state.upsert(&session("s1"), order("41")).await.unwrap();
    let published = h.broker.wait_published(1, Duration::from_secs(5)).await;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].1.key.as_deref(), Some(b"41".as_slice()));

    h.state.upsert(&session("s1"), order("42")).await.unwrap();
    let published = h.broker.wait_published(2, Duration::from_secs(5)).await;
    assert_eq!(published.len(), 2);
    assert_eq!(published[1].1.key.as_deref(), Some(b"42".as_slice()));

    let records = h.records.records();
    let trigger = &records[1].request["trigger"];
    assert_eq!(
        trigger,
        &json!({
            "kind": "on_state_change",
            "resource_type": "orders",
            "id": "42",
            "change": "upserted",
            "session": "s1",
        })
    );
    assert_eq!(records[1].session, "s1");
    // Fires without the lease, and no leader loop runs for it.
    assert_eq!(lease.calls(), 0);

    // Another mock's change is ignored.
    let other = SessionKey {
        mock: "other".into(),
        ..session("s1")
    };
    h.state.upsert(&other, order("43")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.broker.published().len(), 2);
}

#[tokio::test]
async fn state_change_kind_filter() {
    let h = start(
        true,
        vec![Trigger::OnStateChange {
            resource_type: "orders".into(),
            kind: StateChangeKind::Deleted,
        }],
        ScriptedLease::new(vec![Ok(true)]),
    );
    h.state.upsert(&session("s1"), order("42")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.broker.published().len(), 0);

    h.state
        .delete(&session("s1"), "orders", "42")
        .await
        .unwrap();
    let published = h.broker.wait_published(1, Duration::from_secs(5)).await;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].1.key.as_deref(), Some(b"42".as_slice()));
    assert_eq!(
        h.records.records()[0].request["trigger"]["change"],
        json!("deleted")
    );
}

#[tokio::test(start_paused = true)]
async fn disabled_channel_has_no_triggers() {
    let lease = ScriptedLease::new(vec![Ok(true)]);
    let h = start(false, vec![schedule(1000)], lease.clone());
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(h.broker.published().len(), 0);
    assert_eq!(lease.calls(), 0);
    assert!(!h.channels.is_leader());
}

#[tokio::test(start_paused = true)]
async fn manual_only_spawns_nothing() {
    let lease = ScriptedLease::new(vec![Ok(true)]);
    let h = start(true, vec![Trigger::Manual], lease.clone());
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(lease.calls(), 0);
    assert_eq!(h.broker.published().len(), 0);
    assert!(!h.channels.is_leader());
}

/// One topic both consumed with a state effect and published on that
/// state's changes: every consumed message would trigger a publish to the
/// topic it came from.
const LOOP_SPEC: &str = r#"
asyncapi: 2.6.0
info: {title: Loop, version: 1.0.0}
servers:
  k: {url: localhost:9092, protocol: kafka}
defaultContentType: application/json
channels:
  orders:
    publish:
      operationId: orderIn
      x-mockingbird-state: {resource_type: orders, id_field: id}
      message:
        name: OrderIn
        payload:
          type: object
          required: [id]
          properties:
            id: {type: string}
    subscribe:
      operationId: orderOut
      x-mockingbird-triggers:
        - on_state_change: {resource_type: orders}
      message:
        name: OrderOut
        payload:
          type: object
          required: [id]
          properties:
            id: {type: string}
"#;

#[tokio::test]
async fn state_effect_and_trigger_on_one_topic_stay_bounded() {
    let bundle = mb_import::compile(LOOP_SPEC.as_bytes()).unwrap();
    let records = Arc::new(Capture::default());
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "loop",
        Arc::new(bundle),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&state),
    );
    rt.recorder = Some(Arc::clone(&records) as Arc<dyn RequestRecorder>);
    let brokers = memory_brokers();
    let broker = brokers.memory("mem").unwrap();
    let deps = EventDeps {
        brokers,
        lease: ScriptedLease::new(vec![Ok(true)]),
        holder: "replica-1".into(),
        shared_subscriptions: false,
    };
    let setup = EventSetup {
        broker: None,
        channels: HashMap::new(),
        lease_key: "default/loop".into(),
        mode: Default::default(),
    };
    let _channels = ChannelRuntime::start(Arc::new(rt), deps, setup);
    // Let the consumer subscribe.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let binding = mb_mcm::ChannelBinding::Kafka {
        topic: "orders".into(),
        partitions_hint: None,
    };
    broker.inject(
        &binding,
        mb_protocols::events::InMessage {
            address: "orders".into(),
            key: None,
            headers: Vec::new(),
            payload: br#"{"id":"o-1"}"#.to_vec(),
        },
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let published = broker.published();
    // One publish per hop (1, 2, 3); a message at the hop limit changes
    // state but triggers nothing.
    assert_eq!(published.len(), 3, "got {} publishes", published.len());
    let hops: Vec<String> = published
        .iter()
        .map(|(_, m)| {
            m.headers
                .iter()
                .find(|(k, _)| k == "x-mockingbird-hop")
                .map(|(_, v)| v.clone())
                .unwrap()
        })
        .collect();
    assert_eq!(hops, vec!["1", "2", "3"]);
}

/// The chat mock (WebSocket only) with `chatMessage` on `triggers`.
fn start_chat(triggers: Vec<Trigger>, lease: Arc<dyn LeaseProvider>) -> Harness {
    let bundle = mb_import::compile(include_bytes!(
        "../../../fixtures/asyncapi/chat-ws-2.6.yaml"
    ))
    .unwrap();
    let records = Arc::new(Capture::default());
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "chat",
        Arc::new(bundle),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&state),
    );
    rt.recorder = Some(Arc::clone(&records) as Arc<dyn RequestRecorder>);
    let brokers = memory_brokers();
    let broker = brokers.memory("mem").unwrap();
    let deps = EventDeps {
        brokers,
        lease,
        holder: "replica-2".into(),
        shared_subscriptions: false,
    };
    let setup = EventSetup {
        broker: None,
        channels: HashMap::from([(
            "chatMessage".to_string(),
            ChannelSetup {
                enabled: true,
                triggers,
            },
        )]),
        lease_key: "default/chat".into(),
        mode: Default::default(),
    };
    let channels = ChannelRuntime::start(Arc::new(rt), deps, setup);
    Harness {
        channels,
        records,
        broker,
        state,
    }
}

#[tokio::test(start_paused = true)]
async fn websocket_schedule_fires_on_every_replica() {
    // WebSocket fan-out is per replica: a replica without the lease still
    // pushes to its own clients, and no lease is taken for it.
    let lease = ScriptedLease::new(vec![Ok(false)]);
    let h = start_chat(vec![schedule(1000)], lease.clone());
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let publishes = h
        .records
        .records()
        .into_iter()
        .filter(|r| r.method == "PUBLISH" && r.status == 200)
        .count();
    assert_eq!(publishes, 3);
    assert_eq!(lease.calls(), 0);
}

/// Grants the lease once, then never answers.
struct HangingLease {
    calls: AtomicUsize,
}

#[async_trait]
impl LeaseProvider for HangingLease {
    async fn try_acquire(&self, _key: &str, _holder: &str, _ttl: Duration) -> Result<bool, String> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(true)
        } else {
            std::future::pending().await
        }
    }
}

#[tokio::test(start_paused = true)]
async fn hung_lease_renewal_gives_up_leadership_at_ttl() {
    let lease = Arc::new(HangingLease {
        calls: AtomicUsize::new(0),
    });
    let h = start(true, vec![schedule(60_000)], lease.clone());
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(h.channels.is_leader());
    tokio::time::sleep(Duration::from_secs(25)).await;
    assert!(h.channels.is_leader());
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!h.channels.is_leader(), "still leader 31 s after the grant");
}
