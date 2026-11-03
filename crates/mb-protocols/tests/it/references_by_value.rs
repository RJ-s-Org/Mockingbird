//! A request that refers to a stored record by a value no member rule knows: an id
//! inside a URI (`spotify:track:<id>`), a short id in a nested object named after its
//! type (`transition: {id: "21"}`), or a unique member value such as an email. The
//! records the mock has stored are indexed by their identities and unique member values,
//! and a request's values are matched against that index. A match must be unique and
//! not trivial, and is shown as related context only. Driven over HTTP with a scripted
//! LLM, asserting the prompts it was sent and what the store holds.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: References, version: "1"}
paths:
  /contacts:
    post:
      operationId: createContact
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}
  /contacts/{contact_id}:
    get:
      operationId: getContact
      parameters: [{name: contact_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}
  /tickets:
    post:
      operationId: createTicket
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}
  /tickets/{ticket_id}:
    get:
      operationId: getTicket
      parameters: [{name: ticket_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}
  /tracks:
    post:
      operationId: createTrack
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Track"}}}}
  /tracks/{track_id}:
    get:
      operationId: getTrack
      parameters: [{name: track_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Track"}}}}
  /transitions:
    post:
      operationId: createTransition
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Transition"}}}}
  /transitions/{transition_id}:
    get:
      operationId: getTransition
      parameters: [{name: transition_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Transition"}}}}
  /playlists/{playlist_id}/items:
    post:
      operationId: addItems
      parameters: [{name: playlist_id, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}
  /moves:
    post:
      operationId: move
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}
components:
  schemas:
    Contact:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        email: {type: string}
        state: {type: string, enum: [on-hold, active-1]}
        joined: {type: string, format: date}
        api_key: {type: string}
    Ticket:
      type: object
      required: [id, subject]
      properties:
        id: {type: string}
        subject: {type: string}
        requester_id: {type: string}
        requester:
          type: object
          properties:
            id: {type: string}
            name: {type: string}
            email: {type: string}
    Track:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
    Transition:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
    Ack:
      type: object
      required: [ok]
      properties:
        ok: {type: boolean}
"##;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<MemoryStore>,
}

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "default".into(),
    }
}

fn harness(script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store) as Arc<dyn StateStore>,
    );
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

impl Harness {
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    /// The user message of the `n`th LLM call.
    fn prompt(&self, n: usize) -> String {
        let requests = self.provider.requests();
        requests[n].messages.last().unwrap().content.clone()
    }

    async fn held(&self, resource_type: &str, id: &str) -> Value {
        self.store
            .get(&session(), resource_type, id)
            .await
            .unwrap()
            .unwrap()
            .data
    }
}

/// The text between a block's tags, or `None` without one.
fn block<'a>(prompt: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = prompt.find(&open)? + open.len();
    let end = prompt.find(&format!("</{tag}>"))?;
    Some(&prompt[start..end])
}

/// Whether `prompt`'s related resources hold `text`.
fn related_holds(prompt: &str, text: &str) -> bool {
    block(prompt, "related_resources").is_some_and(|r| r.contains(text))
}

const TRACK: &str = "4uLU6hMCjMI75M1A2tKUQC";

fn contact(id: &str, email: &str) -> Value {
    json!({"id": id, "name": "Jane Roe", "email": email})
}

