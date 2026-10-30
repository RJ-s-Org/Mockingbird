//! Close a JSON answer the provider cut short at `max_tokens`.

use serde_json::Value;

/// How many `{` / `[` positions [`close_truncated`] tries.
const MAX_STARTS: usize = 8;

/// The JSON object or array that `text` starts (after any prose or Markdown
/// fence) and that its end cuts short, closed: the trailing member or
/// element that is incomplete is dropped (a key without its value, a cut
/// string, number or literal: `"cust-0"` may be `"cust-0042"`), and the
/// open arrays and objects are closed. `None` when the text holds no value
/// cut short, or when nothing of it is complete (an empty root).
///
/// Only the outermost value is closed: a value nested in it is never
/// returned on its own. A candidate that breaks before the end of the text
/// (a comment, an unquoted key, a mismatched bracket) ends the search, as
/// in `extract::first_value`: the `{` after the break is inside it.
pub(crate) fn close_truncated(text: &str) -> Option<Value> {
    let mut next = 0;
    for (tries, (start, _)) in text.match_indices(['{', '[']).enumerate() {
        if tries >= MAX_STARTS {
            break;
        }
        if start < next {
            continue;
        }
        match close(&text[start..]) {
            Scan::Cut(value) => return Some(value),
            // Brackets in prose (`[draft]`): look after them.
            Scan::Closed(end) => next = start + end,
            Scan::Broken => return None,
        }
    }
    None
}

enum Frame {
    /// An object; `value` once its member's `:` was read.
    Object {
        value: bool,
    },
    Array,
}

enum Scan {
    /// The value, cut short and closed.
    Cut(Value),
    /// The value's brackets closed at this byte offset: it is not cut.
    Closed(usize),
    /// Not JSON before the end of the text, or nothing to salvage.
    Broken,
}

/// Scans the value `text` starts with (`{` or `[`), keeping the last point
/// a complete member or element ends: the text up to it, with the
/// containers still open there closed, is the salvage. A container cut
/// before any complete member is dropped with the member holding it.
fn close(text: &str) -> Scan {
    let bytes = text.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    // (end, open containers there)
    let mut cut = (0, 0);
    let mut i = 0;
    while i < bytes.len() {
        let in_key = matches!(stack.last(), Some(Frame::Object { value: false }));
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b @ (b'{' | b'[') => {
                if in_key {
                    return Scan::Broken;
                }
                stack.push(if b == b'{' {
                    Frame::Object { value: false }
                } else {
                    Frame::Array
                });
                i += 1;
            }
            b @ (b'}' | b']') => {
                match (stack.pop(), b) {
                    (Some(Frame::Object { .. }), b'}') | (Some(Frame::Array), b']') => {}
                    _ => return Scan::Broken,
                }
                i += 1;
                if stack.is_empty() {
                    return Scan::Closed(i);
                }
                cut = (i, stack.len());
            }
            b':' => match stack.last_mut() {
                Some(Frame::Object { value }) if !*value => {
                    *value = true;
                    i += 1;
                }
                _ => return Scan::Broken,
            },
            b',' => {
                if let Some(Frame::Object { value }) = stack.last_mut() {
                    *value = false;
                }
                i += 1;
            }
            b'"' => match string_end(bytes, i + 1) {
                Some(end) => {
                    i = end;
                    if !in_key {
                        cut = (i, stack.len());
                    }
                }
                // Cut inside a key or a string value: dropped.
                None => break,
            },
            _ => {
                let end = bytes[i..]
                    .iter()
                    .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')))
                    .map_or(bytes.len(), |n| i + n);
                if end == i || in_key {
                    return Scan::Broken;
                }
                if end == bytes.len() {
                    // Cut inside a number or literal: `12` may be `1234`.
                    break;
                }
                i = end;
                cut = (i, stack.len());
            }
        }
    }
    let (end, depth) = cut;
    if depth == 0 {
        return Scan::Broken;
    }
    finish(&text[..end], &stack[..depth])
}

