//! The response header a create's answer names its new record by, found from the
//! contract alone (no world description): the detector tier 0 restates as
//! `answer.identity_header`, and the runtime's fallback where the description is
//! silent.

use serde_json::Value;

use crate::Operation;

/// How a response header names the record a create made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityHeaderKind {
    /// The id itself: a header whose last word is `id` (`Resource-ID`, `X-Item-Id`,
    /// `item_id`).
    Id,
    /// `Location` (RFC 9110): the new record's URL.
    Location,
}

/// The header of `op`'s answer of `status` that names a created record: its name as the
/// contract declares it, how it names the record, and its schema.
#[derive(Clone, Debug, PartialEq)]
pub struct IdentityHeader {
    /// The header's name, as declared.
    pub name: String,
    /// How it names the record.
    pub kind: IdentityHeaderKind,
    /// The header's declared schema.
    pub schema: Value,
}

/// Words that make an id header the call's own, not a record's: a request's trace or
/// correlation id (`X-Request-Id`, `X-Trace-Id`, `X-Correlation-ID`).
const CALL_ID_WORDS: [&str; 4] = ["request", "trace", "correlation", "span"];

/// Words before `id` that name no kind of record, so the id is the created record's
/// own: `Resource-ID`, `X-Object-Id`, `Entity-Id`.
const RECORD_ID_WORDS: [&str; 5] = ["resource", "object", "entity", "record", "item"];

/// Whether the words of an id header before its `id` (a leading `X` aside) name the
/// record `op` creates: none, or only words naming no kind ([`RECORD_ID_WORDS`]), or
/// the created type's own name, its last words, plural aside (`X-Job-Id` for `jobs`,
/// `Line-Id` for `ticket_lines`). `X-Account-Id` on a job's create names another kind.
fn names_created(op: &Operation, words: &[&str]) -> bool {
    let Some((_, before)) = words.split_last() else {
        return false;
    };
    let before = match before.split_first() {
        Some((x, rest)) if x.eq_ignore_ascii_case("x") => rest,
        _ => before,
    };
    if before
        .iter()
        .all(|w| RECORD_ID_WORDS.iter().any(|g| w.eq_ignore_ascii_case(g)))
    {
        return true;
    }
    let named = crate::stem(&crate::joined_name(&before.concat()));
    let Some(t) = op.resource_type.as_deref() else {
        return false;
    };
    let type_words = crate::words(t);
    (0..type_words.len())
        .any(|from| crate::stem(&crate::joined_name(&type_words[from..].concat())) == named)
}

/// The one header `op` declares for `status` (its exact variant, else `default`) that
/// names a created record (see [`IdentityHeaderKind`]): an id header of a string or
/// integer schema (no `enum`), or a `Location` of a string schema. A header naming the
/// call, not a record (`X-Request-Id`), or another kind than the created one
/// (`X-Account-Id` on a job's create), is none. With two id headers, or none, `None`;
/// an id header wins over `Location`.
#[must_use]
pub fn identity_header(op: &Operation, status: u16) -> Option<IdentityHeader> {
    let variant = op.output_for_status(status)?;
    let of_type = |schema: &Value, types: &[&str]| {
        schema
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| types.contains(&t))
            && schema.get("enum").is_none()
            && schema.get("const").is_none()
    };
    let mut ids = variant.headers.iter().filter(|(name, schema)| {
        let words = crate::words(name);
        let last_is_id = words.last().is_some_and(|w| w.eq_ignore_ascii_case("id"));
        let names_the_call = words
            .iter()
            .any(|w| CALL_ID_WORDS.iter().any(|c| w.eq_ignore_ascii_case(c)));
        words.len() > 1
            && last_is_id
            && !names_the_call
            && names_created(op, &words)
            && of_type(schema, &["string", "integer"])
    });
    if let Some((name, schema)) = ids.next() {
        return ids.next().is_none().then(|| IdentityHeader {
            name: name.clone(),
            kind: IdentityHeaderKind::Id,
            schema: schema.clone(),
        });
    }
    variant
        .headers
        .iter()
        .find(|(name, schema)| {
            name.eq_ignore_ascii_case("location") && of_type(schema, &["string"])
        })
        .map(|(name, schema)| IdentityHeader {
            name: name.clone(),
            kind: IdentityHeaderKind::Location,
            schema: schema.clone(),
        })
}
