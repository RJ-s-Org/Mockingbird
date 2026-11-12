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

#[tokio::test]
async fn a_syncs_references_name_only_accounts_its_own_item_listed() {
    let listing = |ids: [&str; 2]| {
        json!({"accounts": [{"account_id": ids[0], "name": "Checking"},
                            {"account_id": ids[1], "name": "Savings"}],
               "request_id": "r"})
    };
    // The second Item's sync names an account of the first Item, which the session
    // holds, and one of its own.
    let mut sync = sync_answer();
    sync["accounts"] = json!([]);
    sync["added"] = json!([{"transaction_id": "txn-1", "account_id": "a-1", "amount": 1.5},
                           {"transaction_id": "txn-2", "account_id": "b-2", "amount": 2.5}]);
    let h = support::described(
        CONTRACT,
        world(),
        vec![
            json!({}),
            listing(["a-1", "a-2"]),
            listing(["b-1", "b-2"]),
            sync,
        ],
    );
    let mut tokens = Vec::new();
    for _ in 0..2 {
        let (_, exchanged) = send(
            &h,
            "POST",
            "/item/public_token/exchange",
            Some(json!({"public_token": "p"})),
        )
        .await;
        tokens.push(exchanged["access_token"].clone());
    }
    assert_ne!(tokens[0], tokens[1]);
    for token in &tokens {
        let _ = send(
            &h,
            "POST",
            "/accounts/get",
            Some(json!({"access_token": token})),
        )
        .await;
    }
    let (_, synced) = send(
        &h,
        "POST",
        "/transactions/sync",
        Some(json!({"access_token": tokens[1]})),
    )
    .await;
    let named = accounts_named(&synced, &["added", "modified", "removed"]);
    assert_eq!(named.len(), 4, "{synced}");
    assert!(
        named.iter().all(|id| id == "b-1" || id == "b-2"),
        "{synced}"
    );
    assert_eq!(synced["added"][1]["account_id"], json!("b-2"), "{synced}");
}

/// [`CONTRACT`] with each `account_id` an integer.
fn integer_accounts() -> String {
    CONTRACT
        .replace(
            "required: [account_id, name]\n      properties: {account_id: {type: string}",
            "required: [account_id, name]\n      properties: {account_id: {type: integer}",
        )
        .replace(
            "properties: {transaction_id: {type: string}, account_id: {type: string}, amount",
            "properties: {transaction_id: {type: string}, account_id: {type: integer}, amount",
        )
        .replace(
            "properties: {transaction_id: {type: string}, account_id: {type: string}}",
            "properties: {transaction_id: {type: string}, account_id: {type: integer}}",
        )
}

#[tokio::test]
async fn a_syncs_integer_references_name_the_items_listed_accounts() {
    let contract = integer_accounts();
    assert_eq!(contract.matches("account_id: {type: integer}").count(), 3);
    let accounts = json!({"accounts": [{"account_id": 11, "name": "Checking"},
                                       {"account_id": 12, "name": "Savings"}],
                          "request_id": "r"});
    // The sync draws an account of its own beside its transactions, and names it.
    let sync = json!({
        "accounts": [{"account_id": 91, "name": "Drawn"}],
        "added": [{"transaction_id": "txn-1", "account_id": 91, "amount": 1.5},
                  {"transaction_id": "txn-2", "account_id": 92, "amount": 2.5}],
        "modified": [{"transaction_id": "txn-mod", "account_id": 91, "amount": 3.5}],
        "removed": [{"transaction_id": "txn-gone", "account_id": 92}],
        "next_cursor": "c1", "has_more": false, "request_id": "r"
    });
    let h = support::described(&contract, world(), vec![json!({}), accounts, sync]);
    let (_, exchanged) = send(
        &h,
        "POST",
        "/item/public_token/exchange",
        Some(json!({"public_token": "p"})),
    )
    .await;
    let token = json!({"access_token": exchanged["access_token"]});
    let _ = send(&h, "POST", "/accounts/get", Some(token.clone())).await;
    let (_, synced) = send(&h, "POST", "/transactions/sync", Some(token)).await;
    let held = [json!(11), json!(12)];
    let named: Vec<Value> = ["accounts", "added", "modified", "removed"]
        .iter()
        .flat_map(|m| synced[*m].as_array().cloned().unwrap_or_default())
        .map(|record| record["account_id"].clone())
        .collect();
    assert_eq!(named.len(), 6, "{synced}");
    assert!(named.iter().all(|id| held.contains(id)), "{synced}");
    // The transactions stored name them too, and no drawn account was stored.
    for id in ["txn-1", "txn-2"] {
        let stored = h.stored("transaction", id).await.unwrap_or_default();
        assert!(held.contains(&stored["account_id"]), "{stored}");
    }
    let stored = h.stored_ids("account").await;
    assert_eq!(stored, ["11", "12"], "{stored:?}");
}

