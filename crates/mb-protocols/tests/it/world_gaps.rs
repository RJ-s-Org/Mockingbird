//! The gap report of a world description: what the description does not say
//! that a world needs, routes the contract has but cannot serve, and entries
//! refused at validation.
#![cfg(test)]

use mb_import::compile;
use mb_import::world::tier0;
use mb_mcm::world::{RefusalCode, World, overlay, validate};
use mb_protocols::world_gaps::{GapCode, gaps};
use serde_json::json;

fn codes(
    gaps: &[mb_protocols::world_gaps::Gap],
    code: GapCode,
) -> Vec<&mb_protocols::world_gaps::Gap> {
    gaps.iter().filter(|g| g.code == code).collect()
}

#[test]
fn gaps_list_skipped_routes() {
    let b = compile(
        br##"
openapi: 3.0.0
info: {title: Twice, version: "1"}
paths:
  /a/{x}:
    get:
      operationId: byX
      parameters: [{name: x, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok}}
  /a/{y}:
    get:
      operationId: byY
      parameters: [{name: y, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok}}
"##,
    )
    .unwrap();
    let world = tier0(&b);
    let v = validate(&world, &b);
    let all = gaps(&b, &v.applied, &v.refused);
    let skipped = codes(&all, GapCode::SkippedRoute);
    assert_eq!(skipped.len(), 1, "{all:?}");
    assert_eq!(skipped[0].operation.as_deref(), Some("byY"));
    assert!(
        skipped[0].message.contains("/a/{y}"),
        "{}",
        skipped[0].message
    );
}

#[test]
fn gaps_list_no_identity_for_write_only_type() {
    let b = compile(
        br##"
openapi: 3.0.0
info: {title: Writes, version: "1"}
paths:
  /events:
    post:
      operationId: createEvent
      x-mockingbird-resource: event
      requestBody: {content: {application/json: {schema: {type: object, properties: {kind: {type: string}}}}}}
      responses: {"202": {description: queued}}
"##,
    )
    .unwrap();
    let t = b.operations[0].resource_type.clone().unwrap();
    let world = tier0(&b);
    assert!(
        world
            .types
            .get(&t)
            .and_then(|d| d.identity.as_ref())
            .is_none()
    );
    let v = validate(&world, &b);
    let all = gaps(&b, &v.applied, &v.refused);
    let none = codes(&all, GapCode::NoIdentity);
    assert_eq!(none.len(), 1, "{all:?}");
    assert_eq!(none[0].r#type.as_deref(), Some(t.as_str()));
    assert_eq!(none[0].operation, None);

    // Naming the identity closes the gap.
    let mut authored = World::default();
    authored.types.insert(
        t.clone(),
        serde_json::from_value(json!({"identity": {"member": "kind"}})).unwrap(),
    );
    let all = gaps(&b, &overlay(&v.applied, &authored), &[]);
    assert!(codes(&all, GapCode::NoIdentity).is_empty(), "{all:?}");
}

#[test]
fn gaps_list_no_target_and_no_record() {
    let b = compile(
        br##"
openapi: 3.0.0
info: {title: Wrapped, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      responses:
        "201":
          description: ok
          content: {application/json: {schema: {type: object, properties: {ok: {type: boolean}, note: {type: string}}}}}
  /widgets/{widgetId}:
    parameters: [{name: widgetId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getWidget
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}}
components:
  schemas:
    Widget: {type: object, properties: {id: {type: string}, name: {type: string}}}
"##,
    )
    .unwrap();
    let world = tier0(&b);
    let v = validate(&world, &b);
    assert!(v.refused.is_empty(), "{:?}", v.refused);
    let all = gaps(&b, &v.applied, &v.refused);
    let t = b.operation("getWidget").unwrap().resource_type.clone();
    assert!(t.is_some());
    let no_record = codes(&all, GapCode::NoRecord);
    assert_eq!(no_record.len(), 1, "{all:?}");
    assert_eq!(no_record[0].operation.as_deref(), Some("createWidget"));
    assert!(codes(&all, GapCode::NoTarget).is_empty(), "{all:?}");

    // Dropping the read's target opens a NoTarget gap for it.
    let mut world = v.applied.clone();
    world.operations.get_mut("getWidget").unwrap().target = None;
    let all = gaps(&b, &world, &[]);
    let no_target = codes(&all, GapCode::NoTarget);
    assert_eq!(no_target.len(), 1, "{all:?}");
    assert_eq!(no_target[0].operation.as_deref(), Some("getWidget"));
}

#[test]
fn gaps_read_a_dotted_member_name_as_a_literal_property_first() {
    // As validation and `member_at` read it: a property named `odata.id` is
    // the identity member, so a create answering the record has a record.
    let b = compile(
        br##"
openapi: 3.0.0
info: {title: Dotted, version: "1"}
paths:
  /docs:
    post:
      operationId: createDoc
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}}
  /docs/{docId}:
    parameters: [{name: docId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getDoc
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}}
components:
  schemas:
    Doc: {type: object, properties: {"odata.id": {type: string}, name: {type: string}}}
"##,
    )
    .unwrap();
    let t = b
        .operation("getDoc")
        .unwrap()
        .resource_type
        .clone()
        .unwrap();
    let mut authored = World::default();
    authored.types.insert(
        t.clone(),
        serde_json::from_value(json!({"identity": {"member": "odata.id"}})).unwrap(),
    );
    let v = validate(&overlay(&tier0(&b), &authored), &b);
    assert!(v.refused.is_empty(), "{:?}", v.refused);
    assert_eq!(
        v.applied.types[&t]
            .identity
            .as_ref()
            .and_then(|i| i.member.as_deref()),
        Some("odata.id")
    );
    let all = gaps(&b, &v.applied, &v.refused);
    assert!(codes(&all, GapCode::NoRecord).is_empty(), "{all:?}");
}

