//! What a GraphQL answer leaves in the session: every object it holds that
//! has an `id`, stored under its type (`Cart` → `cart`), so a later root
//! read by id (`cart(id:)`) finds it and a mutation naming it (`cartId`,
//! a `gid://shopify/ProductVariant/…`) is generated knowing it.
//!
//! A mutation's objects are merged into what the session holds (the cart
//! `cartLinesAdd` answers is the cart now); a query's are only added when
//! the session holds none under their id, like a list read's seeded items.

use mb_mcm::{Binding, Bundle, Effect, Operation, OperationKind};
use mb_state::StoredResource;
use serde_json::{Map, Value};

use super::select::Selection;

/// Objects stored per answer, at most.
const MAX_OBJECTS: usize = 50;

/// Bounds the walk (recursive types).
const MAX_DEPTH: usize = 12;

/// How an answer's objects are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Store {
    /// Merged over the stored resource (a mutation).
    Merge,
    /// Added unless one is stored (a query).
    Seed,
}

/// How `op`'s answers are stored: `None` for a delete, whose answer is
/// what no longer exists, and for subscriptions.
pub(crate) fn mode(op: &Operation) -> Option<Store> {
    match op.kind {
        OperationKind::Query => Some(Store::Seed),
        OperationKind::Command(Effect::Delete) => None,
        OperationKind::Command(_) => Some(Store::Merge),
        _ => None,
    }
}

/// The objects with an `id` in `body`, `op`'s answer for `sel` keyed by
/// response key, each keyed by field name and typed by its GraphQL type
/// (lowercased). For a mutation, the objects at the top of its answer
/// (the payload's `cart`, or a returned object itself) also take the
/// arguments' scalar values their type declares and the answer left out
/// (`createIssue(input: {title})` → the issue's `title`).
pub(crate) fn objects(
    bundle: &Bundle,
    op: &Operation,
    sel: &Selection,
    body: &Value,
    args: &Map<String, Value>,
) -> Vec<StoredResource> {
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return Vec::new();
    };
    let mut walk = Walk {
        bundle,
        found: Vec::new(),
    };
    walk.value(schema, sel, body, 0, None);
    if mode(op) == Some(Store::Merge) {
        let inputs = scalar_args(args);
        for (resource, depth) in &mut walk.found {
            if *depth <= 1 {
                fill_from_args(bundle, resource, &inputs);
            }
        }
    }
    walk.found.into_iter().map(|(r, _)| r).collect()
}

/// What an update or a delete mutation names by its `id` argument: the
/// type the record is stored under and its id.
#[derive(Debug, PartialEq)]
pub(crate) struct ById {
    /// Lowercased.
    pub resource_type: String,
    pub id: String,
}