/// [`CONTRACT`] with a sync answering at most one account beside its transactions.
fn one_account_beside() -> String {
    let from = "                  accounts: {type: array, items: {$ref: \"#/components/schemas/Account\"}}\n                  added:";
    let to = "                  accounts: {type: array, maxItems: 1, items: {$ref: \"#/components/schemas/Account\"}}\n                  added:";
    CONTRACT.replace(from, to)
}

#[tokio::test]
async fn a_sync_that_keeps_its_drawn_accounts_names_them() {
    let contract = one_account_beside();
    assert!(contract.contains("maxItems: 1"), "{contract}");
    let accounts = json!({"accounts": [{"account_id": "held-1", "name": "Checking"},
                                       {"account_id": "held-2", "name": "Savings"}],
                          "request_id": "r"});
    // The Item's two accounts do not fit the sync's one: it keeps the one it drew.
    let mut sync = sync_answer();
    sync["accounts"] = json!([{"account_id": "acc-drawn", "name": "Checking"}]);
    let h = support::described(&contract, world(), vec![json!({}), accounts, sync]);
    let (_, exchanged) = send(
        &h,
        "POST",
        "/item/public_token/exchange",
        Some(json!({"public_token": "p"})),
    )
    .await;
    let token = json!({"access_token": exchanged["access_token"]});
    let _ = send(&h, "POST", "/accounts/get", Some(token.clone())).await;
    let (_, synced) = send(&h, "POST", "/transactions/sync", Some(token)).await;
    assert_eq!(
        accounts_named(&synced, &["accounts"]),
        ["acc-drawn"],
        "{synced}"
    );
    // The answer agrees with itself: each reference names the account it lists.
    let named = accounts_named(&synced, &["added", "modified", "removed"]);
    assert_eq!(named.len(), 4, "{synced}");
    assert!(named.iter().all(|id| id == "acc-drawn"), "{synced}");
}

/// The catalog's Plaid example: its own contract and world.
const PLAID: &str = include_str!("../../../../examples/apis/plaid/plaid.openapi.yaml");
const PLAID_WORLD: &str = include_str!("../../../../examples/apis/plaid/plaid.world.yaml");

/// A Plaid id (37 base62 characters) from `stem`.
fn plaid_id(stem: &str) -> String {
    format!("{stem:0<37}")
}

fn plaid_account(id: &str, name: &str, kind: &str, subtype: &str, mask: &str) -> Value {
    json!({"account_id": id, "name": name, "official_name": null, "type": kind,
           "subtype": subtype, "mask": mask,
           "balances": {"available": 100.0, "current": 110.0, "limit": null,
                        "iso_currency_code": "USD", "unofficial_currency_code": null}})
}

fn plaid_transaction(id: &str, account: &str, amount: f64) -> Value {
    json!({"transaction_id": id, "account_id": account, "amount": amount,
           "iso_currency_code": "USD", "unofficial_currency_code": null,
           "date": "2026-11-01", "authorized_date": "2026-11-01",
           "datetime": "2026-11-01T10:00:00Z", "authorized_datetime": "2026-11-01T10:00:00Z",
           "name": "Starbucks", "merchant_name": "Starbucks", "pending": false,
           "pending_transaction_id": null, "account_owner": null, "payment_channel": "in store",
           "payment_meta": {"reference_number": null, "ppd_id": null, "payee": null,
                            "by_order_of": null, "payer": null, "payment_method": null,
                            "payment_processor": null, "reason": null},
           "location": {"address": null, "city": null, "region": null, "postal_code": null,
                        "country": null, "lat": null, "lon": null, "store_number": null},
           "personal_finance_category": null, "transaction_code": null})
}

fn plaid_item() -> Value {
    json!({"item_id": plaid_id("Item"), "institution_id": "ins_109508", "webhook": null,
           "error": null, "available_products": ["balance"], "billed_products": ["auth", "transactions"],
           "consent_expiration_time": null, "update_type": "background"})
}

/// Sends `body` to `path` of the catalog's Plaid mock: its answer, which must be 200.
async fn plaid_post(h: &support::DescribedHarness, path: &str, body: Value) -> Value {
    let (status, _, body) = h.send("POST", path, Some(body), &[]).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    body
}

