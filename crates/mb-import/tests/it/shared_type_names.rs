//! One collection name, several objects: a type is split by what its routes answer,
//! so two objects never share a store, while one object under several routes stays
//! one type.
use mb_import::compile;
use mb_mcm::Bundle;

fn type_of<'a>(bundle: &'a Bundle, op: &str) -> Option<&'a str> {
    bundle
        .operations
        .iter()
        .find(|o| o.id == op)
        .and_then(|o| o.resource_type.as_deref())
}

/// A route answering `schema` (a `$ref` name, or `[name]` for a list of them).
fn route(id: &str, schema: &str) -> String {
    let schema = match schema.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(item) => format!("{{type: array, items: {{$ref: '#/components/schemas/{item}'}}}}"),
        None => format!("{{$ref: '#/components/schemas/{schema}'}}"),
    };
    format!(
        "operationId: {id}\n      responses:\n        '200':\n          description: OK\n          content: {{application/json: {{schema: {schema}}}}}"
    )
}

/// The org and user variants of one object answer one definition, and both create
/// and read it by its id: one type, so a migration started under the org is the one
/// the user's route reads. A list answering a summary of the object, with no read
/// or create of its own, is a view of the same records and stays too.
#[test]
fn one_object_under_several_routes_stays_one_type() {
    let contract = format!(
        r#"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /orgs/{{org}}/migrations:
    parameters: [{{name: org, in: path, required: true, schema: {{type: string}}}}]
    get:
      {}
    post:
      {}
  /orgs/{{org}}/migrations/{{migration_id}}:
    parameters:
      - {{name: org, in: path, required: true, schema: {{type: string}}}}
      - {{name: migration_id, in: path, required: true, schema: {{type: integer}}}}
    get:
      {}
  /user/migrations:
    get:
      {}
    post:
      {}
  /user/migrations/{{migration_id}}:
    parameters: [{{name: migration_id, in: path, required: true, schema: {{type: integer}}}}]
    get:
      {}
  /users/{{username}}/migrations:
    parameters: [{{name: username, in: path, required: true, schema: {{type: string}}}}]
    get:
      {}
components:
  schemas:
    migration:
      type: object
      properties: {{id: {{type: integer}}, state: {{type: string}}}}
    migration-simple:
      type: object
      properties: {{id: {{type: integer}}}}
"#,
        route("listOrgMigrations", "[migration]"),
        route("startOrgMigration", "migration"),
        route("getOrgMigration", "migration"),
        route("listUserMigrations", "[migration]"),
        route("startUserMigration", "migration"),
        route("getUserMigration", "migration"),
        route("listMigrationsOfUser", "[migration-simple]"),
    );
    let bundle = compile(contract.as_bytes()).unwrap();
    for op in [
        "listOrgMigrations",
        "startOrgMigration",
        "getOrgMigration",
        "listUserMigrations",
        "startUserMigration",
        "getUserMigration",
        "listMigrationsOfUser",
    ] {
        assert_eq!(type_of(&bundle, op), Some("migrations"), "{op}");
    }
}

/// Stripe's `transactions`: four objects, none named for the type. The one on the
/// shortest route with the most operations keeps the name, the others take their
/// definitions' names, and an action on a split-off record acts under its type.
#[test]
fn objects_none_named_for_the_type_take_their_definitions_names() {
    let contract = format!(
        r#"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /v1/issuing/transactions:
    get:
      {}
  /v1/issuing/transactions/{{transaction}}:
    parameters: [{{name: transaction, in: path, required: true, schema: {{type: string}}}}]
    get:
      {}
    post:
      {}
  /v1/treasury/transactions/{{id}}:
    parameters: [{{name: id, in: path, required: true, schema: {{type: string}}}}]
    get:
      {}
  /v1/treasury/transactions/{{id}}/void:
    parameters: [{{name: id, in: path, required: true, schema: {{type: string}}}}]
    post:
      operationId: voidTreasuryTransaction
      responses:
        '204': {{description: Voided}}
components:
  schemas:
    issuing.transaction:
      type: object
      properties: {{id: {{type: string}}, amount: {{type: integer}}}}
    treasury.transaction:
      type: object
      properties: {{id: {{type: string}}, flow: {{type: string}}}}
"#,
        route("listIssuing", "[issuing.transaction]"),
        route("getIssuing", "issuing.transaction"),
        route("updateIssuing", "issuing.transaction"),
        route("getTreasury", "treasury.transaction"),
    );
    let bundle = compile(contract.as_bytes()).unwrap();
    for op in ["listIssuing", "getIssuing", "updateIssuing"] {
        assert_eq!(type_of(&bundle, op), Some("transactions"), "{op}");
    }
    assert_eq!(
        type_of(&bundle, "getTreasury"),
        Some("treasury.transaction")
    );
    assert_eq!(
        type_of(&bundle, "voidTreasuryTransaction"),
        Some("treasury.transaction")
    );
}

