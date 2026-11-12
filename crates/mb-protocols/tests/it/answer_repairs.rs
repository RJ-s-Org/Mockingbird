//! An executor's answer built from a stored record that breaks the answer's schema keeps
//! the record's identity and every valid stored value: only the failing members are
//! re-drawn, at the narrowest place, and the repair is counted
//! (`mb_answer_repairs_total`) and kept on the request record (`answer_repair`). A
//! stored identity that breaks the schema is never re-drawn: the answer falls back to
//! the faker's, as before, and says so.
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness};

/// Widgets read loosely and acknowledged by a patch whose answer is stricter: an id
/// starting with `w`, a three-letter upper-case code, a color of two and lower-case tag
/// labels.
const WIDGETS: &str = r##"
openapi: 3.0.3
info: {title: Widgets, version: "1"}
paths:
  /widgets/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getWidget
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
        "404": {description: missing}
    patch:
      operationId: patchWidget
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/WidgetAck"}}}}
components:
  schemas:
    Widget:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
        code: {type: string}
        meta: {type: object, properties: {color: {type: string}, size: {type: integer}}}
        tags:
          type: array
          items: {type: object, properties: {label: {type: string}, rank: {type: integer}}}
    WidgetAck:
      type: object
      required: [id, name, code]
      properties:
        id: {type: string, pattern: "^w"}
        name: {type: string}
        code: {type: string, pattern: "^[A-Z]{3}$"}
        meta:
          type: object
          properties:
            color: {type: string, enum: [red, blue]}
            size: {type: integer}
        tags:
          type: array
          items:
            type: object
            required: [label]
            properties:
              label: {type: string, pattern: "^[a-z]+$"}
              rank: {type: integer}
"##;

fn harness() -> DescribedHarness {
    let world = json!({
        "operations": {"patchWidget": {"kind": "update", "target": {"path_param": "id"}}}
    });
    support::described(WIDGETS, world, Vec::new())
}

/// A widget every answer takes as it is.
fn widget() -> Value {
    json!({
        "id": "w1", "name": "a", "code": "ABC",
        "meta": {"color": "red", "size": 2},
        "tags": [{"label": "x", "rank": 1}, {"label": "y", "rank": 2}]
    })
}

/// Holds `record`, patches its name to `b` and answers the patch's body and its
/// request record's `request`.
async fn patch(h: &DescribedHarness, record: Value) -> (Value, Value) {
    let id = record["id"].as_str().unwrap_or_default().to_owned();
    h.hold("widgets", &[record]).await;
    let (status, headers, body) = h
        .send(
            "PATCH",
            &format!("/widgets/{id}"),
            Some(json!({"name": "b"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers
            .get("x-mockingbird-source")
            .and_then(|v| v.to_str().ok()),
        Some("state"),
        "{body}"
    );
    let records = h.records();
    let mine: Vec<_> = records
        .iter()
        .filter(|r| r.operation_id.as_deref() == Some("patchWidget"))
        .collect();
    assert_eq!(mine.len(), 1, "{records:?}");
    (body, mine[0].request.clone())
}

/// Runs `test` on this thread with a metrics recorder; answers each repair it
/// counted as `(operation, outcome, count)`.
fn counting_repairs<F: std::future::Future<Output = ()>>(test: F) -> Vec<(String, String, u64)> {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(test);
    });
    let mut counted = Vec::new();
    for (key, _, _, value) in snapshotter.snapshot().into_vec() {
        if key.key().name() != "mb_answer_repairs_total" {
            continue;
        }
        let label = |name: &str| {
            key.key()
                .labels()
                .find(|l| l.key() == name)
                .map(|l| l.value().to_string())
                .unwrap_or_default()
        };
        assert!(!label("mock").is_empty());
        let DebugValue::Counter(n) = value else {
            panic!("a counter");
        };
        counted.push((label("operation"), label("outcome"), n));
    }
    counted
}

#[test]
fn a_member_breaking_the_answer_is_redrawn_and_the_rest_kept() {
    let counted = counting_repairs(async {
        let h = harness();
        let mut record = widget();
        record["code"] = json!("abc");
        let (body, request) = patch(&h, record).await;
        let mut want = widget();
        want["name"] = json!("b");
        let code = body["code"].as_str().unwrap_or_default().to_owned();
        assert!(
            code.len() == 3 && code.chars().all(|c| c.is_ascii_uppercase()),
            "{body}"
        );
        want["code"] = json!(code);
        assert_eq!(body, want);
        assert_eq!(
            request["answer_repair"],
            json!({"outcome": "members_redrawn", "members": ["/code"]}),
            "{request}"
        );
    });
    assert_eq!(
        counted,
        vec![("patchWidget".into(), "members_redrawn".into(), 1)]
    );
}

#[tokio::test]
async fn a_nested_member_and_an_item_member_are_redrawn_narrowly() {
    let h = harness();
    let mut record = widget();
    record["meta"]["color"] = json!("green");
    record["tags"][1]["label"] = json!("BAD");
    let (body, request) = patch(&h, record).await;
    assert_eq!(body["id"], "w1", "{body}");
    assert_eq!(body["name"], "b", "{body}");
    assert_eq!(body["code"], "ABC", "{body}");
    assert_eq!(body["meta"]["size"], 2, "{body}");
    assert!(
        ["red", "blue"].contains(&body["meta"]["color"].as_str().unwrap_or_default()),
        "{body}"
    );
    assert_eq!(body["tags"][0], json!({"label": "x", "rank": 1}), "{body}");
    assert_eq!(body["tags"][1]["rank"], 2, "{body}");
    let label = body["tags"][1]["label"].as_str().unwrap_or_default();
    assert!(
        !label.is_empty() && label.chars().all(|c| c.is_ascii_lowercase()),
        "{body}"
    );
    assert_eq!(request["answer_repair"]["outcome"], "members_redrawn");
    assert_eq!(
        request["answer_repair"]["members"],
        json!(["/meta/color", "/tags/1/label"]),
        "{request}"
    );
}

#[test]
fn an_identity_breaking_the_answer_falls_back_and_says_so() {
    let counted = counting_repairs(async {
        let h = harness();
        let mut record = widget();
        record["id"] = json!("x1");
        let (body, request) = patch(&h, record).await;
        // Never repaired into another id: the faker's answer, as before.
        assert_ne!(body["id"], "x1", "{body}");
        assert_eq!(
            request["answer_repair"],
            json!({"outcome": "identity_invalid", "members": []}),
            "{request}"
        );
    });
    assert_eq!(
        counted,
        vec![("patchWidget".into(), "identity_invalid".into(), 1)]
    );
}

#[test]
fn a_record_answering_as_held_takes_no_repair() {
    let counted = counting_repairs(async {
        let h = harness();
        let (body, request) = patch(&h, widget()).await;
        let mut want = widget();
        want["name"] = json!("b");
        assert_eq!(body, want);
        assert!(request.get("answer_repair").is_none(), "{request}");
    });
    assert!(counted.is_empty(), "{counted:?}");
}
