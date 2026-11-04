//! What the contract says of the members of a body the mock builds itself.
//!
//! A refusal (an LLM's, a repeated create's, a stale write's) and a `404` for what the
//! session does not hold are rendered from the contract's response for that status,
//! faker-filled. The faker draws a member it knows nothing more of at random, so an
//! error's `code` read `lorem` where the response's description says the status answers
//! `` `version_conflict` ``. After the faker, each scalar member of such a body takes the
//! value the contract gives it, by the first of these sources that has one:
//!
//! 1. its schema's `const`, or its `enum` when that holds one value;
//! 2. a value this status's response description names in code position (between
//!    backticks, or in `<code>`), when the description ties it to the member: the
//!    member's `enum` holds it and no other value the description names, or the
//!    description names the member itself in code position (its name, or its dotted
//!    path such as `error.code`) and, after it in the same sentence, one value only:
//!    `` `code` is `version_conflict` or `edit_conflict` `` names two, and is no guess.
//!    A text that is the name of a member of the body is no value (`` The `code` and
//!    `message` members `` names two members, not a code);
//! 3. its schema's example (`example`, else the first of `examples`, not a boolean's),
//!    only where no other status's response uses that schema: an example of a shared
//!    `Error` says nothing of this status.
//!
//! A member no source names keeps the faker's value, and a number that is the response's
//! own HTTP status, as the engine reflects it into a problem's `status`, is never
//! changed. Each change is kept only while the whole body still validates, so a value
//! that does not fit the branch the faker took changes nothing. Members are told apart by
//! structure alone: no name is special.

use std::collections::{HashMap, HashSet};

use mb_mcm::{Bundle, StatusSelector};
use serde_json::Value;

use crate::consistency::{deref, example_value, property_schema};
use crate::validate::SchemaValidator;

/// How deep a body or a schema is walked.
const MAX_DEPTH: usize = 8;
/// Longest text in code position taken as a value.
const MAX_SPAN_CHARS: usize = 128;
/// Values a description names that are looked at, at most.
const MAX_SPANS: usize = 32;
/// Definitions followed from one schema, at most.
const MAX_DEFS: usize = 256;

/// `body`, valid against `validator`, with each scalar member the contract names a
/// value for (see the module's documentation) holding that value. `schema` and
/// `description` are those of the response for `status`. A body that is not valid to
/// begin with is left as it is.
pub(crate) fn fill(
    bundle: &Bundle,
    validator: &SchemaValidator,
    (schema, status): (&Value, u16),
    description: Option<&str>,
    body: Value,
) -> Value {
    if !validator.is_valid(&body) {
        return body;
    }
    let spans = description.map(sentenced_spans).unwrap_or_default();
    let mut names = HashSet::new();
    property_names(bundle, schema, 0, &mut HashSet::new(), &mut names);
    let mut walk = Walk {
        bundle,
        spans: &spans,
        names: &names,
        status,
        shared: None,
        changes: Vec::new(),
    };
    let alone = !walk.enters_shared(schema);
    walk.value(schema, &body, &mut String::new(), ("", ""), alone, 0);
    let mut out = body;
    for (pointer, value) in walk.changes {
        let mut candidate = out.clone();
        match candidate.pointer_mut(&pointer) {
            Some(slot) => *slot = value,
            None => continue,
        }
        if validator.is_valid(&candidate) {
            out = candidate;
        }
    }
    out
}

/// What a description names in code position, in order: the text between a pair of
/// backticks, and inside `<code>`, trimmed, when it is short and not empty.
#[must_use]
pub fn code_spans(text: &str) -> Vec<String> {
    sentenced_spans(text).into_iter().map(|(span, _)| span).collect()
}

/// [`code_spans`], each with the number of the sentence it stands in: a `.`, `!` or `?`
/// outside code, followed by a space or the end, ends one.
fn sentenced_spans(text: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut sentence = 0;
    while out.len() < MAX_SPANS {
        let tick = rest.find('`');
        let tag = rest.find("<code>");
        let (start, inner, after) = match (tick, tag) {
            (Some(t), g) if g.is_none_or(|g| t < g) => {
                let open = rest[t..].bytes().take_while(|b| *b == b'`').count();
                let fence = "`".repeat(open);
                let body = &rest[t + open..];
                let Some(end) = body.find(&fence) else { break };
                (t, &body[..end], &body[end + open..])
            }
            (_, Some(g)) => {
                let body = &rest[g + "<code>".len()..];
                let Some(end) = body.find("</code>") else {
                    break;
                };
                (g, &body[..end], &body[end + "</code>".len()..])
            }
            _ => break,
        };
        sentence += sentence_ends(&rest[..start]);
        let inner = inner.trim();
        if !inner.is_empty() && inner.chars().count() <= MAX_SPAN_CHARS {
            out.push((inner.to_string(), sentence));
        }
        rest = after;
    }
    out
}

