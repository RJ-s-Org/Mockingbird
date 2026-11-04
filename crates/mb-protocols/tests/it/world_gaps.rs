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
