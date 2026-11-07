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
    let h = support::harness(
        CONTRACT,
        vec![json!({"id": "w1", "name": "a", "size": 3})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h
        .send("POST", "/widgets", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h.send("GET", "/widgets/w1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"id": "w1", "name": "a", "size": 3}));
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
    let (status, _, body) = h
        .send("POST", "/widgets", Some(json!({"name": "a"})), &[])
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
        .send("POST", "/widgets", Some(json!({"name": "a"})), &[])
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
            .send("POST", "/deals", Some(json!({"title": "a"})), &[])
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
            .send("POST", "/deals", Some(json!({"title": "a"})), &[])
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
        .send("POST", "/deals", Some(json!({"title": "a"})), &[])
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
        .send("POST", "/notes", Some(json!({"text": "hi"})), &[])
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
        .send("POST", "/tags", Some(json!({"label": "red"})), &[])
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
        .send("POST", "/folders", Some(json!({"name": "docs"})), &[])
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
        .send("POST", "/folders", Some(json!({"name": "docs"})), &[])
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
components:
  schemas:
    Box:
      type: object
      required: [id, label]
      properties: {id: {type: string}, label: {type: string}}
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
        .send("POST", "/boxes", Some(json!({"label": "red"})), &[])
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
async fn an_answer_sharing_nothing_with_the_record_falls_back() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"binBox": {"kind": "delete", "type": "boxes"}}});
    let h = a_box(world).await;
    h.send("DELETE", "/boxes/b1/said", None, &[]).await;
    assert_eq!(
        capture.of("binBox"),
        vec![ExecLine {
            operation: "binBox".into(),
            exec: "delete".into(),
            exec_fallback: Some("render_failed".into()),
            render_error: None,
        }]
    );
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
        .send("POST", "/notes", Some(json!({"text": "hi"})), &[])
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
