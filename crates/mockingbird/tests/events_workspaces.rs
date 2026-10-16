//! P3 × P5: event channels of a mock outside the `default` workspace. The
//! WebSocket channels enforce the mock's data-plane auth with the
//! workspace's keys, and event records carry the mock's workspace. A mock
//! named `orders` in two workspaces, on one broker and one state store, is
//! isolated: publishes, activity, the live tail, state-change triggers and
//! consumed-message state effects stay in their own workspace.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use mb_admin::AdminState;
use mb_admin::LiveSubscribe;
use mb_admin::channels::{ChannelControl, ChannelControlError, SharedChannelControl};
use mb_ext::{EnvFileSecrets, Extensions};
use mb_mcm::{StateChangeKind, Trigger};
use mb_protocols::RequestRecorder;
use mb_protocols::events::{BrokerConfig, BrokerKind, Brokers, EventDeps, InMessage, MemoryBroker};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use mb_store::{
    ApiKey, ChannelSettings, ContractStatus, ControlStore, DEFAULT_WORKSPACE, DataPlaneAuthConfig,
    DataPlaneAuthMode, Mock, NewContractVersion, NewMock, RequestKind, RequestLogEntry,
    RequestLogFilter, ResourceSource, SqliteControlStore,
};
use mockingbird::dp_keys::StoreDataPlaneKeys;
use mockingbird::events::RegistryChannels;
use mockingbird::recorder::StoreRecorder;
use mockingbird::registry::MockRegistry;
use mockingbird::runtime::RuntimeDeps;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tower::ServiceExt;

const TEAM_A: &str = "team-a";
const CHAT: &[u8] = include_bytes!("../../../fixtures/asyncapi/chat-ws-2.6.yaml");

struct Setup {
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    mock: Mock,
    /// A data-plane key of `team-a`.
    token: String,
    /// A data-plane key of `default`.
    other_ws: String,
    /// `ws://host:port`.
    base: String,
    _registry: Arc<MockRegistry>,
}

