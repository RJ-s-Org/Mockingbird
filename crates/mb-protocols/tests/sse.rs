#![cfg(feature = "sse")]
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use mb_engine::{Engine, EngineConfig, SchemaValidator};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_mcm::Bundle;
use mb_protocols::http_rest;
use mb_protocols::{MockRuntime, RequestRecord, RequestRecorder};
use mb_state::MemoryStore;
use serde_json::{Value, json};

fn events() -> Arc<Bundle> {
    let path = format!(
        "{}/../../fixtures/openapi/events-3.1.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

fn runtime(provider: Option<Arc<ScriptedProvider>>) -> MockRuntime {
    let dyn_provider = provider.map(|p| p as Arc<dyn Provider>);
    MockRuntime::new(
        "events",
        events(),
        Arc::new(Engine::new(dyn_provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

/// Serves `rt` on an ephemeral port; returns its base URL.
async fn serve(rt: MockRuntime) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http_rest::router(Arc::new(rt));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// One SSE frame: a comment, or an event with its fields.
#[derive(Debug, Default)]
struct Frame {
    comment: Option<String>,
    event: Option<String>,
    id: Option<String>,
    data: Option<String>,
}

impl Frame {
    fn json(&self) -> Value {
        serde_json::from_str(self.data.as_deref().unwrap()).unwrap()
    }
}

/// Splits an SSE body into frames.
struct Reader {
    body: BoxStream<'static, reqwest::Result<axum::body::Bytes>>,
    buf: String,
}

impl Reader {
    fn new(resp: reqwest::Response) -> Self {
        Self {
            body: resp.bytes_stream().boxed(),
            buf: String::new(),
        }
    }

    async fn next_frame(&mut self) -> Frame {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let raw: String = self.buf.drain(..end + 2).collect();
                let mut frame = Frame::default();
                for line in raw.lines().filter(|l| !l.is_empty()) {
                    let (name, value) = line.split_once(": ").unwrap_or((line, ""));
                    let value = Some(value.to_string());
                    match name {
                        "" => frame.comment = value,
                        "event" => frame.event = value,
                        "id" => frame.id = value,
                        "data" => frame.data = value,
                        other => panic!("unexpected field {other:?} in {raw:?}"),
                    }
                }
                return frame;
            }
            let chunk = self.body.next().await.unwrap().unwrap();
            self.buf.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }

    /// The next frame that carries data, skipping comments.
    async fn next_event(&mut self) -> Frame {
        loop {
            let frame = self.next_frame().await;
            if frame.data.is_some() {
                return frame;
            }
        }
    }
}

async fn open(base: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut req = reqwest::Client::new().get(format!("{base}/ticks"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.send().await.unwrap()
}

fn tick_validator(bundle: &Bundle) -> SchemaValidator {
    let tick = json!({"$ref": "#/$defs/Tick"});
    SchemaValidator::new(&bundle.with_reachable_defs(&tick)).unwrap()
}

#[derive(Default)]
struct Records(Mutex<Vec<RequestRecord>>);

impl RequestRecorder for Records {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

async fn timeout<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), f)
        .await
        .unwrap()
}

#[tokio::test]
async fn sse_streams_n_events() {
    let bundle = events();
    let records = Arc::new(Records::default());
    let mut rt = runtime(None);
    rt.recorder = Some(records.clone());
    let base = serve(rt).await;
    let resp = open(&base, &[("x-mockingbird-stream-items", "3")]).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    assert_eq!(resp.headers()["cache-control"], "no-cache");
    let mut reader = Reader::new(resp);
    let validator = tick_validator(&bundle);
    for seq in 1..=3 {
        let frame = timeout(reader.next_event()).await;
        assert_eq!(frame.event.as_deref(), Some("tick"));
        assert_eq!(frame.id, Some(seq.to_string()));
        let data = frame.data.as_deref().unwrap();
        assert!(!data.contains('\n'), "compact JSON");
        assert!(validator.validate(&frame.json()).is_ok(), "{data}");
    }
    // The stream stays open after the generated items, with nothing else
    // to say.
    let more = tokio::time::timeout(Duration::from_millis(200), reader.next_event()).await;
    assert!(more.is_err(), "no further events");
    assert!(
        records.0.lock().unwrap().is_empty(),
        "reported on drop only"
    );

    drop(reader);
    timeout(async {
        while records.0.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let recs = records.0.lock().unwrap();
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!(r.status, 200);
    assert_eq!(r.operation_id.as_deref(), Some("streamTicks"));
    assert_eq!(r.method, "GET");
    assert_eq!(r.path, "/ticks");
    assert_eq!(r.source.as_deref(), Some("fallback"));
    assert_eq!(r.response, Some(json!({"items": 3})));
}

#[tokio::test]
async fn sse_keepalive_comment_sent() {
    let mut rt = runtime(None);
    rt.protocols.sse_keepalive = Duration::from_millis(100);
    let base = serve(rt).await;
    let resp = open(&base, &[("x-mockingbird-stream-items", "1")]).await;
    let mut reader = Reader::new(resp);
    let first = timeout(reader.next_event()).await;
    assert_eq!(first.id.as_deref(), Some("1"));
    let frame = tokio::time::timeout(Duration::from_millis(500), reader.next_frame())
        .await
        .unwrap();
    assert_eq!(frame.comment.as_deref(), Some("keepalive"));
    assert!(frame.data.is_none());
}

fn tick_lines(n: usize) -> Vec<Result<String, mb_llm::LlmError>> {
    (0..n)
        .map(|i| {
            Ok(format!(
                "{}\n",
                json!({"id": format!("t{i}"), "seq": i, "price": 1.5})
            ))
        })
        .collect()
}

#[tokio::test]
async fn sse_disconnect_stops_generation() {
    let provider = Arc::new(
        ScriptedProvider::new(vec![])
            .with_streams(vec![Ok(tick_lines(50))])
            .with_chunk_delay(Duration::from_millis(20)),
    );
    let base = serve(runtime(Some(provider.clone()))).await;
    let resp = open(&base, &[("x-mockingbird-stream-items", "50")]).await;
    let mut reader = Reader::new(resp);
    for _ in 0..2 {
        let frame = timeout(reader.next_event()).await;
        assert_eq!(frame.event.as_deref(), Some("tick"));
    }
    drop(reader);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_drop = provider.chunks_yielded();
    assert!(after_drop <= 5, "{after_drop}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(provider.chunks_yielded(), after_drop);
}

#[tokio::test]
async fn sse_emits_state_changes() {
    let base = serve(runtime(None)).await;
    let session = ("x-mockingbird-session", "s1");
    let resp = open(&base, &[("x-mockingbird-stream-items", "2"), session]).await;
    let mut reader = Reader::new(resp);
    for _ in 0..2 {
        timeout(reader.next_event()).await;
    }
    let created = reqwest::Client::new()
        .post(format!("{base}/ticks"))
        .header(session.0, session.1)
        .json(&json!({"price": 2.5}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.unwrap();
    let frame = timeout(reader.next_event()).await;
    assert_eq!(frame.event.as_deref(), Some("tick"));
    assert_eq!(frame.id.as_deref(), Some("3"));
    assert_eq!(frame.json(), created);

    // Another session's ticks are not streamed.
    let other = reqwest::Client::new()
        .post(format!("{base}/ticks"))
        .header(session.0, "s2")
        .json(&json!({"price": 3.5}))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 201);
    let more = tokio::time::timeout(Duration::from_millis(200), reader.next_event()).await;
    assert!(more.is_err(), "no event from another session");
}

#[tokio::test]
async fn sse_forced_status_returns_error_variant() {
    let base = serve(runtime(None)).await;
    let resp = open(&base, &[("x-mockingbird-status", "503")]).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let body: Value = resp.json().await.unwrap();
    assert!(body["code"].is_string(), "{body}");
    assert!(body["message"].is_string(), "{body}");

    // An undeclared forced status is the unary path's 400 problem.
    let resp = open(&base, &[("x-mockingbird-status", "418")]).await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn sse_latency_delays_first_event() {
    let base = serve(runtime(None)).await;
    let started = std::time::Instant::now();
    let resp = open(
        &base,
        &[
            ("x-mockingbird-stream-items", "1"),
            ("x-mockingbird-latency", "300ms"),
        ],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let mut reader = Reader::new(resp);
    timeout(reader.next_event()).await;
    assert!(started.elapsed() >= Duration::from_millis(300));
}