/// How many sentences `prose` ends.
fn sentence_ends(prose: &str) -> usize {
    let chars: Vec<char> = prose.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|n| n.is_whitespace())
        })
        .count()
}

/// Every member name `schema` declares, at any depth, through `$ref`s and compositions.
fn property_names(
    bundle: &Bundle,
    schema: &Value,
    depth: usize,
    seen: &mut HashSet<String>,
    out: &mut HashSet<String>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Some(name) = def_ref(schema) {
        if !seen.insert(name.to_string()) || seen.len() > MAX_DEFS {
            return;
        }
        if let Some(target) = bundle.defs.get(name) {
            property_names(bundle, target, depth + 1, seen, out);
        }
        return;
    }
    if let Some(Value::Object(properties)) = schema.get("properties") {
        for (name, inner) in properties {
            out.insert(name.clone());
            property_names(bundle, inner, depth + 1, seen, out);
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(inner) = schema.get(key).filter(|v| v.is_object()) {
            property_names(bundle, inner, depth + 1, seen, out);
        }
    }
    for key in ["allOf", "oneOf", "anyOf"] {
        for inner in schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            property_names(bundle, inner, depth + 1, seen, out);
        }
    }
}

/// The definition `schema` refers to, by name.
fn def_ref(schema: &Value) -> Option<&str> {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
}

/// Every definition `schema` reaches, by name.
fn reached_defs(bundle: &Bundle, schema: &Value, out: &mut HashSet<String>) {
    fn walk(bundle: &Bundle, value: &Value, out: &mut HashSet<String>, depth: usize) {
        if depth > 64 || out.len() > MAX_DEFS {
            return;
        }
        match value {
            Value::Object(members) => {
                if let Some(name) = def_ref(value)
                    && out.insert(name.to_string())
                    && let Some(target) = bundle.defs.get(name)
                {
                    walk(bundle, target, out, depth + 1);
                }
                for inner in members.values() {
                    walk(bundle, inner, out, depth + 1);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| walk(bundle, v, out, depth + 1)),
            _ => {}
        }
    }
    walk(bundle, schema, out, 0);
}

/// The statuses whose responses reach each definition, across every operation: `None`
/// for a `default` response, which answers any.
fn statuses_by_def(bundle: &Bundle) -> HashMap<String, HashSet<Option<u16>>> {
    let mut out: HashMap<String, HashSet<Option<u16>>> = HashMap::new();
    for op in &bundle.operations {
        for variant in &op.outputs {
            let Some(schema) = variant.schema.as_ref() else {
                continue;
            };
            let status = match variant.status {
                StatusSelector::Code(code) => Some(code),
                StatusSelector::Default => None,
            };
            let mut defs = HashSet::new();
            reached_defs(bundle, schema, &mut defs);
            for def in defs {
                out.entry(def).or_default().insert(status);
            }
        }
    }
    out
}

struct Walk<'a> {
    bundle: &'a Bundle,
    /// What the description names in code position, with its sentence.
    spans: &'a [(String, usize)],
    /// Every member name the body's schema declares.
    names: &'a HashSet<String>,
    /// The response's HTTP status.
    status: u16,
    /// [`statuses_by_def`], made when first needed.
    shared: Option<HashMap<String, HashSet<Option<u16>>>>,
    /// JSON pointers into the body, with the value the contract gives them.
    changes: Vec<(String, Value)>,
}

