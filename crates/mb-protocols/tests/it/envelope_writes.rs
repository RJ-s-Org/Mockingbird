//! Writes whose answer holds the record in an envelope, and the order one write's
//! records read back in. Driven over HTTP with a scripted LLM, on small synthetic
//! contracts.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{CachingStateStore, MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

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

fn harness(contract: &str, script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        CachingStateStore::wrap(
            Arc::clone(&store) as Arc<dyn StateStore>,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
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

    /// The last prompt the LLM was sent, as text.
    fn last_prompt(&self) -> String {
        format!("{:?}", self.provider.requests().last().unwrap())
    }

    async fn held(&self, t: &str, id: &str) -> Option<Value> {
        let held = self.store.get(&session(), t, id).await.unwrap();
        held.map(|r| r.data)
    }

    async fn listed(&self, t: &str) -> Vec<String> {
        let held = self.store.list(&session(), t, 50).await.unwrap();
        held.into_iter().map(|r| r.id).collect()
    }
}

/// Every answer `{success, messages, result}`, with `result: {}` on the base
/// envelope that each route's `allOf` extends with its own record.
const BASED: &str = r##"
openapi: 3.0.0
info: {title: Based, version: "1"}
paths:
  /gadgets:
    get:
      operationId: listGadgets
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                allOf:
                  - $ref: "#/components/schemas/Envelope"
                  - type: object
                    properties:
                      result: {type: array, items: {$ref: "#/components/schemas/Gadget"}}
  /gadgets/{gadget_id}:
    parameters: [{name: gadget_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getGadget
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                allOf:
                  - $ref: "#/components/schemas/Envelope"
                  - type: object
                    properties:
                      result: {$ref: "#/components/schemas/Gadget"}
    put:
      operationId: putGadget
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/GadgetInput"}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                allOf:
                  - $ref: "#/components/schemas/Envelope"
                  - type: object
                    properties:
                      result: {$ref: "#/components/schemas/Gadget"}
components:
  schemas:
    Envelope:
      type: object
      required: [success, messages, result]
      properties:
        success: {type: boolean}
        messages: {type: array, items: {type: string}}
        result: {}
    GadgetInput:
      type: object
      properties:
        name: {type: string}
        size: {type: integer}
    Gadget:
      type: object
      required: [id, name, size]
      properties:
        id: {type: string}
        name: {type: string}
        size: {type: integer}
"##;

/// A `PUT` with nothing held, answered in the envelope its `allOf` extends from a
/// base whose `result` is a placeholder, stores the record it holds. The read is
/// told it, and once it said its own envelope, serves it from state.
#[tokio::test]
async fn a_put_answered_in_an_extended_envelope_stores_its_record() {
    let envelope = |result: Value| json!({"success": true, "messages": [], "result": result});
    let record = json!({"id": "g1", "name": "replaced", "size": 3});
    let h = harness(
        BASED,
        vec![envelope(record.clone()), envelope(record.clone())],
    );
    let sent = json!({"name": "replaced", "size": 3});
    let (status, put) = h.call("PUT", "/gadgets/g1", Some(sent)).await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!(h.held("gadgets", "g1").await, Some(record));
    // The read's own envelope (a free `success`, `messages`) is not the write's to
    // say: it is generated once, knowing the record.
    let (status, read) = h.call("GET", "/gadgets/g1", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert!(h.last_prompt().contains("replaced"), "{}", h.last_prompt());
    let (status, again) = h.call("GET", "/gadgets/g1", None).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(h.calls(), 2, "the second read is served from state");
    assert_eq!(again["result"]["name"], "replaced", "{again}");
    assert_eq!(again["result"]["size"], 3, "{again}");
}

/// Writes answering an object the contract declares nothing of.
const UNDECLARED: &str = r##"
openapi: 3.0.0
info: {title: Undeclared, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {type: object}}}}
  /widgets/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getWidget
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
    put:
      operationId: putWidget
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object}}}}
components:
  schemas:
    Widget:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        color: {type: string}
"##;

