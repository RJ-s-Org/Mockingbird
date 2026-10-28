//! What a GraphQL answer leaves in the session: every object it holds that
//! has an `id`, stored under its type (`Cart` → `cart`), so a later root
//! read by id (`cart(id:)`) finds it and a mutation naming it (`cartId`,
//! a `gid://shopify/ProductVariant/…`) is generated knowing it.
//!
//! A mutation's objects are merged into what the session holds (the cart
//! `cartLinesAdd` answers is the cart now); a query's are only added when
//! the session holds none under their id, like a list read's seeded items.

use mb_mcm::{Bundle, Effect, Operation, OperationKind};
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
#[allow(clippy::unwrap_used)]
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
}
