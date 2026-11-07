//! Which members an answer's effects set, and on which record: a request's member an
//! acknowledgement does not answer, a member written one object too high, and an id
//! with punctuation around it.
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

use crate::support::Records;

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "effects".into(),
        session: "default".into(),
    }
}

async fn store(state: &MemoryStore, t: &str, id: &str, data: Value) {
    let resource = StoredResource {
        resource_type: t.into(),
        id: id.into(),
        data,
    };
    state.upsert(&session(), resource).await.unwrap();
}

async fn stored(state: &MemoryStore, t: &str, id: &str) -> Value {
    state.get(&session(), t, id).await.unwrap().unwrap().data
}

/// Sends `method path body` to a mock of `contract` whose one LLM answer is `answer`,
/// over `state`; returns the status and the request record's effect outcomes.
async fn call(
    contract: &str,
    state: &Arc<MemoryStore>,
    answer: Value,
    (method, path, body): (&str, &str, Option<Value>),
) -> (StatusCode, Value) {
    let records = Arc::new(Records::default());
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer.to_string())]));
    let mut rt = MockRuntime::new(
        "effects",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(state) as Arc<dyn StateStore>,
    );
    rt.recorder = Some(Arc::clone(&records) as Arc<dyn mb_protocols::RequestRecorder>);
    let mut req = Request::builder().method(method).uri(path);
    // These tests are about the model's effects: each create is seeded
    // ([`crate::support::SEED`]), which takes it past the state-first executor.
    if method == "POST" {
        req = req.header(crate::support::SEED.0, crate::support::SEED.1);
    }
    // And each update also sends a member no record declares, which the state-first
    // executor leaves to the model.
    let body = match (method, body) {
        ("PUT" | "PATCH", Some(Value::Object(mut members))) => {
            members.insert("comment".into(), json!("from the client"));
            Some(Value::Object(members))
        }
        (_, body) => body,
    };
    let body = match body {
        Some(body) => {
            req = req.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let resp = http_rest::router(Arc::new(rt))
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let record = records.wait(1).await.remove(0);
    (status, record.request["effects"].clone())
}

/// Notes whose edit answers an acknowledgement (`Ack`), or the note itself (`/full`).
const NOTES: &str = r##"
openapi: 3.0.0
info: {title: Notes, version: "1"}
paths:
  /notes/{id}:
    get:
      operationId: getNote
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
    put:
      operationId: editNote
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {title: {type: string}, body: {type: string}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}
components:
  schemas:
    Note:
      type: object
      required: [id, title, body]
      properties:
        id: {type: string}
        title: {type: string}
        body: {type: string}
        active: {type: boolean}
    Ack:
      type: object
      required: [id, title]
      properties:
        id: {type: string}
        title: {type: string}
        active: {type: boolean}
"##;

/// The same notes, with an edit that answers the note.
const FULL_NOTES: &str = r##"
openapi: 3.0.0
info: {title: Notes, version: "1"}
paths:
  /notes/{id}:
    get:
      operationId: getNote
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
    put:
      operationId: editNote
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {title: {type: string}, body: {type: string}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
components:
  schemas:
    Note:
      type: object
      required: [id, title, body]
      properties:
        id: {type: string}
        title: {type: string}
        body: {type: string}
        active: {type: boolean}
"##;

fn note() -> Value {
    json!({"id": "n1", "title": "Old", "body": "B1", "active": true})
}

/// An acknowledgement answers the edit's `title` but not its `body`: the effect setting
/// both still sets `body`, which only the effect can tell.
#[tokio::test]
async fn an_acknowledged_edit_keeps_the_request_members_it_does_not_answer() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "notes", "n1", note()).await;
    let answer = json!({"response": {"id": "n1", "title": "New", "active": true}, "effects": [
        {"type": "notes", "id": "n1", "set": {"title": "New", "body": "B2"}},
    ]});
    let edit = json!({"title": "New", "body": "B2"});
    let (status, effects) = call(NOTES, &state, answer, ("PUT", "/notes/n1", Some(edit))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    let note = stored(&state, "notes", "n1").await;
    assert_eq!(note["body"], "B2", "{note}");
    assert_eq!(note["title"], "New", "{note}");
}

/// What the acknowledgement answers it still settles: an effect giving `title` another
/// value than the answer's sets only the `body` the answer leaves out.
#[tokio::test]
async fn an_acknowledged_member_is_still_the_answers() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "notes", "n1", note()).await;
    let answer = json!({"response": {"id": "n1", "title": "New", "active": true}, "effects": [
        {"type": "notes", "id": "n1", "set": {"title": "Other", "body": "B2"}},
    ]});
    let edit = json!({"title": "New", "body": "B2"});
    let (_, effects) = call(NOTES, &state, answer, ("PUT", "/notes/n1", Some(edit))).await;
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    assert_eq!(effects[0]["set"], json!({"body": "B2"}), "{effects}");
    let note = stored(&state, "notes", "n1").await;
    assert_eq!(note["title"], "New", "{note}");
}