/// The record `op`, an update or a delete, acts on by its `id` argument
/// (`issueUpdate(id:, input:)`, `commentDelete(id:)`), whatever its
/// payload selects: an `id` of the operation's resource type, when that
/// type has an `id` key and the name says it writes that record
/// ([`mb_mcm::write_nouns`]: `issueUpdate`, `deleteComment`; not
/// `updateTeamMember` returning a `Team`). An update whose resource type
/// has none (a
/// payload's, `setIssuePriority`'s `IssuePayload`) acts on the one type
/// with an `id` among its payload's object members (`IssuePayload.issue`);
/// a delete never does, as a payload's other object is the parent that
/// stays (`productVariantDelete`'s `product`). `None` for any other
/// mutation, or with no `id`.
pub(crate) fn by_id(bundle: &Bundle, op: &Operation, args: &Map<String, Value>) -> Option<ById> {
    let delete = match op.kind {
        OperationKind::Command(Effect::Delete) => true,
        OperationKind::Command(Effect::Update) => false,
        _ => return None,
    };
    let id = match args.get("id")? {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let keyed = |name: &str| {
        bundle
            .defs
            .iter()
            .find(|(n, _)| n.to_lowercase() == name)
            .and_then(|(_, def)| def.get("properties")?.as_object())
            .is_some_and(|p| p.contains_key("id"))
    };
    if let Some(own) = op.resource_type.as_deref().filter(|t| keyed(t)) {
        // Only the record the name names: `updateTeamMember` returning its
        // `Team` updates no team.
        let named = matches!(&op.binding, Binding::Graphql { field, .. }
        if mb_mcm::write_nouns(field).iter().any(|(effect, noun)| {
            OperationKind::Command(*effect) == op.kind && *noun == own.replace('_', "")
        }));
        return named.then(|| ById {
            resource_type: own.to_string(),
            id,
        });
    }
    if delete {
        return None;
    }
    let payload = op
        .success_output()
        .and_then(|o| o.schema.as_ref())
        .and_then(ref_name)
        .and_then(|name| bundle.defs.get(name)?.get("properties")?.as_object())?;
    let mut members: Vec<String> = Vec::new();
    for member in payload.values() {
        if let Some(name) = ref_name(member).map(str::to_lowercase)
            && keyed(&name)
            && !members.contains(&name)
        {
            members.push(name);
        }
    }
    match members.as_slice() {
        [one] => Some(ById {
            resource_type: one.clone(),
            id,
        }),
        _ => None,
    }
}

/// The input objects' members of an update (`input: {priority, title}`)
/// that `held`, a stored record, has or its type declares: what the
/// update sets on it. Its `id` is left alone.
pub(crate) fn update_of(
    bundle: &Bundle,
    held: &StoredResource,
    args: &Map<String, Value>,
) -> Map<String, Value> {
    let declared = bundle
        .defs
        .iter()
        .find(|(name, _)| name.to_lowercase() == held.resource_type)
        .and_then(|(_, def)| def.get("properties"))
        .and_then(Value::as_object);
    let has = |name: &str| {
        held.data.get(name).is_some() || declared.is_some_and(|d| d.contains_key(name))
    };
    let mut out = Map::new();
    for (name, value) in args.values().filter_map(Value::as_object).flatten() {
        if name != "id" && has(name) {
            out.entry(name.clone()).or_insert_with(|| value.clone());
        }
    }
    out
}

/// The type a schema refers to, through a nullable `anyOf`.
fn ref_name(schema: &Value) -> Option<&str> {
    def_name(schema).or_else(|| schema.get("anyOf")?.as_array()?.iter().find_map(def_name))
}

struct Walk<'a> {
    bundle: &'a Bundle,
    /// With the depth each was found at.
    found: Vec<(StoredResource, usize)>,
}

impl Walk<'_> {
    /// Walks `value` (answered for `schema` and `sel`); `parent` is the
    /// nearest enclosing stored object: its type and scalar fields.
    fn value(
        &mut self,
        schema: &Value,
        sel: &Selection,
        value: &Value,
        depth: usize,
        parent: Option<&(String, Map<String, Value>)>,
    ) {
        if depth > MAX_DEPTH || self.found.len() >= MAX_OBJECTS || sel.fields.is_empty() {
            return;
        }
        match value {
            Value::Array(items) => {
                let Some(item) = list_item(schema) else {
                    return;
                };
                for v in items {
                    self.value(item, sel, v, depth, parent);
                }
            }
            Value::Object(obj) => {
                let Some((name, def)) = object_type(self.bundle, schema, obj) else {
                    return;
                };
                let props = def.get("properties").and_then(Value::as_object);
                let mut own = None;
                if let Some(Value::String(id)) = obj.get("id")
                    && let Some(props) = props.filter(|p| p.contains_key("id"))
                {
                    let mut data = by_field_name(value, sel);
                    if let (Some(parent), Value::Object(fields)) = (parent, &mut data) {
                        link_parent(props, parent, fields);
                    }
                    own = Some((name.to_string(), scalars(&data)));
                    self.found.push((
                        StoredResource {
                            resource_type: name.to_lowercase(),
                            id: id.clone(),
                            data,
                        },
                        depth,
                    ));
                }
                let Some(props) = props else {
                    return;
                };
                let parent = own.as_ref().or(parent);
                for (key, child) in &sel.fields {
                    let (Some(member), Some(field)) = (obj.get(key), props.get(&child.name)) else {
                        continue;
                    };
                    self.value(field, child, member, depth + 1, parent);
                }
            }
            _ => {}
        }
    }
}

