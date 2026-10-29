//! Shared JSON Schema normalisation: ref rewriting and OpenAPI 3.0 dialect fixes.

use std::fmt::Write as _;

use serde_json::{Map, Value};

const DEFS_PREFIX: &str = "#/$defs/";

/// Convert an OpenAPI/Swagger schema into JSON Schema 2020-12.
///
/// `ref_prefix` is the source component prefix (e.g. `#/components/schemas/`)
/// which is rewritten to `#/$defs/`. When `oas30` is set, the OpenAPI 3.0
/// dialect keywords (`nullable`, boolean `exclusiveMinimum`, ...) are converted.
/// Refs are rewritten, never inlined, so recursive schemas are safe.
pub(crate) fn normalize_schema(schema: &Value, ref_prefix: &str, oas30: bool) -> Value {
    match schema {
        Value::Object(obj) => Value::Object(normalize_object(obj, ref_prefix, oas30)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| normalize_schema(v, ref_prefix, oas30))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn normalize_map_of_schemas(map: &Map<String, Value>, ref_prefix: &str, oas30: bool) -> Value {
    Value::Object(
        map.iter()
            .map(|(k, v)| (k.clone(), normalize_schema(v, ref_prefix, oas30)))
            .collect(),
    )
}

fn normalize_object(obj: &Map<String, Value>, ref_prefix: &str, oas30: bool) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in obj {
        if oas30
            && matches!(
                key.as_str(),
                "example" | "xml" | "discriminator" | "externalDocs"
            )
        {
            continue;
        }
        let new = match (key.as_str(), value) {
            ("$ref", Value::String(r)) => Value::String(match r.strip_prefix(ref_prefix) {
                Some(name) => format!("{DEFS_PREFIX}{name}"),
                None => r.clone(),
            }),
            ("properties" | "patternProperties" | "$defs" | "definitions", Value::Object(m)) => {
                normalize_map_of_schemas(m, ref_prefix, oas30)
            }
            (
                "items"
                | "additionalProperties"
                | "not"
                | "if"
                | "then"
                | "else"
                | "contains"
                | "propertyNames"
                | "unevaluatedProperties"
                | "unevaluatedItems",
                v,
            )
            | ("allOf" | "anyOf" | "oneOf" | "prefixItems", v) => {
                normalize_schema(v, ref_prefix, oas30)
            }
            _ => value.clone(),
        };
        out.insert(key.clone(), new);
    }
    if oas30 {
        fix_exclusive_bounds(&mut out);
        apply_nullable(&mut out);
    }
    out
}

/// 3.0: `exclusiveMinimum: true` + `minimum: n` -> `exclusiveMinimum: n` (same for max).
fn fix_exclusive_bounds(obj: &mut Map<String, Value>) {
    for (flag, bound) in [
        ("exclusiveMinimum", "minimum"),
        ("exclusiveMaximum", "maximum"),
    ] {
        match obj.get(flag) {
            Some(Value::Bool(true)) => {
                obj.remove(flag);
                if let Some(n) = obj.remove(bound) {
                    obj.insert(flag.into(), n);
                }
            }
            Some(Value::Bool(false)) => {
                obj.remove(flag);
            }
            _ => {}
        }
    }
}

/// 3.0 `nullable: true` / 2.0 `x-nullable: true` -> `type: [T, "null"]`.
fn apply_nullable(obj: &mut Map<String, Value>) {
    // `x-nullable` is the Swagger 2.0 vendor spelling of the same flag.
    let flags = [obj.remove("nullable"), obj.remove("x-nullable")];
    if !flags.contains(&Some(Value::Bool(true))) {
        return;
    }
    let null = || Value::String("null".into());
    match obj.get_mut("type") {
        Some(Value::String(t)) => {
            let t = Value::String(std::mem::take(t));
            obj.insert("type".into(), Value::Array(vec![t, null()]));
        }
        Some(Value::Array(types)) => {
            if !types.contains(&null()) {
                types.push(null());
            }
        }
        _ => {
            // No `type` (e.g. a bare `$ref`): allow null alongside the schema.
            let inner = Value::Object(std::mem::take(obj));
            obj.insert(
                "anyOf".into(),
                Value::Array(vec![inner, serde_json::json!({"type": "null"})]),
            );
            return;
        }
    }
    if let Some(Value::Array(values)) = obj.get_mut("enum")
        && !values.contains(&Value::Null)
    {
        values.push(Value::Null);
    }
}

/// Keywords whose object value maps names (not keywords) to schemas.
const NAME_MAPS: [&str; 5] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];

