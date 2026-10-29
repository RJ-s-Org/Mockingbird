//! Base64 fields. Models write base64 badly: GitHub's README `content`
//! came back as a garbled line, then as invalid UTF-8, which made `gh repo
//! view` fail. The prompt asks for the plain text of such a field instead,
//! and every answer is checked: a value that does not decode is encoded
//! here from its text.
//!
//! A field is base64 when its schema says `format: byte` or
//! `contentEncoding: base64`, or when it is the `content` of an object whose
//! `encoding` is `"base64"` (GitHub's contents API).

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use mb_mcm::Bundle;
use serde_json::{Map, Value};

use crate::consistency::deref;

/// What the prompt says about a base64 field.
pub const PLAIN_TEXT_HINT: &str =
    "Write the plain decoded text, not base64: the server base64-encodes it.";

/// Bounds the schema walk (recursive types).
const MAX_DEPTH: usize = 24;

/// `doc` (a prompt schema) with [`PLAIN_TEXT_HINT`] as the description of
/// every base64 field it declares.
pub(crate) fn hint(mut doc: Value) -> Value {
    hint_in(&mut doc);
    doc
}

fn hint_in(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            if declares_base64(obj) {
                obj.insert("description".into(), Value::from(PLAIN_TEXT_HINT));
            }
            if let Some(Value::Object(props)) = obj.get_mut("properties")
                && props.contains_key("encoding")
                && let Some(Value::Object(content)) = props.get_mut("content")
                && content.get("type").and_then(Value::as_str) == Some("string")
            {
                let note = match content.get("description").and_then(Value::as_str) {
                    Some(d) => format!("{d} {PLAIN_TEXT_HINT}"),
                    None => PLAIN_TEXT_HINT.to_string(),
                };
                content.insert("description".into(), Value::from(note));
            }
            obj.values_mut().for_each(hint_in);
        }
        Value::Array(items) => items.iter_mut().for_each(hint_in),
        _ => {}
    }
}

fn declares_base64(schema: &Map<String, Value>) -> bool {
    schema.get("format").and_then(Value::as_str) == Some("byte")
        || schema
            .get("contentEncoding")
            .and_then(Value::as_str)
            .is_some_and(|e| e.eq_ignore_ascii_case("base64"))
}

/// `body` (answered for `schema`) with every base64 field that does not
/// decode replaced by the encoding of its text; `None` when nothing
/// changed.
pub(crate) fn patch(bundle: &Bundle, schema: &Value, body: &Value) -> Option<Value> {
    let mut patched = body.clone();
    let mut changed = false;
    walk(bundle, Some(schema), &mut patched, 0, &mut changed);
    changed.then_some(patched)
}

fn walk(
    bundle: &Bundle,
    schema: Option<&Value>,
    value: &mut Value,
    depth: usize,
    changed: &mut bool,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let schema = schema.map(|s| deref(bundle, s));
    let branches = schema.map(|s| branches(bundle, s)).unwrap_or_default();
    match value {
        Value::String(s) => {
            let binary = branches
                .iter()
                .any(|b| b.as_object().is_some_and(declares_base64));
            if binary && let Some(fixed) = fix(s, true) {
                *s = fixed;
                *changed = true;
            }
        }
        Value::Array(items) => {
            let item = branches.iter().find_map(|b| b.get("items"));
            for v in items {
                walk(bundle, item, v, depth + 1, changed);
            }
        }
        Value::Object(obj) => {
            let base64_content = obj
                .get("encoding")
                .and_then(Value::as_str)
                .is_some_and(|e| e.eq_ignore_ascii_case("base64"));
            for (key, v) in obj.iter_mut() {
                if base64_content
                    && key == "content"
                    && let Value::String(s) = v
                {
                    if let Some(fixed) = fix(s, false) {
                        *s = fixed;
                        *changed = true;
                    }
                    continue;
                }
                let member = branches
                    .iter()
                    .find_map(|b| b.get("properties").and_then(|p| p.get(key)));
                walk(bundle, member, v, depth + 1, changed);
            }
        }
        _ => {}
    }
}

/// `schema` and its `allOf`/`anyOf`/`oneOf` members (one level), each
/// dereferenced.
fn branches<'a>(bundle: &'a Bundle, schema: &'a Value) -> Vec<&'a Value> {
    let mut out = vec![schema];
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(members) = schema.get(key).and_then(Value::as_array) {
            out.extend(members.iter().map(|m| deref(bundle, m)));
        }
    }
    out
}

