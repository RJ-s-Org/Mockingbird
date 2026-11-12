//! An executor's answer built from a stored record that breaks the answer's schema keeps
//! the record's identity and every valid stored value: only the failing members are
//! re-drawn, at the narrowest place (inside a union's branch, a map's value, an array's
//! item), and the repair is counted (`mb_answer_repairs_total`) and kept on the request
//! record (`answer_repair`). What names the record is never re-drawn: the answer falls
//! back to the faker's, keeping the stored id where it can, and says so.
#![cfg(feature = "http-rest")]

use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness};

/// Widgets read loosely and acknowledged by a patch whose answer is `ack`, with
/// `defs` beside it.
fn contract(ack: &Value, defs: &Value) -> String {
    let mut schemas = defs.as_object().cloned().unwrap_or_default();
    schemas.insert("WidgetAck".into(), ack.clone());
    schemas.insert(
        "Widget".into(),
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "name": {"type": "string"},
                "code": {"type": "string"},
                "number": {"type": "integer"},
                "status": {"type": "string"},
                "meta": {"type": "object", "additionalProperties": true},
                "labels": {"type": "object", "additionalProperties": true},
                "codes": {"type": "object", "additionalProperties": true},
                "tags": {
                    "type": "array",
                    "items": {"type": "object", "properties": {"label": {"type": "string"}, "rank": {"type": "integer"}}}
                }
            }
        }),
    );
    let doc = json!({
        "openapi": "3.0.3",
        "info": {"title": "Widgets", "version": "1"},
        "paths": {"/widgets/{id}": {
            "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}],
            "get": {"operationId": "getWidget", "responses": {
                "200": {"description": "ok", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Widget"}}}},
                "404": {"description": "missing"}
            }},
            "patch": {"operationId": "patchWidget",
                "requestBody": {"content": {"application/json": {"schema": {"type": "object", "properties": {"name": {"type": "string"}}}}}},
                "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/WidgetAck"}}}}}
            }
        }},
        "components": {"schemas": schemas}
    });
    doc.to_string()
}

/// The patch's answer: an id starting with `w`, a three-letter upper-case code, a
/// number from 100, a color of two, lower-case tag labels and label values, and a
/// closed `meta`.
fn ack() -> Value {
    json!({
        "type": "object",
        "required": ["id", "name", "code"],
        "properties": {
            "id": {"type": "string", "pattern": "^w"},
            "name": {"type": "string"},
            "code": {"type": "string", "pattern": "^[A-Z]{3}$"},
            "number": {"type": "integer", "minimum": 100},
            "meta": {
                "type": "object",
                "additionalProperties": false,
                "properties": {"color": {"type": "string", "enum": ["red", "blue"]}, "size": {"type": "integer"}}
            },
            "labels": {"type": "object", "additionalProperties": {"type": "string", "pattern": "^[a-z]+$"}},
            "codes": {
                "type": "object",
                "patternProperties": {"^[a-z]+$": {"type": "string", "pattern": "^[A-Z]+$"}},
                "additionalProperties": false
            },
            "tags": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["label"],
                    "properties": {"label": {"type": "string", "pattern": "^[a-z]+$"}, "rank": {"type": "integer"}}
                }
            }
        }
    })
}

fn harness_for(ack: &Value, defs: &Value) -> DescribedHarness {
    let world = json!({
        "operations": {"patchWidget": {"kind": "update", "target": {"path_param": "id"}}}
    });
    support::described(&contract(ack, defs), world, Vec::new())
}

fn harness() -> DescribedHarness {
    harness_for(&ack(), &json!({}))
}

/// A widget every answer takes as it is.
fn widget() -> Value {
    json!({
        "id": "w1", "name": "a", "code": "ABC", "number": 100,
        "meta": {"color": "red", "size": 2},
        "labels": {"a": "ok"},
        "tags": [{"label": "x", "rank": 1}, {"label": "y", "rank": 2}]
    })
}

/// Holds `record`, patches its name to `b` and answers the patch's body, headers and
/// its request record's `request`.
async fn patch_held(h: &DescribedHarness, record: Value) -> (Value, HeaderMap, Value) {
    h.hold("widgets", std::slice::from_ref(&record)).await;
    patch(h, record["id"].as_str().unwrap_or_default()).await
}