/// A model that leaves `body` out of its effects on purpose (the API ignores it) is
/// taken at its word: nothing adds the request's `body` on its behalf.
#[tokio::test]
async fn an_effect_that_leaves_a_request_member_out_is_not_overruled() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "notes", "n1", note()).await;
    let answer = json!({"response": {"id": "n1", "title": "New", "active": true}, "effects": [
        {"type": "notes", "id": "n1", "set": {"active": false}},
    ]});
    let edit = json!({"title": "New", "body": "B2"});
    let (_, effects) = call(NOTES, &state, answer, ("PUT", "/notes/n1", Some(edit))).await;
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    let note = stored(&state, "notes", "n1").await;
    assert_eq!(note["body"], "B1", "{note}");
    assert_eq!(note["active"], false, "{note}");
}

/// A note held in an earlier, incomplete shape (no `body`, which its read requires)
/// still takes an effect that leaves it no worse; one that fit is still refused one
/// that breaks it.
#[tokio::test]
async fn an_effect_on_a_record_that_already_breaks_its_schema_is_applied() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(
        &state,
        "notes",
        "n1",
        json!({"id": "n1", "title": "Old", "active": true}),
    )
    .await;
    let answer = json!({"response": {"id": "n1", "title": "New", "active": true}, "effects": [
        {"type": "notes", "id": "n1", "set": {"active": false}},
    ]});
    let edit = json!({"title": "New"});
    let (_, effects) = call(NOTES, &state, answer, ("PUT", "/notes/n1", Some(edit))).await;
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    let note = stored(&state, "notes", "n1").await;
    assert_eq!(note["active"], false, "{note}");
}

/// An effect that breaks a note that fit (a member its schema does not take) is still
/// refused.
#[tokio::test]
async fn an_effect_that_breaks_a_fitting_record_is_refused() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "notes", "n1", note()).await;
    let answer = json!({"response": {"id": "n1", "title": "New", "active": true}, "effects": [
        {"type": "notes", "id": "n1", "set": {"active": "yes"}},
    ]});
    let edit = json!({"title": "New"});
    let (_, effects) = call(NOTES, &state, answer, ("PUT", "/notes/n1", Some(edit))).await;
    assert_eq!(effects[0]["outcome"], "rejected", "{effects}");
    let note = stored(&state, "notes", "n1").await;
    assert_eq!(note["active"], true, "{note}");
}

/// An answer with the note's own schema settles every member the request names: an
/// effect giving one another value is still refused.
#[tokio::test]
async fn a_full_answer_still_settles_what_the_request_names() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "notes", "n1", note()).await;
    let answer = json!({
        "response": {"id": "n1", "title": "New", "body": "B2", "active": true},
        "effects": [{"type": "notes", "id": "n1", "set": {"body": "B3"}}],
    });
    let edit = json!({"title": "New", "body": "B2"});
    let (_, effects) = call(FULL_NOTES, &state, answer, ("PUT", "/notes/n1", Some(edit))).await;
    assert_eq!(effects[0]["outcome"], "rejected", "{effects}");
    let note = stored(&state, "notes", "n1").await;
    assert_eq!(note["body"], "B2", "{note}");
}

