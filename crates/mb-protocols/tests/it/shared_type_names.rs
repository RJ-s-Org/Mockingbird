//! Two objects under one collection name (GitHub's deploy keys at
//! `/repos/{owner}/{repo}/keys` and SSH keys at `/user/keys`) are two types, so their
//! records are stored apart: a read or list of one never answers the other's records,
//! even under the same id. Before, both were type `keys`, one store.
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Keys, version: "1"}
paths:
  /repos/{owner}/{repo}/keys:
    parameters:
      - {name: owner, in: path, required: true, schema: {type: string}}
      - {name: repo, in: path, required: true, schema: {type: string}}
    get:
      operationId: listDeployKeys
      responses:
        "200":
          description: OK
          content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/deploy-key"}}}}
    post:
      operationId: createDeployKey
      requestBody:
        content:
          application/json:
            schema:
              type: object
              required: [title, key]
              properties: {title: {type: string}, key: {type: string}, read_only: {type: boolean}}
      responses:
        "201":
          description: Created
          content: {application/json: {schema: {$ref: "#/components/schemas/deploy-key"}}}
  /repos/{owner}/{repo}/keys/{key_id}:
    parameters:
      - {name: owner, in: path, required: true, schema: {type: string}}
      - {name: repo, in: path, required: true, schema: {type: string}}
      - {name: key_id, in: path, required: true, schema: {type: integer}}
    get:
      operationId: getDeployKey
      responses:
        "200":
          description: OK
          content: {application/json: {schema: {$ref: "#/components/schemas/deploy-key"}}}
        "404": {description: Not found}
  /user/keys:
    get:
      operationId: listSshKeys
      responses:
        "200":
          description: OK
          content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/key"}}}}
    post:
      operationId: createSshKey
      requestBody:
        content:
          application/json:
            schema:
              type: object
              required: [key]
              properties: {title: {type: string}, key: {type: string}}
      responses:
        "201":
          description: Created
          content: {application/json: {schema: {$ref: "#/components/schemas/key"}}}
  /user/keys/{key_id}:
    parameters:
      - {name: key_id, in: path, required: true, schema: {type: integer}}
    get:
      operationId: getSshKey
      responses:
        "200":
          description: OK
          content: {application/json: {schema: {$ref: "#/components/schemas/key"}}}
        "404": {description: Not found}
components:
  schemas:
    deploy-key:
      type: object
      required: [id, key, title, read_only]
      properties:
        id: {type: integer}
        key: {type: string}
        title: {type: string}
        read_only: {type: boolean}
    key:
      type: object
      required: [id, key, title]
      properties:
        id: {type: integer}
        key: {type: string}
        title: {type: string}
        verified: {type: boolean}
"##;

fn harness() -> support::DescribedHarness {
    let options = support::HarnessOptions {
        unknown_ids_not_found: true,
        ..support::HarnessOptions::default()
    };
    support::described_with(CONTRACT, json!({"version": 1}), vec![], &options, false)
}

fn titles(list: &Value) -> Vec<&str> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| k["title"].as_str())
        .collect()
}

#[test]
fn each_object_under_one_collection_name_is_a_type_of_its_own() {
    let bundle = mb_import::compile(CONTRACT.as_bytes()).unwrap();
    let type_of = |id: &str| {
        bundle
            .operations
            .iter()
            .find(|o| o.id == id)
            .and_then(|o| o.resource_type.clone())
    };
    // `key` is named for the type, so it keeps `keys`.
    for op in ["listSshKeys", "createSshKey", "getSshKey"] {
        assert_eq!(type_of(op).as_deref(), Some("keys"), "{op}");
    }
    for op in ["listDeployKeys", "createDeployKey", "getDeployKey"] {
        assert_eq!(type_of(op).as_deref(), Some("deploy-key"), "{op}");
    }
}

#[tokio::test]
async fn records_of_two_objects_under_one_collection_name_never_mix() {
    let h = harness();
    let (status, _, deploy) = h
        .send(
            "POST",
            "/repos/o/r/keys",
            Some(json!({"title": "deploy", "key": "ssh-ed25519 AAAA deploy", "read_only": true})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{deploy}");
    let (status, _, ssh) = h
        .send(
            "POST",
            "/user/keys",
            Some(json!({"title": "ssh", "key": "ssh-ed25519 AAAA ssh"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{ssh}");

    // Stored apart, each under its own type.
    let deploy_ids = h.stored_ids("deploy-key").await;
    let ssh_ids = h.stored_ids("keys").await;
    assert_eq!(deploy_ids, [deploy["id"].to_string()], "{deploy}");
    assert_eq!(ssh_ids, [ssh["id"].to_string()], "{ssh}");

    // Each read answers its own record; across, the other's id is unknown unless
    // both minted the same one, when it is still the route's own record.
    let (status, _, got) = h
        .send(
            "GET",
            &format!("/repos/o/r/keys/{}", deploy["id"]),
            None,
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["title"], "deploy", "{got}");
    let (status, _, got) = h
        .send("GET", &format!("/user/keys/{}", ssh["id"]), None, &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["title"], "ssh", "{got}");
    let (_, _, across) = h
        .send("GET", &format!("/user/keys/{}", deploy["id"]), None, &[])
        .await;
    assert_ne!(across["title"], "deploy", "{across}");
    let (_, _, across) = h
        .send("GET", &format!("/repos/o/r/keys/{}", ssh["id"]), None, &[])
        .await;
    assert_ne!(across["title"], "ssh", "{across}");

    // Lists hold only their own records.
    let (status, _, list) = h.send("GET", "/repos/o/r/keys", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(titles(&list).contains(&"deploy"), "{list}");
    assert!(!titles(&list).contains(&"ssh"), "{list}");
    let (status, _, list) = h.send("GET", "/user/keys", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(titles(&list).contains(&"ssh"), "{list}");
    assert!(!titles(&list).contains(&"deploy"), "{list}");
}

#[tokio::test]
async fn records_under_one_id_in_both_stores_stay_apart() {
    let h = harness();
    h.hold(
        "deploy-key",
        &[json!({"id": 7, "key": "k1", "title": "deploy", "read_only": true})],
    )
    .await;
    h.hold(
        "keys",
        &[json!({"id": 7, "key": "k2", "title": "ssh", "verified": true})],
    )
    .await;
    let (status, _, got) = h.send("GET", "/repos/o/r/keys/7", None, &[]).await;
    assert_eq!(
        (status, &got["title"]),
        (StatusCode::OK, &json!("deploy")),
        "{got}"
    );
    let (status, _, got) = h.send("GET", "/user/keys/7", None, &[]).await;
    assert_eq!(
        (status, &got["title"]),
        (StatusCode::OK, &json!("ssh")),
        "{got}"
    );
    let (_, _, list) = h.send("GET", "/user/keys", None, &[]).await;
    assert_eq!(titles(&list), ["ssh"], "{list}");
    // A held record names no repository, so the scoped list answers none of it; it
    // never answers the other store's record.
    let (_, _, list) = h.send("GET", "/repos/o/r/keys", None, &[]).await;
    assert!(!titles(&list).contains(&"ssh"), "{list}");
}
