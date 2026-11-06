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
    let (_capture, _guard) = Capture::install();
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
    assert_eq!(capture.of("getWidget"), vec![exec(Some("render_failed"))]);
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