/// Boards whose record keeps its members under `attributes`, beside `meta`; pins are made
/// with a board's id.
const BOARDS: &str = r##"
openapi: 3.0.0
info: {title: Boards, version: "1"}
paths:
  /boards/{id}:
    get:
      operationId: getBoard
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}
    patch:
      operationId: updateBoard
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                data:
                  type: object
                  properties:
                    attributes: {type: object, properties: {title: {type: string}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}
  /pins:
    post:
      operationId: createPin
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {board_id: {type: string}, note: {type: string}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Pin"}}}}
  /pins/{id}:
    get:
      operationId: getPin
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Pin"}}}}
components:
  schemas:
    Board:
      type: object
      required: [id, type, attributes]
      properties:
        id: {type: string}
        type: {type: string}
        attributes: {$ref: "#/components/schemas/BoardAttributes"}
        meta:
          type: object
          properties:
            note: {type: string}
    BoardAttributes:
      type: object
      properties:
        title: {type: string}
        frozen: {type: boolean}
        frozen-reason: {type: string}
        note: {type: string}
    Pin:
      type: object
      required: [id]
      properties:
        id: {type: string}
        board_id: {type: string}
        note: {type: string}
"##;

fn board() -> Value {
    json!({"id": "b1", "type": "boards", "attributes": {"title": "Plans", "frozen": false}, "meta": {"note": "m"}})
}

/// An effect writes the board's `frozen` at the top level, where the record keeps it
/// under `attributes`, the one object member declaring it: it goes there, beside what
/// `attributes` already holds.
#[tokio::test]
async fn a_member_written_one_object_too_high_goes_where_it_is_declared() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "boards", "b1", board()).await;
    let answer = json!({"response": board(), "effects": [
        {"type": "boards", "id": "b1", "set": {"frozen": true, "frozen-reason": "audit"}},
    ]});
    let edit = json!({"data": {"attributes": {"title": "Plans"}}});
    let (_, effects) = call(BOARDS, &state, answer, ("PATCH", "/boards/b1", Some(edit))).await;
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    let board = stored(&state, "boards", "b1").await;
    assert_eq!(
        board["attributes"],
        json!({"title": "Plans", "frozen": true, "frozen-reason": "audit"}),
        "{board}"
    );
    assert!(board.get("frozen").is_none(), "{board}");
}

/// The same from another record's call: a pin made on the board freezes it.
#[tokio::test]
async fn a_member_one_object_too_high_on_another_record_goes_where_it_is_declared() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "boards", "b1", board()).await;
    let answer = json!({"response": {"id": "p1", "board_id": "b1", "note": "hi"}, "effects": [
        {"type": "boards", "id": "b1", "set": {"frozen": true}},
    ]});
    let pin = json!({"board_id": "b1", "note": "hi"});
    let (_, effects) = call(BOARDS, &state, answer, ("POST", "/pins", Some(pin))).await;
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    let board = stored(&state, "boards", "b1").await;
    assert_eq!(board["attributes"]["frozen"], true, "{board}");
    assert_eq!(board["attributes"]["title"], "Plans", "{board}");
    assert!(board.get("frozen").is_none(), "{board}");
}

/// `note` is declared under both `attributes` and `meta`: which one the effect meant is
/// not known, so it goes under neither.
#[tokio::test]
async fn a_member_two_objects_declare_is_not_moved() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "boards", "b1", board()).await;
    let answer = json!({"response": {"id": "p1", "board_id": "b1", "note": "hi"}, "effects": [
        {"type": "boards", "id": "b1", "set": {"note": "x"}},
    ]});
    let pin = json!({"board_id": "b1", "note": "hi"});
    let _ = call(BOARDS, &state, answer, ("POST", "/pins", Some(pin))).await;
    let board = stored(&state, "boards", "b1").await;
    assert_eq!(board["attributes"].get("note"), None, "{board}");
    assert_eq!(board["meta"], json!({"note": "m"}), "{board}");
}

/// An effect's id written with punctuation around it (`:b1`) is the shown board's.
#[tokio::test]
async fn an_effect_id_with_punctuation_around_it_binds_to_the_shown_record() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "boards", "b1", board()).await;
    let answer = json!({"response": {"id": "p1", "board_id": "b1", "note": "hi"}, "effects": [
        {"type": "boards", "id": ":b1", "set": {"attributes": {"frozen": true}}},
    ]});
    let pin = json!({"board_id": "b1", "note": "hi"});
    let (_, effects) = call(BOARDS, &state, answer, ("POST", "/pins", Some(pin))).await;
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    assert_eq!(effects[0]["id"], "b1", "{effects}");
    let board = stored(&state, "boards", "b1").await;
    assert_eq!(board["attributes"]["frozen"], true, "{board}");
}

/// Trimmed, an id that names no shown record is still no record's.
#[tokio::test]
async fn a_trimmed_effect_id_naming_no_shown_record_stays_rejected() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "boards", "b1", board()).await;
    let answer = json!({"response": {"id": "p1", "board_id": "b1", "note": "hi"}, "effects": [
        {"type": "boards", "id": ":b9", "set": {"attributes": {"frozen": true}}},
    ]});
    let pin = json!({"board_id": "b1", "note": "hi"});
    let (_, effects) = call(BOARDS, &state, answer, ("POST", "/pins", Some(pin))).await;
    assert_eq!(effects[0]["outcome"], "rejected", "{effects}");
    let board = stored(&state, "boards", "b1").await;
    assert_eq!(board["attributes"]["frozen"], false, "{board}");
}

