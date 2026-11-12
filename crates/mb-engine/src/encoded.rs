//! Base64 fields. Models write base64 badly: GitHub's README `content`
//! came back as a garbled line, then as invalid UTF-8, which made `gh repo
//! view` fail. The prompt asks for the plain text of such a field instead,
//! and every answer is checked: a value that does not decode is encoded
//! here from its text.
//!
//! A field is base64 when its schema says `format: byte` or
//! `contentEncoding: base64`, or when it is the `content` of an object whose
//! `encoding` is `"base64"` (GitHub's contents API) or whose schema declares a
//! string `content` beside an `encoding` that may say `"base64"`: a free-form one
//! (GitHub's), or one whose `enum` (or `const`) names it. The prompt asks for such
//! a declared `content` as plain text, so a model may well write another encoding
//! (`"utf-8"`) beside it: the server encodes it all the same and makes `encoding`
//! say `"base64"`. An `encoding` whose `enum` or `const` does not admit `"base64"`
//! declares nothing.
//!
//! [`crate::Engine::encode_base64`] applies this to any 2xx body, whatever
//! made it (a model, the faker, state, a scenario): the protocols call it on
//! every answer they serve. Only a model's own answer must hold UTF-8 text in a
//! base64 `content` ([`Writer::Model`]); anything else that decodes is kept.

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
    let defs = doc.get("$defs").cloned().unwrap_or(Value::Null);
    hint_in(&mut doc, &defs);
    doc
}

/// `schema` through its `$ref`s into the prompt schema's `defs`.
fn local<'a>(defs: &'a Value, mut schema: &'a Value) -> &'a Value {
    for _ in 0..8 {
        match schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| defs.get(name))
        {
            Some(next) => schema = next,
            None => break,
        }
    }
    schema
}

fn hint_in(value: &mut Value, defs: &Value) {
    match value {
        Value::Object(obj) => {
            if declares_base64(obj) {
                obj.insert("description".into(), Value::from(PLAIN_TEXT_HINT));
            }
            let pair = obj
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|props| base64_pair(props, |s| local(defs, s)));
            if pair
                && let Some(Value::Object(props)) = obj.get_mut("properties")
                && let Some(Value::Object(content)) = props.get_mut("content")
            {
                let note = match content.get("description").and_then(Value::as_str) {
                    Some(d) => format!("{d} {PLAIN_TEXT_HINT}"),
                    None => PLAIN_TEXT_HINT.to_string(),
                };
                content.insert("description".into(), Value::from(note));
            }
            obj.values_mut().for_each(|v| hint_in(v, defs));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| hint_in(v, defs)),
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

fn says_base64(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|e| e.eq_ignore_ascii_case("base64"))
}

/// Whether `props` (an object schema's, `resolve` following a `$ref`) declare a string
/// `content` beside an `encoding` whose `enum` or `const` names `"base64"`: the pair whose
/// `content` [`hint`] asks for as plain text, so the server encodes it and its
/// `encoding` says so. A free-form `encoding` (GitHub's) may say it; one whose
/// `enum` or `const` does not admit `"base64"` declares no such pair.
fn base64_pair<'a>(
    props: &'a Map<String, Value>,
    resolve: impl Fn(&'a Value) -> &'a Value,
) -> bool {
    let (Some(content), Some(encoding)) = (props.get("content"), props.get("encoding")) else {
        return false;
    };
    if resolve(content).get("type").and_then(Value::as_str) != Some("string") {
        return false;
    }
    let encoding = resolve(encoding);
    match (encoding.get("enum"), encoding.get("const")) {
        (Some(Value::Array(values)), _) => values.iter().any(|v| says_base64(Some(v))),
        (_, Some(value)) => says_base64(Some(value)),
        _ => true,
    }
}

fn declares_base64_pair(bundle: &Bundle, schema: &Value) -> bool {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|props| base64_pair(props, |s| deref(bundle, s)))
}

/// Who wrote a body [`patch`] checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Writer {
    /// A model (or the faker): a base64 `content` must decode to UTF-8 text, else
    /// what decodes as readable text is kept.
    Model,
    /// Anything else (a client's request, state, a scenario): any value that decodes
    /// is kept as it is, so a client's binary file is served back unchanged.
    Other,
}

