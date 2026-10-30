//! GraphQL selection sets for eval targets.
//!
//! A GraphQL client always names the fields it reads, and the server narrows
//! a root field's schema to them before generating (see
//! `mb_protocols::graphql::select`). An eval target called without one would
//! ask the model for the whole type graph, which no real request does. A
//! manifest entry gives each GraphQL target a selection, written as nested
//! JSON: a key per field, `{}` for a leaf, and an optional `"@args"` object
//! of the field's literal arguments.
//!
//! ```json
//! {"login": {}, "repositories": {"@args": {"first": 3}, "nodes": {"name": {}}}}
//! ```
//!
//! [`narrow`] follows the server's rules: every object keeps `__typename`
//! and the selected properties, `$ref`s are inlined, `required` keeps the
//! selected names it listed, `additionalProperties` is dropped, and a field
//! queried with arguments says so in its `description`.

use serde_json::{Map, Value, json};

/// Key of a selection node's literal arguments.
pub const ARGS_KEY: &str = "@args";

/// `schema` narrowed to `sel` (see the module docs). The result has no
/// `$ref`.
pub fn narrow(schema: &Value, defs: &Map<String, Value>, sel: &Map<String, Value>) -> Value {
    let Value::Object(obj) = schema else {
        return schema.clone();
    };
    if let Some(target) = obj.get("$ref").and_then(Value::as_str) {
        return match target.strip_prefix("#/$defs/").and_then(|n| defs.get(n)) {
            Some(def) => narrow(def, defs, sel),
            None => json!({}),
        };
    }
    let mut out = Map::new();
    for (key, value) in obj {
        let narrowed = match key.as_str() {
            "anyOf" | "oneOf" | "allOf" => Value::Array(
                value
                    .as_array()
                    .map(|branches| branches.iter().map(|b| narrow(b, defs, sel)).collect())
                    .unwrap_or_default(),
            ),
            "items" => narrow(value, defs, sel),
            "properties" => Value::Object(properties(value, defs, sel)),
            "required" => Value::Array(required(value, sel)),
            "additionalProperties" | "$defs" => continue,
            _ => value.clone(),
        };
        out.insert(key.clone(), narrowed);
    }
    Value::Object(out)
}

/// The selected field names, in order, with their child selections.
fn fields(sel: &Map<String, Value>) -> impl Iterator<Item = (&String, &Map<String, Value>)> {
    static LEAF: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
    sel.iter()
        .filter(|(k, _)| k.as_str() != ARGS_KEY && k.as_str() != "__typename")
        .map(|(k, v)| (k, v.as_object().unwrap_or(&LEAF)))
}

fn required(value: &Value, sel: &Map<String, Value>) -> Vec<Value> {
    let listed = |name: &str| {
        value
            .as_array()
            .is_some_and(|names| names.iter().any(|n| n.as_str() == Some(name)))
    };
    let mut out = Vec::new();
    if listed("__typename") {
        out.push(json!("__typename"));
    }
    for (name, _) in fields(sel) {
        if listed(name) {
            out.push(json!(name));
        }
    }
    out
}

fn properties(
    value: &Value,
    defs: &Map<String, Value>,
    sel: &Map<String, Value>,
) -> Map<String, Value> {
    let Some(props) = value.as_object() else {
        return Map::new();
    };
    let mut out = Map::new();
    if let Some(typename) = props.get("__typename") {
        out.insert("__typename".into(), typename.clone());
    }
    for (name, child) in fields(sel) {
        // Another member of a union may not have this field.
        let Some(schema) = props.get(name) else {
            continue;
        };
        let mut narrowed = narrow(schema, defs, child);
        if let (Value::Object(obj), Some(Value::Object(args))) =
            (&mut narrowed, child.get(ARGS_KEY))
            && !args.is_empty()
        {
            let note = format!("Queried with arguments {}", Value::Object(args.clone()));
            let description = match obj.get("description").and_then(Value::as_str) {
                Some(d) => format!("{d} {note}"),
                None => note,
            };
            obj.insert("description".into(), json!(description));
        }
        out.insert(name.clone(), narrowed);
    }
    out
}