/// An effect giving a request member the answer leaves out another value than the
/// request sent contradicts the request: the member does not take it.
#[tokio::test]
async fn an_effect_contradicting_a_request_member_the_answer_leaves_out_is_not_stored() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "notes", "n1", note()).await;
    let answer = json!({"response": {"id": "n1", "title": "New", "active": true}, "effects": [
        {"type": "notes", "id": "n1", "set": {"body": "Wrong"}},
    ]});
    let edit = json!({"title": "New", "body": "B2"});
    let (_, effects) = call(NOTES, &state, answer, ("PUT", "/notes/n1", Some(edit))).await;
    assert_ne!(effects[0]["outcome"], "applied", "{effects}");
    let note = stored(&state, "notes", "n1").await;
    assert_ne!(note["body"], "Wrong", "{note}");
}

/// Orders holding their customer inline, with its own `id`; refunds are made apart.
const ORDERS: &str = r##"
openapi: 3.0.0
info: {title: Orders, version: "1"}
paths:
  /orders/{id}:
    get:
      operationId: getOrder
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}
  /refunds:
    post:
      operationId: createRefund
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {order_id: {type: string}, name: {type: string}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /refunds/{id}:
    get:
      operationId: getRefund
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
components:
  schemas:
    Order:
      type: object
      required: [id, status]
      properties:
        id: {type: string}
        status: {type: string}
        customer:
          type: object
          properties:
            id: {type: string}
            name: {type: string}
            email: {type: string}
    Refund:
      type: object
      required: [id]
      properties:
        id: {type: string}
        order_id: {type: string}
        name: {type: string}
"##;

/// A related record held inline (the order's `customer`, with an `id` of its own) is no
/// home for a member written too high: a `name` meant for something else does not
/// overwrite the customer's.
#[tokio::test]
async fn a_related_record_held_inline_is_no_home_for_a_member_written_too_high() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let order = json!({"id": "o1", "status": "paid",
        "customer": {"id": "c1", "name": "Ann", "email": "ann@example.com"}});
    store(&state, "orders", "o1", order).await;
    let answer = json!({"response": {"id": "r1", "order_id": "o1", "name": "Refund 1"}, "effects": [
        {"type": "orders", "id": "o1", "set": {"status": "refunded", "name": "Refund 1"}},
    ]});
    let refund = json!({"order_id": "o1", "name": "Refund 1"});
    let (_, effects) = call(ORDERS, &state, answer, ("POST", "/refunds", Some(refund))).await;
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    let order = stored(&state, "orders", "o1").await;
    assert_eq!(order["status"], "refunded", "{order}");
    assert_eq!(order["customer"]["name"], "Ann", "{order}");
}

/// Racks holding their keeper, another type's record keyed by `email`, by its `$ref`.
const RACKS: &str = r##"
openapi: 3.0.0
info: {title: Racks, version: "1"}
paths:
  /racks/{id}:
    get:
      operationId: getRack
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Rack"}}}}
  /keepers/{email}:
    get:
      operationId: getKeeper
      parameters:
        - {name: email, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Keeper"}}}}
  /tags:
    post:
      operationId: createTag
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {rack_id: {type: string}, note: {type: string}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}
  /tags/{id}:
    get:
      operationId: getTag
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}
components:
  schemas:
    Rack:
      type: object
      required: [id, state]
      properties:
        id: {type: string}
        state: {type: string}
        keeper: {$ref: "#/components/schemas/Keeper"}
    Keeper:
      type: object
      required: [email]
      properties:
        email: {type: string}
        label: {type: string}
    Tag:
      type: object
      required: [id]
      properties:
        id: {type: string}
        rack_id: {type: string}
        note: {type: string}
"##;

