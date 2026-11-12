//! A list scoped under a parent record (Plaid's accounts, scoped by the `access_token`
//! their Item was issued; a folder's files, by its path id) whose answer envelope also
//! holds a member of the parent's type (`/accounts/get`'s `item`): once the list is
//! answered from state, that member is filled from the held parent, so it names the
//! same parent on every call. The first list, generated, is filled the same before it is
//! served and recorded. Of two members of the parent's type, only the one named for it
//! is filled. A member elsewhere in the envelope named for the listed type's identity
//! (`/auth/get`'s `numbers.ach[].account_id`) names a listed record, generated or not.
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
        json!({"item": {"item_id": "drawn", "institution_id": "ins_x"}, "request_id": "r",
               "accounts": [{"account_id": "a1", "name": "Checking"}]}),
        json!({"institution_id": "ins_2"}),
        json!({"item": {"item_id": "drawn", "institution_id": "ins_x"}, "request_id": "r",
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
        json!({"item": {"item_id": "drawn", "institution_id": "ins_x"},
               "previous_item": {"item_id": "before", "institution_id": "ins_y"},
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
        json!({"item": {"item_id": "drawn", "institution_id": "ins_x"}, "request_id": "r",
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
        json!({"item": {"item_id": "drawn", "institution_id": "ins_x"}, "request_id": "r",
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

#[tokio::test]
async fn a_generated_list_under_a_scenario_holds_the_held_parent_and_what_it_sets() {
    let script = vec![
        json!({"institution_id": "ins_1"}),
        json!({"item": {"item_id": "drawn", "institution_id": "ins_x"}, "request_id": "r",
               "accounts": [{"account_id": "a1", "name": "Checking"}]}),
    ];
    let world = world();
    let h = support::harness(
        CONTRACT,
        script,
        &support::HarnessOptions::default(),
        |rt| {
            let mut rt = support::with_effective(rt, world, false);
            rt.scenarios.insert(
                "steady".into(),
                mb_protocols::ScenarioRuntime {
                    spec: mb_engine::ScenarioSpec {
                        name: "steady".into(),
                        set: Some(json!({"request_id": "steady"})),
                        ..mb_engine::ScenarioSpec::default()
                    },
                    ..mb_protocols::ScenarioRuntime::default()
                },
            );
            rt
        },
    );
    let (_, exchanged) = send(
        &h,
        "/item/public_token/exchange",
        json!({"public_token": "p"}),
    )
    .await;
    let token = exchanged["access_token"].clone();
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
    assert_eq!(first["request_id"], json!("steady"), "{first}");
}