/// With nothing held, an update answering an envelope the contract does not describe
/// stores the one member of it that is a record of the type and carries the target's
/// id; an acknowledgement, or a member of another id, stores nothing.
#[tokio::test]
async fn an_update_answered_in_an_undeclared_envelope_stores_the_record_it_holds() {
    let h = harness(
        UNDECLARED,
        vec![
            json!({"ok": true, "record": {"id": "w1", "name": "new", "color": "red"}}),
            json!({"ok": true}),
            json!({"ok": true, "record": {"id": "other", "name": "elsewhere"}}),
        ],
    );
    let (status, body) = h
        .call("PUT", "/widgets/w1", Some(json!({"name": "new"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        h.held("widgets", "w1").await,
        Some(json!({"id": "w1", "name": "new", "color": "red"}))
    );
    let (status, _) = h
        .call("PUT", "/widgets/w2", Some(json!({"name": "two"})))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.held("widgets", "w2").await, None);
    let (status, _) = h
        .call("PUT", "/widgets/w3", Some(json!({"name": "three"})))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.held("widgets", "w3").await, None);
    assert_eq!(h.held("widgets", "other").await, None);
}

/// A create answering an envelope the contract does not describe stores the one
/// member of it that is a record of the type.
#[tokio::test]
async fn a_create_answered_in_an_undeclared_envelope_stores_the_record_it_holds() {
    let h = harness(
        UNDECLARED,
        vec![json!({"ok": true, "record": {"id": "w7", "name": "made"}})],
    );
    let (status, body) = h
        .call("POST", "/widgets", Some(json!({"name": "made"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        h.held("widgets", "w7").await,
        Some(json!({"id": "w7", "name": "made"}))
    );
}

/// Blocks appended under a parent, answered as a list of the new blocks.
const BLOCKS: &str = r##"
openapi: 3.0.0
info: {title: Blocks, version: "1"}
paths:
  /blocks/{block_id}:
    parameters: [{name: block_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBlock
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Block"}}}}
  /blocks/{block_id}/children:
    parameters: [{name: block_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listChildren
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/BlockList"}}}}
    patch:
      operationId: appendChildren
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                children: {type: array, items: {type: object}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/BlockList"}}}}
components:
  schemas:
    Block:
      type: object
      required: [id, kind]
      properties:
        id: {type: string}
        kind: {type: string}
        text: {type: string}
    BlockList:
      type: object
      required: [object, results]
      properties:
        object: {type: string}
        results: {type: array, items: {$ref: "#/components/schemas/Block"}}
        has_more: {type: boolean}
"##;

/// The blocks one append answers read back in its order, before those of an earlier
/// write; the next list's prompt shows them so.
#[tokio::test]
async fn the_records_one_write_answers_read_back_in_its_order() {
    let block = |id: &str, kind: &str| json!({"id": id, "kind": kind, "text": id});
    let h = harness(
        BLOCKS,
        vec![
            json!({"object": "list", "results": [block("a1", "heading")], "has_more": false}),
            json!({"object": "list", "results": [
                block("b1", "paragraph"), block("b2", "to_do"), block("b3", "quote"),
            ], "has_more": false}),
            json!({"object": "list", "results": [], "has_more": false}),
        ],
    );
    let append = |n: usize| json!({"children": vec![json!({"kind": "paragraph"}); n]});
    let (status, _) = h
        .call("PATCH", "/blocks/page1/children", Some(append(1)))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .call("PATCH", "/blocks/page1/children", Some(append(3)))
        .await;
    assert_eq!(status, StatusCode::OK);
    let listed = h.listed("blocks").await;
    let order: Vec<&str> = listed
        .iter()
        .map(String::as_str)
        .filter(|id| ["a1", "b1", "b2", "b3"].contains(id))
        .collect();
    assert_eq!(order, ["b1", "b2", "b3", "a1"], "{listed:?}");
    let (status, _) = h.call("GET", "/blocks/page1/children", None).await;
    assert_eq!(status, StatusCode::OK);
    let prompt = h.last_prompt();
    let at = |id: &str| {
        prompt
            .find(&format!("\\\"text\\\": \\\"{id}\\\""))
            .or_else(|| prompt.find(&format!("\\\"text\\\":\\\"{id}\\\"")))
    };
    let (b1, b2, b3) = (at("b1"), at("b2"), at("b3"));
    assert!(b1.is_some() && b2.is_some() && b3.is_some(), "{prompt}");
    assert!(b1 < b2 && b2 < b3, "{b1:?} {b2:?} {b3:?}");
}

/// A list read's items read back in its order.
#[tokio::test]
async fn a_list_answer_s_items_read_back_in_its_order() {
    let block = |id: &str| json!({"id": id, "kind": "paragraph"});
    let h = harness(
        BLOCKS,
        vec![json!({"object": "list", "results": [block("c1"), block("c2"), block("c3")]})],
    );
    let (status, _) = h.call("GET", "/blocks/page1/children", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed = h.listed("blocks").await;
    let order: Vec<&str> = listed
        .iter()
        .map(String::as_str)
        .filter(|id| id.starts_with('c'))
        .collect();
    assert_eq!(order, ["c1", "c2", "c3"], "{listed:?}");
}

/// Every answer an envelope of scalars holding its records in an array, one record
/// or many: `{Id, Status, Things: [Thing], pagination}`.
const LISTED: &str = r##"
openapi: 3.0.0
info: {title: Listed, version: "1"}
paths:
  /Things:
    get:
      operationId: getThings
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Things"}}}}
    put:
      operationId: createThings
      requestBody:
        content:
          application/json:
            schema:
              type: object
              required: [Things]
              properties:
                Things: {type: array, items: {$ref: "#/components/schemas/Thing"}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Things"}}}}
  /Things/{ThingID}:
    parameters: [{name: ThingID, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getThing
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Things"}}}}
components:
  schemas:
    Envelope:
      type: object
      required: [Id, Status]
      properties:
        Id: {type: string}
        Status: {type: string}
    Page:
      type: object
      properties:
        page: {type: integer}
    Things:
      allOf:
        - $ref: "#/components/schemas/Envelope"
        - type: object
          required: [Things]
          properties:
            Things: {type: array, items: {$ref: "#/components/schemas/Thing"}}
            pagination: {$ref: "#/components/schemas/Page"}
    Thing:
      type: object
      properties:
        ThingID: {type: string, readOnly: true}
        Name: {type: string}
        ThingStatus: {type: string, enum: [ACTIVE, ARCHIVED]}
        UpdatedAt: {type: string, readOnly: true}
        Notes:
          type: array
          items:
            type: object
            properties:
              NoteID: {type: string, readOnly: true}
              Text: {type: string}
"##;

/// A request sending its records in an array keeps what the answer's records say
/// beside what it sent: their ids, status and times.
#[tokio::test]
async fn a_request_array_keeps_what_the_answer_s_records_assigned() {
    let h = harness(
        LISTED,
        vec![json!({
            "Id": "env-1",
            "Status": "OK",
            "Things": [{
                "ThingID": "t-1",
                "Name": "Alpha",
                "ThingStatus": "ACTIVE",
                "UpdatedAt": "2026-01-01T00:00:00Z",
                "Notes": [{"NoteID": "n-1", "Text": "first"}],
            }],
        })],
    );
    let sent = json!({"Things": [{"Name": "Alpha", "Notes": [{"Text": "first"}]}]});
    let (status, body) = h.call("PUT", "/Things", Some(sent)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let thing = &body["Things"][0];
    assert_eq!(thing["ThingID"], "t-1", "{body}");
    assert_eq!(thing["ThingStatus"], "ACTIVE", "{body}");
    assert_eq!(thing["UpdatedAt"], "2026-01-01T00:00:00Z", "{body}");
    assert_eq!(thing["Name"], "Alpha", "{body}");
    assert_eq!(thing["Notes"][0]["NoteID"], "n-1", "{body}");
}

/// A read by `{ThingID}` answering its record as the one item of the envelope's
/// array stores the item under its `ThingID`, leaves the envelope's `Id` its own,
/// and serves the next read from state.
#[tokio::test]
async fn a_record_held_as_an_envelope_s_one_item_goes_by_the_item_s_key() {
    let h = harness(
        LISTED,
        vec![json!({
            "Id": "env-9",
            "Status": "OK",
            "Things": [{"ThingID": "t-9", "Name": "Beta", "ThingStatus": "ACTIVE"}],
        })],
    );
    let (status, body) = h.call("GET", "/Things/t-9", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["Id"], "env-9", "{body}");
    assert_eq!(body["Things"][0]["ThingID"], "t-9", "{body}");
    let held = h.held("Things", "t-9").await;
    assert_eq!(
        held,
        Some(json!({"ThingID": "t-9", "Name": "Beta", "ThingStatus": "ACTIVE"}))
    );
    let (status, again) = h.call("GET", "/Things/t-9", None).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(h.calls(), 1, "the second read is served from state");
    assert_eq!(again["Things"][0]["Name"], "Beta", "{again}");
    assert_eq!(again["Things"].as_array().map(Vec::len), Some(1), "{again}");
}

const PARENTED: &str = r##"
openapi: 3.0.0
info: {title: Parented, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {type: object}}}}
  /folders:
    post:
      operationId: createFolder
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "204": {description: created}
  /folders/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getFolder
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
  /widgets/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getWidget
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
components:
  schemas:
    Widget:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        color: {type: string}
    Folder:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        color: {type: string}
"##;

/// A create whose answer lacks the new record's key but mentions a held record of the
/// same type (its parent), holding nothing the request sent, leaves that record be.
#[tokio::test]
async fn a_create_s_envelope_never_replaces_a_held_record_it_mentions() {
    let h = harness(
        PARENTED,
        vec![
            json!({"id": "w1", "name": "parent", "color": "red"}),
            json!({"ok": true, "parent": {"id": "w1", "name": "parent"}}),
        ],
    );
    let (status, _) = h.call("GET", "/widgets/w1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h.held("widgets", "w1").await,
        Some(json!({"id": "w1", "name": "parent", "color": "red"}))
    );
    let (status, body) = h
        .call(
            "POST",
            "/widgets",
            Some(json!({"name": "child", "parent_id": "w1"})),
        )
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(
        h.held("widgets", "w1").await,
        Some(json!({"id": "w1", "name": "parent", "color": "red"})),
        "the parent must not be overwritten by a create of another record"
    );
}

/// A bodyless create stores nothing of a record its request names: the envelope
/// fallback reads only an answer.
#[tokio::test]
async fn a_bodyless_create_stores_nothing_the_request_mentions() {
    let h = harness(
        PARENTED,
        vec![json!({"id": "f1", "name": "root", "color": "blue"})],
    );
    let (status, _) = h.call("GET", "/folders/f1", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = h
        .call(
            "POST",
            "/folders",
            Some(json!({"name": "child", "parent": {"id": "f1", "name": "stub"}})),
        )
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(
        h.held("folders", "f1").await,
        Some(json!({"id": "f1", "name": "root", "color": "blue"})),
        "a request's reference to the parent must not replace it"
    );
}

const ORDERS: &str = r##"
openapi: 3.0.0
info: {title: Orders, version: "1"}
paths:
  /orders/{order_id}:
    parameters: [{name: order_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getOrder
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}
components:
  schemas:
    Order:
      type: object
      properties:
        id: {type: string}
        total: {type: number}
        lines:
          type: array
          items:
            type: object
            properties:
              order_id: {type: string}
              sku: {type: string}
"##;

/// An order with one line, which refers back to it, is stored as the order.
#[tokio::test]
async fn an_order_with_one_line_is_stored_as_the_order() {
    let order = json!({"id": "o1", "total": 9.5, "lines": [{"order_id": "o1", "sku": "S-1"}]});
    let h = harness(ORDERS, vec![order.clone(), order.clone()]);
    let (status, body) = h.call("GET", "/orders/o1", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let held = h.held("orders", "o1").await;
    assert_eq!(held, Some(order));
}