/// Another type's record held by its `$ref` (the rack's `keeper`) is no home either, with
/// no `id` member to show it.
#[tokio::test]
async fn another_types_record_held_by_ref_is_no_home_for_a_member_written_too_high() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rack = json!({"id": "s1", "state": "open",
        "keeper": {"email": "ann@example.com", "label": "Ann"}});
    store(&state, "racks", "s1", rack).await;
    let answer = json!({"response": {"id": "t1", "rack_id": "s1", "note": "Tag 1"}, "effects": [
        {"type": "racks", "id": "s1", "set": {"state": "tagged", "label": "Tag 1"}},
    ]});
    let tag = json!({"rack_id": "s1", "note": "Tag 1"});
    let (_, effects) = call(RACKS, &state, answer, ("POST", "/tags", Some(tag))).await;
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    let rack = stored(&state, "racks", "s1").await;
    assert_eq!(rack["state"], "tagged", "{rack}");
    assert_eq!(rack["keeper"]["label"], "Ann", "{rack}");
}

const NESTED_KEY: &str = r##"
openapi: 3.0.0
info: {title: Keyed, version: "1"}
paths:
  /gadgets/{name}:
    get:
      operationId: readGadget
      parameters:
        - {name: name, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}
  /widgets/{name}:
    get:
      operationId: readWidget
      parameters:
        - {name: name, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
  /pings:
    post:
      operationId: createPing
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {gadget: {type: string}}}
      responses:
        "201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Ping"}}}}
  /pings/{id}:
    get:
      operationId: getPing
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ping"}}}}
components:
  schemas:
    Info:
      type: object
      properties:
        name: {type: string}
        revision: {type: string}
    Gadget:
      type: object
      properties:
        kind: {type: string}
        phase: {type: string}
        info: {$ref: "#/components/schemas/Info"}
    Widget:
      type: object
      properties:
        kind: {type: string}
        info: {$ref: "#/components/schemas/Info"}
    Ping:
      type: object
      required: [id]
      properties:
        id: {type: string}
        gadget: {type: string}
"##;

/// A record keyed by a nested member (`info.name`) whose effect repeats the key at the
/// top level: the key stays where it was written, not moved onto the identity, and the
/// rest of the effect applies.
#[tokio::test]
async fn an_effect_repeating_a_nested_key_at_the_top_still_applies() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(
        &state,
        "gadgets",
        "g1",
        json!({"kind": "Gadget", "phase": "idle", "info": {"name": "g1", "revision": "1"}}),
    )
    .await;
    let answer = json!({"response": {"id": "p1", "gadget": "g1"}, "effects": [
        {"type": "gadgets", "id": "g1", "set": {"name": "g1", "phase": "pinged"}},
    ]});
    let (_, effects) = call(
        NESTED_KEY,
        &state,
        answer,
        ("POST", "/pings", Some(json!({"gadget": "g1"}))),
    )
    .await;
    let g = stored(&state, "gadgets", "g1").await;
    assert_eq!(g["phase"], "pinged", "{effects} {g}");
}

/// Decks keeping their members under `attributes`, beside `links`, an object whose
/// `owner` is itself an object.
const DECKS: &str = r##"
openapi: 3.0.0
info: {title: Decks, version: "1"}
paths:
  /decks/{id}:
    get:
      operationId: getDeck
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Deck"}}}}
  /cards:
    post:
      operationId: createCard
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {deck_id: {type: string}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Card"}}}}
  /cards/{id}:
    get:
      operationId: getCard
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Card"}}}}
components:
  schemas:
    Deck:
      type: object
      required: [id, attributes]
      properties:
        id: {type: string}
        attributes:
          type: object
          properties:
            label: {type: string}
            full: {type: boolean}
        links:
          type: object
          properties:
            owner:
              type: object
              properties:
                href: {type: string}
    Card:
      type: object
      required: [id]
      properties:
        id: {type: string}
        deck_id: {type: string}
"##;

/// `owner` written at the top level as a string does not fit `links.owner`, an object:
/// it is not moved there, so the rest of the effect still applies.
#[tokio::test]
async fn a_member_whose_value_does_not_fit_its_nested_place_is_not_moved() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let deck = json!({"id": "s1", "attributes": {"label": "A", "full": false},
        "links": {"owner": {"href": "/u/1"}}});
    store(&state, "decks", "s1", deck).await;
    let answer = json!({"response": {"id": "k1", "deck_id": "s1"}, "effects": [
        {"type": "decks", "id": "s1", "set": {"attributes": {"full": true}, "owner": "u9"}},
    ]});
    let card = json!({"deck_id": "s1"});
    let (_, effects) = call(DECKS, &state, answer, ("POST", "/cards", Some(card))).await;
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    let deck = stored(&state, "decks", "s1").await;
    assert_eq!(deck["attributes"]["full"], true, "{deck}");
    assert_eq!(deck["links"]["owner"], json!({"href": "/u/1"}), "{deck}");
}

