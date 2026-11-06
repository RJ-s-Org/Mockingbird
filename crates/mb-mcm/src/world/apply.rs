//! Writing a world description into a copy of the bundle it describes.
//!
//! The entries the bundle already models (an operation's kind, resource type
//! and [`Identity`]: target, record pointer, and its type's identity member)
//! are written into the operations, so every heuristic that reads `op.kind`
//! or `op.identity` follows the description with no change of its own.
//! [`apply`] writes only where the description differs from tier 0, the base
//! the bundle's own values restate, so a tier-0 world changes nothing.

use crate::{Bundle, Effect, Identity, Operation, OperationKind, RecordAt};

use super::{OpDesc, OpKind, Pointer, TargetDesc, World};

/// The record location a world description's record pointer names: the three
/// forms the bundle has (`/m` → [`RecordAt::Member`], `/0` → [`RecordAt::Item`],
/// `/m/0` → [`RecordAt::MemberItem`]) where they fit, else
/// [`RecordAt::Pointer`] with its tokens. Not called for the root pointer `""`,
/// which is no location: the answer itself is the record.
#[must_use]
pub fn record_at(p: &Pointer) -> RecordAt {
    let tokens = p.tokens();
    match tokens.as_slice() {
        [item] if item == "0" => RecordAt::Item,
        [member] => RecordAt::Member(member.clone()),
        [member, item] if item == "0" && member != "0" => RecordAt::MemberItem(member.clone()),
        _ => RecordAt::Pointer(tokens),
    }
}

/// The record location of a set `answer.record`: `None` for the root pointer.
fn record_of(p: &Pointer) -> Option<RecordAt> {
    (!p.is_root()).then(|| record_at(p))
}

/// `world`'s value when it is set and differs from `base`'s.
fn changed<'a, T: PartialEq>(world: Option<&'a T>, base: Option<&T>) -> Option<&'a T> {
    world.filter(|w| Some(*w) != base)
}

/// `bundle` with `world` written into its operations where `world` differs
/// from `base`, which is `tier0(bundle)`, computed once by the caller.
///
/// Field by field, and only where `world` sets a value that differs from
/// `base`'s; an absent value keeps the bundle's:
/// - `kind`: create, update, replace, delete and action are the commands
///   (replace is an update; action is [`Effect::Other`]); read is a query with
///   an identity; list is a query with none, so a set `kind: list` clears the
///   operation's identity, and its target and record are then not written;
/// - `type`: the resource type;
/// - `target`: the identity's `path_param` and `input_field`, both written
///   from `world`'s target where it differs from `base`'s, a member it leaves
///   out cleared (an authored `{member}` drops the `path_param` tier 0 took);
///   a set `kind: create` that `world` gives no target clears the
///   `path_param` its route does not name the new record by (a `POST`'s), as
///   [`effective`](super::effective) drops it then, and keeps a `PUT`'s whose
///   last path segment it is;
/// - `answer.record`: the identity's `record`, the root pointer `""` as `None`
///   (the answer is the record);
/// - a type's `identity.member`: the identity `field` of each operation of the
///   type that has an identity.
///
/// An operation of kind create, read, update, replace, delete or action with no
/// identity gets one when `world` differs from `base` in its type's identity
/// member, its target or its record, or sets a kind other than create that
/// differs from `base`'s: its field is the type's effective identity member
/// (else `id`), and its target and record are the effective ones (`world`'s,
/// else `base`'s). Streaming operations are left alone.
#[must_use]
pub fn apply(bundle: &Bundle, base: &World, world: &World) -> Bundle {
    let mut out = bundle.clone();
    for op in &mut out.operations {
        apply_op(op, base, world);
    }
    out
}

