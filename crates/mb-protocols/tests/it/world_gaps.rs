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

/// Gadgets: a read and an update by `gadgetId`, a create and a list, and a record
/// with no member the path parameter names.
const GADGETS: &str = r##"
openapi: 3.0.0
info: {title: Gadgets, version: "1"}
paths:
  /gadgets:
    get:
      operationId: listGadgets
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Gadget"}}}}}}
    post:
      operationId: createGadget
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}}
  /gadgets/{gadgetId}:
    parameters: [{name: gadgetId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getGadget
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}}
    patch:
      operationId: updateGadget
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}}
components:
  schemas:
    Gadget:
      type: object
      properties: {name: {type: string}, size: {type: string}}
"##;

#[test]
fn writes_with_a_read_by_id_are_identified_while_a_list_of_the_type_is_not() {
    let mut b = compile(GADGETS.as_bytes()).unwrap();
    let t = b
        .operation("getGadget")
        .unwrap()
        .resource_type
        .clone()
        .unwrap();
    // The read and the update name the record by `gadgetId`, a field the record
    // does not have: their own identity, and none for the type in tier 0.
    for op in b.operations.iter_mut() {
        if op.id == "getGadget" || op.id == "updateGadget" {
            op.identity = Some(mb_mcm::Identity {
                path_param: Some("gadgetId".into()),
                field: "gadgetId".into(),
                input_field: None,
                path_field: None,
                record: None,
            });
        }
    }
    let world = tier0(&b);
    assert!(
        world
            .types
            .get(&t)
            .and_then(|d| d.identity.as_ref())
            .is_none(),
        "{world:?}"
    );
    let v = validate(&world, &b);
    let applied = mb_mcm::world::apply(&b, &world, &v.applied);
    // The executor's rule: the create and the update work with the read by id's
    // identity, or their own; the list needs the type's identity member.
    let unidentified = mb_protocols::world_gaps::unidentified(&applied, &v.applied);
    assert_eq!(
        unidentified.get(&t).map(Vec::as_slice),
        Some(&["listGadgets".to_owned()][..]),
        "{unidentified:?}"
    );
    let all = gaps(&b, &v.applied, &v.refused);
    let none = codes(&all, GapCode::NoIdentity);
    assert_eq!(none.len(), 1, "{all:?}");
    assert_eq!(none[0].r#type.as_deref(), Some(t.as_str()));
    assert!(
        none[0].message.contains("`listGadgets`"),
        "{}",
        none[0].message
    );
    assert!(!none[0].message.contains("Gadget`"), "{}", none[0].message);
}

#[test]
fn identified_needs_an_identity_member_for_a_list() {
    use mb_mcm::world::{IdentityDesc, OpKind};
    use mb_protocols::world_gaps::identified;
    let with_member = IdentityDesc {
        member: Some("id".into()),
        ..IdentityDesc::default()
    };
    let no_member = IdentityDesc::default();
    // A list: its own identity, or its type's identity member; a read by id is
    // no help.
    assert!(identified(OpKind::List, true, None, false));
    assert!(identified(OpKind::List, false, Some(&with_member), false));
    assert!(!identified(OpKind::List, false, Some(&no_member), false));
    assert!(!identified(OpKind::List, false, None, true));
    // Any other kind: its own, its type's (member or not), or a read by id's.
    for kind in [
        OpKind::Create,
        OpKind::Read,
        OpKind::Update,
        OpKind::Delete,
        OpKind::Action,
    ] {
        assert!(identified(kind, true, None, false), "{kind:?}");
        assert!(identified(kind, false, Some(&no_member), false), "{kind:?}");
        assert!(identified(kind, false, None, true), "{kind:?}");
        assert!(!identified(kind, false, None, false), "{kind:?}");
    }
}

/// A type whose `state` lists its values only in prose, set by an action's
/// narrow request; and a type whose enum'd `kind` nothing but the record names.
const STATES: &[u8] = br##"
openapi: 3.0.0
info: {title: States, version: "1"}
paths:
  /jobs:
    post:
      operationId: createJob
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Job"}}}}}
  /jobs/{id}:
    get:
      operationId: getJob
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Job"}}}}}
  /jobs/{id}/stop:
    post:
      operationId: stopJob
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {State: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Job"}}}}}
  /notes:
    post:
      operationId: createNote
      requestBody: {content: {application/json: {schema: {type: object, properties: {kind: {type: string}}}}}}
      responses: {"201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
  /notes/{id}:
    get:
      operationId: getNote
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
    patch:
      operationId: updateNote
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
components:
  schemas:
    Job:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
        owner: {type: string}
        created: {type: string}
        state: {type: string, nullable: true, description: "The job's state. Can be: `queued`, `running`, `stopped` or `failed`."}
    Note:
      type: object
      properties:
        id: {type: string}
        text: {type: string}
        kind: {type: string, enum: [plain, rich]}
        created: {type: string}
"##;

fn state_gaps(overlay_doc: Option<serde_json::Value>) -> Vec<mb_protocols::world_gaps::Gap> {
    let b = compile(STATES).unwrap();
    let mut world = tier0(&b);
    world.operations.entry("stopJob".into()).or_default().kind =
        Some(mb_mcm::world::OpKind::Action);
    if let Some(doc) = overlay_doc {
        let parsed: World = serde_json::from_value(doc).unwrap();
        world = overlay(&world, &parsed);
    }
    let v = validate(&world, &b);
    assert!(v.refused.is_empty(), "{:?}", v.refused);
    gaps(&b, &v.applied, &v.refused)
}

#[test]
fn a_state_listed_in_prose_and_set_by_an_action_needs_a_lifecycle() {
    let all = state_gaps(None);
    let missing = codes(&all, GapCode::NoLifecycle);
    assert_eq!(missing.len(), 1, "{all:?}");
    let g = missing[0];
    assert_eq!(g.operation.as_deref(), Some("stopJob"));
    assert!(!g.advisory);
    for part in [
        "`state`",
        "`queued`, `running`, `stopped`, `failed`",
        "lifecycle",
        "initial",
        "transition",
    ] {
        assert!(g.message.contains(part), "{part}: {}", g.message);
    }

    // Declaring the lifecycle closes it.
    let t = g.r#type.clone().unwrap();
    let closed = state_gaps(Some(json!({
        "types": {t: {"lifecycle": {
            "member": "state",
            "states": ["queued", "running", "stopped", "failed"],
            "initial": "queued"
        }}},
        "operations": {"stopJob": {"kind": "action", "transition": {"state": "stopped"}}}
    })));
    assert!(
        codes(&closed, GapCode::NoLifecycle).is_empty(),
        "{closed:?}"
    );
}

#[test]
fn an_enum_member_no_request_sets_is_no_state() {
    // `kind` lists its values, but only the create names it: nothing marks it a
    // state, so the note type gets no `no_lifecycle`.
    let b = compile(STATES).unwrap();
    let note = b
        .operation("getNote")
        .and_then(|op| op.resource_type.clone())
        .unwrap();
    let all = state_gaps(None);
    let missing = codes(&all, GapCode::NoLifecycle);
    assert_eq!(missing.len(), 1, "{all:?}");
    assert_ne!(missing[0].r#type.as_deref(), Some(note.as_str()), "{all:?}");
    assert!(
        !missing[0].message.contains("`kind`"),
        "{}",
        missing[0].message
    );
}

#[test]
fn a_request_restating_the_whole_record_marks_no_state() {
    // `updateSettings` sends the whole project, `status` among every other
    // member: that sets no member apart as a state.
    let all = signal_gaps(None);
    assert!(codes(&all, GapCode::NoLifecycle).is_empty(), "{all:?}");
}

#[test]
fn a_verification_whose_status_values_are_prose_needs_a_lifecycle() {
    // Trimmed from a real contract: the record's `status` is a nullable string
    // whose values only its description lists, and the update sets `Status`.
    let b = compile(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/openapi/twilio-verify-v2-verifications.json"
    )))
    .unwrap();
    let world = tier0(&b);
    let v = validate(&world, &b);
    let all = gaps(&b, &v.applied, &v.refused);
    let missing = codes(&all, GapCode::NoLifecycle);
    assert_eq!(missing.len(), 1, "{all:?}");
    let g = missing[0];
    assert!(g.message.contains("`status`"), "{}", g.message);
    for state in [
        "pending",
        "approved",
        "canceled",
        "max_attempts_reached",
        "deleted",
        "failed",
        "expired",
    ] {
        assert!(g.message.contains(&format!("`{state}`")), "{}", g.message);
    }
}

/// A contract of one type `things`: its record's `members`, the create's body
/// `create`, an update `updateThing` with body `update` (when given) and query
/// `query` (when given), and an action `finishThing` with body `action` (when
/// given). Each is a YAML flow mapping of properties. `extra` adds schemas.
fn things(
    members: &str,
    create: &str,
    update: Option<&str>,
    query: Option<&str>,
    action: Option<&str>,
    extra: &str,
) -> String {
    let body = |props: &str| {
        format!(
            "requestBody: {{content: {{application/json: {{schema: {{type: object, properties: {props}}}}}}}}}"
        )
    };
    let answer = r##"responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}"##;
    let id = "{name: id, in: path, required: true, schema: {type: string}}";
    let mut src = format!(
        "openapi: 3.1.0\ninfo: {{title: Things, version: \"1\"}}\npaths:\n  /things:\n    post:\n      operationId: createThing\n      {}\n      {answer}\n  /things/{{id}}:\n    get:\n      operationId: getThing\n      parameters: [{id}]\n      {answer}\n",
        body(create)
    );
    if update.is_some() || query.is_some() {
        let params = query.map_or(String::new(), |q| {
            format!(", {{name: {q}, in: query, schema: {{type: string}}}}")
        });
        src.push_str(&format!(
            "    patch:\n      operationId: updateThing\n      parameters: [{id}{params}]\n      {}\n      {answer}\n",
            update.map_or(String::new(), body)
        ));
    }
    if let Some(a) = action {
        src.push_str(&format!(
            "  /things/{{id}}/finish:\n    post:\n      operationId: finishThing\n      parameters: [{id}]\n      {}\n      {answer}\n",
            body(a)
        ));
    }
    src.push_str(&format!(
        "components:\n  schemas:\n    Thing: {{type: object, properties: {members}}}\n{extra}"
    ));
    src
}

/// The `no_lifecycle` gaps of a [`things`] contract, `finishThing` an action.
fn lifecycle_gaps(src: &str) -> Vec<mb_protocols::world_gaps::Gap> {
    let b = compile(src.as_bytes()).unwrap_or_else(|e| panic!("{e}\n{src}"));
    let mut world = tier0(&b);
    if b.operation("finishThing").is_some() {
        world
            .operations
            .entry("finishThing".into())
            .or_default()
            .kind = Some(mb_mcm::world::OpKind::Action);
    }
    let v = validate(&world, &b);
    assert!(v.refused.is_empty(), "{:?}", v.refused);
    gaps(&b, &v.applied, &v.refused)
        .into_iter()
        .filter(|g| g.code == GapCode::NoLifecycle)
        .collect()
}

const PROSE_STATE: &str = r#"{type: string, description: "Can be: `open`, `held` or `closed`."}"#;

#[test]
fn a_query_parameter_sets_no_state() {
    let members = format!(
        "{{id: {{type: string}}, name: {{type: string}}, owner: {{type: string}}, state: {PROSE_STATE}}}"
    );
    let by_query = things(
        &members,
        "{name: {type: string}}",
        None,
        Some("state"),
        None,
        "",
    );
    assert!(lifecycle_gaps(&by_query).is_empty());
    // The same member sent in the body is a state.
    let by_body = things(
        &members,
        "{name: {type: string}}",
        Some("{state: {type: string}}"),
        None,
        None,
        "",
    );
    assert_eq!(lifecycle_gaps(&by_body).len(), 1);
}

#[test]
fn request_names_match_ignoring_case_separators_and_a_plural() {
    let members = format!(
        "{{id: {{type: string}}, name: {{type: string}}, owner: {{type: string}}, life_state: {PROSE_STATE}}}"
    );
    let src = things(
        &members,
        "{name: {type: string}}",
        Some("{Life-States: {type: string}}"),
        None,
        None,
        "",
    );
    let found = lifecycle_gaps(&src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].message.contains("`life_state`"),
        "{}",
        found[0].message
    );
}

#[test]
fn a_body_naming_more_than_half_the_record_sets_no_state() {
    // Three members: a body naming one of them sets a state, one naming two
    // restates the record. The create sets neither, so only the half rule
    // tells them apart.
    let members = format!("{{id: {{type: string}}, name: {{type: string}}, state: {PROSE_STATE}}}");
    let narrow = things(
        &members,
        "{}",
        Some("{state: {type: string}}"),
        None,
        None,
        "",
    );
    assert_eq!(lifecycle_gaps(&narrow).len(), 1);
    let restating = things(
        &members,
        "{}",
        Some("{name: {type: string}, state: {type: string}}"),
        None,
        None,
        "",
    );
    assert!(lifecycle_gaps(&restating).is_empty());
}

#[test]
fn example_spans_and_unpaired_backticks_list_no_values() {
    for description in [
        "A label, e.g. `alpha`, `beta` or `gamma`.",
        "A label such as `alpha` or `beta`.",
        "Can be `alpha`, `beta` or `gamma.",
    ] {
        let members = format!(
            "{{id: {{type: string}}, name: {{type: string}}, owner: {{type: string}}, label: {{type: string, description: {description:?}}}}}"
        );
        let src = things(
            &members,
            "{name: {type: string}}",
            Some("{label: {type: string}}"),
            None,
            None,
            "",
        );
        assert!(lifecycle_gaps(&src).is_empty(), "{description}");
    }
}

#[test]
fn a_nullable_type_list_and_a_referenced_enum_are_listed_values() {
    let nullable = "{id: {type: string}, name: {type: string}, owner: {type: string}, state: {type: [string, \"null\"], description: \"One of `open`, `held`, `closed`.\"}}";
    let src = things(
        nullable,
        "{name: {type: string}}",
        Some("{state: {type: string}}"),
        None,
        None,
        "",
    );
    let found = lifecycle_gaps(&src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].message.contains("`held`"), "{}", found[0].message);

    let referenced = "{id: {type: string}, name: {type: string}, owner: {type: string}, state: {$ref: \"#/components/schemas/State\"}}";
    let src = things(
        referenced,
        "{name: {type: string}}",
        Some("{state: {type: string}}"),
        None,
        None,
        "    State: {type: string, enum: [open, held, closed]}\n",
    );
    let found = lifecycle_gaps(&src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].message.contains("`closed`"),
        "{}",
        found[0].message
    );
}

