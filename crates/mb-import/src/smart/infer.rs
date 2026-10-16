//! Deterministic JSON Schema (2020-12) inference from example values.
//!
//! The result never depends on the order of the samples: object properties
//! and `required` are sorted, enums are sorted, and `anyOf` branches follow a
//! fixed type order.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use super::MAX_EXAMPLES;

/// Schemas nested deeper than this become `{}`.
const MAX_DEPTH: usize = 32;
/// Enum inference: the most distinct values an enum may have.
const ENUM_MAX_DISTINCT: usize = 5;
/// Enum inference: the fewest samples needed.
const ENUM_MIN_SAMPLES: usize = 3;
/// Enum inference: the longest value (in chars) an enum may hold.
const ENUM_MAX_CHARS: usize = 24;
/// Most bodies fed to schema inference per request body or response.
pub(crate) const MAX_SCHEMA_SAMPLES: usize = 20;

/// A JSON Schema 2020-12 every sample validates against.
pub fn infer_schema(samples: &[Value]) -> Value {
    let refs: Vec<&Value> = samples.iter().collect();
    infer(&refs, 1)
}

/// The schema of a body set: inferred from at most `MAX_SCHEMA_SAMPLES`
/// values, in the given order.
pub(crate) fn schema_of(values: &[&Value]) -> Value {
    let samples: Vec<Value> = values
        .iter()
        .take(MAX_SCHEMA_SAMPLES)
        .map(|v| (*v).clone())
        .collect();
    infer_schema(&samples)
}

/// The first `MAX_EXAMPLES` distinct values, in the given order.
pub(crate) fn distinct_examples(values: &[&Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for value in values {
        if out.len() >= MAX_EXAMPLES {
            break;
        }
        if !out.contains(value) {
            out.push((*value).clone());
        }
    }
    out
}

/// Distinct observed parameter values, typed after the inferred scalar
/// schema, capped at `MAX_EXAMPLES`.
pub(crate) fn typed_examples(values: &[&str], schema: &Value) -> Vec<Value> {
    let kind = schema.get("type").and_then(Value::as_str);
    let mut out: Vec<Value> = Vec::new();
    for value in values {
        let typed = match kind {
            Some("integer") => value.parse::<i64>().ok().map(Value::from),
            Some("number") => value.parse::<f64>().ok().map(Value::from),
            Some("boolean") => value.parse::<bool>().ok().map(Value::from),
            _ => None,
        }
        .unwrap_or_else(|| Value::from(*value));
        if out.len() < MAX_EXAMPLES && !out.contains(&typed) {
            out.push(typed);
        }
    }
    out
}

/// A media type without parameters, lowercased (`Application/JSON; charset=utf-8`
/// becomes `application/json`).
pub(crate) fn base_mime(raw: &str) -> String {
    raw.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// The schema of a query/path parameter from its observed string values.
pub fn infer_scalar(samples: &[&str]) -> Value {
    if samples.is_empty() {
        return json!({});
    }
    if samples.iter().all(|s| s.parse::<i64>().is_ok()) {
        return json!({"type": "integer"});
    }
    if samples
        .iter()
        .all(|s| s.parse::<f64>().is_ok_and(f64::is_finite))
    {
        return json!({"type": "number"});
    }
    if samples.iter().all(|s| *s == "true" || *s == "false") {
        return json!({"type": "boolean"});
    }
    Value::Object(string_schema(samples))
}

/// Number of keys in the schema tree; the "richer" of two schemas has more.
pub fn schema_richness(schema: &Value) -> usize {
    match schema {
        Value::Object(map) => map.len() + map.values().map(schema_richness).sum::<usize>(),
        Value::Array(items) => items.iter().map(schema_richness).sum(),
        _ => 0,
    }
}

/// JSON types in `anyOf` order. Integer is folded into number when both occur.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Kind {
    Object,
    Array,
    String,
    Number,
    Integer,
    Boolean,
    Null,
}

impl Kind {
    fn of(value: &Value) -> Kind {
        match value {
            Value::Null => Kind::Null,
            Value::Bool(_) => Kind::Boolean,
            Value::Number(n) if n.is_i64() || n.is_u64() => Kind::Integer,
            Value::Number(_) => Kind::Number,
            Value::String(_) => Kind::String,
            Value::Array(_) => Kind::Array,
            Value::Object(_) => Kind::Object,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Object => "object",
            Kind::Array => "array",
            Kind::String => "string",
            Kind::Number => "number",
            Kind::Integer => "integer",
            Kind::Boolean => "boolean",
            Kind::Null => "null",
        }
    }
}

fn infer(samples: &[&Value], depth: usize) -> Value {
    if samples.is_empty() || depth > MAX_DEPTH {
        return json!({});
    }
    let mut groups: BTreeMap<Kind, Vec<&Value>> = BTreeMap::new();
    for sample in samples {
        groups.entry(Kind::of(sample)).or_default().push(sample);
    }
    if groups.contains_key(&Kind::Number)
        && let Some(ints) = groups.remove(&Kind::Integer)
    {
        groups.entry(Kind::Number).or_default().extend(ints);
    }
    let has_null = groups.remove(&Kind::Null).is_some();
    let mut branches: Vec<Map<String, Value>> = groups
        .iter()
        .map(|(kind, values)| typed_schema(*kind, values, depth))
        .collect();

    match (branches.len(), has_null) {
        (0, _) => json!({"type": "null"}),
        (1, false) => Value::Object(branches.remove(0)),
        (1, true) => {
            let mut schema = branches.remove(0);
            let name = schema.remove("type").unwrap_or(Value::Null);
            if let Some(Value::Array(values)) = schema.get_mut("enum") {
                values.push(Value::Null);
            }
            let mut out = Map::new();
            out.insert("type".into(), json!([name, "null"]));
            out.extend(schema);
            Value::Object(out)
        }
        _ => {
            let mut any_of: Vec<Value> = branches.into_iter().map(Value::Object).collect();
            if has_null {
                any_of.push(json!({"type": "null"}));
            }
            json!({ "anyOf": any_of })
        }
    }
}

/// The schema of samples that all have JSON type `kind` (integers may appear
/// among numbers). The `type` keyword comes first.
fn typed_schema(kind: Kind, values: &[&Value], depth: usize) -> Map<String, Value> {
    match kind {
        Kind::Object => object_schema(values, depth),
        Kind::Array => array_schema(values, depth),
        Kind::String => {
            let strings: Vec<&str> = values.iter().filter_map(|v| v.as_str()).collect();
            string_schema(&strings)
        }
        Kind::Number | Kind::Integer | Kind::Boolean | Kind::Null => {
            let mut map = Map::new();
            map.insert("type".into(), Value::from(kind.name()));
            map
        }
    }
}

fn object_schema(values: &[&Value], depth: usize) -> Map<String, Value> {
    let objects: Vec<&Map<String, Value>> = values.iter().filter_map(|v| v.as_object()).collect();
    let mut by_key: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for object in &objects {
        for (key, value) in object.iter() {
            by_key.entry(key.as_str()).or_default().push(value);
        }
    }
    let mut map = Map::new();
    map.insert("type".into(), Value::from("object"));
    if by_key.is_empty() {
        return map;
    }
    let required: Vec<Value> = by_key
        .iter()
        .filter(|(_, vs)| vs.len() == objects.len())
        .map(|(k, _)| Value::from(*k))
        .collect();
    let properties: Map<String, Value> = by_key
        .iter()
        .map(|(k, vs)| ((*k).to_string(), infer(vs, depth + 1)))
        .collect();
    map.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        map.insert("required".into(), Value::Array(required));
    }
    map
}

