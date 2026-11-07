//! A described action on a record a create stored in an earlier, incomplete shape (one
//! branch of an `anyOf` lacking members the read schema requires): the action keeps
//! the record its own answer gives and sets its described members on it, so reads and
//! lists see the change.
#![cfg(test)]
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support;

/// Items are created answering one of two shapes (the full `Item`, or a `Pending` one
/// that lacks what `Item` requires), and starred by an action answering the full item.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Items, version: "1"}
paths:
  /items:
    get:
      operationId: listItems
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}}
    post:
      operationId: createItem
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses:
        "201":
          description: created
          content: {application/json: {schema: {anyOf: [{$ref: "#/components/schemas/Item"}, {$ref: "#/components/schemas/Pending"}]}}}
  /items/{id}:
    get:
      operationId: getItem
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}, "404": {description: missing}}
  /items/{id}/star:
    post:
      operationId: starItem
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
components:
  schemas:
    Item:
      type: object
      required: [id, name, created]
      properties:
        id: {type: string}
        name: {type: string}
        created: {type: string}
        starred: {type: boolean}
    Pending:
      type: object
      required: [id, queued_at]
      properties:
        id: {type: string}
        queued_at: {type: string}
"##;

fn item(starred: bool) -> Value {
    json!({"id": "i1", "name": "a", "created": "2026-01-01T00:00:00Z", "starred": starred})
}

#[tokio::test]
async fn a_described_action_on_a_record_stored_incomplete_keeps_its_answer_and_change() {
    let world = json!({
        "operations": {
            "starItem": {"kind": "action", "target": {"path_param": "id"}, "changes": {"starred": {"value": true}}}
        }
    });
    let h = support::described(
        CONTRACT,
        world,
        vec![
            json!({"id": "i1", "queued_at": "2026-01-01T00:00:00Z"}),
            item(false),
            json!([item(false)]),
            item(false),
        ],
    );
    let (status, _, body) = h
        .seeded("POST", "/items", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, body) = h.send("POST", "/items/i1/star", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["starred"], json!(true), "{body}");
    let t =
        h.rt.bundle()
            .operation("getItem")
            .and_then(|o| o.resource_type.clone())
            .unwrap_or_default();
    let stored = h.stored(&t, "i1").await.unwrap_or(Value::Null);
    assert_eq!(stored["starred"], json!(true), "{stored}");
    assert_eq!(stored["name"], json!("a"), "{stored}");
    let (status, _, body) = h.send("GET", "/items/i1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["starred"], json!(true), "{body}");
    let (status, _, body) = h.send("GET", "/items", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let listed = body
        .as_array()
        .and_then(|items| items.iter().find(|i| i["id"] == json!("i1")))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(listed["starred"], json!(true), "{body}");
}