const GRAPHQL: &[u8] = br##"
type Issue { id: ID! title: String version: Int }
type Query { issue(id: ID!): Issue issues: [Issue!]! }
type Mutation {
  createIssue(title: String!): Issue
  updateIssue(id: ID!, title: String): Issue
}
"##;

#[test]
fn gaps_for_graphql_have_no_skipped_routes() {
    let b = compile(GRAPHQL).unwrap();
    let world = tier0(&b);
    let v = validate(&world, &b);
    assert!(v.refused.is_empty(), "{:?}", v.refused);
    let all = gaps(&b, &v.applied, &v.refused);
    assert!(codes(&all, GapCode::SkippedRoute).is_empty(), "{all:?}");
    // Tier 0 holds kind and type, keyed by operation id.
    assert!(!v.applied.operations.is_empty());
    assert!(v.applied.operations.values().all(|o| o.kind.is_some()));

    // An HTTP-only key on a GraphQL type is refused as not applicable,
    // and shows up as a refused gap; the rest still applies.
    let t = b
        .operations
        .iter()
        .find_map(|o| o.resource_type.clone())
        .unwrap();
    let mut authored = World::default();
    authored.types.insert(
        t.clone(),
        serde_json::from_value(json!({"concurrency": {"header": "If-Match"}})).unwrap(),
    );
    let v = validate(&overlay(&world, &authored), &b);
    assert_eq!(v.refused.len(), 1, "{:?}", v.refused);
    assert_eq!(v.refused[0].code, RefusalCode::NotApplicable);
    let all = gaps(&b, &v.applied, &v.refused);
    let refused = codes(&all, GapCode::Refused);
    assert_eq!(refused.len(), 1, "{all:?}");
    assert_eq!(refused[0].r#type.as_deref(), Some(t.as_str()));
}

#[test]
fn stale_entries_are_refused_not_fatal() {
    // An overlay written against an older contract: the `status` member
    // is gone from this version, the `title` filter still resolves.
    let b = compile(
        br##"
openapi: 3.0.0
info: {title: Stale, version: "2"}
paths:
  /notes:
    get:
      operationId: listNotes
      parameters: [{name: title, in: query, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Note"}}}}}}
  /notes/{noteId}:
    parameters: [{name: noteId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getNote
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
components:
  schemas:
    Note: {type: object, properties: {id: {type: string}, title: {type: string}}}
"##,
    )
    .unwrap();
    let t = b
        .operation("getNote")
        .unwrap()
        .resource_type
        .clone()
        .unwrap();
    let authored: World = serde_json::from_value(json!({
        "version": 1,
        "types": {t.clone(): {"lifecycle": {"member": "status"}}},
        "operations": {"listNotes": {"filters": [{"param": "title", "kind": "contains"}]}}
    }))
    .unwrap();
    let base = tier0(&b);
    let v = validate(&overlay(&base, &authored), &b);
    assert_eq!(v.refused.len(), 1, "{:?}", v.refused);
    assert_eq!(v.refused[0].code, RefusalCode::Unresolved);
    assert_eq!(v.refused[0].path, format!("/types/{t}/lifecycle/member"));
    // Everything else applies: tier 0 and the still-valid filter.
    assert_eq!(v.applied.operations["listNotes"].filters.len(), 1);
    assert_eq!(v.applied.operations["getNote"], base.operations["getNote"]);
    assert_eq!(v.applied.types[&t].identity, base.types[&t].identity);
    let all = gaps(&b, &v.applied, &v.refused);
    let refused = codes(&all, GapCode::Refused);
    assert_eq!(refused.len(), 1, "{all:?}");
    assert_eq!(refused[0].r#type.as_deref(), Some(t.as_str()));
    assert!(
        refused[0].message.contains("status"),
        "{}",
        refused[0].message
    );
}

/// One contract that signals each behaviour the new gaps point at.
const SIGNALS: &[u8] = br##"
openapi: 3.0.0
info: {title: Signals, version: "1"}
paths:
  /projects:
    get:
      operationId: listProjects
      parameters:
        - {name: sort, in: query, schema: {type: string}}
        - {name: name, in: query, schema: {type: string}}
        - {name: limit, in: query, schema: {type: integer}}
        - {name: page, in: query, schema: {type: integer}}
        - {name: include_deleted, in: query, schema: {type: boolean}}
        - {name: "filter[owner]", in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Project"}}}}
    post:
      operationId: createProject
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
      responses:
        "201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
        "409": {description: taken}
  /projects/{id}:
    get:
      operationId: getProject
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}}
    delete:
      operationId: deleteProject
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"204": {description: gone}}
  /projects/{id}/settings:
    patch:
      operationId: updateSettings
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}}
  /projects/{id}/archive:
    post:
      operationId: archiveProject
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}}
components:
  schemas:
    Project:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
        owner: {type: string}
        archived: {type: boolean}
        status: {type: string, enum: [active, deleted]}
        deleted_at: {type: string, format: date-time}
