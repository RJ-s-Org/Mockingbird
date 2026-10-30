//! Recover a JSON value from LLM text that may be wrapped in prose or
//! Markdown fences.

use serde_json::Value;

/// How many `{` / `[` positions [`first_value`] tries before giving up, so
/// a long answer of prose full of brackets stays linear.
const MAX_STARTS: usize = 32;

/// Tries, in order: the whole trimmed text; the contents of the first
/// Markdown code fence; the first complete JSON object or array in the
/// text (see [`first_value`]). Text after that value is ignored: a second
/// object (`{..}{..}`, which some models write) or prose that contains
/// braces.
pub fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    parse(trimmed)
        .or_else(|| fenced(trimmed).and_then(|body| parse(body).or_else(|| first_value(body))))
        .or_else(|| first_value(trimmed))
}

fn parse(s: &str) -> Option<Value> {
    serde_json::from_str(s.trim()).ok()
}

/// Contents of the first ```` ``` ```` fence, skipping an info string such
/// as `json` on the opening line.
fn fenced(text: &str) -> Option<&str> {
    let start = text.find("```")? + 3;
    let rest = &text[start..];
    let body_start = rest.find('\n').map_or(rest.len(), |i| i + 1);
    let body = &rest[body_start..];
    let end = body.find("```")?;
    Some(&body[..end])
}

/// The first complete JSON object or array, starting at a `{` or `[`: the
/// first such position whose text parses as a value, whatever follows it.
///
/// A position inside a candidate that failed is never tried: a value
/// nested in a broken answer (cut short, or with a trailing comma or a
/// comment) is a fragment, not the answer. A candidate cut off by the end
/// of the text ends the search, so a truncated answer is `None` and goes to
/// repair. An empty `{}` or `[]` (prose such as "left empty ([])") is
/// returned only when no non-empty value follows it.
fn first_value(text: &str) -> Option<Value> {
    let mut empty = None;
    let mut next = 0;
    let mut tries = 0;
    for (start, _) in text.match_indices(['{', '[']) {
        if start < next {
            continue;
        }
        tries += 1;
        if tries > MAX_STARTS {
            break;
        }
        let rest = &text[start..];
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        match values.next() {
            Some(Ok(value)) if is_empty(&value) => {
                next = start + values.byte_offset();
                empty.get_or_insert(value);
            }
            Some(Ok(value)) => return Some(value),
            Some(Err(e)) if e.is_eof() => return None,
            Some(Err(e)) => next = start + error_offset(rest, &e).max(1),
            None => return None,
        }
    }
    empty
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// The byte offset in `text` of the position `e` reports (1-based line and
/// column).
fn error_offset(text: &str, e: &serde_json::Error) -> usize {
    let line_start: usize = text
        .split_inclusive('\n')
        .take(e.line().saturating_sub(1))
        .map(str::len)
        .sum();
    (line_start + e.column().saturating_sub(1)).min(text.len())
}

#[cfg(test)]
mod tests {
    use super::extract_json;
    use serde_json::json;

    #[test]
    fn doubled_objects_give_the_first() {
        assert_eq!(
            extract_json(r#"{"id": 1, "name": "a"}{"id": 2, "name": "b"}"#),
            Some(json!({"id": 1, "name": "a"}))
        );
        assert_eq!(
            extract_json("{\"id\": 1}\n\n{\"id\": 2}"),
            Some(json!({"id": 1}))
        );
    }

    #[test]
    fn trailing_prose_with_braces_is_ignored() {
        assert_eq!(
            extract_json(r#"Sure: {"a": {"b": 2}} Note: fields like {id} are made up."#),
            Some(json!({"a": {"b": 2}}))
        );
    }

    #[test]
    fn leading_brackets_that_are_not_json_are_skipped() {
        assert_eq!(
            extract_json(r#"[draft] {"a": 1} [end]"#),
            Some(json!({"a": 1}))
        );
    }

    #[test]
    fn fenced_block_followed_by_prose() {
        let text = "```json\n{\"a\": 1}\n```\nI added {braces} and [brackets] here.";
        assert_eq!(extract_json(text), Some(json!({"a": 1})));
        let doubled = "```json\n{\"a\": 1}\n{\"a\": 2}\n```";
        assert_eq!(extract_json(doubled), Some(json!({"a": 1})));
    }

    #[test]
    fn array_root() {
        assert_eq!(
            extract_json(r#"Here you go: [{"id": 1}, {"id": 2}] (two items)"#),
            Some(json!([{"id": 1}, {"id": 2}]))
        );
        assert_eq!(extract_json("[1, 2][3]"), Some(json!([1, 2])));
    }

    /// A truncated answer is not replaced by a value nested in it.
    #[test]
    fn truncated_answers_are_none_not_their_inner_values() {
        let nested = r#"{"user": {"id":1,"name":"a"}, "orders": [{"id":9}, {"id":"#;
        assert_eq!(extract_json(nested), None);
        assert_eq!(extract_json(r#"[{"id": 1}, {"id": 2}, {"id""#), None);
        assert_eq!(extract_json(r#"{"items": [{"id": 1}, {"id": 2"#), None);
        assert_eq!(extract_json(r#"Here: {"a": {"b": 1}, "c": [1, 2"#), None);
    }

    /// Nor is a broken one: a trailing comma, a comment or `NaN`.
    #[test]
    fn broken_answers_are_none_not_their_inner_values() {
        assert_eq!(extract_json(r#"{"a": {"b": 1}, "c": [{"d": 2}],}"#), None);
        assert_eq!(
            extract_json(
                r#"{"a": {"b": 1}, // note
"c": 2}"#
            ),
            None
        );
        assert_eq!(extract_json(r#"{"a": {"b": 1}, "c": NaN}"#), None);
        // A value after the broken one still counts.
        assert_eq!(
            extract_json(r#"{"a": 1,} oops, again: {"a": 1}"#),
            Some(json!({"a": 1}))
        );
    }

    #[test]
    fn empty_values_in_prose_lose_to_a_later_answer() {
        assert_eq!(
            extract_json(r#"Fields left empty ([]) as requested: [{"id":1}]"#),
            Some(json!([{"id": 1}]))
        );
        assert_eq!(
            extract_json(r#"No filter {} applied. {"id": 1}"#),
            Some(json!({"id": 1}))
        );
        assert_eq!(extract_json("The answer: [] (none)."), Some(json!([])));
    }

    #[test]
    fn incomplete_json_is_none() {
        assert_eq!(extract_json(r#"{"a": [1, 2"#), None);
        assert_eq!(extract_json("no json here"), None);
    }
}