impl Walk<'_> {
    /// Whether a definition `schema` is, or composes, is used by a response of another
    /// status than this one.
    fn enters_shared(&mut self, schema: &Value) -> bool {
        let mut node = schema;
        let mut names = Vec::new();
        for _ in 0..16 {
            let Some(name) = def_ref(node) else { break };
            names.push(name.to_string());
            match self.bundle.defs.get(name) {
                Some(target) => node = target,
                None => break,
            }
        }
        for key in ["allOf", "oneOf", "anyOf"] {
            for inner in node
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = def_ref(inner) {
                    names.push(name.to_string());
                }
            }
        }
        if names.is_empty() {
            return false;
        }
        let bundle = self.bundle;
        let shared = self.shared.get_or_insert_with(|| statuses_by_def(bundle));
        let status = Some(self.status);
        names.iter().any(|name| {
            shared
                .get(name)
                .is_some_and(|statuses| statuses.iter().any(|s| *s != status))
        })
    }

    /// `value` at `pointer`, of `schema`: the member named and dotted as `at`, under
    /// examples this status alone uses when `alone`.
    fn value(
        &mut self,
        schema: &Value,
        value: &Value,
        pointer: &mut String,
        at: (&str, &str),
        alone: bool,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let (name, dotted) = at;
        match value {
            Value::Object(members) => {
                for (key, member) in members {
                    let Some(inner) = property_schema(self.bundle, schema, key) else {
                        continue;
                    };
                    let path = match dotted {
                        "" => key.clone(),
                        _ => format!("{dotted}.{key}"),
                    };
                    let alone = alone && !self.enters_shared(&inner);
                    let len = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                    self.value(&inner, member, pointer, (key, &path), alone, depth + 1);
                    pointer.truncate(len);
                }
            }
            Value::Array(items) => {
                let Some(inner) = deref(self.bundle, schema).get("items").cloned() else {
                    return;
                };
                let alone = alone && !self.enters_shared(&inner);
                for (i, item) in items.iter().enumerate() {
                    let len = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&i.to_string());
                    self.value(&inner, item, pointer, at, alone, depth + 1);
                    pointer.truncate(len);
                }
            }
            scalar => {
                // What the engine reflects of the HTTP status stays.
                if scalar.as_u64() == Some(u64::from(self.status)) {
                    return;
                }
                if let Some(named) = self.named(schema, (name, dotted), alone)
                    && &named != scalar
                {
                    self.changes.push((pointer.clone(), named));
                }
            }
        }
    }

    /// The value the contract gives a scalar member of `schema` named and dotted as `at`
    /// (see the module's documentation).
    fn named(&self, schema: &Value, (name, dotted): (&str, &str), alone: bool) -> Option<Value> {
        let node = deref(self.bundle, schema);
        if let Some(fixed) = node.get("const") {
            return scalar(fixed);
        }
        let values = node.get("enum").and_then(Value::as_array);
        if let Some([only]) = values.map(Vec::as_slice) {
            return scalar(only);
        }
        if let Some(described) = self.described(node, values, (name, dotted)) {
            return Some(described);
        }
        if alone && let Some(example) = example_value(self.bundle, node) {
            return scalar(&example);
        }
        None
    }

    /// What the description names for the member (source 2 of the module's
    /// documentation): one value, or none.
    fn described(
        &self,
        node: &Value,
        values: Option<&Vec<Value>>,
        (name, dotted): (&str, &str),
    ) -> Option<Value> {
        if self.spans.is_empty() || name.is_empty() {
            return None;
        }
        if let Some(values) = values {
            let mut named = values
                .iter()
                .filter(|v| self.spans.iter().any(|(s, _)| matches_span(v, s)));
            return match (named.next(), named.next()) {
                (Some(one), None) => scalar(one),
                _ => None,
            };
        }
        let mut found: Vec<Value> = Vec::new();
        // Every value after the member's name, to the end of its sentence.
        for (at, (span, sentence)) in self.spans.iter().enumerate() {
            if span != name && span != dotted {
                continue;
            }
            let after = self.spans[at + 1..]
                .iter()
                .take_while(|(_, s)| s == sentence)
                .filter(|(value, _)| !self.names.contains(value));
            for (value, _) in after {
                if let Some(value) = typed(node, value)
                    && !found.contains(&value)
                {
                    found.push(value);
                }
            }
        }
        match found.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }
}

/// `value` when it is a scalar a member can hold.
fn scalar(value: &Value) -> Option<Value> {
    (!value.is_object() && !value.is_array() && !value.is_null()).then(|| value.clone())
}

/// Whether `value` is the text `span` names.
fn matches_span(value: &Value, span: &str) -> bool {
    match value {
        Value::String(text) => text == span,
        Value::Number(n) => n.to_string() == span,
        _ => false,
    }
}

/// The text `span` as a value of `node`'s type: a number for a member typed only as a
/// number or an integer, otherwise the text.
fn typed(node: &Value, span: &str) -> Option<Value> {
    let types: Vec<&str> = match node.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let numeric = !types.is_empty()
        && !types.contains(&"string")
        && types.iter().any(|t| matches!(*t, "integer" | "number"));
    if numeric {
        let parsed: Value = serde_json::from_str(span).ok()?;
        return parsed.is_number().then_some(parsed);
    }
    Some(Value::String(span.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_spans_read_backticks_and_code_tags_in_order() {
        assert_eq!(
            code_spans("Set `code` to `version_conflict`; see <code>E42</code> and ``a`b``."),
            vec!["code", "version_conflict", "E42", "a`b"]
        );
        assert!(code_spans("no code here, and an `unclosed one").is_empty());
        assert!(code_spans("`  `").is_empty());
    }

    #[test]
    fn typed_spans_follow_the_member_type() {
        assert_eq!(
            typed(&serde_json::json!({"type": "integer"}), "409"),
            Some(409.into())
        );
        assert_eq!(typed(&serde_json::json!({"type": "integer"}), "x"), None);
        assert_eq!(
            typed(&serde_json::json!({"type": ["string", "null"]}), "409"),
            Some("409".into())
        );
    }
}
