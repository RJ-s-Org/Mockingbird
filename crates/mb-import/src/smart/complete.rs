//! The `required` and nullable post-pass over draft schemas: the docs and
//! their examples say which properties are always present and which may be
//! `null`, whether the schema was written by the model or inferred.

use serde_json::{Map, Value, json};

use super::assemble::MAX_SCHEMA_DEPTH;

/// Description phrases that make a property nullable.
const NULLABLE_PHRASES: [&str; 5] = [
    "nullable",
    "may be null",
    "can be null",
    "or null",
    "optional",
];

/// Adds `required` where missing and marks properties nullable, in place,
/// for every schema with a `properties` object, recursively:
///
/// 1. Without a `required` key: the sorted properties present in every
///    object example; with no object example, the sorted properties whose
///    description says `required` (and neither `not required` nor
///    `optional`); omitted when empty.
/// 2. A property is nullable when an object example has it as `null`, or
///    its description says it may be null or is optional.
/// 3. Each property is completed with that key's non-null example values,
///    and `items` with the elements of the array examples.
pub fn complete_schema(schema: &mut Value, examples: &[&Value]) {
    complete(schema, examples, 0);
}

fn complete(schema: &mut Value, examples: &[&Value], depth: usize) {
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    let Value::Object(obj) = schema else {
        return;
    };
    if let Some(items) = obj.get_mut("items") {
        let elements: Vec<&Value> = examples
            .iter()
            .filter_map(|e| e.as_array())
            .flatten()
            .collect();
        complete(items, &elements, depth + 1);
    }
    let objects: Vec<&Map<String, Value>> = examples.iter().filter_map(|e| e.as_object()).collect();
    let required = match obj.get("required") {
        Some(_) => None,
        None => Some(required_names(obj.get("properties"), &objects)),
    };
    let Some(Value::Object(properties)) = obj.get_mut("properties") else {
        return;
    };
    for (name, property) in properties.iter_mut() {
        let values: Vec<&Value> = objects
            .iter()
            .filter_map(|o| o.get(name))
            .filter(|v| !v.is_null())
            .collect();
        complete(property, &values, depth + 1);
        let null_example = objects
            .iter()
            .any(|o| o.get(name).is_some_and(Value::is_null));
        let null_docs =
            description(property).is_some_and(|d| NULLABLE_PHRASES.iter().any(|p| d.contains(p)));
        if null_example || null_docs {
            make_nullable(property);
        }
    }
    if let Some(required) = required
        && !required.is_empty()
    {
        obj.insert("required".into(), json!(required));
    }
}

/// The sorted names that rule 1 makes required.
fn required_names(properties: Option<&Value>, objects: &[&Map<String, Value>]) -> Vec<String> {
    let Some(Value::Object(properties)) = properties else {
        return Vec::new();
    };
    // `properties` is a `Map` (sorted keys), so the names come out sorted.
    properties
        .iter()
        .filter(|(name, property)| {
            if objects.is_empty() {
                description(property).is_some_and(|d| {
                    d.contains("required") && !d.contains("not required") && !d.contains("optional")
                })
            } else {
                objects.iter().all(|o| o.contains_key(name.as_str()))
            }
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// The lower-cased description of a property schema.
fn description(property: &Value) -> Option<String> {
    property
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_lowercase)
}

/// Whether `schema` already accepts `null`.
fn accepts_null(schema: &Map<String, Value>) -> bool {
    let null_type = |s: &Value| match s.get("type") {
        Some(Value::String(t)) => t == "null",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "null"),
        _ => false,
    };
    null_type(&Value::Object(schema.clone()))
        || ["anyOf", "oneOf"].iter().any(|k| {
            schema
                .get(*k)
                .and_then(Value::as_array)
                .is_some_and(|alts| alts.iter().any(null_type))
        })
}

/// Makes `schema` accept `null`: a string `type` becomes `[type, "null"]`,
/// an array `type` gets `"null"`, no `type` wraps the schema in
/// `anyOf: [schema, {"type": "null"}]`; an `enum` gets `null`.
fn make_nullable(schema: &mut Value) {
    let Value::Object(obj) = schema else {
        return;
    };
    if let Some(Value::Array(values)) = obj.get_mut("enum")
        && !values.iter().any(Value::is_null)
    {
        values.push(Value::Null);
    }
    if accepts_null(obj) {
        return;
    }
    match obj.get_mut("type") {
        Some(Value::String(t)) => {
            let t = std::mem::take(t);
            obj.insert("type".into(), json!([t, "null"]));
        }
        Some(Value::Array(ts)) => ts.push(json!("null")),
        _ => {
            let inner = std::mem::take(obj);
            obj.insert(
                "anyOf".into(),
                json!([Value::Object(inner), {"type": "null"}]),
            );
        }
    }
}