/// `body` (answered for `schema`, written by `writer`) with every base64 field that
/// does not decode replaced by the encoding of its text, and the `encoding` beside a
/// declared base64 `content` saying `"base64"`; `None` when nothing changed.
/// Nothing is copied unless something changes.
pub(crate) fn patch(
    bundle: &Bundle,
    schema: &Value,
    body: &Value,
    writer: Writer,
) -> Option<Value> {
    let mut edits = Vec::new();
    walk(
        bundle,
        Some(schema),
        body,
        (&mut String::new(), 0, writer),
        &mut edits,
    );
    if edits.is_empty() {
        return None;
    }
    let mut patched = body.clone();
    for (pointer, value) in edits {
        match patched.pointer_mut(&pointer) {
            Some(slot) => *slot = value,
            None => {
                // A member added beside its parent's others (`encoding`).
                let (parent, key) = pointer.rsplit_once('/')?;
                let key = key.replace("~1", "/").replace("~0", "~");
                patched
                    .pointer_mut(parent)?
                    .as_object_mut()?
                    .insert(key, value);
            }
        }
    }
    Some(patched)
}

fn push_key(pointer: &mut String, key: &str) {
    pointer.push('/');
    pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
}

/// Collects into `edits` the JSON pointers of `value` (at `pointer`) to change, with
/// their new values.
fn walk(
    bundle: &Bundle,
    schema: Option<&Value>,
    value: &Value,
    (pointer, depth, writer): (&mut String, usize, Writer),
    edits: &mut Vec<(String, Value)>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let schema = schema.map(|s| deref(bundle, s));
    let branches = schema.map(|s| branches(bundle, s)).unwrap_or_default();
    let len = pointer.len();
    match value {
        Value::String(s) => {
            let binary = branches
                .iter()
                .any(|b| b.as_object().is_some_and(declares_base64));
            if binary && let Some(fixed) = fix(s, true) {
                edits.push((pointer.clone(), Value::String(fixed)));
            }
        }
        Value::Array(items) => {
            let item = branches.iter().find_map(|b| b.get("items"));
            for (i, v) in items.iter().enumerate() {
                pointer.push('/');
                pointer.push_str(&i.to_string());
                walk(bundle, item, v, (pointer, depth + 1, writer), edits);
                pointer.truncate(len);
            }
        }
        Value::Object(obj) => {
            let said = says_base64(obj.get("encoding"));
            // A declared pair's `content` was asked for as plain text (see [`hint`]), so
            // the server encodes it whatever `encoding` the model wrote beside it.
            let declared = branches.iter().any(|b| declares_base64_pair(bundle, b));
            let content = obj.get("content").and_then(Value::as_str);
            if declared
                && !said
                && let Some(content) = content
                && !content.is_empty()
            {
                push_key(pointer, "encoding");
                edits.push((pointer.clone(), Value::from("base64")));
                pointer.truncate(len);
            }
            // An `encoding` that names another one (`"utf-8"`) says the content is text,
            // even where it looks like base64 (`README`).
            let text = declared && !said && obj.get("encoding").is_some_and(Value::is_string);
            for (key, v) in obj {
                if (said || declared)
                    && key == "content"
                    && let Value::String(s) = v
                {
                    let fixed = if text {
                        (!s.is_empty()).then(|| encode(s, s.contains('\n')))
                    } else {
                        fix(s, writer == Writer::Other)
                    };
                    if let Some(fixed) = fixed {
                        push_key(pointer, key);
                        edits.push((pointer.clone(), Value::String(fixed)));
                        pointer.truncate(len);
                    }
                    continue;
                }
                let member = branches
                    .iter()
                    .find_map(|b| b.get("properties").and_then(|p| p.get(key)));
                push_key(pointer, key);
                walk(bundle, member, v, (pointer, depth + 1, writer), edits);
                pointer.truncate(len);
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

    fn bundle() -> Bundle {
        mb_import::compile(
            br#"
openapi: 3.0.3
info: {title: t, version: "1"}
paths: {}
"#,
        )
        .unwrap()
    }

    #[test]
    fn a_declared_pair_is_encoded_whatever_its_encoding_says() {
        let bundle = bundle();
        let encoding = json!({"type": "string", "enum": ["base64", "utf-8", "none"]});
        let schema = json!({"type": "object", "properties": {
            "content": {"type": "string"},
            "encoding": encoding,
            "files": {"type": "array", "items": {"type": "object", "properties": {
                "content": {"type": "string"}, "encoding": encoding,
            }}},
        }});
        // `README` looks like base64, but `utf-8` says it is text.
        let body = json!({"content": "README", "encoding": "utf-8",
                          "files": [{"content": "a/b ~c"}, {"content": "", "encoding": "none"}]});
        let patched = patch(&bundle, &schema, &body, Writer::Model).unwrap();
        assert_eq!(patched["encoding"], "base64");
        assert_eq!(decoded(patched["content"].as_str().unwrap()), "README");
        // A missing `encoding` is added; empty content keeps what it says.
        assert_eq!(patched["files"][0]["encoding"], "base64");
        assert_eq!(
            decoded(patched["files"][0]["content"].as_str().unwrap()),
            "a/b ~c"
        );
        assert_eq!(
            patched["files"][1],
            json!({"content": "", "encoding": "none"})
        );
        // Valid base64 under `base64` is left alone, and nothing is copied.
        let ok = json!({"content": STANDARD.encode("hi"), "encoding": "base64"});
        assert_eq!(patch(&bundle, &schema, &ok, Writer::Model), None);
        // An `encoding` that cannot say base64 declares no base64 content.
        let text_only = json!({"type": "object", "properties": {
            "content": {"type": "string"},
            "encoding": {"type": "string", "enum": ["utf-8"]},
        }});
        let body = json!({"content": "# hi", "encoding": "utf-8"});
        assert_eq!(patch(&bundle, &text_only, &body, Writer::Model), None);
    }

    /// A free-form `encoding` (GitHub's) beside a string `content` is a base64
    /// pair: a model's `encoding: "utf-8"` text is encoded and the encoding set, and
    /// the prompt hints it as plain text.
    #[test]
    fn a_free_form_encoding_declares_a_base64_pair() {
        let bundle = bundle();
        let schema = json!({"type": "object", "properties": {
            "content": {"type": "string"}, "encoding": {"type": "string"},
        }});
        let text = json!({"content": "# hi", "encoding": "utf-8"});
        let patched = patch(&bundle, &schema, &text, Writer::Model).unwrap();
        assert_eq!(patched["encoding"], "base64");
        assert_eq!(decoded(patched["content"].as_str().unwrap()), "# hi");
        let doc = hint(schema);
        assert_eq!(doc["properties"]["content"]["description"], PLAIN_TEXT_HINT);
        // An enum without base64 is no pair, for the hint as for the server.
        let text_only = hint(json!({"type": "object", "properties": {
            "content": {"type": "string"},
            "encoding": {"type": "string", "enum": ["utf-8"]},
        }}));
        assert!(
            text_only["properties"]["content"]
                .get("description")
                .is_none()
        );
    }

    /// A model's base64 `content` must decode to text; anything else's (a client's
    /// PNG stored and read back) is kept whenever it decodes.
    #[test]
    fn only_a_model_s_content_must_be_text() {
        let bundle = bundle();
        let schema = json!({"type": "object", "properties": {
            "content": {"type": "string"}, "encoding": {"type": "string"},
        }});
        let png = STANDARD.encode([0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00]);
        let body = json!({"content": png, "encoding": "base64"});
        assert_eq!(patch(&bundle, &schema, &body, Writer::Other), None);
        assert!(patch(&bundle, &schema, &body, Writer::Model).is_some());
        // Text that does not decode is encoded whoever wrote it.
        let text = json!({"content": "# hi", "encoding": "base64"});
        assert!(patch(&bundle, &schema, &text, Writer::Other).is_some());
    }

    #[test]
    fn hint_marks_base64_fields() {
        let doc = hint(json!({
            "type": "object",
            "properties": {
                "content": {"type": "string"},
                "encoding": {"type": "string", "enum": ["base64", "utf-8"]},
                "blob": {"type": "string", "format": "byte"},
                "name": {"type": "string"},
            },
        }));
        assert_eq!(doc["properties"]["content"]["description"], PLAIN_TEXT_HINT);
        assert_eq!(doc["properties"]["blob"]["description"], PLAIN_TEXT_HINT);
        assert!(doc["properties"]["name"].get("description").is_none());
    }
}
