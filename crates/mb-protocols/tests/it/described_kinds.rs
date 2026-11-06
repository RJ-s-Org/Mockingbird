//! Kinds, targets, transitions and changes from a world description: an action the
//! description calls a delete removes its record, a body member names a read's target,
//! a transition or a change sets the target's members whatever the model answered, an
//! update takes its record from where its request holds it, and a replace drops what
//! it does not send. Where the description is silent, the heuristics answer as before.
#![cfg(test)]
#![cfg(feature = "http-rest")]

use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness, HarnessOptions};

/// Workspaces are deleted by an action route; files are read by a body member; folders
/// are archived and moved by custom methods; things are patched with a wrapped record
/// answering an acknowledgement, and put whole.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Kinds, version: "1"}
paths:
  /workspaces:
    post:
      operationId: createWorkspace
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Workspace"}}}}}
  /workspaces/{id}:
    get:
      operationId: getWorkspace
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Workspace"}}}}, "404": {description: missing}}
  /workspaces/{id}/actions/safe-delete:
    post:
      operationId: safeDeleteWorkspace
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"204": {description: done}}
  /files/create:
    post:
      operationId: createFile
      requestBody: {content: {application/json: {schema: {type: object, properties: {path: {type: string}, size: {type: integer}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/File"}}}}}
  /files/get:
    post:
      operationId: getFile
      requestBody: {content: {application/json: {schema: {type: object, properties: {path: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/File"}}}}}
  /folders:
    post:
      operationId: createFolder
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}, parent: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
  /folders/{id}:
    get:
      operationId: getFolder
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}, "404": {description: missing}}
  /folders/{id}:archive:
    post:
      operationId: archiveFolder
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
  /folders/{id}:move:
    post:
      operationId: moveFolder
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {destinationParent: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
  /things:
    post:
      operationId: createThing
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}, note: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
  /things/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getThing
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}, "404": {description: missing}}
    patch:
      operationId: patchThing
      requestBody: {content: {application/json: {schema: {type: object, properties: {data: {type: object, properties: {title: {type: string}}}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
    put:
      operationId: putThing
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
components:
  schemas:
    Workspace:
      type: object
      properties: {id: {type: string}, name: {type: string}}
    File:
      type: object
      properties: {path: {type: string}, size: {type: integer}}
    Folder:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
        parent: {type: string}
        state: {type: string, enum: [ACTIVE, ARCHIVED]}
    Thing:
      type: object
      properties:
        id: {type: string}
        title: {type: string}
        note: {type: string}
        createdAt: {type: string, readOnly: true}
    Ack:
      type: object
      properties: {ok: {type: boolean}}
"##;

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn source(headers: &HeaderMap) -> String {
    header(headers, "x-mockingbird-source")
}

fn plain(script: Vec<Value>) -> DescribedHarness {
    support::harness(CONTRACT, script, &HarnessOptions::default(), |rt| rt)
}

/// The resource type the contract gives operation `op`.
fn type_of(h: &DescribedHarness, op: &str) -> String {
    h.rt.bundle()
        .operation(op)
        .and_then(|o| o.resource_type.clone())
        .unwrap_or_else(|| panic!("{op} has no resource type"))
}

#[tokio::test]
async fn an_action_described_as_delete_removes_the_record() {
    let world = json!({"operations": {"safeDeleteWorkspace": {"kind": "delete"}}});
    let h = support::described(
        CONTRACT,
        world,
        vec![
            json!({"id": "w1", "name": "a"}),
            json!({"id": "w1", "name": "made up"}),
        ],
    );
    let (status, _, body) = h
        .send("POST", "/workspaces", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, headers, body) = h.send("GET", "/workspaces/w1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    let (status, _, body) = h
        .send("POST", "/workspaces/w1/actions/safe-delete", None, &[])
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let t = type_of(&h, "getWorkspace");
    assert_eq!(h.stored(&t, "w1").await, None);
    let (status, _, body) = h.send("GET", "/workspaces/w1", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn a_body_target_reads_from_state() {
    let world = json!({
        "types": {"files": {"identity": {"member": "path"}}},
        "operations": {
            "createFile": {"kind": "create", "type": "files"},
            "getFile": {"kind": "read", "type": "files", "target": {"member": "path"}}
        }
    });
    let h = support::described(
        CONTRACT,
        world,
        vec![
            json!({"path": "/a", "size": 1}),
            json!({"path": "/b", "size": 9}),
        ],
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/files/create",
            Some(json!({"path": "/a", "size": 1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, headers, body) = h
        .send("POST", "/files/get", Some(json!({"path": "/a"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!({"path": "/a", "size": 1}));
}

/// Creates folder `f1` (named "a", under `p1`, `ACTIVE`) through `h`, whose first
/// scripted answer is its record.
async fn create_folder(h: &DescribedHarness) {
    let (status, _, body) = h
        .send(
            "POST",
            "/folders",
            Some(json!({"name": "a", "parent": "p1"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], json!("f1"), "{body}");
}

fn folder(state: &str, parent: &str) -> Value {
    json!({"id": "f1", "name": "a", "parent": parent, "state": state})
}

#[tokio::test]
async fn a_transition_sets_the_state_whatever_the_model_answered() {
    let world = json!({
        "types": {"folders": {"lifecycle": {"member": "state", "states": ["ACTIVE", "ARCHIVED"]}}},
        "operations": {"archiveFolder": {"kind": "action", "target": {"path_param": "id"}, "transition": {"state": "ARCHIVED"}}}
    });
    let h = support::described(
        CONTRACT,
        world,
        vec![folder("ACTIVE", "p1"), folder("ACTIVE", "p1")],
    );
    create_folder(&h).await;
    let (status, _, body) = h.send("POST", "/folders/f1:archive", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], json!("ARCHIVED"), "{body}");
    let t = type_of(&h, "getFolder");
    assert_eq!(
        h.stored(&t, "f1").await.map(|r| r["state"].clone()),
        Some(json!("ARCHIVED"))
    );
    let (status, headers, body) = h.send("GET", "/folders/f1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body["state"], json!("ARCHIVED"), "{body}");
}

#[tokio::test]
async fn a_transition_value_the_member_refuses_is_dropped() {
    let world = json!({
        // No states listed: the validator takes any, and the member's `enum` refuses this.
        "types": {"folders": {"lifecycle": {"member": "state"}}},
        "operations": {"archiveFolder": {"kind": "action", "target": {"path_param": "id"}, "transition": {"state": "GONE"}}}
    });
    let h = support::described(
        CONTRACT,
        world,
        vec![folder("ACTIVE", "p1"), folder("ACTIVE", "p1")],
    );
    create_folder(&h).await;
    let (status, _, body) = h.send("POST", "/folders/f1:archive", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], json!("ACTIVE"), "{body}");
    let t = type_of(&h, "getFolder");
    assert_eq!(
        h.stored(&t, "f1").await.map(|r| r["state"].clone()),
        Some(json!("ACTIVE"))
    );
}

#[tokio::test]
async fn changes_apply_request_members_to_the_target() {
    let world = json!({
        "operations": {
            "moveFolder": {"kind": "action", "target": {"path_param": "id"}, "changes": {"parent": {"from_request": "/destinationParent"}, "state": {"value": "ACTIVE"}}}
        }
    });
    let h = support::described(
        CONTRACT,
        world,
        vec![folder("ACTIVE", "p1"), folder("ARCHIVED", "p1")],
    );
    create_folder(&h).await;
    let (status, _, body) = h
        .send(
            "POST",
            "/folders/f1:move",
            Some(json!({"destinationParent": "p2"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["parent"], json!("p2"), "{body}");
    assert_eq!(body["state"], json!("ACTIVE"), "{body}");
    let (status, headers, body) = h.send("GET", "/folders/f1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, folder("ACTIVE", "p2"));
}

/// Creates thing `t1` (titled "a", noted "n") through `h`, whose first scripted answer
/// is its record.
async fn create_thing(h: &DescribedHarness) {
    let (status, _, body) = h
        .send(
            "POST",
            "/things",
            Some(json!({"title": "a", "note": "n"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

fn thing() -> Value {
    json!({"id": "t1", "title": "a", "note": "n", "createdAt": "2026-01-01T00:00:00Z"})
}

#[tokio::test]
async fn an_update_whose_request_wraps_the_record_merges_the_inner_record() {
    let world = json!({
        "operations": {"patchThing": {"target": {"path_param": "id"}, "request": {"record": "/data"}}}
    });
    let h = support::described(CONTRACT, world, vec![thing(), json!({"ok": true})]);
    create_thing(&h).await;
    let (status, _, body) = h
        .send(
            "PATCH",
            "/things/t1",
            Some(json!({"data": {"title": "b"}})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let t = type_of(&h, "getThing");
    let held = h.stored(&t, "t1").await.unwrap();
    assert_eq!(held["title"], json!("b"), "{held}");
    assert!(held.get("data").is_none(), "{held}");
    assert_eq!(held["note"], json!("n"), "{held}");

    // With no world, the merge is as today: the wrapper is no member of the record.
    let h = plain(vec![thing(), json!({"ok": true})]);
    create_thing(&h).await;
    h.send(
        "PATCH",
        "/things/t1",
        Some(json!({"data": {"title": "b"}})),
        &[],
    )
    .await;
    let held = h.stored(&t, "t1").await.unwrap();
    assert!(held.get("data").is_none(), "{held}");
}

#[tokio::test]
async fn a_replace_drops_members_it_does_not_send() {
    let world = json!({"operations": {"putThing": {"kind": "replace"}}});
    let mut replaced = thing();
    replaced["title"] = json!("b");
    let h = support::described(CONTRACT, world, vec![thing(), replaced.clone()]);
    create_thing(&h).await;
    let (status, _, body) = h
        .send("PUT", "/things/t1", Some(json!({"title": "b"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let t = type_of(&h, "getThing");
    let held = h.stored(&t, "t1").await.unwrap();
    assert_eq!(
        held,
        json!({"id": "t1", "title": "b", "createdAt": "2026-01-01T00:00:00Z"})
    );

    // With no world, a put merges as today: what it did not send stays.
    let h = plain(vec![thing(), replaced]);
    create_thing(&h).await;
    h.send("PUT", "/things/t1", Some(json!({"title": "b"})), &[])
        .await;
    let held = h.stored(&t, "t1").await.unwrap();
    assert_eq!(held["note"], json!("n"), "{held}");
}

#[cfg(feature = "graphql")]
mod graphql {
    use std::sync::Arc;
    use std::time::Duration;

    use mb_engine::{Engine, EngineConfig};
    use mb_llm::{Provider, ScriptedProvider};
    use mb_protocols::MockRuntime;
    use mb_state::MemoryStore;
    use serde_json::{Value, json};

    const SDL: &str = r#"
type Widget { id: ID! name: String! }
type Query { widget(id: ID!): Widget }
type Mutation {
  createWidget(name: String!): Widget
  removeWidget(id: ID!): Boolean
}
"#;

    async fn post(router: &axum::Router, body: Value) -> Value {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        crate::support::send_to(router, req).await.2
    }

    #[tokio::test]
    async fn a_described_delete_on_a_graphql_mutation_removes_the_record() {
        let bundle = mb_import::compile(SDL.as_bytes()).unwrap();
        let t = bundle
            .operation("Query.widget")
            .and_then(|o| o.resource_type.clone())
            .unwrap();
        let world = json!({
            "operations": {
                "Mutation.removeWidget": {"kind": "delete", "type": t, "target": {"member": "id"}}
            }
        });
        let provider = Arc::new(ScriptedProvider::new(
            [
                json!({"__typename": "Widget", "id": "w1", "name": "a"}),
                json!(true),
                json!({"__typename": "Widget", "id": "w1", "name": "made up"}),
            ]
            .into_iter()
            .map(|v| Ok(v.to_string()))
            .collect(),
        ));
        let rt = MockRuntime::new(
            "described",
            Arc::new(bundle),
            Arc::new(Engine::new(
                Some(Arc::clone(&provider) as Arc<dyn Provider>),
                EngineConfig::default(),
            )),
            Arc::new(MemoryStore::new(Duration::from_secs(3600))),
        );
        let rt = crate::support::with_effective(rt, world, false);
        let router = mb_protocols::graphql::try_router(Arc::new(rt)).unwrap();
        let created = post(
            &router,
            json!({"query": "mutation { createWidget(name: \"a\") { id name } }"}),
        )
        .await;
        assert_eq!(
            created["data"]["createWidget"]["id"],
            json!("w1"),
            "{created}"
        );
        let read = json!({"query": "{ widget(id: \"w1\") { id name } }"});
        let before = post(&router, read.clone()).await;
        assert_eq!(before["data"]["widget"]["name"], json!("a"), "{before}");
        let removed = post(
            &router,
            json!({"query": "mutation { removeWidget(id: \"w1\") }"}),
        )
        .await;
        assert!(removed.get("errors").is_none(), "{removed}");
        let after = post(&router, read).await;
        assert_eq!(after["data"]["widget"], Value::Null, "{after}");
        assert_eq!(
            provider.requests().len(),
            2,
            "the read after is not generated"
        );
    }
}
