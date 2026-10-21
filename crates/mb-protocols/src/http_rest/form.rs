//! Form fields and query parameters as JSON: bracket notation
//! (`a[b][c]=1`, `a[0]=x`, `a[]=y`, as Stripe, Rails and `qs` encode them)
//! nests into objects and arrays, and every value is coerced to the type the
//! schema declares for it.

use serde_json::{Map, Number, Value};

/// How deep `$ref`s and combinators are followed to find a value's schema.
const MAX_SCHEMA_DEPTH: usize = 16;

/// Decoded keys: the values of a key, or its bracketed children in order.
enum Node {
    Leaf(Vec<String>),
    Map(Vec<(String, Node)>),
}

/// `pairs` as an object, nested by bracket notation and coerced through
/// `schema` (an object schema; `$ref`s resolve against `defs`). A key the
/// schema declares literally (`filter[status]`) is kept flat. Repeated keys
/// become an array only where the schema wants one; otherwise the last
/// value wins.
pub(super) fn decode(
    pairs: Vec<(String, String)>,
    schema: Option<&Value>,
    defs: &Map<String, Value>,
) -> Map<String, Value> {
    let root = branches(schema, defs);
    let declared = |key: &str| {
        root.iter()
            .any(|b| b.get("properties").and_then(|p| p.get(key)).is_some())
    };
    let mut entries = Vec::new();
    for (key, value) in pairs {
        match split_key(&key).filter(|_| !declared(&key)) {
            Some(path) => insert(&mut entries, &path, value),
            None => insert(&mut entries, &[key], value),
        }
    }
    object(entries, &root, defs)
}

/// `base[a][b]` as `[base, a, b]`; `None` for a key without brackets or
/// with malformed ones.
fn split_key(key: &str) -> Option<Vec<String>> {
    let open = key.find('[')?;
    if open == 0 {
        return None;
    }
    let mut path = vec![key[..open].to_string()];
    let mut rest = &key[open..];
    while !rest.is_empty() {
        let inner = rest.strip_prefix('[')?;
        let close = inner.find(']')?;
        let segment = &inner[..close];
        if segment.contains('[') {
            return None;
        }
        path.push(segment.to_string());
        rest = &inner[close + 1..];
    }
    Some(path)
}

/// Inserts `value` at `path` below `entries`; an empty segment (`a[]`)
/// appends a new element.
fn insert(entries: &mut Vec<(String, Node)>, path: &[String], value: String) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    let key = if first.is_empty() {
        entries.len().to_string()
    } else {
        first.clone()
    };
    let slot = match entries.iter().position(|(k, _)| *k == key) {
        Some(i) => &mut entries[i].1,
        None => {
            let node = if rest.is_empty() {
                Node::Leaf(Vec::new())
            } else {
                Node::Map(Vec::new())
            };
            entries.push((key, node));
            let last = entries.len() - 1;
            &mut entries[last].1
        }
    };
    match (slot, rest.is_empty()) {
        (Node::Leaf(values), true) => values.push(value),
        // A key given both a value and children keeps its children.
        (Node::Map(_), true) => {}
        (slot @ Node::Leaf(_), false) => {
            let mut children = Vec::new();
            insert(&mut children, rest, value);
            *slot = Node::Map(children);
        }
        (Node::Map(children), false) => insert(children, rest, value),
    }
}

/// The concrete schemas `schema` stands for: itself and, recursively, its
/// `$ref` target and `anyOf`/`oneOf`/`allOf` members.
fn branches<'a>(schema: Option<&'a Value>, defs: &'a Map<String, Value>) -> Vec<&'a Value> {
    fn walk<'a>(
        schema: &'a Value,
        defs: &'a Map<String, Value>,
        depth: usize,
        out: &mut Vec<&'a Value>,
    ) {
        if depth > MAX_SCHEMA_DEPTH {
            return;
        }
        if let Some(target) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| defs.get(&name.replace("~1", "/").replace("~0", "~")))
        {
            walk(target, defs, depth + 1, out);
        }
        for key in ["anyOf", "oneOf", "allOf"] {
            for member in schema
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                walk(member, defs, depth + 1, out);
            }
        }
        out.push(schema);
    }
    let mut out = Vec::new();
    if let Some(schema) = schema {
        walk(schema, defs, 0, &mut out);
    }
    out
}

