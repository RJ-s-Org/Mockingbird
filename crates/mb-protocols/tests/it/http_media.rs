#![cfg(test)]
//! REST operations whose responses declare a non-JSON media type are
//! answered in that media type: a declared example as written, a faked
//! text body for `text/*`, XML through the SOAP XML mapper, and
//! `application/octet-stream` only from a declared example.
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::MockRuntime;
use mb_protocols::http_rest;
use mb_state::MemoryStore;
use serde_json::{Value, json};
use tower::ServiceExt;

const MEDIA_CONTRACT: &str = r##"
openapi: 3.0.3
info: {title: Media API, version: "1"}
paths:
  /report:
    get:
      operationId: getReport
      responses:
        "200":
          description: A CSV report
          content:
            text/csv:
              schema: {type: string}
              example: "id,name\n1,Ada\n"
  /greeting:
    get:
      operationId: getGreeting
      responses:
        "200":
          description: Plain text
          content:
            text/plain:
              schema: {type: string, enum: [hello, bonjour]}
  /notes:
    get:
      operationId: getNotes
      responses:
        "200":
          description: Plain text without a schema
          content:
            text/plain: {}
  /pets/{id}:
    get:
      operationId: getPet
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: A pet as XML
          content:
            application/xml:
              schema: {$ref: "#/components/schemas/Pet"}
  /feed:
    get:
      operationId: getFeed
      responses:
        "200":
          description: An XML example
          content:
            application/rss+xml:
              example: "<rss version=\"2.0\"><channel/></rss>"
  /export:
    get:
      operationId: getExport
      responses:
        "200":
          description: Bytes from an example
          content:
            application/octet-stream:
              schema: {type: string, format: binary}
              examples:
                sample: {value: "PK-bytes"}
  /blob:
    get:
      operationId: getBlob
      responses:
        "200":
          description: Bytes without an example
          content:
            application/octet-stream:
              schema: {type: string, format: binary}
components:
  schemas:
    Pet:
      type: object
      required: [name, age]
      properties:
        name: {type: string}
        age: {type: integer}
"##;

struct Harness {
    router: Router,
    provider: Option<Arc<ScriptedProvider>>,
}

fn harness(script: Option<Vec<Value>>) -> Harness {
    let provider = script.map(|s| {
        Arc::new(ScriptedProvider::new(
            s.into_iter().map(|v| Ok(v.to_string())).collect(),
        ))
    });
    let rt = Arc::new(MockRuntime::new(
        "media",
        Arc::new(compile(MEDIA_CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            provider.clone().map(|p| p as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    ));
    Harness {
        router: http_rest::router(rt),
        provider,
    }
}

async fn get(h: &Harness, uri: &str) -> (StatusCode, String, String) {
    let resp: Response = h
        .router
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, ct, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn a_declared_text_example_is_served_as_written() {
    let h = harness(None);
    let (status, ct, body) = get(&h, "/report").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "text/csv");
    assert_eq!(body, "id,name\n1,Ada\n");
}

#[tokio::test]
async fn a_text_body_without_an_example_is_faked_as_text() {
    let h = harness(None);
    let (status, ct, body) = get(&h, "/greeting").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "text/plain");
    assert!(["hello", "bonjour"].contains(&body.as_str()), "{body:?}");

    let (_, ct, body) = get(&h, "/notes").await;
    assert_eq!(ct, "text/plain");
    assert!(!body.is_empty());
    assert!(!body.starts_with('"'), "not JSON-quoted: {body:?}");
}

#[tokio::test]
async fn text_media_never_calls_the_llm() {
    let h = harness(Some(vec![]));
    let (_, ct, body) = get(&h, "/report").await;
    assert_eq!(
        (ct.as_str(), body.as_str()),
        ("text/csv", "id,name\n1,Ada\n")
    );
    let (_, ct, _) = get(&h, "/greeting").await;
    assert_eq!(ct, "text/plain");
    assert!(h.provider.as_ref().unwrap().requests().is_empty());
}

#[tokio::test]
async fn an_xml_response_is_rendered_by_the_xml_mapper() {
    let h = harness(None);
    let (status, ct, body) = get(&h, "/pets/p1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "application/xml");
    assert!(body.starts_with("<Pet>"), "{body}");
    assert!(body.contains("<name>") && body.contains("<age>"), "{body}");
    assert!(body.ends_with("</Pet>"), "{body}");
}

#[tokio::test]
async fn an_xml_response_from_the_llm_is_rendered_as_xml() {
    let h = harness(Some(vec![json!({"name": "Rex", "age": 3})]));
    let (_, ct, body) = get(&h, "/pets/p1").await;
    assert_eq!(ct, "application/xml");
    assert_eq!(body, "<Pet><age>3</age><name>Rex</name></Pet>");
}

#[tokio::test]
async fn a_declared_xml_example_is_served_as_written() {
    let h = harness(None);
    let (_, ct, body) = get(&h, "/feed").await;
    assert_eq!(ct, "application/rss+xml");
    assert_eq!(body, "<rss version=\"2.0\"><channel/></rss>");
}

#[tokio::test]
async fn octet_stream_is_served_only_from_a_declared_example() {
    let h = harness(None);
    let (status, ct, body) = get(&h, "/export").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "application/octet-stream");
    assert_eq!(body, "PK-bytes");

    let (status, ct, body) = get(&h, "/blob").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "");
    assert_eq!(body, "");
}