/// A member whose value fits its nested place is still moved there.
#[tokio::test]
async fn a_member_whose_value_fits_its_nested_place_is_moved() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let deck = json!({"id": "s1", "attributes": {"label": "A", "full": false},
        "links": {"owner": {"href": "/u/1"}}});
    store(&state, "decks", "s1", deck).await;
    let answer = json!({"response": {"id": "k1", "deck_id": "s1"}, "effects": [
        {"type": "decks", "id": "s1", "set": {"full": true, "owner": {"href": "/u/9"}}},
    ]});
    let card = json!({"deck_id": "s1"});
    let (_, effects) = call(DECKS, &state, answer, ("POST", "/cards", Some(card))).await;
    assert_eq!(effects[0]["outcome"], "applied", "{effects}");
    let deck = stored(&state, "decks", "s1").await;
    assert_eq!(deck["attributes"]["full"], true, "{deck}");
    assert_eq!(deck["links"]["owner"], json!({"href": "/u/9"}), "{deck}");
}

/// An effect repeating the board's old `title` at the top level, beside what followed
/// (`frozen`), does not undo the request's change, which the answer settles.
#[tokio::test]
async fn a_stale_member_written_too_high_does_not_undo_the_request() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "boards", "b1", board()).await;
    let mut answered = board();
    answered["attributes"]["title"] = json!("New");
    let answer = json!({"response": answered, "effects": [
        {"type": "boards", "id": "b1", "set": {"frozen": true, "title": "Plans"}},
    ]});
    let edit = json!({"attributes": {"title": "New"}});
    let contract = unwrapped_boards();
    let (_, effects) = call(
        &contract,
        &state,
        answer,
        ("PATCH", "/boards/b1", Some(edit)),
    )
    .await;
    let board = stored(&state, "boards", "b1").await;
    assert_eq!(board["attributes"]["title"], "New", "{effects} {board}");
    assert_eq!(board["attributes"]["frozen"], true, "{effects} {board}");
}

/// The same with the effect written where the record keeps it.
#[tokio::test]
async fn a_stale_nested_member_does_not_undo_the_request() {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "boards", "b1", board()).await;
    let mut answered = board();
    answered["attributes"]["title"] = json!("New");
    let answer = json!({"response": answered, "effects": [
        {"type": "boards", "id": "b1", "set": {"attributes": {"frozen": true, "title": "Plans"}}},
    ]});
    let edit = json!({"attributes": {"title": "New"}});
    let contract = unwrapped_boards();
    let (_, effects) = call(
        &contract,
        &state,
        answer,
        ("PATCH", "/boards/b1", Some(edit)),
    )
    .await;
    let board = stored(&state, "boards", "b1").await;
    assert_eq!(board["attributes"]["title"], "New", "{effects} {board}");
    assert_eq!(board["attributes"]["frozen"], true, "{effects} {board}");
}

/// A request wrapping the record (`{data: {attributes: …}}`) names its members as
/// well: an effect giving the request's `title` another value than the answer settled
/// does not set it, written at the top level or where the record keeps it.
#[tokio::test]
async fn a_wrapped_request_member_the_answer_settles_is_not_the_effects() {
    for set in [
        json!({"frozen": true, "title": "Other"}),
        json!({"attributes": {"frozen": true, "title": "Other"}}),
    ] {
        let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
        store(&state, "boards", "b1", board()).await;
        let mut answered = board();
        answered["attributes"]["title"] = json!("New");
        let answer = json!({"response": answered, "effects": [
            {"type": "boards", "id": "b1", "set": set},
        ]});
        let edit = json!({"data": {"attributes": {"title": "New"}}});
        let (_, effects) = call(BOARDS, &state, answer, ("PATCH", "/boards/b1", Some(edit))).await;
        let board = stored(&state, "boards", "b1").await;
        assert_ne!(board["attributes"]["title"], "Other", "{effects} {board}");
        assert_eq!(board["attributes"]["frozen"], true, "{effects} {board}");
    }
}

