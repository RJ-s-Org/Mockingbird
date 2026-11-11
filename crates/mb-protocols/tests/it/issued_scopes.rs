//! Lists scoped by a value the server issued the items' parent (Plaid's `access_token`
//! naming an Item, whose accounts `/accounts/get` and `/auth/get` both list): a scope
//! with no member. Every list of the type with that scope shares the parent's
//! collection, answered from state once a first list (generated) marked it; another
//! parent's records stay out; a value never issued answers the declared not-found.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support;

const CONTRACT: &str = r##"
openapi: 3.1.0
info: {title: Items, version: "1"}
paths:
  /item/public_token/exchange:
    post:
      operationId: exchange
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [public_token], properties: {public_token: {type: string}}}}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [access_token, item_id]
                additionalProperties: false
                properties: {access_token: {type: string}, item_id: {type: string}}
  /item/get:
    post:
      operationId: itemGet
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/TokenRequest"}}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [item]
                properties: {item: {$ref: "#/components/schemas/Item"}}
  /accounts/get:
    post:
      operationId: accountsGet
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/TokenRequest"}}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [accounts]
                properties: {accounts: {type: array, items: {$ref: "#/components/schemas/Account"}}}
        "400": {$ref: "#/components/responses/Error"}
  /auth/get:
    post:
      operationId: authGet
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/TokenRequest"}}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [accounts]
                properties: {accounts: {type: array, items: {$ref: "#/components/schemas/Account"}}}
        "400": {$ref: "#/components/responses/Error"}
components:
  responses:
    Error:
      description: bad
      content: {application/json: {schema: {type: object, properties: {error_code: {type: string}}}}}
  schemas:
    TokenRequest: {type: object, required: [access_token], properties: {access_token: {type: string}}}
    Item:
      type: object
      required: [item_id, institution_id]
      properties: {item_id: {type: string}, institution_id: {type: string}}
    Account:
      type: object
      required: [account_id, name]
      properties: {account_id: {type: string}, name: {type: string}}
"##;

fn world() -> Value {
    json!({
        "types": {
            "item": {"identity": {"member": "item_id", "alternates": ["access_token"]}},
            "account": {"identity": {"member": "account_id"}, "parent": {"type": "item"}}
        },
        "operations": {
            "exchange": {"kind": "create", "type": "item", "answer": {"record": ""}},
            "itemGet": {"kind": "read", "type": "item", "target": {"member": "access_token"},
                        "answer": {"record": "/item"}},
            "accountsGet": {"kind": "list", "type": "account", "answer": {"records": "/accounts"},
                            "scope": {"param": "access_token"}, "not_found": 400},
            "authGet": {"kind": "list", "type": "account", "answer": {"records": "/accounts"},
                        "scope": {"param": "access_token"}, "not_found": 400}
        }
    })
}

async fn token(h: &support::DescribedHarness) -> String {
    let (status, _, body) = h
        .send(
            "POST",
            "/item/public_token/exchange",
            Some(json!({"public_token": "p"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["access_token"].as_str().unwrap_or_default().to_owned()
}

async fn list(h: &support::DescribedHarness, path: &str, token: &str) -> (String, Vec<Value>) {
    let (status, headers, body) = h
        .send("POST", path, Some(json!({"access_token": token})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    let source = headers["x-mockingbird-source"]
        .to_str()
        .unwrap_or_default()
        .to_owned();
    let ids = body["accounts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|a| a["account_id"].clone())
        .collect();
    (source, ids)
}

#[tokio::test]
async fn lists_scoped_by_an_issued_token_share_its_parents_records() {
    let script = vec![
        json!({"institution_id": "ins_1"}),
        json!({"accounts": [{"account_id": "a1", "name": "Checking"}, {"account_id": "a2", "name": "Saving"}]}),
        json!({"institution_id": "ins_2"}),
        json!({"accounts": [{"account_id": "b1", "name": "Card"}]}),
    ];
    let h = support::described(CONTRACT, world(), script);
    let first = token(&h).await;
    let (source, ids) = list(&h, "/accounts/get", &first).await;
    assert_ne!(source, "state");
    assert_eq!(ids, [json!("a1"), json!("a2")]);
    // Every list of the type with the token answers its item's accounts from state.
    for path in ["/accounts/get", "/auth/get"] {
        let (source, again) = list(&h, path, &first).await;
        assert_eq!((source.as_str(), &again), ("state", &ids), "{path}");
    }
    // Another item's accounts are its own.
    let second = token(&h).await;
    let (_, other) = list(&h, "/accounts/get", &second).await;
    assert_eq!(other, [json!("b1")]);
    let (source, again) = list(&h, "/auth/get", &second).await;
    assert_eq!((source.as_str(), &again), ("state", &other));
    let (_, again) = list(&h, "/auth/get", &first).await;
    assert_eq!(again, ids);
    // A token never issued names no item.
    let (status, _, body) = h
        .send(
            "POST",
            "/accounts/get",
            Some(json!({"access_token": "nope"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