#[test]
fn a_description_listing_more_than_the_enum_gives_the_values() {
    let members = "{id: {type: string}, name: {type: string}, owner: {type: string}, state: {type: string, enum: [open, closed], description: \"Can be `open`, `held` or `closed`.\"}}";
    let src = things(
        members,
        "{name: {type: string}}",
        Some("{state: {type: string}}"),
        None,
        None,
        "",
    );
    let found = lifecycle_gaps(&src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].message.contains("`held`"), "{}", found[0].message);
}

#[test]
fn of_two_named_members_the_one_only_actions_set_is_the_state() {
    // `mode` comes first and an update sets it; `phase` only the action sets.
    let members = "{id: {type: string}, name: {type: string}, owner: {type: string}, created: {type: string}, mode: {type: string, enum: [fast, slow]}, phase: {type: string, enum: [open, done]}}";
    let src = things(
        members,
        "{name: {type: string}}",
        Some("{mode: {type: string}}"),
        None,
        Some("{phase: {type: string}}"),
        "",
    );
    let found = lifecycle_gaps(&src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].operation.as_deref(), Some("finishThing"));
    assert!(found[0].message.contains("`phase`"), "{}", found[0].message);
}

#[test]
fn a_setting_the_create_sets_is_no_state() {
    let members = "{id: {type: string}, name: {type: string}, owner: {type: string}, visibility: {type: string, enum: [public, private]}}";
    let src = things(
        members,
        "{name: {type: string}, visibility: {type: string}}",
        Some("{visibility: {type: string}}"),
        None,
        None,
        "",
    );
    assert!(lifecycle_gaps(&src).is_empty());
    // Marked read-only on the create, it is the API's to set.
    let src = things(
        members,
        "{name: {type: string}, visibility: {type: string, readOnly: true}}",
        Some("{visibility: {type: string}}"),
        None,
        None,
        "",
    );
    assert_eq!(lifecycle_gaps(&src).len(), 1);
}