/// The base64 of `s`'s text, or `None` when `s` already decodes (to any
/// bytes when `binary`, else to UTF-8 text). A value that is not base64 at
/// all is taken as the text; a corrupt one keeps what decodes as readable
/// text.
fn fix(s: &str, binary: bool) -> Option<String> {
    // Line breaks wrap base64; a space means text.
    let compact: String = s.chars().filter(|c| !matches!(c, '\n' | '\r')).collect();
    let shaped = compact
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='));
    let decoded = shaped.then(|| decode(&compact)).flatten();
    if let Some(bytes) = &decoded
        && (binary || std::str::from_utf8(bytes).is_ok())
    {
        return None;
    }
    let recovered = match decoded {
        Some(bytes) => readable(&bytes),
        None if shaped => readable(&decode_lossy(&compact)),
        None => String::new(),
    };
    // Nothing readable: the value was text after all (`README`).
    let text = if recovered.trim().is_empty() {
        s.to_string()
    } else {
        recovered
    };
    Some(encode(&text, s.contains('\n')))
}

fn decode(s: &str) -> Option<Vec<u8>> {
    STANDARD
        .decode(s)
        .ok()
        .or_else(|| STANDARD_NO_PAD.decode(s.trim_end_matches('=')).ok())
}

/// What decodes of a corrupt base64 string: whole 4-character groups, a
/// bad group skipped.
fn decode_lossy(s: &str) -> Vec<u8> {
    let chars: Vec<char> = s.trim_end_matches('=').chars().collect();
    let mut out = Vec::new();
    for group in chars.chunks(4) {
        let group: String = group.iter().collect();
        if let Ok(bytes) = STANDARD_NO_PAD.decode(&group) {
            out.extend(bytes);
        }
    }
    out
}

/// `bytes` as text: invalid UTF-8 and control characters (other than line
/// breaks and tabs) dropped.
fn readable(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| *c != char::REPLACEMENT_CHARACTER)
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// `text` in base64, in lines of 60 characters each ending in `\n` (as
/// GitHub sends it) when `lines`.
fn encode(text: &str, lines: bool) -> String {
    let encoded = STANDARD.encode(text.as_bytes());
    if !lines {
        return encoded;
    }
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 60 + 1);
    for chunk in encoded.as_bytes().chunks(60) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decoded(s: &str) -> String {
        let compact: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        String::from_utf8(STANDARD.decode(compact).unwrap()).unwrap()
    }

    #[test]
    fn valid_base64_is_kept() {
        let ok = STANDARD.encode("# Trellis\n");
        assert_eq!(fix(&ok, false), None);
        assert_eq!(fix(&format!("{}\n{}\n", &ok[..8], &ok[8..]), false), None);
    }

    #[test]
    fn plain_text_is_encoded() {
        let fixed = fix("# Trellis\n\nA fast HTTP router.", false).unwrap();
        assert_eq!(decoded(&fixed), "# Trellis\n\nA fast HTTP router.");
    }

    #[test]
    fn corrupt_base64_keeps_its_readable_text() {
        // Valid base64 of invalid UTF-8 between two readable parts.
        let mut bytes = b"# Trellis\n".to_vec();
        bytes.extend([0xff, 0xfe, 0xfd]);
        bytes.extend(b"\nA router.\n");
        let corrupt = STANDARD.encode(&bytes);
        let fixed = fix(&corrupt, false).unwrap();
        assert_eq!(decoded(&fixed), "# Trellis\n\nA router.\n");
        // Binary content may be any bytes.
        assert_eq!(fix(&corrupt, true), None);
        // A broken group is skipped.
        let broken = format!("{}*{}", STANDARD.encode("abc"), STANDARD.encode("def"));
        assert!(fix(&broken, false).is_some());
    }

    #[test]
    fn github_style_lines() {
        let text = "x".repeat(100);
        let fixed = encode(&text, true);
        assert!(fixed.lines().all(|l| l.len() <= 60));
        assert!(fixed.ends_with('\n'));
        assert_eq!(decoded(&fixed), text);
    }

    #[test]
    fn hint_marks_base64_fields() {
        let doc = hint(json!({
            "type": "object",
            "properties": {
                "content": {"type": "string"},
                "encoding": {"type": "string"},
                "blob": {"type": "string", "format": "byte"},
                "name": {"type": "string"},
            },
        }));
        assert_eq!(doc["properties"]["content"]["description"], PLAIN_TEXT_HINT);
        assert_eq!(doc["properties"]["blob"]["description"], PLAIN_TEXT_HINT);
        assert!(doc["properties"]["name"].get("description").is_none());
    }
}
