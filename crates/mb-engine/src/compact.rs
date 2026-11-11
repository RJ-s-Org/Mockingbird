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

/// Longest description kept when the schema is still over budget with its far
/// definitions stubbed (a root with many members).
pub const SHORT_DESCRIPTION: usize = 60;

/// Bytes of compact JSON the schema of an answer the model writes may take (about
/// 7k tokens). Smaller than [`PROMPT_SCHEMA_BUDGET`]: the model reads it on every
/// call, and with structured output reads it twice, while the answer only needs its
/// own members in detail.
pub const RESPONSE_SCHEMA_BUDGET: usize = 24 * 1024;

/// `schema` with its reachable definitions, fitted to the prompt budget. A
/// schema that carries its own `$defs` (one shaped to the output budget) is
/// taken as complete.
/// Its `description` strings have their credentials redacted, as the prompt's other
/// contract text does.
pub fn prompt_schema(bundle: &Bundle, schema: &Value) -> Value {
    prompt_schema_expanding(bundle, schema, &HashSet::new(), PROMPT_SCHEMA_BUDGET)
}

/// [`prompt_schema`], with each id-or-object member (see [`collapse_expandables`])
/// asking for its id unless `expand` names it.
pub(crate) fn prompt_schema_expanding(
    bundle: &Bundle,
    schema: &Value,
    expand: &HashSet<String>,
    budget: usize,
) -> Value {
    let mut doc = if schema.get("$defs").is_some() {
        schema.clone()
    } else {
        bundle.with_reachable_defs(schema)
    };
    if collapse_expandables(&mut doc, None, expand) {
        drop_unreachable_defs(&mut doc);
    }
    redact_descriptions(&mut doc);
    fit(doc, budget)
}

/// The member names a request asks to have expanded: each step of the paths in
/// its `expand` (or `expand[]`) query parameter or body member, a string or a list
/// of strings (`expand[]=line_items`, `expand=data.customer`).
pub(crate) fn expanded_members(call: &crate::OperationCall) -> HashSet<String> {
    let mut out = HashSet::new();
    let body = call.body.as_ref().and_then(Value::as_object);
    for source in [Some(&call.query), body].into_iter().flatten() {
        for key in ["expand", "expand[]"] {
            let paths: Vec<&str> = match source.get(key) {
                Some(Value::String(p)) => vec![p.as_str()],
                Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            for path in paths {
                out.extend(path.split('.').filter(|s| !s.is_empty()).map(String::from));
            }
        }
    }
    out
}

/// Rewrites each id-or-object union in `node` (an `anyOf` or `oneOf` of a plain
/// string, schema references or objects, and `null`) to its string and `null`
/// branches, unless the member holding it is named in `expand`: such a member
/// answers the related record's id unless asked to expand it (Stripe's
/// `customer`, `latest_charge`), and its object branch pulls the related
/// record's whole schema graph into the prompt. `name` is the member `node` is
/// the schema of. Returns whether anything changed. For the prompt only: the
/// answer is validated against the contract's schema, which still allows either.
fn collapse_expandables(node: &mut Value, name: Option<&str>, expand: &HashSet<String>) -> bool {
    let Value::Object(obj) = node else {
        return match node {
            Value::Array(items) => items
                .iter_mut()
                .fold(false, |c, v| collapse_expandables(v, name, expand) | c),
            _ => false,
        };
    };
    let mut changed = false;
    for key in ["anyOf", "oneOf"] {
        let Some(Value::Array(branches)) = obj.get(key) else {
            continue;
        };
        let is_id = |b: &Value| {
            b.get("type").and_then(Value::as_str) == Some("string")
                && b.get("enum").is_none()
                && b.get("const").is_none()
        };
        let is_null = |b: &Value| b.get("type").and_then(Value::as_str) == Some("null");
        let is_object = |b: &Value| {
            b.get("$ref").is_some() || b.get("type").and_then(Value::as_str) == Some("object")
        };
        let expandable = branches.iter().any(is_id)
            && branches.iter().any(is_object)
            && branches
                .iter()
                .all(|b| is_id(b) || is_null(b) || is_object(b))
            && !name.is_some_and(|n| expand.contains(n));
        if expandable {
            let kept: Vec<Value> = branches
                .iter()
                .filter(|b| is_id(b) || is_null(b))
                .cloned()
                .collect();
            obj.insert(key.to_string(), Value::Array(kept));
            changed = true;
        }
    }
    for (key, value) in obj.iter_mut() {
        changed |= match key.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" => match value {
                Value::Object(members) => members.iter_mut().fold(false, |c, (member, schema)| {
                    let next = if key == "properties" || key == "patternProperties" {
                        Some(member.as_str())
                    } else {
                        None
                    };
                    collapse_expandables(schema, next, expand) | c
                }),
                _ => false,
            },
            // An array's items, and a union's or composition's branches, are still
            // the member's.
            "items" | "anyOf" | "oneOf" | "allOf" | "additionalProperties" => {
                collapse_expandables(value, name, expand)
            }
            _ => false,
        };
    }
    changed
}