/// A split-off definition whose name another type has for another object is
/// prefixed with the type it was split from; a type named for the very definition
/// it holds is that object's type, so the split-off routes join it.
#[test]
fn a_split_name_another_type_has_is_prefixed_or_joined() {
    let keys = |third: &str| {
        format!(
            r#"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /user/keys/{{key_id}}:
    parameters: [{{name: key_id, in: path, required: true, schema: {{type: integer}}}}]
    get:
      {}
  /repos/{{repo}}/keys/{{key_id}}:
    parameters:
      - {{name: repo, in: path, required: true, schema: {{type: string}}}}
      - {{name: key_id, in: path, required: true, schema: {{type: integer}}}}
    get:
      {}
  /deploy-key/{{id}}:
    parameters: [{{name: id, in: path, required: true, schema: {{type: integer}}}}]
    get:
      {}
components:
  schemas:
    key:
      type: object
      properties: {{id: {{type: integer}}}}
    deploy-key:
      type: object
      properties: {{id: {{type: integer}}, read_only: {{type: boolean}}}}
    audit:
      type: object
      properties: {{id: {{type: integer}}, action: {{type: string}}}}
"#,
            route("getSshKey", "key"),
            route("getDeployKey", "deploy-key"),
            route("getOther", third),
        )
    };
    let bundle = compile(keys("audit").as_bytes()).unwrap();
    assert_eq!(type_of(&bundle, "getSshKey"), Some("keys"));
    assert_eq!(type_of(&bundle, "getDeployKey"), Some("keys.deploy-key"));
    assert_eq!(type_of(&bundle, "getOther"), Some("deploy-key"));

    let bundle = compile(keys("deploy-key").as_bytes()).unwrap();
    assert_eq!(type_of(&bundle, "getSshKey"), Some("keys"));
    assert_eq!(type_of(&bundle, "getDeployKey"), Some("deploy-key"));
    assert_eq!(type_of(&bundle, "getOther"), Some("deploy-key"));
}

/// Beside two objects that split, a list answering a summary definition with no read
/// or create of its own is a view of the records the type's name keeps, and an
/// answer whose definition declares no members (`empty-object`) ties no routes
/// together nor holds anything.
#[test]
fn a_view_stays_with_the_name_and_empty_answers_hold_nothing() {
    let contract = format!(
        r#"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /user/keys:
    put:
      {}
  /user/keys/{{key_id}}:
    parameters: [{{name: key_id, in: path, required: true, schema: {{type: integer}}}}]
    get:
      {}
  /repos/{{repo}}/keys:
    parameters: [{{name: repo, in: path, required: true, schema: {{type: string}}}}]
    put:
      {}
  /repos/{{repo}}/keys/{{key_id}}:
    parameters:
      - {{name: repo, in: path, required: true, schema: {{type: string}}}}
      - {{name: key_id, in: path, required: true, schema: {{type: integer}}}}
    get:
      {}
  /users/{{username}}/keys:
    parameters: [{{name: username, in: path, required: true, schema: {{type: string}}}}]
    get:
      {}
  /orgs/{{org}}/keys/{{key_id}}:
    parameters:
      - {{name: org, in: path, required: true, schema: {{type: string}}}}
      - {{name: key_id, in: path, required: true, schema: {{type: integer}}}}
    get:
      {}
components:
  schemas:
    key:
      type: object
      properties: {{id: {{type: integer}}, title: {{type: string}}}}
    deploy-key:
      type: object
      properties: {{id: {{type: integer}}, read_only: {{type: boolean}}}}
    key-simple:
      type: object
      properties: {{id: {{type: integer}}}}
    empty-object:
      type: object
      properties: {{}}
"#,
        route("touchSshKeys", "empty-object"),
        route("getSshKey", "key"),
        route("touchDeployKeys", "empty-object"),
        route("getDeployKey", "deploy-key"),
        route("listKeysOfUser", "[key-simple]"),
        route("getOrgKey", "empty-object"),
    );
    let bundle = compile(contract.as_bytes()).unwrap();
    for op in ["touchSshKeys", "getSshKey", "listKeysOfUser", "getOrgKey"] {
        assert_eq!(type_of(&bundle, op), Some("keys"), "{op}");
    }
    for op in ["touchDeployKeys", "getDeployKey"] {
        assert_eq!(type_of(&bundle, op), Some("deploy-key"), "{op}");
    }
}

