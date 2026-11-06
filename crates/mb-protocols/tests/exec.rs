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
        .send("POST", "/boxes", Some(json!({"label": "red"})), &[])
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
        .send("POST", "/deals", Some(json!({"title": "big"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h.send("DELETE", "/deals/7", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"success": true, "data": {"id": 7}}));
}

/// Items with a lifecycle, acted on by actions answering the item (`archiveItem`),
/// nothing (`starItem`), an acknowledgement (`renameItem`), the item in an envelope
/// (`pinItem`, and `boostItem` whose envelope requires what the mock does not hold),
/// and one the description says nothing of (`pingItem`).
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
  /items/{id}/ping:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: pingItem
      responses: {"200": {description: pinged, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
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
    }
}

/// Creates item `i1` (the LLM's first answer).
async fn create_i1(h: &support::DescribedHarness) {
    let (status, _, body) = h
        .send("POST", "/items", Some(json!({"name": "a"})), &[])
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

#[tokio::test]
async fn an_action_whose_record_does_not_render_falls_back() {
    let (capture, _guard) = Capture::install();
    // `boostItem` answers the item in an envelope that also requires a `receipt`,
    // which nothing the mock holds gives.
    let h = actions(vec![
        item_i1(),
        json!({"item": {"id": "i1", "name": "a", "status": "ACTIVE", "pinned": true}, "receipt": "r1"}),
    ]);
    create_i1(&h).await;
    let (status, headers, body) = h.send("POST", "/items/i1/boost", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "llm", "{body}");
    assert_eq!(
        capture.of("boostItem"),
        vec![action_line("boostItem", Some("render_failed"))]
    );
}

#[tokio::test]
async fn with_no_llm_answer_an_action_still_changes_state() {
    let (_capture, _guard) = Capture::install();
    // No answers: the create is the faker's.
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
    assert_eq!(source(&headers), "state", "{body}");
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
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json!({"query": query}).to_string()))
            .unwrap();
        super::support::send_to(router, req).await.2
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
        let made = post(
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
            }),
            "{:?}",
            capture.of("Mutation.starWidget")
        );
        let read = post(&router, "{ widget(id: \"w1\") { id name starred } }").await;
        assert_eq!(read["data"]["widget"]["starred"], json!(true), "{read}");
    }
}