/// Collect every `$ref` string found in `value`, with the JSON pointer (relative
/// to `value`) of the object holding it. Vendor extensions (`x-*` keywords)
/// are annotations and are skipped: their refs are neither rewritten by
/// [`normalize_schema`] nor resolved by anything downstream.
pub(crate) fn collect_refs<'a>(value: &'a Value, out: &mut Vec<(String, &'a str)>) {
    walk_refs(value, &mut String::new(), false, out);
}

/// Walks `value`, whose pointer is `at`. `at` is one buffer extended and
/// truncated around each child, so a pointer `String` is allocated only
/// for a collected `$ref`.
fn walk_refs<'a>(value: &'a Value, at: &mut String, names: bool, out: &mut Vec<(String, &'a str)>) {
    let len = at.len();
    match value {
        Value::Object(m) => {
            for (k, v) in m {
                let recurse_names = match (k.as_str(), v) {
                    _ if names => false,
                    ("$ref", Value::String(r)) => {
                        out.push((at.clone(), r));
                        continue;
                    }
                    ("enum" | "const" | "default" | "examples", _) => continue,
                    (k, _) if k.starts_with("x-") => continue,
                    (k, _) => NAME_MAPS.contains(&k),
                };
                at.push('/');
                for c in k.chars() {
                    match c {
                        '~' => at.push_str("~0"),
                        '/' => at.push_str("~1"),
                        c => at.push(c),
                    }
                }
                walk_refs(v, at, recurse_names, out);
                at.truncate(len);
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                // Writing to a String cannot fail.
                let _ = write!(at, "/{i}");
                walk_refs(v, at, false, out);
                at.truncate(len);
            }
        }
        _ => {}
    }
}

/// Resolve a `#/$defs/Name` ref to its definition name, if it has that shape.
pub(crate) fn defs_name(r: &str) -> Option<&str> {
    r.strip_prefix(DEFS_PREFIX)
}

#[cfg(test)]
mod perf {
    use super::collect_refs;
    use serde_json::{Map, Value, json};

    /// A schema tree of about `n` nodes with one `$ref` in each property.
    fn wide(n: usize) -> Value {
        let mut props = Map::new();
        for i in 0..n / 8 {
            props.insert(
                format!("p/{i}~x"),
                json!({"type": "object", "properties": {
                    "a": {"type": "string"},
                    "b": {"type": "array", "items": [{"type": "integer"}, {"type": "number"}]},
                    "c": {"$ref": "#/$defs/C"}
                }}),
            );
        }
        json!({"type": "object", "properties": props})
    }

    /// MB-IMPORT-PERF-8: `collect_refs` over `n` and `10n` nodes. Run with
    /// `cargo test -p mockingbird-import --release --lib perf -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing harness"]
    fn perf_8_collect_refs() {
        for n in [100_000, 1_000_000] {
            let schema = wide(n);
            let t = (0..3)
                .map(|_| {
                    let start = std::time::Instant::now();
                    let mut refs = Vec::new();
                    collect_refs(&schema, &mut refs);
                    std::hint::black_box(refs);
                    start.elapsed()
                })
                .min()
                .unwrap_or_default();
            println!("PERF-8 n={n}: collect_refs {t:?}");
        }
    }

    #[test]
    fn collect_refs_escapes_pointer_segments() {
        let schema = json!({
            "properties": {"a/b~c": {"items": [{"$ref": "#/$defs/X"}]}},
            "allOf": [{"$ref": "#/$defs/Y"}],
            "x-skip": {"$ref": "#/$defs/Z"},
            "enum": [{"$ref": "#/$defs/E"}],
            "$defs": {"$ref": {"$ref": "#/$defs/D"}},
            "$ref": "#/$defs/R"
        });
        let mut refs = Vec::new();
        collect_refs(&schema, &mut refs);
        refs.sort();
        assert_eq!(
            refs,
            vec![
                (String::new(), "#/$defs/R"),
                ("/$defs/$ref".to_string(), "#/$defs/D"),
                ("/allOf/0".to_string(), "#/$defs/Y"),
                ("/properties/a~1b~0c/items/0".to_string(), "#/$defs/X"),
            ]
        );
    }
}