/// Sets the field of `fields` (an object whose type declares `props`) that
/// refers to the enclosing object's type, when the answer left it out, to
/// that object's scalars: a variant read inside its product knows the
/// product (`ProductVariant.product`).
fn link_parent(
    props: &Map<String, Value>,
    (parent_type, parent): &(String, Map<String, Value>),
    fields: &mut Map<String, Value>,
) {
    let refers = |schema: &Value| {
        let mut schema = schema;
        for _ in 0..4 {
            if def_name(schema) == Some(parent_type.as_str()) {
                return true;
            }
            match schema
                .get("anyOf")
                .and_then(Value::as_array)
                .and_then(|b| b.iter().find(|b| def_name(b).is_some()))
            {
                Some(branch) => schema = branch,
                None => return false,
            }
        }
        false
    };
    if let Some((field, _)) = props
        .iter()
        .find(|(field, schema)| !fields.contains_key(*field) && refers(schema))
    {
        fields.insert(field.clone(), Value::Object(parent.clone()));
    }
}

/// The scalar members of `data` (an object), `__typename` included.
fn scalars(data: &Value) -> Map<String, Value> {
    data.as_object()
        .map(|obj| {
            obj.iter()
                .filter(|(_, v)| !v.is_object() && !v.is_array())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// The object type `schema` refers to (through a nullable `anyOf`, and
/// for an interface or union the member `obj`'s `__typename` names),
/// with its definition.
fn object_type<'a>(
    bundle: &'a Bundle,
    schema: &Value,
    obj: &Map<String, Value>,
) -> Option<(&'a str, &'a Value)> {
    let mut schema = schema;
    for _ in 0..4 {
        if let Some(name) = def_name(schema) {
            let def = bundle.defs.get(name)?;
            // An interface or union: a `oneOf` of its members.
            if def.get("oneOf").is_some() {
                let typename = obj.get("__typename").and_then(Value::as_str)?;
                let (name, def) = bundle.defs.get_key_value(typename)?;
                return Some((name.as_str(), def));
            }
            let (name, def) = bundle.defs.get_key_value(name)?;
            return Some((name.as_str(), def));
        }
        schema = schema
            .get("anyOf")
            .and_then(Value::as_array)?
            .iter()
            .find(|b| def_name(b).is_some())?;
    }
    None
}

/// The item schema of a list schema (nullable or not).
fn list_item(schema: &Value) -> Option<&Value> {
    if let Some(items) = schema.get("items") {
        return Some(items);
    }
    schema
        .get("anyOf")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|b| b.get("items"))
}

fn def_name(schema: &Value) -> Option<&str> {
    schema.get("$ref")?.as_str()?.strip_prefix("#/$defs/")
}

/// An answer keyed by response key re-keyed by field name, recursively; a
/// member under no selected key (a model's extra field) is kept.
pub(crate) fn by_field_name(value: &Value, sel: &Selection) -> Value {
    if sel.fields.is_empty() {
        return value.clone();
    }
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|v| by_field_name(v, sel)).collect()),
        Value::Object(obj) => {
            let mut out = Map::new();
            for (key, child) in &sel.fields {
                if let Some(member) = obj.get(key)
                    && !out.contains_key(&child.name)
                {
                    out.insert(child.name.clone(), by_field_name(member, child));
                }
            }
            for (key, member) in obj {
                if !sel.fields.contains_key(key) && !out.contains_key(key) {
                    out.insert(key.clone(), member.clone());
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// The scalar arguments of a mutation, and those of its input objects (one
/// level: `input: {title, body}`). Ids and references are left out.
fn scalar_args(args: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    let nested = args.values().filter_map(Value::as_object).flatten();
    for (name, value) in args.iter().chain(nested) {
        let scalar = matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_));
        let reference = name == "id" || name.ends_with("Id") || name.ends_with("_id");
        if scalar && !reference {
            out.entry(name.clone()).or_insert_with(|| value.clone());
        }
    }
    out
}

/// `resource` with each of `inputs` its type declares and it lacks.
fn fill_from_args(bundle: &Bundle, resource: &mut StoredResource, inputs: &Map<String, Value>) {
    let declared = bundle
        .defs
        .iter()
        .find(|(name, _)| name.to_lowercase() == resource.resource_type)
        .and_then(|(_, def)| def.get("properties"))
        .and_then(Value::as_object);
    let (Some(declared), Value::Object(data)) = (declared, &mut resource.data) else {
        return;
    };
    for (name, value) in inputs {
        if declared.contains_key(name) && !data.contains_key(name) {
            data.insert(name.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn sel(fields: &[(&str, &str, Selection)]) -> Selection {
        Selection {
            fields: fields
                .iter()
                .map(|(k, n, c)| {
                    (
                        k.to_string(),
                        Selection {
                            name: n.to_string(),
                            ..c.clone()
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
            ..Selection::default()
        }
    }

    #[test]
    fn by_field_name_undoes_aliases() {
        let s = sel(&[
            ("ref", "id", Selection::default()),
            ("heading", "title", Selection::default()),
        ]);
        assert_eq!(
            by_field_name(&json!({"ref": "i-1", "heading": "Crash", "extra": 1}), &s),
            json!({"id": "i-1", "title": "Crash", "extra": 1})
        );
    }

    #[test]
    fn scalar_args_skip_references() {
        let args = json!({"input": {"repositoryId": "r", "title": "T", "labels": ["a"]}, "id": "x", "draft": true});
        assert_eq!(
            Value::Object(scalar_args(args.as_object().unwrap())),
            json!({"draft": true, "title": "T"})
        );
    }

    const SDL: &str = r#"
type Issue { id: ID! title: String! priority: Int! }
type Product { id: ID! title: String! }
type ProductVariant { id: ID! title: String! }
type Note { body: String! }
type IssuePayload { success: Boolean! issue: Issue }
type VariantDeletePayload { deletedId: ID product: Product }
type PairPayload { issue: Issue product: Product }
type NotePayload { note: Note }
input IssueInput { title: String priority: Int stateId: String }
type Query { issue(id: ID!): Issue }
type Mutation {
  issueUpdate(id: ID!, input: IssueInput!): IssuePayload!
  setIssuePriority(id: ID!, priority: Int!): IssuePayload!
  setPair(id: ID!, input: IssueInput!): PairPayload!
  productVariantDelete(id: ID!): VariantDeletePayload!
  cancelOrder(id: ID!): IssuePayload!
  setNote(id: ID!, input: IssueInput!): NotePayload!
  issueBulkUpdate(input: IssueInput!): IssuePayload!
}
"#;

    fn by_id_of(name: &str, args: Value) -> Option<ById> {
        let bundle = mb_import::compile(SDL.as_bytes()).unwrap();
        let op = bundle
            .operations
            .iter()
            .find(|o| o.id == format!("Mutation.{name}"))
            .unwrap();
        by_id(&bundle, op, args.as_object().unwrap())
    }

    fn target(t: &str, id: &str) -> Option<ById> {
        Some(ById {
            resource_type: t.into(),
            id: id.into(),
        })
    }

    #[test]
    fn by_id_takes_the_operation_s_own_keyed_type() {
        assert_eq!(
            by_id_of("issueUpdate", json!({"id": "i-1"})),
            target("issue", "i-1")
        );
        // A numeric id is its decimal text.
        assert_eq!(
            by_id_of("issueUpdate", json!({"id": 5})),
            target("issue", "5")
        );
        assert_eq!(by_id_of("issueUpdate", json!({"id": ""})), None);
        assert_eq!(by_id_of("issueBulkUpdate", json!({"input": {}})), None);
    }

    #[test]
    fn a_delete_acts_only_on_its_own_type() {
        // The payload's `product` is the parent that stays.
        assert_eq!(
            by_id_of("productVariantDelete", json!({"id": "1"})),
            target("productvariant", "1")
        );
        // `cancelOrder` names no record: its payload's issue is not it.
        assert_eq!(by_id_of("cancelOrder", json!({"id": "i-1"})), None);
    }

    #[test]
    fn an_update_falls_back_to_its_payload_s_one_keyed_member() {
        // `IssuePayload` has no `id`: its one keyed member, the issue.
        assert_eq!(
            by_id_of("setIssuePriority", json!({"id": "i-1"})),
            target("issue", "i-1")
        );
        // Two keyed members: which one is unknown.
        assert_eq!(by_id_of("setPair", json!({"id": "i-1"})), None);
        // A member type with no `id` key is none (`keyed`).
        assert_eq!(by_id_of("setNote", json!({"id": "n-1"})), None);
    }

    #[test]
    fn update_of_keeps_the_record_s_members_only() {
        let bundle = mb_import::compile(SDL.as_bytes()).unwrap();
        let held = StoredResource {
            resource_type: "issue".into(),
            id: "i-1".into(),
            data: json!({"id": "i-1", "title": "Crash", "priority": 3, "extra": 0}),
        };
        let args =
            json!({"id": "i-2", "input": {"id": "x", "priority": 1, "stateId": "s", "extra": 9}});
        assert_eq!(
            Value::Object(update_of(&bundle, &held, args.as_object().unwrap())),
            json!({"priority": 1, "extra": 9})
        );
    }
}
