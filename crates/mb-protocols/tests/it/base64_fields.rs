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
use mb_llm::{Provider, ScriptedProvider};
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
                encoding: {type: string}
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
components:
  schemas:
    File:
      type: object
      required: [name, encoding, content]
      properties:
        name: {type: string}
        encoding: {type: string}
        content: {type: string}
    Blob:
      type: object
      required: [id, content]
      properties:
        id: {type: string}
        content: {type: string}
        encoding: {type: string}
        data: {type: string, format: byte}
"##;

fn router(script: Vec<Value>) -> axum::Router {
    let provider = Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ));
    let rt = MockRuntime::new(
        "contents",
        Arc::new(compile(CONTENTS.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        CachingStateStore::wrap(
            Arc::new(MemoryStore::new(Duration::from_secs(3600))) as Arc<dyn StateStore>,
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
