#![cfg(test)]
#![cfg(feature = "events")]
//! The provider policy runs for event-channel generation and webhooks too:
//! a broker or WebSocket reply, a scheduled producer and a webhook payload
//! generate with the profile the policy picks for their tenancy, a
//! rejection skips that one message (warned, recorded and metered) without
//! stopping the producer, and the self-hosted `ConfiguredProfile` builds no
//! engine besides the mock's own.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use mb_engine::{Engine, EngineConfig};
use mb_ext::{
    CallInfo, EnvFileSecrets, ProviderPolicy, Rejection, Tenancy, TenantResolver, UsageEvent,
    UsageSink,
};
use mb_llm::{Provider, ScriptedProvider};
use mb_mcm::{Bundle, ChannelBinding, Trigger};
use mb_protocols::events::{
    BrokerConfig, BrokerKind, Brokers, ChannelRuntime, ChannelSetup, EventDeps, EventSetup,
    InMessage, MemoryBroker,
};
use mb_protocols::{MockRuntime, ProfileEngines, RequestRecord, RequestRecorder};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::support::{subscribed, wait_until};

/// One LLM answer that fits every message these tests generate, marked
/// with `who` (the profile that answered).
fn answer(who: &str) -> String {
    json!({
        "orderId": who,
        "cancelledAt": "2026-01-01T00:00:00Z",
        "id": who,
        "status": "placed",
        "total": 1,
        "user": who,
        "text": "hi",
        "at": "2026-01-01T00:00:00Z",
        "name": who,
        "adopted": true,
    })
    .to_string()
}

/// A scripted provider answering `answer(who)` many times.
fn provider(who: &str) -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new(
        (0..64).map(|_| Ok(answer(who))).collect(),
    ))
}

fn engine(provider: &Arc<ScriptedProvider>) -> Arc<Engine> {
    let provider: Arc<dyn Provider> = provider.clone();
    Arc::new(Engine::new(Some(provider), EngineConfig::default()))
}

/// The tenant named by an `x-tenant` header, else by the operation for
/// `orderCreated` (`acme`), else `default`.
struct HeaderTenant;

#[async_trait]
impl TenantResolver for HeaderTenant {
    async fn resolve(&self, call: &CallInfo<'_>) -> Result<Tenancy, Rejection> {
        let tenant = call
            .headers
            .get("x-tenant")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(match call.operation_id {
                "orderCreated" => "acme",
                _ => "default",
            });
        Ok(Tenancy::new(tenant, "default"))
    }
}

/// Tenant `acme` gets profile `gold`; every other tenant the configured one.
struct GoldForAcme;

impl ProviderPolicy for GoldForAcme {
    fn choose(
        &self,
        tenancy: &Tenancy,
        _mock: &str,
        configured: Option<&str>,
    ) -> Result<Option<String>, Rejection> {
        Ok(match tenancy.tenant.as_str() {
            "acme" => Some("gold".into()),
            _ => configured.map(str::to_string),
        })
    }
}

/// Refuses every call with 402.
struct NoPlan;

impl ProviderPolicy for NoPlan {
    fn choose(
        &self,
        _tenancy: &Tenancy,
        _mock: &str,
        _configured: Option<&str>,
    ) -> Result<Option<String>, Rejection> {
        Err(Rejection::new(
            402,
            "Payment Required",
            "the plan has no LLM calls left",
        ))
    }
}

/// Builds an engine answering with the profile's name, counting the builds
/// and keeping each profile's provider.
#[derive(Default)]
struct Counting {
    built: AtomicUsize,
    providers: Mutex<HashMap<Option<String>, Arc<ScriptedProvider>>>,
}

impl Counting {
    fn built(&self) -> usize {
        self.built.load(Ordering::SeqCst)
    }

    /// The LLM requests the engine built for `profile` received.
    fn requests(&self, profile: &str) -> usize {
        self.providers
            .lock()
            .unwrap()
            .get(&Some(profile.to_string()))
            .map_or(0, |p| p.requests().len())
    }
}