fn array_schema(values: &[&Value], depth: usize) -> Map<String, Value> {
    let elements: Vec<&Value> = values
        .iter()
        .filter_map(|v| v.as_array())
        .flatten()
        .collect();
    let mut map = Map::new();
    map.insert("type".into(), Value::from("array"));
    if !elements.is_empty() {
        map.insert("items".into(), infer(&elements, depth + 1));
    }
    map
}

/// `{"type": "string"}` plus a `format` every sample matches, or else an
/// `enum` when the samples look like a small closed set.
fn string_schema(samples: &[&str]) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("type".into(), Value::from("string"));
    if let Some(format) = common_format(samples) {
        map.insert("format".into(), Value::from(format));
    } else if let Some(values) = enum_values(samples) {
        map.insert(
            "enum".into(),
            Value::Array(values.into_iter().map(Value::from).collect()),
        );
    }
    map
}

/// A string format name and its matcher.
type Format = (&'static str, fn(&str) -> bool);

/// Formats in the order they are tried.
const FORMATS: [Format; 4] = [
    ("uuid", is_uuid),
    ("date-time", is_date_time),
    ("email", is_email),
    ("uri", is_uri),
];

fn common_format(samples: &[&str]) -> Option<&'static str> {
    if samples.is_empty() {
        return None;
    }
    FORMATS
        .iter()
        .find(|(_, matches)| samples.iter().all(|s| matches(s)))
        .map(|(name, _)| *name)
}

fn enum_values<'a>(samples: &[&'a str]) -> Option<BTreeSet<&'a str>> {
    if samples.len() < ENUM_MIN_SAMPLES {
        return None;
    }
    let distinct: BTreeSet<&str> = samples.iter().copied().collect();
    let short = distinct.iter().all(|s| s.chars().count() <= ENUM_MAX_CHARS);
    (distinct.len() <= ENUM_MAX_DISTINCT && distinct.len() < samples.len() && short)
        .then_some(distinct)
}

fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn is_date_time(s: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(s).is_ok()
}

fn is_email(s: &str) -> bool {
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    let mut parts = s.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(_), Some(domain), None) => domain.contains('.'),
        _ => false,
    }
}

fn is_uri(s: &str) -> bool {
    s.contains("://") && url::Url::parse(s).is_ok()
}
