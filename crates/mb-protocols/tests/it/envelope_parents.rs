//! A list scoped under a parent record (Plaid's accounts, scoped by the `access_token`
//! their Item was issued; a folder's files, by its path id) whose answer envelope also
//! holds a member of the parent's type (`/accounts/get`'s `item`): once the list is
//! answered from state, that member is filled from the held parent, so it names the
//! same parent on every call. The first list, generated, is filled the same before it is
//! served and recorded. Of two members of the parent's type, only the one named for it
//! is filled. A member elsewhere in the envelope named for the listed type's identity
//! (`/auth/get`'s `numbers.ach[].account_id`) names a listed record, generated or not;
//! records of the listed type beside the list (a sync's `modified`, `removed`) are no
//! such reference. A scenario's `set` stands over the fill.
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
                required: [accounts, item, request_id]
                properties:
                  accounts: {type: array, items: {$ref: "#/components/schemas/Account"}}
                  item: {$ref: "#/components/schemas/Item"}
                  request_id: {type: string}
        "400": {$ref: "#/components/responses/Error"}
components:
  responses:
    Error:
      description: bad
      content: {application/json: {schema: {type: object, properties: {error_code: {type: string}}}}}
  schemas:
    TokenRequest: {type: object, required: [access_token], properties: {access_token: {type: string}}}
    # Ids no word the faker draws can repeat: two Items, or an Item and a drawn
    # `previous_item`, never share one by chance.
    AccessToken: {type: string, pattern: "^access-[a-z0-9]{16}$"}
    ItemId: {type: string, pattern: "^item-[a-z0-9]{16}$"}
    Item:
      type: object
      required: [item_id, institution_id]
      properties: {item_id: {$ref: "#/components/schemas/ItemId"}, institution_id: {type: string}}
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
                            "scope": {"param": "access_token"}, "not_found": 400}
        }
    })
}

async fn send(h: &support::DescribedHarness, path: &str, body: Value) -> (String, Value) {
    let (status, headers, body) = h.send("POST", path, Some(body), &[]).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    let source = headers["x-mockingbird-source"]
        .to_str()
        .unwrap_or_default()
        .to_owned();
    (source, body)
}

#[tokio::test]
async fn a_scoped_lists_parent_shaped_member_is_the_held_parent() {
    let script = vec![
        json!({"institution_id": "ins_1"}),
        json!({"item": {"item_id": "item-drawn00000000000", "institution_id": "ins_x"}, "request_id": "r",
               "accounts": [{"account_id": "a1", "name": "Checking"}]}),
        json!({"institution_id": "ins_2"}),
        json!({"item": {"item_id": "item-drawn00000000000", "institution_id": "ins_x"}, "request_id": "r",
               "accounts": [{"account_id": "b1", "name": "Card"}]}),
    ];
    let h = support::described(CONTRACT, world(), script);
    let (_, exchanged) = send(
        &h,
        "/item/public_token/exchange",
        json!({"public_token": "p"}),
    )
    .await;
    let token = exchanged["access_token"].clone();
    let item_id = exchanged["item_id"].clone();
    let (_, read) = send(&h, "/item/get", json!({"access_token": token})).await;
    let item = read["item"].clone();
    assert_eq!(item["item_id"], item_id, "{read}");
    // The first list is generated and marks the collection; later ones are from state.
    let _ = send(&h, "/accounts/get", json!({"access_token": token})).await;
    for _ in 0..2 {
        let (source, page) = send(&h, "/accounts/get", json!({"access_token": token})).await;
        assert_eq!(source, "state", "{page}");
        assert_eq!(page["accounts"][0]["account_id"], json!("a1"), "{page}");
        assert_eq!(page["item"], item, "{page}");
    }
    // An Item never read whole: its envelope member still names it by its identity.
    let (_, exchanged) = send(
        &h,
        "/item/public_token/exchange",
        json!({"public_token": "q"}),
    )
    .await;
    let token = exchanged["access_token"].clone();
    let _ = send(&h, "/accounts/get", json!({"access_token": token})).await;
    for _ in 0..2 {
        let (source, page) = send(&h, "/accounts/get", json!({"access_token": token})).await;
        assert_eq!(source, "state", "{page}");
        assert_eq!(page["accounts"][0]["account_id"], json!("b1"), "{page}");
        assert_eq!(page["item"]["item_id"], exchanged["item_id"], "{page}");
    }
}

