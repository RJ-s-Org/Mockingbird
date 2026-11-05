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

/// `schema` with its reachable definitions, fitted to the prompt budget. A
/// schema that carries its own `$defs` (one shaped to the output budget) is
/// taken as complete.
/// Its `description` strings have their credentials redacted, as the prompt's other
/// contract text does.
pub fn prompt_schema(bundle: &Bundle, schema: &Value) -> Value {
    let mut doc = if schema.get("$defs").is_some() {
        schema.clone()
    } else {
        bundle.with_reachable_defs(schema)
    };
    redact_descriptions(&mut doc);
    fit(doc, PROMPT_SCHEMA_BUDGET)
}

/// Every `description` string in `value`, at any depth, with its credentials
/// redacted (see [`crate::prompt::redacted_text`]); one with none is left as written.
/// A member named `description` whose value is a schema is walked, not replaced.
fn redact_descriptions(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            for (k, v) in obj.iter_mut() {
                match v {
                    Value::String(d) if k == "description" => {
                        if let Some(redacted) = crate::prompt::redacted_text(d) {
                            *d = redacted;
                        }
                    }
                    other => redact_descriptions(other),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_descriptions),
        _ => {}
    }
}

/// [`prompt_schema`] for a response the engine finishes (see
/// [`crate::encoded`]): base64 fields ask for their plain text.
pub(crate) fn response_prompt_schema(bundle: &Bundle, schema: &Value) -> Value {
    crate::encoded::hint(prompt_schema(bundle, schema))
}

/// Most members [`with_echoes_required`] adds to one answer's `required` lists.
pub(crate) const MAX_ECHOED_REQUIRED: usize = 12;

/// One step into a schema: a member of its `properties`, or its `items`.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    Member(String),
    Items,
}

/// `doc`, the prompt's schema of `op`'s 2xx answer of `status` to `call`, with members
/// the answer is sure to need added to its `required` lists, since some models write
/// only what a schema requires: the operation's identity member on the record the
/// answer holds (at its [`mb_mcm::RecordAt`], whose envelope member is required too),
/// or `id` on a create naming none; a list read's one declared list member; and on a
/// create or update, the record's top-level members the request sets (no `null`),
/// which the answer echoes. Only members the schema declares at that place, none it
/// marks `writeOnly`, none the request's schema marks `readOnly` or `writeOnly`, and at
/// most [`MAX_ECHOED_REQUIRED`]. A `$ref` on the way is replaced by a copy of its
/// definition, so no other use of the definition changes. For the prompt only:
/// validation keeps the contract's schema.
pub(crate) fn with_echoes_required(
    bundle: &Bundle,
    op: &mb_mcm::Operation,
    status: u16,
    call: &crate::OperationCall,
    mut doc: Value,
) -> Value {
    use mb_mcm::{Effect, OperationKind, RecordAt};
    if !(200..300).contains(&status) || !doc.is_object() {
        return doc;
    }
    let identity = op.identity.as_ref();
    let record: Vec<Step> = match identity.and_then(|i| i.record.as_ref()) {
        None => Vec::new(),
        Some(RecordAt::Member(m)) => vec![Step::Member(m.clone())],
        Some(RecordAt::Item) => vec![Step::Items],
        Some(RecordAt::MemberItem(m)) => vec![Step::Member(m.clone()), Step::Items],
    };
    let mut wanted: Vec<(Vec<Step>, String)> = Vec::new();
    if let Some(RecordAt::Member(m) | RecordAt::MemberItem(m)) =
        identity.and_then(|i| i.record.as_ref())
    {
        wanted.push((Vec::new(), m.clone()));
    }
    let is_list = op.kind == OperationKind::Query
        && identity.is_none_or(|i| i.path_param.is_none() && i.input_field.is_none());
    if is_list {
        if let [member] = crate::consistency::list_members(bundle, &doc).as_slice() {
            wanted.push((Vec::new(), member.clone()));
        }
    } else if let Some(identity) = identity {
        // A key held in an object member (`metadata.name`) needs that member and the
        // key in it.
        let mut at = record.clone();
        let mut steps: Vec<&str> = identity.field.split('.').collect();
        let leaf = steps.pop().unwrap_or(identity.field.as_str());
        for step in steps {
            wanted.push((at.clone(), step.to_owned()));
            at.push(Step::Member(step.to_owned()));
        }
        wanted.push((at, leaf.to_owned()));
    } else if op.kind == OperationKind::Command(Effect::Create) {
        wanted.push((record.clone(), "id".to_string()));
    }
    let writes = matches!(
        op.kind,
        OperationKind::Command(Effect::Create | Effect::Update)
    );
    if let Some(body) = call.body.as_ref().filter(|_| writes) {
        let held = identity
            .filter(|i| i.record.is_some())
            .and_then(|i| Some((i, i.record_in(body)?)));
        let sent = held.map_or(body, |(_, record)| record);
        // The request's schema of what it sent: of the record its envelope holds.
        let input = match (
            held.and_then(|(i, _)| i.record.as_ref()),
            op.input.body.as_ref(),
        ) {
            (Some(RecordAt::Member(m)), Some(schema)) => {
                crate::consistency::property_schema(bundle, schema, m)
            }
            (Some(RecordAt::MemberItem(m)), Some(schema)) => {
                crate::consistency::property_schema(bundle, schema, m).and_then(|list| {
                    crate::consistency::deref(bundle, &list)
                        .get("items")
                        .cloned()
                })
            }
            (Some(RecordAt::Item), Some(schema)) => crate::consistency::deref(bundle, schema)
                .get("items")
                .cloned(),
            (_, schema) => schema.cloned(),
        };
        let input = input.as_ref();
        for (name, value) in sent.as_object().into_iter().flatten() {
            let flagged = input
                .and_then(|schema| crate::consistency::property_schema(bundle, schema, name))
                .is_some_and(|member| {
                    let member = crate::consistency::deref(bundle, &member).clone();
                    ["readOnly", "writeOnly"]
                        .iter()
                        .any(|k| member.get(*k) == Some(&Value::Bool(true)))
                });
            if !value.is_null() && !flagged {
                wanted.push((record.clone(), name.clone()));
            }
        }
    }
    let defs = doc.as_object_mut().and_then(|root| root.remove("$defs"));
    let none = Value::Object(serde_json::Map::new());
    let mut added = 0;
    for (path, name) in &wanted {
        if added < MAX_ECHOED_REQUIRED
            && require_at(&mut doc, path, name, defs.as_ref().unwrap_or(&none))
        {
            added += 1;
        }
    }
    if let (Some(defs), Some(root)) = (defs, doc.as_object_mut()) {
        root.insert("$defs".to_string(), defs);
    }
    doc
}