/// `text` with `open` closed, parsed; a closed root that is empty is not
/// a salvage.
fn finish(text: &str, open: &[Frame]) -> Scan {
    let mut closed = text.trim_end().to_string();
    for frame in open.iter().rev() {
        closed.push(match frame {
            Frame::Object { .. } => '}',
            Frame::Array => ']',
        });
    }
    match serde_json::from_str::<Value>(&closed) {
        Ok(value) if !is_empty(&value) => Scan::Cut(value),
        _ => Scan::Broken,
    }
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => true,
    }
}

/// The offset just past the `"` closing the string whose content starts at
/// `from`; `None` when the text ends first.
fn string_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::close_truncated;
    use serde_json::json;

    #[test]
    fn a_cut_nested_object_drops_its_incomplete_member() {
        assert_eq!(
            close_truncated(r#"{"id": 1, "owner": {"name": "a", "email""#),
            Some(json!({"id": 1, "owner": {"name": "a"}}))
        );
        assert_eq!(
            close_truncated(r#"{"id": 1, "owner": {"name": "a", "age": 4"#),
            Some(json!({"id": 1, "owner": {"name": "a"}}))
        );
        assert_eq!(
            close_truncated(r#"{"id": 1, "owner": {"na"#),
            Some(json!({"id": 1}))
        );
    }

    #[test]
    fn a_cut_array_keeps_its_complete_elements() {
        assert_eq!(
            close_truncated(r#"[{"id": 1}, {"id": 2}, {"id""#),
            Some(json!([{"id": 1}, {"id": 2}]))
        );
        assert_eq!(close_truncated("[1, 2, 3"), Some(json!([1, 2])));
        assert_eq!(close_truncated("[1, 2, "), Some(json!([1, 2])));
    }

    /// A cut string is dropped like a cut number: a shortened id or URL
    /// is worse than none (a required one is refilled).
    #[test]
    fn a_cut_string_value_is_dropped() {
        assert_eq!(
            close_truncated(r#"{"id": 1, "ref": "7f9c2ba4-e88f"#),
            Some(json!({"id": 1}))
        );
        assert_eq!(
            close_truncated(r#"{"id": 1, "bio": "a \"quote\" and \"#),
            Some(json!({"id": 1}))
        );
        // Half a surrogate pair goes with it.
        assert_eq!(
            close_truncated(r#"{"id": 1, "note": "hi \uD83D\uDE"#),
            Some(json!({"id": 1}))
        );
    }

    #[test]
    fn a_cut_items_envelope_is_closed_as_the_envelope() {
        assert_eq!(
            close_truncated(r#"{"items": [{"id": 1}, {"id": 2, "name": "b"#),
            Some(json!({"items": [{"id": 1}, {"id": 2}]}))
        );
    }

    #[test]
    fn prose_and_fences_before_the_value_are_skipped() {
        assert_eq!(
            close_truncated("Sure [draft]:\n```json\n{\"a\": [1, 2"),
            Some(json!({"a": [1]}))
        );
    }

    #[test]
    fn complete_broken_or_empty_values_are_not_salvaged() {
        assert_eq!(close_truncated(r#"{"a": 1}"#), None);
        assert_eq!(close_truncated(r#"{"a": 1,}"#), None);
        assert_eq!(close_truncated(r#"{"a": {"b": 1}, // note"#), None);
        assert_eq!(close_truncated(r#"{"a"#), None);
        assert_eq!(close_truncated("no json"), None);
        // A value nested in one that broke is never a root.
        assert_eq!(
            close_truncated("{\"a\": 1, // c\n \"b\": {\"c\": 2, \"d\": {\"e"),
            None
        );
        assert_eq!(close_truncated(r#"{a: 1, "b": {"c": 2, "d": [1, 2"#), None);
        assert_eq!(close_truncated(r#"{"a": [1}, "b": {"c": 2, "d": [1"#), None);
    }
}
