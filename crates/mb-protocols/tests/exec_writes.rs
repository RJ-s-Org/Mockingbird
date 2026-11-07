//! The state-first executor's updates and replaces: the held record changed by the
//! request, stored, and rendered into the answer, with no LLM call; and each case it
//! cannot decide going on to generation, logged with its `exec_fallback` reason.
//!
//! Its own binary, as `tests/exec.rs` is: it installs a tracing subscriber, and tracing
//! caches each callsite's interest process-wide.
#![cfg(test)]
#![cfg(feature = "http-rest")]

#[path = "it/support.rs"]
#[allow(dead_code)]
mod support;

use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use tracing::field::{Field, Visit};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

use support::{DescribedHarness, HarnessOptions};

/// The fields of one logged event that name the executor.
#[derive(Clone, Debug, Default, PartialEq)]
struct ExecLine {
    operation: String,
    exec: String,
    exec_fallback: Option<String>,
}

impl Visit for ExecLine {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "exec" => self.exec = value.to_owned(),
            "exec_fallback" => self.exec_fallback = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "operation" {
            self.operation = format!("{value:?}");
        }
    }
}

/// Records the executor's log lines of this thread.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<ExecLine>>>);

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut line = ExecLine::default();
        event.record(&mut line);
        if !line.exec.is_empty() {
            self.0.lock().unwrap().push(line);
        }
    }
}

impl Capture {
    fn install() -> (Self, DefaultGuard) {
        let capture = Self::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
        (capture, guard)
    }

    /// The lines logged for `operation`.
    fn of(&self, operation: &str) -> Vec<ExecLine> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.operation == operation)
            .cloned()
            .collect()
    }
}

fn line(operation: &str, kind: &str, fallback: Option<&str>) -> ExecLine {
    ExecLine {
        operation: operation.into(),
        exec: kind.into(),
        exec_fallback: fallback.map(str::to_owned),
    }
}

fn source(headers: &HeaderMap) -> &str {
    headers
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// Items are updated (`PATCH`) answering the record and replaced (`PUT`, a replace
/// where the world says so); notes are updated answering an acknowledgement; labels
/// answering nothing; boxes take a list of operations as their update; docs answer a
/// long-running operation.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Writes, version: "1"}
paths:
  /items:
    post:
      operationId: createItem
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/ItemInput"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /items/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getItem
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}, "404": {description: missing}}
    patch:
      operationId: updateItem
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/ItemPatch"}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "409": {description: conflict, content: {application/json: {schema: {type: object, properties: {message: {type: string}}}}}}
    put:
      operationId: replaceItem
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/ItemInput"}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /items/{id}/close:
    post:
      operationId: closeItem
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /items/{id}/touch:
    post:
      operationId: touchItem
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"204": {description: touched}}
  /notes:
    post:
      operationId: createNote
      requestBody: {content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
  /notes/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getNote
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}, "404": {description: missing}}
    patch:
      operationId: updateNote
      requestBody: {content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /labels:
    post:
      operationId: createLabel
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Label"}}}}}
  /labels/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getLabel
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Label"}}}}, "404": {description: missing}}
    patch:
      operationId: updateLabel
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"204": {description: updated}}
  /boxes:
    post:
      operationId: createBox
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Box"}}}}}
  /boxes/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBox
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Box"}}}}, "404": {description: missing}}
    patch:
      operationId: updateBox
      requestBody: {content: {application/json: {schema: {type: array, items: {type: object}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Box"}}}}}
  /docs:
    post:
      operationId: createDoc
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}}
  /docs/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getDoc
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}, "404": {description: missing}}
    patch:
      operationId: updateDoc
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"202": {description: accepted, content: {application/json: {schema: {$ref: "#/components/schemas/Operation"}}}}}
components:
  schemas:
    Item:
      type: object
      required: [id, name, size, created]
      properties:
        id: {type: string, readOnly: true}
        name: {type: string}
        size: {type: integer}
        note: {type: string, nullable: true}
        created: {type: string, readOnly: true}
        version: {type: integer}
        status: {type: string, enum: [open, edited]}
    ItemInput:
      type: object
      properties: {name: {type: string}, size: {type: integer}, note: {type: string, nullable: true}, status: {type: string}}
    ItemPatch:
      type: object
      properties: {name: {type: string, nullable: true}, size: {type: integer}, note: {type: string, nullable: true}, version: {type: integer}}
    Note:
      type: object
      required: [id, text, rev]
      properties: {id: {type: string}, text: {type: string}, rev: {type: integer}}
    Ack:
      type: object
      required: [ok, id, rev]
      properties: {ok: {type: boolean}, id: {type: string}, rev: {type: integer}}
    Label:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Box:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Doc:
      type: object
      required: [id, title]
      properties: {id: {type: string}, title: {type: string}}
    Operation:
      type: object
      required: [name, done]
      properties: {name: {type: string}, done: {type: boolean}, response: {$ref: "#/components/schemas/Doc"}}
