//! The state-first executor's seam: a read the description classifies is answered from
//! the store and logged with `exec`; one it cannot answer goes on to generation, logged
//! with `exec_fallback` and its reason.
//!
//! Its own binary, as `tests/spans.rs` is: it installs a tracing subscriber, and tracing
//! caches each callsite's interest process-wide.
#![cfg(test)]
#![cfg(feature = "http-rest")]

#[path = "it/support.rs"]
#[allow(dead_code)]
mod support;

use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use serde_json::json;
use tracing::field::{Field, Visit};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

use support::HarnessOptions;

/// The fields of one logged event that name the executor.
#[derive(Clone, Debug, Default, PartialEq)]
struct ExecLine {
    operation: String,
    exec: String,
    exec_fallback: Option<String>,
    render_error: Option<String>,
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
        match field.name() {
            "operation" => self.operation = format!("{value:?}"),
            "render_error" => self.render_error = Some(format!("{value:?}")),
            _ => {}
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

/// Widgets answer their create with fewer members than their read requires.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Exec, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/NewWidget"}}}}}
  /widgets/{id}:
    get:
      operationId: getWidget
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
        "404": {description: missing, content: {application/json: {schema: {type: object, properties: {message: {type: string}}}}}}
components:
  schemas:
    NewWidget:
      type: object
      required: [id]
      properties: {id: {type: string}, name: {type: string}, size: {type: integer}}
    Widget:
      type: object
      required: [id, name, size]
      properties: {id: {type: string}, name: {type: string}, size: {type: integer}}
"##;

fn source(headers: &HeaderMap) -> &str {
    headers
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// A create's seed: a seeded create takes the generated path, which stores the record
/// the model answered under the id it chose, as the tests written before creates were
/// executed expect.
const SEEDED: &[(&str, &str)] = &[("x-mockingbird-seed", "1")];

fn exec(fallback: Option<&str>) -> ExecLine {
    ExecLine {
        operation: "getWidget".into(),
        exec: "read".into(),
        exec_fallback: fallback.map(str::to_owned),
        render_error: None,
    }
}

#[tokio::test]
async fn a_held_record_is_read_by_the_executor() {
    let (capture, _guard) = Capture::install();
    // The one answer is the create's fill of `size`.
    let h = support::harness(
        CONTRACT,
        vec![json!({"size": 3})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/widgets", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, headers, body) = h.send("GET", &format!("/widgets/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"id": id, "name": "a", "size": 3}));
    assert_eq!(capture.of("getWidget"), vec![exec(None)]);
    assert_eq!(h.provider.requests().len(), 1, "the read made no LLM call");
}

#[tokio::test]
async fn an_unheld_id_generates_under_unknown_ids_generate() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        CONTRACT,
        vec![json!({"id": "w9", "name": "made up", "size": 1})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, headers, body) = h.send("GET", "/widgets/w9", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "llm", "{body}");
    assert_eq!(capture.of("getWidget"), vec![exec(Some("not_held"))]);
}

#[tokio::test]
async fn an_unheld_id_is_not_found_under_unknown_ids_404() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        CONTRACT,
        vec![json!({"message": "widget w9 not found"})],
        &HarnessOptions {
            unknown_ids_not_found: true,
            ..HarnessOptions::default()
        },
        |rt| rt,
    );
    let (status, _, body) = h.send("GET", "/widgets/w9", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // Refused before the executor is reached.
    assert!(capture.of("getWidget").is_empty());
}

#[tokio::test]
async fn a_record_that_does_not_render_generates() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": "w1", "name": "a"}),
            json!({"id": "w1", "name": "a", "size": 2}),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    // A seeded create takes the generated path, which stores the model's short record.
    let (status, _, body) = h
        .send("POST", "/widgets", Some(json!({"name": "a"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h.send("GET", "/widgets/w1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "llm", "{body}");
    // The stored record lacks a member the read requires: logged with why.
    let failed = ExecLine {
        render_error: Some("/size required".into()),
        ..exec(Some("render_failed"))
    };
    assert_eq!(capture.of("getWidget"), vec![failed]);
}

#[tokio::test]
async fn consistency_off_falls_back_and_still_reads_the_held_record() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        CONTRACT,
        vec![json!({"id": "w1", "name": "a", "size": 3})],
        &HarnessOptions::default(),
        |mut rt| {
            rt.consistency = false;
            rt
        },
    );
    let (status, _, body) = h
        .send("POST", "/widgets", Some(json!({"name": "a"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h.send("GET", "/widgets/w1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(capture.of("getWidget"), vec![exec(Some("consistency_off"))]);
}

/// A create and a read of a deal answering `{<envelope's members>, data: Deal}`, the
/// envelope's other members as `envelope` gives them, and a list saying deals are
/// `Deal`s.
fn enveloped_contract(envelope: &str) -> String {
    format!(
        r##"
openapi: 3.0.0
info: {{title: Envelopes, version: "1"}}
paths:
  /deals:
    get:
      operationId: getDeals
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{data: {{type: array, items: {{$ref: "#/components/schemas/Deal"}}}}}}}}}}}}}}}}
    post:
      operationId: addDeal
      requestBody: {{content: {{application/json: {{schema: {{type: object, properties: {{title: {{type: string}}}}}}}}}}}}
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/DealResponse"}}}}}}}}}}
  /deals/{{id}}:
    get:
      operationId: getDeal
      parameters: [{{name: id, in: path, required: true, schema: {{type: integer}}}}]
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/DealResponse"}}}}}}}}
        "404": {{description: missing, content: {{application/json: {{schema: {{type: object, properties: {{error: {{type: string}}}}}}}}}}}}
components:
  schemas:
    Deal:
      type: object
      required: [id, title]
      properties: {{id: {{type: integer}}, title: {{type: string}}}}
    Envelope:
{envelope}
    DealResponse:
      allOf:
        - $ref: "#/components/schemas/Envelope"
        - type: object
          required: [data]
          properties: {{data: {{$ref: "#/components/schemas/Deal"}}}}
"##
    )
}

/// The envelopes of the bench's reads that fell back with `render_failed`: members
/// beside the record that nothing kept and that their schema does not fix, none a
/// boolean.
const ENVELOPES: [(&str, &str); 2] = [
    // Zoho Books' `{code, message, invoice}`: a free integer and string.
    (
        "code, message",
        r#"      type: object
      required: [code, message]
      properties: {code: {type: integer}, message: {type: string}}"#,
    ),
    // Cloudflare's `{success, errors, messages, result}`: `messages` is a free array.
    (
        "success, errors, messages",
        r#"      type: object
      required: [success, errors, messages]
      properties:
        success: {type: boolean, enum: [true]}
        errors: {type: array, maxItems: 0, items: {type: object}}
        messages: {type: array, items: {type: object, required: [code, message], properties: {code: {type: integer, minimum: 1000}, message: {type: string}}}}"#,
    ),
];

#[tokio::test]
async fn an_enveloped_record_is_read_with_the_envelope_drawn_by_the_faker() {
    for (members, envelope) in ENVELOPES {
        let (capture, _guard) = Capture::install();
        let contract = enveloped_contract(envelope);
        let created = match members {
            "code, message" => {
                json!({"code": 0, "message": "Created", "data": {"id": 7, "title": "a"}})
            }
            _ => {
                json!({"success": true, "errors": [], "messages": [], "data": {"id": 7, "title": "a"}})
            }
        };
        let h = support::harness(&contract, vec![created], &HarnessOptions::default(), |rt| {
            rt
        });
        let (status, _, body) = h
            .send("POST", "/deals", Some(json!({"title": "a"})), SEEDED)
            .await;
        assert_eq!(status, StatusCode::OK, "{members}: {body}");
        let (status, headers, body) = h.send("GET", "/deals/7", None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{members}");
        assert_eq!(source(&headers), "state", "{members}: {body}");
        assert_eq!(body["data"], json!({"id": 7, "title": "a"}), "{members}");
        for member in members.split(", ") {
            assert!(body.get(member).is_some(), "{members}: {body}");
        }
        let read = ExecLine {
            operation: "getDeal".into(),
            exec: "read".into(),
            exec_fallback: None,
            render_error: None,
        };
        assert_eq!(capture.of("getDeal"), vec![read], "{members}");
        assert_eq!(
            h.provider.requests().len(),
            1,
            "{members}: the read made no LLM call"
        );
        // A second read answers the envelope the same.
        let (_, _, again) = h.send("GET", "/deals/7", None, &[]).await;
        assert_eq!(again, body, "{members}");
    }
}

/// Pipedrive's `{success, data}`: a boolean nothing tells is `true` in a success, never
/// a random draw. What the schema tells (a `default`) goes first.
#[tokio::test]
async fn a_free_boolean_in_the_envelope_is_true() {
    let deal = json!({"id": 7, "title": "a"});
    let free = r#"      type: object
      required: [success]
      properties: {success: {type: boolean}}"#;
    for (envelope, success) in [
        (free.to_owned(), true),
        (
            free.replace("{type: boolean}", "{type: boolean, default: false}"),
            false,
        ),
    ] {
        let (capture, _guard) = Capture::install();
        let h = support::harness(
            &enveloped_contract(&envelope),
            vec![json!({"success": true, "data": deal})],
            &HarnessOptions::default(),
            |rt| rt,
        );
        let (status, _, body) = h
            .send("POST", "/deals", Some(json!({"title": "a"})), SEEDED)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, headers, body) = h.send("GET", "/deals/7", None, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(source(&headers), "state", "{body}");
        assert_eq!(body, json!({"success": success, "data": deal}));
        let read = ExecLine {
            operation: "getDeal".into(),
            exec: "read".into(),
            exec_fallback: None,
            render_error: None,
        };
        assert_eq!(capture.of("getDeal"), vec![read]);
        assert_eq!(h.provider.requests().len(), 1, "the read made no LLM call");
    }
}

/// What the session's answers said of the read's envelope goes over what the executor
/// draws: a read on today's path (seeded) is generated with `success: false` and
/// `code: 5`, which are kept, and a `note` too long to keep. The executor's read then
/// answers the kept members from state and draws only the `note`.
#[tokio::test]
async fn what_a_read_said_of_its_envelope_goes_over_a_draw() {
    let deal = json!({"id": 7, "title": "a"});
    let long = "x".repeat(300);
    let said = json!({"success": false, "code": 5, "note": long, "data": deal});
    let envelope = r#"      type: object
      required: [success, code, note]
      properties: {success: {type: boolean}, code: {type: integer}, note: {type: string}}"#;
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        &enveloped_contract(envelope),
        vec![
            json!({"success": true, "code": 0, "note": "made", "data": deal}),
            said.clone(),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/deals", Some(json!({"title": "a"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seeded = [("x-mockingbird-seed", "1")];
    let (status, headers, body) = h.send("GET", "/deals/7", None, &seeded).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "llm", "{body}");
    assert_eq!(body, said);
    assert_eq!(h.provider.requests().len(), 2);

    let (status, headers, body) = h.send("GET", "/deals/7", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["success"], false, "{body}");
    assert_eq!(body["code"], 5, "{body}");
    assert_eq!(body["data"], deal, "{body}");
    // Not kept, so drawn.
    assert!(body["note"].is_string() && body["note"] != long, "{body}");
    let read = ExecLine {
        operation: "getDeal".into(),
        exec: "read".into(),
        exec_fallback: None,
        render_error: None,
    };
    assert_eq!(capture.of("getDeal").last(), Some(&read));
    assert_eq!(h.provider.requests().len(), 2, "the read made no LLM call");
}

/// Notes are deleted answering nothing; tags answering the deleted record; folders are
/// soft-deleted.
const DELETES: &str = r##"
openapi: 3.0.0
info: {title: Deletes, version: "1"}
paths:
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
    delete:
      operationId: deleteNote
      parameters: [{name: Idempotency-Key, in: header, schema: {type: string}}]
      responses: {"204": {description: deleted}}
  /tags:
    post:
      operationId: createTag
      requestBody: {content: {application/json: {schema: {type: object, properties: {label: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}}
  /tags/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTag
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}, "404": {description: missing}}
    delete:
      operationId: deleteTag
      responses: {"200": {description: deleted, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}}
  /folders:
    post:
      operationId: createFolder
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
  /folders/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getFolder
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}, "404": {description: missing}}
    delete:
      operationId: deleteFolder
      parameters: [{name: force, in: query, schema: {type: boolean}}]
      responses: {"204": {description: deleted}}
  /folders/{id}/touch:
    post:
      operationId: touchFolder
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"204": {description: touched}}
components:
  schemas:
    Note:
      type: object
      required: [id, text]
      properties: {id: {type: string}, text: {type: string}}
    Tag:
      type: object
      required: [id, label]
      properties: {id: {type: string}, label: {type: string}}
    Folder:
      type: object
      required: [id, name, state]
      properties: {id: {type: string}, name: {type: string}, state: {type: string, enum: [ACTIVE, DELETED]}}
"##;

/// The type the contract gives operation `op`.
fn type_of(h: &support::DescribedHarness, op: &str) -> String {
    h.rt.bundle()
        .operation(op)
        .and_then(|o| o.resource_type.clone())
        .unwrap()
}

