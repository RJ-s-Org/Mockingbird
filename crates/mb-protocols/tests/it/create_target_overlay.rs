//! An authored create target replaces tier 0's whole. A POST whose route ends in its
//! parent's key (`POST /things/{parentKey}`) is taken by the heuristics for an action
//! on that key; a description that makes it a create named by a body member keys each
//! new record by that member, never by the parent's path parameter: two creates under
//! one parent store two records, a read by key serves the stored one, and a described
//! unique key refuses a repeat from state.
#![cfg(test)]
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Things, version: "1"}
paths:
  /things/{parentKey}:
    parameters: [{name: parentKey, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createThing
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/ThingInput"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}
        "409": {description: taken, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /things/{parentKey}/{thingKey}:
    parameters:
      - {name: parentKey, in: path, required: true, schema: {type: string}}
      - {name: thingKey, in: path, required: true, schema: {type: string}}
    get:
      operationId: getThing
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}
        "404": {description: missing, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
components:
  schemas:
    Thing:
      type: object
      required: [key, name]
      properties:
        key: {type: string}
        name: {type: string}
    ThingInput:
      type: object
      required: [key, name]
      properties:
        key: {type: string}
        name: {type: string}
    Problem:
      type: object
      properties: {message: {type: string}}
"##;

fn described() -> Value {
    json!({
        "types": {"things": {"unique": [{"members": ["key"], "conflict": 409}]}},
        "operations": {"createThing": {"kind": "create", "target": {"member": "key"}}}
    })
}

fn thing(key: &str, name: &str) -> Value {
    json!({"key": key, "name": name})
}

#[test]
fn the_effective_create_target_is_the_authored_member_only() {
    let bundle = mb_import::compile(CONTRACT.as_bytes()).unwrap();
    let (base, effective, refused) = support::effective_world(&bundle, described());
    assert_eq!(refused, vec![]);
    // The heuristic took the parent's key for the target.
    assert_eq!(
        serde_json::to_value(&base.operations["createThing"].target).unwrap(),
        json!({"path_param": "parentKey"})
    );
    assert_eq!(
        serde_json::to_value(&effective.operations["createThing"].target).unwrap(),
        json!({"member": "key"})
    );
}

#[tokio::test]
async fn creates_under_one_parent_are_keyed_by_the_body_member() {
    let h = support::described(
        CONTRACT,
        described(),
        vec![thing("alpha", "First"), thing("beta", "Second")],
    );
    let (status, _, body) = h
        .send("POST", "/things/p1", Some(thing("alpha", "First")), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, body) = h
        .send("POST", "/things/p1", Some(thing("beta", "Second")), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        h.stored("things", "alpha").await.map(|r| r["name"].clone()),
        Some(json!("First"))
    );
    assert_eq!(
        h.stored("things", "beta").await.map(|r| r["name"].clone()),
        Some(json!("Second"))
    );
    assert_eq!(h.stored("things", "p1").await, None);

    // A read by key serves the stored record, with no call to the model.
    let calls = h.provider.requests().len();
    let (status, _, body) = h.send("GET", "/things/p1/alpha", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["key"], "alpha");
    assert_eq!(body["name"], "First");
    assert_eq!(h.provider.requests().len(), calls);
}

#[tokio::test]
async fn a_described_unique_key_refuses_a_repeat_from_state() {
    let h = support::described(CONTRACT, described(), vec![thing("alpha", "First")]);
    let (status, _, body) = h
        .send("POST", "/things/p1", Some(thing("alpha", "First")), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, body) = h
        .send("POST", "/things/p1", Some(thing("alpha", "Again")), &[])
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(
        h.stored("things", "alpha").await.map(|r| r["name"].clone()),
        Some(json!("First"))
    );
}
