#![cfg(feature = "graphql")]
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_mcm::Bundle;
use mb_protocols::{MockRuntime, RequestRecord, RequestRecorder, graphql};
use mb_state::MemoryStore;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const SUBSCRIPTION: &str = r#"subscription { issueAdded(repositoryId: "r-1") { id title } }"#;

fn github() -> Arc<Bundle> {
    let path = format!(
        "{}/../../fixtures/graphql/github.graphql",
        env!("CARGO_MANIFEST_DIR")
    );
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

fn runtime(provider: Option<Arc<ScriptedProvider>>) -> MockRuntime {
    let dyn_provider = provider.map(|p| p as Arc<dyn Provider>);
    MockRuntime::new(
        "github",
        github(),
        Arc::new(Engine::new(dyn_provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

/// Serves `rt`; returns its `host:port`.
async fn serve(rt: MockRuntime) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr.to_string()
}

async fn connect(addr: &str, protocol: &str) -> Ws {
    let mut req = format!("ws://{addr}/graphql")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("sec-websocket-protocol", protocol.parse().unwrap());
    let (ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(resp.headers()["sec-websocket-protocol"], protocol);
    ws
}

async fn send(ws: &mut Ws, msg: Value) {
    ws.send(Message::text(msg.to_string())).await.unwrap();
}

/// The next text message as JSON, skipping pings and pongs.
async fn recv(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out")
            .unwrap()
            .unwrap();
        match msg {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// Opens a `graphql-transport-ws` connection initialised with `payload`.
async fn init(addr: &str, payload: Value) -> Ws {
    let mut ws = connect(addr, "graphql-transport-ws").await;
    send(
        &mut ws,
        json!({"type": "connection_init", "payload": payload}),
    )
    .await;
    assert_eq!(recv(&mut ws).await, json!({"type": "connection_ack"}));
    ws
}

fn assert_issue(msg: &Value, ty: &str) {
    assert_eq!(msg["type"], ty, "{msg}");
    assert_eq!(msg["id"], "1", "{msg}");
    let issue = &msg["payload"]["data"]["issueAdded"];
    let keys: Vec<&str> = issue
        .as_object()
        .unwrap_or_else(|| panic!("{msg}"))
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["id", "title"], "{msg}");
}

#[derive(Default)]
struct Records(Mutex<Vec<RequestRecord>>);

impl RequestRecorder for Records {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

#[tokio::test]
async fn subscription_transport_ws_receives_n_items() {
    let records = Arc::new(Records::default());
    let mut rt = runtime(None);
    rt.recorder = Some(records.clone());
    let addr = serve(rt).await;
    let mut ws = init(&addr, json!({"x-mockingbird-stream-items": "3"})).await;
    send(
        &mut ws,
        json!({"type": "subscribe", "id": "1", "payload": {"query": SUBSCRIPTION}}),
    )
    .await;
    for _ in 0..3 {
        assert_issue(&recv(&mut ws).await, "next");
    }
    // The stream stays open for state changes until the client completes.
    send(&mut ws, json!({"type": "complete", "id": "1"})).await;
    drop(ws);
    let mut reported = Vec::new();
    for _ in 0..50 {
        reported = records.0.lock().unwrap().drain(..).collect();
        if !reported.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(reported.len(), 1);
    let r = &reported[0];
    assert_eq!(r.operation_id.as_deref(), Some("Subscription.issueAdded"));
    assert_eq!(r.status, 200);
    assert_eq!(r.response, Some(json!({"items": 3})));
}

#[tokio::test]
async fn legacy_graphql_ws_protocol_accepted() {
    let addr = serve(runtime(None)).await;
    let mut ws = connect(&addr, "graphql-ws").await;
    send(
        &mut ws,
        json!({"type": "connection_init", "payload": {"x-mockingbird-stream-items": "2"}}),
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "connection_ack");
    send(
        &mut ws,
        json!({"type": "start", "id": "1", "payload": {"query": SUBSCRIPTION}}),
    )
    .await;
    for _ in 0..2 {
        assert_issue(&recv(&mut ws).await, "data");
    }
}

#[tokio::test]
async fn subscription_emits_state_change() {
    let addr = serve(runtime(None)).await;
    let mut ws = init(
        &addr,
        json!({"x-mockingbird-stream-items": "2", "x-mockingbird-session": "s1"}),
    )
    .await;
    send(
        &mut ws,
        json!({"type": "subscribe", "id": "1", "payload": {"query": SUBSCRIPTION}}),
    )
    .await;
    for _ in 0..2 {
        assert_issue(&recv(&mut ws).await, "next");
    }
    let created: Value = reqwest::Client::new()
        .post(format!("http://{addr}/graphql"))
        .header("x-mockingbird-session", "s1")
        .json(&json!({"query": r#"mutation { createIssue(input: {repositoryId: "r-1", title: "Crash"}) { id title } }"#}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let created = &created["data"]["createIssue"];
    assert!(created["id"].is_string(), "{created}");
    let msg = recv(&mut ws).await;
    assert_issue(&msg, "next");
    assert_eq!(&msg["payload"]["data"]["issueAdded"], created);

    // Changes in another session are not streamed.
    let other = reqwest::Client::new()
        .post(format!("http://{addr}/graphql"))
        .header("x-mockingbird-session", "s2")
        .json(&json!({"query": r#"mutation { createIssue(input: {repositoryId: "r-1", title: "Other"}) { id } }"#}))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 200);
    let more = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    assert!(more.is_err(), "no message from another session: {more:?}");
}

fn issue_line(i: usize) -> String {
    let issue = json!({
        "__typename": "Issue",
        "id": format!("i-{i}"),
        "number": i,
        "title": format!("Issue {i}"),
        "body": null,
        "state": "OPEN",
        "author": null,
        "repository": {
            "__typename": "Repository",
            "id": "r-1",
            "name": "hello",
            "owner": {
                "__typename": "User",
                "id": "u-1",
                "login": "octocat",
                "name": null,
                "createdAt": "2024-01-01T00:00:00Z",
                "repositories": [],
            },
            "isPrivate": false,
            "stargazerCount": 1,
            "issues": [],
        },
        "createdAt": "2024-01-01T00:00:00Z",
        "labels": [],
    });
    format!("{issue}\n")
}

#[tokio::test]
async fn complete_stops_engine_stream() {
    let lines: Vec<_> = (1..=40).map(|i| Ok(issue_line(i))).collect();
    let provider = Arc::new(
        ScriptedProvider::new(vec![])
            .with_streams(vec![Ok(lines)])
            .with_chunk_delay(Duration::from_millis(20)),
    );
    let addr = serve(runtime(Some(provider.clone()))).await;
    let mut ws = init(&addr, json!({"x-mockingbird-stream-items": "40"})).await;
    send(
        &mut ws,
        json!({"type": "subscribe", "id": "1", "payload": {"query": SUBSCRIPTION}}),
    )
    .await;
    let first = recv(&mut ws).await;
    assert_issue(&first, "next");
    assert_eq!(first["payload"]["data"]["issueAdded"]["title"], "Issue 1");
    send(&mut ws, json!({"type": "complete", "id": "1"})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_complete = provider.chunks_yielded();
    assert!(after_complete <= 8, "{after_complete}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(provider.chunks_yielded(), after_complete);
}

#[tokio::test]
async fn forced_status_sends_one_error_then_completes() {
    let addr = serve(runtime(None)).await;
    let mut ws = init(&addr, json!({"x-mockingbird-status": "404"})).await;
    send(
        &mut ws,
        json!({"type": "subscribe", "id": "1", "payload": {"query": SUBSCRIPTION}}),
    )
    .await;
    let msg = recv(&mut ws).await;
    assert_eq!(msg["type"], "next", "{msg}");
    assert_eq!(
        msg["payload"]["errors"][0]["extensions"]["code"], "NOT_FOUND",
        "{msg}"
    );
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "1"}));
}

#[tokio::test]
async fn upgrade_headers_are_overrides() {
    let addr = serve(runtime(None)).await;
    let mut req = format!("ws://{addr}/graphql")
        .into_client_request()
        .unwrap();
    let headers = req.headers_mut();
    headers.insert(
        "sec-websocket-protocol",
        "graphql-transport-ws".parse().unwrap(),
    );
    headers.insert("x-mockingbird-stream-items", "1".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    send(&mut ws, json!({"type": "connection_init"})).await;
    assert_eq!(recv(&mut ws).await["type"], "connection_ack");
    send(
        &mut ws,
        json!({"type": "subscribe", "id": "1", "payload": {"query": SUBSCRIPTION}}),
    )
    .await;
    assert_issue(&recv(&mut ws).await, "next");
    let more = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    assert!(more.is_err(), "one item only: {more:?}");
}

#[tokio::test]
async fn introspection_query_over_http_lists_subscription_type() {
    let addr = serve(runtime(None)).await;
    let body: Value = reqwest::Client::new()
        .post(format!("http://{addr}/graphql"))
        .json(&json!({"query": "{ __schema { subscriptionType { name fields { name } } } }"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["data"]["__schema"]["subscriptionType"],
        json!({"name": "Subscription", "fields": [{"name": "issueAdded"}]}),
        "{body}"
    );
}

#[tokio::test]
async fn shutdown_completes_open_subscriptions() {
    let (stop, rx) = tokio::sync::watch::channel(false);
    let mut rt = runtime(None);
    rt.shutdown = mb_protocols::Shutdown::new(rx);
    let addr = serve(rt).await;
    let mut ws = init(&addr, json!({"x-mockingbird-stream-items": "1"})).await;
    send(
        &mut ws,
        json!({"type": "subscribe", "id": "1", "payload": {"query": SUBSCRIPTION}}),
    )
    .await;
    assert_issue(&recv(&mut ws).await, "next");
    stop.send_replace(true);
    assert_eq!(recv(&mut ws).await, json!({"type": "complete", "id": "1"}));
}