/// An object only a create holds splits off when it answers a keyed record no read
/// lists; one answering a receipt (no key) beside a read of other records stays.
/// Two types splitting off one definition give it one name (here `membership`, named
/// for `memberships`, keeps it).
#[test]
fn creates_hold_keyed_records_and_one_definition_gets_one_name() {
    let contract = format!(
        r#"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /v1/checkout/sessions:
    post:
      {}
  /v1/checkout/sessions/{{session}}:
    parameters: [{{name: session, in: path, required: true, schema: {{type: string}}}}]
    get:
      {}
  /v1/billing_portal/sessions:
    post:
      {}
  /v1/receipts/sessions:
    post:
      {}
  /user/memberships/orgs/{{org}}:
    parameters: [{{name: org, in: path, required: true, schema: {{type: string}}}}]
    get:
      {}
  /orgs/{{org}}:
    parameters: [{{name: org, in: path, required: true, schema: {{type: string}}}}]
    get:
      {}
  /orgs/{{org}}/memberships/{{username}}:
    parameters:
      - {{name: org, in: path, required: true, schema: {{type: string}}}}
      - {{name: username, in: path, required: true, schema: {{type: string}}}}
    get:
      {}
  /teams/{{team}}/memberships/{{username}}:
    parameters:
      - {{name: team, in: path, required: true, schema: {{type: string}}}}
      - {{name: username, in: path, required: true, schema: {{type: string}}}}
    get:
      {}
components:
  schemas:
    checkout.session:
      type: object
      properties: {{id: {{type: string}}, url: {{type: string}}}}
    billing_portal.session:
      type: object
      properties: {{id: {{type: string}}, return_url: {{type: string}}}}
    receipt:
      type: object
      properties: {{snapshot_id: {{type: string}}}}
    organization:
      type: object
      properties: {{login: {{type: string}}, id: {{type: integer}}}}
    org-membership:
      type: object
      properties: {{state: {{type: string}}, role: {{type: string}}}}
    membership:
      type: object
      properties: {{state: {{type: string}}, role: {{type: string}}, team: {{type: string}}}}
"#,
        route("createCheckout", "checkout.session"),
        route("getCheckout", "checkout.session"),
        route("createPortal", "billing_portal.session"),
        route("createReceipt", "receipt"),
        route("getMyMembership", "org-membership"),
        route("getOrg", "organization"),
        route("getOrgMembership", "org-membership"),
        route("getTeamMembership", "membership"),
    );
    let bundle = compile(contract.as_bytes()).unwrap();
    assert_eq!(type_of(&bundle, "createCheckout"), Some("sessions"));
    assert_eq!(type_of(&bundle, "getCheckout"), Some("sessions"));
    assert_eq!(
        type_of(&bundle, "createPortal"),
        Some("billing_portal.session")
    );
    assert_eq!(type_of(&bundle, "createReceipt"), Some("sessions"));
    assert_eq!(type_of(&bundle, "getOrg"), Some("orgs"));
    assert_eq!(type_of(&bundle, "getTeamMembership"), Some("memberships"));
    assert_eq!(type_of(&bundle, "getMyMembership"), Some("org-membership"));
    assert_eq!(type_of(&bundle, "getOrgMembership"), Some("org-membership"));
}
