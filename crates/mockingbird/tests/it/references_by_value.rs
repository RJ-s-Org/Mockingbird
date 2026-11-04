//! Records a request refers to by a member value, through the store a server builds:
//! [`ha::cached_shared_state`] over the in-memory store. On one node (a local bus) this
//! process sees every write, so a unique email names its contact; with a bus other nodes
//! write through, the store is asked whether the email is unique before it names one.
#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use mb_engine::{Engine, EngineConfig};
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{Bus, LocalBus, MemoryBusHub, MemoryStore};
use mockingbird::ha;
use serde_json::json;
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
components:
  schemas:
    Contact:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        email: {type: string}
    Ticket:
      type: object
      required: [id, subject]
      properties:
        id: {type: string}
        subject: {type: string}
"##;

/// The prompt of a ticket created with the email of a contact created before it, over
/// the server's store on `bus`.
async fn ticket_prompt(bus: Arc<dyn Bus>) -> (bool, String) {
    let state = ha::cached_shared_state(
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
        bus,
        Duration::from_secs(3600),
        mb_state::DEFAULT_CACHE_ENTRIES,
    );
    let local = state.local_only();
    let script = [
        json!({"id": "ct-000101", "name": "Jane Roe", "email": "jane.roe@example.com"}),
        json!({"effects": [], "response": {"id": "tk-000001", "subject": "Help"}}),
    ];
    let provider = Arc::new(ScriptedProvider::new(
        script.iter().map(|v| Ok(v.to_string())).collect(),
    ));
    let rt = MockRuntime::new(
        "m",
        Arc::new(mb_import::compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        state,
    );
    let router = http_rest::router(Arc::new(rt));
    for (uri, body) in [
        (
            "/contacts",
            json!({"name": "Jane Roe", "email": "jane.roe@example.com"}),
        ),
        (
            "/tickets",
            json!({"subject": "Help", "email": "jane.roe@example.com"}),
        ),
    ] {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        router.clone().oneshot(req).await.unwrap();
    }
    let requests = provider.requests();
    let prompt = requests[1].messages.last().unwrap().content.clone();
    (local, prompt)
}

fn related(prompt: &str) -> Option<&str> {
    let start = prompt.find("<related_resources>")?;
    let end = prompt.find("</related_resources>")?;
    Some(&prompt[start..end])
}

/// One node, as `mockingbird serve` runs by default: the email names its contact.
#[tokio::test]
async fn a_local_bus_matches_member_values() {
    let (local, prompt) = ticket_prompt(LocalBus::new_shared("n1".into())).await;
    assert!(local, "the server's store over a local bus is local only");
    assert!(
        related(&prompt).is_some_and(|r| r.contains("ct-000101")),
        "{prompt}"
    );
}

/// Several nodes: another may have written the same email, so the store is asked, and
/// as it shows one contact holding it, the email names that contact.
#[tokio::test]
async fn a_shared_bus_matches_member_values_the_store_shows_unique() {
    let hub = MemoryBusHub::new_shared();
    let (local, prompt) = ticket_prompt(hub.join("n1".into())).await;
    assert!(
        !local,
        "a store other nodes write through is not local only"
    );
    assert!(
        related(&prompt).is_some_and(|r| r.contains("ct-000101")),
        "{prompt}"
    );
}
