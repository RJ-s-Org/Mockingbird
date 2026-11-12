//! Bulk creates whose records the world names by `request.records` (and
//! `answer.records`): Postmark's `POST /email/batch` takes and answers top-level
//! arrays (the root pointer `""`), others wrap them in an envelope (`/Messages`).
//! Offline, each answer holds one record per record sent, in order, the create's
//! `set` applied to each with `request.body` the record sent at its place; each is
//! stored under its own identity, and a `set` identity an earlier record of the call
//! or the session holds stores nothing over it.
#![cfg(feature = "http-rest")]

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use mb_engine::{Engine, EngineConfig};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};

use crate::support;

const CONTRACT: &str = r##"
openapi: 3.0.3
info: {title: batch, version: "1"}
paths:
  /email/batch:
    post:
      operationId: sendEmailBatch
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: array
              items: {$ref: '#/components/schemas/Email'}
      responses:
        "200":
          description: One result per message, in order
          content:
            application/json:
              schema:
                type: array
                items: {$ref: '#/components/schemas/SendResult'}
  /email/envelope:
    post:
      operationId: sendEmailEnvelope
      requestBody:
        required: true
        content:
          application/json:
            schema: {$ref: '#/components/schemas/Emails'}
      responses:
        "200":
          description: One result per message, in order
          content:
            application/json:
              schema:
                type: object
                properties:
                  Messages:
                    type: array
                    items: {$ref: '#/components/schemas/SendResult'}
  /email/queue:
    post:
      operationId: queueEmails
      requestBody:
        required: true
        content:
          application/json:
            schema: {$ref: '#/components/schemas/Emails'}
      responses:
        "202":
          description: Queued
          content:
            application/json:
              schema:
                type: object
                properties:
                  Queued: {type: integer}
  /messages/{MessageID}:
    get:
      operationId: getMessage
      parameters:
        - {name: MessageID, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: The message
          content:
            application/json:
              schema: {$ref: '#/components/schemas/SendResult'}
components:
  schemas:
    Email:
      type: object
      required: [From, To]
      properties:
        From: {type: string}
        To: {type: string}
        Subject: {type: string}
    Emails:
      type: object
      properties:
        Messages:
          type: array
          items: {$ref: '#/components/schemas/Email'}
    SendResult:
      type: object
      properties:
        To: {type: string}
        SubmittedAt: {type: string}
        MessageID: {type: string}
        ErrorCode: {type: integer}
        Message: {type: string}
"##;

fn world() -> Value {
    json!({
        "version": 1,
        "types": {"message": {"identity": {"member": "MessageID"}}},
        "operations": {
            "sendEmailBatch": {
                "kind": "create",
                "type": "message",
                "request": {"records": ""},
                "answer": {"records": ""},
                "set": {
                    "MessageID": "mint('[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}')",
                    "To": "request.body.To",
                    "ErrorCode": "0",
                    "Message": "'OK'"
                }
            },
            "sendEmailEnvelope": {
                "kind": "create",
                "type": "message",
                "request": {"records": "/Messages"},
                "answer": {"records": "/Messages"},
                "set": {"MessageID": "'e-' + request.body.Subject", "To": "request.body.To"}
            },
            "queueEmails": {
                "kind": "create",
                "type": "message",
                "request": {"records": "/Messages"},
                "set": {"MessageID": "'q-' + request.body.Subject", "ErrorCode": "0"}
            },
            "getMessage": {"kind": "read", "type": "message", "target": {"path_param": "MessageID"}}
        }
    })
}

/// The contract served with [`world`], offline (no model).
fn offline() -> axum::Router {
    let bundle = mb_import::compile(CONTRACT.as_bytes()).unwrap();
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rt = mb_protocols::MockRuntime::new(
        "batch",
        Arc::new(bundle),
        Arc::new(Engine::new(None, EngineConfig::default())),
        store,
    );
    let rt = support::with_effective(rt, world(), false);
    mb_protocols::http_rest::router(Arc::new(rt))
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String, Value) {
    let mut req = axum::http::Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let body = body.map_or_else(axum::body::Body::empty, |b| {
        axum::body::Body::from(b.to_string())
    });
    let (status, headers, body) = support::send_to(router, req.body(body).unwrap()).await;
    let source = headers
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    (status, source, body)
}

/// The message stored under `id`, read back from state.
async fn held(router: &axum::Router, id: &str) -> Value {
    let (status, source, body) = send(router, "GET", &format!("/messages/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{id}: {body}");
    assert_eq!(source, "state", "{id}: {body}");
    body
}

fn emails(sent: &[(&str, &str)]) -> Vec<Value> {
    sent.iter()
        .map(|(to, subject)| json!({"From": "me@example.com", "To": to, "Subject": subject}))
        .collect()
}

#[tokio::test]
async fn a_root_array_bulk_create_answers_one_record_per_request_element_in_order() {
    let router = offline();
    let to = ["a@example.com", "b@example.com", "c@example.com"];
    let sent = emails(&to.map(|t| (t, "hi")));
    let (status, _, body) = send(&router, "POST", "/email/batch", Some(Value::Array(sent))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answered = body.as_array().unwrap_or_else(|| panic!("{body}"));
    assert_eq!(answered.len(), to.len(), "{body}");
    let mut ids = Vec::new();
    for (result, to) in answered.iter().zip(to) {
        assert_eq!(result["To"], json!(to), "{body}");
        assert_eq!(result["ErrorCode"], json!(0), "{body}");
        assert_eq!(result["Message"], json!("OK"), "{body}");
        let id = result["MessageID"]
            .as_str()
            .unwrap_or_else(|| panic!("{body}"));
        assert_eq!(id.len(), 36, "{body}");
        ids.push(id.to_owned());
    }
    // Each is stored under its own id, with its own `To`.
    for (id, to) in ids.iter().zip(to) {
        let stored = held(&router, id).await;
        assert_eq!(stored["To"], json!(to), "{stored}");
        assert_eq!(stored["MessageID"], json!(id), "{stored}");
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), to.len(), "each message has its own id: {body}");
}

#[tokio::test]
async fn an_enveloped_bulk_create_is_aligned_and_keeps_the_first_of_a_repeated_identity() {
    let router = offline();
    // The first two set the same identity: the second answers the first's record
    // and stores nothing over it.
    let sent = emails(&[("dave", "s2"), ("erin", "s2"), ("frank", "s3")]);
    let (status, _, body) = send(
        &router,
        "POST",
        "/email/envelope",
        Some(json!({"Messages": sent})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answered = body["Messages"]
        .as_array()
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(answered.len(), 3, "{body}");
    let shown: Vec<(&Value, &Value)> = answered
        .iter()
        .map(|r| (&r["MessageID"], &r["To"]))
        .collect();
    assert_eq!(
        shown,
        vec![
            (&json!("e-s2"), &json!("dave")),
            (&json!("e-s2"), &json!("dave")),
            (&json!("e-s3"), &json!("frank")),
        ],
        "{body}"
    );
    assert_eq!(held(&router, "e-s2").await["To"], json!("dave"));
    assert_eq!(held(&router, "e-s3").await["To"], json!("frank"));
}

#[tokio::test]
async fn a_request_only_bulk_create_sets_and_stores_each_request_record_once() {
    let router = offline();
    let sent = emails(&[("alice", "s1"), ("bob", "s1"), ("carol", "s4")]);
    let (status, _, body) = send(
        &router,
        "POST",
        "/email/queue",
        Some(json!({"Messages": sent})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let first = held(&router, "q-s1").await;
    assert_eq!(first["To"], json!("alice"), "{first}");
    assert_eq!(first["ErrorCode"], json!(0), "{first}");
    assert_eq!(held(&router, "q-s4").await["To"], json!("carol"));
    // An identity the session holds already is not stored over.
    let again = emails(&[("dan", "s1")]);
    let (status, _, body) = send(
        &router,
        "POST",
        "/email/queue",
        Some(json!({"Messages": again})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    assert_eq!(held(&router, "q-s1").await["To"], json!("alice"));
}
