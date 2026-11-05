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