/// Removes the `$defs` entries no `$ref` from the root (outside `$defs`) reaches.
fn drop_unreachable_defs(doc: &mut Value) {
    let mut pending = Vec::new();
    if let Value::Object(root) = &*doc {
        for (k, v) in root {
            if k != "$defs" {
                refs(v, &mut pending);
            }
        }
    }
    let Some(defs) = doc.get("$defs").and_then(Value::as_object) else {
        return;
    };
    let mut reached: HashSet<String> = HashSet::new();
    while let Some(name) = pending.pop() {
        if reached.insert(name.clone())
            && let Some(def) = defs.get(&name)
        {
            refs(def, &mut pending);
        }
    }
    if let Some(defs) = doc.get_mut("$defs").and_then(Value::as_object_mut) {
        defs.retain(|name, _| reached.contains(name));
    }
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
pub(crate) fn response_prompt_schema(
    bundle: &Bundle,
    schema: &Value,
    expand: &HashSet<String>,
) -> Value {
    crate::encoded::hint(prompt_schema_expanding(
        bundle,
        schema,
        expand,
        RESPONSE_SCHEMA_BUDGET,
    ))
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
        Some(RecordAt::Pointer(tokens)) => pointer_steps(&doc, tokens),
    };
    let mut wanted: Vec<(Vec<Step>, String)> = Vec::new();
    // Each envelope member on the way to the record is required where it is.
    for (at, step) in record.iter().enumerate() {
        if let Step::Member(m) = step {
            wanted.push((record[..at].to_vec(), m.clone()));
        }
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
        let input = match (held.map(|(i, _)| i), op.input.body.as_ref()) {
            (Some(identity), Some(schema)) => crate::record_schema(bundle, identity, schema),
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

/// The steps of a [`RecordAt::Pointer`](mb_mcm::RecordAt::Pointer)'s `tokens` into
/// `doc`: a `"0"` where the schema there is an array's is its items, any other token a
/// member.
fn pointer_steps(doc: &Value, tokens: &[String]) -> Vec<Step> {
    let none = Value::Object(serde_json::Map::new());
    let defs = doc.get("$defs").unwrap_or(&none);
    let mut node = Some(resolved(doc, defs));
    tokens
        .iter()
        .map(|token| {
            let items = node.and_then(|n| n.get("items")).filter(|_| token == "0");
            let (step, next) = match items {
                Some(items) => (Step::Items, Some(items)),
                None => (
                    Step::Member(token.clone()),
                    node.and_then(|n| n.get("properties")?.get(token)),
                ),
            };
            node = next.map(|n| resolved(n, defs));
            step
        })
        .collect()
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
/// [`MAX_DESCRIPTION`] characters; else also with the definitions furthest from
/// the root stubbed (see [`fill`]); else stubbed again from descriptions cut
/// to [`SHORT_DESCRIPTION`].
pub(crate) fn fit(doc: Value, budget: usize) -> Value {
    if size(&doc) <= budget {
        return doc;
    }
    let doc = cut_descriptions(doc, MAX_DESCRIPTION);
    if size(&doc) <= budget {
        return doc;
    }
    let filled = fill(doc.clone(), budget);
    if size(&filled) <= budget {
        return filled;
    }
    // Shorter descriptions, then stubs again: the stubs keep their whole text.
    fill(cut_descriptions(doc, SHORT_DESCRIPTION), budget)
}

/// Adds the definitions nearest the root first (smallest first at one depth)
/// while the result stays within `budget`; a definition that does not fit
/// becomes a short stub where the root or a kept one references it, and is
/// dropped otherwise.
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
    let stub_size = |name: &str| 2 * name.len() + 80;
    // A root that is a reference keeps its definition, which is the answer's own shape.
    // The definitions its members reference start as stubs like the rest, so a root
    // with many large members (Stripe's `payment_intent`) still fits.
    let own = doc
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
        .map(def_name);
    let needed = non_null_refs(&doc);
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
    let mut kept: HashSet<&str> = own
        .as_deref()
        .and_then(|own| sizes.get_key_value(own).map(|(k, _)| *k))
        .into_iter()
        .collect();
    // A root member that cannot be null needs its definition whole: the model would
    // guess its shape from a stub. Only those reached through members that may be
    // null start as stubs (the model can write null).
    kept.extend(
        needed
            .iter()
            .filter_map(|n| sizes.get_key_value(n.as_str()).map(|(k, _)| *k)),
    );
    let mut stubs: HashSet<String> = depths
        .iter()
        .filter(|(n, d)| **d == 0 && sizes.contains_key(n.as_str()))
        .map(|(n, _)| n.clone())
        .chain(
            kept.iter()
                .flat_map(|n| edges.get(n).into_iter().flatten().cloned()),
        )
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

/// The definitions the root needs whole to show its own shape: those it reaches
/// through `$ref`, `items`, `allOf`, union branches, and members that cannot
/// be null, without passing a member or branch that may be null (the model can
/// write null there) or another definition.
fn non_null_refs(doc: &Value) -> HashSet<String> {
    fn admits_null(schema: &Value) -> bool {
        let null_type = |s: &Value| match s.get("type") {
            Some(Value::String(t)) => t == "null",
            Some(Value::Array(ts)) => ts.iter().any(|t| t == "null"),
            _ => false,
        };
        null_type(schema)
            || schema.get("nullable") == Some(&Value::Bool(true))
            || ["anyOf", "oneOf"].iter().any(|k| {
                schema
                    .get(*k)
                    .and_then(Value::as_array)
                    .is_some_and(|bs| bs.iter().any(admits_null))
            })
    }
    fn walk(schema: &Value, out: &mut HashSet<String>) {
        let Value::Object(obj) = schema else {
            return;
        };
        if let Some(rest) = obj
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
        {
            out.insert(def_name(rest));
        }
        if let Some(items) = obj.get("items") {
            walk(items, out);
        }
        if let Some(Value::Array(parts)) = obj.get("allOf") {
            parts.iter().for_each(|p| walk(p, out));
        }
        // A union that admits null may be null: its other branches can go.
        if !admits_null(schema) {
            for key in ["anyOf", "oneOf"] {
                if let Some(Value::Array(branches)) = obj.get(key) {
                    branches.iter().for_each(|b| walk(b, out));
                }
            }
        }
        if let Some(Value::Object(members)) = obj.get("properties") {
            members
                .values()
                .filter(|m| !admits_null(m))
                .for_each(|m| walk(m, out));
        }
    }
    let mut out = HashSet::new();
    if let Value::Object(root) = doc {
        let mut root = root.clone();
        root.remove("$defs");
        walk(&Value::Object(root), &mut out);
    }
    out
}

fn size(v: &Value) -> usize {
    v.to_string().len()
}

fn cut_descriptions(value: Value, max: usize) -> Value {
    match value {
        Value::Object(obj) => Value::Object(
            obj.into_iter()
                .map(|(k, v)| match (k.as_str(), v) {
                    ("description", Value::String(d)) if d.chars().count() > max => {
                        let cut: String = d.chars().take(max).collect();
                        (k, Value::String(format!("{cut}…")))
                    }
                    (_, v) => (k, cut_descriptions(v, max)),
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| cut_descriptions(v, max))
                .collect(),
        ),
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

/// The definition a `#/$defs/` reference's `rest` names: its first step, with
/// the JSON Pointer escapes decoded (`a~1b` is `a/b`), as the bundle names them.
fn def_name(rest: &str) -> String {
    let name = rest.split('/').next().unwrap_or(rest);
    name.replace("~1", "/").replace("~0", "~")
}

fn refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(obj) => {
            for (k, v) in obj {
                match (
                    k.as_str(),
                    v.as_str().and_then(|r| r.strip_prefix("#/$defs/")),
                ) {
                    ("$ref", Some(rest)) => out.push(def_name(rest)),
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
            credentials: Default::default(),
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
        let doc = response_prompt_schema(bundle, &schema, &HashSet::new());
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
        let doc = response_prompt_schema(&bundle, &schema, &HashSet::new());
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

#[cfg(test)]
mod expand_tests {
    use super::*;
    use serde_json::json;

    fn nullable_expandable(def: &str) -> Value {
        json!({"anyOf": [
            {"anyOf": [{"type": "string", "maxLength": 5000}, {"$ref": format!("#/$defs/{def}")}],
             "description": "id or object"},
            {"type": "null"}
        ]})
    }

    #[test]
    fn an_id_or_object_member_asks_for_its_id_unless_expanded() {
        let mut doc = json!({
            "type": "object",
            "properties": {"latest_charge": nullable_expandable("charge"), "customer": nullable_expandable("customer")},
            "$defs": {"charge": {"type": "object", "properties": {"id": {"type": "string"}}},
                      "customer": {"type": "object", "properties": {"id": {"type": "string"}}}}
        });
        let expand: HashSet<String> = ["customer".to_string()].into();
        assert!(collapse_expandables(&mut doc, None, &expand));
        drop_unreachable_defs(&mut doc);
        assert_eq!(
            doc["properties"]["latest_charge"]["anyOf"][0]["anyOf"],
            json!([{"type": "string", "maxLength": 5000}])
        );
        assert_eq!(
            doc["properties"]["customer"]["anyOf"][0]["anyOf"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        assert!(doc["$defs"].get("charge").is_none());
        assert!(doc["$defs"].get("customer").is_some());
    }

    #[test]
    fn expand_paths_name_each_step() {
        let call = crate::OperationCall {
            credentials: Default::default(),
            operation_id: "x".into(),
            path_params: Default::default(),
            query: json!({"expand[]": ["line_items", "data.customer"]})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            headers: Default::default(),
            body: Some(json!({"expand": "payment_intent"})),
        };
        let got = expanded_members(&call);
        for m in ["line_items", "data", "customer", "payment_intent"] {
            assert!(got.contains(m), "{m}: {got:?}");
        }
    }
}

#[cfg(test)]
mod root_member_tests {
    use super::*;
    use serde_json::json;

    fn big() -> Value {
        json!({"type": "object", "properties": {"v": {"enum": (0..300).map(|i| format!("v{i}")).collect::<Vec<_>>()}}})
    }

    #[test]
    fn a_definition_named_with_a_slash_survives_a_collapse() {
        let mut doc = json!({
            "type": "object",
            "properties": {
                "x": {"$ref": "#/$defs/a~1b"},
                "c": {"anyOf": [{"type": "string"}, {"$ref": "#/$defs/C"}]}
            },
            "$defs": {"a/b": {"type": "object"}, "C": {"type": "object"}}
        });
        assert!(collapse_expandables(&mut doc, None, &HashSet::new()));
        drop_unreachable_defs(&mut doc);
        assert!(doc["$defs"].get("a/b").is_some(), "{doc}");
        assert!(doc["$defs"].get("C").is_none(), "{doc}");
    }

    #[test]
    fn an_array_root_keeps_its_item_definition() {
        let doc = json!({"type": "array", "items": {"$ref": "#/$defs/Pet"}, "$defs": {"Pet": big(), "Other": big()}});
        let out = fit(doc, 200);
        assert!(out["$defs"]["Pet"]["properties"].is_object(), "{out}");
    }

    #[test]
    fn short_descriptions_come_before_stubs_which_keep_their_text() {
        let long = "d".repeat(150);
        let props: serde_json::Map<String, Value> = (0..40)
            .map(|i| {
                (
                    format!("m{i}"),
                    json!({"type": "string", "description": long}),
                )
            })
            .collect();
        let mut props = props;
        props.insert(
            "optional".into(),
            json!({"anyOf": [{"$ref": "#/$defs/Optional"}, {"type": "null"}]}),
        );
        let doc = json!({"type": "object", "properties": props, "$defs": {"Optional": big()}});
        let out = fit(doc, 4000);
        let cut = out["properties"]["m0"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(cut.chars().count() <= SHORT_DESCRIPTION + 1, "{cut}");
        let stub = out["$defs"]["Optional"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(stub.ends_with("null where allowed)"), "{stub}");
    }

    #[test]
    fn a_member_that_cannot_be_null_keeps_its_definition_and_a_nullable_one_is_stubbed() {
        let big = || json!({"type": "object", "properties": {"v": {"enum": (0..300).map(|i| format!("v{i}")).collect::<Vec<_>>()}}});
        let doc = json!({
            "type": "object",
            "properties": {
                "needed": {"$ref": "#/$defs/Needed"},
                "optional": {"anyOf": [{"$ref": "#/$defs/Optional"}, {"type": "null"}]}
            },
            "$defs": {"Needed": big(), "Optional": big()}
        });
        let out = fit(doc.clone(), size(&doc["$defs"]["Needed"]) + 400);
        assert!(out["$defs"]["Needed"]["properties"].is_object(), "{out}");
        assert!(
            out["$defs"]["Optional"].get("properties").is_none(),
            "{out}"
        );
    }
}
