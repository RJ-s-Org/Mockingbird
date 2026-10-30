//! Close a JSON answer cut short: the output stopped at `max_tokens`, or
//! ended inside the value.

use serde_json::Value;

/// How many `{` / `[` positions [`close_truncated`] tries.
const MAX_STARTS: usize = 8;

/// The JSON object or array that `text` starts (after any prose or Markdown
/// fence) and that its end cuts short, closed: the trailing member or
/// element that is incomplete is dropped (a key without its value, a cut
/// number or literal), a cut string value is closed where it stops, and the
/// open arrays and objects are closed. `None` when the text holds no value
/// cut short, or when nothing of it is complete (an empty root).
///
/// Only the outermost value is closed: a value nested in it is never
/// returned on its own (see `extract::first_value`).
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
            Scan::Ended(end) => next = start + end.max(1),
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
    /// The value, cut short and closed (`None` from [`close`] when it
    /// could not be).
    Cut(Value),
    /// The value ended (closed, or broken) at this byte offset.
    Ended(usize),
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
                    return Scan::Ended(i);
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
                    _ => return Scan::Ended(i),
                }
                i += 1;
                if stack.is_empty() {
                    return Scan::Ended(i);
                }
                cut = (i, stack.len());
            }
            b':' => match stack.last_mut() {
                Some(Frame::Object { value }) if !*value => {
                    *value = true;
                    i += 1;
                }
                _ => return Scan::Ended(i),
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
                None if in_key => break,
                // Cut inside a string value: it ends where it stops.
                None => {
                    let closed = format!("{}\"", without_partial_escape(text));
                    return finish(&closed, &stack);
                }
            },
            _ => {
                let end = bytes[i..]
                    .iter()
                    .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')))
                    .map_or(bytes.len(), |n| i + n);
                if end == i || in_key {
                    return Scan::Ended(i);
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
        return Scan::Ended(bytes.len());
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
        _ => Scan::Ended(text.len()),
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

/// `text`, which ends inside a string, without an escape sequence the cut
/// left incomplete (`\` or `\u12`).
fn without_partial_escape(text: &str) -> &str {
    let bytes = text.as_bytes();
    let backslashes = bytes.iter().rev().take_while(|&&b| b == b'\\').count();
    if backslashes % 2 == 1 {
        return &text[..text.len() - 1];
    }
    // `\u` and up to three hex digits.
    let hex = bytes
        .iter()
        .rev()
        .take(3)
        .take_while(|b| b.is_ascii_hexdigit())
        .count();
    let before = bytes.len() - hex;
    if before >= 2 && &bytes[before - 2..before] == b"\\u" {
        let escaped = bytes[..before - 2]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count();
        if escaped % 2 == 0 {
            return &text[..before - 2];
        }
    }
    text
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

    #[test]
    fn a_cut_string_value_is_closed_where_it_stops() {
        assert_eq!(
            close_truncated(r#"{"id": 1, "bio": "Writes about bir"#),
            Some(json!({"id": 1, "bio": "Writes about bir"}))
        );
        assert_eq!(
            close_truncated(r#"{"id": 1, "bio": "a \"quote\" and \"#),
            Some(json!({"id": 1, "bio": "a \"quote\" and "}))
        );
        assert_eq!(
            close_truncated(r#"{"id": 1, "bio": "caf\u00"#),
            Some(json!({"id": 1, "bio": "caf"}))
        );
    }

    #[test]
    fn a_cut_items_envelope_is_closed_as_the_envelope() {
        assert_eq!(
            close_truncated(r#"{"items": [{"id": 1}, {"id": 2, "name": "b"#),
            Some(json!({"items": [{"id": 1}, {"id": 2, "name": "b"}]}))
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
    }
}