"##;

fn signal_gaps(overlay_doc: Option<serde_json::Value>) -> Vec<mb_protocols::world_gaps::Gap> {
    let b = compile(SIGNALS).unwrap();
    let mut world = tier0(&b);
    if let Some(doc) = overlay_doc {
        let parsed: World = serde_json::from_value(doc).unwrap();
        world = overlay(&world, &parsed);
    }
    let v = validate(&world, &b);
    assert!(v.refused.is_empty(), "{:?}", v.refused);
    gaps(&b, &v.applied, &v.refused)
}

#[test]
fn gaps_point_at_behaviour_the_contract_signals() {
    let all = signal_gaps(None);
    // `name` filters a member of the items and `limit` sizes the page, so the
    // executor reads both; the others it reads nothing for.
    let open = codes(&all, GapCode::UndescribedParam);
    assert_eq!(open.len(), 1, "{all:?}");
    for param in ["`sort`", "`include_deleted`", "`filter[owner]`", "`page`"] {
        assert!(open[0].message.contains(param), "{}", open[0].message);
    }
    for param in ["`name`", "`limit`"] {
        assert!(!open[0].message.contains(param), "{}", open[0].message);
    }
    assert!(!open[0].advisory);
    let unique = codes(&all, GapCode::NoUnique);
    assert_eq!(unique.len(), 1, "{all:?}");
    assert!(unique[0].advisory);
    // `archived` is a state of its own beside a hard delete; `deleted_at` hints
    // at a soft delete.
    let soft = codes(&all, GapCode::NoSoftDelete);
    assert_eq!(soft.len(), 1, "{all:?}");
    assert!(
        soft[0].message.contains("`deleted_at`"),
        "{}",
        soft[0].message
    );
    assert!(soft[0].advisory);
    // An update the route only looks like an action on (`/settings`, and
    // `/archive` that tier 0 reads as an update) is merged by the executor.
    assert!(codes(&all, GapCode::NoEffect).is_empty(), "{all:?}");
}

#[test]
fn an_action_with_nothing_to_change_is_an_advisory_gap() {
    let all = signal_gaps(Some(json!({
        "operations": {"archiveProject": {"kind": "action"}}
    })));
    let effect = codes(&all, GapCode::NoEffect);
    assert_eq!(effect.len(), 1, "{all:?}");
    assert_eq!(effect[0].operation.as_deref(), Some("archiveProject"));
    assert!(effect[0].advisory);
    let closed = signal_gaps(Some(json!({
        "operations": {"archiveProject": {"kind": "action", "changes": {"archived": {"value": true}}}}
    })));
    assert!(codes(&closed, GapCode::NoEffect).is_empty(), "{closed:?}");
}

#[test]
fn behaviour_gaps_close_when_described() {
    let all = signal_gaps(Some(json!({
        "types": {
            "projects": {
                "unique": [{"members": ["name"], "conflict": 409}],
                "lifecycle": {
                    "member": "status",
                    "states": ["active", "deleted"],
                    "soft_delete": {"state": "deleted", "listed_with": {"param": "include_deleted", "value": "true"}}
                }
            }
        },
        "operations": {
            "listProjects": {
                "filters": [{"param": "filter", "grammar": "deep_object"}],
                "paging": {"style": "page", "request": "page", "size": "limit"},
                "ignores": ["sort"]
            }
        }
    })));
    for code in [
        GapCode::NoEffect,
        GapCode::UndescribedParam,
        GapCode::NoUnique,
        GapCode::NoSoftDelete,
    ] {
        assert!(codes(&all, code).is_empty(), "{code}: {all:?}");
    }
}

#[test]
fn a_described_paging_replaces_the_page_size_detection() {
    let all = signal_gaps(Some(json!({
        "operations": {"listProjects": {"paging": {"style": "page", "request": "page"}}}
    })));
    let open = codes(&all, GapCode::UndescribedParam);
    assert_eq!(open.len(), 1, "{all:?}");
    assert!(open[0].message.contains("`limit`"), "{}", open[0].message);
    assert!(!open[0].message.contains("`page`"), "{}", open[0].message);
}
