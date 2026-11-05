//! A create whose 2xx answer holds no record but declares a header carrying the new
//! record's id (a `*-id` header, or a `Location` naming the item route): the id is
//! minted, the request stored as the record under it, and the same id served in the
//! header, so later reads by it find the record. Driven over HTTP with a scripted LLM.
#![cfg(test)]
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Capabilities, ChatRequest, ChatResponse, LlmError, Provider, ResponseFormat, Usage};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Tickets answer an id header and a body of line ids; a ticket's lines answer an id
/// header and no body; boards answer a `Location` and no body.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Desk, version: "1"}
paths:
  /tickets:
    post:
      operationId: createTicket
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewTicket"}}}
      responses:
        "201":
          description: created
          headers:
            Resource-ID: {schema: {type: integer}}
          content: {application/json: {schema: {type: object, properties: {lineIds: {type: array, items: {type: integer}}}}}}
  /tickets/{ticketId}:
    parameters: [{name: ticketId, in: path, required: true, schema: {type: integer}}]
    get:
      operationId: getTicket
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}
        "404": {description: missing}
  /tickets/{ticketId}/lines:
    parameters: [{name: ticketId, in: path, required: true, schema: {type: integer}}]
    post:
      operationId: createLine
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}
      responses:
        "201":
          description: created
          headers:
            Resource-ID: {schema: {type: integer}}
  /tickets/{ticketId}/lines/{lineId}:
    parameters:
      - {name: ticketId, in: path, required: true, schema: {type: integer}}
      - {name: lineId, in: path, required: true, schema: {type: integer}}
    get:
      operationId: getLine
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}
  /boards:
    post:
      operationId: createBoard
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201":
          description: created
          headers:
            Location: {schema: {type: string}}
  /boards/{boardId}:
    get:
      operationId: getBoard
      parameters: [{name: boardId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}
  /jobs:
    post:
      operationId: createJob
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {kind: {type: string}}}}}
      responses:
        "202":
          description: accepted
          headers:
            X-Account-Id: {schema: {type: string}}
  /jobs/{jobId}:
    get:
      operationId: getJob
      parameters: [{name: jobId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Job"}}}}
  /accounts/{accountId}:
    get:
      operationId: getAccount
      parameters: [{name: accountId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}
  /exports:
    post:
      operationId: createExport
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {format: {type: string}}}}}
      responses:
        "202":
          description: accepted
          headers:
            X-Export-Id: {schema: {type: string}}
  /exports/{exportId}:
    get:
      operationId: getExport
      parameters: [{name: exportId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Export"}}}}
  /pings:
    post:
      operationId: createPing
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {note: {type: string}}}}}
      responses:
        "201":
          description: created
          headers:
            X-Request-Id: {schema: {type: string}}
  /pings/{pingId}:
    get:
      operationId: getPing
      parameters: [{name: pingId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ping"}}}}
  /folders:
    post:
      operationId: createFolder
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201":
          description: created
          headers:
            Location: {schema: {type: string}}
  /folders/{folderId}/contents/{entryId}:
    get:
      operationId: getFolderEntry
      parameters:
        - {name: folderId, in: path, required: true, schema: {type: string}}
        - {name: entryId, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
  /notes:
    post:
      operationId: createNote
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}
      responses:
        "201":
          description: created
          headers:
            Note-Id: {schema: {type: string}}
          content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}
  /notes/{noteId}:
    get:
      operationId: getNote
      parameters: [{name: noteId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
components:
  schemas:
    Job:
      type: object
      properties:
        id: {type: string}
        kind: {type: string}
    Export:
      type: object
      properties:
        id: {type: string}
        format: {type: string}
    Ping:
      type: object
      properties:
        id: {type: string}
        note: {type: string}
    Folder:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
    Note:
      type: object
      properties:
        id: {type: string}
        text: {type: string}
    NewTicket:
      type: object
      properties:
        title: {type: string}
        status: {type: string, enum: [open, closed]}
    Ticket:
      type: object
      properties:
        id: {type: integer}
        title: {type: string}
        status: {type: string, enum: [open, closed]}
    Line:
      type: object
      properties:
        id: {type: integer}
        text: {type: string}
    Board:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
"##;

/// Answers each request with the next of its answers (wrapped in the effects
/// envelope when asked), else fails, so the faker answers.
struct Scripted {
    answers: Mutex<VecDeque<Value>>,
}

#[async_trait]
impl Provider for Scripted {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let enveloped = match &req.response_format {
            ResponseFormat::JsonSchema { schema, .. } => {
                schema.pointer("/properties/effects").is_some()
            }
            _ => false,
        };
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| LlmError::Empty("script exhausted".into()))?;
        let content = match enveloped {
            true => json!({"effects": [], "response": answer}),
            false => answer,
        };
        Ok(ChatResponse {
            content: content.to_string(),
            usage: Usage::default(),
        })
    }
}

fn router(answers: Vec<Value>) -> axum::Router {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let provider = Arc::new(Scripted {
        answers: Mutex::new(answers.into()),
    });
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        store,
    );
    http_rest::router(Arc::new(rt))
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn source(headers: &HeaderMap) -> String {
    header(headers, "x-mockingbird-source")
}

/// The ticket's id is in `Resource-ID` only: a read by it is the ticket as sent, from
/// state, and a second create takes another id.
#[tokio::test]
async fn an_id_header_names_the_stored_record() {
    let r = router(vec![json!({"lineIds": [5]}), json!({"lineIds": [6]})]);
    let (status, headers, body) = send(
        &r,
        "POST",
        "/tickets",
        Some(json!({"title": "Printer jam", "status": "open"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body, json!({"lineIds": [5]}), "the answer stays as given");
    let id = header(&headers, "resource-id");
    assert!(id.parse::<i64>().is_ok(), "an integer id: {id:?}");

    let (status, headers, read) = send(&r, "GET", &format!("/tickets/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["title"], "Printer jam");
    assert_eq!(read["status"], "open");
    assert_eq!(read["id"].to_string(), id);

    let (_, headers, _) = send(&r, "POST", "/tickets", Some(json!({"title": "Toner"}))).await;
    let other = header(&headers, "resource-id");
    assert_ne!(other, id);
    let (_, _, read) = send(&r, "GET", &format!("/tickets/{other}"), None).await;
    assert_eq!(read["title"], "Toner");
}

/// A create under a parent answering no body at all: the line is stored under the
/// header's id, beneath its ticket.
#[tokio::test]
async fn a_bodiless_sub_collection_create_is_stored_under_its_header_id() {
    let r = router(vec![json!({"lineIds": []})]);
    let (_, headers, _) = send(&r, "POST", "/tickets", Some(json!({"title": "Jam"}))).await;
    let ticket = header(&headers, "resource-id");
    let (status, headers, _) = send(
        &r,
        "POST",
        &format!("/tickets/{ticket}/lines"),
        Some(json!({"text": "Called the vendor"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let line = header(&headers, "resource-id");
    assert!(line.parse::<i64>().is_ok(), "{line:?}");
    let (status, headers, read) =
        send(&r, "GET", &format!("/tickets/{ticket}/lines/{line}"), None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["text"], "Called the vendor");
}

/// A `Location` header becomes the item URL of the new board, which reads back.
#[tokio::test]
async fn a_location_header_is_the_new_records_url() {
    let r = router(vec![]);
    let (status, headers, _) = send(&r, "POST", "/boards", Some(json!({"name": "Roadmap"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let location = header(&headers, "location");
    // The URL the call reached the mock at, and the new board's id under it.
    let id = location
        .strip_prefix("http://localhost/boards/")
        .unwrap_or_else(|| panic!("an item URL: {location:?}"));
    assert!(!id.is_empty() && !id.contains('/'), "{location:?}");
    let (status, headers, read) = send(&r, "GET", &format!("/boards/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["name"], "Roadmap");
    assert_eq!(read["id"], id);
}

/// An id header naming another kind of record (the caller's account) is not the new
/// job's id: nothing is stored under it. One naming the created kind is.
#[tokio::test]
async fn an_id_header_naming_another_kind_is_not_the_records() {
    let r = router(vec![]);
    let (status, headers, _) = send(&r, "POST", "/jobs", Some(json!({"kind": "sync"}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let account = header(&headers, "x-account-id");
    assert!(!account.is_empty() && !account.contains('/'), "{account:?}");
    let (_, headers, read) = send(&r, "GET", &format!("/jobs/{account}"), None).await;
    assert_ne!(source(&headers), "state", "{read}");

    let (status, headers, _) = send(&r, "POST", "/exports", Some(json!({"format": "csv"}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let export = header(&headers, "x-export-id");
    let (_, headers, read) = send(&r, "GET", &format!("/exports/{export}"), None).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["format"], "csv", "{read}");
}

/// A create answering only the call's own request id stores nothing under it.
#[tokio::test]
async fn a_request_id_header_stores_nothing() {
    let r = router(vec![]);
    let (status, headers, _) = send(&r, "POST", "/pings", Some(json!({"note": "hi"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let request = header(&headers, "x-request-id");
    assert!(!request.is_empty() && !request.contains('/'), "{request:?}");
    let (_, headers, read) = send(&r, "GET", &format!("/pings/{request}"), None).await;
    assert_ne!(source(&headers), "state", "{read}");
}

/// A `Location` whose record is read deeper than one segment under the create's path
/// names no item of it: the header is left to its declared value, and not made the
/// create's URL.
#[tokio::test]
async fn a_location_off_the_item_route_is_left_alone() {
    let r = router(vec![]);
    let (status, headers, _) = send(&r, "POST", "/folders", Some(json!({"name": "Inbox"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let location = header(&headers, "location");
    assert!(
        !location.starts_with("http://localhost/folders"),
        "{location:?}"
    );
}

/// A create whose answer carries the record's id keeps it, and the id header declared
/// beside it serves that same id; so does one whose answer leaves the id out and is
/// given one.
#[tokio::test]
async fn a_body_id_is_served_in_the_id_header() {
    let r = router(vec![
        json!({"id": "n_1", "text": "first"}),
        json!({"text": "second"}),
    ]);
    let (status, headers, body) = send(&r, "POST", "/notes", Some(json!({"text": "first"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], "n_1", "{body}");
    assert_eq!(header(&headers, "note-id"), "n_1");
    let (_, headers, read) = send(&r, "GET", "/notes/n_1", None).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["text"], "first", "{read}");

    let (status, headers, body) = send(&r, "POST", "/notes", Some(json!({"text": "second"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap_or_default().to_string();
    assert!(!id.is_empty(), "{body}");
    assert_eq!(header(&headers, "note-id"), id, "{body}");
    let (_, headers, read) = send(&r, "GET", &format!("/notes/{id}"), None).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["text"], "second", "{read}");
}