/// With an acknowledgement answering only the board's `id`, a nested request member
/// takes an effect's value only where it is the one the request sent.
#[tokio::test]
async fn an_acknowledged_nested_request_member_takes_only_the_requests_value() {
    let acked = BOARDS.replace(
        r##""200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}
  /pins:"##,
        r##""200": {description: ok, content: {application/json: {schema: {type: object, required: [id], properties: {id: {type: string}}}}}}
  /pins:"##,
    );
    assert_ne!(acked, BOARDS);
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "boards", "b1", board()).await;
    let answer = json!({"response": {"id": "b1"}, "effects": [
        {"type": "boards", "id": "b1", "set": {"frozen": true, "title": "Other"}},
    ]});
    let edit = json!({"data": {"attributes": {"title": "New"}}});
    let (_, effects) = call(&acked, &state, answer, ("PATCH", "/boards/b1", Some(edit))).await;
    let board = stored(&state, "boards", "b1").await;
    assert_ne!(board["attributes"]["title"], "Other", "{effects} {board}");
    assert_eq!(board["attributes"]["frozen"], true, "{effects} {board}");
}

/// [`BOARDS`] with an update whose body is the record's own members, unwrapped.
fn unwrapped_boards() -> String {
    let wrapped = r##"              properties:
                data:
                  type: object
                  properties:
                    attributes: {type: object, properties: {title: {type: string}}}"##;
    assert!(BOARDS.contains(wrapped));
    BOARDS.replace(
        wrapped,
        r##"              properties:
                attributes: {type: object, properties: {title: {type: string}}}"##,
    )
}

