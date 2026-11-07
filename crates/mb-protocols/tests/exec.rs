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

/// Items with a lifecycle, acted on by actions answering the item (`archiveItem`),
/// nothing (`starItem`), an acknowledgement (`renameItem`), the item in an envelope
/// (`pinItem`, and `boostItem` whose envelope requires what the mock does not hold),
/// and two the description sets nothing for (`pingItem`, and `nudgeItem` answering nothing).
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
    // The fill's answer: a value for what the request left out that a read requires
    // (`size`); an optional member (`color`), an id and a name it never takes.
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
        json!({"id": id, "name": "a", "size": 3}),
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
    // The fill's answer: the `status` the read requires inside `fields`, and a
    // `summary` it never takes (the request sent one).
    let h = support::harness(
        NESTED,
        vec![json!({"fields": {"status": "open", "summary": "not sent"}})],
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
        json!({"id": id, "fields": {"summary": "printer jam", "status": "open"}})
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
async fn a_required_member_its_schema_fixes_takes_that_value() {
    let (capture, _guard) = Capture::install();
    let h = support::described(IDS, json!({}), Vec::new());
    let (status, headers, body) = h
        .send("POST", "/gauges", Some(json!({"name": "g"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    // `kind` is required and fixed; the optional one-value `unit` is never invented.
    assert_eq!(body, json!({"id": id, "name": "g", "kind": "gauge"}));
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
