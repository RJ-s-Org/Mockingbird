//! Recover a JSON value from LLM text that may be wrapped in prose or
//! Markdown fences.

use serde_json::Value;

/// How many `{` / `[` positions [`first_value`] tries before giving up, so
/// a long answer of prose full of brackets stays linear.
const MAX_STARTS: usize = 32;

/// Tries, in order: the whole trimmed text; the contents of the first
/// Markdown code fence; the first complete JSON object or array in the
/// text. Text after that value is ignored: a second object (`{..}{..}`,
/// which some models write) or prose that contains braces.
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
fn first_value(text: &str) -> Option<Value> {
    text.match_indices(['{', '['])
        .take(MAX_STARTS)
        .find_map(|(start, _)| {
            serde_json::Deserializer::from_str(&text[start..])
                .into_iter::<Value>()
                .next()
                .and_then(Result::ok)
        })
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

    #[test]
    fn incomplete_json_is_none() {
        assert_eq!(extract_json(r#"{"a": [1, 2"#), None);
        assert_eq!(extract_json("no json here"), None);
    }
}