fn types<'a>(branches: &[&'a Value]) -> Vec<&'a str> {
    branches
        .iter()
        .flat_map(|b| match b.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn has_type(b: &Value, ty: &str) -> bool {
    types(&[b]).contains(&ty)
}

/// The schema of `key` in an object described by `branches`: a declared
/// property, else the schema of additional properties.
fn property<'a>(branches: &[&'a Value], key: &str) -> Option<&'a Value> {
    branches
        .iter()
        .find_map(|b| b.get("properties").and_then(|p| p.get(key)))
        .or_else(|| {
            branches
                .iter()
                .find_map(|b| b.get("additionalProperties").filter(|a| a.is_object()))
        })
}

/// The item schema of an array described by `branches`.
fn items<'a>(branches: &[&'a Value]) -> Option<&'a Value> {
    branches
        .iter()
        .filter(|b| has_type(b, "array"))
        .find_map(|b| b.get("items"))
}

fn object(
    entries: Vec<(String, Node)>,
    branches: &[&Value],
    defs: &Map<String, Value>,
) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, node)| {
            let value = to_json(node, property(branches, &key), defs);
            (key, value)
        })
        .collect()
}

fn to_json(node: Node, schema: Option<&Value>, defs: &Map<String, Value>) -> Value {
    let branches = branches(schema, defs);
    let wants_array = branches.iter().any(|b| has_type(b, "array"));
    match node {
        Node::Leaf(values) if wants_array => {
            let items = items(&branches);
            Value::Array(values.iter().map(|v| leaf(v, items, defs)).collect())
        }
        Node::Leaf(values) => values
            .last()
            .map(|v| coerce(v, &types(&branches)))
            .unwrap_or(Value::Null),
        Node::Map(entries) => {
            let indexed = entries.iter().all(|(k, _)| k.parse::<usize>().is_ok());
            let wants_object = branches
                .iter()
                .any(|b| has_type(b, "object") || b.get("properties").is_some());
            if indexed && (wants_array || !wants_object) {
                let mut entries = entries;
                entries.sort_by_key(|(k, _)| k.parse::<usize>().unwrap_or(usize::MAX));
                let items = items(&branches);
                Value::Array(
                    entries
                        .into_iter()
                        .map(|(_, n)| to_json(n, items, defs))
                        .collect(),
                )
            } else {
                Value::Object(object(entries, &branches, defs))
            }
        }
    }
}

fn leaf(raw: &str, schema: Option<&Value>, defs: &Map<String, Value>) -> Value {
    coerce(raw, &types(&branches(schema, defs)))
}

/// `raw` as the first of `types` it parses as (integer, number, boolean),
/// else as a string.
pub(super) fn coerce(raw: &str, types: &[&str]) -> Value {
    if types.contains(&"integer")
        && let Ok(n) = raw.parse::<i64>()
    {
        return Value::Number(n.into());
    }
    if types.contains(&"number")
        && let Some(n) = raw.parse::<f64>().ok().and_then(Number::from_f64)
    {
        return Value::Number(n);
    }
    if types.contains(&"boolean") {
        match raw {
            "true" => return Value::Bool(true),
            "false" => return Value::Bool(false),
            _ => {}
        }
    }
    Value::String(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pairs(raw: &str) -> Vec<(String, String)> {
        serde_urlencoded::from_str(raw).unwrap_or_default()
    }

    #[test]
    fn nests_without_a_schema() {
        let out = decode(
            pairs("a[b][c]=1&l[1]=y&l[0]=x&e[]=p&e[]=q&flat=1&flat=2&odd[=z&a=gone"),
            None,
            &Map::new(),
        );
        assert_eq!(
            Value::Object(out),
            json!({"a": {"b": {"c": "1"}}, "l": ["x", "y"], "e": ["p", "q"], "flat": "2", "odd[": "z"})
        );
    }

    #[test]
    fn follows_the_schema() {
        let defs: Map<String, Value> = json!({
            "Addr": {"type": "object", "properties": {"zip": {"type": "integer"}}}
        })
        .as_object()
        .cloned()
        .unwrap_or_default();
        let schema = json!({"type": "object", "properties": {
            "n": {"type": "integer"},
            "tags": {"type": "array", "items": {"type": "integer"}},
            "meta": {"anyOf": [
                {"type": "object", "additionalProperties": {"type": "string"}},
                {"type": "string", "enum": [""]}
            ]},
            "addr": {"$ref": "#/$defs/Addr"},
            "filter[x]": {"type": "boolean"}
        }});
        let out = decode(
            pairs("n=3&tags=1&tags=2&meta[0]=7&addr[zip]=10115&filter[x]=true"),
            Some(&schema),
            &defs,
        );
        assert_eq!(
            Value::Object(out),
            json!({"n": 3, "tags": [1, 2], "meta": {"0": "7"}, "addr": {"zip": 10115}, "filter[x]": true})
        );
    }
}