/// Patches widget `id`'s name to `b`; see [`patch_held`].
async fn patch(h: &DescribedHarness, id: &str) -> (Value, HeaderMap, Value) {
    let (status, headers, body) = h
        .send(
            "PATCH",
            &format!("/widgets/{id}"),
            Some(json!({"name": "b"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(header(&headers, "x-mockingbird-source"), "state", "{body}");
    let records = h.records();
    let last = records
        .iter()
        .rev()
        .find(|r| r.operation_id.as_deref() == Some("patchWidget"))
        .map(|r| r.request.clone())
        .unwrap_or_default();
    (body, headers, last)
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
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

fn is_code(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|c| c.len() == 3 && c.chars().all(|c| c.is_ascii_uppercase()))
}

#[test]
fn a_member_breaking_the_answer_is_redrawn_and_the_rest_kept() {
    let counted = counting_repairs(async {
        let h = harness();
        let mut record = widget();
        record["code"] = json!("abc");
        let (mut body, headers, request) = patch_held(&h, record).await;
        assert!(is_code(&body["code"]), "{body}");
        // A member the record lacks is the faker's, as in any acknowledgement.
        if let Some(b) = body.as_object_mut() {
            b.remove("codes");
        }
        let mut want = widget();
        want["name"] = json!("b");
        want["code"] = body["code"].clone();
        assert_eq!(body, want);
        assert_eq!(header(&headers, "x-mockingbird-fallback-reason"), "");
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
    let (body, _, request) = patch_held(&h, record).await;
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

#[tokio::test]
async fn a_map_value_is_redrawn_under_its_key() {
    let h = harness();
    let mut record = widget();
    record["labels"] = json!({"a": "ok", "b": "BAD"});
    let (body, _, request) = patch_held(&h, record).await;
    assert_eq!(body["labels"]["a"], "ok", "{body}");
    let b = body["labels"]["b"].as_str().unwrap_or_default();
    assert!(
        !b.is_empty() && b.chars().all(|c| c.is_ascii_lowercase()),
        "{body}"
    );
    assert_eq!(
        request["answer_repair"]["members"],
        json!(["/labels/b"]),
        "{request}"
    );
}

#[tokio::test]
async fn a_member_that_cannot_be_redrawn_in_place_is_redrawn_with_its_parent() {
    // `codes` takes its keys by pattern only: its value under `x` has no schema of its
    // own to draw from, so `codes` is re-drawn, and nothing else.
    let h = harness();
    let mut record = widget();
    record["codes"] = json!({"x": "bad"});
    let (body, _, request) = patch_held(&h, record).await;
    assert_ne!(body["codes"], json!({"x": "bad"}), "{body}");
    assert_eq!(body["code"], "ABC", "{body}");
    assert_eq!(body["labels"], json!({"a": "ok"}), "{body}");
    assert_eq!(
        request["answer_repair"],
        json!({"outcome": "members_redrawn", "members": ["/codes"]}),
        "{request}"
    );
}

#[tokio::test]
async fn one_undeclared_member_is_removed() {
    let h = harness();
    let mut record = widget();
    record["meta"]["extra"] = json!(1);
    let (body, _, request) = patch_held(&h, record).await;
    assert_eq!(body["meta"], json!({"color": "red", "size": 2}), "{body}");
    assert_eq!(
        request["answer_repair"],
        json!({"outcome": "members_redrawn", "members": ["/meta/extra"]}),
        "{request}"
    );
}

#[tokio::test]
async fn several_undeclared_members_are_removed() {
    let h = harness();
    let mut record = widget();
    record["meta"]["x"] = json!(1);
    record["meta"]["y"] = json!(2);
    let (body, _, request) = patch_held(&h, record).await;
    assert_eq!(body["meta"], json!({"color": "red", "size": 2}), "{body}");
    let mut members: Vec<String> =
        serde_json::from_value(request["answer_repair"]["members"].clone()).unwrap_or_default();
    members.sort();
    assert_eq!(members, vec!["/meta/x", "/meta/y"], "{request}");
}

#[tokio::test]
async fn a_member_is_drawn_alike_for_one_record_and_apart_for_two() {
    let h = harness();
    let mut first = widget();
    first["code"] = json!("abc");
    let mut second = first.clone();
    second["id"] = json!("w2");
    h.hold("widgets", &[first, second]).await;
    let (once, _, _) = patch(&h, "w1").await;
    let (again, _, _) = patch(&h, "w1").await;
    let (other, _, _) = patch(&h, "w2").await;
    assert!(is_code(&once["code"]), "{once}");
    assert_eq!(once["code"], again["code"], "{once} {again}");
    assert!(is_code(&other["code"]), "{other}");
    assert_ne!(once["code"], other["code"], "{once} {other}");
}

#[tokio::test]
async fn a_discriminated_union_is_repaired_inside_its_branch() {
    let defs = json!({
        "Done": {
            "type": "object",
            "required": ["id", "status", "code"],
            "properties": {
                "id": {"type": "string", "pattern": "^w"},
                "status": {"type": "string", "enum": ["done"]},
                "name": {"type": "string"},
                "code": {"type": "string", "pattern": "^[A-Z]{3}$"}
            }
        },
        "Pending": {
            "type": "object",
            "required": ["id", "status"],
            "properties": {
                "id": {"type": "string", "pattern": "^w"},
                "status": {"type": "string", "enum": ["pending"]},
                "eta": {"type": "integer"}
            }
        }
    });
    let ack = json!({
        "oneOf": [{"$ref": "#/components/schemas/Done"}, {"$ref": "#/components/schemas/Pending"}],
        "discriminator": {"propertyName": "status"}
    });
    let h = harness_for(&ack, &defs);
    let record = json!({"id": "w1", "name": "a", "code": "abc", "status": "done"});
    let (body, _, request) = patch_held(&h, record).await;
    assert_eq!(body["id"], "w1", "{body}");
    assert_eq!(body["status"], "done", "{body}");
    assert_eq!(body["name"], "b", "{body}");
    assert!(is_code(&body["code"]), "{body}");
    assert_eq!(
        request["answer_repair"],
        json!({"outcome": "members_redrawn", "members": ["/code"]}),
        "{request}"
    );
}

#[test]
fn an_identity_breaking_the_answer_falls_back_and_says_so() {
    let counted = counting_repairs(async {
        let h = harness();
        let mut record = widget();
        record["id"] = json!("x1");
        let (body, headers, request) = patch_held(&h, record).await;
        // Never repaired into another id: the faker's answer, as before, and said so.
        assert_ne!(body["id"], "x1", "{body}");
        assert!(
            header(&headers, "x-mockingbird-fallback-reason").contains("stored identity"),
            "{headers:?}"
        );
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

#[tokio::test]
async fn a_number_naming_the_record_is_never_redrawn() {
    // `number` is a key reads take: one the answer refuses is not replaced in a kept
    // answer, the faker's answer stands in with the record's id.
    let h = harness();
    let mut record = widget();
    record["number"] = json!(7);
    let (body, _, request) = patch_held(&h, record).await;
    assert_eq!(
        request["answer_repair"],
        json!({"outcome": "identity_invalid", "members": []}),
        "{request}"
    );
    assert_eq!(body["id"], "w1", "{body}");
    assert!(body["number"].as_i64().is_none_or(|n| n >= 100), "{body}");
}

#[test]
fn an_unrepairable_answer_keeps_the_stored_id() {
    // At most two members: the record's three cannot be cut to fit by re-drawing one.
    let ack = json!({
        "type": "object",
        "maxProperties": 2,
        "required": ["id"],
        "properties": {
            "id": {"type": "string", "pattern": "^w"},
            "name": {"type": "string"},
            "code": {"type": "string"}
        }
    });
    let counted = counting_repairs(async move {
        let h = harness_for(&ack, &json!({}));
        let (body, headers, request) = patch_held(&h, widget()).await;
        assert_eq!(body["id"], "w1", "{body}");
        assert!(
            header(&headers, "x-mockingbird-fallback-reason").contains("record's identity"),
            "{headers:?}"
        );
        assert_eq!(
            request["answer_repair"],
            json!({"outcome": "unrepairable", "members": []}),
            "{request}"
        );
    });
    assert_eq!(
        counted,
        vec![("patchWidget".into(), "unrepairable".into(), 1)]
    );
}

#[test]
fn a_record_answering_as_held_takes_no_repair() {
    let counted = counting_repairs(async {
        let h = harness();
        let (mut body, headers, request) = patch_held(&h, widget()).await;
        if let Some(b) = body.as_object_mut() {
            b.remove("codes");
        }
        let mut want = widget();
        want["name"] = json!("b");
        assert_eq!(body, want);
        assert_eq!(header(&headers, "x-mockingbird-fallback-reason"), "");
        assert!(request.get("answer_repair").is_none(), "{request}");
    });
    assert!(counted.is_empty(), "{counted:?}");
}