async fn store_key(store: &Arc<dyn ControlStore>, ws: &str) -> String {
    let generated = mb_auth::generate_key();
    store
        .create_api_key(&ApiKey {
            key_id: generated.key_id.clone(),
            workspace: ws.into(),
            name: "k".into(),
            role: "viewer".into(),
            scopes: vec!["data-plane".into()],
            sysadmin: false,
            secret_hash: generated.secret_hash,
            created_by: "test".into(),
            created_at: Utc::now(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        })
        .await
        .unwrap();
    generated.token
}

/// The chat WebSocket contract as mock `chat` of `team-a` at `/chat-a`, in
/// `api_key` mode, served on an ephemeral port with the request log on.
async fn setup() -> Setup {
    let store: Arc<dyn ControlStore> = Arc::new(
        SqliteControlStore::connect("sqlite::memory:")
            .await
            .unwrap(),
    );
    store.create_workspace(TEAM_A, "Team A").await.unwrap();
    let mock = store
        .create_mock(
            TEAM_A,
            NewMock {
                name: "chat".into(),
                route_prefix: Some("/chat-a".into()),
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: DataPlaneAuthConfig {
                    mode: DataPlaneAuthMode::ApiKey,
                    ..DataPlaneAuthConfig::default()
                },
            },
        )
        .await
        .unwrap();
    let v = store
        .add_contract_version(
            mock.id,
            NewContractVersion {
                source: CHAT.to_vec(),
                bundle: mb_import::compile(CHAT).unwrap(),
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await
        .unwrap();
    store
        .activate_contract_version(mock.id, v.version)
        .await
        .unwrap();
    let token = store_key(&store, TEAM_A).await;
    let other_ws = store_key(&store, DEFAULT_WORKSPACE).await;

    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let recorder: Arc<dyn RequestRecorder> = StoreRecorder::new(store.clone(), 1000, vec![]);
    let deps = RuntimeDeps {
        store: store.clone(),
        state: state.clone(),
        ext: Extensions::default(),
        recorder: Some(recorder),
        offline: true,
        default_profile: None.into(),
        data_plane_keys: Some(StoreDataPlaneKeys::new(store.clone())),
        protocols: mb_protocols::ProtocolSettings::default(),
        stream: mb_engine::StreamConfig::default(),
        events: EventDeps::default(),
    };
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert!(
        registry.failed_mocks().is_empty(),
        "{:?}",
        registry.failed_mocks()
    );
    assert!(registry.channel_runtime(TEAM_A, "chat").is_some());
    assert!(
        registry
            .channel_runtime(DEFAULT_WORKSPACE, "chat")
            .is_none()
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = registry.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Setup {
        store,
        state,
        mock,
        token,
        other_ws,
        base: format!("ws://{addr}"),
        _registry: registry,
    }
}

/// The HTTP status of a refused WebSocket handshake; `None` when it is
/// accepted.
async fn handshake(url: &str, key_header: Option<&str>) -> Option<u16> {
    let mut req = url.into_client_request().unwrap();
    if let Some(key) = key_header {
        req.headers_mut().insert("x-api-key", key.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok(_) => None,
        Err(WsError::Http(resp)) => Some(resp.status().as_u16()),
        Err(e) => panic!("unexpected handshake error: {e}"),
    }
}

/// The mock's request-log entries of `kind`, waiting up to 5 s until
/// `pred` holds for one of them.
async fn wait_for_log(
    s: &Setup,
    kind: RequestKind,
    pred: impl Fn(&RequestLogEntry) -> bool,
) -> Vec<RequestLogEntry> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let entries = s
            .store
            .list_request_log(
                s.mock.id,
                &RequestLogFilter {
                    kind: Some(kind),
                    ..RequestLogFilter::default()
                },
            )
            .await
            .unwrap();
        if entries.iter().any(&pred) {
            return entries;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no matching entry in {entries:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn websocket_channel_enforces_workspace_data_plane_key() {
    let s = setup().await;
    let url = format!("{}/chat-a/chat", s.base);

    assert_eq!(handshake(&url, None).await, Some(401));
    assert_eq!(handshake(&url, Some("mb_bogus")).await, Some(401));
    // A data-plane key of another workspace is not one of this mock's.
    assert_eq!(handshake(&url, Some(&s.other_ws)).await, Some(401));
    assert_eq!(handshake(&url, Some(&s.token)).await, None);
    assert_eq!(
        handshake(&format!("{url}?api_key={}", s.token), None).await,
        None
    );

    // Refusals are recorded as event calls of the mock, in its workspace,
    // without the presented key.
    let entries = wait_for_log(&s, RequestKind::Event, |e| e.status == 401).await;
    let refused = entries.iter().find(|e| e.status == 401).unwrap();
    assert_eq!(refused.method, "GET");
    assert_eq!(refused.path, "/chat");
    assert!(
        !refused.request.to_string().contains("mb_bogus"),
        "{}",
        refused.request
    );
}

#[tokio::test]
async fn event_records_are_listed_under_the_mock_workspace() {
    let s = setup().await;
    let mut req = format!("{}/chat-a/chat", s.base)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("x-api-key", s.token.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::text(
        json!({"user": "ada", "text": "hello"}).to_string(),
    ))
    .await
    .unwrap();
    // The reply on `chatMessage` arrives.
    let reply = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => return t.to_string(),
                Some(Ok(_)) => continue,
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert!(reply["user"].is_string(), "{reply}");

    wait_for_log(&s, RequestKind::Event, |e| e.method == "RECEIVE").await;

    // Through the Admin API: the workspace-scoped path lists them with
    // kind `event`; the default workspace has no such mock.
    let app = mb_admin::router(mb_admin::AdminState::new(s.store.clone(), s.state.clone()));
    let get = |uri: String| {
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    let (status, list) = get(format!(
        "/api/v1/workspaces/{TEAM_A}/mocks/chat/requests?kind=event"
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let list = list.as_array().unwrap();
    assert!(!list.is_empty());
    assert!(list.iter().all(|e| e["kind"] == "event"), "{list:?}");
    assert!(list.iter().any(|e| e["method"] == "RECEIVE"), "{list:?}");
    let (status, _) = get("/api/v1/mocks/chat/requests?kind=event".into()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ------------------------------------------- same-named mocks, two workspaces

const ORDERS: &[u8] = include_bytes!("../../../fixtures/asyncapi/orders-amqp-2.6.yaml");

struct Twins {
    store: Arc<dyn ControlStore>,
    state: Arc<dyn StateStore>,
    registry: Arc<MockRegistry>,
    broker: Arc<MemoryBroker>,
    control: RegistryChannels,
    /// `orders` in `default`.
    default_mock: Mock,
    /// `orders` in `team-a`.
    team_mock: Mock,
    /// The Admin API, `http://host:port`.
    admin: String,
}

async fn orders_mock(store: &Arc<dyn ControlStore>, ws: &str, prefix: &str) -> Mock {
    let mock = store
        .create_mock(
            ws,
            NewMock {
                name: "orders".into(),
                route_prefix: Some(prefix.into()),
                persona: None,
                llm_profile: None,
                header_overrides: true,
                source: ResourceSource::Api,
                chaos: None,
                limits: Default::default(),
                data_plane_auth: DataPlaneAuthConfig::default(),
            },
        )
        .await
        .unwrap();
    let v = store
        .add_contract_version(
            mock.id,
            NewContractVersion {
                source: ORDERS.to_vec(),
                bundle: mb_import::compile(ORDERS).unwrap(),
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await
        .unwrap();
    store
        .activate_contract_version(mock.id, v.version)
        .await
        .unwrap();
    // `orderCreated` fires on every upserted order of the mock's sessions.
    store
        .put_channel_settings(&ChannelSettings {
            mock_id: mock.id,
            channel_id: "orderCreated".into(),
            enabled: true,
            triggers: Some(vec![Trigger::OnStateChange {
                resource_type: "orders".into(),
                kind: StateChangeKind::Upserted,
            }]),
            source: ResourceSource::Api,
            updated_at: Utc::now(),
        })
        .await
        .unwrap();
    mock
}

/// Mock `orders` in `default` and in `team-a`, both on the one memory
/// broker `mem` and one state store. `default`'s `cancelOrder` consumer is
/// disabled, so a cancellation on the shared queue is consumed by `team-a`
/// alone.
async fn twins() -> Twins {
    let store: Arc<dyn ControlStore> = Arc::new(
        SqliteControlStore::connect("sqlite::memory:")
            .await
            .unwrap(),
    );
    store.create_workspace(TEAM_A, "Team A").await.unwrap();
    let default_mock = orders_mock(&store, DEFAULT_WORKSPACE, "/orders").await;
    let team_mock = orders_mock(&store, TEAM_A, "/orders-a").await;
    store
        .put_channel_settings(&ChannelSettings {
            mock_id: default_mock.id,
            channel_id: "cancelOrder".into(),
            enabled: false,
            triggers: None,
            source: ResourceSource::Api,
            updated_at: Utc::now(),
        })
        .await
        .unwrap();

    let brokers = Brokers::new(
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
    );
    let broker = brokers.memory("mem").unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let recorder = StoreRecorder::new(store.clone(), 1000, vec![]);
    let deps = RuntimeDeps {
        store: store.clone(),
        state: state.clone(),
        ext: Extensions::default(),
        recorder: Some(recorder.clone() as Arc<dyn RequestRecorder>),
        offline: true,
        default_profile: None.into(),
        data_plane_keys: Some(StoreDataPlaneKeys::new(store.clone())),
        protocols: mb_protocols::ProtocolSettings::default(),
        stream: mb_engine::StreamConfig::default(),
        events: EventDeps {
            brokers: brokers.clone(),
            ..EventDeps::default()
        },
    };
    let registry = MockRegistry::new();
    registry.reload_all(&deps).await.unwrap();
    assert!(
        registry.failed_mocks().is_empty(),
        "{:?}",
        registry.failed_mocks()
    );
    let a = registry.channel_runtime(TEAM_A, "orders").unwrap();
    let d = registry
        .channel_runtime(DEFAULT_WORKSPACE, "orders")
        .unwrap();
    assert!(!Arc::ptr_eq(&a, &d));

    let live_recorder = recorder.clone();
    let live: LiveSubscribe = Arc::new(move || live_recorder.live());
    let shared: SharedChannelControl = Arc::new(RegistryChannels {
        registry: registry.clone(),
        brokers: brokers.clone(),
    });
    let app = mb_admin::router(AdminState {
        live: Some(live),
        ..AdminState::new(store.clone(), state.clone())
    })
    .layer(axum::Extension(shared));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Let the consumers subscribe.
    tokio::time::sleep(Duration::from_millis(100)).await;

    Twins {
        store,
        state,
        control: RegistryChannels {
            registry: registry.clone(),
            brokers,
        },
        registry,
        broker,
        default_mock,
        team_mock,
        admin: format!("http://{addr}"),
    }
}

impl Twins {
    /// The event entries of `mock` in the request log.
    async fn events(&self, mock: &Mock) -> Vec<RequestLogEntry> {
        self.store
            .list_request_log(
                mock.id,
                &RequestLogFilter {
                    kind: Some(RequestKind::Event),
                    ..RequestLogFilter::default()
                },
            )
            .await
            .unwrap()
    }

    /// `mock`'s event entries once `n` of them are `method`, waiting up to 5 s.
    async fn wait_events(&self, mock: &Mock, method: &str, n: usize) -> Vec<RequestLogEntry> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let entries = self.events(mock).await;
            if entries.iter().filter(|e| e.method == method).count() >= n {
                return entries;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "fewer than {n} {method} entries in {entries:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn count(&self, mock: &Mock, method: &str) -> usize {
        self.events(mock)
            .await
            .iter()
            .filter(|e| e.method == method)
            .count()
    }

    /// Opens the live tail of `orders` in `ws`.
    async fn tail(&self, ws: &str) -> reqwest::Response {
        let resp = reqwest::get(format!(
            "{}/api/v1/workspaces/{ws}/mocks/orders/requests/stream",
            self.admin
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        resp
    }
}

fn order_key(ws: &str, session: &str) -> SessionKey {
    SessionKey {
        workspace: ws.into(),
        mock: "orders".into(),
        session: session.into(),
    }
}

fn order(id: &str) -> StoredResource {
    StoredResource {
        resource_type: "orders".into(),
        id: id.into(),
        data: json!({"id": id, "status": "placed", "total": 10}),
    }
}

/// The SSE data read from `tail` within `wait` (keep-alives skipped).
async fn tail_data(tail: &mut reqwest::Response, wait: Duration) -> String {
    let mut out = String::new();
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(Ok(Some(chunk))) = tokio::time::timeout_at(deadline, tail.chunk()).await {
        let text = String::from_utf8_lossy(&chunk).into_owned();
        out.extend(
            text.lines()
                .filter(|l| l.starts_with("data:"))
                .map(|l| format!("{l}\n")),
        );
        if out.contains("PUBLISH") {
            break;
        }
    }
    out
}

#[tokio::test]
async fn publish_activity_and_live_tail_stay_in_their_workspace() {
    let t = twins().await;
    let mut team_tail = t.tail(TEAM_A).await;
    let mut default_tail = t.tail(DEFAULT_WORKSPACE).await;

    let published = t
        .control
        .publish(TEAM_A, "orders", "orderCreated", 2, None)
        .await
        .unwrap();
    assert_eq!(published.len(), 2);
    assert_eq!(t.broker.published().len(), 2);

    // Recorded under team-a's mock only.
    t.wait_events(&t.team_mock, "PUBLISH", 2).await;
    assert_eq!(t.count(&t.default_mock, "PUBLISH").await, 0);

    // Activity: team-a counts both, default none.
    assert_eq!(
        t.control.activity(TEAM_A, "orders")["orderCreated"].published,
        2
    );
    assert_eq!(
        t.control
            .activity(DEFAULT_WORKSPACE, "orders")
            .get("orderCreated")
            .map_or(0, |a| a.published),
        0
    );

    // Live tail: team-a's sees the publishes, default's does not.
    let seen = tail_data(&mut team_tail, Duration::from_secs(5)).await;
    assert!(seen.contains("PUBLISH"), "{seen}");
    let seen = tail_data(&mut default_tail, Duration::from_millis(500)).await;
    assert!(!seen.contains("PUBLISH"), "{seen}");

    // A partial failure through the real control reports what went out.
    t.broker.fail_publishes_after(1, usize::MAX);
    match t
        .control
        .publish(TEAM_A, "orders", "orderCreated", 3, None)
        .await
    {
        Err(ChannelControlError::Partial { published, error }) => {
            assert_eq!(published.len(), 1);
            assert!(
                matches!(*error, ChannelControlError::Failed(_)),
                "{error:?}"
            );
        }
        other => panic!("expected a partial failure, got {other:?}"),
    }
    t.broker.fail_publishes(0);
}

#[tokio::test]
async fn state_change_triggers_fire_only_in_their_workspace() {
    let t = twins().await;

    t.state
        .upsert(&order_key(TEAM_A, "s1"), order("41"))
        .await
        .unwrap();
    let entries = t.wait_events(&t.team_mock, "PUBLISH", 1).await;
    let publish = entries.iter().find(|e| e.method == "PUBLISH").unwrap();
    assert_eq!(publish.request["trigger"]["kind"], "on_state_change");
    assert_eq!(publish.session, "s1");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(t.count(&t.default_mock, "PUBLISH").await, 0);
    assert_eq!(t.broker.published().len(), 1);

    // And the other way round.
    t.state
        .upsert(&order_key(DEFAULT_WORKSPACE, "s1"), order("42"))
        .await
        .unwrap();
    t.wait_events(&t.default_mock, "PUBLISH", 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(t.count(&t.team_mock, "PUBLISH").await, 1);
}

#[tokio::test]
async fn consumed_message_state_effect_lands_in_the_consuming_workspace() {
    let t = twins().await;
    let bundle = mb_import::compile(ORDERS).unwrap();
    let binding = bundle.channel("cancelOrder").unwrap().binding.clone();
    t.broker.inject(
        &binding,
        InMessage {
            address: binding.address().to_string(),
            key: None,
            headers: Vec::new(),
            payload: br#"{"orderId":"c1","reason":"changed mind"}"#.to_vec(),
        },
    );
    t.wait_events(&t.team_mock, "RECEIVE", 1).await;

    let team = t
        .state
        .get(&order_key(TEAM_A, "default"), "cancellations", "c1")
        .await
        .unwrap();
    assert_eq!(team.unwrap().data["orderId"], "c1");
    let default = t
        .state
        .get(
            &order_key(DEFAULT_WORKSPACE, "default"),
            "cancellations",
            "c1",
        )
        .await
        .unwrap();
    assert!(default.is_none());
    assert_eq!(t.count(&t.default_mock, "RECEIVE").await, 0);
    // The registry still holds both runtimes.
    assert!(
        t.registry
            .channel_runtime(DEFAULT_WORKSPACE, "orders")
            .is_some()
    );
}
