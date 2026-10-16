#![cfg(feature = "events")]
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use mb_engine::{Engine, EngineConfig};
use mb_mcm::Bundle;
use mb_protocols::events::{CLOSE_RUNTIME_REPLACED, ChannelRuntime, EventDeps, EventSetup};
use mb_protocols::{MockRuntime, RequestKind, RequestRecord, RequestRecorder};
use mb_state::{MemoryStore, StateStore};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

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

    /// The first record matching `pred`, waiting up to 5 s for it.
    async fn find(&self, pred: impl Fn(&RequestRecord) -> bool) -> RequestRecord {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(r) = self.records().into_iter().find(&pred) {
                return r;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no matching record in {:?}",
                self.records()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn chat() -> Bundle {
    mb_import::compile(include_bytes!(
        "../../../fixtures/asyncapi/chat-ws-2.6.yaml"
    ))
    .unwrap()
}

fn orders() -> Bundle {
    mb_import::compile(include_bytes!(
        "../../../fixtures/asyncapi/orders-amqp-2.6.yaml"
    ))
    .unwrap()
}

struct Harness {
    channels: Arc<ChannelRuntime>,
    records: Arc<Capture>,
    engine: Arc<Engine>,
    bundle: Arc<Bundle>,
}

fn runtime(bundle: Bundle) -> Harness {
    let records = Arc::new(Capture::default());
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let engine = Arc::new(Engine::new(None, EngineConfig::default()));
    let bundle = Arc::new(bundle);
    let mut rt = MockRuntime::new("chat", Arc::clone(&bundle), Arc::clone(&engine), state);
    rt.recorder = Some(Arc::clone(&records) as Arc<dyn RequestRecorder>);
    let channels = ChannelRuntime::start(Arc::new(rt), EventDeps::default(), EventSetup::default());
    Harness {
        channels,
        records,
        engine,
        bundle,
    }
}

/// Serve the WebSocket router on an ephemeral port; its `ws://host:port`.
async fn serve(h: &Harness) -> String {
    let router = h.channels.ws_router().expect("a WebSocket router");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("ws://{addr}")
}

async fn connect(base: &str) -> Client {
    tokio_tungstenite::connect_async(format!("{base}/chat"))
        .await
        .unwrap()
        .0
}

/// The next text frame, skipping pings; panics after 5 s.
async fn next_text(ws: &mut Client) -> String {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a frame");
        match msg {
            Some(Ok(Message::Text(t))) => return t.to_string(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// The close code the server sends, skipping other frames; panics after 5 s.
async fn close_code(ws: &mut Client) -> u16 {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a close");
        match msg {
            Some(Ok(Message::Close(Some(frame)))) => return u16::from(frame.code),
            Some(Ok(_)) => continue,
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

/// Whether `text` is a valid `chatMessage` payload.
fn is_chat_message(h: &Harness, text: &str) -> bool {
    let value: Value = serde_json::from_str(text).unwrap();
    let channel = h.bundle.channel("chatMessage").unwrap();
    h.engine
        .validate_message(&h.bundle, channel, &value)
        .is_ok()
}

#[tokio::test]
async fn ws_client_receives_manual_publish() {
    let h = runtime(chat());
    let base = serve(&h).await;
    let mut ws = connect(&base).await;
    let published = h
        .channels
        .publish_manual("chatMessage", 2, None)
        .await
        .unwrap();
    assert_eq!(published.len(), 2);
    for _ in 0..2 {
        let text = next_text(&mut ws).await;
        assert!(is_chat_message(&h, &text), "invalid message {text}");
    }
}

#[tokio::test]
async fn ws_publish_without_clients_succeeds_and_records() {
    let h = runtime(chat());
    let published = h
        .channels
        .publish_manual("chatMessage", 1, None)
        .await
        .unwrap();
    assert_eq!(published[0].address, "/chat");
    let record = h
        .records
        .find(|r| r.method == "PUBLISH" && r.operation_id.as_deref() == Some("chatMessage"))
        .await;
    assert_eq!(record.status, 200);
}

#[tokio::test]
async fn ws_message_validated_recorded_and_replied_to_sender() {
    let h = runtime(chat());
    let base = serve(&h).await;
    let mut a = connect(&base).await;
    let mut b = connect(&base).await;
    a.send(Message::text(r#"{"user":"a","text":"hi"}"#))
        .await
        .unwrap();

    let reply = next_text(&mut a).await;
    assert!(is_chat_message(&h, &reply), "invalid reply {reply}");
    let silent = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            match b.next().await {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                other => return other,
            }
        }
    })
    .await;
    assert!(silent.is_err(), "B received {silent:?}");

    let record = h.records.find(|r| r.method == "RECEIVE").await;
    assert_eq!(record.kind, RequestKind::Event);
    assert_eq!(record.path, "/chat");
    assert_eq!(record.status, 200);
    assert_eq!(record.operation_id.as_deref(), Some("sendMessage"));
}

#[tokio::test]
async fn ws_invalid_frame_recorded_and_socket_stays_open() {
    let h = runtime(chat());
    let base = serve(&h).await;
    let mut ws = connect(&base).await;
    ws.send(Message::text("nope")).await.unwrap();
    let record = h.records.find(|r| r.method == "RECEIVE").await;
    assert_eq!(record.status, 400);

    ws.send(Message::text(r#"{"user":"a","text":"again"}"#))
        .await
        .unwrap();
    let reply = next_text(&mut ws).await;
    assert!(is_chat_message(&h, &reply), "invalid reply {reply}");
    let ok = h
        .records
        .find(|r| r.method == "RECEIVE" && r.status == 200)
        .await;
    assert_eq!(ok.path, "/chat");
}

#[tokio::test]
async fn ws_oversized_frame_closes_1009() {
    let h = runtime(chat());
    let base = serve(&h).await;
    let mut ws = connect(&base).await;
    let big = "x".repeat(1024 * 1024 + 512 * 1024);
    // The server may close before the whole frame is written.
    let _ = ws.send(Message::text(big)).await;
    assert_eq!(close_code(&mut ws).await, 1009);
}

#[tokio::test]
async fn ws_runtime_drop_closes_1012() {
    let h = runtime(chat());
    let base = serve(&h).await;
    let mut ws = connect(&base).await;
    let Harness { channels, .. } = h;
    drop(channels);
    assert_eq!(close_code(&mut ws).await, CLOSE_RUNTIME_REPLACED);
    assert_eq!(CLOSE_RUNTIME_REPLACED, 1012);
}

#[tokio::test]
async fn ws_unknown_path_is_not_upgraded() {
    let h = runtime(chat());
    let base = serve(&h).await;
    assert!(
        tokio_tungstenite::connect_async(format!("{base}/other"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn no_ws_channels_no_router() {
    let h = runtime(orders());
    assert!(h.channels.ws_router().is_none());
}
