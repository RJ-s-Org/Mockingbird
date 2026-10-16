//! P3 × P5: event channels of a mock outside the `default` workspace. The
//! WebSocket channels enforce the mock's data-plane auth with the
//! workspace's keys, and event records carry the mock's workspace.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use mb_ext::Extensions;
use mb_protocols::RequestRecorder;
use mb_protocols::events::EventDeps;
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    ApiKey, ContractStatus, ControlStore, DEFAULT_WORKSPACE, DataPlaneAuthConfig,
    DataPlaneAuthMode, Mock, NewContractVersion, NewMock, RequestKind, RequestLogEntry,
    RequestLogFilter, ResourceSource, SqliteControlStore,
};
use mockingbird::dp_keys::StoreDataPlaneKeys;
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
