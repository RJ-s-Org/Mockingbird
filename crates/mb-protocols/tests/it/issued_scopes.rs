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

/// [`CONTRACT`] with more request members and an `api_version` query parameter on
/// `/accounts/get`, for a description to ignore or read.
fn ignoring_contract() -> String {
    CONTRACT
        .replace(
            "properties: {access_token: {type: string}}}",
            "properties: {access_token: {type: string}, client_id: {type: string}, \
             secret: {type: string}, note: {type: string}, options: {type: object, \
             properties: {client_id: {type: string}, account_ids: {type: array, \
             items: {type: string}}}}}}",
        )
        .replace(
            "      operationId: accountsGet\n",
            "      operationId: accountsGet\n      parameters: \
             [{name: api_version, in: query, schema: {type: string}}]\n",
        )
}

/// Sends `/accounts/get` (with `query`) and returns the answer's source and accounts.
async fn accounts(h: &support::DescribedHarness, query: &str, body: Value) -> (String, Value) {
    let (status, headers, body) = h
        .send("POST", &format!("/accounts/get{query}"), Some(body), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let source = headers["x-mockingbird-source"]
        .to_str()
        .unwrap_or_default()
        .to_owned();
    (source, body["accounts"].clone())
}

fn scripted() -> Vec<Value> {
    let accounts = |id: &str| json!({"accounts": [{"account_id": id, "name": "Checking"}]});
    vec![
        json!({"institution_id": "ins_1"}),
        accounts("a1"),
        accounts("a2"),
        accounts("a3"),
        accounts("a4"),
    ]
}

#[tokio::test]
async fn a_list_whose_only_extra_query_parameter_is_ignored_is_recorded() {
    let mut world = world();
    world["operations"]["accountsGet"]["ignores"] = json!(["api_version"]);
    let h = support::described(&ignoring_contract(), world, scripted());
    let first = token(&h).await;
    let body = json!({"access_token": first});
    let (source, generated) = accounts(&h, "?api_version=1", body.clone()).await;
    assert_ne!(source, "state");
    let (source, held) = accounts(&h, "?api_version=2", body).await;
    assert_eq!((source.as_str(), &held), ("state", &generated));
}

#[tokio::test]
async fn an_object_holding_only_an_ignored_member_is_served_from_state() {
    let mut world = world();
    world["operations"]["accountsGet"]["ignores"] = json!(["/options/client_id"]);
    let h = support::described(&ignoring_contract(), world, scripted());
    let first = token(&h).await;
    let (source, generated) = accounts(&h, "", json!({"access_token": first})).await;
    assert_ne!(source, "state");
    let body = json!({"access_token": first, "options": {"client_id": "c"}});
    let (source, held) = accounts(&h, "", body).await;
    assert_eq!((source.as_str(), &held), ("state", &generated));
}

#[tokio::test]
async fn an_ignored_member_holding_a_described_filter_still_asks_something() {
    // `/options` is ignored, but the filter below it narrows the page: a list sending
    // it is not the whole collection, so it is not recorded.
    let mut world = world();
    let op = &mut world["operations"]["accountsGet"];
    op["ignores"] = json!(["/options"]);
    op["filters"] =
        json!([{"param": "/options/account_ids", "member": "account_id", "kind": "eq"}]);
    let h = support::described(&ignoring_contract(), world, scripted());
    let first = token(&h).await;
    let filtered = json!({"access_token": first, "options": {"account_ids": ["a1"]}});
    let (source, _) = accounts(&h, "", filtered).await;
    assert_ne!(source, "state");
    let (source, _) = accounts(&h, "", json!({"access_token": first})).await;
    assert_ne!(source, "state");
}

#[tokio::test]
async fn a_list_carrying_ignored_body_members_is_recorded_and_answered_from_state() {
    // Plaid's catalog world: credentials in the body, which the description `ignores`.
    let contract = ignoring_contract();
    let mut world = world();
    world["operations"]["accountsGet"]["ignores"] = json!(["client_id", "secret"]);
    let script = vec![
        json!({"institution_id": "ins_1"}),
        json!({"accounts": [{"account_id": "a1", "name": "Checking"}]}),
        json!({"institution_id": "ins_2"}),
        json!({"accounts": [{"account_id": "b1", "name": "Card"}]}),
        json!({"accounts": [{"account_id": "b2", "name": "Loan"}]}),
    ];
    let h = support::described(&contract, world, script);
    let send = |body: Value| {
        let h = &h;
        async move {
            let (status, headers, body) = h.send("POST", "/accounts/get", Some(body), &[]).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let source = headers["x-mockingbird-source"]
                .to_str()
                .unwrap_or_default()
                .to_owned();
            (source, body["accounts"].clone())
        }
    };
    let creds = |token: &str| json!({"access_token": token, "client_id": "c", "secret": "s"});

    // The first list generates and marks the collection; the next answers from state.
    let first = token(&h).await;
    let (source, generated) = send(creds(&first)).await;
    assert_ne!(source, "state");
    let mut again = creds(&first);
    again["secret"] = json!("s2");
    let (source, held) = send(again).await;
    assert_eq!((source.as_str(), &held), ("state", &generated));

    // A member neither ignored nor described still asks something: that list is not
    // recorded, so the next one is generated too.
    let second = token(&h).await;
    let mut noted = creds(&second);
    noted["note"] = json!("x");
    let (source, _) = send(noted).await;
    assert_ne!(source, "state");
    let (source, _) = send(creds(&second)).await;
    assert_ne!(source, "state");
}