/// [`CONTRACT`] with a second member of the Item's type beside `item`.
fn with_previous_item() -> String {
    CONTRACT
        .replace(
            "required: [accounts, item, request_id]",
            "required: [accounts, item, previous_item, request_id]",
        )
        .replace(
            "                  item: {$ref: \"#/components/schemas/Item\"}\n",
            "                  item: {$ref: \"#/components/schemas/Item\"}\n                  previous_item: {$ref: \"#/components/schemas/Item\"}\n",
        )
}

#[tokio::test]
async fn of_two_parent_shaped_members_the_one_named_for_the_type_is_filled() {
    let contract = with_previous_item();
    assert!(contract.contains("previous_item: {$ref"), "{contract}");
    let script = vec![
        json!({"institution_id": "ins_1"}),
        json!({"item": {"item_id": "item-drawn00000000000", "institution_id": "ins_x"},
               "previous_item": {"item_id": "item-before0000000000", "institution_id": "ins_y"},
               "request_id": "r", "accounts": [{"account_id": "a1", "name": "Checking"}]}),
    ];
    let h = support::described(&contract, world(), script);
    let (_, exchanged) = send(
        &h,
        "/item/public_token/exchange",
        json!({"public_token": "p"}),
    )
    .await;
    let token = exchanged["access_token"].clone();
    let item_id = exchanged["item_id"].clone();
    let _ = send(&h, "/accounts/get", json!({"access_token": token})).await;
    for _ in 0..2 {
        let (source, page) = send(&h, "/accounts/get", json!({"access_token": token})).await;
        assert_eq!(source, "state", "{page}");
        assert_eq!(page["item"]["item_id"], item_id, "{page}");
        assert!(page["previous_item"].is_object(), "{page}");
        assert_ne!(page["previous_item"]["item_id"], item_id, "{page}");
    }
}

const FOLDERS: &str = r##"
openapi: 3.1.0
info: {title: Folders, version: "1"}
paths:
  /folders:
    post:
      operationId: createFolder
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [name], properties: {name: {type: string}}}}}
      responses:
        "201":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}
  /folders/{folder_id}:
    get:
      operationId: getFolder
      parameters: [{name: folder_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}
        "404": {description: missing}
  /folders/{folder_id}/files:
    parameters: [{name: folder_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createFile
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [name], properties: {name: {type: string}}}}}
      responses:
        "201":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/File"}}}
    get:
      operationId: listFiles
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [data, parent]
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/File"}}
                  parent: {$ref: "#/components/schemas/Folder"}
components:
  schemas:
    Folder:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    File:
      type: object
      required: [id, name, folder_id]
      properties: {id: {type: string}, name: {type: string}, folder_id: {type: string}}
"##;

fn folders_world() -> Value {
    json!({
        "types": {
            "folder": {"identity": {"member": "id"}},
            "file": {"identity": {"member": "id"},
                     "parent": {"type": "folder", "member": "folder_id", "path_param": "folder_id"}}
        },
        "operations": {
            "createFolder": {"kind": "create", "type": "folder"},
            "getFolder": {"kind": "read", "type": "folder"},
            "createFile": {"kind": "create", "type": "file"},
            "listFiles": {"kind": "list", "type": "file", "answer": {"records": "/data"}}
        }
    })
}

