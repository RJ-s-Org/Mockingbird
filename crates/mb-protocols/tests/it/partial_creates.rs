//! A create whose answer carries the record's identity beside members of its own, and
//! not the record: Plaid's `/item/public_token/exchange` answers `{access_token,
//! item_id, request_id}` while `/item/get` reads the whole Item. The record is still
//! stored, its other members filled, and read back from state.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support;

pub(crate) const CONTRACT: &str = r##"
openapi: 3.1.0
info: {title: Items, version: "1"}
paths:
  /item/public_token/exchange:
    post:
      operationId: exchange
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [public_token]
              properties: {public_token: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [access_token, item_id, request_id]
                additionalProperties: false
                properties:
                  access_token: {type: string}
                  item_id: {type: string}
                  request_id: {type: string}
  /item/get:
    post:
      operationId: itemGet
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              properties: {access_token: {type: string}, item_id: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/ItemGetResponse"}
        "400":
          description: bad
          content:
            application/json:
              schema:
                type: object
                properties: {error_code: {type: string}, error_message: {type: string}}
components:
  schemas:
    ItemGetResponse:
      type: object
      required: [item, request_id]
      additionalProperties: false
      properties:
        item: {$ref: "#/components/schemas/Item"}
        request_id: {type: string}
    Item:
      type: object
      required: [item_id, institution_id, products]
      properties:
        item_id: {type: string}
        institution_id: {type: string}
        products: {type: array, items: {type: string}}
"##;

pub(crate) fn world() -> Value {
    json!({
        "types": {"item": {"identity": {"member": "item_id"}}},
        "operations": {
            "exchange": {"kind": "create", "type": "item", "answer": {"record": ""}},
            "itemGet": {"kind": "read", "type": "item", "target": {"member": "item_id"},
                        "answer": {"record": "/item"}}
        }
    })
}

#[tokio::test]
async fn a_create_answering_only_the_identity_and_its_own_members_stores_the_record() {
    let fill = json!({"institution_id": "ins_1", "products": ["auth"]});
    let h = support::described(CONTRACT, world(), vec![fill]);
    let (status, headers, body) = h
        .send(
            "POST",
            "/item/public_token/exchange",
            Some(json!({"public_token": "public-sandbox-1"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-mockingbird-source"], "state", "{body}");
    let id = body["item_id"].as_str().unwrap_or_default().to_owned();
    assert!(!id.is_empty(), "{body}");
    // The answer keeps its own closed shape: no Item member leaks into it.
    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["access_token", "item_id", "request_id"], "{body}");
    let stored = h
        .stored("item", &id)
        .await
        .unwrap_or_else(|| panic!("not stored: {body}"));
    assert_eq!(stored["item_id"], json!(id), "{stored}");
    // An id-like member is the faker's; the free-text `products` the model's.
    assert!(stored["institution_id"].is_string(), "{stored}");
    assert_eq!(stored["products"], json!(["auth"]), "{stored}");

    // Read back from state, in the read's own envelope.
    let (status, headers, body) = h
        .send("POST", "/item/get", Some(json!({"item_id": id})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-mockingbird-source"], "state", "{body}");
    assert_eq!(body["item"], stored, "{body}");
    assert!(body["request_id"].is_string(), "{body}");
}