/// Lamps whose edit answers an acknowledgement. A lamp's read requires `label`, and
/// declares a boolean, an `enum` and a value object with a required member.
const LAMPS: &str = r##"
openapi: 3.0.0
info: {title: Lamps, version: "1"}
paths:
  /lamps/{id}:
    get:
      operationId: getLamp
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Lamp"}}}}
    put:
      operationId: editLamp
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {name: {type: string}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/LampAck"}}}}
components:
  schemas:
    Lamp:
      type: object
      required: [id, name, label]
      properties:
        id: {type: string}
        name: {type: string}
        label: {type: string}
        lit: {type: boolean}
        tone: {type: string, enum: [warm, cool]}
        socket:
          type: object
          required: [kind]
          properties:
            kind: {type: string}
            watts: {type: integer}
    LampAck:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
"##;

/// A lamp held in an earlier, incomplete shape: no `label`, which its read requires.
fn incomplete_lamp() -> Value {
    json!({"id": "l1", "name": "Old", "lit": false, "tone": "warm", "socket": {"kind": "E27", "watts": 40}})
}

/// Edits lamp `l1`, held as `held`, with an answer whose one effect sets `set`;
/// answers the effect's outcome and the lamp as stored afterwards (the edit itself
/// may have set its `name`).
async fn edit_lamp(held: Value, set: Value) -> (Value, Value) {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store(&state, "lamps", "l1", held).await;
    let answer = json!({"response": {"id": "l1", "name": "New"}, "effects": [
        {"type": "lamps", "id": "l1", "set": set},
    ]});
    let edit = json!({"name": "New"});
    let (_, effects) = call(LAMPS, &state, answer, ("PUT", "/lamps/l1", Some(edit))).await;
    (
        effects[0]["outcome"].clone(),
        stored(&state, "lamps", "l1").await,
    )
}

/// A lamp that already breaks its schema takes no wrong-typed value.
#[tokio::test]
async fn an_effect_adding_a_wrong_type_to_a_record_that_already_breaks_its_schema_is_refused() {
    let (outcome, lamp) = edit_lamp(incomplete_lamp(), json!({"lit": "yes"})).await;
    assert_eq!(outcome, "rejected", "{lamp}");
    assert_eq!(lamp["lit"], false, "{lamp}");
}

/// A lamp that already breaks its schema takes a value its member's schema takes.
#[tokio::test]
async fn an_effect_adding_no_violation_to_a_record_that_already_breaks_its_schema_is_applied() {
    let (outcome, lamp) = edit_lamp(incomplete_lamp(), json!({"lit": true})).await;
    assert_eq!(outcome, "applied", "{lamp}");
    assert_eq!(lamp["lit"], true, "{lamp}");
}

/// A lamp that already breaks its schema takes no `null` on a required member of its
/// value object.
#[tokio::test]
async fn an_effect_nulling_a_required_nested_member_of_a_record_that_already_breaks_its_schema_is_refused()
 {
    let (outcome, lamp) = edit_lamp(incomplete_lamp(), json!({"socket": {"kind": null}})).await;
    assert_eq!(outcome, "rejected", "{lamp}");
    assert_eq!(lamp["socket"], incomplete_lamp()["socket"], "{lamp}");
}

/// A lamp that already breaks its schema takes no value outside an `enum`.
#[tokio::test]
async fn an_effect_adding_a_value_outside_an_enum_to_a_record_that_already_breaks_its_schema_is_refused()
 {
    let (outcome, lamp) = edit_lamp(incomplete_lamp(), json!({"tone": "neon"})).await;
    assert_eq!(outcome, "rejected", "{lamp}");
    assert_eq!(lamp["tone"], "warm", "{lamp}");
}

/// A lamp that fits its schema still refuses an effect that breaks it.
#[tokio::test]
async fn an_effect_breaking_a_fitting_lamp_is_refused() {
    let mut fitting = incomplete_lamp();
    fitting["label"] = json!("Desk");
    let (outcome, lamp) = edit_lamp(fitting, json!({"tone": "neon"})).await;
    assert_eq!(outcome, "rejected", "{lamp}");
    assert_eq!(lamp["tone"], "warm", "{lamp}");
}

/// Fixtures whose read answers a union: a lamp (`label` required) or a bulb (`watts`
/// required), both with a boolean `lit` and a value object `base`. An edit answers an
/// acknowledgement.
const FIXTURES: &str = r##"
openapi: 3.0.0
info: {title: Fixtures, version: "1"}
paths:
  /fixtures/{id}:
    get:
      operationId: getFixture
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Fixture"}}}}
    put:
      operationId: editFixture
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {name: {type: string}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/FixtureAck"}}}}
components:
  schemas:
    Fixture:
      oneOf:
        - $ref: "#/components/schemas/Lamp"
        - $ref: "#/components/schemas/Bulb"
    Base:
      type: object
      properties:
        height: {type: integer}
        color: {type: string}
    Lamp:
      type: object
      required: [id, name, label]
      properties:
        id: {type: string}
        name: {type: string}
        label: {type: string}
        lit: {type: boolean}
        base: {$ref: "#/components/schemas/Base"}
    Bulb:
      type: object
      required: [id, name, watts]
      properties:
        id: {type: string}
        name: {type: string}
        watts: {type: integer}
        lit: {type: boolean}
        base: {$ref: "#/components/schemas/Base"}
    FixtureAck:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
"##;

/// Edits fixture `f1`, held as `held` (fitting neither variant), with an answer whose
/// one effect sets `set`; answers the effect's outcome and the fixture as stored.
async fn edit_fixture(set: Value) -> (Value, Value) {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let held = json!({"id": "f1", "name": "Old", "lit": false, "base": {"height": 3}});
    store(&state, "fixtures", "f1", held).await;
    let answer = json!({"response": {"id": "f1", "name": "New"}, "effects": [
        {"type": "fixtures", "id": "f1", "set": set},
    ]});
    let edit = json!({"name": "New"});
    let (_, effects) = call(
        FIXTURES,
        &state,
        answer,
        ("PUT", "/fixtures/f1", Some(edit)),
    )
    .await;
    (
        effects[0]["outcome"].clone(),
        stored(&state, "fixtures", "f1").await,
    )
}

/// Under a union read, a record that fits no variant shows one violation of the union
/// whatever its members hold: a wrong-typed member is still refused, by its own schema.
#[tokio::test]
async fn an_effect_adding_a_wrong_type_under_a_union_read_is_refused() {
    let (outcome, fixture) = edit_fixture(json!({"lit": "yes"})).await;
    assert_eq!(outcome, "rejected", "{fixture}");
    assert_eq!(fixture["lit"], false, "{fixture}");
}

/// The leaf a dotted path sets is checked against its own schema too.
#[tokio::test]
async fn an_effect_adding_a_wrong_type_by_a_path_under_a_union_read_is_refused() {
    let (outcome, fixture) = edit_fixture(json!({"base.height": "tall"})).await;
    assert_eq!(outcome, "rejected", "{fixture}");
    assert_eq!(fixture["base"], json!({"height": 3}), "{fixture}");
}

/// A value each member's schema takes still applies to a record that fits no variant.
#[tokio::test]
async fn an_effect_adding_no_violation_under_a_union_read_is_applied() {
    let (outcome, fixture) = edit_fixture(json!({"lit": true, "base.color": "red"})).await;
    assert_eq!(outcome, "applied", "{fixture}");
    assert_eq!(fixture["lit"], true, "{fixture}");
    assert_eq!(
        fixture["base"],
        json!({"height": 3, "color": "red"}),
        "{fixture}"
    );
}
