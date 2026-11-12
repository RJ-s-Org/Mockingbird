//! A generated list whose records sit beside other arrays of records with no standard
//! envelope key (Plaid's `/transactions/sync`: `added`, `modified`, `removed` and
//! `accounts`, a cursor and `has_more`). The records at the described `answer.records`
//! are stored under the list's type, and the records of another described type beside
//! them (`accounts`) under theirs, so a later read of either is answered from state. A
//! record's reference to another type (`account_id`) names a record of that type the
//! answer holds or the session holds.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support;

const CONTRACT: &str = r##"
openapi: 3.1.0
info: {title: Sync, version: "1"}
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
                properties: {access_token: {$ref: "#/components/schemas/AccessToken"}, item_id: {$ref: "#/components/schemas/ItemId"}}
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
                required: [accounts, request_id]
                properties:
                  accounts: {type: array, items: {$ref: "#/components/schemas/Account"}}
                  request_id: {type: string}
        "400": {$ref: "#/components/responses/Error"}
  /accounts/{account_id}:
    get:
      operationId: accountGet
      parameters: [{name: account_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Account"}}}
        "404": {description: missing}
  /transactions/{transaction_id}:
    get:
      operationId: transactionGet
      parameters: [{name: transaction_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Transaction"}}}
        "404": {description: missing}
  /transactions/sync:
    post:
      operationId: transactionsSync
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
                required: [accounts, added, modified, removed, next_cursor, has_more, request_id]
                properties:
                  accounts: {type: array, items: {$ref: "#/components/schemas/Account"}}
                  added: {type: array, items: {$ref: "#/components/schemas/Transaction"}}
                  modified: {type: array, items: {$ref: "#/components/schemas/Transaction"}}
                  removed:
                    type: array
                    items:
                      type: object
                      required: [transaction_id, account_id]
                      properties: {transaction_id: {type: string}, account_id: {type: string}}
                  next_cursor: {type: string}
                  has_more: {type: boolean}
                  request_id: {type: string}
        "400": {$ref: "#/components/responses/Error"}
components:
  responses:
    Error:
      description: bad
      content: {application/json: {schema: {type: object, properties: {error_code: {type: string}}}}}
  schemas:
    TokenRequest: {type: object, required: [access_token], properties: {access_token: {type: string}, cursor: {type: string}}}
    AccessToken: {type: string, pattern: "^access-[a-z0-9]{16}$"}
    ItemId: {type: string, pattern: "^item-[a-z0-9]{16}$"}
    Item:
      type: object
      required: [item_id]
      properties: {item_id: {$ref: "#/components/schemas/ItemId"}, institution_id: {type: string}}
    Account:
      type: object
      required: [account_id, name]
      properties: {account_id: {type: string}, name: {type: string}}
    Transaction:
      type: object
      required: [transaction_id, account_id, amount]
      properties: {transaction_id: {type: string}, account_id: {type: string}, amount: {type: number}}
"##;

fn world() -> Value {
    json!({
        "types": {
            "item": {"identity": {"member": "item_id", "alternates": ["access_token"]}},
            "account": {"identity": {"member": "account_id"}, "parent": {"type": "item"}},
            "transaction": {"identity": {"member": "transaction_id"}, "parent": {"type": "item"}}
        },
        "operations": {
            "exchange": {"kind": "create", "type": "item", "answer": {"record": ""}},
            "itemGet": {"kind": "read", "type": "item", "target": {"member": "access_token"},
                        "answer": {"record": "/item"}},
            "accountsGet": {"kind": "list", "type": "account", "answer": {"records": "/accounts"},
                            "scope": {"param": "access_token"}, "not_found": 400},
            "accountGet": {"kind": "read", "type": "account"},
            "transactionGet": {"kind": "read", "type": "transaction"},
            "transactionsSync": {"kind": "list", "type": "transaction", "answer": {"records": "/added"},
                                 "scope": {"param": "access_token"}, "not_found": 400}
        }
    })
}

/// A sync answer: two added transactions (one naming no account the answer or the
/// session holds), one modified, one removed, and the Item's accounts.
fn sync_answer() -> Value {
    json!({
        "accounts": [{"account_id": "acc-1", "name": "Checking"},
                     {"account_id": "acc-2", "name": "Savings"}],
        "added": [{"transaction_id": "txn-1", "account_id": "acc-2", "amount": 1.5},
                  {"transaction_id": "txn-2", "account_id": "acc-nowhere", "amount": 2.5}],
        "modified": [{"transaction_id": "txn-mod", "account_id": "acc-1", "amount": 3.5}],
        "removed": [{"transaction_id": "txn-gone", "account_id": "acc-1"}],
        "next_cursor": "c1",
        "has_more": false,
        "request_id": "r"
    })
}

async fn send(
    h: &support::DescribedHarness,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (String, Value) {
    let (status, headers, body) = h.send(method, path, body, &[]).await;
    assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
    let source = headers["x-mockingbird-source"]
        .to_str()
        .unwrap_or_default()
        .to_owned();
    (source, body)
}

/// The `account_id` of each record in `members` of `page`.
fn accounts_named(page: &Value, members: &[&str]) -> Vec<String> {
    members
        .iter()
        .flat_map(|m| page[*m].as_array().cloned().unwrap_or_default())
        .filter_map(|record| record["account_id"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn a_sync_answers_records_beside_other_arrays_are_stored_and_name_its_accounts() {
    let h = support::described(CONTRACT, world(), vec![json!({}), sync_answer()]);
    let (_, exchanged) = send(
        &h,
        "POST",
        "/item/public_token/exchange",
        Some(json!({"public_token": "p"})),
    )
    .await;
    let token = exchanged["access_token"].clone();
    let (source, first) = send(
        &h,
        "POST",
        "/transactions/sync",
        Some(json!({"access_token": token})),
    )
    .await;
    assert_ne!(source, "state", "{first}");
    let listed = accounts_named(&first, &["accounts"]);
    assert_eq!(listed, ["acc-1", "acc-2"], "{first}");
    // Every reference names an account the answer lists.
    let named = accounts_named(&first, &["added", "modified", "removed"]);
    assert_eq!(named.len(), 4, "{first}");
    assert!(named.iter().all(|id| listed.contains(id)), "{first}");
    assert_eq!(first["added"][0]["account_id"], json!("acc-2"), "{first}");
    // The added transactions are stored: a read of one answers it from state.
    for added in first["added"].as_array().cloned().unwrap_or_default() {
        let id = added["transaction_id"].as_str().unwrap_or_default();
        let (source, read) = send(&h, "GET", &format!("/transactions/{id}"), None).await;
        assert_eq!(source, "state", "{read}");
        assert_eq!(read, added, "{read}");
    }
    // So are the accounts beside them: each account a transaction names is held.
    for id in &named {
        let (source, read) = send(&h, "GET", &format!("/accounts/{id}"), None).await;
        assert_eq!(source, "state", "{read}");
        assert_eq!(read["account_id"], json!(id), "{read}");
    }
    // The sync's collection was listed: the next sync is answered from state.
    let (source, again) = send(
        &h,
        "POST",
        "/transactions/sync",
        Some(json!({"access_token": token})),
    )
    .await;
    assert_eq!(source, "state", "{again}");
    assert_eq!(again["added"], first["added"], "{again}");
    // Its accounts are the held ones its transactions name, not drawn ones, and what
    // the drawn records beside them name is one of those.
    let held = accounts_named(&again, &["accounts"]);
    assert_eq!(held, ["acc-2"], "{again}");
    let named = accounts_named(&again, &["added", "modified", "removed"]);
    assert!(named.iter().all(|id| held.contains(id)), "{again}");
}

#[tokio::test]
async fn a_syncs_references_name_accounts_the_session_holds() {
    let accounts = json!({"accounts": [{"account_id": "held-1", "name": "Checking"},
                                       {"account_id": "held-2", "name": "Savings"}],
                          "request_id": "r"});
    // The sync lists no account of its own: its transactions name the held ones.
    let mut sync = sync_answer();
    sync["accounts"] = json!([]);
    let h = support::described(CONTRACT, world(), vec![json!({}), accounts, sync]);
    let (_, exchanged) = send(
        &h,
        "POST",
        "/item/public_token/exchange",
        Some(json!({"public_token": "p"})),
    )
    .await;
    let token = exchanged["access_token"].clone();
    let _ = send(
        &h,
        "POST",
        "/accounts/get",
        Some(json!({"access_token": token})),
    )
    .await;
    let (_, first) = send(
        &h,
        "POST",
        "/transactions/sync",
        Some(json!({"access_token": token})),
    )
    .await;
    let named = accounts_named(&first, &["added", "modified", "removed"]);
    assert_eq!(named.len(), 4, "{first}");
    assert!(
        named.iter().all(|id| id == "held-1" || id == "held-2"),
        "{first}"
    );
    for added in first["added"].as_array().cloned().unwrap_or_default() {
        let id = added["transaction_id"].as_str().unwrap_or_default();
        let (source, read) = send(&h, "GET", &format!("/transactions/{id}"), None).await;
        assert_eq!(source, "state", "{read}");
        let account = read["account_id"].as_str().unwrap_or_default();
        let (source, _) = send(&h, "GET", &format!("/accounts/{account}"), None).await;
        assert_eq!(source, "state", "{read}");
    }
}

/// [`CONTRACT`] with no read of a transaction (as Plaid's): the sync's records are keyed
/// by the type's described identity member, `transaction_id`.
fn without_transaction_read() -> String {
    let start = CONTRACT
        .find("  /transactions/{transaction_id}:")
        .unwrap_or_default();
    let end = CONTRACT.find("  /transactions/sync:").unwrap_or_default();
    format!("{}{}", &CONTRACT[..start], &CONTRACT[end..])
}

#[tokio::test]
async fn a_sync_with_no_read_of_its_records_is_answered_from_state_next() {
    let contract = without_transaction_read();
    assert!(!contract.contains("transactionGet"), "{contract}");
    let mut world = world();
    if let Some(ops) = world["operations"].as_object_mut() {
        ops.remove("transactionGet");
    }
    let h = support::described(&contract, world, vec![json!({}), sync_answer()]);
    let (_, exchanged) = send(
        &h,
        "POST",
        "/item/public_token/exchange",
        Some(json!({"public_token": "p"})),
    )
    .await;
    let token = exchanged["access_token"].clone();
    let (source, first) = send(
        &h,
        "POST",
        "/transactions/sync",
        Some(json!({"access_token": token})),
    )
    .await;
    assert_ne!(source, "state", "{first}");
    let (source, again) = send(
        &h,
        "POST",
        "/transactions/sync",
        Some(json!({"access_token": token})),
    )
    .await;
    assert_eq!(source, "state", "{again}");
    assert_eq!(again["added"], first["added"], "{again}");
    for id in accounts_named(&again, &["added"]) {
        let (source, read) = send(&h, "GET", &format!("/accounts/{id}"), None).await;
        assert_eq!(source, "state", "{read}");
    }
}
