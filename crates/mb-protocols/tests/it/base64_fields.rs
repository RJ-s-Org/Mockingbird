//! Base64 fields are valid base64 on every answer a mock serves, whatever made
//! it: the model (which may write `encoding: "utf-8"` beside the plain text it
//! was asked for), state (a record built from a request that sent its content as
//! text), or a cached answer. Driven over HTTP with a scripted LLM.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use base64::Engine as _;
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{LlmError, Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{CachingStateStore, MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

const CONTENTS: &str = r##"
openapi: 3.0.3
info: {title: contents, version: "1"}
paths:
  /repos/{owner}/{repo}/readme:
    get:
      operationId: getReadme
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: the README
          content: {application/json: {schema: {$ref: "#/components/schemas/File"}}}
  /blobs:
    post:
      operationId: createBlob
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [content]
              properties:
                content: {type: string}
                encoding: {type: string, enum: [base64, utf-8]}
                data: {type: string}
      responses:
        "201":
          description: created
          content: {application/json: {schema: {$ref: "#/components/schemas/Blob"}}}
  /blobs/{id}:
    get:
      operationId: getBlob
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: the blob
          content: {application/json: {schema: {$ref: "#/components/schemas/Blob"}}}
        "404": {description: missing}
  /files/{file_id}:
    get:
      operationId: getFile
      parameters: [{name: file_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: the file
          content: {application/json: {schema: {$ref: "#/components/schemas/Stored"}}}
  /files/{file_id}/close:
    post:
      operationId: closeFile
      parameters: [{name: file_id, in: path, required: true, schema: {type: string}}]
      responses:
        "204": {description: closed}
components:
  schemas:
    Stored:
      type: object
      required: [id, status, content, encoding]
      properties:
        id: {type: string}
        status: {type: string, enum: [open, closed]}
        content: {type: string}
        encoding: {type: string, enum: [base64, utf-8]}
    File:
      type: object
      required: [name, encoding, content]
      properties:
        name: {type: string}
        encoding: {type: string, enum: [base64, utf-8]}
        content: {type: string}
    Blob:
      type: object
      required: [id, content]
      properties:
        id: {type: string}
        content: {type: string}
        encoding: {type: string, enum: [base64, utf-8]}
        data: {type: string, format: byte}
"##;

fn router(script: Vec<Value>) -> axum::Router {
    router_with(script.into_iter().map(|v| Ok(v.to_string())).collect())
}

fn router_with(script: Vec<Result<String, LlmError>>) -> axum::Router {
    router_on(
        script,
        Arc::new(MemoryStore::new(Duration::from_secs(3600))) as Arc<dyn StateStore>,
    )
}

/// [`router_with`] over `store`, which a test may write to directly.
fn router_on(script: Vec<Result<String, LlmError>>, store: Arc<dyn StateStore>) -> axum::Router {
    let provider = Arc::new(ScriptedProvider::new(script));
    let rt = MockRuntime::new(
        "contents",
        Arc::new(compile(CONTENTS.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        CachingStateStore::wrap(
            store,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
    );
    http_rest::router(Arc::new(rt))
}

async fn send(router: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    send_with_source(router, method, uri, body).await.1
}

/// The answer's `X-Mockingbird-Source` and body.
async fn send_with_source(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (String, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let source = resp
        .headers()
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (source, serde_json::from_slice(&bytes).unwrap())
}

fn decoded(s: &str) -> String {
    let compact: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(compact)
            .unwrap_or_else(|e| panic!("{s:?} is not base64: {e}")),
    )
    .unwrap()
}

/// The `gh` trial's README: asked for plain text, the model wrote it with
/// `encoding: "utf-8"`, and it was served as it was (`gh repo view`: "illegal
/// base64 data at input byte 0"). The server encodes it and says so, on the
/// generated answer and on the cached repeat.
#[tokio::test]
async fn a_readme_written_as_utf8_text_is_served_as_base64() {
    let router = router(vec![json!({
        "name": "README.md",
        "encoding": "utf-8",
        "content": "# trellis — a small, fast HTTP router for Rust",
    })]);
    for expected in ["llm", "cache"] {
        let (source, readme) = send_with_source(&router, "GET", "/repos/o/r/readme", None).await;
        assert_eq!(source, expected);
        assert_eq!(readme["encoding"], "base64", "{readme}");
        assert_eq!(
            decoded(readme["content"].as_str().unwrap()),
            "# trellis — a small, fast HTTP router for Rust"
        );
    }
}

/// A record built from a request that sent its content as text (GitHub's
/// `encoding: "utf-8"` blobs) is answered with it encoded, by the create and by
/// a later read from state; a `format: byte` member too.
#[tokio::test]
async fn a_record_stored_with_text_content_is_read_as_base64() {
    let router = router(vec![]);
    let made = send(
        &router,
        "POST",
        "/blobs",
        Some(json!({"content": "hello, world", "encoding": "utf-8", "data": "raw bytes"})),
    )
    .await;
    assert_eq!(made["encoding"], "base64", "{made}");
    assert_eq!(decoded(made["content"].as_str().unwrap()), "hello, world");
    let id = made["id"].as_str().unwrap();
    let read = send(&router, "GET", &format!("/blobs/{id}"), None).await;
    assert_eq!(read["encoding"], "base64", "{read}");
    assert_eq!(decoded(read["content"].as_str().unwrap()), "hello, world");
    assert_eq!(decoded(read["data"].as_str().unwrap()), "raw bytes");
}

fn png() -> String {
    base64::engine::general_purpose::STANDARD.encode([
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00, 0xfe,
    ])
}

/// A client's binary file (valid base64 of bytes that are no UTF-8) goes through
/// state unchanged: stored by a create, served back by it and by a later read.
#[tokio::test]
async fn a_client_s_binary_file_round_trips_through_state() {
    let router = router(vec![]);
    let png = png();
    let made = send(
        &router,
        "POST",
        "/blobs",
        Some(json!({"content": png, "encoding": "base64", "data": png})),
    )
    .await;
    assert_eq!(made["content"], png, "{made}");
    assert_eq!(made["data"], png, "{made}");
    let id = made["id"].as_str().unwrap();
    let (source, read) = send_with_source(&router, "GET", &format!("/blobs/{id}"), None).await;
    assert_eq!(source, "state");
    assert_eq!(read["content"], png, "{read}");
    assert_eq!(read["encoding"], "base64", "{read}");
    assert_eq!(read["data"], png, "{read}");
}

/// A read that cannot catch up with a later write is answered from state (the
/// catch-up fallback). State holds the file as text (a record written as the client
/// sent it, `encoding: "utf-8"`); that answer is encoded on its way out like any
/// other.
#[tokio::test]
async fn a_catch_up_fallback_serves_valid_base64() {
    let down = || Err(LlmError::Transport("down".into()));
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let key = mb_state::SessionKey {
        workspace: "default".into(),
        mock: "contents".into(),
        session: "s1".into(),
    };
    store
        .upsert(
            &key,
            mb_state::StoredResource {
                resource_type: "files".into(),
                id: "f1".into(),
                data: json!({"id": "f1", "status": "open", "content": "# Notes", "encoding": "utf-8"}),
            },
        )
        .await
        .unwrap();
    let router = router_on(vec![down(), down()], Arc::clone(&store));
    let req = Request::builder()
        .method("POST")
        .uri("/files/f1/close")
        .header("x-mockingbird-session", "s1")
        .body(Body::empty())
        .unwrap();
    assert!(
        router
            .clone()
            .oneshot(req)
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let (source, read) = send_with_source(&router, "GET", "/files/f1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["encoding"], "base64", "{read}");
    assert_eq!(
        decoded(read["content"].as_str().unwrap()),
        "# Notes",
        "{read}"
    );
    // What state holds is left as it was.
    let held = store.get(&key, "files", "f1").await.unwrap().unwrap();
    assert_eq!(held.data["content"], "# Notes");
}