/// The field names of `sel` that `schema` (after `$ref`s) has nowhere, so
/// a typo in a manifest fails the load instead of silently narrowing to
/// nothing. Names are reported as dotted paths.
pub fn unknown_fields(
    schema: &Value,
    defs: &Map<String, Value>,
    sel: &Map<String, Value>,
) -> Vec<String> {
    let mut out = Vec::new();
    unknown_into(schema, defs, sel, "", &mut out, 0);
    out
}

fn unknown_into(
    schema: &Value,
    defs: &Map<String, Value>,
    sel: &Map<String, Value>,
    prefix: &str,
    out: &mut Vec<String>,
    depth: usize,
) {
    // Selections are shallow; this bounds a cyclic `$ref` walk.
    if depth > 32 {
        return;
    }
    let mut branches = Vec::new();
    collect_objects(schema, defs, &mut branches, 0);
    for (name, child) in fields(sel) {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let found: Vec<&Value> = branches
            .iter()
            .filter_map(|b| b.get("properties").and_then(|p| p.get(name)))
            .collect();
        if found.is_empty() {
            out.push(path);
            continue;
        }
        if fields(child).next().is_some() {
            for f in found {
                unknown_into(f, defs, child, &path, out, depth + 1);
            }
        }
    }
}

/// The object schemas `schema` stands for: itself, its `$ref` target, its
/// `items`, and the branches of `anyOf`/`oneOf`/`allOf`.
fn collect_objects<'a>(
    schema: &'a Value,
    defs: &'a Map<String, Value>,
    out: &mut Vec<&'a Value>,
    hops: usize,
) {
    if hops > 16 {
        return;
    }
    let Value::Object(obj) = schema else {
        return;
    };
    if let Some(def) = obj
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|t| t.strip_prefix("#/$defs/"))
        .and_then(|n| defs.get(n))
    {
        collect_objects(def, defs, out, hops + 1);
    }
    if obj.contains_key("properties") {
        out.push(schema);
    }
    if let Some(items) = obj.get("items") {
        collect_objects(items, defs, out, hops + 1);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        for b in obj.get(key).and_then(Value::as_array).into_iter().flatten() {
            collect_objects(b, defs, out, hops + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs() -> Map<String, Value> {
        json!({
            "User": {
                "type": "object",
                "properties": {
                    "__typename": {"const": "User"},
                    "login": {"type": "string"},
                    "bio": {"type": ["string", "null"]},
                    "repositories": {"$ref": "#/$defs/RepoConnection"}
                },
                "required": ["__typename", "login", "bio", "repositories"],
                "additionalProperties": false
            },
            "RepoConnection": {
                "type": "object",
                "properties": {
                    "nodes": {"type": "array", "items": {"$ref": "#/$defs/Repo"}},
                    "totalCount": {"type": "integer"}
                },
                "required": ["nodes", "totalCount"]
            },
            "Repo": {
                "type": "object",
                "properties": {"name": {"type": "string"}, "owner": {"$ref": "#/$defs/User"}},
                "required": ["name", "owner"]
            }
        })
        .as_object()
        .cloned()
        .unwrap_or_default()
    }

    fn sel(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn keeps_only_selected_fields_and_inlines_refs() {
        let s = sel(
            json!({"login": {}, "repositories": {"@args": {"first": 2}, "nodes": {"name": {}}}}),
        );
        let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &s);
        assert_eq!(
            out,
            json!({
                "type": "object",
                "properties": {
                    "__typename": {"const": "User"},
                    "login": {"type": "string"},
                    "repositories": {
                        "type": "object",
                        "description": "Queried with arguments {\"first\":2}",
                        "properties": {
                            "nodes": {"type": "array", "items": {
                                "type": "object",
                                "properties": {"name": {"type": "string"}},
                                "required": ["name"]
                            }}
                        },
                        "required": ["nodes"]
                    }
                },
                "required": ["__typename", "login", "repositories"]
            })
        );
    }

    #[test]
    fn reports_unknown_fields_by_path() {
        let s = sel(
            json!({"login": {}, "nmae": {}, "repositories": {"nodes": {"owner": {"lgin": {}}}}}),
        );
        let unknown = unknown_fields(&json!({"$ref": "#/$defs/User"}), &defs(), &s);
        assert_eq!(unknown, vec!["nmae", "repositories.nodes.owner.lgin"]);
    }
}
