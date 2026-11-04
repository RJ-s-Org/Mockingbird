//! What the contract says of the members of a body the mock builds itself.
//!
//! A refusal (an LLM's, a repeated create's, a stale write's) and a `404` for what the
//! session does not hold are rendered from the contract's response for that status,
//! faker-filled. The faker draws a member it knows nothing more of at random, so an
//! error's `code` read `lorem` where the response's schema gives it an example, or its
//! description says the status answers `` `version_conflict` ``. After the faker, each
//! scalar member of such a body takes the value the contract gives it, by the first of
//! these sources that has one:
//!
//! 1. its schema's `const`;
//! 2. its schema's `enum`, when that holds one value;
//! 3. its schema's example (`example`, else the first of `examples`), not a boolean's;
//! 4. a value the response's description names in code position (between backticks, or
//!    in `<code>`), when the description ties it to the member: the member's `enum`
//!    holds it and no other value the description names, or the description names the
//!    member itself in code position (its name, or its dotted path such as
//!    `error.code`) right before the value.
//!
//! A member no source names keeps the faker's value. Each change is kept only while the
//! whole body still validates, so a value that does not fit the branch the faker took
//! changes nothing. Members are told apart by structure alone: no name is special.

use mb_mcm::Bundle;
use serde_json::Value;

use crate::consistency::{deref, example_value, property_schema};
use crate::validate::SchemaValidator;

/// How deep a body is walked.
const MAX_DEPTH: usize = 8;
/// Longest text in code position taken as a value.
const MAX_SPAN_CHARS: usize = 128;
/// Values a description names that are looked at, at most.
const MAX_SPANS: usize = 32;

/// `body`, valid against `validator`, with each scalar member the contract names a
/// value for (see the module's documentation) holding that value. `description` is the
/// response's own. A body that is not valid to begin with is left as it is.
pub(crate) fn fill(
    bundle: &Bundle,
    validator: &SchemaValidator,
    schema: &Value,
    description: Option<&str>,
    body: Value,
) -> Value {
    if !validator.is_valid(&body) {
        return body;
    }
    let spans = description.map(code_spans).unwrap_or_default();
    let mut walk = Walk {
        bundle,
        spans: &spans,
        changes: Vec::new(),
    };
    walk.value(schema, &body, &mut String::new(), "", "", 0);
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
pub(crate) fn code_spans(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while out.len() < MAX_SPANS {
        let tick = rest.find('`');
        let tag = rest.find("<code>");
        let (inner, after) = match (tick, tag) {
            (Some(t), g) if g.is_none_or(|g| t < g) => {
                let open = rest[t..].bytes().take_while(|b| *b == b'`').count();
                let fence = "`".repeat(open);
                let body = &rest[t + open..];
                let Some(end) = body.find(&fence) else { break };
                (&body[..end], &body[end + open..])
            }
            (_, Some(g)) => {
                let body = &rest[g + "<code>".len()..];
                let Some(end) = body.find("</code>") else {
                    break;
                };
                (&body[..end], &body[end + "</code>".len()..])
            }
            _ => break,
        };
        let inner = inner.trim();
        if !inner.is_empty() && inner.chars().count() <= MAX_SPAN_CHARS {
            out.push(inner.to_string());
        }
        rest = after;
    }
    out
}

struct Walk<'a> {
    bundle: &'a Bundle,
    spans: &'a [String],
    /// JSON pointers into the body, with the value the contract gives them.
    changes: Vec<(String, Value)>,
}

impl Walk<'_> {
    /// `value` at `pointer`, of `schema`: the member `name` at the dotted path `dotted`.
    fn value(
        &mut self,
        schema: &Value,
        value: &Value,
        pointer: &mut String,
        name: &str,
        dotted: &str,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
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
                    let len = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                    self.value(&inner, member, pointer, key, &path, depth + 1);
                    pointer.truncate(len);
                }
            }
            Value::Array(items) => {
                let Some(inner) = deref(self.bundle, schema).get("items").cloned() else {
                    return;
                };
                for (i, item) in items.iter().enumerate() {
                    let len = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&i.to_string());
                    self.value(&inner, item, pointer, name, dotted, depth + 1);
                    pointer.truncate(len);
                }
            }
            scalar => {
                if let Some(named) = self.named(schema, name, dotted)
                    && &named != scalar
                {
                    self.changes.push((pointer.clone(), named));
                }
            }
        }
    }

    /// The value the contract gives a scalar member of `schema` named `name`, at the
    /// dotted path `dotted` (see the module's documentation).
    fn named(&self, schema: &Value, name: &str, dotted: &str) -> Option<Value> {
        let node = deref(self.bundle, schema);
        if let Some(fixed) = node.get("const") {
            return scalar(fixed);
        }
        let values = node.get("enum").and_then(Value::as_array);
        if let Some([only]) = values.map(Vec::as_slice) {
            return scalar(only);
        }
        if let Some(example) = example_value(self.bundle, node) {
            return scalar(&example);
        }
        if self.spans.is_empty() || name.is_empty() {
            return None;
        }
        if let Some(values) = values {
            let mut named = values
                .iter()
                .filter(|v| self.spans.iter().any(|s| matches_span(v, s)));
            return match (named.next(), named.next()) {
                (Some(one), None) => scalar(one),
                _ => None,
            };
        }
        self.spans
            .windows(2)
            .find(|pair| pair[0] == name || pair[0] == dotted)
            .and_then(|pair| typed(node, &pair[1]))
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
