//! Owner ties on a compound path parameter (`{organization_id_or_slug}`): a record's
//! own member is not its owner, and a key the route did not use says nothing.
use super::*;
use serde_json::json;
use std::sync::Arc;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: t, version: "1"}
paths:
  /organizations/{organization_id_or_slug}/projects/{project_slug}:
    get:
      operationId: getProject
      parameters:
        - {name: organization_id_or_slug, in: path, required: true, schema: {type: string}}
        - {name: project_slug, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
  /users/{user_id_or_username}/followers/{id}:
    get:
      operationId: getFollower
      parameters:
        - {name: user_id_or_username, in: path, required: true, schema: {type: string}}
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
components:
  schemas:
    Project:
      type: object
      properties:
        id: {type: string}
        slug: {type: string}
        organization_id: {type: string}
    User:
      type: object
      properties:
        id: {type: string}
        username: {type: string}
"##;

fn runtime() -> MockRuntime {
    MockRuntime::new(
        "m",
        Arc::new(mb_import::compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(mb_engine::Engine::new(
            None,
            mb_engine::EngineConfig::default(),
        )),
        Arc::new(mb_state::MemoryStore::new(std::time::Duration::from_secs(
            60,
        ))),
    )
}

fn owner(param: &str, value: &str, name: &str, collection: &str) -> Owner {
    Owner {
        collection: Collection {
            template: String::new(),
            path: collection.into(),
        },
        param: param.into(),
        value: value.into(),
        name: name.into(),
        tail: format!("{value}/{name}"),
        writing: true,
    }
}

// K1: the stem of a compound parameter is a prefix of the record's OWN member.
#[test]
fn k1_a_records_own_username_is_not_its_owner() {
    assert!(
        !tied_to_param("username", "user_id_or_username"),
        "`username` (the follower's own) is tied to the owner parameter"
    );
}

// K1: `/users/alice/followers/bob`, record bob: is bob known to be another's?
#[test]
fn k1_follower_is_not_foreign_to_its_route_owner() {
    let rt = runtime();
    let bob = StoredResource {
        resource_type: "followers".into(),
        id: "u2".into(),
        data: json!({"id": "u2", "username": "bob"}),
    };
    let o = owner(
        "user_id_or_username",
        "alice",
        "u2",
        "/users/alice/followers",
    );
    assert_ne!(
        owner_evidence(&rt, &bob, &o),
        Some(false),
        "bob's own username is read as naming another owner"
    );
}

// K1: a record holding only the owner's id, reached by the owner's slug.
#[test]
fn k1_owner_id_mismatch_under_a_slug_is_not_another_owner() {
    let rt = runtime();
    let project = StoredResource {
        resource_type: "projects".into(),
        id: "p1".into(),
        data: json!({"id": "p1", "slug": "web", "organization_id": "o_17"}),
    };
    let o = owner(
        "organization_id_or_slug",
        "acme",
        "web",
        "/organizations/acme/projects",
    );
    // Before the PR this was None (no tie): a write acted. Now `organization_id` ties,
    // holds "o_17" != "acme", and the record is known_foreign -> write leaves it.
    assert_ne!(
        owner_evidence(&rt, &project, &o),
        Some(false),
        "the owner's id is taken as naming another owner when the path used its slug"
    );
}
