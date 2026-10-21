//! The response schema as the prompt shows it: compact, and cut to a budget.
//!
//! Real contracts reach huge schema graphs: Stripe's `payment_intent` pulls
//! in 868 definitions through its expandable fields, 350k tokens printed.
//! The prompt only needs the response's own shape in detail, so a schema over
//! [`PROMPT_SCHEMA_BUDGET`] first loses long descriptions, then the
//! definitions furthest from the root (replaced by a stub). Validation still
//! uses the full schema.

use mb_mcm::Bundle;
use std::collections::HashMap;

use serde_json::Value;

/// Bytes of compact JSON the prompt's schema may take (about 16k tokens).
pub const PROMPT_SCHEMA_BUDGET: usize = 64 * 1024;

/// Longest description kept once the schema is over budget.
pub const MAX_DESCRIPTION: usize = 160;

/// `schema` with its reachable definitions, fitted to the prompt budget.
pub fn prompt_schema(bundle: &Bundle, schema: &Value) -> Value {
    fit(bundle.with_reachable_defs(schema), PROMPT_SCHEMA_BUDGET)
}

/// `doc` (a schema carrying `$defs`) fitted to `budget` bytes of compact
/// JSON: unchanged when it fits; else with descriptions cut to
/// [`MAX_DESCRIPTION`] characters; else also with the definitions more than
/// N `$ref` hops from the root stubbed, for the largest N that fits (0 when
/// none does).
pub(crate) fn fit(doc: Value, budget: usize) -> Value {
    if size(&doc) <= budget {
        return doc;
    }
    let doc = cut_descriptions(doc);
    if size(&doc) <= budget {
        return doc;
    }
    let depths = def_depths(&doc);
    let deepest = depths.values().copied().max().unwrap_or(0);
    let mut fitted = doc.clone();
    for limit in (0..deepest).rev() {
        fitted = stub_beyond(&doc, &depths, limit);
        if size(&fitted) <= budget {
            break;
        }
    }
    fitted
}

fn size(v: &Value) -> usize {
    v.to_string().len()
}

fn cut_descriptions(value: Value) -> Value {
    match value {
        Value::Object(obj) => Value::Object(
            obj.into_iter()
                .map(|(k, v)| match (k.as_str(), v) {
                    ("description", Value::String(d)) if d.chars().count() > MAX_DESCRIPTION => {
                        let cut: String = d.chars().take(MAX_DESCRIPTION).collect();
                        (k, Value::String(format!("{cut}…")))
                    }
                    (_, v) => (k, cut_descriptions(v)),
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(cut_descriptions).collect()),
        other => other,
    }
}

/// The `$ref` hops from the root schema to each definition: 0 for those the
/// root references directly.
fn def_depths(doc: &Value) -> HashMap<String, u64> {
    let empty = serde_json::Map::new();
    let defs = doc
        .get("$defs")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut depths = HashMap::new();
    let mut frontier = Vec::new();
    if let Value::Object(root) = doc {
        let mut root = root.clone();
        root.remove("$defs");
        refs(&Value::Object(root), &mut frontier);
    }
    let mut depth = 0u64;
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for name in frontier {
            if depths.contains_key(&name) {
                continue;
            }
            if let Some(def) = defs.get(&name) {
                refs(def, &mut next);
            }
            depths.insert(name, depth);
        }
        frontier = next;
        depth += 1;
    }
    depths
}

fn refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(obj) => {
            for (k, v) in obj {
                match (
                    k.as_str(),
                    v.as_str().and_then(|r| r.strip_prefix("#/$defs/")),
                ) {
                    ("$ref", Some(name)) => {
                        out.push(name.split('/').next().unwrap_or(name).to_string())
                    }
                    _ => refs(v, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
        _ => {}
    }
}

/// `doc` with every definition deeper than `limit` replaced by a stub.
fn stub_beyond(doc: &Value, depths: &HashMap<String, u64>, limit: u64) -> Value {
    let mut out = doc.clone();
    if let Some(defs) = out.get_mut("$defs").and_then(Value::as_object_mut) {
        for (name, def) in defs.iter_mut() {
            let depth = depths.get(name).copied().unwrap_or(u64::MAX);
            if depth > limit {
                *def = serde_json::json!({
                    "description": format!("{name}: schema omitted for size. Where a field allows an id string instead, use one; otherwise a small {name} object.")
                });
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_schema_within_budget_is_unchanged() {
        let doc = json!({"$ref": "#/$defs/A", "$defs": {"A": {"type": "object", "description": "x".repeat(500)}}});
        assert_eq!(fit(doc.clone(), 10_000), doc);
    }

    #[test]
    fn long_descriptions_are_cut_first() {
        let doc = json!({
            "$ref": "#/$defs/A",
            "$defs": {"A": {"type": "object", "properties": {"b": {"type": "string", "description": "d".repeat(2000)}}}}
        });
        let out = fit(doc, 1000);
        let d = out["$defs"]["A"]["properties"]["b"]["description"]
            .as_str()
            .unwrap();
        assert!(d.chars().count() <= MAX_DESCRIPTION + 1, "{d}");
        assert_eq!(out["$defs"]["A"]["properties"]["b"]["type"], "string");
    }

    #[test]
    fn far_definitions_are_stubbed_until_it_fits() {
        // A -> B -> C -> D, each carrying a large enum.
        let big = |next: Option<&str>| {
            let mut props =
                json!({"v": {"enum": (0..200).map(|i| format!("value-{i}")).collect::<Vec<_>>()}});
            if let Some(n) = next {
                props["next"] =
                    json!({"anyOf": [{"type": "string"}, {"$ref": format!("#/$defs/{n}")}]});
            }
            json!({"type": "object", "properties": props})
        };
        let doc = json!({
            "$ref": "#/$defs/A",
            "$defs": {"A": big(Some("B")), "B": big(Some("C")), "C": big(Some("D")), "D": big(None)}
        });
        let one = size(&doc["$defs"]["A"]);
        let out = fit(doc, one * 2 + 600);
        assert!(size(&out) <= one * 2 + 600, "{}", size(&out));
        let defs = out["$defs"].as_object().unwrap();
        // The root and its direct reference keep their shape...
        assert!(defs["A"]["properties"]["v"].is_object());
        assert!(defs["B"]["properties"]["v"].is_object());
        // ...the rest become stubs, so every reference still resolves.
        assert!(defs["C"].get("properties").is_none(), "{}", defs["C"]);
        assert!(defs["D"].get("properties").is_none());
        assert!(
            defs["C"]["description"]
                .as_str()
                .unwrap()
                .contains("omitted")
        );
    }

    #[test]
    fn nothing_fits_keeps_only_the_root() {
        let doc = json!({
            "$ref": "#/$defs/A",
            "$defs": {"A": {"type": "object", "properties": {"b": {"$ref": "#/$defs/B"}}}, "B": {"enum": (0..500).collect::<Vec<_>>()}}
        });
        let out = fit(doc, 10);
        assert!(out["$defs"]["A"]["properties"].is_object());
        assert!(out["$defs"]["B"].get("enum").is_none());
    }
}