#[async_trait]
impl ProfileEngines for Counting {
    async fn engine(&self, profile: Option<&str>) -> Result<Arc<Engine>, String> {
        self.built.fetch_add(1, Ordering::SeqCst);
        let p = provider(profile.unwrap_or("none"));
        self.providers
            .lock()
            .unwrap()
            .insert(profile.map(str::to_string), p.clone());
        Ok(engine(&p))
    }
}

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

#[derive(Default)]
struct Metered(Mutex<Vec<UsageEvent>>);

impl UsageSink for Metered {
    fn record(&self, event: &UsageEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

impl Metered {
    fn events(&self) -> Vec<UsageEvent> {
        self.0.lock().unwrap().clone()
    }
}

fn fixture(name: &str) -> Bundle {
    let path = format!(
        "{}/../../fixtures/asyncapi/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    mb_import::compile(&std::fs::read(path).unwrap()).unwrap()
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
    broker: Arc<MemoryBroker>,
    engines: Arc<Counting>,
    records: Arc<Capture>,
    usage: Arc<Metered>,
}

/// The mock `bundle` configured with profile `base` (answering `base`),
/// other profiles' engines built by a [`Counting`], tenancy from
/// [`HeaderTenant`] and `policy` choosing; `orderCreated`, when the
/// contract has it, fires on `triggers`.
fn start(bundle: Bundle, policy: Arc<dyn ProviderPolicy>, triggers: Vec<Trigger>) -> Harness {
    let engines = Arc::new(Counting::default());
    let records = Arc::new(Capture::default());
    let usage = Arc::new(Metered::default());
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new("orders", Arc::new(bundle), engine(&provider("base")), state);
    rt.configured_profile = Some("base".into());
    rt.profile_engines = Some(engines.clone());
    rt.ext.tenants = Arc::new(HeaderTenant);
    rt.ext.providers = policy;
    rt.ext.usage = usage.clone();
    rt.recorder = Some(records.clone());
    let brokers = memory_brokers();
    let broker = brokers.memory("mem").unwrap_or_default();
    let deps = EventDeps {
        brokers,
        ..EventDeps::default()
    };
    let setup = EventSetup {
        channels: HashMap::from([(
            "orderCreated".to_string(),
            ChannelSetup {
                enabled: true,
                triggers,
            },
        )]),
        ..EventSetup::default()
    };
    let channels = ChannelRuntime::start(Arc::new(rt), deps, setup);
    Harness {
        channels,
        broker,
        engines,
        records,
        usage,
    }
}

fn cancel(bundle_binding: &ChannelBinding, h: &Harness, order: &str, tenant: Option<&str>) {
    let headers = tenant
        .map(|t| vec![("x-tenant".to_string(), t.to_string())])
        .unwrap_or_default();
    h.broker.inject(
        bundle_binding,
        InMessage {
            address: "order-cancellations".into(),
            key: None,
            headers,
            payload: json!({"orderId": order}).to_string().into_bytes(),
        },
    );
}

/// The `orderId` of every reply published so far, waiting for `n`.
async fn replies(h: &Harness, n: usize) -> Vec<Value> {
    h.broker
        .wait_published(n, Duration::from_secs(5))
        .await
        .iter()
        .map(|(_, m)| serde_json::from_slice::<Value>(&m.payload).unwrap()["orderId"].clone())
        .collect()
}

fn cancel_binding() -> ChannelBinding {
    fixture("orders-amqp-2.6.yaml")
        .channel("cancelOrder")
        .unwrap()
        .binding
        .clone()
}

#[tokio::test]
async fn a_broker_reply_generates_with_the_profile_the_policy_picks_for_its_tenant() {
    let h = start(
        fixture("orders-amqp-2.6.yaml"),
        Arc::new(GoldForAcme),
        Vec::new(),
    );
    subscribed(&h.broker, 1).await;
    let binding = cancel_binding();

    // One at a time: replies are published in the order they finish.
    cancel(&binding, &h, "o1", Some("acme"));
    assert_eq!(replies(&h, 1).await, vec!["gold"]);
    cancel(&binding, &h, "o2", None);
    assert_eq!(replies(&h, 2).await[1], "base");
    cancel(&binding, &h, "o3", Some("acme"));
    assert_eq!(replies(&h, 3).await[2], "gold");

    // Built once, on first use, for the profile that differs; each reply is
    // metered to the tenant it was generated for.
    assert_eq!(h.engines.built(), 1);
    let tenants: Vec<String> = h
        .usage
        .events()
        .into_iter()
        .filter(|e| e.operation_id == "orderCancelled")
        .map(|e| e.tenancy.tenant)
        .collect();
    assert_eq!(tenants, vec!["acme", "default", "acme"]);
}

#[tokio::test]
async fn a_scheduled_producer_generates_with_the_profile_of_its_tenant() {
    let h = start(
        fixture("orders-amqp-2.6.yaml"),
        Arc::new(GoldForAcme),
        vec![Trigger::Schedule {
            every: Duration::from_millis(100),
        }],
    );
    let sent = h.broker.wait_published(2, Duration::from_secs(5)).await;
    for (_, msg) in &sent {
        let payload: Value = serde_json::from_slice(&msg.payload).unwrap();
        assert_eq!(payload["id"], "gold", "{payload}");
    }
    assert_eq!(h.engines.built(), 1);
}

#[tokio::test]
async fn the_configured_profile_policy_builds_no_engine_for_events() {
    let h = start(
        fixture("orders-amqp-2.6.yaml"),
        Arc::new(mb_ext::ConfiguredProfile),
        vec![Trigger::Schedule {
            every: Duration::from_millis(100),
        }],
    );
    subscribed(&h.broker, 1).await;
    let binding = cancel_binding();
    cancel(&binding, &h, "o1", Some("acme"));
    cancel(&binding, &h, "o2", None);
    let sent = h.broker.wait_published(4, Duration::from_secs(5)).await;
    for (_, msg) in &sent {
        let payload: Value = serde_json::from_slice(&msg.payload).unwrap();
        let who = payload.get("orderId").or(payload.get("id")).unwrap();
        assert_eq!(who, "base", "{payload}");
    }
    assert_eq!(h.engines.built(), 0);
}

#[tokio::test]
async fn a_rejected_generation_is_skipped_recorded_and_metered_and_the_producer_carries_on() {
    let h = start(
        fixture("orders-amqp-2.6.yaml"),
        Arc::new(NoPlan),
        vec![Trigger::Schedule {
            every: Duration::from_millis(100),
        }],
    );
    subscribed(&h.broker, 1).await;
    let binding = cancel_binding();
    cancel(&binding, &h, "o1", Some("acme"));
    cancel(&binding, &h, "o2", Some("acme"));

    // Both messages are received (the consumer keeps going), neither is
    // answered, and the schedule keeps firing (and being skipped).
    let records = wait_until(Duration::from_secs(5), Duration::from_millis(5), || async {
        let records = h.records.records();
        let received = records.iter().filter(|r| r.method == "RECEIVE").count();
        let scheduled = records
            .iter()
            .filter(|r| r.operation_id.as_deref() == Some("orderCreated"))
            .count();
        if received == 2 && scheduled >= 2 {
            Ok(records)
        } else {
            Err(format!("{records:?}"))
        }
    })
    .await;
    assert!(h.broker.published().is_empty());
    for r in records.iter().filter(|r| r.method == "RECEIVE") {
        let note = r.response.as_ref().unwrap()["reply"].as_str().unwrap();
        assert!(note.starts_with("rejected: 402"), "{note}");
    }
    let skipped: Vec<&RequestRecord> = records.iter().filter(|r| r.method == "PUBLISH").collect();
    assert!(!skipped.is_empty());
    for r in &skipped {
        assert_eq!(r.status, 402, "{r:?}");
        assert!(r.response.is_none(), "{r:?}");
    }

    let events = h.usage.events();
    let replies: Vec<&UsageEvent> = events
        .iter()
        .filter(|e| e.operation_id == "orderCancelled")
        .collect();
    assert_eq!(replies.len(), 2, "{events:?}");
    for e in &replies {
        assert_eq!(e.status, 402);
        assert_eq!(e.source, UsageEvent::SOURCE_NOT_GENERATED);
        assert_eq!(e.tenancy.tenant, "acme");
        assert_eq!(e.prompt_tokens + e.completion_tokens, 0);
    }
    assert!(
        events
            .iter()
            .any(|e| e.operation_id == "orderCreated" && e.status == 402)
    );
    let activity = h.channels.activity();
    assert_eq!(activity["orderCancelled"].published, 0);
    assert!(
        activity["orderCancelled"]
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("402")),
        "{activity:?}"
    );
    assert_eq!(h.engines.built(), 0);
}

#[tokio::test]
async fn a_websocket_reply_generates_with_the_profile_of_the_upgrade_tenant() {
    let h = start(
        fixture("chat-ws-2.6.yaml"),
        Arc::new(GoldForAcme),
        Vec::new(),
    );
    let router = h.channels.ws_router().expect("a WebSocket router");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    for (tenant, want) in [(Some("acme"), "gold"), (None, "base")] {
        let mut req = format!("ws://{addr}/chat").into_client_request().unwrap();
        if let Some(t) = tenant {
            req.headers_mut().insert("x-tenant", t.parse().unwrap());
        }
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        ws.send(Message::text(r#"{"user":"a","text":"one"}"#))
            .await
            .unwrap();
        let reply = loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("timed out waiting for a reply");
            match msg {
                Some(Ok(Message::Text(t))) => break t.to_string(),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                other => panic!("expected a text frame, got {other:?}"),
            }
        };
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["user"], want, "{reply}");
    }
    assert_eq!(h.engines.built(), 1);
}

/// A webhook payload is generated with the engine of the call that sent
/// it: the profile the policy picked for the call's tenant.
#[cfg(feature = "http-rest")]
#[tokio::test]
async fn a_webhook_payload_generates_with_the_profile_of_the_calling_tenant() {
    use axum::body::Body;
    use axum::http::Request;
    use mb_protocols::webhooks::deliver::{DispatchSettings, ResolvedTarget, WebhookDispatcher};
    use mb_protocols::webhooks::log::{DeliveryStatus, SharedDeliveryLog};
    use mb_protocols::webhooks::sign::Signing;
    use tower::ServiceExt;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let log: SharedDeliveryLog = Arc::default();
    let dispatcher = WebhookDispatcher::new(
        "default".into(),
        "pets".into(),
        vec![ResolvedTarget {
            url: format!("{}/hook", server.uri()),
            events: vec!["*".into()],
            signing: Signing::None,
            secret: Ok(None),
            delay: Duration::ZERO,
        }],
        reqwest::Client::new(),
        log.clone(),
        DispatchSettings::default(),
    );
    let path = format!(
        "{}/../../fixtures/openapi/webhooks-3.1.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let bundle = Arc::new(mb_import::compile(&std::fs::read(path).unwrap()).unwrap());
    let base = provider("base");
    let engines = Arc::new(Counting::default());
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new("pets", bundle, engine(&base), state);
    rt.configured_profile = Some("base".into());
    rt.profile_engines = Some(engines.clone());
    rt.ext.tenants = Arc::new(HeaderTenant);
    rt.ext.providers = Arc::new(GoldForAcme);
    rt.webhooks = Some(Arc::new(dispatcher));
    let router = mb_protocols::http_rest::router(Arc::new(rt));

    let req = Request::post("/pets")
        .header("content-type", "application/json")
        .header("x-tenant", "acme")
        .body(Body::from(json!({"name": "Rex"}).to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    wait_until(Duration::from_secs(5), Duration::from_millis(5), || async {
        let delivered = log
            .list("default", "pets", 10)
            .iter()
            .any(|r| r.status == DeliveryStatus::Delivered);
        if delivered {
            Ok(())
        } else {
            Err("no delivery yet".to_string())
        }
    })
    .await;

    // The response and the webhook payload: both from `gold`, none from
    // the configured profile.
    assert_eq!(engines.requests("gold"), 2);
    assert_eq!(base.requests().len(), 0);
}