/// Whether [`require_at`] adds `name` at `path`: the schema there declares it (in its
/// own `properties` or an `allOf` part's), not as `writeOnly`, and requires it nowhere
/// yet.
fn would_require(doc: &Value, path: &[Step], name: &str, defs: &Value) -> bool {
    let mut node = resolved(doc, defs);
    for step in path {
        let next = match step {
            Step::Member(m) => node.get("properties").and_then(|p| p.get(m)),
            Step::Items => node.get("items"),
        };
        let Some(next) = next else {
            return false;
        };
        node = resolved(next, defs);
    }
    let parts: Vec<&Value> = std::iter::once(node)
        .chain(
            node.get("allOf")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|part| resolved(part, defs)),
        )
        .collect();
    let requires = parts.iter().any(|part| {
        part.get("required")
            .and_then(Value::as_array)
            .is_some_and(|r| r.iter().any(|n| n.as_str() == Some(name)))
    });
    let declared = parts
        .iter()
        .find_map(|part| part.get("properties").and_then(|p| p.get(name)))
        .map(|member| resolved(member, defs));
    !requires && declared.is_some_and(|m| m.get("writeOnly") != Some(&Value::Bool(true)))
}

/// `node` with a `$ref` to one of `defs` replaced by a copy of the definition (its
/// own members beside the `$ref`, a `description`, kept), as long as it is one.
fn inline(node: &mut Value, defs: &Value) {
    for _ in 0..8 {
        let Some(def) = node
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| defs.get(name))
            .filter(|def| def.is_object())
        else {
            return;
        };
        let mut copy = def.clone();
        if let (Some(own), Some(out)) = (node.as_object(), copy.as_object_mut()) {
            for (k, v) in own {
                if k != "$ref" && !out.contains_key(k) {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        *node = copy;
    }
}

/// `schema` with `$ref`s to `defs` followed, without copying.
fn resolved<'a>(mut schema: &'a Value, defs: &'a Value) -> &'a Value {
    for _ in 0..8 {
        match schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| defs.get(name))
        {
            Some(def) => schema = def,
            None => break,
        }
    }
    schema
}

/// Adds `name` to the `required` of the object schema at `path` in `doc` (see
/// [`with_echoes_required`]), when that schema declares it (in its own `properties`
/// or an `allOf` part's) and not as `writeOnly`. Whether it was added.
fn require_at(doc: &mut Value, path: &[Step], name: &str, defs: &Value) -> bool {
    // Nothing is copied for a member required already, or not declared there.
    if !would_require(doc, path, name, defs) {
        return false;
    }
    let mut node = doc;
    for step in path {
        inline(node, defs);
        let next = match step {
            Step::Member(m) => node.get_mut("properties").and_then(|p| p.get_mut(m)),
            Step::Items => node.get_mut("items"),
        };
        let Some(next) = next else {
            return false;
        };
        node = next;
    }
    inline(node, defs);
    let parts = std::iter::once(&*node).chain(
        node.get("allOf")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|part| resolved(part, defs)),
    );
    let mut declared = None;
    for part in parts {
        if let Some(member) = part.get("properties").and_then(|p| p.get(name)) {
            declared = Some(resolved(member, defs));
            break;
        }
    }
    let Some(member) = declared else {
        return false;
    };
    if member.get("writeOnly") == Some(&Value::Bool(true)) {
        return false;
    }
    let Some(obj) = node.as_object_mut() else {
        return false;
    };
    let required = obj
        .entry("required")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(list) = required.as_array_mut() else {
        return false;
    };
    if list.iter().any(|r| r.as_str() == Some(name)) {
        return false;
    }
    list.push(Value::from(name));
    true
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

    /// A contract whose schemas require nothing: an envelope create, a list, a shared
    /// `Item` definition.
    const ECHOES: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /items:
    post:
      operationId: createItem
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                item:
                  type: object
                  properties: {title: {type: string}, secret: {type: string, writeOnly: true}, stamp: {type: string, readOnly: true}, extra: {type: string}}
      responses:
        "201":
          description: created
          content:
            application/json:
              schema:
                type: object
                properties:
                  item: {$ref: "#/components/schemas/Item"}
    get:
      operationId: listItems
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  total: {type: integer}
                  entries: {type: array, items: {$ref: "#/components/schemas/Item"}}
components:
  schemas:
    Item:
      type: object
      properties:
        id: {type: string}
        title: {type: string}
        secret: {type: string, writeOnly: true}
        stamp: {type: string}
        parent: {$ref: "#/components/schemas/Item"}
"##;

    fn call(body: Option<Value>) -> crate::OperationCall {
        crate::OperationCall {
            operation_id: String::new(),
            path_params: serde_json::Map::new(),
            query: serde_json::Map::new(),
            headers: serde_json::Map::new(),
            body,
        }
    }

    fn sent(bundle: &Bundle, op_id: &str, body: Option<Value>) -> Value {
        let op = bundle.operation(op_id).unwrap();
        let schema = op.success_output().unwrap().schema.clone().unwrap();
        let doc = response_prompt_schema(bundle, &schema);
        with_echoes_required(bundle, op, 201, &call(body), doc)
    }

    fn names(schema: &Value) -> Vec<&str> {
        schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_create_requires_its_envelope_its_identity_and_what_the_request_set() {
        let bundle = mb_import::compile(ECHOES.as_bytes()).unwrap();
        let body = json!({"item": {"title": "t", "secret": "s", "stamp": "x", "extra": null}});
        let out = sent(&bundle, "createItem", Some(body));
        assert_eq!(names(&out), ["item"], "{out}");
        let record = &out["properties"]["item"];
        // The record is this answer's copy: `title` sent, `secret` writeOnly, `stamp`
        // readOnly in the request, `extra` null and undeclared.
        assert_eq!(names(record), ["id", "title"], "{out}");
        // The shared definition, and the nested use of it, are as they were.
        let defs = out["$defs"].as_object().unwrap();
        let shared = defs
            .values()
            .find(|d| d.get("properties").is_some())
            .unwrap();
        assert!(shared.get("required").is_none(), "{out}");
    }

    #[test]
    fn a_list_requires_its_list_member_only() {
        let bundle = mb_import::compile(ECHOES.as_bytes()).unwrap();
        let op = bundle.operation("listItems").unwrap();
        let schema = op.success_output().unwrap().schema.clone().unwrap();
        let doc = response_prompt_schema(&bundle, &schema);
        let out = with_echoes_required(&bundle, op, 200, &call(None), doc.clone());
        assert_eq!(names(&out), ["entries"], "{out}");
        // Not for an error answer.
        let error = with_echoes_required(&bundle, op, 404, &call(None), doc.clone());
        assert_eq!(error, doc);
    }

    #[test]
    fn echoed_members_are_bounded() {
        let props: serde_json::Map<String, Value> = (0..30)
            .map(|i| (format!("m{i}"), json!({"type": "string"})))
            .collect();
        let contract = json!({
            "openapi": "3.0.0",
            "info": {"title": "T", "version": "1"},
            "paths": {"/things": {"post": {
                "operationId": "createThing",
                "requestBody": {"content": {"application/json": {"schema": {"type": "object", "properties": props}}}},
                "responses": {"201": {"description": "created", "content": {"application/json": {"schema": {"type": "object", "properties": props}}}}}
            }}}
        });
        let bundle = mb_import::compile(contract.to_string().as_bytes()).unwrap();
        let body: serde_json::Map<String, Value> =
            (0..30).map(|i| (format!("m{i}"), json!("v"))).collect();
        let out = sent(&bundle, "createThing", Some(Value::Object(body)));
        assert_eq!(names(&out).len(), MAX_ECHOED_REQUIRED, "{out}");
    }
}