#[tokio::test]
async fn a_path_scoped_lists_parent_member_is_the_held_parent_by_its_definition() {
    let h = support::described(FOLDERS, folders_world(), Vec::new());
    let (status, _, folder) = h
        .send("POST", "/folders", Some(json!({"name": "Docs"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{folder}");
    let id = folder["id"].as_str().unwrap_or_default().to_owned();
    let (status, _, held) = h.send("GET", &format!("/folders/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{held}");
    let files = format!("/folders/{id}/files");
    let (status, _, file) = h
        .send("POST", &files, Some(json!({"name": "a.txt"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{file}");
    for _ in 0..2 {
        let (status, headers, page) = h.send("GET", &files, None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(headers["x-mockingbird-source"], "state", "{page}");
        assert_eq!(page["data"][0]["id"], file["id"], "{page}");
        assert_eq!(page["parent"], held, "{page}");
    }
}

#[tokio::test]
async fn the_first_generated_scoped_lists_parent_member_is_the_held_parent() {
    let script = vec![
        json!({"institution_id": "ins_1"}),
        json!({"item": {"item_id": "item-drawn00000000000", "institution_id": "ins_x"}, "request_id": "r",
               "accounts": [{"account_id": "a1", "name": "Checking"}]}),
    ];
    let h = support::described(CONTRACT, world(), script);
    let (_, exchanged) = send(
        &h,
        "/item/public_token/exchange",
        json!({"public_token": "p"}),
    )
    .await;
    let token = exchanged["access_token"].clone();
    let (_, read) = send(&h, "/item/get", json!({"access_token": token})).await;
    let item = read["item"].clone();
    assert_eq!(item["item_id"], exchanged["item_id"], "{read}");
    let (source, first) = send(&h, "/accounts/get", json!({"access_token": token})).await;
    assert_ne!(source, "state", "{first}");
    assert_eq!(first["accounts"][0]["account_id"], json!("a1"), "{first}");
    assert_eq!(first["item"], item, "{first}");
    let (source, later) = send(&h, "/accounts/get", json!({"access_token": token})).await;
    assert_eq!(source, "state", "{later}");
    assert_eq!(later["item"], first["item"], "{later}");
}

/// [`CONTRACT`] with Plaid's `/auth/get`: the accounts, and their numbers beside them,
/// each naming its account by `account_id`.
fn with_auth() -> String {
    CONTRACT.replace(
        "components:\n",
        r##"  /auth/get:
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
                required: [accounts, numbers, item, request_id]
                properties:
                  accounts: {type: array, items: {$ref: "#/components/schemas/Account"}}
                  numbers:
                    type: object
                    required: [ach]
                    properties:
                      ach:
                        type: array
                        minItems: 2
                        items:
                          type: object
                          required: [account_id, routing]
                          properties: {account_id: {type: string}, routing: {type: string}}
                  item: {$ref: "#/components/schemas/Item"}
                  request_id: {type: string}
        "400": {$ref: "#/components/responses/Error"}
components:
"##,
    )
}

fn auth_world() -> Value {
    let mut world = world();
    world["operations"]["authGet"] = json!({
        "kind": "list", "type": "account", "answer": {"records": "/accounts"},
        "scope": {"param": "access_token"}, "not_found": 400
    });
    world
}

/// The `account_id` each `numbers.ach` entry of `page` names.
fn ach_accounts(page: &Value) -> Vec<Value> {
    page["numbers"]["ach"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| entry["account_id"].clone())
        .collect()
}

#[tokio::test]
async fn envelope_references_to_the_listed_type_name_listed_records() {
    let contract = with_auth();
    assert!(contract.contains("/auth/get:"), "{contract}");
    let script = vec![
        json!({"institution_id": "ins_1"}),
        json!({"item": {"item_id": "item-drawn00000000000", "institution_id": "ins_x"}, "request_id": "r",
               "accounts": [{"account_id": "a1", "name": "Checking"},
                            {"account_id": "a2", "name": "Savings"}],
               "numbers": {"ach": [{"account_id": "elsewhere", "routing": "011"},
                                   {"account_id": "a2", "routing": "012"},
                                   {"account_id": "nowhere", "routing": "013"}]}}),
    ];
    let h = support::described(&contract, auth_world(), script);
    let (_, exchanged) = send(
        &h,
        "/item/public_token/exchange",
        json!({"public_token": "p"}),
    )
    .await;
    let token = exchanged["access_token"].clone();
    let listed = |page: &Value| -> Vec<Value> {
        page["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| a["account_id"].clone())
            .collect()
    };
    // The generated list: by position among the references, cycling through the listed.
    let (source, first) = send(&h, "/auth/get", json!({"access_token": token})).await;
    assert_ne!(source, "state", "{first}");
    assert_eq!(listed(&first), vec![json!("a1"), json!("a2")], "{first}");
    assert_eq!(
        ach_accounts(&first),
        vec![json!("a1"), json!("a2"), json!("a1")],
        "{first}"
    );
    assert_eq!(
        first["numbers"]["ach"][2]["routing"],
        json!("013"),
        "{first}"
    );
    assert_eq!(first["item"]["item_id"], exchanged["item_id"], "{first}");
    // A list from state: its drawn numbers name listed accounts too.
    for _ in 0..2 {
        let (source, page) = send(&h, "/auth/get", json!({"access_token": token})).await;
        assert_eq!(source, "state", "{page}");
        let ids = listed(&page);
        assert_eq!(ids, vec![json!("a1"), json!("a2")], "{page}");
        let named = ach_accounts(&page);
        assert!(named.len() >= 2, "{page}");
        assert!(named.iter().all(|id| ids.contains(id)), "{page}");
    }
}

/// [`support::described`] over `contract`, with a scenario `steady` whose `set` is `set`.
fn with_scenario(
    contract: &str,
    world: Value,
    script: Vec<Value>,
    set: Value,
) -> support::DescribedHarness {
    support::harness(
        contract,
        script,
        &support::HarnessOptions::default(),
        |rt| {
            let mut rt = support::with_effective(rt, world, false);
            rt.scenarios.insert(
                "steady".into(),
                mb_protocols::ScenarioRuntime {
                    spec: mb_engine::ScenarioSpec {
                        name: "steady".into(),
                        set: Some(set),
                        ..mb_engine::ScenarioSpec::default()
                    },
                    ..mb_protocols::ScenarioRuntime::default()
                },
            );
            rt
        },
    )
}

#[tokio::test]
async fn a_generated_list_under_a_scenario_holds_the_held_parent_and_what_it_sets() {
    let script = vec![
        json!({"institution_id": "ins_1"}),
        json!({"item": {"item_id": "item-drawn00000000000", "institution_id": "ins_x"},
               "request_id": "r", "accounts": [{"account_id": "a1", "name": "Checking"}]}),
    ];
    // The scenario sets a member of the very member the held parent fills.
    let h = with_scenario(
        CONTRACT,
        world(),
        script,
        json!({"item": {"institution_id": "steady"}}),
    );
    let (_, exchanged) = send(
        &h,
        "/item/public_token/exchange",
        json!({"public_token": "p"}),
    )
    .await;
    let token = exchanged["access_token"].clone();
    // Read whole, the held Item holds an `institution_id` the fill would write.
    let (_, read) = send(&h, "/item/get", json!({"access_token": token})).await;
    assert!(read["item"]["institution_id"].is_string(), "{read}");
    assert_ne!(read["item"]["institution_id"], json!("steady"), "{read}");
    let (status, _, first) = h
        .send(
            "POST",
            "/accounts/get",
            Some(json!({"access_token": token})),
            &[("x-mockingbird-scenario", "steady")],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["accounts"][0]["account_id"], json!("a1"), "{first}");
    assert_eq!(first["item"]["item_id"], exchanged["item_id"], "{first}");
    assert_eq!(first["item"]["institution_id"], json!("steady"), "{first}");
}

const FILE_ARRAYS: &str = r##"
openapi: 3.1.0
info: {title: Files, version: "1"}
paths:
  /folders:
    post:
      operationId: createFolder
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [name], properties: {name: {type: string}}}}}
      responses:
        "201":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}
  /folders/{folder_id}:
    get:
      operationId: getFolder
      parameters: [{name: folder_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}
        "404": {description: missing}
  /folders/{folder_id}/files:
    get:
      operationId: listFiles
      parameters: [{name: folder_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {type: array, items: {$ref: "#/components/schemas/File"}}
components:
  schemas:
    Folder:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    File:
      type: object
      required: [file_id, name, folder_id]
      properties: {file_id: {type: string}, name: {type: string}, folder_id: {type: string}}
"##;

#[tokio::test]
async fn a_scenario_set_leaves_a_generated_array_list_an_array() {
    let world = json!({
        "types": {
            "folder": {"identity": {"member": "id"}},
            "file": {"identity": {"member": "file_id"},
                     "parent": {"type": "folder", "member": "folder_id", "path_param": "folder_id"}}
        },
        "operations": {
            "createFolder": {"kind": "create", "type": "folder"},
            "getFolder": {"kind": "read", "type": "folder"},
            "listFiles": {"kind": "list", "type": "file"}
        }
    });
    let h = with_scenario(
        FILE_ARRAYS,
        world,
        vec![json!([{"file_id": "file-1", "name": "a.txt", "folder_id": "f"}])],
        json!({"note": "steady"}),
    );
    let (status, _, folder) = h
        .send("POST", "/folders", Some(json!({"name": "Docs"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{folder}");
    let id = folder["id"].as_str().unwrap_or_default().to_owned();
    let (status, _, page) = h
        .send(
            "GET",
            &format!("/folders/{id}/files"),
            None,
            &[("x-mockingbird-scenario", "steady")],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.is_array(), "{page}");
    assert_eq!(page[0]["file_id"], json!("file-1"), "{page}");
}

/// [`CONTRACT`] with a sync of an Item's transactions: the added ones listed (under
/// `data`, and read by `transaction_id`, which a generated answer's records are seeded
/// by beside other arrays of records), and beside them the modified (whole records), the removed (each named with its account) and the
/// voided (bare ids), none of which is a reference to an added one.
fn with_sync() -> String {
    CONTRACT
        .replace(
            "components:\n",
            r##"  /transactions/{transaction_id}:
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
                required: [data, modified, removed, voided, request_id]
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/Transaction"}}
                  modified: {type: array, minItems: 1, items: {$ref: "#/components/schemas/Transaction"}}
                  removed:
                    type: array
                    minItems: 1
                    items:
                      type: object
                      required: [transaction_id, account_id]
                      properties: {transaction_id: {type: string}, account_id: {type: string}}
                  voided:
                    type: array
                    minItems: 1
                    items:
                      type: object
                      required: [transaction_id]
                      properties: {transaction_id: {type: string}}
                  request_id: {type: string}
        "400": {$ref: "#/components/responses/Error"}
components:
"##,
        )
        .replace(
            "  schemas:\n",
            "  schemas:\n    Transaction:\n      type: object\n      required: [transaction_id, account_id, amount]\n      properties: {transaction_id: {type: string}, account_id: {type: string}, amount: {type: number}}\n",
        )
}

/// The `transaction_id`s each of `members` of `page` holds.
fn transaction_ids(page: &Value, members: &[&str]) -> Vec<Value> {
    members
        .iter()
        .flat_map(|m| page[*m].as_array().cloned().unwrap_or_default())
        .map(|entry| entry["transaction_id"].clone())
        .collect()
}

#[tokio::test]
async fn records_of_the_listed_type_beside_the_list_are_no_references() {
    let contract = with_sync();
    assert!(contract.contains("/transactions/sync:"), "{contract}");
    assert!(contract.contains("    Transaction:"), "{contract}");
    let mut world = world();
    world["types"]["transaction"] =
        json!({"identity": {"member": "transaction_id"}, "parent": {"type": "item"}});
    world["operations"]["transactionGet"] = json!({"kind": "read", "type": "transaction"});
    world["operations"]["transactionsSync"] = json!({
        "kind": "list", "type": "transaction", "answer": {"records": "/data"},
        "scope": {"param": "access_token"}, "not_found": 400
    });
    let generated = json!({
        "data": [{"transaction_id": "txn-1", "account_id": "acc-1", "amount": 1.5},
                  {"transaction_id": "txn-2", "account_id": "acc-1", "amount": 2.5}],
        "modified": [{"transaction_id": "txn-mod", "account_id": "acc-1", "amount": 3.5}],
        "removed": [{"transaction_id": "txn-gone", "account_id": "acc-1"}],
        "voided": [{"transaction_id": "txn-void"}],
        "request_id": "r"
    });
    let h = support::described(&contract, world, vec![json!({}), generated.clone()]);
    let (_, exchanged) = send(
        &h,
        "/item/public_token/exchange",
        json!({"public_token": "p"}),
    )
    .await;
    let token = exchanged["access_token"].clone();
    let others = ["modified", "removed", "voided"];
    let (source, first) = send(&h, "/transactions/sync", json!({"access_token": token})).await;
    assert_ne!(source, "state", "{first}");
    for member in others {
        assert_eq!(first[member], generated[member], "{first}");
    }
    // From state, the drawn records beside the list are left as drawn.
    let listed = [json!("txn-1"), json!("txn-2")];
    for _ in 0..2 {
        let (source, page) = send(&h, "/transactions/sync", json!({"access_token": token})).await;
        assert_eq!(source, "state", "{page}");
        assert_eq!(transaction_ids(&page, &["data"]), listed.to_vec(), "{page}");
        let beside = transaction_ids(&page, &others);
        assert!(beside.len() >= 3, "{page}");
        assert!(beside.iter().all(|id| !listed.contains(id)), "{page}");
    }
}
