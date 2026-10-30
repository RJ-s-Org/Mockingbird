//! Helpers that keep answers consistent across calls: finding the items of
//! a list response, the field that identifies them, and the schema of one
//! of their properties.

use mb_mcm::{Bundle, Operation, OperationKind};
use serde_json::Value;

/// Object members that commonly wrap a list (`{"data": [...]}`).
pub const ENVELOPE_KEYS: [&str; 6] = ["data", "items", "results", "records", "entries", "nodes"];

/// The member of an object body that holds its list: the first
/// [`ENVELOPE_KEYS`] member that is an array, else the only top-level
/// member that is an array of objects.
fn list_key(obj: &serde_json::Map<String, Value>) -> Option<&str> {
    if let Some(key) = ENVELOPE_KEYS
        .iter()
        .find(|k| obj.get(**k).is_some_and(Value::is_array))
    {
        return Some(key);
    }
    let mut arrays = obj.iter().filter(|(_, v)| {
        v.as_array()
            .is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_object))
    });
    let (key, _) = arrays.next()?;
    arrays.next().is_none().then_some(key.as_str())
}

/// The items of a list body: the body when it is an array, else the array
/// an envelope holds (see [`ENVELOPE_KEYS`]); `None` otherwise.
pub fn list_items(body: &Value) -> Option<&Vec<Value>> {
    match body {
        Value::Array(items) => Some(items),
        Value::Object(obj) => obj.get(list_key(obj)?)?.as_array(),
        _ => None,
    }
}

/// [`list_items`], mutably.
pub fn list_items_mut(body: &mut Value) -> Option<&mut Vec<Value>> {
    match body {
        Value::Array(items) => Some(items),
        Value::Object(obj) => {
            let key = list_key(obj)?.to_string();
            obj.get_mut(&key)?.as_array_mut()
        }
        _ => None,
    }
}

/// The field identifying `op`'s list items: the identity field of a read
/// by path id of the same resource type, else `"id"`.
pub fn list_identity(bundle: &Bundle, op: &Operation) -> String {
    bundle
        .operations
        .iter()
        .filter(|o| matches!(o.kind, OperationKind::Query))
        .filter(|o| o.resource_type.is_some() && o.resource_type == op.resource_type)
        .find_map(|o| {
            o.identity
                .as_ref()
                .filter(|i| i.path_param.is_some())
                .map(|i| i.field.clone())
        })
        .unwrap_or_else(|| "id".to_string())
}

/// The field identifying the items of `op`'s list answer `body`:
/// [`list_identity`] when an item carries it, else the list member's name
/// made singular with `_id` or `Id` (`accounts` → `account_id`) when an
/// item carries that.
pub fn item_identity(bundle: &Bundle, op: &Operation, body: &Value) -> String {
    let field = list_identity(bundle, op);
    let first = list_items(body).and_then(|items| items.iter().find(|i| i.is_object()));
    let Some(first) = first else {
        return field;
    };
    if first.get(&field).is_some() {
        return field;
    }
    let singular = match body {
        Value::Object(obj) => list_key(obj).map(|k| k.strip_suffix('s').unwrap_or(k)),
        _ => None,
    };
    singular
        .into_iter()
        .flat_map(|s| [format!("{s}_id"), format!("{s}Id")])
        .find(|candidate| first.get(candidate).is_some())
        .unwrap_or(field)
}

/// The members of object `schema` (`$ref`s followed, and the branches of
/// an `anyOf`/`oneOf`/`allOf`, Slack's `anyOf [success, error]`) that hold
/// lists of objects: `accounts` in Plaid's `{accounts: [...], item: {...}}`.
pub fn list_members(bundle: &Bundle, schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_list_members(bundle, schema, 0, &mut out);
    out
}

fn collect_list_members(bundle: &Bundle, schema: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > 4 {
        return;
    }
    let schema = deref(bundle, schema);
    for key in ["anyOf", "oneOf", "allOf"] {
        for branch in schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            collect_list_members(bundle, branch, depth + 1, out);
        }
    }
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let found = props
        .iter()
        .filter(|(_, p)| {
            deref(bundle, p)
                .get("items")
                .map(|item| deref(bundle, item))
                .is_some_and(|item| {
                    item.get("properties").is_some()
                        || item.get("type").and_then(Value::as_str) == Some("object")
                        || ["allOf", "anyOf", "oneOf"]
                            .iter()
                            .any(|k| item.get(*k).is_some())
                })
        })
        .map(|(name, _)| name.clone());
    for name in found {
        if !out.contains(&name) {
            out.push(name);
        }
    }
}

/// `schema` with `$ref`s to `#/$defs/...` followed (bounded).
pub(crate) fn deref<'a>(bundle: &'a Bundle, mut schema: &'a Value) -> &'a Value {
    for _ in 0..16 {
        let Some(name) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
        else {
            break;
        };
        match bundle.defs.get(name) {
            Some(target) => schema = target,
            None => break,
        }
    }
    schema
}

/// The schema of `field` in object `schema`, looking through `$ref`s and
/// `allOf`/`oneOf`/`anyOf` members.
pub fn property_schema(bundle: &Bundle, schema: &Value, field: &str) -> Option<Value> {
    fn find(bundle: &Bundle, schema: &Value, field: &str, depth: usize) -> Option<Value> {
        if depth > 8 {
            return None;
        }
        let schema = deref(bundle, schema);
        if let Some(p) = schema.get("properties").and_then(|p| p.get(field)) {
            return Some(p.clone());
        }
        ["allOf", "oneOf", "anyOf"]
            .iter()
            .filter_map(|k| schema.get(*k).and_then(Value::as_array))
            .flatten()
            .find_map(|member| find(bundle, member, field, depth + 1))
    }
    find(bundle, schema, field, 0)
}

/// The schema of one item of list `body` answered for `schema`.
pub(crate) fn item_schema(bundle: &Bundle, schema: &Value, body: &Value) -> Option<Value> {
    let list = match body {
        Value::Array(_) => deref(bundle, schema).clone(),
        Value::Object(obj) => property_schema(bundle, schema, list_key(obj)?)?,
        _ => return None,
    };
    let list = deref(bundle, &list);
    list.get("items").cloned()
}
