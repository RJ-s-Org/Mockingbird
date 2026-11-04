//! Writes on a parent's other routes: a write on a sibling sub-route of a parent
//! (`POST /racks/{id}/links/books`) reaches the list reads under that parent whose
//! route or whose member it names, a 2xx write answering no body is still asked for
//! the effects it has on what it was shown, and a child's delete is told to its
//! parent. Driven over HTTP with a scripted LLM, asserting the prompts it was sent,
//! how many calls it took, and that state changed only through a validated effect.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};
use tower::ServiceExt;

/// A synthetic API: racks holding books through a link route, the rack's own list
/// of books, notes made under a rack, and a rack's comments written through the
/// rack itself.
const RACKS: &str = r##"
openapi: 3.0.0
info: {title: Racks, version: "1"}
paths:
  /racks/{rack_id}:
    parameters: [{name: rack_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getRack
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Rack"}}}}
    put:
      operationId: putRack
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/RackUpdate"}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Rack"}}}}
  /racks/{rack_id}/books:
    parameters: [{name: rack_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listRackBooks
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Book"}}}}}
  /racks/{rack_id}/remarks:
    parameters: [{name: rack_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listRackRemarks
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Remark"}}}}}
  /racks/{rack_id}/links/books:
    parameters: [{name: rack_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: linkBooks
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/Links"}}}
      responses:
        "204": {description: linked}
    delete:
      operationId: unlinkBooks
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/Links"}}}
      responses:
        "204": {description: unlinked}
  /racks/{rack_id}/notes:
    parameters: [{name: rack_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createNote
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
  /racks/{rack_id}/notes/{note_id}:
    parameters:
      - {name: rack_id, in: path, required: true, schema: {type: string}}
      - {name: note_id, in: path, required: true, schema: {type: string}}
    get:
      operationId: getNote
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
    delete:
      operationId: deleteNote
      responses:
        "204": {description: gone}
  /racks/{rack_id}/archived/books:
    parameters: [{name: rack_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listArchivedRackBooks
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Book"}}}}}
  /racks/{rack_id}/featured:
    parameters: [{name: rack_id, in: path, required: true, schema: {type: string}}]
    put:
      operationId: setFeatured
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {book: {$ref: "#/components/schemas/Book"}, books: {type: array, items: {$ref: "#/components/schemas/Book"}}}}}}
      responses:
        "204": {description: set}
  /bins/{bin_id}/books:
    parameters: [{name: bin_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listBinBooks
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Book"}}}}}
  /books/{book_id}:
    parameters: [{name: book_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBook
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Book"}}}}
components:
  schemas:
    Rack:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        note_count: {type: integer}
    RackUpdate:
      type: object
      properties:
        name: {type: string}
        remark: {type: object, properties: {text: {type: string}}}
    Book:
      type: object
      required: [id, title]
      properties:
        id: {type: string}
        title: {type: string}
        racked: {type: boolean}
    Remark:
      type: object
      properties:
        id: {type: string}
        text: {type: string}
    Note:
      type: object
      required: [id]
      properties:
        id: {type: string}
        text: {type: string}
    Links:
      type: object
      properties:
        data:
          type: array
          items:
            type: object
            properties:
              type: {type: string}
              id: {type: string}
"##;

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "default".into(),
    }
}

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<MemoryStore>,
}

fn harness(script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(RACKS.as_bytes()).unwrap()),
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
        self.call_with(method, uri, body, &[]).await
    }

    async fn call_with(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
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

    /// The response format's schema of the `n`th LLM call.
    fn format(&self, n: usize) -> Value {
        let requests = self.provider.requests();
        match &requests[n].response_format {
            mb_llm::ResponseFormat::JsonSchema { schema, .. } => schema.clone(),
            other => panic!("not a schema: {other:?}"),
        }
    }

    async fn hold(&self, resource_type: &str, id: &str, data: Value) {
        self.store
            .upsert(
                &session(),
                StoredResource {
                    resource_type: resource_type.into(),
                    id: id.into(),
                    data,
                },
            )
            .await
            .unwrap();
    }

    async fn held(&self, resource_type: &str, id: &str) -> Option<Value> {
        self.store
            .get(&session(), resource_type, id)
            .await
            .unwrap()
            .map(|r| r.data)
    }
}

/// The text between a block's tags, or `None` without one.
fn block<'a>(prompt: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = prompt.find(&open)? + open.len();
    let end = prompt.find(&format!("</{tag}>"))?;
    Some(&prompt[start..end])
}

fn no_effects() -> Value {
    json!({"effects": [], "response": null})
}

// A write on a sibling sub-route.

/// A link written on `/racks/{id}/links/books` is told to `/racks/{id}/books`, the
/// list under the same parent that its route names, and not to another rack's.
#[tokio::test]
async fn a_sibling_routes_write_is_told_to_the_parents_list() {
    let h = harness(vec![no_effects(), json!([]), json!([])]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    let (status, _) = h
        .call(
            "POST",
            "/racks/s-1/links/books",
            Some(json!({"data": [{"type": "book", "id": "bk-4711"}]})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h.call("GET", "/racks/s-1/books", None).await;
    assert_eq!(status, StatusCode::OK);
    let prompt = h.prompt(h.calls() - 1);
    let told = block(&prompt, "earlier_writes").unwrap_or_else(|| panic!("{prompt}"));
    assert!(told.contains("linkBooks"), "{prompt}");
    assert!(told.contains("bk-4711"), "{prompt}");
    assert!(
        prompt.contains("may have changed"),
        "told, not forced: {prompt}"
    );

    h.call("GET", "/racks/s-2/books", None).await;
    let other = h.prompt(h.calls() - 1);
    assert!(block(&other, "earlier_writes").is_none(), "{other}");
}

/// An unlink on the same sibling route is told after the link, in order.
#[tokio::test]
async fn a_sibling_routes_link_and_unlink_are_told_in_order() {
    let h = harness(vec![no_effects(), no_effects(), json!([])]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    let links = json!({"data": [{"type": "book", "id": "bk-4711"}]});
    h.call("POST", "/racks/s-1/links/books", Some(links.clone()))
        .await;
    h.call("DELETE", "/racks/s-1/links/books", Some(links))
        .await;
    h.call("GET", "/racks/s-1/books", None).await;
    let prompt = h.prompt(h.calls() - 1);
    let told = block(&prompt, "earlier_writes").unwrap_or_else(|| panic!("{prompt}"));
    let (link, unlink) = (
        told.find("linkBooks").unwrap(),
        told.find("unlinkBooks").unwrap(),
    );
    assert!(link < unlink, "{told}");
}

/// A write on the parent itself reaches a list under it only with what its body says
/// under a member naming that list's items: the remark, not the name.
#[tokio::test]
async fn a_parents_write_tells_a_child_list_only_the_member_naming_it() {
    let h = harness(vec![
        json!({"effects": [], "response": {"id": "s-1", "name": "Fact"}}),
        json!([]),
        json!([]),
    ]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.call(
        "PUT",
        "/racks/s-1",
        Some(json!({"name": "Fact", "remark": {"text": "well kept"}})),
    )
    .await;
    h.call("GET", "/racks/s-1/remarks", None).await;
    let prompt = h.prompt(h.calls() - 1);
    let told = block(&prompt, "earlier_writes").unwrap_or_else(|| panic!("{prompt}"));
    assert!(told.contains("well kept"), "{prompt}");
    assert!(
        !told.contains("Fact"),
        "only the member naming the list: {prompt}"
    );

    // A list it names nothing of is told nothing.
    h.call("GET", "/racks/s-1/books", None).await;
    let other = h.prompt(h.calls() - 1);
    assert!(block(&other, "earlier_writes").is_none(), "{other}");
}

// A write answering no body.

/// A 204 write shown records besides its target is asked for its effects only, and a
/// valid effect applies.
#[tokio::test]
async fn a_bodyless_write_is_asked_for_its_effects_and_they_apply() {
    let h = harness(vec![
        json!({"effects": [{"type": "books", "id": "bk-4711", "set": {"racked": true}}], "response": null}),
    ]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    let (status, body) = h
        .call(
            "POST",
            "/racks/s-1/links/books",
            Some(json!({"data": [{"type": "book", "id": "bk-4711"}]})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null, "no body is invented");
    assert_eq!(h.calls(), 1);
    let prompt = h.prompt(0);
    let shown = block(&prompt, "shown_resources").unwrap_or_else(|| panic!("{prompt}"));
    assert!(shown.contains("bk-4711"), "{prompt}");
    assert!(prompt.contains("no body"), "{prompt}");
    let format = h.format(0);
    assert_eq!(format["properties"]["response"], json!({"type": "null"}));
    let book = h.held("books", "bk-4711").await.unwrap();
    assert_eq!(book["racked"], json!(true), "{book}");
}

/// An effect the record's schema refuses is not applied, nor one on a record not shown.
#[tokio::test]
async fn a_bodyless_writes_effects_are_guarded() {
    let h = harness(vec![json!({"effects": [
        {"type": "books", "id": "bk-4711", "set": {"racked": "very"}},
        {"type": "books", "id": "bk-9999", "set": {"racked": true}}
    ], "response": null})]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    h.call(
        "POST",
        "/racks/s-1/links/books",
        Some(json!({"data": [{"type": "book", "id": "bk-4711"}]})),
    )
    .await;
    let book = h.held("books", "bk-4711").await.unwrap();
    assert_eq!(book, json!({"id": "bk-4711", "title": "Dune"}));
    assert!(h.held("books", "bk-9999").await.is_none());
}

/// A 204 write shown nothing makes no call, as before.
#[tokio::test]
async fn a_bodyless_write_shown_nothing_makes_no_call() {
    let h = harness(vec![]);
    let (status, _) = h
        .call(
            "POST",
            "/racks/s-1/links/books",
            Some(json!({"data": [{"type": "book", "id": "bk-4711"}]})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(h.calls(), 0);
}

/// A failed effects call still answers the write, with nothing changed.
#[tokio::test]
async fn a_bodyless_write_survives_a_failed_effects_call() {
    let h = harness(vec![json!("not an envelope")]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    let (status, _) = h
        .call(
            "POST",
            "/racks/s-1/links/books",
            Some(json!({"data": [{"type": "book", "id": "bk-4711"}]})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(h.calls(), 1, "no repair round");
    let book = h.held("books", "bk-4711").await.unwrap();
    assert_eq!(book, json!({"id": "bk-4711", "title": "Dune"}));
}

// A child's delete.

/// Deleting a note under a rack tells the rack's next read, naming what went, and
/// makes no call of its own.
#[tokio::test]
async fn a_childs_delete_is_told_to_its_parent() {
    let h = harness(vec![
        json!({"id": "s-1", "name": "Fiction", "note_count": 0}),
    ]);
    h.hold(
        "racks",
        "s-1",
        json!({"id": "s-1", "name": "Fiction", "note_count": 1}),
    )
    .await;
    h.hold("notes", "n-1", json!({"id": "n-1", "text": "hi"}))
        .await;
    let (status, _) = h.call("DELETE", "/racks/s-1/notes/n-1", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(h.calls(), 0, "a delete asks for no effects");
    let (status, rack) = h.call("GET", "/racks/s-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.calls(), 1, "the read catches up");
    let prompt = h.prompt(0);
    let told = block(&prompt, "earlier_requests").unwrap_or_else(|| panic!("{prompt}"));
    assert!(told.contains("deleteNote"), "{prompt}");
    assert!(told.contains("n-1"), "names what went: {prompt}");
    assert_eq!(rack["note_count"], json!(0));
}

/// A delete of a child under a parent the session does not hold tells nothing.
#[tokio::test]
async fn a_childs_delete_under_no_held_parent_tells_nothing() {
    let h = harness(vec![]);
    h.hold("notes", "n-1", json!({"id": "n-1", "text": "hi"}))
        .await;
    h.call("DELETE", "/racks/s-1/notes/n-1", None).await;
    let kept = h
        .store
        .list(&session(), "__mockingbird.earlier_requests", 10)
        .await
        .unwrap();
    assert!(
        kept.iter()
            .all(|d| d.id.starts_with("#deleted") || d.id == "#last-write"),
        "only the delete's own markers: {kept:?}"
    );
    assert_eq!(h.calls(), 0);
}

/// A delete of a child the session never held tells the held parent nothing: there is
/// nothing to name, and the parent's stored copy stands.
#[tokio::test]
async fn an_unheld_childs_delete_leaves_the_parent_as_stored() {
    let h = harness(vec![
        json!({"id": "s-1", "name": "Fiction", "note_count": 0}),
    ]);
    let rack = json!({"id": "s-1", "name": "Fiction", "note_count": 3});
    h.hold("racks", "s-1", rack.clone()).await;
    h.call("DELETE", "/racks/s-1/notes/n-unknown", None).await;
    let (_, read) = h.call("GET", "/racks/s-1", None).await;
    assert_eq!(h.calls(), 0, "the read answers from state");
    assert_eq!(read, rack);
    assert_eq!(h.held("racks", "s-1").await.unwrap(), rack);
}

// One level, by route.

/// A list two literal segments under the parent is another list, even when it ends in
/// the segment the write's route ends in.
#[tokio::test]
async fn a_list_two_segments_under_the_parent_is_not_told() {
    let h = harness(vec![no_effects(), json!([])]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    h.call(
        "POST",
        "/racks/s-1/links/books",
        Some(json!({"data": [{"type": "book", "id": "bk-4711"}]})),
    )
    .await;
    h.call("GET", "/racks/s-1/archived/books", None).await;
    let prompt = h.prompt(h.calls() - 1);
    assert!(block(&prompt, "earlier_writes").is_none(), "{prompt}");
}

/// The same segment under another parent's literal is another parent's list.
#[tokio::test]
async fn the_same_segment_under_another_parent_is_not_told() {
    let h = harness(vec![no_effects(), json!([]), json!([])]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    h.call(
        "POST",
        "/racks/s-1/links/books",
        Some(json!({"data": [{"type": "book", "id": "bk-4711"}]})),
    )
    .await;
    h.call("GET", "/bins/s-1/books", None).await;
    let other = h.prompt(h.calls() - 1);
    assert!(block(&other, "earlier_writes").is_none(), "{other}");
    h.call("GET", "/racks/s-1/books", None).await;
    let own = h.prompt(h.calls() - 1);
    assert!(block(&own, "earlier_writes").is_some(), "{own}");
}

// By member.

/// A member on a sibling route is no membership of the parent's list, even one named
/// for it: only a write on the parent itself is told by member.
#[tokio::test]
async fn a_member_on_a_sibling_route_is_not_told() {
    let h = harness(vec![no_effects(), json!([])]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.call(
        "PUT",
        "/racks/s-1/featured",
        Some(json!({
            "book": {"id": "bk-9", "title": "Emma"},
            "books": [{"id": "bk-8", "title": "Ulysses"}],
        })),
    )
    .await;
    h.call("GET", "/racks/s-1/books", None).await;
    let prompt = h.prompt(h.calls() - 1);
    assert!(block(&prompt, "earlier_writes").is_none(), "{prompt}");
}

// Repeats.

/// A client's retry of a bodyless `POST`, the same operation, path, body and
/// `Idempotency-Key`, asks for its effects once, and is remembered once.
#[tokio::test]
async fn a_retried_bodyless_write_asks_for_effects_once() {
    let h = harness(vec![no_effects(), json!([])]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    let links = json!({"data": [{"type": "book", "id": "bk-4711"}]});
    for _ in 0..3 {
        let (status, _) = h
            .call_with(
                "POST",
                "/racks/s-1/links/books",
                Some(links.clone()),
                &[("Idempotency-Key", "retry-1")],
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    assert_eq!(h.calls(), 1, "one effects call");
    h.call("GET", "/racks/s-1/books", None).await;
    let prompt = h.prompt(h.calls() - 1);
    let told = block(&prompt, "earlier_writes").unwrap_or_else(|| panic!("{prompt}"));
    assert_eq!(told.matches("linkBooks").count(), 1, "{told}");
}

/// The same body after another write to the path is no repeat.
#[tokio::test]
async fn a_bodyless_write_after_another_is_asked_again() {
    let h = harness(vec![no_effects(), no_effects(), no_effects()]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    let links = json!({"data": [{"type": "book", "id": "bk-4711"}]});
    h.call("POST", "/racks/s-1/links/books", Some(links.clone()))
        .await;
    h.call("DELETE", "/racks/s-1/links/books", Some(links.clone()))
        .await;
    h.call("POST", "/racks/s-1/links/books", Some(links)).await;
    assert_eq!(h.calls(), 2, "link, then link again after the unlink");
}

/// Two `POST`s with no `Idempotency-Key` are two writes, whatever they say: each is asked
/// for its effects, and both are told.
#[tokio::test]
async fn a_deliberate_repeat_is_two_writes() {
    let h = harness(vec![no_effects(), no_effects(), json!([])]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    let links = json!({"data": [{"type": "book", "id": "bk-4711"}]});
    h.call("POST", "/racks/s-1/links/books", Some(links.clone()))
        .await;
    h.call("POST", "/racks/s-1/links/books", Some(links)).await;
    assert_eq!(h.calls(), 2, "each is asked");
    h.call("GET", "/racks/s-1/books", None).await;
    let prompt = h.prompt(h.calls() - 1);
    let told = block(&prompt, "earlier_writes").unwrap_or_else(|| panic!("{prompt}"));
    assert_eq!(told.matches("linkBooks").count(), 2, "{told}");
}

/// Another `Idempotency-Key` is another write.
#[tokio::test]
async fn another_idempotency_key_is_another_write() {
    let h = harness(vec![no_effects(), no_effects()]);
    h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
        .await;
    h.hold(
        "books",
        "bk-4711",
        json!({"id": "bk-4711", "title": "Dune"}),
    )
    .await;
    let links = json!({"data": [{"type": "book", "id": "bk-4711"}]});
    for key in ["k-1", "k-2"] {
        h.call_with(
            "POST",
            "/racks/s-1/links/books",
            Some(links.clone()),
            &[("Idempotency-Key", key)],
        )
        .await;
    }
    assert_eq!(h.calls(), 2);
}

/// What a retry is told apart by goes with the session: after a reset, the same write is
/// asked again, and its effect applies.
#[tokio::test]
async fn the_latest_write_goes_with_a_session_reset() {
    let effect = json!({"effects": [{"type": "books", "id": "bk-4711", "set": {"racked": true}}], "response": null});
    let h = harness(vec![effect.clone(), effect]);
    let links = json!({"data": [{"type": "book", "id": "bk-4711"}]});
    for round in 1..=2 {
        h.hold("racks", "s-1", json!({"id": "s-1", "name": "Fiction"}))
            .await;
        h.hold(
            "books",
            "bk-4711",
            json!({"id": "bk-4711", "title": "Dune"}),
        )
        .await;
        h.call_with(
            "POST",
            "/racks/s-1/links/books",
            Some(links.clone()),
            &[("Idempotency-Key", "retry-1")],
        )
        .await;
        assert_eq!(h.calls(), round, "after a reset the write is asked again");
        let book = h.held("books", "bk-4711").await.unwrap();
        assert_eq!(book["racked"], json!(true), "{book}");
        h.store.reset(&session()).await.unwrap();
    }
}