/// The catalog's Plaid example (its own contract and world, unedited), in the
/// quickstart's order: link token, exchange, `/accounts/get`, `/auth/get`,
/// `/identity/get`, `/transactions/sync`, balance, item. The model's sync answer draws
/// accounts of its own beside its transactions, and names them; every `account_id` the
/// sync answers and stores is one of the Item's listed accounts all the same, and the
/// drawn accounts are not stored under the Item.
#[tokio::test]
async fn the_catalog_plaid_syncs_transactions_name_the_items_listed_accounts() {
    let world = mb_import::parse_document(PLAID_WORLD.as_bytes()).unwrap();
    let accounts = vec![
        plaid_account(
            &plaid_id("Checking"),
            "Plaid Checking",
            "depository",
            "checking",
            "0000",
        ),
        plaid_account(
            &plaid_id("Saving"),
            "Plaid Saving",
            "depository",
            "savings",
            "1111",
        ),
        plaid_account(
            &plaid_id("Credit"),
            "Plaid Credit Card",
            "credit",
            "credit card",
            "3333",
        ),
    ];
    let owner = json!({"names": ["Alberta Charleson"], "phone_numbers": [], "emails": [],
                       "addresses": []});
    let owned: Vec<Value> = accounts
        .iter()
        .cloned()
        .map(|mut account| {
            account["owners"] = json!([owner]);
            account
        })
        .collect();
    let (drawn_a, drawn_b) = (plaid_id("DrawnA"), plaid_id("DrawnB"));
    let sync = json!({
        "transactions_update_status": "HISTORICAL_UPDATE_COMPLETE",
        "accounts": [plaid_account(&drawn_a, "Plaid Checking", "depository", "checking", "0000"),
                     plaid_account(&drawn_b, "Plaid Credit Card", "credit", "credit card", "3333")],
        "added": [plaid_transaction(&plaid_id("TxnOne"), &drawn_a, 4.5),
                  plaid_transaction(&plaid_id("TxnTwo"), &drawn_b, 12.0),
                  plaid_transaction(&plaid_id("TxnThree"), &drawn_a, -1500.0)],
        "modified": [plaid_transaction(&plaid_id("TxnFour"), &drawn_b, 7.25)],
        "removed": [{"transaction_id": plaid_id("TxnGone"), "account_id": drawn_a}],
        "next_cursor": "a".repeat(44), "has_more": false, "request_id": "abcdefghijklmno"
    });
    let item = json!({"institution_id": null, "institution_name": "First Platypus Bank",
                      "webhook": null, "error": null, "available_products": ["balance"],
                      "billed_products": ["auth", "transactions"],
                      "consent_expiration_time": null, "update_type": "background"});
    let script = vec![
        json!({"link_token": "link-sandbox-00000000-0000-4000-8000-000000000000",
               "expiration": "2026-11-13T00:00:00Z", "request_id": "abcdefghijklmno"}),
        // The exchange's fill, then its repair.
        json!({}),
        item,
        json!({"accounts": accounts, "item": plaid_item(), "request_id": "abcdefghijklmno"}),
        // `/identity/get`: the held accounts have no owners to serve it from state.
        json!({"accounts": owned, "item": plaid_item(), "request_id": "abcdefghijklmno"}),
        sync,
    ];
    let h = support::described(PLAID, world, script);
    let _ = plaid_post(
        &h,
        "/link/token/create",
        json!({"client_name": "Ledgerly", "language": "en", "country_codes": ["US"],
               "user": {"client_user_id": "u1"}, "products": ["transactions"]}),
    )
    .await;
    let exchanged = plaid_post(
        &h,
        "/item/public_token/exchange",
        json!({"public_token": "public-sandbox-5c224a01-8314-4491-a06f-39e193d5cddc"}),
    )
    .await;
    let token = json!({"access_token": exchanged["access_token"]});
    let listed = accounts_named(
        &plaid_post(&h, "/accounts/get", token.clone()).await,
        &["accounts"],
    );
    assert_eq!(listed.len(), 3, "{listed:?}");
    let _ = plaid_post(&h, "/auth/get", token.clone()).await;
    let _ = plaid_post(&h, "/identity/get", token.clone()).await;
    let synced = plaid_post(&h, "/transactions/sync", token.clone()).await;
    let named = accounts_named(&synced, &["added", "modified", "removed"]);
    assert_eq!(named.len(), 5, "{synced}");
    assert!(
        named.iter().all(|id| listed.contains(id)),
        "{listed:?} {synced}"
    );
    // The two accounts the model drew beside them are swapped for the Item's three.
    assert_eq!(accounts_named(&synced, &["accounts"]), listed, "{synced}");
    // No account the sync drew was stored.
    let stored = h.stored_ids("account").await;
    assert!(
        !stored.contains(&drawn_a) && !stored.contains(&drawn_b),
        "{stored:?}"
    );
    // The Item's accounts are still the three listed: none the sync drew joined them.
    for path in ["/accounts/balance/get", "/accounts/get"] {
        let answer = plaid_post(&h, path, token.clone()).await;
        assert_eq!(
            accounts_named(&answer, &["accounts"]),
            listed,
            "{path}: {answer}"
        );
    }
    let _ = plaid_post(&h, "/item/get", token.clone()).await;
    // The stored transactions name them too: the next sync answers from state.
    let (_, headers, again) = h.send("POST", "/transactions/sync", Some(token), &[]).await;
    assert_eq!(headers["x-mockingbird-source"], "state", "{again}");
    let named = accounts_named(&again, &["added"]);
    assert_eq!(named.len(), 3, "{again}");
    assert!(
        named.iter().all(|id| listed.contains(id)),
        "{listed:?} {again}"
    );
}
