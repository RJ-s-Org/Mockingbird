//! Member names a model wrote with JSON around them, mended before an answer
//! is checked.
//!
//! A model sometimes writes a member's name with what should have followed it:
//! `"id:"`, `"System.Title:"`, `" .tag"`, `"indexedDocuments\":null},"`. In an
//! object that admits other members such a name validates, so the member the
//! schema declares is missing, and an answer that does not require it passes
//! without it. [`mend_member_names`] gives each such member its declared name
//! back.

use serde_json::{Map, Value};

/// Schema nodes visited at one place of the value.
const MAX_NODES: usize = 64;

/// How deep [`mend_member_names`] goes into the value.
const MAX_DEPTH: usize = 32;

/// The characters a corrupted member name is cut at: JSON that followed the
/// name.
const CUT_AT: &[char] = &['"', ':', '}', ','];

/// The name `key` was meant to be: trimmed of ASCII whitespace and cut at the
/// first [`CUT_AT`] character, when what that cuts off is JSON debris alone
/// (see [`is_debris`]): `"id:"` and `"total\":null},"` were `id` and `total`,
/// while `name:en`, `id:alt`, `xmlns:foo` and `a,b` are names of their own.
/// `None` when that leaves the key as it was, or leaves nothing.
fn meant(key: &str) -> Option<&str> {
    fn ascii_trim(s: &str) -> &str {
        s.trim_matches(|c: char| c.is_ascii_whitespace())
    }
    let trimmed = ascii_trim(key);
    let (name, rest) = match trimmed.find(CUT_AT) {
        Some(i) => trimmed.split_at(i),
        None => (trimmed, ""),
    };
    let name = ascii_trim(name);
    (!name.is_empty() && name != key && is_debris(rest)).then_some(name)
}

