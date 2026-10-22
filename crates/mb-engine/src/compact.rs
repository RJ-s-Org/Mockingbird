//! The response schema as the prompt shows it: compact, and cut to a budget.
//!
//! Real contracts reach huge schema graphs: Stripe's `payment_intent` pulls
//! in 868 definitions through its expandable fields, 350k tokens printed.
//! The prompt only needs the response's own shape in detail, so a schema over
//! [`PROMPT_SCHEMA_BUDGET`] first loses long descriptions, then the
//! definitions furthest from the root (replaced by a stub). Validation still
//! uses the full schema.

use mb_mcm::Bundle;
use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// Bytes of compact JSON the prompt's schema may take (about 16k tokens).
pub use mb_import::PROMPT_SCHEMA_BUDGET;

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
    fill(doc, budget)
}

/// Keeps the definitions the root references directly, then adds the rest
/// nearest first (smallest first at one depth) while the result stays within
/// `budget`; a definition that does not fit becomes a short stub where a
/// kept one references it, and is dropped otherwise.
fn fill(mut doc: Value, budget: usize) -> Value {
    let depths = def_depths(&doc);
    let root_size = match &doc {
        Value::Object(root) => root
            .iter()
            .filter(|(k, _)| k.as_str() != "$defs")
            .map(|(k, v)| k.len() + 4 + size(v))
            .sum::<usize>(),
        other => size(other),
    } + 12;
    let Some(defs) = doc.get_mut("$defs").and_then(Value::as_object_mut) else {
        return doc;
    };
    let sizes: HashMap<&str, usize> = defs
        .iter()
        .map(|(n, d)| (n.as_str(), n.len() + 4 + size(d)))
        .collect();
    let edges: HashMap<&str, HashSet<String>> = defs
        .iter()
        .map(|(n, d)| {
            let mut out = Vec::new();
            refs(d, &mut out);
            (n.as_str(), out.into_iter().collect())
        })
        .collect();
    let stub_size = |name: &str| 2 * name.len() + 80;
    let mut kept: HashSet<&str> = depths
        .iter()
        .filter(|(_, d)| **d == 0)
        .filter_map(|(n, _)| sizes.get_key_value(n.as_str()).map(|(k, _)| *k))
        .collect();
    let mut stubs: HashSet<String> = kept
        .iter()
        .flat_map(|n| edges.get(n).into_iter().flatten().cloned())
        .filter(|r| !kept.contains(r.as_str()))
        .collect();
    let mut total = root_size
        + kept.iter().map(|n| sizes[n]).sum::<usize>()
        + stubs.iter().map(|n| stub_size(n)).sum::<usize>();
    let mut order: Vec<&str> = sizes
        .keys()
        .copied()
        .filter(|n| !kept.contains(n))
        .collect();
    order.sort_by_key(|n| (depths.get(*n).copied().unwrap_or(u64::MAX), sizes[n]));
    for _ in 0..2 {
        for name in &order {
            if kept.contains(name) || !stubs.contains(*name) {
                continue;
            }
            let new_stubs: Vec<&String> = edges[name]
                .iter()
                .filter(|r| {
                    r.as_str() != *name && !kept.contains(r.as_str()) && !stubs.contains(*r)
                })
                .collect();
            let grown = total - stub_size(name)
                + sizes[name]
                + new_stubs.iter().map(|r| stub_size(r)).sum::<usize>();
            if grown <= budget {
                total = grown;
                let added: Vec<String> = new_stubs.into_iter().cloned().collect();
                stubs.remove(*name);
                stubs.extend(added);
                kept.insert(name);
            }
        }
    }
    let kept: HashSet<String> = kept.into_iter().map(String::from).collect();
    defs.retain(|name, _| kept.contains(name) || stubs.contains(name));
    for (name, def) in defs.iter_mut() {
        if !kept.contains(name) {
            *def = serde_json::json!({
                "description": format!("{name} (schema omitted for size: prefer an id string or null where allowed)")
            });
        }
    }
    doc
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
        // ...what they reference becomes a stub, so every reference still
        // resolves, and what only the stub referenced is dropped.
        assert!(defs["C"].get("properties").is_none(), "{}", defs["C"]);
        assert!(
            defs["C"]["description"]
                .as_str()
                .unwrap()
                .contains("omitted")
        );
        assert!(!defs.contains_key("D"), "{}", out["$defs"]);
    }

    #[test]
    fn small_definitions_are_kept_while_big_siblings_are_stubbed() {
        let doc = json!({
            "$ref": "#/$defs/A",
            "$defs": {
                "A": {"type": "object", "properties": {"big": {"$ref": "#/$defs/Big"}, "small": {"$ref": "#/$defs/Small"}}},
                "Big": {"enum": (0..2000).collect::<Vec<_>>()},
                "Small": {"type": "object", "properties": {"amount": {"type": "integer"}}, "required": ["amount"]}
            }
        });
        let out = fit(doc, 600);
        let defs = out["$defs"].as_object().unwrap();
        assert_eq!(
            defs["Small"]["required"],
            json!(["amount"]),
            "{}",
            out["$defs"]
        );
        assert!(defs["Big"].get("enum").is_none());
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
