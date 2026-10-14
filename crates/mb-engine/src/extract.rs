//! Recover a JSON value from LLM text that may be wrapped in prose or
//! Markdown fences.

use serde_json::Value;

/// Tries, in order: the whole trimmed text; the contents of the first
/// Markdown code fence; the substring from the first `{` or `[` to the last
/// matching `}` or `]`.
pub fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    parse(trimmed)
        .or_else(|| fenced(trimmed).and_then(parse))
        .or_else(|| bracketed(trimmed).and_then(parse))
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

fn bracketed(text: &str) -> Option<&str> {
    let start = text.find(['{', '['])?;
    let close = if text[start..].starts_with('{') {
        '}'
    } else {
        ']'
    };
    let end = text.rfind(close)?;
    (end > start).then(|| &text[start..=end])
}