/// Whether `rest`, what followed a member name, is only JSON written after it:
/// ASCII whitespace, the punctuation `"`, `:`, `,`, `{`, `}`, `[` and `]`, the
/// literals `null`, `true` and `false`, numbers and quoted strings. A `"` it
/// starts with closes the name.
fn is_debris(rest: &str) -> bool {
    let mut chars = rest.strip_prefix('"').unwrap_or(rest).chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_ascii_whitespace() => {}
            ':' | ',' | '{' | '}' | '[' | ']' => {}
            '"' => {
                // A quoted string, to its closing quote.
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            chars.next();
                        }
                        '"' => {
                            closed = true;
                            break;
                        }
                        _ => {}
                    }
                }
                if !closed {
                    return false;
                }
            }
            c if c == '-' || c.is_ascii_digit() => {
                while chars
                    .peek()
                    .is_some_and(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
                {
                    chars.next();
                }
            }
            c if c.is_ascii_alphabetic() => {
                let mut word = String::from(c);
                while let Some(c) = chars.peek().copied().filter(char::is_ascii_alphabetic) {
                    word.push(c);
                    chars.next();
                }
                if !matches!(word.as_str(), "null" | "true" | "false") {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// `value`, whose schema is `schema`, with each member name that is not one its
/// object's schema declares (in `properties`) but becomes one once trimmed and
/// cut at its first `"`, `:`, `}` or `,` (and trimmed again) renamed to that one; when the object holds the declared member
/// too, the corrupted one is dropped. It goes on into the members and items
/// the schema describes. A name is never changed to one the schema does not
/// declare, and an object whose schema declares no member (a map) keeps every
/// name. `resolve` gives the schema a `$ref` names (`None` when it is not
/// known). Returns how many names it mended or dropped.
pub fn mend_member_names<'a>(
    schema: &'a Value,
    resolve: &dyn Fn(&str) -> Option<&'a Value>,
    value: &mut Value,
) -> usize {
    mend(&expand(vec![schema], resolve), resolve, value, 0)
}

fn mend<'a>(
    nodes: &[&'a Value],
    resolve: &dyn Fn(&str) -> Option<&'a Value>,
    value: &mut Value,
    depth: usize,
) -> usize {
    if nodes.is_empty() || depth > MAX_DEPTH {
        return 0;
    }
    match value {
        Value::Object(members) => {
            let mut mended = rename(nodes, members);
            for (key, member) in members.iter_mut() {
                let inner: Vec<&Value> = nodes
                    .iter()
                    .filter_map(|n| {
                        n.get("properties")
                            .and_then(|p| p.get(key))
                            .or_else(|| n.get("additionalProperties").filter(|a| a.is_object()))
                    })
                    .collect();
                mended += mend(&expand(inner, resolve), resolve, member, depth + 1);
            }
            mended
        }
        Value::Array(items) => {
            let mut mended = 0;
            for (i, item) in items.iter_mut().enumerate() {
                let inner: Vec<&Value> = nodes
                    .iter()
                    .filter_map(|n| {
                        n.get("prefixItems")
                            .and_then(|p| p.get(i))
                            .or_else(|| n.get("items").filter(|s| s.is_object()))
                    })
                    .collect();
                mended += mend(&expand(inner, resolve), resolve, item, depth + 1);
            }
            mended
        }
        _ => 0,
    }
}

/// Renames (or drops) the corrupted member names of `members`, an object of
/// schema `nodes`, in place, keeping the order of the others.
fn rename(nodes: &[&Value], members: &mut Map<String, Value>) -> usize {
    let declared = |name: &str| {
        nodes.iter().any(|n| {
            n.get("properties")
                .and_then(Value::as_object)
                .is_some_and(|p| p.contains_key(name))
        })
    };
    // A key a `patternProperties` pattern matches is a name the schema allows
    // (one that does not compile is taken to match).
    let patterns: Vec<Option<regex::Regex>> = nodes
        .iter()
        .filter_map(|n| n.get("patternProperties").and_then(Value::as_object))
        .flat_map(|p| p.keys())
        .map(|p| regex::Regex::new(p).ok())
        .collect();
    let patterned = |key: &str| {
        patterns
            .iter()
            .any(|p| p.as_ref().is_none_or(|p| p.is_match(key)))
    };
    let corrupted: Vec<(String, String)> = members
        .keys()
        .filter(|k| !declared(k) && !patterned(k))
        .filter_map(|k| {
            let to = meant(k).filter(|to| declared(to))?;
            Some((k.clone(), to.to_string()))
        })
        .collect();
    if corrupted.is_empty() {
        return 0;
    }
    // Declared names the object holds as they are.
    let written: Vec<String> = members.keys().filter(|k| declared(k)).cloned().collect();
    let mut out = Map::new();
    for (key, member) in std::mem::take(members) {
        match corrupted.iter().find(|(from, _)| *from == key) {
            // The declared member wins, whether the model wrote it under its own
            // name (it is not among the corrupted ones) or an earlier corrupted
            // name took it.
            Some((_, to)) => {
                if !out.contains_key(to) && !written.contains(to) {
                    out.insert(to.clone(), member);
                }
            }
            None => {
                out.insert(key, member);
            }
        }
    }
    *members = out;
    tracing::info!(names = corrupted.len(), "mended corrupted member names");
    corrupted.len()
}

/// `pending` and every schema it reaches through `$ref`, `allOf`, `anyOf` and
/// `oneOf`, bounded.
fn expand<'a>(
    mut pending: Vec<&'a Value>,
    resolve: &dyn Fn(&str) -> Option<&'a Value>,
) -> Vec<&'a Value> {
    let mut out: Vec<&Value> = Vec::new();
    while let Some(node) = pending.pop() {
        if out.len() >= MAX_NODES {
            break;
        }
        if out.iter().any(|n| std::ptr::eq(*n, node)) {
            continue;
        }
        out.push(node);
        if let Some(target) = node.get("$ref").and_then(Value::as_str).and_then(resolve) {
            pending.push(target);
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            if let Some(Value::Array(branches)) = node.get(key) {
                pending.extend(branches);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn mended(schema: &Value, mut value: Value) -> (Value, usize) {
        let resolve = |r: &str| r.strip_prefix('#').and_then(|p| schema.pointer(p));
        let n = mend_member_names(schema, &resolve, &mut value);
        (value, n)
    }

    fn open_item() -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "System.Title": {"type": "string"},
                ".tag": {"type": "string"},
                "indexedDocuments": {"type": ["integer", "null"]},
                "owner": {"$ref": "#/$defs/User"},
                "labels": {"type": "object", "additionalProperties": {"type": "string"}},
                "items": {"type": "array", "items": {"$ref": "#/$defs/User"}}
            },
            "$defs": {"User": {"type": "object", "properties": {"login": {"type": "string"}}}}
        })
    }

    #[test]
    fn a_name_with_json_after_it_is_the_declared_one() {
        let (value, n) = mended(
            &open_item(),
            json!({"id:": "a1", "System.Title:": "Fix", " .tag": "file", "indexedDocuments\":null},": 3}),
        );
        assert_eq!(n, 4);
        assert_eq!(
            value,
            json!({"id": "a1", "System.Title": "Fix", ".tag": "file", "indexedDocuments": 3})
        );
    }

    #[test]
    fn the_declared_member_wins_over_a_corrupted_one() {
        let (value, n) = mended(&open_item(), json!({"id:": "bad", "id": "a1"}));
        assert_eq!(n, 1);
        assert_eq!(value, json!({"id": "a1"}));
        let (value, _) = mended(&open_item(), json!({"id": "a1", " id": "bad"}));
        assert_eq!(value, json!({"id": "a1"}));
    }

    #[test]
    fn a_name_is_never_mended_to_an_undeclared_one() {
        let value = json!({"name:": "x", "id": "a1", "  ": 1, ":": 2});
        let (out, n) = mended(&open_item(), value.clone());
        assert_eq!((out, n), (value, 0));
    }

    #[test]
    fn nested_objects_and_items_are_mended_but_map_keys_are_not() {
        let (value, n) = mended(
            &open_item(),
            json!({
                "owner": {"login:": "ada"},
                "items": [{" login": "bo"}, {"login": "cy"}],
                "labels": {"id:": "kept", "env:": "prod"}
            }),
        );
        assert_eq!(n, 2);
        assert_eq!(
            value,
            json!({
                "owner": {"login": "ada"},
                "items": [{"login": "bo"}, {"login": "cy"}],
                "labels": {"id:": "kept", "env:": "prod"}
            })
        );
    }

    /// What follows the cut must be JSON debris: a name of its own with a `:` or a
    /// `,` in it is kept, beside the declared member or alone.
    #[test]
    fn a_name_whose_rest_is_no_json_is_kept() {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}, "id": {"type": "string"},
                           "xmlns": {"type": "string"}, "a": {"type": "string"}},
            "additionalProperties": {"type": "string"}
        });
        for value in [
            json!({"name": "Ada", "name:en": "Ada"}),
            json!({"name:en": "Ada"}),
            json!({"id": "1", "id:alt": "2"}),
            json!({"id:alt": "2"}),
            json!({"xmlns:foo": "urn:x"}),
            json!({"a,b": "x"}),
            json!({"name}x": "x"}),
            json!({"\u{a0}name": "x"}),
            json!({"name\u{3000}": "x"}),
            json!({"\u{a0}id:": "x"}),
        ] {
            assert_eq!(mended(&schema, value.clone()), (value.clone(), 0), "{value}");
        }
    }

    /// JSON debris after the name is cut off: punctuation, literals, numbers and
    /// quoted strings.
    #[test]
    fn a_name_followed_by_json_debris_is_mended() {
        let schema = json!({"type": "object", "properties": {"name": {"type": "string"}}});
        for key in [
            "name:",
            "name\":null},",
            "name\":\"books\"},",
            "name\": true,",
            "name\":-1.5e3]",
            "\tname ",
            "name\":\"a \\\" b\",",
        ] {
            let (value, n) = mended(&schema, json!({key: "x"}));
            assert_eq!((value, n), (json!({"name": "x"}), 1), "{key}");
        }
        // An unclosed string or a word is no debris.
        for key in ["name\":\"books", "name\":nil", "name: x"] {
            assert_eq!(mended(&schema, json!({key: "x"})).1, 0, "{key}");
        }
    }

    /// A key a `patternProperties` pattern matches is the schema's own.
    #[test]
    fn a_patterned_key_is_kept() {
        let schema = json!({
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "patternProperties": {"^id:": {"type": "string"}}
        });
        let value = json!({"id:": "x"});
        assert_eq!(mended(&schema, value.clone()), (value, 0));
    }

    #[test]
    fn a_schema_declaring_nothing_keeps_every_name() {
        let schema = json!({"type": "object", "additionalProperties": true});
        let value = json!({"id:": 1});
        assert_eq!(mended(&schema, value.clone()), (value, 0));
    }

    #[test]
    fn a_union_branch_declares_its_members() {
        let schema = json!({"oneOf": [
            {"type": "object", "properties": {"a": {"type": "string"}}},
            {"type": "object", "properties": {"b": {"type": "string"}}}
        ]});
        assert_eq!(mended(&schema, json!({"b,": "x"})), (json!({"b": "x"}), 1));
    }
}