#[tokio::test]
async fn a_delete_removes_the_held_record_with_no_llm_call() {
    let (capture, _guard) = Capture::install();
    // One answer: the create's. A delete asking the model would find none.
    let h = support::harness(
        DELETES,
        vec![json!({"id": "n1", "text": "hi"})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/notes", Some(json!({"text": "hi"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h.send("DELETE", "/notes/n1", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(source(&headers), "state");
    assert_eq!(body, serde_json::Value::Null);
    assert_eq!(
        h.provider.requests().len(),
        1,
        "the delete made no LLM call"
    );
    assert_eq!(h.stored(&type_of(&h, "getNote"), "n1").await, None);
    let lines: Vec<ExecLine> = capture.0.lock().unwrap().clone();
    assert!(
        lines.contains(&ExecLine {
            operation: "deleteNote".into(),
            exec: "delete".into(),
            exec_fallback: None,
            render_error: None,
        }),
        "{lines:?}"
    );
    let (status, _, body) = h.send("GET", "/notes/n1", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn a_delete_answering_the_record_answers_it_as_held() {
    let (_capture, _guard) = Capture::install();
    let h = support::harness(
        DELETES,
        // A second answer a reconciling path could take for the delete: never used.
        vec![
            json!({"id": "t1", "label": "red"}),
            json!({"id": "t1", "label": "made up"}),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/tags", Some(json!({"label": "red"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h.send("DELETE", "/tags/t1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"id": "t1", "label": "red"}));
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(h.stored(&type_of(&h, "getTag"), "t1").await, None);
}

#[tokio::test]
async fn a_soft_delete_keeps_the_record_in_its_deleted_state() {
    let (_capture, _guard) = Capture::install();
    let world = json!({"types": {"folders": {"lifecycle": {
        "member": "state", "states": ["ACTIVE", "DELETED"], "initial": "ACTIVE",
        "soft_delete": {"state": "DELETED"}
    }}}});
    let h = support::harness(
        DELETES,
        vec![json!({"id": "f1", "name": "docs", "state": "ACTIVE"})],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let t = type_of(&h, "getFolder");
    assert_eq!(t, "folders", "the world above names this type");
    let (status, _, body) = h
        .send("POST", "/folders", Some(json!({"name": "docs"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h.send("DELETE", "/folders/f1", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(source(&headers), "state");
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(
        h.stored(&t, "f1").await,
        Some(json!({"id": "f1", "name": "docs", "state": "DELETED"}))
    );
}

/// A record already in its soft-deleted state has nowhere further to move: a second
/// delete (Okta's deactivate, then delete) removes it, and its read is not found.
#[tokio::test]
async fn a_delete_of_a_soft_deleted_record_removes_it() {
    let (capture, _guard) = Capture::install();
    let world = json!({"types": {"folders": {"lifecycle": {
        "member": "state", "states": ["ACTIVE", "DELETED"], "initial": "ACTIVE",
        "soft_delete": {"state": "DELETED"}
    }}}, "operations": {"getFolder": {"kind": "read", "type": "folders", "not_found": 404}}});
    let h = support::harness(
        DELETES,
        vec![json!({"id": "f1", "name": "docs", "state": "ACTIVE"})],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let t = type_of(&h, "getFolder");
    let (status, _, body) = h
        .send("POST", "/folders", Some(json!({"name": "docs"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for _ in 0..2 {
        let (status, headers, body) = h.send("DELETE", "/folders/f1", None, &[]).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        assert_eq!(source(&headers), "state");
    }
    assert_eq!(h.stored(&t, "f1").await, None);
    assert_eq!(h.provider.requests().len(), 1, "no delete asked the model");
    assert!(
        capture
            .of("deleteFolder")
            .iter()
            .all(|l| l.exec_fallback.is_none()),
        "{:?}",
        capture.of("deleteFolder")
    );
    let (status, _, body) = h.send("GET", "/folders/f1", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// A delete sending the soft delete's `unless` parameter at its value removes the record
/// at once, on the executor's path and on the generated one (a seeded delete).
#[tokio::test]
async fn a_delete_sending_the_soft_delete_s_unless_parameter_removes_the_record() {
    let (capture, _guard) = Capture::install();
    let world = json!({"types": {"folders": {"lifecycle": {
        "member": "state", "states": ["ACTIVE", "DELETED"], "initial": "ACTIVE",
        "soft_delete": {"state": "DELETED", "unless": {"param": "force", "value": true}}
    }}}, "operations": {"getFolder": {"kind": "read", "type": "folders", "not_found": 404}}});
    let h = support::harness(
        DELETES,
        vec![
            json!({"id": "f1", "name": "docs", "state": "ACTIVE"}),
            json!({"id": "f2", "name": "logs", "state": "ACTIVE"}),
        ],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let t = type_of(&h, "getFolder");
    for name in ["docs", "logs"] {
        let (status, _, body) = h
            .send("POST", "/folders", Some(json!({"name": name})), SEEDED)
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    // Another value of the parameter soft-deletes, as a delete without it does.
    let (status, _, body) = h.send("DELETE", "/folders/f1?force=false", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        h.stored(&t, "f1").await,
        Some(json!({"id": "f1", "name": "docs", "state": "DELETED"}))
    );
    let (status, headers, body) = h.send("DELETE", "/folders/f1?force=true", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(source(&headers), "state");
    assert_eq!(h.stored(&t, "f1").await, None);
    let (status, _, body) = h.send("GET", "/folders/f1", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        capture
            .of("deleteFolder")
            .iter()
            .all(|l| l.exec_fallback.is_none()),
        "{:?}",
        capture.of("deleteFolder")
    );
    // A seeded delete takes the generated path, which removes it too.
    let seeded = [("x-mockingbird-seed", "7")];
    let (status, _, body) = h
        .send("DELETE", "/folders/f2?force=true", None, &seeded)
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(h.stored(&t, "f2").await, None);
    assert_eq!(
        h.provider.requests().len(),
        2,
        "no executed delete asked the model"
    );
}

/// SCIM-like users whose error body says its HTTP status as free text.
const SCIM_ERRORS: &str = r##"
openapi: 3.0.0
info: {title: Scim errors, version: "1"}
paths:
  /Users:
    post:
      operationId: createUser
      requestBody: {content: {application/json: {schema: {type: object, properties: {userName: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}}
  /Users/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getUser
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
        "404": {description: missing, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
    delete:
      operationId: deleteUser
      responses: {"204": {description: deleted}}
components:
  schemas:
    User:
      type: object
      required: [id, userName]
      properties: {id: {type: string}, userName: {type: string}}
    Error:
      type: object
      required: [status]
      properties:
        status: {type: string, description: "The HTTP status as a string."}
        detail: {type: string}
"##;

/// A refusal from state (a read of a deleted record) says its HTTP status in its error
/// body's free `status` text, whatever the body was drawn with.
#[tokio::test]
async fn a_refusal_from_state_says_its_status_in_a_free_status_member() {
    let (_capture, _guard) = Capture::install();
    let world = json!({"operations": {"getUser": {"kind": "read", "not_found": 404}}});
    let h = support::harness(
        SCIM_ERRORS,
        vec![json!({"id": "u1", "userName": "ann"})],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let (status, _, body) = h
        .send("POST", "/Users", Some(json!({"userName": "ann"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, body) = h.send("DELETE", "/Users/u1", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _, body) = h.send("GET", "/Users/u1", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["status"], json!("404"), "{body}");
}

/// The generated path agrees: a seeded delete (`seeded`, which the executor leaves to
/// generation) of a record already soft-deleted removes it too.
#[tokio::test]
async fn a_generated_delete_of_a_soft_deleted_record_removes_it() {
    let (capture, _guard) = Capture::install();
    let world = json!({"types": {"folders": {"lifecycle": {
        "member": "state", "states": ["ACTIVE", "DELETED"], "initial": "ACTIVE",
        "soft_delete": {"state": "DELETED"}
    }}}});
    let h = support::harness(
        DELETES,
        vec![json!({"id": "f1", "name": "docs", "state": "ACTIVE"})],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let t = type_of(&h, "getFolder");
    let (status, _, body) = h
        .send("POST", "/folders", Some(json!({"name": "docs"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, body) = h.send("DELETE", "/folders/f1", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        h.stored(&t, "f1").await,
        Some(json!({"id": "f1", "name": "docs", "state": "DELETED"}))
    );
    let seeded = [("x-mockingbird-seed", "7")];
    let (status, _, body) = h.send("DELETE", "/folders/f1", None, &seeded).await;
    assert!(status.is_success(), "{body}");
    assert_eq!(
        capture
            .of("deleteFolder")
            .last()
            .and_then(|l| l.exec_fallback.clone()),
        Some("seeded".to_owned())
    );
    assert_eq!(h.stored(&t, "f1").await, None);
}

#[tokio::test]
async fn a_delete_of_an_unheld_record_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(DELETES, vec![], &HarnessOptions::default(), |rt| rt);
    let (status, _, _) = h.send("DELETE", "/notes/n9", None, &[]).await;
    assert!(status.is_success() || status.is_client_error());
    let lines: Vec<ExecLine> = capture.0.lock().unwrap().clone();
    assert!(
        lines.contains(&ExecLine {
            operation: "deleteNote".into(),
            exec: "delete".into(),
            exec_fallback: Some("not_held".into()),
            render_error: None,
        }),
        "{lines:?}"
    );
}

#[tokio::test]
async fn an_executed_write_settles_what_write_memory_kept_for_its_record() {
    let (capture, _guard) = Capture::install();
    let world = json!({"types": {"folders": {"lifecycle": {
        "member": "state", "states": ["ACTIVE", "DELETED"], "initial": "ACTIVE",
        "soft_delete": {"state": "DELETED"}
    }}}});
    let h = support::harness(
        DELETES,
        vec![
            json!({"id": "f1", "name": "docs", "state": "ACTIVE"}),
            json!({"effects": []}),
            json!({"id": "f1", "name": "renamed", "state": "ACTIVE"}),
        ],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let (status, _, body) = h
        .send("POST", "/folders", Some(json!({"name": "docs"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // An action answering nothing: write memory keeps its request for the folder.
    let (status, _, body) = h
        .send(
            "POST",
            "/folders/f1/touch",
            Some(json!({"name": "renamed"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _, body) = h.send("DELETE", "/folders/f1", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    // The soft-deleted folder reads back from state, not behind the action.
    let (status, headers, body) = h.send("GET", "/folders/f1", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["state"], json!("DELETED"));
    assert!(
        !capture
            .of("getFolder")
            .iter()
            .any(|l| l.exec_fallback.as_deref() == Some("behind")),
        "{:?}",
        capture.of("getFolder")
    );
}

/// Boxes are deleted answering their record in an envelope, an acknowledgement sharing
/// the record's id, or a message sharing nothing with it.
const ANSWERS: &str = r##"
openapi: 3.0.0
info: {title: Answers, version: "1"}
paths:
  /boxes:
    post:
      operationId: createBox
      requestBody: {content: {application/json: {schema: {type: object, properties: {label: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Box"}}}}}
  /boxes/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBox
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Box"}}}}, "404": {description: missing}}
    delete:
      operationId: deleteBox
      responses:
        "200":
          description: deleted
          content: {application/json: {schema: {type: object, required: [data], properties: {data: {$ref: "#/components/schemas/Box"}}}}}
  /boxes/{id}/ack:
    delete:
      operationId: dropBox
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: dropped
          content: {application/json: {schema: {type: object, required: [id, deleted], properties: {id: {type: string}, deleted: {type: boolean}}}}}
  /boxes/{id}/said:
    delete:
      operationId: binBox
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: binned
          content: {application/json: {schema: {type: object, required: [message], properties: {message: {type: string}}}}}
  /boxes/{id}/gone:
    delete:
      operationId: removeBox
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: removed
          content: {application/json: {schema: {type: object, required: [id, deleted], properties: {id: {type: string}, deleted: {type: boolean, enum: [true]}}}}}
  /boxes/{id}/wrapped:
    delete:
      operationId: trashBox
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: trashed
          content: {application/json: {schema: {type: object, required: [success, data], properties: {success: {type: boolean}, data: {type: object, required: [id], properties: {id: {type: string}}}}}}}
  /boxes/{id}/owned:
    delete:
      operationId: shedBox
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: shed
          content: {application/json: {schema: {type: object, required: [id, owner], properties: {id: {type: string}, owner: {type: object, required: [id, label], properties: {id: {type: string}, label: {type: string}}}}}}}
  /boxes/{id}/status:
    delete:
      operationId: closeBox
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: closed
          content: {application/json: {schema: {type: object, required: [id, label], properties: {id: {type: string}, label: {type: string, enum: [closed]}}}}}
components:
  schemas:
    Box:
      type: object
      required: [id, label]
      properties: {id: {type: string}, label: {type: string}, deleted: {type: boolean}}
"##;

/// The `Box` type, its create made and served from state.
async fn a_box(world: serde_json::Value) -> support::DescribedHarness {
    let h = support::harness(
        ANSWERS,
        vec![json!({"id": "b1", "label": "red"})],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let (status, _, body) = h
        .send("POST", "/boxes", Some(json!({"label": "red"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    h
}

#[tokio::test]
async fn a_delete_answers_its_record_in_the_envelope_its_operation_holds_it_in() {
    let (_capture, _guard) = Capture::install();
    let h = a_box(json!({"operations": {"deleteBox": {"answer": {"record": "/data"}}}})).await;
    let (status, headers, body) = h.send("DELETE", "/boxes/b1", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"data": {"id": "b1", "label": "red"}}));
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn an_acknowledgement_takes_the_record_s_members_and_the_faker_s_for_the_rest() {
    let (_capture, _guard) = Capture::install();
    let world = json!({"operations": {"dropBox": {"kind": "delete", "type": "boxes"}}});
    let h = a_box(world).await;
    let (status, headers, body) = h.send("DELETE", "/boxes/b1/ack", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["id"], json!("b1"), "{body}");
    assert!(body["deleted"].is_boolean(), "{body}");
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn an_answer_sharing_nothing_with_the_record_is_the_faker_s_acknowledgement() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"binBox": {"kind": "delete", "type": "boxes"}}});
    let h = a_box(world).await;
    let (status, headers, body) = h.send("DELETE", "/boxes/b1/said", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert!(body["message"].is_string(), "{body}");
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(h.stored("boxes", "b1").await, None, "the delete stands");
    assert_eq!(
        capture.of("binBox"),
        vec![ExecLine {
            operation: "binBox".into(),
            exec: "delete".into(),
            exec_fallback: None,
            render_error: None,
        }]
    );
}

#[tokio::test]
async fn an_acknowledgement_s_boolean_members_say_the_write_succeeded() {
    let (_capture, _guard) = Capture::install();
    let world = json!({"operations": {"removeBox": {"kind": "delete", "type": "boxes"}}});
    let h = support::harness(
        ANSWERS,
        // The record says `deleted: false`, which the acknowledgement does not repeat.
        vec![json!({"id": "b1", "label": "red", "deleted": false})],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let (status, _, body) = h
        .send("POST", "/boxes", Some(json!({"label": "red"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h.send("DELETE", "/boxes/b1/gone", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"id": "b1", "deleted": true}));
}

#[tokio::test]
async fn an_acknowledgement_holds_the_record_s_members_one_envelope_member_down() {
    let (_capture, _guard) = Capture::install();
    let world = json!({"operations": {"trashBox": {"kind": "delete", "type": "boxes"}}});
    let h = a_box(world).await;
    let (status, headers, body) = h.send("DELETE", "/boxes/b1/wrapped", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"success": true, "data": {"id": "b1"}}));
    assert_eq!(h.stored("boxes", "b1").await, None, "the delete stands");
}

#[tokio::test]
async fn another_resource_in_an_acknowledgement_keeps_the_faker_s_values() {
    let (_capture, _guard) = Capture::install();
    let world = json!({"operations": {"shedBox": {"kind": "delete", "type": "boxes"}}});
    let h = a_box(world).await;
    let (status, headers, body) = h.send("DELETE", "/boxes/b1/owned", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["id"], json!("b1"), "{body}");
    // The owner is another record: none of the deleted box's members go in it.
    assert_ne!(body["owner"]["id"], json!("b1"), "{body}");
    assert_ne!(body["owner"]["label"], json!("red"), "{body}");
}

#[tokio::test]
async fn a_record_member_the_acknowledgement_refuses_leaves_the_faker_s_value() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"closeBox": {"kind": "delete", "type": "boxes"}}});
    let h = a_box(world).await;
    let (status, headers, body) = h.send("DELETE", "/boxes/b1/status", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"id": "b1", "label": "closed"}));
    assert!(
        capture
            .of("closeBox")
            .iter()
            .all(|l| l.exec_fallback.is_none()),
        "{:?}",
        capture.of("closeBox")
    );
}

#[tokio::test]
async fn an_acknowledgement_is_the_same_in_every_run() {
    let (_capture, _guard) = Capture::install();
    let mut bodies = Vec::new();
    for _ in 0..2 {
        let world = json!({"operations": {"binBox": {"kind": "delete", "type": "boxes"}}});
        let h = a_box(world).await;
        let (status, _, body) = h.send("DELETE", "/boxes/b1/said", None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        bodies.push(body);
    }
    assert_eq!(bodies[0], bodies[1]);
}

#[tokio::test]
async fn a_repeated_idempotency_key_replays_an_executed_delete() {
    let (capture, _guard) = Capture::install();
    let world =
        json!({"operations": {"deleteNote": {"idempotency": {"header": "Idempotency-Key"}}}});
    let h = support::harness(
        DELETES,
        vec![json!({"id": "n1", "text": "hi"})],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let (status, _, body) = h
        .send("POST", "/notes", Some(json!({"text": "hi"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let key = [("idempotency-key", "k-1")];
    let (first, _, _) = h.send("DELETE", "/notes/n1", None, &key).await;
    let (again, _, _) = h.send("DELETE", "/notes/n1", None, &key).await;
    assert_eq!(first, StatusCode::NO_CONTENT);
    assert_eq!(again, StatusCode::NO_CONTENT);
    // The repeat is the first answer replayed: it never reaches the executor.
    assert_eq!(
        capture.of("deleteNote").len(),
        1,
        "{:?}",
        capture.of("deleteNote")
    );
    assert_eq!(h.provider.requests().len(), 1);
}

/// Deals are numbered: their delete answers `{success, data: {id}}` with an integer id.
const DEALS: &str = r##"
openapi: 3.0.0
info: {title: Deals, version: "1"}
paths:
  /deals:
    post:
      operationId: createDeal
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Deal"}}}}}
  /deals/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
    get:
      operationId: getDeal
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Deal"}}}}, "404": {description: missing}}
    delete:
      operationId: deleteDeal
      responses:
        "200":
          description: deleted
          content: {application/json: {schema: {type: object, required: [success, data], properties: {success: {type: boolean}, data: {type: object, required: [id], properties: {id: {type: integer}}}}}}}
components:
  schemas:
    Deal:
      type: object
      required: [id, title]
      properties: {id: {type: integer}, title: {type: string}}
"##;

#[tokio::test]
async fn an_envelope_member_holds_the_record_s_id_as_the_record_holds_it() {
    let (_capture, _guard) = Capture::install();
    let h = support::harness(
        DEALS,
        vec![json!({"id": 7, "title": "big"})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/deals", Some(json!({"title": "big"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h.send("DELETE", "/deals/7", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"success": true, "data": {"id": 7}}));
}

/// Queues a message is published to (`routed`) and got from, oldest first, `count` at a
/// time, kept or removed by `ackmode`.
const QUEUES: &str = r##"
openapi: 3.0.0
info: {title: Queues, version: "1"}
paths:
  /queues:
    post:
      operationId: createQueue
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Queue"}}}}}
  /queues/{name}:
    parameters: [{name: name, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getQueue
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Queue"}}}}}
    delete:
      operationId: deleteQueue
      responses: {"204": {description: deleted}}
  /queues/{name}/publish:
    parameters: [{name: name, in: path, required: true, schema: {type: string}}]
    post:
      operationId: publish
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {payload: {type: string}, routing_key: {type: string}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {routed: {type: boolean}}}}}}
  /queues/{name}/get:
    parameters: [{name: name, in: path, required: true, schema: {type: string}}]
    post:
      operationId: getMessages
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                count: {type: integer}
                ackmode: {type: string, enum: [ack_requeue_true, ack_requeue_false]}
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Message"}}}}}
components:
  schemas:
    Queue:
      type: object
      required: [name]
      properties: {name: {type: string}, durable: {type: boolean}}
    Message:
      type: object
      properties: {payload: {type: string}, routing_key: {type: string}, redelivered: {type: boolean}}
"##;

#[tokio::test]
async fn a_queue_hands_out_its_messages_oldest_first_kept_or_removed() {
    let (capture, _guard) = Capture::install();
    let world = json!({"types": {"queues": {"identity": {"member": "name"}}}, "operations": {
        "publish": {"kind": "action", "type": "queues", "target": {"path_param": "name"},
                    "queue": {"effect": "append"}},
        "getMessages": {"kind": "action", "type": "queues", "target": {"path_param": "name"},
                        "queue": {"effect": "take", "count": "/count",
                                  "requeue": {"at": "/ackmode", "value": ["ack_requeue_true"]},
                                  "redelivered": "redelivered"}}
    }});
    let h = support::harness(
        QUEUES,
        vec![json!({"name": "audit", "durable": true})],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, false),
    );
    let (status, _, body) = h
        .send("POST", "/queues", Some(json!({"name": "audit"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for payload in ["one", "two"] {
        let sent = json!({"payload": payload, "routing_key": "audit"});
        let (status, headers, body) = h
            .send("POST", "/queues/audit/publish", Some(sent), &[])
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(source(&headers), "state");
        assert_eq!(body, json!({"routed": true}));
    }
    let get = |count: u64, ackmode: &str| {
        let body = json!({"count": count, "ackmode": ackmode});
        let h = &h;
        async move {
            let (status, _, body) = h.send("POST", "/queues/audit/get", Some(body), &[]).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body
        }
    };
    let message = |payload: &str, redelivered: bool| json!({"payload": payload, "routing_key": "audit", "redelivered": redelivered});
    // The oldest first, put back and then marked redelivered.
    assert_eq!(
        get(1, "ack_requeue_true").await,
        json!([message("one", false)])
    );
    assert_eq!(
        get(5, "ack_requeue_true").await,
        json!([message("one", true), message("two", false)])
    );
    // Taken off for good.
    assert_eq!(
        get(1, "ack_requeue_false").await,
        json!([message("one", true)])
    );
    assert_eq!(
        get(5, "ack_requeue_false").await,
        json!([message("two", true)])
    );
    assert_eq!(get(5, "ack_requeue_false").await, json!([]));
    assert_eq!(
        h.provider.requests().len(),
        1,
        "only the create asked the model"
    );
    let lines = capture.0.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .filter(|l| l.operation == "publish" || l.operation == "getMessages")
            .all(|l| l.exec == "action" && l.exec_fallback.is_none()),
        "{lines:?}"
    );
}

/// The world [`QUEUES`] is described with.
fn queues_world() -> serde_json::Value {
    json!({"types": {"queues": {"identity": {"member": "name"}}}, "operations": {
        "publish": {"kind": "action", "type": "queues", "target": {"path_param": "name"},
                    "queue": {"effect": "append"}},
        "getMessages": {"kind": "action", "type": "queues", "target": {"path_param": "name"},
                        "queue": {"effect": "take", "count": "/count",
                                  "requeue": {"at": "/ackmode", "value": ["ack_requeue_true"]},
                                  "redelivered": "redelivered"}}
    }})
}

/// [`QUEUES`] with the queue `audit` created.
async fn audit_queue(script: Vec<serde_json::Value>) -> support::DescribedHarness {
    let h = support::harness(QUEUES, script, &HarnessOptions::default(), |rt| {
        support::with_effective(rt, queues_world(), false)
    });
    let (status, _, body) = h
        .send("POST", "/queues", Some(json!({"name": "audit"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    h
}

/// The executor's reasons for `op`'s calls: `None` for one it answered.
fn reasons(capture: &Capture, op: &str) -> Vec<Option<String>> {
    capture
        .of(op)
        .into_iter()
        .map(|l| l.exec_fallback)
        .collect()
}

#[tokio::test]
async fn a_queue_action_on_an_unheld_target_or_past_its_cap_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = audit_queue(vec![
        json!({"name": "audit", "durable": true}),
        json!([]),
        json!({"routed": false}),
    ])
    .await;
    // No queue `ghost` is held.
    let get = json!({"count": 1, "ackmode": "ack_requeue_false"});
    let (status, _, body) = h.send("POST", "/queues/ghost/get", Some(get), &[]).await;
    assert!(status.is_success(), "{body}");
    assert_eq!(
        reasons(&capture, "getMessages"),
        [Some("not_held".to_owned())]
    );
    // A message that takes the queue past its bytes is generated, and not kept.
    let big = "x".repeat(600 * 1024);
    for _ in 0..2 {
        let sent = json!({"payload": big, "routing_key": "audit"});
        let (status, _, body) = h
            .send("POST", "/queues/audit/publish", Some(sent), &[])
            .await;
        assert!(status.is_success(), "{body}");
    }
    assert_eq!(
        reasons(&capture, "publish"),
        [None, Some("over_cap".to_owned())]
    );
    let get = json!({"count": 5, "ackmode": "ack_requeue_false"});
    let (_, _, body) = h.send("POST", "/queues/audit/get", Some(get), &[]).await;
    assert_eq!(body.as_array().map(Vec::len), Some(1), "one message kept");
}

#[tokio::test]
async fn a_deleted_queue_s_messages_go_with_it() {
    let (_capture, _guard) = Capture::install();
    let h = audit_queue(vec![
        json!({"name": "audit", "durable": true}),
        json!({"name": "audit", "durable": true}),
    ])
    .await;
    let sent = json!({"payload": "one", "routing_key": "audit"});
    let (status, _, body) = h
        .send("POST", "/queues/audit/publish", Some(sent), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = h.send("DELETE", "/queues/audit", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    // Made again under the same name, it holds nothing.
    let (status, _, body) = h
        .send("POST", "/queues", Some(json!({"name": "audit"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let get = json!({"count": 5, "ackmode": "ack_requeue_false"});
    let (status, _, body) = h.send("POST", "/queues/audit/get", Some(get), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));
}

/// Items with a lifecycle, acted on by actions answering the item (`archiveItem`),
/// nothing (`starItem`), an acknowledgement (`renameItem`), the item in an envelope
/// (`pinItem`; `boostItem`, whose envelope also requires a `receipt` the faker draws;
/// and `auditItem`, whose envelope also requires an `audit` object, another resource
/// the model writes), and two the description sets nothing for (`pingItem`, and
/// `nudgeItem` answering nothing).
const ACTIONS: &str = r##"
openapi: 3.0.0
info: {title: Actions, version: "1"}
paths:
  /items:
    post:
      operationId: createItem
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /items/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getItem
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}, "404": {description: missing}}
  /items/{id}/archive:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: archiveItem
      responses: {"200": {description: archived, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /items/{id}/star:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: starItem
      responses: {"204": {description: starred}}
  /items/{id}/rename:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: renameItem
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses:
        "200":
          description: renamed
          content:
            application/json:
              schema:
                type: object
                required: [ok, id]
                properties: {ok: {type: boolean}, id: {type: string}, rev: {type: integer}}
  /items/{id}/pin:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: pinItem
      requestBody: {content: {application/json: {schema: {type: object, properties: {rank: {}}}}}}
      responses:
        "200":
          description: pinned
          content:
            application/json:
              schema:
                type: object
                required: [item]
                properties: {item: {$ref: "#/components/schemas/Item"}}
  /items/{id}/boost:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: boostItem
      responses:
        "200":
          description: boosted
          content:
            application/json:
              schema:
                type: object
                required: [item, receipt]
                properties: {item: {$ref: "#/components/schemas/Item"}, receipt: {type: string}}
  /items/{id}/audit:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: auditItem
      responses:
        "200":
          description: audited
          content:
            application/json:
              schema:
                type: object
                required: [item, audit]
                properties:
                  item: {$ref: "#/components/schemas/Item"}
                  audit: {type: object, required: [id], properties: {id: {type: string}}}
  /items/{id}/ping:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: pingItem
      responses: {"200": {description: pinged, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /items/{id}/nudge:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: nudgeItem
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"204": {description: nudged}}
components:
  schemas:
    Item:
      type: object
      required: [id, name, status]
      properties:
        id: {type: string}
        name: {type: string}
        status: {type: string, enum: [ACTIVE, ARCHIVED]}
        starred: {type: boolean}
        pinned: {type: boolean}
        rank: {type: integer}
"##;

/// The description of [`ACTIONS`]: what each action sets.
fn actions_world() -> serde_json::Value {
    let target = json!({"path_param": "id"});
    json!({
        "types": {"items": {"lifecycle": {
            "member": "status", "states": ["ACTIVE", "ARCHIVED"], "initial": "ACTIVE"
        }}},
        "operations": {
            "archiveItem": {"kind": "action", "target": target, "transition": {"status": "ARCHIVED"}},
            "starItem": {"kind": "action", "target": target, "changes": {"starred": {"value": true}}},
            "renameItem": {"kind": "action", "target": target, "changes": {"name": {"from_request": "/name"}}},
            "pinItem": {
                "kind": "action", "target": target, "answer": {"record": "/item"},
                "changes": {"pinned": {"value": true}, "rank": {"from_request": "/rank"}}
            },
            "boostItem": {
                "kind": "action", "target": target, "answer": {"record": "/item"},
                "changes": {"pinned": {"value": true}}
            },
            "auditItem": {
                "kind": "action", "target": target, "answer": {"record": "/item"},
                "changes": {"pinned": {"value": true}}
            },
            "pingItem": {"kind": "action", "target": target}
        }
    })
}

/// A harness of [`ACTIONS`] whose LLM answers `script`.
fn actions(script: Vec<serde_json::Value>) -> support::DescribedHarness {
    support::harness(ACTIONS, script, &HarnessOptions::default(), |rt| {
        support::with_effective(rt, actions_world(), false)
    })
}

fn item_i1() -> serde_json::Value {
    json!({"id": "i1", "name": "a", "status": "ACTIVE"})
}

/// The line the executor logs for an action `operation`.
fn action_line(operation: &str, fallback: Option<&str>) -> ExecLine {
    ExecLine {
        operation: operation.into(),
        exec: "action".into(),
        exec_fallback: fallback.map(str::to_owned),
        render_error: None,
    }
}

/// Widgets answer their create as their record; docs acknowledge theirs with
/// `{ok, id, rev}`; boards answer only a `Location`; gizmos answer a record their read
/// does not hold (`name` an integer); reports are accepted (`202`) as an operation
/// holding them; batches are created in bulk.
const CREATES: &str = r##"
openapi: 3.0.0
info: {title: Creates, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody: {content: {application/json: {schema: {type: object, properties: {id: {type: string}, name: {type: string}, size: {type: integer}, color: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}}
  /widgets/{id}:
    get:
      operationId: getWidget
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}, "404": {description: missing}}
  /docs:
    post:
      operationId: createDoc
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}, body: {type: string}}}}}}
      responses:
        "201":
          description: written
          content:
            application/json:
              schema:
                type: object
                required: [ok, id, rev]
                properties: {ok: {type: boolean}, id: {type: string}, rev: {type: string, pattern: "^[0-9]+-[0-9a-f]{8}$"}}
  /docs/{id}:
    get:
      operationId: getDoc
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}}
  /boards:
    post:
      operationId: createBoard
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, headers: {Location: {schema: {type: string}}}}}
  /boards/{boardId}:
    get:
      operationId: getBoard
      parameters: [{name: boardId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}}
  /gizmos:
    post:
      operationId: createGizmo
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses:
        "201":
          description: created
          content: {application/json: {schema: {type: object, required: [id, name], properties: {id: {type: string}, name: {type: integer}}}}}
  /gizmos/{id}:
    get:
      operationId: getGizmo
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Gizmo"}}}}}
  /reports:
    post:
      operationId: createReport
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"202": {description: accepted, content: {application/json: {schema: {$ref: "#/components/schemas/ReportJob"}}}}}
  /reports/{id}:
    get:
      operationId: getReport
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Report"}}}}}
  /batches:
    post:
      operationId: createBatch
      requestBody: {content: {application/json: {schema: {type: object, properties: {items: {type: array, items: {type: object}}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {type: object, properties: {items: {type: array, items: {$ref: "#/components/schemas/Widget"}}}}}}}}
components:
  schemas:
    Widget:
      type: object
      required: [id, name, size]
      properties: {id: {type: string}, name: {type: string}, size: {type: integer}, color: {type: string}}
    Doc:
      type: object
      required: [id, title, body]
      properties: {id: {type: string}, title: {type: string}, body: {type: string}}
    Board:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Gizmo:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Report:
      type: object
      required: [id, title]
      properties: {id: {type: string}, title: {type: string}}
    ReportJob:
      type: object
      required: [name, done]
      properties: {name: {type: string}, done: {type: boolean}, response: {$ref: "#/components/schemas/Report"}}
"##;

fn create_line(op: &str, fallback: Option<&str>) -> ExecLine {
    ExecLine {
        operation: op.into(),
        exec: "create".into(),
        exec_fallback: fallback.map(str::to_owned),
        render_error: None,
    }
}

/// Creates item `i1` (the LLM's first answer), seeded: the create takes the generated
/// path, which stores the record under the id the model chose.
async fn create_i1(h: &support::DescribedHarness) {
    let (status, _, body) = h
        .send("POST", "/items", Some(json!({"name": "a"})), SEEDED)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_transition_is_applied_to_the_held_target_with_no_llm_call() {
    let (capture, _guard) = Capture::install();
    // A second answer a reconciling path could take for the action: never used.
    let h = actions(vec![
        item_i1(),
        json!({"id": "i1", "name": "decoy", "status": "ACTIVE", "rank": 9}),
    ]);
    assert_eq!(
        type_of(&h, "getItem"),
        "items",
        "the world above names this type"
    );
    create_i1(&h).await;
    let (status, headers, body) = h.send("POST", "/items/i1/archive", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let archived = json!({"id": "i1", "name": "a", "status": "ARCHIVED"});
    assert_eq!(body, archived);
    assert_eq!(
        h.provider.requests().len(),
        1,
        "the action made no LLM call"
    );
    assert_eq!(h.stored("items", "i1").await, Some(archived.clone()));
    assert_eq!(
        capture.of("archiveItem"),
        vec![action_line("archiveItem", None)]
    );
    let (status, headers, body) = h.send("GET", "/items/i1", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, archived);
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn a_bodiless_action_answers_its_declared_status_and_stores_the_change() {
    let (capture, _guard) = Capture::install();
    let h = actions(vec![item_i1(), json!({"effects": []})]);
    create_i1(&h).await;
    let (status, headers, body) = h.send("POST", "/items/i1/star", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(source(&headers), "state");
    assert_eq!(body, serde_json::Value::Null);
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(
        h.stored("items", "i1").await,
        Some(json!({"id": "i1", "name": "a", "status": "ACTIVE", "starred": true}))
    );
    assert_eq!(capture.of("starItem"), vec![action_line("starItem", None)]);
}

#[tokio::test]
async fn an_acknowledging_action_answers_its_own_schema_with_the_records_values() {
    let (capture, _guard) = Capture::install();
    let h = actions(vec![
        item_i1(),
        json!({"ok": false, "id": "decoy", "rev": 1}),
    ]);
    create_i1(&h).await;
    let (status, headers, body) = h
        .send("POST", "/items/i1/rename", Some(json!({"name": "b"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["id"], json!("i1"), "{body}");
    assert!(body["ok"].is_boolean(), "{body}");
    assert!(
        body.get("name").is_none(),
        "only the answer's own members: {body}"
    );
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(
        h.stored("items", "i1").await,
        Some(json!({"id": "i1", "name": "b", "status": "ACTIVE"}))
    );
    assert_eq!(
        capture.of("renameItem"),
        vec![action_line("renameItem", None)]
    );
}

#[tokio::test]
async fn an_action_answering_its_record_in_an_envelope_answers_it_there() {
    let (_capture, _guard) = Capture::install();
    let h = actions(vec![item_i1(), json!({"item": item_i1()})]);
    create_i1(&h).await;
    // `rank` does not fit its member: dropped, and the rest still set.
    let (status, headers, body) = h
        .send("POST", "/items/i1/pin", Some(json!({"rank": "high"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let pinned = json!({"id": "i1", "name": "a", "status": "ACTIVE", "pinned": true});
    assert_eq!(body, json!({"item": pinned}));
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(h.stored("items", "i1").await, Some(pinned));
}

#[tokio::test]
async fn an_action_the_description_sets_nothing_for_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = actions(vec![item_i1(), item_i1()]);
    create_i1(&h).await;
    let (status, headers, body) = h.send("POST", "/items/i1/ping", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "llm", "{body}");
    assert_eq!(
        capture.of("pingItem"),
        vec![action_line("pingItem", Some("undescribed"))]
    );
}

#[tokio::test]
async fn an_action_on_a_target_behind_remembered_writes_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = actions(vec![
        item_i1(),
        json!({"effects": []}),
        json!({"id": "i1", "name": "renamed", "status": "ARCHIVED"}),
    ]);
    create_i1(&h).await;
    // An action answering nothing: write memory keeps its request for the item, which
    // the stored copy does not show.
    let (status, _, body) = h
        .send(
            "POST",
            "/items/i1/nudge",
            Some(json!({"name": "renamed"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, headers, body) = h.send("POST", "/items/i1/archive", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(source(&headers), "state", "{body}");
    assert_eq!(
        capture.of("archiveItem"),
        vec![action_line("archiveItem", Some("behind"))]
    );
    // The earlier write is not lost: the read after shows it.
    let (status, _, body) = h.send("GET", "/items/i1", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], json!("renamed"), "{body}");
    assert_eq!(body["status"], json!("ARCHIVED"), "{body}");
}

#[tokio::test]
async fn an_action_on_an_unheld_target_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = actions(vec![json!({"id": "i9", "name": "x", "status": "ARCHIVED"})]);
    let (status, headers, body) = h.send("POST", "/items/i9/archive", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "llm", "{body}");
    assert_eq!(
        capture.of("archiveItem"),
        vec![action_line("archiveItem", Some("not_held"))]
    );
}

/// An action answering its record in an envelope that also requires a member nothing
/// the mock holds gives (Cal.com's `{status, data}`) fills it as a read of the record
/// does, from the faker, and is answered from state.
#[tokio::test]
async fn an_action_s_envelope_is_filled_as_a_read_s_is() {
    let (capture, _guard) = Capture::install();
    // `boostItem` answers the item in an envelope that also requires a `receipt`.
    let h = actions(vec![item_i1()]);
    create_i1(&h).await;
    let (status, headers, body) = h.send("POST", "/items/i1/boost", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let boosted = json!({"id": "i1", "name": "a", "status": "ACTIVE", "pinned": true});
    assert_eq!(body["item"], boosted, "{body}");
    assert!(body["receipt"].is_string(), "{body}");
    assert_eq!(
        capture.of("boostItem"),
        vec![action_line("boostItem", None)]
    );
    assert_eq!(
        h.provider.requests().len(),
        1,
        "the action made no LLM call"
    );
    assert_eq!(h.stored("items", "i1").await, Some(boosted));
}

/// An envelope member beside the record that is an object (Zendesk's `audit`) is
/// another resource the write made, not metadata the faker draws: the action falls
/// back and the model answers it.
#[tokio::test]
async fn an_action_whose_envelope_requires_an_object_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = actions(vec![
        item_i1(),
        json!({"item": {"id": "i1", "name": "a", "status": "ACTIVE", "pinned": true}, "audit": {"id": "a1"}}),
    ]);
    create_i1(&h).await;
    let (status, headers, body) = h.send("POST", "/items/i1/audit", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "llm", "{body}");
    assert_eq!(
        capture.of("auditItem"),
        vec![action_line("auditItem", Some("render_failed"))]
    );
}

#[tokio::test]
async fn with_no_llm_answer_an_action_still_changes_state() {
    let (capture, _guard) = Capture::install();
    // No answers: the executor creates the item, the faker filling what it needs.
    let h = actions(vec![]);
    let (status, _, made) = h
        .send("POST", "/items", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let id = made["id"].as_str().unwrap().to_owned();
    let (status, headers, body) = h
        .send("POST", &format!("/items/{id}/archive"), None, &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The executed create leaves nothing for the LLM to catch up on: the action is
    // executed too.
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(
        capture.of("archiveItem"),
        vec![action_line("archiveItem", None)]
    );
    assert_eq!(body["status"], json!("ARCHIVED"));
    assert_eq!(body["id"], json!(id));
    assert_eq!(h.stored("items", &id).await, Some(body));
}

/// A described action on GraphQL: a mutation setting a member of its target is executed.
#[cfg(feature = "graphql")]
mod graphql {
    use std::sync::Arc;
    use std::time::Duration;

    use mb_engine::{Engine, EngineConfig};
    use mb_llm::{Provider, ScriptedProvider};
    use mb_protocols::MockRuntime;
    use mb_state::MemoryStore;
    use serde_json::{Value, json};

    use super::{Capture, ExecLine};

    const SDL: &str = r#"
type Widget { id: ID! name: String! starred: Boolean }
type Query { widget(id: ID!): Widget }
type Mutation {
  createWidget(name: String!): Widget
  starWidget(id: ID!): Widget
}
"#;

    async fn post(router: &axum::Router, query: &str) -> Value {
        post_with(router, query, &[]).await
    }

    /// [`post`] of a create, seeded (see [`super::SEEDED`]).
    async fn seeded(router: &axum::Router, query: &str) -> Value {
        post_with(router, query, super::SEEDED).await
    }

    async fn post_with(router: &axum::Router, query: &str, headers: &[(&str, &str)]) -> Value {
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json");
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let req = req
            .body(axum::body::Body::from(json!({"query": query}).to_string()))
            .unwrap();
        super::support::send_to(router, req).await.2
    }

    /// Linear's noun-first create (#353's tier 0): its payload holds the comment.
    const COMMENTS: &str = r#"
type Comment { id: ID! body: String! }
type CommentPayload { success: Boolean! comment: Comment }
input CommentCreateInput { body: String! }
type Query { comment(id: ID!): Comment }
type Mutation { commentCreate(input: CommentCreateInput!): CommentPayload! }
"#;

    #[tokio::test]
    async fn a_noun_first_create_mutation_is_executed_and_read_from_state() {
        let (capture, _guard) = Capture::install();
        let bundle = mb_import::compile(COMMENTS.as_bytes()).unwrap();
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let rt = MockRuntime::new(
            "described",
            Arc::new(bundle),
            Arc::new(Engine::new(
                Some(Arc::clone(&provider) as Arc<dyn Provider>),
                EngineConfig::default(),
            )),
            Arc::new(MemoryStore::new(Duration::from_secs(3600))),
        );
        let base = mb_mcm::world::tier0(rt.bundle());
        let rt = rt.with_world(&base, &base, 0);
        let router = mb_protocols::graphql::try_router(Arc::new(rt)).unwrap();
        let made = post(
            &router,
            "mutation { commentCreate(input: {body: \"hi\"}) { success comment { id body } } }",
        )
        .await;
        assert!(made.get("errors").is_none(), "{made}");
        let payload = &made["data"]["commentCreate"];
        assert_eq!(payload["success"], json!(true), "{made}");
        assert_eq!(payload["comment"]["body"], json!("hi"), "{made}");
        let id = payload["comment"]["id"].as_str().unwrap().to_owned();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"query": format!("{{ comment(id: \"{id}\") {{ id body }} }}")}).to_string(),
            ))
            .unwrap();
        let (_, headers, read) = super::support::send_to(&router, req).await;
        assert_eq!(
            read["data"]["comment"],
            json!({"id": id, "body": "hi"}),
            "{read}"
        );
        assert_eq!(super::source(&headers), "state", "{read}");
        assert!(provider.requests().is_empty(), "no LLM call");
        assert_eq!(
            capture.of("Mutation.commentCreate"),
            vec![ExecLine {
                operation: "Mutation.commentCreate".into(),
                exec: "create".into(),
                exec_fallback: None,
                render_error: None,
            }]
        );
    }

    #[tokio::test]
    async fn a_graphql_action_mutation_is_executed() {
        let (capture, _guard) = Capture::install();
        let bundle = mb_import::compile(SDL.as_bytes()).unwrap();
        let t = bundle
            .operation("Query.widget")
            .and_then(|o| o.resource_type.clone())
            .unwrap();
        let world = json!({"operations": {"Mutation.starWidget": {
            "kind": "action", "type": t, "target": {"member": "id"},
            "changes": {"starred": {"value": true}}
        }}});
        let provider = Arc::new(ScriptedProvider::new(
            [
                json!({"__typename": "Widget", "id": "w1", "name": "a", "starred": false}),
                json!({"__typename": "Widget", "id": "w1", "name": "decoy", "starred": false}),
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
        let made = seeded(
            &router,
            "mutation { createWidget(name: \"a\") { id name starred } }",
        )
        .await;
        assert!(made.get("errors").is_none(), "{made}");
        let starred = post(
            &router,
            "mutation { starWidget(id: \"w1\") { id name starred } }",
        )
        .await;
        assert!(starred.get("errors").is_none(), "{starred}");
        assert_eq!(
            starred["data"]["starWidget"],
            json!({"id": "w1", "name": "a", "starred": true})
        );
        assert_eq!(
            provider.requests().len(),
            1,
            "the mutation made no LLM call"
        );
        assert!(
            capture.of("Mutation.starWidget").contains(&ExecLine {
                operation: "Mutation.starWidget".into(),
                exec: "action".into(),
                exec_fallback: None,
                render_error: None,
            }),
            "{:?}",
            capture.of("Mutation.starWidget")
        );
        let read = post(&router, "{ widget(id: \"w1\") { id name starred } }").await;
        assert_eq!(read["data"]["widget"]["starred"], json!(true), "{read}");
    }
}

/// An RPC-style API: every call a `POST` to `<type>.<verb>`, the record named by a body
/// member and answered in an `{ok, <record>}` envelope.
const RPC: &str = r##"
openapi: 3.0.0
info: {title: Rpc, version: "1"}
paths:
  /api/widgets.create:
    post:
      operationId: widgetsCreate
      requestBody: {content: {application/json: {schema: {type: object, required: [name], properties: {name: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/WidgetAnswer"}}}}}
  /api/widgets.info:
    post:
      operationId: widgetsInfo
      requestBody: {content: {application/json: {schema: {type: object, required: [widget], properties: {widget: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/WidgetAnswer"}}}}}
  /api/widgets.list:
    post:
      operationId: widgetsList
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {ok: {type: boolean}, widgets: {type: array, items: {$ref: "#/components/schemas/Widget"}}}}}}}}
components:
  schemas:
    Widget:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    WidgetAnswer:
      type: object
      required: [widget]
      properties: {ok: {type: boolean}, widget: {$ref: "#/components/schemas/Widget"}}
"##;

/// Tier 0 classifies the path-less read as a read of one widget, its target the body
/// member named for the type, so the executor answers it from the store.
#[tokio::test]
async fn a_path_less_rpc_read_names_its_target_by_a_body_member() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        RPC,
        vec![json!({"ok": true, "widget": {"id": "W1", "name": "a"}})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/api/widgets.create",
            Some(json!({"name": "a"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["widget"]["id"].as_str().unwrap().to_owned();
    let (status, headers, body) = h
        .send(
            "POST",
            "/api/widgets.info",
            Some(json!({"widget": id})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["widget"], json!({"id": id, "name": "a"}));
    assert_eq!(
        capture.of("widgetsInfo"),
        vec![ExecLine {
            operation: "widgetsInfo".into(),
            exec: "read".into(),
            exec_fallback: None,
            render_error: None,
        }]
    );
    assert!(
        h.provider.requests().is_empty(),
        "neither the executed create nor the read made an LLM call"
    );
    assert_eq!(
        capture.of("widgetsCreate"),
        vec![create_line("widgetsCreate", None)]
    );
}

#[tokio::test]
async fn a_create_stores_and_answers_the_request_s_record_with_a_minted_id() {
    let (capture, _guard) = Capture::install();
    // The fill's answer: a value for what the request left out, required (`size`) or
    // not (`color`); an id and a name it never takes.
    let h = support::harness(
        CREATES,
        vec![json!({"id": "w-model", "size": 3, "color": "teal", "name": "not sent"})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, headers, body) = h
        .send("POST", "/widgets", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_ne!(id, "w-model", "the id is minted, never filled");
    assert_eq!(
        body,
        json!({"id": id, "name": "a", "size": 3, "color": "teal"}),
        "the request's values stand; fill writes only what it left out"
    );
    assert_eq!(
        capture.of("createWidget"),
        vec![create_line("createWidget", None)]
    );
    let t = type_of(&h, "getWidget");
    assert_eq!(
        h.stored(&t, &id).await,
        Some(body.clone()),
        "stored is answered"
    );
    // One LLM call (the fill), whose prompt shows the record so far.
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 1);
    let prompt = &requests[0].messages[1].content;
    assert!(prompt.contains("<record>"), "{prompt}");
    // A later read is the executor's, from state.
    let (status, headers, read) = h.send("GET", &format!("/widgets/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state");
    assert_eq!(read, body);
    assert_eq!(capture.of("getWidget"), vec![exec(None)]);
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn a_create_naming_every_member_makes_no_llm_call() {
    let (_capture, _guard) = Capture::install();
    let h = support::harness(CREATES, vec![], &HarnessOptions::default(), |rt| rt);
    let sent = json!({"name": "a", "size": 2, "color": "red"});
    let (status, _, body) = h.send("POST", "/widgets", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["size"], json!(2));
    assert!(h.provider.requests().is_empty());
}

/// Intents show their id's shape only in an example; notes show none.
const SHAPED_IDS: &str = r##"
openapi: 3.1.0
info: {title: Shaped, version: "1"}
paths:
  /intents:
    post:
      operationId: createIntent
      requestBody: {content: {application/json: {schema: {type: object, properties: {amount: {type: integer}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}}
  /intents/{intent}:
    get:
      operationId: getIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}}
  /notes:
    post:
      operationId: createNote
      requestBody: {content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
  /notes/{id}:
    get:
      operationId: getNote
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
components:
  schemas:
    Intent:
      type: object
      required: [id, amount]
      properties:
        id: {type: string, maxLength: 5000, examples: [pi_3MtwBwLkdIwHu7ix28a3tqPa]}
        amount: {type: integer}
    Note:
      type: object
      required: [id, text]
      properties: {id: {type: string}, text: {type: string}}
"##;

/// A minted id takes the shape the contract's example of it shows (its prefix, then
/// as many letters and digits), each its own; with no example it is drawn as before.
#[tokio::test]
async fn a_minted_id_takes_the_shape_of_the_contract_s_example() {
    let (_capture, _guard) = Capture::install();
    let h = support::harness(SHAPED_IDS, vec![], &HarnessOptions::default(), |rt| rt);
    let mut ids = Vec::new();
    for amount in 0..20 {
        let (status, headers, body) = h
            .send("POST", "/intents", Some(json!({"amount": amount})), &[])
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(source(&headers), "state", "{body}");
        let id = body["id"].as_str().unwrap().to_owned();
        let rest = id.strip_prefix("pi_").unwrap_or_default();
        assert!(
            rest.len() == 24 && rest.bytes().all(|b| b.is_ascii_alphanumeric()),
            "{id}"
        );
        let (status, _, read) = h.send("GET", &format!("/intents/{id}"), None, &[]).await;
        assert_eq!((status, read), (StatusCode::OK, body));
        ids.push(id);
    }
    let mut distinct = ids.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), ids.len(), "{ids:?}");
    let (status, _, note) = h
        .send("POST", "/notes", Some(json!({"text": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{note}");
    let id = note["id"].as_str().unwrap();
    assert!(!id.is_empty() && !id.contains('_'), "{id}");
}

/// A 3.0 contract shows its id's shape in the schema's singular `example`.
const SHAPED_IDS_30: &str = r##"
openapi: 3.0.3
info: {title: Shaped, version: "1"}
paths:
  /payment_intents:
    post:
      operationId: createPaymentIntent
      requestBody: {content: {application/json: {schema: {type: object, properties: {amount: {type: integer}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/PaymentIntent"}}}}}
  /payment_intents/{intent}:
    get:
      operationId: getPaymentIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/PaymentIntent"}}}}}
components:
  schemas:
    PaymentIntent:
      type: object
      required: [id, amount]
      properties:
        id: {type: string, maxLength: 5000, example: pi_3MtwBwLkdIwHu7ix28a3tqPa}
        amount: {type: integer}
"##;

/// A 3.0 schema's `example` shapes a minted id as `examples` does.
#[tokio::test]
async fn a_minted_id_takes_the_shape_of_a_30_schema_s_example() {
    let (_capture, _guard) = Capture::install();
    let h = support::harness(SHAPED_IDS_30, vec![], &HarnessOptions::default(), |rt| rt);
    let (status, headers, body) = h
        .send("POST", "/payment_intents", Some(json!({"amount": 1})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = body["id"].as_str().unwrap();
    let rest = id.strip_prefix("pi_").unwrap_or_default();
    assert!(
        rest.len() == 24 && rest.bytes().all(|b| b.is_ascii_alphanumeric()),
        "{id}"
    );
}

/// With no example, a minted id takes the shape of the ids the session's records hold
/// already: the model's, here.
#[tokio::test]
async fn a_minted_id_takes_the_shape_of_the_session_s_ids() {
    let (_capture, _guard) = Capture::install();
    let model = json!({"id": "nt_Ab12Cd34Ef56Gh78", "text": "a"});
    let h = support::harness(
        SHAPED_IDS,
        vec![model.clone()],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/notes", Some(json!({"text": "a"})), SEEDED)
        .await;
    assert_eq!((status, body), (StatusCode::CREATED, model));
    let (status, headers, note) = h
        .send("POST", "/notes", Some(json!({"text": "b"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{note}");
    assert_eq!(source(&headers), "state", "{note}");
    let id = note["id"].as_str().unwrap();
    let rest = id.strip_prefix("nt_").unwrap_or_default();
    assert!(
        rest.len() == 16 && rest.bytes().all(|b| b.is_ascii_alphanumeric()),
        "{id}"
    );
}

#[tokio::test]
async fn two_creates_mint_two_ids_and_a_client_chosen_id_is_kept() {
    let (_capture, _guard) = Capture::install();
    let h = support::harness(CREATES, vec![], &HarnessOptions::default(), |rt| rt);
    let sent = json!({"name": "a", "size": 2, "color": "red"});
    let (_, _, first) = h.send("POST", "/widgets", Some(sent.clone()), &[]).await;
    let (_, _, second) = h.send("POST", "/widgets", Some(sent), &[]).await;
    assert_ne!(first["id"], second["id"]);
    let chosen = json!({"id": "mine", "name": "b", "size": 1, "color": "red"});
    let (status, _, body) = h.send("POST", "/widgets", Some(chosen.clone()), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body, chosen);
    assert_eq!(
        h.stored(&type_of(&h, "getWidget"), "mine").await,
        Some(chosen)
    );
}

#[tokio::test]
async fn with_no_llm_answer_the_faker_fills_and_state_still_holds() {
    let (capture, _guard) = Capture::install();
    // The provider fails every call: fill falls back to the faker.
    let h = support::harness(CREATES, vec![], &HarnessOptions::default(), |rt| rt);
    let (status, headers, body) = h
        .send("POST", "/widgets", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["name"], json!("a"));
    assert!(body["size"].is_i64(), "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (_, _, read) = h.send("GET", &format!("/widgets/{id}"), None, &[]).await;
    assert_eq!(read, body);
    assert_eq!(
        capture.of("createWidget"),
        vec![create_line("createWidget", None)]
    );
}

#[tokio::test]
async fn an_acknowledgement_is_drawn_from_its_schema_with_the_record_s_id() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        CREATES,
        vec![json!({"body": "filled"})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, headers, ack) = h
        .send("POST", "/docs", Some(json!({"title": "t"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    assert_eq!(source(&headers), "state", "{ack}");
    assert_eq!(ack["ok"], json!(true), "the write succeeded: {ack}");
    assert!(ack["rev"].is_string(), "{ack}");
    let id = ack["id"].as_str().unwrap().to_owned();
    assert_eq!(
        capture.of("createDoc"),
        vec![create_line("createDoc", None)]
    );
    let stored = h.stored(&type_of(&h, "getDoc"), &id).await;
    assert_eq!(
        stored,
        Some(json!({"id": id, "title": "t", "body": "filled"})),
        "the record, not the acknowledgement, is stored"
    );
    let (status, headers, read) = h.send("GET", &format!("/docs/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state");
    assert_eq!(Some(read), stored);
}

#[tokio::test]
async fn a_location_create_names_the_one_id_it_stores_under() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(CREATES, vec![], &HarnessOptions::default(), |rt| rt);
    let (status, headers, body) = h
        .send("POST", "/boards", Some(json!({"name": "plan"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let location = headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let id = location.rsplit('/').next().unwrap().to_owned();
    assert_eq!(
        capture.of("createBoard"),
        vec![create_line("createBoard", None)]
    );
    assert_eq!(
        h.stored(&type_of(&h, "getBoard"), &id).await,
        Some(json!({"id": id, "name": "plan"}))
    );
    let (status, headers, read) = h.send("GET", &format!("/boards/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state");
    assert_eq!(read, json!({"id": id, "name": "plan"}));
}

#[tokio::test]
async fn a_created_record_starts_in_its_initial_state_which_fill_never_writes() {
    let (_capture, _guard) = Capture::install();
    let world = json!({"types": {"folders": {"lifecycle": {
        "member": "state", "states": ["ACTIVE", "DELETED"], "initial": "ACTIVE"
    }}}});
    let h = support::harness(DELETES, vec![], &HarnessOptions::default(), |rt| {
        support::with_effective(rt, world, false)
    });
    let (status, _, body) = h
        .send("POST", "/folders", Some(json!({"name": "docs"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], json!("ACTIVE"));
    assert!(h.provider.requests().is_empty(), "nothing was left to fill");
    let id = body["id"].as_str().unwrap();
    assert_eq!(h.stored("folders", id).await, Some(body.clone()));
}

#[tokio::test]
async fn a_held_id_falls_back() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(CREATES, vec![], &HarnessOptions::default(), |rt| rt);
    let chosen = json!({"id": "mine", "name": "b", "size": 1, "color": "red"});
    h.send("POST", "/widgets", Some(chosen.clone()), &[]).await;
    h.send("POST", "/widgets", Some(chosen), &[]).await;
    assert_eq!(
        capture.of("createWidget"),
        vec![
            create_line("createWidget", None),
            create_line("createWidget", Some("held"))
        ]
    );
}

#[tokio::test]
async fn an_answer_the_record_does_not_fit_is_the_faker_s_with_the_record_s_id() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(CREATES, vec![], &HarnessOptions::default(), |rt| rt);
    let (status, headers, body) = h
        .send("POST", "/gizmos", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    // The answer's `name` is an integer: the faker's, beside the record's id.
    assert!(body["name"].is_i64(), "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        h.stored(&type_of(&h, "getGizmo"), &id).await,
        Some(json!({"id": id, "name": "a"})),
        "the record is stored as sent"
    );
    assert_eq!(
        capture.of("createGizmo"),
        vec![create_line("createGizmo", None)]
    );
}

#[tokio::test]
async fn a_bulk_create_falls_back() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"createBatch": {
        "kind": "create", "type": "widgets",
        "request": {"records": "/items"}, "answer": {"records": "/items"}
    }}});
    let h = support::harness(
        CREATES,
        vec![json!({"items": [{"id": "w1", "name": "a", "size": 1}]})],
        &HarnessOptions::default(),
        |rt| support::with_effective(rt, world, true),
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/batches",
            Some(json!({"items": [{"name": "a", "size": 1}]})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createBatch"),
        vec![create_line("createBatch", Some("bulk"))]
    );
}

#[tokio::test]
async fn an_accepted_create_is_stored_at_once_and_its_operation_reads_as_done() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"createReport": {
        "answer": {"long_running": {"done": "done", "result": "response"}}
    }}});
    let h = support::harness(CREATES, vec![], &HarnessOptions::default(), |rt| {
        support::with_effective(rt, world, false)
    });
    let (status, headers, body) = h
        .send("POST", "/reports", Some(json!({"title": "q3"})), &[])
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["done"], json!(true), "{body}");
    let id = body["response"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["response"], json!({"id": id, "title": "q3"}));
    assert_eq!(
        capture.of("createReport"),
        vec![create_line("createReport", None)]
    );
    let (status, headers, read) = h.send("GET", &format!("/reports/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read, json!({"id": id, "title": "q3"}));
    assert!(h.provider.requests().is_empty());
}

/// Sites are listed by status and offset pages, read, updated and deleted; racks are
/// created and listed under their site; things answer their create with fewer members
/// than their read and their list require.
const JOURNEY: &str = r##"
openapi: 3.0.0
info: {title: Journey, version: "1"}
paths:
  /sites:
    get:
      operationId: listSites
      parameters:
        - {name: limit, in: query, schema: {type: integer, minimum: 1, default: 50}}
        - {name: offset, in: query, schema: {type: integer, minimum: 0, default: 0}}
        - {name: status, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [count, next, previous, results]
                properties:
                  count: {type: integer}
                  next: {type: string, format: uri, nullable: true}
                  previous: {type: string, format: uri, nullable: true}
                  results: {type: array, items: {$ref: "#/components/schemas/Site"}}
    post:
      operationId: createSite
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}, status: {type: string}, region: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Site"}}}}}
  /sites/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getSite
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Site"}}}}
        "404": {description: missing, content: {application/json: {schema: {type: object, properties: {message: {type: string}}}}}}
    patch:
      operationId: updateSite
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}, status: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Site"}}}}}
    delete:
      operationId: deleteSite
      responses: {"204": {description: deleted}}
  /sites/{siteId}/racks:
    parameters: [{name: siteId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listRacks
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: object, required: [racks], properties: {racks: {type: array, items: {$ref: "#/components/schemas/Rack"}}}}}}
    post:
      operationId: createRack
      requestBody: {content: {application/json: {schema: {type: object, properties: {label: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Rack"}}}}}
  /sites/{siteId}/racks/{rackId}:
    parameters:
      - {name: siteId, in: path, required: true, schema: {type: string}}
      - {name: rackId, in: path, required: true, schema: {type: string}}
    get:
      operationId: getRack
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Rack"}}}}}
  /things:
    get:
      operationId: listThings
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/ThingSummary"}}}}}}
    post:
      operationId: createThing
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/NewThing"}}}}}
  /things/{id}:
    get:
      operationId: getThing
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
components:
  schemas:
    Site:
      type: object
      required: [id, name, status]
      properties:
        id: {type: string}
        name: {type: string}
        status: {type: string, enum: [active, planned, retired]}
        region: {type: string}
    Rack:
      type: object
      required: [id, label, site]
      properties: {id: {type: string}, label: {type: string}, site: {type: string}}
    NewThing:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Thing:
      type: object
      required: [id, name, size]
      properties: {id: {type: string}, name: {type: string}, size: {type: integer}}
    ThingSummary:
      type: object
      required: [id, color]
      properties: {id: {type: string}, color: {type: string}}
"##;

fn journey_world() -> serde_json::Value {
    json!({
        "types": {"racks": {"parent": {"type": "sites", "member": "site", "path_param": "siteId"}}},
        "operations": {"listSites": {
            "filters": [{"param": "status", "member": "status", "kind": "eq"}],
            "paging": {"style": "offset", "request": "offset", "size": "limit"}
        }}
    })
}

fn line(op: &str, kind: &str, fallback: Option<&str>) -> ExecLine {
    ExecLine {
        operation: op.into(),
        exec: kind.into(),
        exec_fallback: fallback.map(str::to_owned),
        render_error: None,
    }
}

/// The `id`s of `items`.
fn ids(items: &serde_json::Value) -> Vec<serde_json::Value> {
    items
        .as_array()
        .map(|items| items.iter().map(|i| i["id"].clone()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_created_record_is_read_listed_updated_and_deleted_by_its_minted_id() {
    let (capture, _guard) = Capture::install();
    let h = support::described(JOURNEY, journey_world(), Vec::new());
    let mut created = Vec::new();
    for (name, status) in [("a", "active"), ("b", "planned"), ("c", "active")] {
        let sent = json!({"name": name, "status": status, "region": "eu"});
        let (status, headers, body) = h.send("POST", "/sites", Some(sent), &[]).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(source(&headers), "state", "{body}");
        created.push(body);
    }
    let id = |at: usize| created[at]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        created[0],
        json!({"id": id(0), "name": "a", "status": "active", "region": "eu"})
    );
    assert_ne!(id(0), id(1));
    // Read.
    let (status, headers, read) = h.send("GET", &format!("/sites/{}", id(0)), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read, created[0]);
    // Listed, filtered and paged.
    let (_, headers, list) = h.send("GET", "/sites?status=active", None, &[]).await;
    assert_eq!(source(&headers), "state", "{list}");
    assert_eq!(
        ids(&list["results"]),
        [json!(id(0)), json!(id(2))],
        "{list}"
    );
    let (_, _, page) = h.send("GET", "/sites?limit=1&offset=1", None, &[]).await;
    assert_eq!(page["count"], json!(3), "{page}");
    assert_eq!(ids(&page["results"]), [json!(id(1))], "{page}");
    // Updated.
    let (status, headers, updated) = h
        .send(
            "PATCH",
            &format!("/sites/{}", id(0)),
            Some(json!({"name": "a2"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(source(&headers), "state", "{updated}");
    assert_eq!(updated["name"], json!("a2"));
    let (_, _, read) = h.send("GET", &format!("/sites/{}", id(0)), None, &[]).await;
    assert_eq!(read, updated);
    // Deleted.
    let (status, _, _) = h
        .send("DELETE", &format!("/sites/{}", id(0)), None, &[])
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, gone) = h.send("GET", &format!("/sites/{}", id(0)), None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    let (_, _, list) = h.send("GET", "/sites", None, &[]).await;
    assert_eq!(ids(&list["results"]), [json!(id(1)), json!(id(2))]);
    assert!(h.provider.requests().is_empty(), "no LLM call");
    assert_eq!(
        capture.of("createSite"),
        vec![line("createSite", "create", None); 3]
    );
    assert_eq!(
        capture.of("updateSite"),
        vec![line("updateSite", "update", None)]
    );
    assert_eq!(
        capture.of("deleteSite"),
        vec![line("deleteSite", "delete", None)]
    );
    assert_eq!(
        capture.of("listSites"),
        vec![line("listSites", "list", None); 3]
    );
    assert_eq!(
        capture.of("getSite"),
        vec![line("getSite", "read", None); 2],
        "the read of the deleted site is refused before the executor"
    );
}

#[tokio::test]
async fn a_record_created_under_its_parent_is_listed_there_only() {
    let (capture, _guard) = Capture::install();
    let h = support::described(JOURNEY, journey_world(), Vec::new());
    let (status, headers, rack) = h
        .send("POST", "/sites/s1/racks", Some(json!({"label": "r1"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{rack}");
    assert_eq!(source(&headers), "state", "{rack}");
    let id = rack["id"].as_str().unwrap().to_owned();
    assert_eq!(rack, json!({"id": id, "label": "r1", "site": "s1"}));
    let (_, headers, listed) = h.send("GET", "/sites/s1/racks", None, &[]).await;
    assert_eq!(source(&headers), "state", "{listed}");
    assert_eq!(listed, json!({"racks": [rack.clone()]}));
    let (_, _, other) = h.send("GET", "/sites/s2/racks", None, &[]).await;
    assert_eq!(other, json!({"racks": []}));
    let (status, headers, read) = h
        .send("GET", &format!("/sites/s1/racks/{id}"), None, &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read, rack);
    assert_eq!(
        capture.of("createRack"),
        vec![line("createRack", "create", None)]
    );
    assert_eq!(
        capture.of("listRacks"),
        vec![line("listRacks", "list", None); 2]
    );
    assert!(h.provider.requests().is_empty());
}

#[tokio::test]
async fn a_create_fills_what_its_reads_require_so_they_are_answered_from_state() {
    let (capture, _guard) = Capture::install();
    // The fill's answer: the members the read (`size`) and the list (`color`) require,
    // which neither the request nor the create's answer holds.
    let h = support::described(
        JOURNEY,
        json!({}),
        vec![json!({"size": 4, "color": "teal"})],
    );
    let (status, headers, body) = h
        .send("POST", "/things", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(body, json!({"id": id, "name": "a"}));
    let prompt = &h.provider.requests()[0].messages[0].content;
    assert!(
        prompt.contains("size") && prompt.contains("color"),
        "{prompt}"
    );
    let (status, headers, read) = h.send("GET", &format!("/things/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state", "{read}");
    let stored = json!({"id": id, "name": "a", "size": 4, "color": "teal"});
    assert_eq!(read, stored, "the record as stored");
    assert_eq!(capture.of("getThing"), vec![line("getThing", "read", None)]);
    let (_, headers, listed) = h.send("GET", "/things", None, &[]).await;
    assert_eq!(source(&headers), "state", "{listed}");
    assert_eq!(listed, json!([{"id": id, "color": "teal"}]));
    assert_eq!(h.provider.requests().len(), 1, "reads make no LLM call");
}

/// Tickets are created with some of their `fields`; their read requires a `status`
/// among them.
const NESTED: &str = r##"
openapi: 3.0.0
info: {title: Nested, version: "1"}
paths:
  /tickets:
    post:
      operationId: createTicket
      requestBody: {content: {application/json: {schema: {type: object, properties: {fields: {type: object, properties: {summary: {type: string}}}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {type: object, required: [id], properties: {id: {type: string}}}}}}}
  /tickets/{id}:
    get:
      operationId: getTicket
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}}
components:
  schemas:
    Ticket:
      type: object
      required: [id, fields]
      properties:
        id: {type: string}
        fields:
          type: object
          required: [summary, status]
          properties: {summary: {type: string}, status: {type: string}, notes: {type: string}}
"##;

#[tokio::test]
async fn a_create_fills_what_an_object_member_it_sent_lacks() {
    let (capture, _guard) = Capture::install();
    // The fill's answer: the `status` the read requires inside `fields`, the optional
    // `notes` nobody set, and a `summary` it never takes (the request sent one).
    let h = support::harness(
        NESTED,
        vec![json!({"fields": {"status": "open", "notes": "n", "summary": "not sent"}})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let sent = json!({"fields": {"summary": "printer jam"}});
    let (status, headers, body) = h.send("POST", "/tickets", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, headers, read) = h.send("GET", &format!("/tickets/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(
        read,
        json!({"id": id, "fields": {"summary": "printer jam", "status": "open", "notes": "n"}})
    );
    assert_eq!(
        capture.of("getTicket"),
        vec![line("getTicket", "read", None)]
    );
    assert_eq!(h.provider.requests().len(), 1, "the fill's one call");
}

/// Orders count their lines, a member their read requires; lines are created under
/// their order.
const DERIVED: &str = r##"
openapi: 3.0.0
info: {title: Derived, version: "1"}
paths:
  /orders:
    post:
      operationId: createOrder
      requestBody: {content: {application/json: {schema: {type: object, properties: {note: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{orderId}:
    get:
      operationId: getOrder
      parameters: [{name: orderId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{orderId}/lines:
    parameters: [{name: orderId, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createLine
      requestBody: {content: {application/json: {schema: {type: object, properties: {sku: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}}
  /orders/{orderId}/lines/{lineId}:
    get:
      operationId: getLine
      parameters:
        - {name: orderId, in: path, required: true, schema: {type: string}}
        - {name: lineId, in: path, required: true, schema: {type: string}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}}
components:
  schemas:
    Order:
      type: object
      required: [id, note, line_count]
      properties: {id: {type: string}, note: {type: string}, line_count: {type: integer}}
    Line:
      type: object
      required: [id, sku]
      properties: {id: {type: string}, sku: {type: string}}
"##;

#[tokio::test]
async fn a_created_record_answers_its_derived_members_from_state() {
    let (capture, _guard) = Capture::install();
    let world = json!({"types": {
        "lines": {"parent": {"type": "orders", "path_param": "orderId"}},
        "orders": {"derived": {"line_count": {"kind": "count", "of": "lines"}}}
    }});
    let h = support::described(DERIVED, world, Vec::new());
    let (status, headers, order) = h
        .send("POST", "/orders", Some(json!({"note": "n"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(source(&headers), "state", "{order}");
    let id = order["id"].as_str().unwrap().to_owned();
    assert_eq!(order, json!({"id": id, "note": "n", "line_count": 0}));
    assert_eq!(
        capture.of("createOrder"),
        vec![create_line("createOrder", None)]
    );
    for sku in ["a", "b"] {
        let (status, _, line) = h
            .send(
                "POST",
                &format!("/orders/{id}/lines"),
                Some(json!({"sku": sku})),
                &[],
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{line}");
    }
    let (status, headers, read) = h.send("GET", &format!("/orders/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["line_count"], json!(2), "{read}");
    // Computed on serve, never stored.
    let stored = h.stored("orders", &id).await.unwrap();
    assert!(stored.get("line_count").is_none(), "{stored}");
    assert!(h.provider.requests().is_empty(), "no LLM call");
}

/// Creates that name their id elsewhere than a body member: keys `PUT` under the name
/// in their path; folders take a patterned id; counters an integer one; conversations
/// answer only a `Resource-ID` header and tickets a uuid `Ticket-Id`; zones answer
/// their record one item deep in an envelope; gauges carry a fixed `kind`; payments
/// take an idempotency key.
const IDS: &str = r##"
openapi: 3.0.0
info: {title: Ids, version: "1"}
paths:
  /keys/{name}:
    parameters: [{name: name, in: path, required: true, schema: {type: string}}]
    put:
      operationId: putKey
      requestBody: {content: {application/json: {schema: {type: object, properties: {value: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Key"}}}}}
    get:
      operationId: getKey
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Key"}}}}}
  /folders:
    post:
      operationId: createFolder
      requestBody: {content: {application/json: {schema: {type: object, properties: {displayName: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
  /folders/{folderId}:
    get:
      operationId: getFolder
      parameters: [{name: folderId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
  /counters:
    post:
      operationId: createCounter
      requestBody: {content: {application/json: {schema: {type: object, properties: {label: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Counter"}}}}}
  /counters/{counterId}:
    get:
      operationId: getCounter
      parameters: [{name: counterId, in: path, required: true, schema: {type: integer}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Counter"}}}}}
  /conversations:
    post:
      operationId: createConversation
      requestBody: {content: {application/json: {schema: {type: object, properties: {subject: {type: string}}}}}}
      responses:
        "201":
          description: created
          headers: {Resource-ID: {schema: {type: string}}}
  /conversations/{conversationId}:
    get:
      operationId: getConversation
      parameters: [{name: conversationId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Conversation"}}}}}
  /tickets:
    post:
      operationId: createTicket
      requestBody: {content: {application/json: {schema: {type: object, properties: {subject: {type: string}}}}}}
      responses:
        "201":
          description: created
          headers: {Ticket-Id: {schema: {type: string, format: uuid}}}
  /tickets/{ticketId}:
    get:
      operationId: getTicket
      parameters: [{name: ticketId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Conversation"}}}}}
  /zones:
    post:
      operationId: createZone
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/ZoneEnvelope"}}}}}
  /zones/{zoneId}:
    get:
      operationId: getZone
      parameters: [{name: zoneId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/ZoneEnvelope"}}}}}
  /gauges:
    post:
      operationId: createGauge
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Gauge"}}}}}
  /gauges/{gaugeId}:
    get:
      operationId: getGauge
      parameters: [{name: gaugeId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Gauge"}}}}}
  /payments:
    post:
      operationId: createPayment
      parameters: [{name: Idempotency-Key, in: header, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {amount: {type: integer}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}}
  /payments/{paymentId}:
    get:
      operationId: getPayment
      parameters: [{name: paymentId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}}
components:
  schemas:
    Key:
      type: object
      required: [name, value]
      properties: {name: {type: string}, value: {type: string}}
    Folder:
      type: object
      required: [id, displayName]
      properties: {id: {type: string}, displayName: {type: string}}
    Counter:
      type: object
      required: [id, label]
      properties: {id: {type: integer}, label: {type: string}}
    Conversation:
      type: object
      required: [id, subject]
      properties: {id: {type: string}, subject: {type: string}}
    Zone:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    ZoneEnvelope:
      type: object
      required: [success, result]
      properties:
        success: {type: boolean}
        result: {type: array, items: {$ref: "#/components/schemas/Zone"}}
    Gauge:
      type: object
      required: [id, name, kind]
      properties: {id: {type: string}, name: {type: string}, kind: {type: string, const: gauge}, unit: {type: string, enum: [ms]}}
    Payment:
      type: object
      required: [id, amount]
      properties: {id: {type: string}, amount: {type: integer}}
"##;

/// `uri`'s read, answered from state.
async fn read_from_state(h: &support::DescribedHarness, uri: &str) -> serde_json::Value {
    let (status, headers, body) = h.send("GET", uri, None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    assert_eq!(source(&headers), "state", "{uri}: {body}");
    body
}

#[tokio::test]
async fn a_put_create_takes_the_id_its_uri_names() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"putKey": {"kind": "create"}}});
    let h = support::described(IDS, world, Vec::new());
    let (status, headers, body) = h
        .send("PUT", "/keys/alpha", Some(json!({"value": "1"})), &[])
        .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"name": "alpha", "value": "1"}));
    assert_eq!(capture.of("putKey"), vec![line("putKey", "create", None)]);
    assert_eq!(read_from_state(&h, "/keys/alpha").await, body);
}

#[tokio::test]
async fn a_patterned_id_is_minted_in_its_pattern() {
    let (capture, _guard) = Capture::install();
    let world = json!({"types": {"folders": {"identity": {"pattern": "fld_{id}"}}}});
    let h = support::described(IDS, world, Vec::new());
    let (status, headers, body) = h
        .send("POST", "/folders", Some(json!({"displayName": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let full = body["id"].as_str().unwrap().to_owned();
    let bare = full
        .strip_prefix("fld_")
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(h.stored("folders", &full).await, Some(body.clone()));
    assert_eq!(read_from_state(&h, &format!("/folders/{bare}")).await, body);
    assert_eq!(
        capture.of("createFolder"),
        vec![create_line("createFolder", None)]
    );
}

#[tokio::test]
async fn an_integer_identity_is_minted_as_an_integer() {
    let (capture, _guard) = Capture::install();
    let h = support::described(IDS, json!({}), Vec::new());
    let (status, headers, body) = h
        .send("POST", "/counters", Some(json!({"label": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = body["id"].as_i64().unwrap_or_else(|| panic!("{body}"));
    assert_eq!(read_from_state(&h, &format!("/counters/{id}")).await, body);
    assert_eq!(
        capture.of("createCounter"),
        vec![create_line("createCounter", None)]
    );
}

#[tokio::test]
async fn an_identity_header_names_the_minted_id() {
    let (capture, _guard) = Capture::install();
    let h = support::described(IDS, json!({}), Vec::new());
    let (status, headers, body) = h
        .send(
            "POST",
            "/conversations",
            Some(json!({"subject": "hi"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = headers
        .get("resource-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_else(|| panic!("{headers:?}"))
        .to_owned();
    assert_eq!(
        read_from_state(&h, &format!("/conversations/{id}")).await,
        json!({"id": id, "subject": "hi"})
    );
    assert_eq!(
        capture.of("createConversation"),
        vec![create_line("createConversation", None)]
    );
}

#[tokio::test]
async fn an_identity_header_whose_schema_refuses_the_pattern_is_not_placed() {
    let (_capture, _guard) = Capture::install();
    // `Ticket-Id` is a uuid: an id in the `tickets/{id}` pattern is none.
    let world = json!({"types": {"tickets": {"identity": {"pattern": "tickets/{id}"}}}});
    let h = support::described(IDS, world, Vec::new());
    let (status, headers, _) = h
        .send("POST", "/tickets", Some(json!({"subject": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let placed = headers
        .get("ticket-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(!placed.starts_with("tickets/"), "{placed}");
}

#[tokio::test]
async fn a_record_answered_in_an_envelope_is_stored_and_read_there() {
    let (capture, _guard) = Capture::install();
    let world = json!({
        "types": {"zones": {"identity": {"member": "id"}}},
        "operations": {
            "createZone": {"answer": {"record": "/result/0"}},
            "getZone": {"kind": "read", "target": {"path_param": "zoneId"}, "answer": {"record": "/result/0"}}
        }
    });
    let h = support::described(IDS, world, Vec::new());
    let (status, headers, body) = h
        .send("POST", "/zones", Some(json!({"name": "z"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let zone = body["result"][0].clone();
    let id = zone["id"].as_str().unwrap_or_else(|| panic!("{body}"));
    assert_eq!(zone, json!({"id": id, "name": "z"}));
    assert_eq!(h.stored("zones", id).await, Some(zone.clone()));
    let read = read_from_state(&h, &format!("/zones/{id}")).await;
    assert_eq!(read["result"][0], zone, "{read}");
    assert_eq!(
        capture.of("createZone"),
        vec![create_line("createZone", None)]
    );
}

#[tokio::test]
async fn a_member_its_schema_fixes_takes_that_value() {
    let (capture, _guard) = Capture::install();
    let h = support::described(IDS, json!({}), Vec::new());
    let (status, headers, body) = h
        .send("POST", "/gauges", Some(json!({"name": "g"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    // `kind` is required and fixed, and so is the optional one-value `unit`: neither is
    // a guess.
    assert_eq!(
        body,
        json!({"id": id, "name": "g", "kind": "gauge", "unit": "ms"})
    );
    assert!(h.provider.requests().is_empty(), "nothing to fill");
    assert_eq!(
        capture.of("createGauge"),
        vec![create_line("createGauge", None)]
    );
}

#[tokio::test]
async fn an_executed_create_is_replayed_for_its_idempotency_key() {
    let (capture, _guard) = Capture::install();
    let world =
        json!({"operations": {"createPayment": {"idempotency": {"header": "Idempotency-Key"}}}});
    let h = support::described(IDS, world, Vec::new());
    let key = [("Idempotency-Key", "k-1")];
    let (status, _, first) = h
        .send("POST", "/payments", Some(json!({"amount": 5})), &key)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, _, again) = h
        .send("POST", "/payments", Some(json!({"amount": 5})), &key)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    assert_eq!(again, first, "the repeat is the first answer");
    let id = first["id"].as_str().unwrap();
    assert_eq!(h.stored("payments", id).await, Some(first.clone()));
    assert_eq!(
        capture.of("createPayment"),
        vec![create_line("createPayment", None)],
        "the repeat is replayed before the executor"
    );
}

#[tokio::test]
async fn an_executed_create_sends_its_webhook() {
    use mb_protocols::webhooks::deliver::{DispatchSettings, ResolvedTarget, WebhookDispatcher};
    use mb_protocols::webhooks::log::SharedDeliveryLog;
    use mb_protocols::webhooks::sign::Signing;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (capture, _guard) = Capture::install();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let log: SharedDeliveryLog = std::sync::Arc::default();
    let dispatcher = WebhookDispatcher::new(
        "default".into(),
        "described".into(),
        vec![ResolvedTarget {
            url: server.uri(),
            events: vec!["*".into()],
            signing: Signing::None,
            secret: Ok(None),
            delay: std::time::Duration::ZERO,
        }],
        reqwest::Client::new(),
        log,
        DispatchSettings::default(),
    );
    let h = support::harness(IDS, Vec::new(), &HarnessOptions::default(), |mut rt| {
        rt.webhooks = Some(std::sync::Arc::new(dispatcher));
        rt
    });
    let (status, headers, body) = h
        .send("POST", "/gauges", Some(json!({"name": "g"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(
        capture.of("createGauge"),
        vec![create_line("createGauge", None)]
    );
    let mut sent = Vec::new();
    for _ in 0..200 {
        sent = server.received_requests().await.unwrap_or_default();
        if !sent.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(sent.len(), 1);
    let event: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert!(
        event["type"]
            .as_str()
            .is_some_and(|t| t.ends_with(".created")),
        "{event}"
    );
    assert_eq!(event["data"]["object"], body);
}

/// Flags acknowledge their create with `{ok, data}`, `data` holding the record.
const FLAGS: &str = r##"
openapi: 3.0.0
info: {title: Flags, version: "1"}
paths:
  /flags:
    post:
      operationId: createFlag
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}, enabled: {type: boolean}}}}}}
      responses:
        "201":
          description: created
          content:
            application/json:
              schema:
                type: object
                required: [ok, data]
                properties:
                  ok: {type: boolean}
                  data: {type: object, required: [id], properties: {id: {type: string}, name: {type: string}, enabled: {type: boolean}, archived: {type: boolean}}}
  /flags/{id}:
    get:
      operationId: getFlag
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Flag"}}}}}
components:
  schemas:
    Flag:
      type: object
      required: [id, name, enabled]
      properties: {id: {type: string}, name: {type: string}, enabled: {type: boolean}}
"##;

#[tokio::test]
async fn an_acknowledgement_keeps_the_record_s_false_and_affirms_its_own_booleans() {
    let (capture, _guard) = Capture::install();
    let h = support::described(FLAGS, json!({}), Vec::new());
    let (status, headers, body) = h
        .send(
            "POST",
            "/flags",
            Some(json!({"name": "beta", "enabled": false})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    // The acknowledgement's own booleans say the write succeeded; the record's `false`
    // stays false.
    assert_eq!(body["ok"], json!(true), "{body}");
    assert_eq!(body["data"]["enabled"], json!(false), "{body}");
    assert_eq!(body["data"]["name"], json!("beta"), "{body}");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        h.stored("flags", &id).await,
        Some(json!({"id": id, "name": "beta", "enabled": false}))
    );
    assert_eq!(
        capture.of("createFlag"),
        vec![create_line("createFlag", None)]
    );
}

/// GitLab's issue notes: their list is scoped by the issue's `issue_iid` path value,
/// which a note holds as `noteable_iid`.
const NOTES: &str = r##"
openapi: 3.0.0
info: {title: Notes, version: "1"}
paths:
  /issues/{issue_iid}/notes:
    parameters: [{name: issue_iid, in: path, required: true, schema: {type: integer}}]
    get:
      operationId: listIssueNotes
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Note"}}}}}}
    post:
      operationId: createIssueNote
      requestBody: {content: {application/json: {schema: {type: object, required: [body], properties: {body: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
  /issues/{issue_iid}/notes/{note_id}:
    get:
      operationId: getIssueNote
      parameters:
        - {name: issue_iid, in: path, required: true, schema: {type: integer}}
        - {name: note_id, in: path, required: true, schema: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
components:
  schemas:
    Note:
      type: object
      required: [id, body, noteable_iid]
      properties: {id: {type: integer}, body: {type: string}, noteable_iid: {type: integer}}
"##;

#[tokio::test]
async fn a_nested_create_holds_the_path_value_its_list_s_scope_matches() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"listIssueNotes": {
        "scope": {"param": "issue_iid", "member": "noteable_iid"}
    }}});
    let h = support::described(NOTES, world, Vec::new());
    let (status, headers, note) = h
        .send("POST", "/issues/7/notes", Some(json!({"body": "hi"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{note}");
    assert_eq!(source(&headers), "state", "{note}");
    assert_eq!(
        note["noteable_iid"],
        json!(7),
        "the issue's iid, typed: {note}"
    );
    let listed = read_from_state(&h, "/issues/7/notes").await;
    assert_eq!(listed, json!([note]));
    assert_eq!(read_from_state(&h, "/issues/8/notes").await, json!([]));
    assert_eq!(
        capture.of("createIssueNote"),
        vec![create_line("createIssueNote", None)]
    );
    assert_eq!(
        capture.of("listIssueNotes"),
        vec![line("listIssueNotes", "list", None); 2]
    );
    assert!(h.provider.requests().is_empty(), "no LLM call");
}

/// Chargebee's customers: an id the client may give, unique, and the codes the API's
/// description says a taken one and a deleted one answer.
const CUSTOMERS: &str = r##"
openapi: 3.0.0
info:
  title: Customers
  version: "1"
  description: |
    A customer's `id` can be given on create; it is unique, and a create with an id
    in use is refused with 400 and `api_error_code` `duplicate_entry`. A deleted
    customer answers 404 with `api_error_code` `resource_not_found`.
paths:
  /customers:
    post:
      operationId: createCustomer
      requestBody: {content: {application/json: {schema: {type: object, properties: {id: {type: string}, name: {type: string}}}}}}
      responses:
        "200": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}
        "400": {description: invalid, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /customers/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getCustomer
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}
        "404": {description: missing, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
    delete:
      operationId: deleteCustomer
      responses: {"204": {description: deleted}}
components:
  schemas:
    Customer:
      type: object
      required: [id]
      properties: {id: {type: string}, name: {type: string}}
    Error:
      type: object
      required: [message, api_error_code]
      properties:
        message: {type: string}
        api_error_code: {type: string, description: "What went wrong: `duplicate_entry`, `resource_not_found` and so on."}
"##;

fn customers(script: Vec<serde_json::Value>) -> support::DescribedHarness {
    let world = json!({
        "types": {"customers": {
            "identity": {"member": "id"},
            "unique": [{"members": ["id"], "conflict": 400}]
        }},
        "operations": {"getCustomer": {"kind": "read", "type": "customers", "not_found": 404}}
    });
    support::harness(CUSTOMERS, script, &HarnessOptions::default(), |rt| {
        support::with_effective(rt, world, false)
    })
}

/// A unique set refused from state fills what the model's body left empty with the
/// code the contract gives a taken value, and lays the call's message over it.
#[tokio::test]
async fn a_unique_refusal_from_state_fills_the_contract_s_code_where_left_empty() {
    let (_capture, _guard) = Capture::install();
    let h = customers(vec![
        json!({"id": "c1", "name": "Ana"}),
        json!({"message": "taken", "api_error_code": ""}),
    ]);
    let (status, _, body) = h
        .send(
            "POST",
            "/customers",
            Some(json!({"id": "c1", "name": "Ana"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, headers, body) = h
        .send(
            "POST",
            "/customers",
            Some(json!({"id": "c1", "name": "Rui"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["api_error_code"], json!("duplicate_entry"), "{body}");
    assert_eq!(
        body["message"],
        json!("id 'c1' is already in use"),
        "{body}"
    );
}

/// A not-found refusal from state keeps the code the model wrote for it, whatever the
/// contract's prose names: only the members it left empty take a value.
#[tokio::test]
async fn a_not_found_refusal_from_state_keeps_the_model_s_code() {
    let (_capture, _guard) = Capture::install();
    let h = customers(vec![
        json!({"id": "c1", "name": "Ana"}),
        json!({"message": "gone", "api_error_code": "customer_gone"}),
    ]);
    let (status, _, body) = h
        .send(
            "POST",
            "/customers",
            Some(json!({"id": "c1", "name": "Ana"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = h.send("DELETE", "/customers/c1", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _, body) = h.send("GET", "/customers/c1", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["api_error_code"], json!("customer_gone"), "{body}");
}

/// Pipelines are read with their owner as an object, but created naming it by id;
/// orders are read with an id on every line, created without; accounts default some
/// members their create may leave out, in their record's schema or in the create's
/// request schema. Pipelines and orders answer their create with an acknowledgement
/// (`{id}`), which renders whatever the record holds.
const GATED: &str = r##"
openapi: 3.0.0
info: {title: Gated, version: "1"}
paths:
  /pipelines:
    post:
      operationId: createPipeline
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}, owner: {}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /pipelines/{id}:
    get:
      operationId: getPipeline
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Pipeline"}}}}}
  /teams:
    post:
      operationId: createTeam
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}, members: {type: array}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /teams/{id}:
    get:
      operationId: getTeam
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Team"}}}}}
  /coupons:
    post:
      operationId: createCoupon
      requestBody: {content: {application/json: {schema: {type: object, properties: {code: {type: string}, tier: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /coupons/{id}:
    get:
      operationId: getCoupon
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Coupon"}}}}}
  /carts:
    post:
      operationId: createCart
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}}
  /carts/{id}:
    get:
      operationId: getCart
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}}
  /deals:
    post:
      operationId: createDeal
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}, org_id: {type: integer}, tag_ids: {type: array, items: {type: integer}}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Deal"}}}}}
  /deals/{id}:
    get:
      operationId: getDeal
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Deal"}}}}}
  /users/{id}:
    get:
      operationId: getUser
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}}
  /orders:
    post:
      operationId: createOrder
      requestBody: {content: {application/json: {schema: {type: object, properties: {lines: {type: array, items: {type: object, properties: {sku: {type: string}, qty: {type: integer}}}}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /orders/{id}:
    get:
      operationId: getOrder
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /accounts:
    post:
      operationId: createAccount
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}, plan: {type: string, default: free}, settings: {type: object, properties: {theme: {type: string}}}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Account"}}}}}
  /accounts/{id}:
    get:
      operationId: getAccount
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Account"}}}}}
  /notes:
    get:
      operationId: listNotes
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/NoteSummary"}}}}}}
    post:
      operationId: createNote
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}, body: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
  /notes/{id}:
    get:
      operationId: getNote
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
  /receipts:
    post:
      operationId: createReceipt
      requestBody: {content: {application/json: {schema: {type: object, properties: {total: {type: integer}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {type: object, required: [id], properties: {id: {type: string}}, not: {}}}}}}
  /receipts/{id}:
    get:
      operationId: getReceipt
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, required: [id, total], properties: {id: {type: string}, total: {type: integer}}}}}}}
components:
  schemas:
    Ack:
      type: object
      required: [id]
      properties: {id: {type: string}}
    Note:
      type: object
      required: [id, title, body]
      properties: {id: {type: string}, title: {type: string}, body: {type: string}}
    NoteSummary:
      type: object
      additionalProperties: false
      required: [id, title]
      properties: {id: {type: string}, title: {type: string}}
    Pipeline:
      type: object
      required: [id, name, owner]
      properties:
        id: {type: string}
        name: {type: string}
        owner: {$ref: "#/components/schemas/User"}
    Deal:
      type: object
      required: [id, title]
      properties: {id: {type: string}, title: {type: string}, org_id: {type: integer}, tag_ids: {type: array, items: {type: integer}}, stage: {type: string}, probability: {type: integer}}
    Team:
      type: object
      required: [id, name, members]
      properties: {id: {type: string}, name: {type: string}, members: {type: array, items: {$ref: "#/components/schemas/User"}}}
    Coupon:
      type: object
      required: [id, code, tier]
      properties: {id: {type: string}, code: {type: string}, tier: {type: string, enum: [gold, silver]}}
    Cart:
      type: object
      required: [id, name, version]
      properties: {id: {type: string}, name: {type: string}, version: {type: integer}}
    User:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Order:
      type: object
      required: [id, lines]
      properties:
        id: {type: string}
        lines: {type: array, items: {type: object, required: [id, sku], properties: {id: {type: string}, sku: {type: string}, qty: {type: integer}}}}
    Account:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        is_active: {type: boolean, default: true}
        plan: {type: string}
        password: {type: string, writeOnly: true}
        object: {type: string, enum: [account]}
        settings:
          type: object
          properties: {theme: {type: string}, notify: {type: boolean, default: false}}
"##;

fn invalid_line(op: &str, why: &str) -> ExecLine {
    ExecLine {
        render_error: Some(why.into()),
        ..create_line(op, Some("record_invalid"))
    }
}

#[tokio::test]
async fn a_reference_to_a_held_record_is_stored_as_that_record() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(GATED, vec![], &HarnessOptions::default(), |rt| rt);
    let users = type_of(&h, "getUser");
    h.hold(
        &users,
        &[json!({"id": "u1", "name": "Ann", "email": "ann@example.com"})],
    )
    .await;
    let sent = json!({"name": "deploy", "owner": "u1"});
    let (status, headers, body) = h.send("POST", "/pipelines", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(
        capture.of("createPipeline"),
        vec![create_line("createPipeline", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let t = type_of(&h, "getPipeline");
    let stored = h.stored(&t, &id).await.unwrap();
    assert_eq!(
        stored["owner"],
        json!({"id": "u1", "name": "Ann"}),
        "the held user, as the member declares it"
    );
    assert!(h.provider.requests().is_empty(), "nothing to fill");
}

#[tokio::test]
async fn a_reference_to_no_held_record_is_filled_never_stored_raw() {
    let (capture, _guard) = Capture::install();
    // The fill's answer for the owner the request named no held record by.
    let h = support::harness(
        GATED,
        vec![json!({"owner": {"id": "u9", "name": "Zed"}})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let sent = json!({"name": "deploy", "owner": "u9"});
    let (status, headers, body) = h.send("POST", "/pipelines", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(
        capture.of("createPipeline"),
        vec![create_line("createPipeline", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let t = type_of(&h, "getPipeline");
    let stored = h.stored(&t, &id).await.unwrap();
    assert_eq!(stored["owner"], json!({"id": "u9", "name": "Zed"}));
    let (_, headers, read) = h.send("GET", &format!("/pipelines/{id}"), None, &[]).await;
    assert_eq!(source(&headers), "state", "{read}");
}

#[tokio::test]
async fn an_array_item_declaring_the_identity_gets_a_minted_id_of_its_own() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(GATED, vec![], &HarnessOptions::default(), |rt| rt);
    let sent = json!({"lines": [{"sku": "a"}, {"sku": "b"}]});
    let (status, headers, body) = h.send("POST", "/orders", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(
        capture.of("createOrder"),
        vec![create_line("createOrder", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let t = type_of(&h, "getOrder");
    let stored = h.stored(&t, &id).await.unwrap();
    let lines = stored["lines"].as_array().unwrap();
    let ids: Vec<&str> = lines.iter().filter_map(|l| l["id"].as_str()).collect();
    assert_eq!(ids.len(), 2, "{stored}");
    assert_ne!(ids[0], ids[1]);
    assert_eq!(lines[0]["sku"], json!("a"));
    let (_, headers, read) = h.send("GET", &format!("/orders/{id}"), None, &[]).await;
    assert_eq!(source(&headers), "state", "{read}");
}

#[tokio::test]
async fn what_a_sent_array_item_lacks_is_filled_item_by_item() {
    let (capture, _guard) = Capture::install();
    // The fill's answer: the `sku` the line lacks, and a `qty` it never takes.
    let h = support::harness(
        GATED,
        vec![json!({"lines": [{"sku": "s-1", "qty": 9}]})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let sent = json!({"lines": [{"qty": 2}]});
    let (status, headers, body) = h.send("POST", "/orders", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(
        capture.of("createOrder"),
        vec![create_line("createOrder", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let stored = h.stored(&type_of(&h, "getOrder"), &id).await.unwrap();
    let line = &stored["lines"][0];
    assert_eq!(
        (&line["sku"], &line["qty"]),
        (&json!("s-1"), &json!(2)),
        "{stored}"
    );
    assert!(line["id"].is_string(), "minted: {stored}");
}

#[tokio::test]
async fn an_unsent_member_takes_its_schema_s_default_at_any_depth() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(GATED, vec![], &HarnessOptions::default(), |rt| rt);
    let sent = json!({"name": "acme", "settings": {"theme": "dark"}});
    let (status, headers, body) = h.send("POST", "/accounts", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let record = json!({
        "id": id,
        "name": "acme",
        "is_active": true,
        "plan": "free",
        "object": "account",
        "settings": {"theme": "dark", "notify": false}
    });
    assert_eq!(body, record, "the answer holds the defaults");
    let t = type_of(&h, "getAccount");
    assert_eq!(
        h.stored(&t, &id).await,
        Some(record),
        "and so does the record"
    );
    assert_eq!(
        capture.of("createAccount"),
        vec![create_line("createAccount", None)]
    );
    assert!(
        h.provider.requests().is_empty(),
        "nothing is filled, the write-only `password` neither"
    );
}

#[tokio::test]
async fn a_sent_member_keeps_its_value_over_its_default() {
    let (_capture, _guard) = Capture::install();
    let h = support::harness(GATED, vec![], &HarnessOptions::default(), |rt| rt);
    let sent = json!({"name": "acme", "is_active": false});
    let (status, _, body) = h.send("POST", "/accounts", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["is_active"], json!(false));
}

#[tokio::test]
async fn a_create_whose_record_its_reads_render_still_executes() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(GATED, vec![], &HarnessOptions::default(), |rt| rt);
    let sent = json!({"name": "deploy", "owner": {"id": "u1", "name": "Ann"}});
    let (status, headers, body) = h.send("POST", "/pipelines", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        capture.of("createPipeline"),
        vec![create_line("createPipeline", None)]
    );
    let (_, headers, read) = h.send("GET", &format!("/pipelines/{id}"), None, &[]).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["owner"], json!({"id": "u1", "name": "Ann"}));
}

#[tokio::test]
async fn a_create_its_slimmer_closed_list_item_renders_once_projected_executes() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(GATED, vec![], &HarnessOptions::default(), |rt| rt);
    let sent = json!({"title": "t", "body": "b"});
    let (status, headers, body) = h.send("POST", "/notes", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    // The list's items hold no `body`: the list projects the record before it checks it.
    assert_eq!(
        capture.of("createNote"),
        vec![create_line("createNote", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let t = type_of(&h, "getNote");
    assert_eq!(h.stored_ids(&t).await, vec![id]);
}

#[tokio::test]
async fn a_create_whose_answer_does_not_render_falls_back_as_render_failed() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        GATED,
        vec![json!({"id": "r-gen"})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (_, _, body) = h
        .send("POST", "/receipts", Some(json!({"total": 3})), &[])
        .await;
    // The record would render into its read; the answer never does.
    assert_eq!(
        capture.of("createReceipt"),
        vec![create_line("createReceipt", Some("render_failed"))],
        "{body}"
    );
    let t = type_of(&h, "getReceipt");
    let stored = h.stored_ids(&t).await;
    assert!(
        stored.len() <= 1 && stored.iter().all(|id| body["id"] == json!(id)),
        "the executor stored nothing: {stored:?} {body}"
    );
}

/// Columns and tags are read only in their lists, which hold their ids as an integer
/// and as a 24-digit hex string.
const LISTED_IDS: &str = r##"
openapi: 3.0.0
info: {title: Listed, version: "1"}
paths:
  /columns:
    get:
      operationId: listColumns
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Column"}}}}}}
    post:
      operationId: createColumn
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Column"}}}}}
  /tags:
    get:
      operationId: listTags
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Tag"}}}}}}
    post:
      operationId: createTag
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}}
components:
  schemas:
    Column:
      type: object
      required: [id, name]
      properties: {id: {type: integer}, name: {type: string}}
    Tag:
      type: object
      required: [id, name]
      properties: {id: {type: string, pattern: "^[0-9a-f]{24}$"}, name: {type: string}}
"##;

#[tokio::test]
async fn a_minted_id_fits_the_lists_that_read_its_record() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(LISTED_IDS, vec![], &HarnessOptions::default(), |rt| rt);
    let (status, headers, body) = h
        .send("POST", "/columns", Some(json!({"name": "todo"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert!(body["id"].is_i64(), "an integer id: {body}");
    assert_eq!(
        capture.of("createColumn"),
        vec![create_line("createColumn", None)]
    );
    let (status, _, body) = h
        .send("POST", "/tags", Some(json!({"name": "red"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createTag"),
        vec![create_line("createTag", None)]
    );
    let id = body["id"].as_str().unwrap_or_default();
    assert!(
        id.len() == 24 && id.chars().all(|c| c.is_ascii_hexdigit()),
        "{body}"
    );
}

#[tokio::test]
async fn unsent_optional_members_are_filled_but_a_reference_the_request_could_send() {
    let (capture, _guard) = Capture::install();
    // The fill's answer: the optional members, and an `org_id` it never takes.
    let h = support::harness(
        GATED,
        vec![json!({"stage": "open", "probability": 50, "org_id": 7, "tag_ids": [1]})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, headers, body) = h
        .send("POST", "/deals", Some(json!({"title": "big"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let record = json!({"id": id, "title": "big", "stage": "open", "probability": 50});
    assert_eq!(body, record, "no organization the request did not name");
    let t = type_of(&h, "getDeal");
    assert_eq!(h.stored(&t, &id).await, Some(record));
    assert_eq!(
        capture.of("createDeal"),
        vec![create_line("createDeal", None)]
    );
    let prompt = &h.provider.requests()[0].messages[1].content;
    assert!(
        !prompt.contains("org_id") && !prompt.contains("tag_ids"),
        "{prompt}"
    );
}

#[tokio::test]
async fn an_array_of_references_to_held_records_is_stored_as_those_records() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(GATED, vec![], &HarnessOptions::default(), |rt| rt);
    let users = type_of(&h, "getUser");
    h.hold(
        &users,
        &[
            json!({"id": "u1", "name": "Ann"}),
            json!({"id": "u2", "name": "Bo"}),
        ],
    )
    .await;
    let sent = json!({"name": "core", "members": ["u1", "u2"]});
    let (status, _, body) = h.send("POST", "/teams", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createTeam"),
        vec![create_line("createTeam", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let stored = h.stored(&type_of(&h, "getTeam"), &id).await.unwrap();
    assert_eq!(
        stored["members"],
        json!([{"id": "u1", "name": "Ann"}, {"id": "u2", "name": "Bo"}])
    );
}

#[tokio::test]
async fn an_embedded_record_of_another_type_gets_no_minted_id() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        GATED,
        vec![json!({"id": "t-gen"})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    // A member is a user, whose id is not the team's to mint.
    let sent = json!({"name": "core", "members": [{"name": "Ann"}]});
    let (status, _, body) = h.send("POST", "/teams", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createTeam"),
        vec![invalid_line("createTeam", "/members/0/id required")]
    );
}

#[tokio::test]
async fn a_value_lacking_only_required_members_is_kept_and_completed() {
    let (capture, _guard) = Capture::install();
    // The fill's answer: the owner's `name`, and an `id` it never takes.
    let h = support::harness(
        GATED,
        vec![json!({"owner": {"id": "other", "name": "Kim"}})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let sent = json!({"name": "deploy", "owner": {"id": "u5"}});
    let (status, _, body) = h.send("POST", "/pipelines", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createPipeline"),
        vec![create_line("createPipeline", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let stored = h.stored(&type_of(&h, "getPipeline"), &id).await.unwrap();
    assert_eq!(stored["owner"], json!({"id": "u5", "name": "Kim"}));
}

#[tokio::test]
async fn a_sent_value_breaking_its_enum_is_never_replaced_and_the_create_generates() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        GATED,
        vec![json!({"id": "c-gen"})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let sent = json!({"code": "SAVE", "tier": "Gold"});
    let (status, headers, body) = h.send("POST", "/coupons", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "llm", "{body}");
    assert_eq!(
        capture.of("createCoupon"),
        vec![invalid_line("createCoupon", "/tier enum")]
    );
    let t = type_of(&h, "getCoupon");
    assert_eq!(h.stored_ids(&t).await, vec!["c-gen".to_owned()]);
}

#[tokio::test]
async fn a_reference_is_looked_up_by_its_patterned_id() {
    let (capture, _guard) = Capture::install();
    let world = json!({"types": {"users": {"identity": {"pattern": "users/{id}"}}}});
    let h = support::described(GATED, world, Vec::new());
    let users = type_of(&h, "getUser");
    assert_eq!(users, "users");
    h.hold(&users, &[json!({"id": "users/u1", "name": "Ann"})])
        .await;
    let sent = json!({"name": "deploy", "owner": "u1"});
    let (status, _, body) = h.send("POST", "/pipelines", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createPipeline"),
        vec![create_line("createPipeline", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let stored = h.stored(&type_of(&h, "getPipeline"), &id).await.unwrap();
    assert_eq!(stored["owner"], json!({"id": "users/u1", "name": "Ann"}));
}

#[tokio::test]
async fn a_described_concurrency_token_starts_minted_never_filled() {
    let (capture, _guard) = Capture::install();
    let world = json!({"types": {"carts": {"concurrency": {"member": "version"}}}});
    // A fill's answer for `version`, never taken.
    let h = support::described(GATED, world, vec![json!({"version": 7})]);
    assert_eq!(type_of(&h, "getCart"), "carts");
    let (status, _, body) = h
        .send("POST", "/carts", Some(json!({"name": "c"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createCart"),
        vec![create_line("createCart", None)]
    );
    assert_eq!(body["version"], json!(1), "{body}");
    assert!(h.provider.requests().is_empty(), "nothing is filled");
}

/// Things are read by id with a text id, and listed with an integer one: no id is both.
const CONFLICTING_IDS: &str = r##"
openapi: 3.0.0
info: {title: Conflicting, version: "1"}
paths:
  /things:
    get:
      operationId: listThings
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {type: object, required: [id], properties: {id: {type: integer}, name: {type: string}}}}}}}}
    post:
      operationId: createThing
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {type: object, required: [ok], properties: {ok: {type: boolean}}}}}}}
  /things/{id}:
    get:
      operationId: getThing
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, required: [id], properties: {id: {type: string}, name: {type: string}}}}}}}
"##;

#[tokio::test]
async fn reads_declaring_conflicting_ids_mint_none_and_the_create_generates() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        CONFLICTING_IDS,
        vec![json!({"ok": true})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/things", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createThing"),
        vec![create_line("createThing", Some("no_id"))]
    );
}

/// Creates whose records only the server completes: a lifecycle state the member does not
/// take, a nullable reference, a member sent as another JSON type, a sender's wrapper, a
/// nullable parent sent short, a version a write carries, a server-set boolean, a
/// comment under its pull request and a secret embedding the id.
const SERVER_SET: &str = r##"
openapi: 3.0.0
info: {title: ServerSet, version: "1"}
paths:
  /folders:
    post:
      operationId: createFolder
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}, parent: {type: object, properties: {id: {type: string}}}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /folders/{id}:
    get:
      operationId: getFolder
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
  /tasks:
    post:
      operationId: createTask
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}, assignee: {type: string, nullable: true}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /tasks/{id}:
    get:
      operationId: getTask
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}}
  /docs:
    post:
      operationId: createDoc
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}, owner: {}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /docs/{id}:
    get:
      operationId: getDoc
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}}
  /members/{id}:
    get:
      operationId: getMember
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Member"}}}}}
  /issues:
    post:
      operationId: createIssue
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}, labels: {type: string}, notes: {type: string}, zip: {type: integer}, count: {type: string}, open: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /issues/{id}:
    get:
      operationId: getIssue
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}}
  /conversations:
    post:
      operationId: createConversation
      requestBody: {content: {application/json: {schema: {type: object, properties: {from: {type: object, properties: {type: {type: string}, id: {type: string}}}, body: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /conversations/{id}:
    get:
      operationId: getConversation
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Conversation"}}}}}
  /carts:
    post:
      operationId: createCart
      requestBody: {content: {application/json: {schema: {type: object, properties: {currency: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}}
  /carts/{id}:
    get:
      operationId: getCart
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}}
    post:
      operationId: updateCart
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, required: [version, actions], properties: {version: {type: integer}, actions: {type: array, items: {type: object}}}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}
        "409": {description: stale version, content: {application/json: {schema: {type: object, properties: {message: {type: string}}}}}}
  /workspaces:
    post:
      operationId: createWorkspace
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Workspace"}}}}}
  /workspaces/{id}:
    get:
      operationId: getWorkspace
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Workspace"}}}}}
  /pulls/{pull_request_id}:
    get:
      operationId: getPull
      parameters: [{name: pull_request_id, in: path, required: true, schema: {type: integer}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Pull"}}}}}
  /pulls/{pull_request_id}/comments:
    post:
      operationId: createComment
      parameters: [{name: pull_request_id, in: path, required: true, schema: {type: integer}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {content: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Comment"}}}}}
  /pulls/{pull_request_id}/comments/{comment_id}:
    get:
      operationId: getComment
      parameters:
        - {name: pull_request_id, in: path, required: true, schema: {type: integer}}
        - {name: comment_id, in: path, required: true, schema: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Comment"}}}}}
  /intents:
    post:
      operationId: createIntent
      requestBody: {content: {application/json: {schema: {type: object, properties: {amount: {type: integer}}}}}}
      responses: {"200": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}}
  /intents/{id}:
    get:
      operationId: getIntent
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}}
components:
  schemas:
    Ack:
      type: object
      required: [id]
      properties: {id: {type: string}}
    Folder:
      type: object
      required: [id, name, item_status, parent]
      properties:
        id: {type: string}
        name: {type: string}
        item_status: {type: string, enum: [active, trashed]}
        parent:
          anyOf:
            - {type: object, required: [id, name], properties: {id: {type: string}, name: {type: string}}}
            - {type: "null"}
    Member:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Doc:
      type: object
      required: [id, title, owner]
      properties:
        id: {type: string}
        title: {type: string}
        owner: {anyOf: [{$ref: "#/components/schemas/Member"}, {type: "null"}]}
    Task:
      type: object
      required: [id, name, assignee]
      properties:
        id: {type: string}
        name: {type: string}
        assignee: {allOf: [{$ref: "#/components/schemas/Member"}], nullable: true}
    Issue:
      type: object
      required: [id, title, labels]
      properties:
        id: {type: string}
        title: {type: string}
        labels: {type: array, items: {type: string}}
        notes: {type: array, items: {type: object, required: [body], properties: {body: {type: string}}}}
        zip: {type: string}
        count: {type: integer}
        open: {type: boolean}
    Conversation:
      type: object
      required: [id, type, body]
      properties: {id: {type: string}, type: {type: string, enum: [conversation]}, body: {type: string}}
    Cart:
      type: object
      required: [id, version, currency]
      properties: {id: {type: string}, version: {type: integer}, currency: {type: string}}
    Workspace:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}, locked: {type: boolean, nullable: true}, "@odata.etag": {type: string}}
    Pull:
      type: object
      required: [id, title]
      properties: {id: {type: integer}, title: {type: string}}
    Comment:
      type: object
      required: [id, content, pullrequest]
      properties: {id: {type: integer}, content: {type: string}, pullrequest: {$ref: "#/components/schemas/Pull"}}
    Intent:
      type: object
      required: [id, amount, client_secret]
      properties:
        id: {type: string, pattern: "^pi_[A-Za-z0-9]{24}$"}
        amount: {type: integer}
        client_secret: {type: string, pattern: "^pi_[A-Za-z0-9]{24}_secret_[A-Za-z0-9]{24}$"}
"##;

#[tokio::test]
async fn a_lifecycle_state_its_member_does_not_take_is_filled_instead() {
    let (capture, _guard) = Capture::install();
    let world =
        json!({"types": {"folders": {"lifecycle": {"member": "item_status", "initial": "LIVE"}}}});
    let h = support::described(SERVER_SET, world, vec![json!({"item_status": "active"})]);
    let sent = json!({"name": "docs", "parent": {"id": "0"}});
    let (status, _, body) = h.send("POST", "/folders", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createFolder"),
        vec![create_line("createFolder", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let stored = h.stored("folders", &id).await.unwrap();
    assert!(
        ["active", "trashed"].contains(&stored["item_status"].as_str().unwrap_or("LIVE")),
        "filled as its member declares it: {stored}"
    );
    // The nullable parent sent with its id alone is kept, its name filled.
    assert_eq!(stored["parent"]["id"], json!("0"), "{stored}");
    assert!(stored["parent"]["name"].is_string(), "{stored}");
}

#[tokio::test]
async fn a_nullable_reference_through_an_all_of_is_embedded_or_settled() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(SERVER_SET, vec![], &HarnessOptions::default(), |rt| rt);
    let members = type_of(&h, "getMember");
    h.hold(&members, &[json!({"id": "u1", "name": "Ann"})])
        .await;
    let (_, _, body) = h
        .send(
            "POST",
            "/tasks",
            Some(json!({"name": "a", "assignee": "u1"})),
            &[],
        )
        .await;
    let id = body["id"].as_str().unwrap().to_owned();
    let t = type_of(&h, "getTask");
    let stored = h.stored(&t, &id).await.unwrap();
    assert_eq!(stored["assignee"], json!({"id": "u1", "name": "Ann"}));
    let (_, _, body) = h
        .send(
            "POST",
            "/tasks",
            Some(json!({"name": "b", "assignee": "u404"})),
            &[],
        )
        .await;
    assert_eq!(
        capture.of("createTask"),
        vec![
            create_line("createTask", None),
            create_line("createTask", None)
        ]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let stored = h.stored(&t, &id).await.unwrap();
    assert_eq!(
        stored["assignee"],
        json!(null),
        "a reference to no held record names none: never the raw id, never invented"
    );
    assert!(
        stored.as_object().unwrap().contains_key("assignee"),
        "{stored}"
    );
}

#[tokio::test]
async fn a_member_sent_as_another_json_type_is_converted_losing_nothing() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(SERVER_SET, vec![], &HarnessOptions::default(), |rt| rt);
    let sent =
        json!({"title": "t", "labels": "bug,ui", "zip": 94107, "count": "3", "open": "true"});
    let (status, _, body) = h.send("POST", "/issues", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createIssue"),
        vec![create_line("createIssue", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let stored = h.stored(&type_of(&h, "getIssue"), &id).await.unwrap();
    assert_eq!(stored["labels"], json!(["bug", "ui"]));
    assert_eq!(stored["zip"], json!("94107"));
    assert_eq!(stored["count"], json!(3));
    assert_eq!(stored["open"], json!(true));
}

#[tokio::test]
async fn a_member_of_another_json_type_with_no_lossless_form_is_refused_never_filled() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        SERVER_SET,
        vec![json!({"id": "i-gen"})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    // Neither text becomes what the reads hold without losing or inventing something.
    for (sent, why) in [
        (
            json!({"title": "t", "labels": "a", "notes": "hello"}),
            "/notes type",
        ),
        (
            json!({"title": "t", "labels": "a", "count": "0.50"}),
            "/count type",
        ),
    ] {
        let (status, _, body) = h.send("POST", "/issues", Some(sent), &[]).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let lines = capture.of("createIssue");
        assert_eq!(
            lines.last(),
            Some(&invalid_line("createIssue", why)),
            "{lines:?}"
        );
    }
}

#[tokio::test]
async fn a_wrapper_s_member_that_does_not_fit_the_record_is_not_taken() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(SERVER_SET, vec![], &HarnessOptions::default(), |rt| rt);
    let sent = json!({"from": {"type": "user", "id": "c9"}, "body": "hi"});
    let (status, _, body) = h.send("POST", "/conversations", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createConversation"),
        vec![create_line("createConversation", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let stored = h
        .stored(&type_of(&h, "getConversation"), &id)
        .await
        .unwrap();
    assert_eq!(stored["type"], json!("conversation"), "{stored}");
}

#[tokio::test]
async fn a_version_a_write_carries_starts_at_one() {
    let (capture, _guard) = Capture::install();
    // A fill's answer for `version`, never taken.
    let h = support::harness(
        SERVER_SET,
        vec![json!({"version": 2})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/carts", Some(json!({"currency": "EUR"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createCart"),
        vec![create_line("createCart", None)]
    );
    assert_eq!(body["version"], json!(1), "{body}");
}

#[tokio::test]
async fn a_nullable_boolean_fill_leaves_null_stays_null() {
    let (_capture, _guard) = Capture::install();
    let h = support::harness(
        SERVER_SET,
        vec![json!({"locked": null, "@odata.etag": "W/\"1\""})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/workspaces", Some(json!({"name": "w"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["locked"], json!(null), "{body}");
}

#[tokio::test]
async fn a_member_named_for_the_path_s_parent_is_that_record() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(SERVER_SET, vec![], &HarnessOptions::default(), |rt| rt);
    let pulls = type_of(&h, "getPull");
    h.hold(&pulls, &[json!({"id": 2, "title": "Fix totals"})])
        .await;
    let sent = json!({"content": "looks good"});
    let (status, _, body) = h.send("POST", "/pulls/2/comments", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createComment"),
        vec![create_line("createComment", None)]
    );
    assert_eq!(body["pullrequest"], json!({"id": 2, "title": "Fix totals"}));
}

#[tokio::test]
async fn a_member_whose_pattern_embeds_the_id_starts_with_the_record_s_id() {
    let (_capture, _guard) = Capture::install();
    let h = support::harness(SERVER_SET, vec![], &HarnessOptions::default(), |rt| rt);
    let (status, _, body) = h
        .send("POST", "/intents", Some(json!({"amount": 5})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["id"].as_str().unwrap();
    let secret = body["client_secret"].as_str().unwrap();
    assert!(secret.starts_with(&format!("{id}_secret_")), "{body}");
}

#[tokio::test]
async fn an_embedded_record_s_id_is_never_invented_through_a_nullable_wrapper() {
    let (capture, _guard) = Capture::install();
    let h = support::harness(
        SERVER_SET,
        vec![json!({"owner": {"id": "invented", "name": "Ann"}})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    // The owner is a member (`anyOf: [$ref, null]`) sent without its id.
    let sent = json!({"title": "a", "owner": {"name": "Ann"}});
    let (status, _, body) = h.send("POST", "/docs", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let lines = capture.of("createDoc");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(
        lines[0].exec_fallback.as_deref(),
        Some("record_invalid"),
        "{lines:?}"
    );
}

#[tokio::test]
async fn a_lifecycle_state_is_matched_to_its_member_s_enum_spelling() {
    let (capture, _guard) = Capture::install();
    let world = json!({"types": {"folders": {"lifecycle": {"member": "item_status", "initial": "ACTIVE"}}}});
    let h = support::described(SERVER_SET, world, Vec::new());
    let sent = json!({"name": "docs", "parent": {"id": "0", "name": "root"}});
    let (status, _, body) = h.send("POST", "/folders", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createFolder"),
        vec![create_line("createFolder", None)]
    );
    let id = body["id"].as_str().unwrap().to_owned();
    let stored = h.stored("folders", &id).await.unwrap();
    assert_eq!(stored["item_status"], json!("active"), "{stored}");
}

/// Records placed by their path: items under a space, held at a dotted `meta.space`;
/// cards under a board, whose `board` member is an object; tags under an owner the
/// path names as `owner_id_or_key`; repos created at their own URI under a group.
const PLACED: &str = r##"
openapi: 3.0.0
info: {title: Placed, version: "1"}
paths:
  /spaces/{space}:
    parameters: [{name: space, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getSpace
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}}
  /boards/{board}:
    parameters: [{name: board, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBoard
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {slug: {type: string}}}}}}}
  /spaces/{space}/items:
    parameters: [{name: space, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listItems
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}}
    post:
      operationId: createItem
      requestBody: {content: {application/json: {schema: {type: object, properties: {meta: {type: object, properties: {name: {type: string}}}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "409": {description: taken, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /boards/{board}/cards:
    parameters: [{name: board, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listCards
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Card"}}}}}}
    post:
      operationId: createCard
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Card"}}}}}
  /owners/{owner_id_or_key}/tags:
    parameters: [{name: owner_id_or_key, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createTag
      requestBody: {content: {application/json: {schema: {type: object, properties: {label: {type: string}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}
        "409": {description: taken, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /groups/{group}/repos/{repo_key}:
    parameters:
      - {name: group, in: path, required: true, schema: {type: string}}
      - {name: repo_key, in: path, required: true, schema: {type: string}}
    post:
      operationId: createRepo
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Repo"}}}}
        "400": {description: invalid, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
    get:
      operationId: getRepo
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Repo"}}}}}
  /orgs/{org}:
    parameters: [{name: org, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getOrg
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}, name: {type: string}}}}}}}
  /orgs/{org}/notes:
    parameters: [{name: org, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listNotes
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Note"}}}}}}
    post:
      operationId: createNote
      requestBody: {content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
  /projects/{project_id}/issues:
    parameters: [{name: project_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createIssue
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
        "409": {description: taken, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
components:
  schemas:
    Item:
      type: object
      required: [id, meta]
      properties:
        id: {type: string}
        meta: {type: object, required: [name, space], properties: {name: {type: string}, space: {type: string}}}
    Card:
      type: object
      required: [id, title, board]
      properties:
        id: {type: string}
        title: {type: string}
        board: {type: object, required: [slug, name], properties: {slug: {type: string}, name: {type: string}}}
    Tag:
      type: object
      required: [id, label, owner]
      properties:
        id: {type: string}
        label: {type: string}
        owner: {type: object, required: [id, title], properties: {id: {type: string}, title: {type: string}}}
    Repo:
      type: object
      required: [key, title, group]
      properties:
        key: {type: string}
        title: {type: string}
        group: {type: object, required: [slug], properties: {slug: {type: string}, name: {type: string}}}
    Note:
      type: object
      required: [id, text, organization]
      properties:
        id: {type: string}
        text: {type: string}
        organization: {type: object, required: [id, name], properties: {id: {type: string}, name: {type: string}}}
    Issue:
      type: object
      required: [id, title, project]
      properties:
        id: {type: string}
        title: {type: string}
        project: {type: object, required: [id], properties: {id: {type: integer}}}
    Problem:
      type: object
      properties: {message: {type: string}}
"##;

fn placed_world() -> serde_json::Value {
    json!({
        "types": {
            "items": {
                "parent": {"type": "spaces", "member": "meta.space", "path_param": "space"},
                "unique": [{"members": ["meta.name"], "scope": "meta.space", "conflict": 409}]
            },
            "cards": {"parent": {"type": "boards", "path_param": "board"}},
            "tags": {"unique": [{"members": ["label"], "scope": "owner.id", "conflict": 409}]},
            "repos": {
                "identity": {"member": "key"},
                "unique": [{"members": ["key"], "scope": "group.slug", "conflict": 400}]
            },
            "notes": {"parent": {"type": "orgs", "member": "organization", "path_param": "org"}},
            "issues": {"unique": [{"members": ["title"], "scope": "project.id", "conflict": 409}]}
        },
        "operations": {
            "listItems": {"kind": "list", "type": "items", "scope": {"param": "space", "member": "meta.space"}},
            "getSpace": {"kind": "read", "type": "spaces", "target": {"path_param": "space"}},
            "getBoard": {"kind": "read", "type": "boards", "target": {"path_param": "board"}},
            "createItem": {"kind": "create", "type": "items"},
            "listCards": {"kind": "list", "type": "cards", "scope": {"param": "board", "member": "board.slug"}},
            "createCard": {"kind": "create", "type": "cards"},
            "createTag": {"kind": "create", "type": "tags"},
            "createRepo": {"kind": "create", "type": "repos", "target": {"path_param": "repo_key"}},
            "getRepo": {"kind": "read", "type": "repos", "target": {"path_param": "repo_key"}},
            "getOrg": {"kind": "read", "type": "orgs", "target": {"path_param": "org"}},
            "listNotes": {"kind": "list", "type": "notes"},
            "createNote": {"kind": "create", "type": "notes"},
            "createIssue": {"kind": "create", "type": "issues"}
        }
    })
}

#[tokio::test]
async fn a_dotted_parent_member_is_set_nested_and_scopes_its_list_and_unique_set() {
    let (capture, _guard) = Capture::install();
    let h = support::described(PLACED, placed_world(), Vec::new());
    let mut made = Vec::new();
    for (space, name) in [("s1", "a"), ("s1", "b"), ("s2", "a")] {
        let sent = json!({"meta": {"name": name}});
        let uri = format!("/spaces/{space}/items");
        let (status, headers, body) = h.send("POST", &uri, Some(sent), &[]).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(source(&headers), "state", "{body}");
        assert_eq!(
            body["meta"],
            json!({"name": name, "space": space}),
            "{body}"
        );
        assert!(
            body.get("meta.space").is_none(),
            "no literal dotted key: {body}"
        );
        made.push(body);
    }
    let id = made[0]["id"].as_str().unwrap().to_owned();
    let stored = h.stored("items", &id).await.unwrap();
    assert!(stored.get("meta.space").is_none(), "{stored}");
    assert_eq!(stored["meta"]["space"], json!("s1"), "{stored}");
    assert_eq!(
        read_from_state(&h, "/spaces/s1/items").await,
        json!([made[0], made[1]])
    );
    assert_eq!(
        read_from_state(&h, "/spaces/s2/items").await,
        json!([made[2]])
    );
    // A repeat within the same space is refused; the space is the path's.
    let (status, _, body) = h
        .send(
            "POST",
            "/spaces/s1/items",
            Some(json!({"meta": {"name": "a"}})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        capture.of("createItem"),
        vec![create_line("createItem", None); 3]
    );
}

#[tokio::test]
async fn a_path_value_goes_into_the_described_member_of_an_object_member() {
    let (capture, _guard) = Capture::install();
    let h = support::described(PLACED, placed_world(), Vec::new());
    let (status, headers, card) = h
        .send("POST", "/boards/b1/cards", Some(json!({"title": "x"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{card}");
    assert_eq!(source(&headers), "state", "{card}");
    // The path's value, never the faker's, and the board's other members filled.
    assert_eq!(card["board"]["slug"], json!("b1"), "{card}");
    assert!(card["board"]["name"].is_string(), "{card}");
    assert_eq!(read_from_state(&h, "/boards/b1/cards").await, json!([card]));
    assert_eq!(read_from_state(&h, "/boards/b2/cards").await, json!([]));
    assert_eq!(
        capture.of("createCard"),
        vec![create_line("createCard", None)]
    );
}

#[tokio::test]
async fn a_unique_scope_under_a_member_the_path_names_refuses_a_repeat_there() {
    let h = support::described(PLACED, placed_world(), Vec::new());
    let (status, _, tag) = h
        .send(
            "POST",
            "/owners/o1/tags",
            Some(json!({"label": "red"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{tag}");
    assert_eq!(
        tag["owner"]["id"],
        json!("o1"),
        "the path's, not the faker's: {tag}"
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/owners/o1/tags",
            Some(json!({"label": "red"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _, other) = h
        .send(
            "POST",
            "/owners/o2/tags",
            Some(json!({"label": "red"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{other}");
    assert_eq!(other["owner"]["id"], json!("o2"), "{other}");
}

#[tokio::test]
async fn a_create_at_its_own_uri_takes_its_id_and_scope_from_the_path() {
    let h = support::described(PLACED, placed_world(), Vec::new());
    let (status, headers, repo) = h
        .send(
            "POST",
            "/groups/g1/repos/r1",
            Some(json!({"title": "t"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{repo}");
    assert_eq!(source(&headers), "state", "{repo}");
    assert_eq!(repo["key"], json!("r1"), "{repo}");
    assert_eq!(repo["group"]["slug"], json!("g1"), "{repo}");
    let (status, _, body) = h
        .send(
            "POST",
            "/groups/g1/repos/r1",
            Some(json!({"title": "t"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(read_from_state(&h, "/groups/g1/repos/r1").await, repo);
}

#[tokio::test]
async fn a_top_level_parent_member_that_is_an_object_never_holds_the_raw_path_value() {
    let h = support::described(PLACED, placed_world(), Vec::new());
    let (status, headers, note) = h
        .send("POST", "/orgs/o1/notes", Some(json!({"text": "x"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{note}");
    assert_eq!(source(&headers), "state", "{note}");
    assert!(note["organization"].is_object(), "{note}");
    let id = note["id"].as_str().unwrap().to_owned();
    let stored = h.stored("notes", &id).await.unwrap();
    assert!(stored["organization"].is_object(), "{stored}");
}

#[tokio::test]
async fn an_integer_scope_from_the_path_is_typed_and_refuses_a_repeat() {
    let h = support::described(PLACED, placed_world(), Vec::new());
    let (status, _, issue) = h
        .send(
            "POST",
            "/projects/7/issues",
            Some(json!({"title": "bug"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{issue}");
    assert_eq!(issue["project"]["id"], json!(7), "{issue}");
    let (status, _, body) = h
        .send(
            "POST",
            "/projects/7/issues",
            Some(json!({"title": "bug"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _, other) = h
        .send(
            "POST",
            "/projects/8/issues",
            Some(json!({"title": "bug"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{other}");
}

/// Boards are created from their query; workspaces hold settings with a server-set
/// `locked`, are locked by an action route past their key and copied by a create past
/// it, hold vars, and are moved by a long-running action; employees are keyed by a
/// plain text id another route types as an integer; apps are read by id or name; lines
/// are created under a deal; builds carry a number besides their id that a pipeline's
/// route reads them by.
const REPAIRS: &str = r##"
openapi: 3.0.0
info: {title: Repairs, version: "1"}
paths:
  /boards:
    post:
      operationId: createBoard
      parameters:
        - {name: name, in: query, schema: {type: string}}
        - {name: desc, in: query, schema: {type: string}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}}
  /boards/{id}:
    get:
      operationId: getBoard
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}}
  /workspaces:
    post:
      operationId: createWorkspace
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}, settings: {type: object, properties: {color: {type: string}}}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Workspace"}}}}}
  /workspaces/{workspace_id}:
    get:
      operationId: getWorkspace
      parameters: [{name: workspace_id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Workspace"}}}}}
  /workspaces/{workspace_id}/actions/lock:
    post:
      operationId: lockWorkspace
      parameters: [{name: workspace_id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Workspace"}}}}}
  /workspaces/{workspace_id}/actions/copy:
    post:
      operationId: copyWorkspace
      parameters: [{name: workspace_id, in: path, required: true, schema: {type: string}}]
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Workspace"}}}}}
  /workspaces/{workspace_id}/vars:
    post:
      operationId: createVar
      parameters: [{name: workspace_id, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {key: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Var"}}}}}
  /vars/{id}:
    get:
      operationId: getVar
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Var"}}}}}
  /workspaces/{workspace_id}/move:
    post:
      operationId: moveWorkspace
      parameters: [{name: workspace_id, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {parent: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Operation"}}}}}
  /employees:
    post:
      operationId: createEmployee
      requestBody: {content: {application/json: {schema: {type: object, properties: {email: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Employee"}}}}}
  /employees/{id}:
    get:
      operationId: getEmployee
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Employee"}}}}}
  /employees/{employeeId}/notes:
    put:
      operationId: setEmployeeNotes
      parameters: [{name: employeeId, in: path, required: true, schema: {type: integer}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}}}
  /apps:
    post:
      operationId: createApp
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/App"}}}}
        "422": {description: invalid, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /apps/{app_id_or_name}:
    get:
      operationId: getApp
      parameters: [{name: app_id_or_name, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/App"}}}}}
  /deals/{id}:
    get:
      operationId: getDeal
      parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Deal"}}}}}
  /deals/{id}/lines:
    post:
      operationId: createLine
      parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {sku: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}}
  /lines/{id}:
    get:
      operationId: getLine
      parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}}
  /entries:
    post:
      operationId: createEntry
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Entry"}}}}}
  /entries/{id}:
    get:
      operationId: getEntry
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Entry"}}}}}
  /builds:
    post:
      operationId: createBuild
      requestBody: {content: {application/json: {schema: {type: object, properties: {branch: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Build"}}}}}
  /builds/{id}:
    get:
      operationId: getBuild
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Build"}}}}}
  /pipelines/{pipeline}/builds/{number}:
    get:
      operationId: getPipelineBuild
      parameters:
        - {name: pipeline, in: path, required: true, schema: {type: string}}
        - {name: number, in: path, required: true, schema: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Build"}}}}}
components:
  schemas:
    Board:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}, desc: {type: string}}
    Workspace:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        parent: {type: string}
        settings:
          type: object
          properties: {color: {type: string}, locked: {type: boolean, readOnly: true}}
    Var:
      type: object
      required: [id, key]
      properties: {id: {type: string}, key: {type: string}}
    Operation:
      type: object
      required: [name, done]
      properties: {name: {type: string}, done: {type: boolean}, response: {$ref: "#/components/schemas/Workspace"}}
    Employee:
      type: object
      required: [id]
      properties: {id: {type: string}, email: {type: string}}
    App:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Problem:
      type: object
      properties: {id: {type: string}, message: {type: string}}
    Deal:
      type: object
      required: [id]
      properties: {id: {type: integer}, title: {type: string}}
    Line:
      type: object
      required: [id, sku]
      properties: {id: {type: integer}, deal_id: {type: integer}, sku: {type: string}}
    Entry:
      type: object
      required: [meta, title]
      properties:
        title: {type: string}
        meta: {type: object, required: [id], properties: {id: {type: string}, published: {type: integer}}}
    Build:
      type: object
      required: [id, number, branch]
      properties: {id: {type: string}, number: {type: integer}, branch: {type: string}}
"##;

/// A [`REPAIRS`] harness with no description, whose LLM answers `script`.
fn repairs(script: Vec<serde_json::Value>) -> support::DescribedHarness {
    support::harness(REPAIRS, script, &HarnessOptions::default(), |rt| rt)
}

#[tokio::test]
async fn a_create_takes_the_record_members_its_query_names() {
    let (capture, _guard) = Capture::install();
    let h = repairs(Vec::new());
    let (status, headers, body) = h
        .send("POST", "/boards?name=alpha&desc=first", None, &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["name"], json!("alpha"), "{body}");
    assert_eq!(body["desc"], json!("first"), "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let t = type_of(&h, "getBoard");
    assert_eq!(
        h.stored(&t, &id).await,
        Some(json!({"id": id, "name": "alpha", "desc": "first"}))
    );
    assert_eq!(
        capture.of("createBoard"),
        vec![create_line("createBoard", None)]
    );
}

#[tokio::test]
async fn fill_completes_the_optional_members_of_an_object_member_sent() {
    let (capture, _guard) = Capture::install();
    let h = repairs(vec![json!({"parent": "p0", "settings": {"locked": false}})]);
    let (status, _, body) = h
        .send(
            "POST",
            "/workspaces",
            Some(json!({"name": "w", "settings": {"color": "red"}})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["settings"],
        json!({"color": "red", "locked": false}),
        "{body}"
    );
    assert_eq!(
        capture.of("createWorkspace"),
        vec![create_line("createWorkspace", None)]
    );
}

#[tokio::test]
async fn an_object_sent_inside_a_wrapper_takes_its_optional_members_too() {
    let h = repairs(vec![json!({"parent": "p0", "settings": {"locked": false}})]);
    let sent = json!({"workspace": {"name": "w", "settings": {"color": "red"}}});
    let (status, _, body) = h.send("POST", "/workspaces", Some(sent), &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["settings"],
        json!({"color": "red", "locked": false}),
        "{body}"
    );
}

#[tokio::test]
async fn an_object_only_the_server_opened_takes_no_optional_members() {
    // What a fill would answer for the entry's unpublished version.
    let world = json!({"types": {"entries": {"identity": {"member": "meta.id"}}}});
    let h = support::described(REPAIRS, world, vec![json!({"meta": {"published": 1}})]);
    let (status, _, body) = h
        .send("POST", "/entries", Some(json!({"title": "t"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let meta = body["meta"].as_object().unwrap();
    assert_eq!(meta.keys().collect::<Vec<_>>(), vec!["id"], "{body}");
}

#[tokio::test]
async fn a_post_past_its_own_record_s_key_is_an_action_never_a_create() {
    let (capture, _guard) = Capture::install();
    let h = repairs(Vec::new());
    let t = type_of(&h, "getWorkspace");
    h.hold(&t, &[json!({"id": "w1", "name": "a"})]).await;
    h.send("POST", "/workspaces/w1/actions/lock", None, &[])
        .await;
    let lines = capture.of("lockWorkspace");
    assert!(
        !lines.is_empty() && lines.iter().all(|l| l.exec == "action"),
        "{lines:?}"
    );
    // A child collection under the same record still creates.
    let (status, _, body) = h
        .send(
            "POST",
            "/workspaces/w1/vars",
            Some(json!({"key": "k"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        capture.of("createVar"),
        vec![create_line("createVar", None)]
    );
}

#[tokio::test]
async fn a_described_alternate_is_minted_unique_never_filled() {
    let (capture, _guard) = Capture::install();
    let world =
        json!({"types": {"builds": {"identity": {"member": "id", "alternates": ["number"]}}}});
    // What a fill would answer for both: the same number.
    let h = support::described(
        REPAIRS,
        world,
        vec![json!({"number": 2}), json!({"number": 2})],
    );
    let mut numbers = Vec::new();
    for branch in ["main", "next"] {
        let (status, _, body) = h
            .send("POST", "/builds", Some(json!({"branch": branch})), &[])
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        numbers.push(body["number"].clone());
    }
    assert_eq!(numbers, vec![json!(1), json!(2)]);
    assert!(h.provider.requests().is_empty(), "no fill wrote a number");
    assert_eq!(
        capture.of("createBuild"),
        vec![create_line("createBuild", None); 2]
    );
}

#[tokio::test]
async fn a_minted_text_id_takes_the_digits_another_route_types_it_by() {
    let h = repairs(Vec::new());
    for _ in 0..4 {
        let (status, _, body) = h
            .send("POST", "/employees", Some(json!({"email": "a@b.c"})), &[])
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["id"].as_str().unwrap_or_default().to_owned();
        assert!(
            !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()),
            "{body}"
        );
    }
}

#[tokio::test]
async fn a_create_repeating_the_name_its_read_route_names_a_record_by_is_refused() {
    let h = repairs(Vec::new());
    let (status, _, made) = h
        .send("POST", "/apps", Some(json!({"name": "alpha"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, _, body) = h
        .send("POST", "/apps", Some(json!({"name": "alpha"})), &[])
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let t = type_of(&h, "getApp");
    assert_eq!(h.stored_ids(&t).await.len(), 1);
    let (status, _, other) = h
        .send("POST", "/apps", Some(json!({"name": "beta"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{other}");
}

#[tokio::test]
async fn an_undescribed_child_create_names_the_parent_its_path_names() {
    let (capture, _guard) = Capture::install();
    // What a fill would answer for the parent's id.
    let h = repairs(vec![json!({"deal_id": 99})]);
    let (status, _, body) = h
        .send("POST", "/deals/7/lines", Some(json!({"sku": "s1"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["deal_id"], json!(7), "{body}");
    assert_eq!(
        capture.of("createLine"),
        vec![create_line("createLine", None)]
    );
}

#[tokio::test]
async fn a_long_running_action_answers_its_changed_record_as_done() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"moveWorkspace": {
        "kind": "action",
        "target": {"path_param": "workspace_id"},
        "changes": {"parent": {"from_request": "/parent"}},
        "answer": {"long_running": {"done": "done", "result": "response"}}
    }}});
    let h = support::described(REPAIRS, world, Vec::new());
    let t = type_of(&h, "getWorkspace");
    h.hold(&t, &[json!({"id": "w1", "name": "a", "parent": "p1"})])
        .await;
    for _ in 0..3 {
        let (status, headers, body) = h
            .send(
                "POST",
                "/workspaces/w1/move",
                Some(json!({"parent": "p2"})),
                &[],
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(source(&headers), "state", "{body}");
        assert_eq!(body["done"], json!(true), "{body}");
        assert_eq!(
            body["response"],
            json!({"id": "w1", "name": "a", "parent": "p2"}),
            "{body}"
        );
    }
    assert_eq!(
        capture.of("moveWorkspace"),
        vec![action_line("moveWorkspace", None); 3]
    );
}

#[tokio::test]
async fn a_post_past_its_own_record_s_key_answering_201_is_a_create() {
    let (capture, _guard) = Capture::install();
    let h = repairs(Vec::new());
    let t = type_of(&h, "getWorkspace");
    h.hold(&t, &[json!({"id": "w1", "name": "a"})]).await;
    let (status, headers, body) = h
        .send("POST", "/workspaces/w1/actions/copy", None, &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_ne!(body["id"], json!("w1"), "{body}");
    assert_eq!(
        capture.of("copyWorkspace"),
        vec![create_line("copyWorkspace", None)]
    );
    assert_eq!(h.stored_ids(&t).await.len(), 2);
}

#[tokio::test]
async fn a_member_a_route_reads_records_by_is_minted_unique_and_kept_when_sent() {
    let (capture, _guard) = Capture::install();
    // What a fill would answer for both: the same number.
    let h = repairs(vec![json!({"number": 2}), json!({"number": 2})]);
    let mut numbers = Vec::new();
    for branch in ["main", "next"] {
        let (status, _, body) = h
            .send("POST", "/builds", Some(json!({"branch": branch})), &[])
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        numbers.push(body["number"].clone());
    }
    assert_eq!(numbers, vec![json!(1), json!(2)]);
    let (status, _, body) = h
        .send(
            "POST",
            "/builds",
            Some(json!({"branch": "main", "number": 7})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["number"], json!(7), "{body}");
    // Sent again where the create declares no conflict, state stores no second 7.
    h.send(
        "POST",
        "/builds",
        Some(json!({"branch": "next", "number": 7})),
        &[],
    )
    .await;
    let mut lines = vec![create_line("createBuild", None); 3];
    lines.push(create_line("createBuild", Some("repeated_key")));
    assert_eq!(capture.of("createBuild"), lines);
}

#[tokio::test]
async fn a_body_member_wins_over_the_query_parameter_of_its_name() {
    let h = repairs(Vec::new());
    let (status, _, body) = h
        .send(
            "POST",
            "/boards?name=query",
            Some(json!({"name": "body"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], json!("body"), "{body}");
}

/// Mints four employees in `contract` and answers their ids.
async fn employee_ids(contract: &str) -> Vec<String> {
    let h = support::harness(contract, Vec::new(), &HarnessOptions::default(), |rt| rt);
    let mut ids = Vec::new();
    for _ in 0..4 {
        let (status, _, body) = h
            .send("POST", "/employees", Some(json!({"email": "a@b.c"})), &[])
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        ids.push(body["id"].as_str().unwrap_or_default().to_owned());
    }
    ids
}

#[tokio::test]
async fn a_text_id_that_says_more_or_a_text_route_names_is_not_reshaped_into_digits() {
    let digits = |ids: &[String]| ids.iter().all(|id| id.chars().all(|c| c.is_ascii_digit()));
    let patterned = REPAIRS.replace(
        "properties: {id: {type: string}, email: {type: string}}",
        "properties: {id: {type: string, pattern: \"^E[0-9]{3}$\"}, email: {type: string}}",
    );
    assert_ne!(patterned, REPAIRS);
    let ids = employee_ids(&patterned).await;
    assert!(ids.iter().all(|id| id.starts_with('E')), "{ids:?}");
    let bounded = REPAIRS.replace(
        "properties: {id: {type: string}, email: {type: string}}",
        "properties: {id: {type: string, maxLength: 4}, email: {type: string}}",
    );
    let ids = employee_ids(&bounded).await;
    assert!(ids.iter().all(|id| id.len() <= 4), "{ids:?}");
    let textual = REPAIRS.replace(
        "{name: employeeId, in: path, required: true, schema: {type: integer}}",
        "{name: employeeId, in: path, required: true, schema: {type: string}}",
    );
    assert_ne!(textual, REPAIRS);
    assert!(!digits(&employee_ids(&textual).await));
}
