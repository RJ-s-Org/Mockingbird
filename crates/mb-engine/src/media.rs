//! REST responses in a media type other than JSON.
//!
//! A response the contract declares as JSON (or `+json`) is generated as
//! usual. Any other media type of an HTTP operation is answered without
//! the LLM, whose JSON-mode answer could not be that media type: from the
//! schema's first `examples` entry when it declares one, else faked for
//! `text/*`, else left empty (`application/octet-stream` and other binary
//! media are served only from a declared example). XML without an example
//! is generated as JSON like any other body; the data plane renders it.

use mb_mcm::{Binding, Operation, OutputVariant};
use serde_json::Value;

/// How a non-JSON response is answered.
#[derive(Debug, PartialEq)]
pub(crate) enum MediaAnswer<'a> {
    /// The declared example, served as written.
    Example(&'a Value),
    /// A faker value for this schema (a text media type).
    Fake(&'a Value),
    /// No body: a binary media type without an example.
    Empty,
}

/// How `op` answers `variant` (whose body schema is `schema`), when its
/// media type is not answered by JSON generation; `None` when it is.
pub(crate) fn answer<'a>(
    op: &Operation,
    variant: &OutputVariant,
    schema: &'a Value,
) -> Option<MediaAnswer<'a>> {
    if !matches!(op.binding, Binding::Http { .. }) {
        return None;
    }
    let base = base(variant.content_type.as_deref()?);
    if is_json(&base) || is_stream(&base) {
        return None;
    }
    if let Some(example) = schema
        .get("examples")
        .and_then(Value::as_array)
        .and_then(|e| e.first())
    {
        return Some(MediaAnswer::Example(example));
    }
    if is_xml(&base) {
        None
    } else if base.starts_with("text/") {
        Some(MediaAnswer::Fake(schema))
    } else {
        Some(MediaAnswer::Empty)
    }
}

/// Whether `variant` of `op` is an HTTP response in a non-JSON media type.
pub(crate) fn is_non_json(op: &Operation, variant: &OutputVariant) -> bool {
    matches!(op.binding, Binding::Http { .. })
        && variant
            .content_type
            .as_deref()
            .map(base)
            .is_some_and(|b| !is_json(&b))
}

/// The lowercased media type without parameters.
fn base(ct: &str) -> String {
    ct.split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase()
}

fn is_json(base: &str) -> bool {
    base == "application/json" || base.ends_with("+json")
}

fn is_xml(base: &str) -> bool {
    base == "application/xml" || base == "text/xml" || base.ends_with("+xml")
}

/// Streamed media, answered by the stream path.
fn is_stream(base: &str) -> bool {
    matches!(
        base,
        "text/event-stream" | "application/x-ndjson" | "application/jsonl"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types_are_classified() {
        assert!(is_json(&base("application/problem+json; charset=utf-8")));
        assert!(is_xml(&base("Application/Atom+XML")));
        assert!(is_xml("text/xml"));
        assert!(is_stream(&base("text/event-stream")));
        assert!(!is_json("text/plain"));
    }
}