fn apply_op(op: &mut Operation, base: &World, world: &World) {
    if matches!(
        op.kind,
        OperationKind::StreamServer | OperationKind::StreamClient | OperationKind::StreamBidi
    ) {
        return;
    }
    let none = OpDesc::default();
    let b = base.operations.get(&op.id).unwrap_or(&none);
    let w = world.operations.get(&op.id).unwrap_or(&none);

    let retyped = changed(w.r#type.as_ref(), b.r#type.as_ref());
    if let Some(t) = retyped {
        op.resource_type = Some(t.clone());
    }

    // The type's identity member: `world`'s where it differs from `base`'s,
    // and on an operation moved to another type, that type's effective one.
    let t = op.resource_type.clone();
    let member_of = |w: &World, t: &str| {
        w.types
            .get(t)
            .and_then(|d| d.identity.as_ref())
            .and_then(|i| i.member.clone())
    };
    let (world_member, base_member) = match t.as_deref() {
        Some(t) => (member_of(world, t), member_of(base, t)),
        None => (None, None),
    };
    let effective_member = world_member.clone().or_else(|| base_member.clone());
    let member = changed(world_member.as_ref(), base_member.as_ref())
        .cloned()
        .or_else(|| retyped.and(effective_member.clone()));

    let kind = changed(w.kind.as_ref(), b.kind.as_ref()).copied();
    if let Some(kind) = kind {
        op.kind = match kind {
            OpKind::Create => OperationKind::Command(Effect::Create),
            OpKind::Update | OpKind::Replace => OperationKind::Command(Effect::Update),
            OpKind::Delete => OperationKind::Command(Effect::Delete),
            OpKind::Action => OperationKind::Command(Effect::Other),
            OpKind::Read | OpKind::List => OperationKind::Query,
        };
    }
    let effective_kind = w.kind.or(b.kind);
    if effective_kind == Some(OpKind::List) {
        if kind.is_some() {
            op.identity = None;
        }
        return;
    }

    let wt = w.target.as_ref();
    let bt = b.target.as_ref();
    // A target is written whole where `world` sets one that differs from `base`'s,
    // or drops `base`'s by making the operation a create whose route does not name
    // the new record by `base`'s path parameter (see [`effective`]).
    let made_create = kind == Some(OpKind::Create);
    let retarget = match wt {
        Some(t) => (Some(t) != bt).then_some(Some(t)),
        None => {
            let names_record = |t: &TargetDesc| {
                t.path_param
                    .as_deref()
                    .is_some_and(|p| op.route_names_record(p))
            };
            (made_create && bt.is_some_and(|t| !names_record(t))).then_some(None)
        }
    };
    let path_param = retarget.and_then(|t| t?.path_param.as_ref());
    let input_field = retarget.and_then(|t| t?.member.as_ref());
    let wr = w.answer.as_ref().and_then(|a| a.record.as_ref());
    let br = b.answer.as_ref().and_then(|a| a.record.as_ref());
    let record = changed(wr, br);

    if op.identity.is_none() {
        let names_a_target = matches!(
            kind,
            Some(OpKind::Read | OpKind::Update | OpKind::Replace | OpKind::Delete | OpKind::Action)
        );
        let identifies = effective_kind.is_some()
            && (member.is_some()
                || path_param.is_some()
                || input_field.is_some()
                || record.is_some()
                || names_a_target);
        if identifies {
            op.identity = Some(Identity {
                field: effective_member.unwrap_or_else(|| "id".to_owned()),
                path_param: retarget.unwrap_or(bt).and_then(|t| t.path_param.clone()),
                input_field: retarget.unwrap_or(bt).and_then(|t| t.member.clone()),
                path_field: None,
                record: wr.or(br).and_then(record_of),
            });
        }
        return;
    }
    let Some(identity) = op.identity.as_mut() else {
        return;
    };
    if let Some(field) = member {
        identity.field = field;
    }
    if let Some(t) = retarget {
        identity.path_param = t.and_then(|t| t.path_param.clone());
        identity.input_field = t.and_then(|t| t.member.clone());
    }
    if let Some(r) = record {
        identity.record = record_of(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{RefusalCode, effective, tier0, validate};
    use crate::{Binding, HttpMethod, InputSchemas, OutputVariant, Protocol, StatusSelector};
    use serde_json::{Map, Value, json};

    fn world(v: Value) -> World {
        serde_json::from_value(v).unwrap()
    }

    fn identity(field: &str) -> Identity {
        Identity {
            path_param: None,
            field: field.into(),
            input_field: None,
            path_field: None,
            record: None,
        }
    }

    /// An HTTP operation `id` on `method path` answering `schema` with 200,
    /// its kind following the method (a `POST` to a path ending in an action
    /// segment is an action); each `{param}` a string path parameter.
    fn op(id: &str, method: &str, path: &str, schema: Value) -> Operation {
        let action = path.contains("/actions/") || path.contains(':');
        let (method, kind) = match method {
            "GET" => (HttpMethod::Get, OperationKind::Query),
            "POST" if action => (HttpMethod::Post, OperationKind::Command(Effect::Other)),
            "POST" => (HttpMethod::Post, OperationKind::Command(Effect::Create)),
            "PATCH" => (HttpMethod::Patch, OperationKind::Command(Effect::Update)),
            "PUT" => (HttpMethod::Put, OperationKind::Command(Effect::Update)),
            "DELETE" => (HttpMethod::Delete, OperationKind::Command(Effect::Delete)),
            other => panic!("unknown method {other}"),
        };
        let params: Map<String, Value> = path
            .split('/')
            .filter_map(|s| s.strip_prefix('{')?.split('}').next())
            .map(|p| (p.to_owned(), json!({"type": "string"})))
            .collect();
        Operation {
            id: id.into(),
            protocol: Protocol::Http,
            binding: Binding::Http {
                method,
                path: path.into(),
            },
            kind,
            summary: None,
            input: InputSchemas {
                path: (!params.is_empty()).then(|| json!({"type": "object", "properties": params})),
                query: None,
                headers: None,
                body: None,
                body_required: false,
                bodies: Vec::new(),
            },
            outputs: vec![OutputVariant {
                status: StatusSelector::Code(200),
                content_type: Some("application/json".into()),
                schema: Some(schema),
                headers: Map::new(),
                description: None,
            }],
            resource_type: None,
            identity: None,
            semantic_hints: vec![],
            security: vec![],
        }
    }

    fn typed(mut o: Operation, t: &str, identity: Option<Identity>) -> Operation {
        o.resource_type = Some(t.into());
        o.identity = identity;
        o
    }

    fn by(param: &str, field: &str) -> Identity {
        Identity {
            path_param: Some(param.into()),
            ..identity(field)
        }
    }

    fn bundle(operations: Vec<Operation>, defs: Value) -> Bundle {
        Bundle {
            id: "t".into(),
            title: "t".into(),
            version: "1".into(),
            description: None,
            source_digest: String::new(),
            operations,
            defs: defs.as_object().cloned().unwrap_or_default(),
            protocol_payload: None,
            channels: vec![],
            webhooks: vec![],
            api_key_names: None,
        }
    }

    fn reference(def: &str) -> Value {
        json!({"$ref": format!("#/$defs/{def}")})
    }

    /// Workspaces with a read by `{id}` and a safe-delete action route, which
    /// the bundle holds as an action with no identity.
    fn bundle_with_action() -> Bundle {
        let ws = reference("Workspace");
        bundle(
            vec![
                typed(
                    op("getWorkspace", "GET", "/workspaces/{id}", ws.clone()),
                    "workspace",
                    Some(by("id", "id")),
                ),
                typed(
                    op(
                        "safeDelete",
                        "POST",
                        "/workspaces/{id}/actions/safe-delete",
                        ws,
                    ),
                    "workspace",
                    None,
                ),
            ],
            json!({"Workspace": {"type": "object", "properties": {
                "id": {"type": "string"}, "name": {"type": "string"}
            }}}),
        )
    }

    /// Zones, whose create answers the record in a `data` envelope, things
    /// listed and read, and a list `listZones` answering an array.
    fn zones() -> Bundle {
        let zone = reference("Zone");
        let thing = reference("Thing");
        let mut create = typed(
            op(
                "createZone",
                "POST",
                "/zones",
                json!({"type": "object", "properties": {"data": zone.clone(), "done": {"type": "boolean"}}}),
            ),
            "zone",
            Some(Identity {
                record: Some(RecordAt::Member("data".into())),
                ..identity("id")
            }),
        );
        create.input.body = Some(zone.clone());
        bundle(
            vec![
                typed(
                    op("getZone", "GET", "/zones/{id}", zone.clone()),
                    "zone",
                    Some(by("id", "id")),
                ),
                create,
                typed(
                    op(
                        "listZones",
                        "GET",
                        "/zones",
                        json!({"type": "array", "items": zone}),
                    ),
                    "zone",
                    None,
                ),
                typed(
                    op(
                        "listThings",
                        "GET",
                        "/things",
                        json!({"type": "array", "items": thing.clone()}),
                    ),
                    "thing",
                    None,
                ),
                typed(
                    op("getBundle", "GET", "/bundles/{id}", thing),
                    "thing",
                    Some(by("id", "id")),
                ),
            ],
            json!({
                "Zone": {"type": "object", "properties": {
                    "id": {"type": "string"}, "name": {"type": "string"}
                }},
                "Thing": {"type": "object", "properties": {"id": {"type": "string"}}}
            }),
        )
    }

    /// Widgets, which the bundle names by `label` in their read's path but
    /// whose identity field (`id`) is no member of the record, so tier 0 gives
    /// the type no identity.
    fn widgets() -> Bundle {
        let widget = reference("Widget");
        let mut create = typed(
            op("createWidget", "POST", "/widgets", widget.clone()),
            "widget",
            None,
        );
        create.input.body = Some(widget.clone());
        let mut retire = typed(
            op("retireWidget", "POST", "/widgets:retire", widget.clone()),
            "widget",
            None,
        );
        retire.input.body =
            Some(json!({"type": "object", "properties": {"id": {"type": "string"}}}));
        bundle(
            vec![
                create,
                typed(
                    op("getWidget", "GET", "/widgets/{label}", widget.clone()),
                    "widget",
                    Some(by("label", "id")),
                ),
                typed(
                    op(
                        "listWidgets",
                        "GET",
                        "/widgets",
                        json!({"type": "array", "items": widget}),
                    ),
                    "widget",
                    None,
                ),
                retire,
            ],
            json!({"Widget": {"type": "object", "properties": {
                "label": {"type": "string"}, "size": {"type": "integer"}
            }}}),
        )
    }

    fn applied(b: &Bundle, w: &World, id: &str) -> Operation {
        apply(b, &tier0(b), w).operation(id).cloned().unwrap()
    }

    #[test]
    fn record_in_follows_a_deep_pointer() {
        let id = Identity {
            record: Some(RecordAt::Pointer(vec!["result".into(), "0".into()])),
            ..identity("id")
        };
        assert_eq!(
            id.record_in(&json!({"success": true, "result": [{"id": "a"}]})),
            Some(&json!({"id": "a"}))
        );
        assert_eq!(
            id.record_in(&json!({"result": [{"id": "a"}, {"id": "b"}]})),
            None
        );
        // A `0` over an object is a member of that name.
        let id = Identity {
            record: Some(RecordAt::Pointer(vec!["a".into(), "0".into(), "b".into()])),
            ..identity("id")
        };
        let answer = json!({"a": {"0": {"b": {"id": "x"}}}});
        assert_eq!(id.record_in(&answer), Some(&json!({"id": "x"})));
        let mut answer = answer;
        id.record_in_mut(&mut answer).unwrap()["id"] = json!("y");
        assert_eq!(answer["a"]["0"]["b"]["id"], "y");
    }

    #[test]
    fn answer_holding_builds_the_same_path() {
        let id = Identity {
            record: Some(RecordAt::Pointer(vec![
                "data".into(),
                "result".into(),
                "0".into(),
            ])),
            ..identity("id")
        };
        let rest = json!({"ok": true, "data": {"count": 1, "result": [{"id": "old"}]}});
        let answer = id.answer_holding(json!({"id": "a"}), Some(&rest));
        assert_eq!(
            answer,
            json!({"ok": true, "data": {"count": 1, "result": [{"id": "a"}]}})
        );
        assert_eq!(id.record_in(&answer), Some(&json!({"id": "a"})));
        let bare = id.answer_holding(json!({"id": "a"}), None);
        assert_eq!(bare, json!({"data": {"result": [{"id": "a"}]}}));
        assert_eq!(id.envelope_member(), Some("data"));
    }

    #[test]
    fn record_at_keeps_the_three_shallow_forms() {
        let at = |s: &str| record_at(&Pointer::parse(s).unwrap());
        assert_eq!(at("/m"), RecordAt::Member("m".into()));
        assert_eq!(at("/0"), RecordAt::Item);
        assert_eq!(at("/m/0"), RecordAt::MemberItem("m".into()));
        assert_eq!(at("/a/b"), RecordAt::Pointer(vec!["a".into(), "b".into()]));
        assert_eq!(at("/0/0"), RecordAt::Pointer(vec!["0".into(), "0".into()]));
        for p in ["/m", "/0", "/m/0", "/a/b", "/a~1b/0/c"] {
            let pointer = Pointer::parse(p).unwrap();
            assert_eq!(super::super::record_pointer(&at(p)), Some(pointer), "{p}");
        }
    }

    #[test]
    fn apply_rewrites_an_action_described_as_delete() {
        let b = bundle_with_action();
        let w = world(
            json!({"operations": {"safeDelete": {"kind": "delete", "target": {"path_param": "id"}}}}),
        );
        let op = applied(&b, &w, "safeDelete");
        assert_eq!(op.kind, OperationKind::Command(Effect::Delete));
        assert_eq!(op.identity.unwrap().path_param.as_deref(), Some("id"));
    }

    #[test]
    fn apply_sets_identity_field_from_the_type_only_where_it_differs_from_tier0() {
        let mut b = bundle_with_action();
        b.defs["Workspace"]["properties"]["slug"] = json!({"type": "string"});
        b.operations.push(typed(
            op(
                "getWorkspaceBySlug",
                "GET",
                "/workspaces/by-slug/{slug}",
                reference("Workspace"),
            ),
            "workspace",
            Some(Identity {
                path_field: Some("slug".into()),
                ..by("slug", "id")
            }),
        ));
        let base = tier0(&b);
        assert_eq!(
            base.types["workspace"]
                .identity
                .as_ref()
                .unwrap()
                .member
                .as_deref(),
            Some("id")
        );
        // The same member as tier 0: nothing is written.
        let same = world(json!({"types": {"workspace": {"identity": {"member": "id"}}}}));
        assert_eq!(apply(&b, &base, &same), b);
        let w = world(json!({"types": {"workspace": {"identity": {"member": "name"}}}}));
        let out = apply(&b, &base, &w);
        let get = out
            .operation("getWorkspace")
            .unwrap()
            .identity
            .clone()
            .unwrap();
        assert_eq!(get.field, "name");
        assert_eq!(get.path_param.as_deref(), Some("id"));
        let by_slug = out
            .operation("getWorkspaceBySlug")
            .unwrap()
            .identity
            .clone()
            .unwrap();
        assert_eq!(by_slug.field, "name");
        assert_eq!(by_slug.path_field.as_deref(), Some("slug"));
    }

    #[test]
    fn apply_replace_is_an_update() {
        let b = bundle_with_action();
        let w = world(json!({"operations": {"safeDelete": {"kind": "replace"}}}));
        let op = applied(&b, &w, "safeDelete");
        assert_eq!(op.kind, OperationKind::Command(Effect::Update));
        // A kind naming a target gives the operation an identity.
        assert_eq!(op.identity.map(|i| i.field), Some("id".to_owned()));
    }

    #[test]
    fn an_absent_answer_record_keeps_the_bundles_pointer() {
        let b = zones();
        let base = tier0(&b);
        assert_eq!(
            base.operations["createZone"]
                .answer
                .as_ref()
                .and_then(|a| a.record.as_ref())
                .map(Pointer::as_str),
            Some("/data")
        );
        let w = world(
            json!({"operations": {"createZone": {"answer": {"long_running": {"done": "done", "result": "data"}}}}}),
        );
        let op = apply(&b, &base, &w)
            .operation("createZone")
            .cloned()
            .unwrap();
        assert_eq!(
            op.identity.unwrap().record,
            Some(RecordAt::Member("data".into()))
        );
    }

    #[test]
    fn the_root_pointer_says_the_answer_is_the_record() {
        let b = zones();
        let base = tier0(&b);
        let doc = world(json!({"operations": {"createZone": {"answer": {"record": ""}}}}));
        let (effective, refused) = effective(&b, &base, &doc);
        assert_eq!(refused, vec![]);
        let record = effective.operations["createZone"]
            .answer
            .as_ref()
            .and_then(|a| a.record.as_ref())
            .unwrap();
        assert!(record.is_root());
        let op = apply(&b, &base, &effective)
            .operation("createZone")
            .cloned()
            .unwrap();
        let identity = op.identity.unwrap();
        assert_eq!(identity.record, None);
        assert_eq!(identity.field, "id");
    }

    #[test]
    fn the_root_pointer_is_refused_where_it_is_not_a_record() {
        let mut b = zones();
        let create = b
            .operations
            .iter_mut()
            .find(|o| o.id == "createZone")
            .unwrap();
        create.input.body = Some(json!({"type": "object", "properties": {
            "items": {"type": "array", "items": reference("Zone")}
        }}));
        for (doc, at) in [
            (
                json!({"operations": {"createZone": {"answer": {"records": ""}}}}),
                "/operations/createZone/answer/records",
            ),
            (
                json!({"operations": {"createZone": {"request": {"records": ""}}}}),
                "/operations/createZone/request/records",
            ),
            (
                json!({"operations": {"createZone": {"changes": {"name": {"from_request": ""}}}}}),
                "/operations/createZone/changes/name/from_request",
            ),
        ] {
            let v = validate(&world(doc), &b);
            let refused: Vec<_> = v
                .refused
                .iter()
                .map(|r| (r.path.as_str(), r.code))
                .collect();
            assert_eq!(refused, vec![(at, RefusalCode::Invalid)], "{at}");
        }
        // The request body as the record is accepted.
        let v = validate(
            &world(json!({"operations": {"createZone": {"request": {"record": ""}}}})),
            &b,
        );
        assert_eq!(v.refused, vec![]);
    }

    #[test]
    fn the_root_pointer_needs_an_object_answer() {
        let b = zones();
        let v = validate(
            &world(json!({"operations": {"listZones": {"answer": {"record": ""}}}})),
            &b,
        );
        let refused: Vec<_> = v
            .refused
            .iter()
            .map(|r| (r.path.as_str(), r.code))
            .collect();
        assert_eq!(
            refused,
            vec![(
                "/operations/listZones/answer/record",
                RefusalCode::Unresolved
            )]
        );
    }

    #[test]
    fn a_kind_only_read_becomes_a_read() {
        let b = zones();
        let w = world(json!({"operations": {"listThings": {"kind": "read"}}}));
        let op = applied(&b, &w, "listThings");
        assert_eq!(
            op.identity,
            Some(Identity {
                path_param: None,
                ..identity("id")
            })
        );
        assert_eq!(OpKind::of(&op), Some(OpKind::Read));
    }

    #[test]
    fn apply_kind_list_over_a_read_clears_its_identity() {
        let b = zones();
        assert!(b.operation("getBundle").unwrap().identity.is_some());
        for w in [
            json!({"operations": {"getBundle": {"kind": "list"}}}),
            json!({"operations": {"getBundle": {"kind": "list", "target": {"path_param": "id"}}}}),
        ] {
            let op = applied(&b, &world(w), "getBundle");
            assert_eq!(op.identity, None);
            assert_eq!(OpKind::of(&op), Some(OpKind::List));
        }
    }

    #[test]
    fn a_kind_only_delete_takes_the_effective_target() {
        let mut b = bundle_with_action();
        b.operations
            .iter_mut()
            .find(|o| o.id == "safeDelete")
            .unwrap()
            .identity = Some(by("id", "id"));
        let base = tier0(&b);
        assert_eq!(
            base.operations["safeDelete"]
                .target
                .as_ref()
                .and_then(|t| t.path_param.as_deref()),
            Some("id")
        );
        let w = world(json!({"operations": {"safeDelete": {"kind": "delete"}}}));
        let op = apply(&b, &base, &w)
            .operation("safeDelete")
            .cloned()
            .unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Delete));
        assert_eq!(op.identity.unwrap().path_param.as_deref(), Some("id"));
        // Without an identity in the bundle, the kind alone gives one with
        // the target the description names.
        let b = bundle_with_action();
        let w = world(json!({"operations": {"safeDelete": {"kind": "delete"}}}));
        let op = applied(&b, &w, "safeDelete");
        assert_eq!(op.identity, Some(identity("id")));
    }

    /// Things created under a parent: the bundle took the parent's key for the
    /// create's target, as a heuristic does for a POST whose route ends in a
    /// parameter.
    fn things_under_parents() -> Bundle {
        let thing = reference("Thing");
        let mut create = typed(
            op(
                "createThing",
                "POST",
                "/parents/{parentKey}/things",
                thing.clone(),
            ),
            "things",
            Some(by("parentKey", "key")),
        );
        create.input.body = Some(thing.clone());
        bundle(
            vec![
                create,
                typed(
                    op(
                        "getThing",
                        "GET",
                        "/parents/{parentKey}/things/{key}",
                        thing,
                    ),
                    "things",
                    Some(by("key", "key")),
                ),
            ],
            json!({"Thing": {"type": "object", "properties": {
                "key": {"type": "string"}, "name": {"type": "string"}
            }}}),
        )
    }

    /// Buckets stored by `PUT /buckets/{name}` (tier 0: an update by `name`),
    /// and a `POST /parents/{parentKey}:things` action keyed by its parent.
    fn put_and_post_routes() -> Bundle {
        let thing = reference("Thing");
        bundle(
            vec![
                typed(
                    op("putBucket", "PUT", "/buckets/{name}", thing.clone()),
                    "buckets",
                    Some(by("name", "name")),
                ),
                typed(
                    op("addThing", "POST", "/parents/{parentKey}:things", thing),
                    "things",
                    Some(by("parentKey", "key")),
                ),
            ],
            json!({"Thing": {"type": "object", "properties": {
                "name": {"type": "string"}, "key": {"type": "string"}
            }}}),
        )
    }

    #[test]
    fn a_put_made_a_create_keeps_the_path_param_naming_the_record() {
        // RFC 9110: a PUT's target URI is the resource itself.
        let b = put_and_post_routes();
        let base = tier0(&b);
        let doc = world(json!({"operations": {"putBucket": {"kind": "create"}}}));
        let (effective, refused) = effective(&b, &base, &doc);
        assert_eq!(refused, vec![]);
        assert_eq!(
            effective.operations["putBucket"].target,
            base.operations["putBucket"].target
        );
        let op = apply(&b, &base, &effective)
            .operation("putBucket")
            .cloned()
            .unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Create));
        assert_eq!(op.identity, Some(by("name", "name")));
    }

    #[test]
    fn a_post_made_a_create_drops_the_parents_path_param() {
        // A POST's path parameter names where the record goes, not the record.
        let b = put_and_post_routes();
        let base = tier0(&b);
        assert!(base.operations["addThing"].target.is_some());
        let doc = world(json!({"operations": {"addThing": {"kind": "create"}}}));
        let (effective, refused) = effective(&b, &base, &doc);
        assert_eq!(refused, vec![]);
        assert_eq!(effective.operations["addThing"].target, None);
        let op = apply(&b, &base, &effective)
            .operation("addThing")
            .cloned()
            .unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Create));
        assert_eq!(op.identity, Some(identity("key")));
    }

    #[test]
    fn an_authored_target_replaces_the_path_param_tier0_took() {
        let b = things_under_parents();
        let base = tier0(&b);
        assert_eq!(
            base.operations["createThing"].target,
            Some(TargetDesc {
                path_param: Some("parentKey".into()),
                ..TargetDesc::default()
            })
        );
        let doc = world(json!({"operations": {"createThing": {"target": {"member": "key"}}}}));
        let (effective, refused) = effective(&b, &base, &doc);
        assert_eq!(refused, vec![]);
        assert_eq!(
            effective.operations["createThing"].target,
            Some(TargetDesc {
                member: Some("key".into()),
                ..TargetDesc::default()
            })
        );
        let op = apply(&b, &base, &effective)
            .operation("createThing")
            .cloned()
            .unwrap();
        assert_eq!(
            op.identity,
            Some(Identity {
                input_field: Some("key".into()),
                ..identity("key")
            })
        );
    }

    #[test]
    fn a_described_type_identity_gives_identities_to_ops_that_had_none() {
        let b = widgets();
        let base = tier0(&b);
        assert!(!base.types.contains_key("widget"));
        let w = world(json!({"types": {"widget": {"identity": {"member": "label"}}}}));
        let out = apply(&b, &base, &w);
        let identity_of = |id: &str| out.operation(id).unwrap().identity.clone();
        assert_eq!(identity_of("createWidget"), Some(identity("label")));
        assert_eq!(identity_of("getWidget"), Some(by("label", "label")));
        assert_eq!(identity_of("listWidgets"), None);
    }

    #[test]
    fn a_described_target_gives_an_identity_to_an_action_that_had_none() {
        let b = widgets();
        let w = world(json!({"operations": {"retireWidget": {"target": {"member": "id"}}}}));
        assert_eq!(validate(&w, &b).refused, vec![]);
        let op = applied(&b, &w, "retireWidget");
        assert_eq!(
            op.identity,
            Some(Identity {
                input_field: Some("id".into()),
                ..identity("id")
            })
        );
        // With the type's member described too, the field is the member.
        let w = world(json!({
            "types": {"widget": {"identity": {"member": "label"}}},
            "operations": {"retireWidget": {"target": {"member": "id"}}}
        }));
        let op = applied(&b, &w, "retireWidget");
        assert_eq!(op.identity.map(|i| i.field), Some("label".to_owned()));
    }

    #[test]
    fn apply_moves_an_operation_to_another_type() {
        let b = zones();
        let w = world(json!({"operations": {"getBundle": {"type": "zone"}}}));
        let op = applied(&b, &w, "getBundle");
        assert_eq!(op.resource_type.as_deref(), Some("zone"));
        assert_eq!(op.identity, Some(by("id", "id")));
    }

    #[test]
    fn apply_writes_a_deep_record_pointer() {
        let mut b = zones();
        let get = b.operations.iter_mut().find(|o| o.id == "getZone").unwrap();
        get.outputs[0].schema = Some(json!({"type": "object", "properties": {
            "data": {"type": "object", "properties": {"zone": reference("Zone")}}
        }}));
        let w = world(json!({"operations": {"getZone": {"answer": {"record": "/data/zone"}}}}));
        assert_eq!(validate(&w, &b).refused, vec![]);
        let op = applied(&b, &w, "getZone");
        assert_eq!(
            op.identity.unwrap().record,
            Some(RecordAt::Pointer(vec!["data".into(), "zone".into()]))
        );
    }

    #[test]
    fn apply_leaves_streams_alone() {
        let mut b = zones();
        let mut s = op("watchZones", "GET", "/zones/watch", reference("Zone"));
        s.kind = OperationKind::StreamServer;
        s.resource_type = Some("zone".into());
        b.operations.push(s);
        let w = world(json!({
            "types": {"zone": {"identity": {"member": "name"}}},
            "operations": {"watchZones": {"kind": "read", "target": {"path_param": "x"}}}
        }));
        let op = apply(&b, &tier0(&b), &w)
            .operation("watchZones")
            .cloned()
            .unwrap();
        assert_eq!(op.kind, OperationKind::StreamServer);
        assert_eq!(op.identity, None);
    }

    #[test]
    fn apply_of_tier0_and_of_an_empty_world_change_nothing() {
        for b in [zones(), widgets(), bundle_with_action()] {
            let base = tier0(&b);
            assert_eq!(apply(&b, &base, &base), b);
            assert_eq!(apply(&b, &base, &World::default()), b);
        }
    }
}