/// Spotify's `uris`: the id a stored track was created under, inside a URI, shows
/// that track to the add, so the model does not invent one.
#[tokio::test]
async fn an_id_inside_a_uri_shows_its_record() {
    let h = harness(vec![
        json!({"id": TRACK, "name": "Song of the Mock"}),
        json!({"effects": [], "response": {"ok": true}}),
    ]);
    h.call("POST", "/tracks", Some(json!({"name": "Song of the Mock"})))
        .await;
    let uri = format!("spotify:track:{TRACK}");
    let (status, _) = h
        .call(
            "POST",
            "/playlists/pl-000001/items",
            Some(json!({"uris": [uri], "position": 0})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = h.prompt(1);
    assert!(related_holds(&prompt, "Song of the Mock"), "{prompt}");
    let related = block(&prompt, "related_resources").unwrap();
    assert_eq!(related.matches(TRACK).count(), 1, "shown once: {prompt}");
}

/// Jira's `transition: {id: "21"}`: a short id under an identity member of an object
/// named after a stored type shows that record.
#[tokio::test]
async fn a_short_id_in_an_object_named_after_its_type_shows_its_record() {
    let h = harness(vec![
        json!({"id": "21", "name": "Done"}),
        json!({"effects": [], "response": {"ok": true}}),
    ]);
    h.call("POST", "/transitions", Some(json!({"name": "Done"})))
        .await;
    h.call("POST", "/moves", Some(json!({"transition": {"id": "21"}})))
        .await;
    let prompt = h.prompt(1);
    assert!(related_holds(&prompt, r#""name":"Done""#), "{prompt}");
}

/// Freshdesk: a ticket created with its requester's email is shown that contact, so the
/// model does not invent a `requester_id`.
#[tokio::test]
async fn a_unique_member_value_shows_its_record() {
    let h = harness(vec![
        contact("ct-000101", "jane.roe@example.com"),
        json!({"effects": [], "response": {"id": "tk-000001", "subject": "Help", "requester_id": "ct-000101"}}),
    ]);
    h.call(
        "POST",
        "/contacts",
        Some(json!({"name": "Jane Roe", "email": "jane.roe@example.com"})),
    )
    .await;
    h.call(
        "POST",
        "/tickets",
        Some(json!({"subject": "Help", "email": "jane.roe@example.com"})),
    )
    .await;
    let prompt = h.prompt(1);
    assert!(related_holds(&prompt, "ct-000101"), "{prompt}");
}

/// A value two stored records hold shows neither: which one the request means is a guess.
#[tokio::test]
async fn an_ambiguous_value_shows_nothing() {
    let h = harness(vec![
        contact("ct-000101", "shared@example.com"),
        contact("ct-000102", "shared@example.com"),
        json!({"id": "tk-000001", "subject": "Help"}),
    ]);
    for _ in 0..2 {
        h.call(
            "POST",
            "/contacts",
            Some(json!({"name": "Jane Roe", "email": "shared@example.com"})),
        )
        .await;
    }
    h.call(
        "POST",
        "/tickets",
        Some(json!({"subject": "Help", "email": "shared@example.com"})),
    )
    .await;
    let prompt = h.prompt(2);
    assert!(!prompt.contains("ct-00010"), "{prompt}");
}

/// Short and trivial values match nothing: a short id outside an object named after its
/// type, a short token of a longer string, a word, an enum value the schema declares, a
/// date, and what a credential member holds.
#[tokio::test]
async fn short_and_trivial_values_show_nothing() {
    let h = harness(vec![
        json!({"id": "21", "name": "Done"}),
        json!({
            "id": "ct-000101", "name": "Jane Roe", "email": "jane.roe@example.com",
            "state": "on-hold", "joined": "2026-11-03", "api_key": "sk-test-4711abcdef",
        }),
        json!({"id": "tk-000001", "subject": "Help"}),
    ]);
    h.call("POST", "/transitions", Some(json!({"name": "Done"})))
        .await;
    h.call("POST", "/contacts", Some(json!({"name": "Jane Roe"})))
        .await;
    h.call(
        "POST",
        "/tickets",
        Some(json!({
            "subject": "Help",
            "count": "21",
            "ref": "pr-21",
            "list": {"id": "21"},
            "kind": "general",
            "status": "on-hold",
            "since": "2026-11-03",
            "secret": "sk-test-4711abcdef",
        })),
    )
    .await;
    let prompt = h.prompt(2);
    assert!(block(&prompt, "related_resources").is_none(), "{prompt}");
}

/// A record matched by value is context only: the create's answer, which embeds a changed
/// copy of it, does not rewrite it, and its next read costs no generation and answers it
/// as stored.
#[tokio::test]
async fn a_value_matched_record_is_not_rewritten_by_the_answer() {
    let h = harness(vec![
        contact("ct-000101", "jane.roe@example.com"),
        json!({"effects": [], "response": {
            "id": "tk-000001", "subject": "Help", "requester_id": "ct-000101",
            "requester": {"id": "ct-000101", "name": "Someone Else", "email": "jane.roe@example.com"},
        }}),
    ]);
    h.call(
        "POST",
        "/contacts",
        Some(json!({"name": "Jane Roe", "email": "jane.roe@example.com"})),
    )
    .await;
    let stored = h.held("contacts", "ct-000101").await;
    h.call(
        "POST",
        "/tickets",
        Some(json!({"subject": "Help", "email": "jane.roe@example.com"})),
    )
    .await;
    assert!(related_holds(&h.prompt(1), "ct-000101"), "{}", h.prompt(1));
    assert_eq!(h.held("contacts", "ct-000101").await, stored);
    let (status, body) = h.call("GET", "/contacts/ct-000101", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Jane Roe");
    assert_eq!(h.calls(), 2, "the read is answered from the store");
}