"##;

/// The item every test starts from.
fn item() -> Value {
    json!({"id": "i1", "name": "a", "size": 1, "note": "n", "created": "2026-01-01"})
}

/// The type the contract gives operation `op`.
fn type_of(h: &DescribedHarness, op: &str) -> String {
    h.rt.bundle()
        .operation(op)
        .and_then(|o| o.resource_type.clone())
        .unwrap()
}

/// A harness over [`CONTRACT`] serving `world`, its item `i1` created.
async fn with_item(world: Value, script: Vec<Value>) -> DescribedHarness {
    let h = support::harness(CONTRACT, script, &HarnessOptions::default(), |rt| {
        support::with_effective(rt, world, false)
    });
    let (status, _, body) = h
        .send(
            "POST",
            "/items",
            Some(json!({"name": "a", "size": 1, "note": "n"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    h
}

/// An answer for the model that a reconciling or generating path would take: never
/// used by the executor.
fn decoy() -> Value {
    json!({"id": "i1", "name": "made up", "size": 99, "created": "1999-01-01"})
}

#[tokio::test]
async fn an_update_merges_its_body_into_the_held_record_with_no_llm_call() {
    let (capture, _guard) = Capture::install();
    let h = with_item(json!({}), vec![item(), decoy()]).await;
    let (status, headers, body) = h
        .send("PATCH", "/items/i1", Some(json!({"size": 5})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let merged = json!({"id": "i1", "name": "a", "size": 5, "note": "n", "created": "2026-01-01"});
    assert_eq!(body, merged);
    assert_eq!(
        h.provider.requests().len(),
        1,
        "the update made no LLM call"
    );
    assert_eq!(
        capture.of("updateItem"),
        vec![line("updateItem", "update", None)]
    );
    // The executor's record is what is stored, and what a read answers: nothing after
    // it reconciled either with an answer of the model's.
    let t = type_of(&h, "getItem");
    assert_eq!(h.stored(&t, "i1").await, Some(merged.clone()));
    let (status, headers, body) = h.send("GET", "/items/i1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state");
    assert_eq!(body, merged);
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn an_update_keeps_the_identity_and_read_only_members() {
    let (capture, _guard) = Capture::install();
    let h = with_item(json!({}), vec![item(), decoy(), decoy()]).await;
    // The path's id wins over the body's; a read-only member echoed is no change.
    let (status, _, body) = h
        .send(
            "PATCH",
            "/items/i1",
            Some(json!({"id": "other", "created": "2026-01-01", "name": "b"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"id": "i1", "name": "b", "size": 1, "note": "n", "created": "2026-01-01"})
    );
    assert_eq!(h.provider.requests().len(), 1);
    // A new value of a read-only member is the client's word against the contract's.
    let (status, _, body) = h
        .send(
            "PATCH",
            "/items/i1",
            Some(json!({"created": "never", "name": "c"})),
            &[],
        )
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(
        capture.of("updateItem"),
        vec![
            line("updateItem", "update", None),
            line("updateItem", "update", Some("read_only_member")),
        ]
    );
}

/// Docs whose `PUT` names their read-only `version` in its `409`: a token only a
/// model's answers are seen to move.
const TIED: &str = r##"
openapi: 3.0.0
info: {title: Tied, version: "1"}
paths:
  /docs:
    post:
      operationId: createDoc
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}}
  /docs/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getDoc
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}}
    put:
      operationId: putDoc
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}, version: {type: integer}}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
        "409": {description: "The `version` sent is stale.", content: {application/json: {schema: {type: object, properties: {message: {type: string}}}}}}
components:
  schemas:
    Doc:
      type: object
      required: [id, title, version]
      properties: {id: {type: string}, title: {type: string}, version: {type: integer, readOnly: true}}
"##;

#[tokio::test]
async fn an_update_advancing_a_token_only_a_model_moves_falls_back() {
    let (capture, _guard) = Capture::install();
    let doc = |title: &str, version: i64| json!({"id": "d1", "title": title, "version": version});
    let h = support::harness(
        TIED,
        vec![doc("a", 1), json!({"effects": [], "response": doc("b", 2)})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, body) = h
        .send(
            "PUT",
            "/docs/d1",
            Some(json!({"title": "b", "version": 1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], json!(2), "the model's version: {body}");
    assert_eq!(
        capture.of("putDoc"),
        vec![line("putDoc", "update", Some("model_token"))]
    );
}

#[tokio::test]
async fn a_null_the_member_admits_is_stored_and_one_it_does_not_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = with_item(json!({}), vec![item(), decoy()]).await;
    let (status, _, body) = h
        .send("PATCH", "/items/i1", Some(json!({"note": null})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["note"], Value::Null);
    assert_eq!(h.provider.requests().len(), 1);
    // Removing a required member adds a violation the record did not have.
    let (status, _, body) = h
        .send("PATCH", "/items/i1", Some(json!({"name": null})), &[])
        .await;
    assert!(status.is_success() || status.is_client_error(), "{body}");
    assert_eq!(
        capture.of("updateItem"),
        vec![
            line("updateItem", "update", None),
            line("updateItem", "update", Some("adds_violation")),
        ]
    );
}

#[tokio::test]
async fn an_update_sending_a_member_the_record_does_not_declare_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = with_item(json!({}), vec![item(), decoy()]).await;
    let (status, _, body) = h
        .send(
            "PATCH",
            "/items/i1",
            Some(json!({"name": "b", "extra": 1})),
            &[],
        )
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(
        capture.of("updateItem"),
        vec![line("updateItem", "update", Some("undeclared_member"))]
    );
}

#[tokio::test]
async fn an_update_whose_body_is_no_record_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": "b1", "name": "a"}),
            json!({"id": "b1", "name": "b"}),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/boxes", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, body) = h
        .send(
            "PATCH",
            "/boxes/b1",
            Some(json!([{"op": "replace", "path": "/name", "value": "b"}])),
            &[],
        )
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(
        capture.of("updateBox"),
        vec![line("updateBox", "update", Some("unmerged_body"))]
    );
}

#[tokio::test]
async fn an_update_on_an_action_route_falls_back() {
    let (capture, _guard) = Capture::install();
    let mut closed = item();
    closed["status"] = json!("edited");
    let h = with_item(json!({}), vec![item(), closed]).await;
    let (status, _, body) = h.send("POST", "/items/i1/close", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["status"],
        json!("edited"),
        "the model's answer: {body}"
    );
    assert_eq!(
        capture.of("closeItem"),
        vec![line("closeItem", "update", Some("action_route"))]
    );
}

#[tokio::test]
async fn an_update_of_a_record_with_writes_to_catch_up_on_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = with_item(
        json!({}),
        vec![
            item(),
            json!({"effects": []}),
            json!({"effects": [], "response": decoy()}),
        ],
    )
    .await;
    // An action answering nothing: what it did is remembered for the item.
    let (status, _, body) = h.send("POST", "/items/i1/touch", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _, body) = h
        .send("PATCH", "/items/i1", Some(json!({"size": 5})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        capture.of("updateItem"),
        vec![line("updateItem", "update", Some("behind"))]
    );
}

#[tokio::test]
async fn an_update_of_an_unheld_record_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(CONTRACT, vec![decoy()], &HarnessOptions::default(), |rt| rt);
    let (status, _, body) = h
        .send("PATCH", "/items/i9", Some(json!({"size": 2})), &[])
        .await;
    assert!(status.is_success() || status.is_client_error(), "{body}");
    assert_eq!(
        capture.of("updateItem"),
        vec![line("updateItem", "update", Some("not_held"))]
    );
}

/// The world of the replace tests: `replaceItem` replaces, and items have a lifecycle.
fn replacing() -> Value {
    json!({
        "types": {"items": {"lifecycle": {"member": "status", "states": ["open", "edited"], "initial": "open"}}},
        "operations": {"replaceItem": {"kind": "replace"}}
    })
}

#[tokio::test]
async fn a_replace_keeps_the_id_and_server_owned_members_and_drops_the_rest() {
    let (capture, _guard) = Capture::install();
    let mut created = item();
    created["status"] = json!("open");
    let h = with_item(replacing(), vec![created, decoy()]).await;
    let t = type_of(&h, "getItem");
    assert_eq!(t, "items", "the world above names this type");
    let (status, headers, body) = h
        .send(
            "PUT",
            "/items/i1",
            Some(json!({"name": "c", "size": 2})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let replaced =
        json!({"id": "i1", "name": "c", "size": 2, "created": "2026-01-01", "status": "open"});
    assert_eq!(body, replaced);
    assert_eq!(h.stored(&t, "i1").await, Some(replaced));
    assert_eq!(
        h.provider.requests().len(),
        1,
        "the replace made no LLM call"
    );
    assert_eq!(
        capture.of("replaceItem"),
        vec![line("replaceItem", "replace", None)]
    );
}

#[tokio::test]
async fn a_replace_missing_a_required_member_falls_back_for_fill() {
    let (capture, _guard) = Capture::install();
    let h = with_item(replacing(), vec![item(), decoy()]).await;
    let (status, _, body) = h
        .send("PUT", "/items/i1", Some(json!({"name": "c"})), &[])
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(
        capture.of("replaceItem"),
        vec![line("replaceItem", "replace", Some("needs_fill"))]
    );
}

#[tokio::test]
async fn an_update_applies_the_changes_the_description_says_it_makes() {
    let (_capture, _guard) = Capture::install();
    let world = json!({
        "types": {"items": {"lifecycle": {"member": "status", "states": ["open", "edited"], "initial": "open"}}},
        "operations": {"updateItem": {"transition": {"status": "edited"}}}
    });
    let mut created = item();
    created["status"] = json!("open");
    let h = with_item(world, vec![created, decoy()]).await;
    let (status, _, body) = h
        .send("PATCH", "/items/i1", Some(json!({"size": 4})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("edited"));
    assert_eq!(body["size"], json!(4));
    assert_eq!(
        h.stored("items", "i1").await.unwrap()["status"],
        json!("edited")
    );
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn an_update_advances_the_described_token_and_a_stale_one_is_refused() {
    let (_capture, _guard) = Capture::install();
    let world =
        json!({"types": {"items": {"concurrency": {"member": "version", "conflict": 409}}}});
    let mut created = item();
    created["version"] = json!(1);
    let h = with_item(world, vec![created, json!({"message": "stale"})]).await;
    let (status, _, body) = h
        .send(
            "PATCH",
            "/items/i1",
            Some(json!({"name": "b", "version": 1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], json!(2), "{body}");
    assert_eq!(h.stored("items", "i1").await.unwrap()["version"], json!(2));
    let (status, _, body) = h
        .send(
            "PATCH",
            "/items/i1",
            Some(json!({"name": "c", "version": 1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(h.stored("items", "i1").await.unwrap()["name"], json!("b"));
    let (status, _, body) = h
        .send(
            "PATCH",
            "/items/i1",
            Some(json!({"name": "c", "version": 2})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], json!(3), "{body}");
}

#[tokio::test]
async fn an_update_answering_an_acknowledgement_renders_it_from_the_record() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": "n1", "text": "hi", "rev": 7}),
            json!({"ok": true, "id": "made up", "rev": 99}),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/notes", Some(json!({"text": "hi"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h
        .send("PATCH", "/notes/n1", Some(json!({"text": "new"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["id"], json!("n1"), "{body}");
    assert_eq!(body["rev"], json!(7), "{body}");
    assert!(body["ok"].is_boolean(), "{body}");
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(
        h.stored(&type_of(&h, "getNote"), "n1").await,
        Some(json!({"id": "n1", "text": "new", "rev": 7}))
    );
    assert_eq!(
        capture.of("updateNote"),
        vec![line("updateNote", "update", None)]
    );
}

#[tokio::test]
async fn a_bodiless_update_stores_the_merged_record() {
    let (_capture, _guard) = Capture::install();
    let h = support::harness(
        CONTRACT,
        vec![json!({"id": "l1", "name": "a"}), json!({"effects": []})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/labels", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h
        .send("PATCH", "/labels/l1", Some(json!({"name": "b"})), &[])
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(source(&headers), "state");
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(
        h.stored(&type_of(&h, "getLabel"), "l1").await,
        Some(json!({"id": "l1", "name": "b"}))
    );
    let (status, headers, body) = h.send("GET", "/labels/l1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state");
    assert_eq!(body, json!({"id": "l1", "name": "b"}));
}

#[tokio::test]
async fn an_accepted_long_running_update_lands_at_once_and_reads_as_done() {
    let (_capture, _guard) = Capture::install();
    let world = json!({"operations": {"updateDoc": {"answer": {"long_running": {"done": "done", "result": "response"}}}}});
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": "d1", "title": "a"}),
            json!({"name": "op", "done": false}),
        ],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let (status, _, body) = h
        .send("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h
        .send("PATCH", "/docs/d1", Some(json!({"title": "b"})), &[])
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["done"], json!(true), "{body}");
    assert_eq!(
        body["response"],
        json!({"id": "d1", "title": "b"}),
        "{body}"
    );
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(
        h.stored(&type_of(&h, "getDoc"), "d1").await,
        Some(json!({"id": "d1", "title": "b"}))
    );
}

#[cfg(feature = "graphql")]
mod graphql {
    use std::sync::Arc;
    use std::time::Duration;

    use mb_engine::{Engine, EngineConfig};
    use mb_llm::{Provider, ScriptedProvider};
    use mb_protocols::MockRuntime;
    use mb_state::MemoryStore;
    use serde_json::{Value, json};

    use super::{Capture, line};

    const SDL: &str = r#"
type Widget { id: ID! name: String! size: Int }
input WidgetInput { name: String size: Int }
type Query { widget(id: ID!): Widget }
type Mutation {
  createWidget(name: String!, size: Int): Widget
  updateWidget(id: ID!, input: WidgetInput!): Widget
}
"#;

    async fn post(router: &axum::Router, query: &str) -> Value {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json!({"query": query}).to_string()))
            .unwrap();
        super::support::send_to(router, req).await.2
    }

    #[tokio::test]
    async fn a_graphql_update_mutation_is_executed_on_the_held_record() {
        let (capture, _guard) = Capture::install();
        let bundle = mb_import::compile(SDL.as_bytes()).unwrap();
        let t = bundle
            .operation("Query.widget")
            .and_then(|o| o.resource_type.clone())
            .unwrap();
        let world = json!({
            "operations": {
                "Mutation.updateWidget": {
                    "kind": "update", "type": t, "target": {"member": "id"},
                    "request": {"record": "/input"}
                }
            }
        });
        let provider = Arc::new(ScriptedProvider::new(
            [
                json!({"__typename": "Widget", "id": "w1", "name": "a", "size": 1}),
                json!({"__typename": "Widget", "id": "w1", "name": "made up", "size": 9}),
            ]
            .into_iter()
            .map(|v| Ok(v.to_string()))
            .collect(),
        ));
        let rt = MockRuntime::new(
            "described",
            Arc::new(bundle),
            Arc::new(Engine::new(
                Some(Arc::clone(&provider) as Arc<dyn Provider>),
                EngineConfig::default(),
            )),
            Arc::new(MemoryStore::new(Duration::from_secs(3600))),
        );
        let rt = super::support::with_effective(rt, world, false);
        let router = mb_protocols::graphql::try_router(Arc::new(rt)).unwrap();
        let created = post(
            &router,
            "mutation { createWidget(name: \"a\", size: 1) { id name size } }",
        )
        .await;
        assert_eq!(
            created["data"]["createWidget"]["id"],
            json!("w1"),
            "{created}"
        );
        let updated = post(
            &router,
            "mutation { updateWidget(id: \"w1\", input: {name: \"b\"}) { id name size } }",
        )
        .await;
        assert_eq!(
            updated["data"]["updateWidget"],
            json!({"id": "w1", "name": "b", "size": 1}),
            "{updated}"
        );
        assert_eq!(provider.requests().len(), 1, "the update made no LLM call");
        assert_eq!(
            capture.of("Mutation.updateWidget"),
            vec![line("Mutation.updateWidget", "update", None)]
        );
        let read = post(&router, "{ widget(id: \"w1\") { id name size } }").await;
        assert_eq!(
            read["data"]["widget"],
            json!({"id": "w1", "name": "b", "size": 1}),
            "{read}"
        );
    }
}
