//! Values for the response headers a contract declares: rate-limit and
//! retry headers get numbers that agree with each other and with the
//! status, ids get random tokens, anything else a faker value of its schema.
//! [`identity_header`] finds the header a create's answer names its new record by.

use mb_mcm::{Bundle, Operation};
use rand::{Rng, RngExt};
use serde_json::Value;

use crate::Faker;

/// Requests per window the rate-limit headers report.
const LIMITS: [u64; 4] = [60, 100, 1000, 5000];

/// The headers `op` declares for `status` (its exact variant, else
/// `default`), each with a value.
pub fn declared_headers(bundle: &Bundle, op: &Operation, status: u16) -> Vec<(String, String)> {
    let Some(variant) = op.output_for_status(status) else {
        return Vec::new();
    };
    if variant.headers.is_empty() {
        return Vec::new();
    }
    let mut rng = rand::rng();
    let limit = LIMITS[rng.random_range(0..LIMITS.len())];
    let remaining = if status == 429 {
        0
    } else {
        rng.random_range(limit / 2..limit)
    };
    let window = rng.random_range(1..=60u64);
    let rate_limit = RateLimit {
        limit,
        remaining,
        window,
    };
    variant
        .headers
        .iter()
        .map(|(name, schema)| {
            let value = header_value(bundle, name, schema, status, rate_limit, &mut rng);
            (name.clone(), value)
        })
        .collect()
}

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

/// The one header `op` declares for `status` (its exact variant, else `default`) that
/// names a created record (see [`IdentityHeaderKind`]): an id header of a string or
/// integer schema (no `enum`), or a `Location` of a string schema. A header naming the
/// call, not a record (`X-Request-Id`), is none. With two id headers, or none, `None`;
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
        let words = mb_mcm::words(name);
        let last_is_id = words.last().is_some_and(|w| w.eq_ignore_ascii_case("id"));
        let names_the_call = words
            .iter()
            .any(|w| CALL_ID_WORDS.iter().any(|c| w.eq_ignore_ascii_case(c)));
        words.len() > 1 && last_is_id && !names_the_call && of_type(schema, &["string", "integer"])
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

/// The rate-limit numbers every header of one response agrees on.
#[derive(Clone, Copy)]
struct RateLimit {
    limit: u64,
    remaining: u64,
    /// Seconds until the window resets.
    window: u64,
}

fn header_value(
    bundle: &Bundle,
    name: &str,
    schema: &Value,
    status: u16,
    rate_limit: RateLimit,
    rng: &mut impl Rng,
) -> String {
    let RateLimit {
        limit,
        remaining,
        window,
    } = rate_limit;
    let lower = name.to_ascii_lowercase();
    let rate = lower.contains("ratelimit") || lower.contains("rate-limit");
    if lower == "retry-after" {
        return if status == 429 || status >= 500 {
            window.to_string()
        } else {
            "1".into()
        };
    }
    if rate && lower.ends_with("remaining") {
        return remaining.to_string();
    }
    if rate && lower.ends_with("limit") {
        return limit.to_string();
    }
    if rate && lower.ends_with("used") {
        return (limit - remaining).to_string();
    }
    if rate && lower.ends_with("reset") {
        // GitHub-style `X-RateLimit-Reset` is a Unix time; the IETF
        // `RateLimit-Reset` counts seconds.
        return if lower.starts_with("x-") {
            (chrono::Utc::now().timestamp().unsigned_abs() + window).to_string()
        } else {
            window.to_string()
        };
    }
    let declared_string = schema.get("type").and_then(Value::as_str) == Some("string")
        && schema.get("enum").is_none()
        && schema.get("format").is_none()
        && schema.get("pattern").is_none();
    if declared_string && (lower.ends_with("-id") || lower.ends_with("_id")) {
        return token(rng, 22);
    }
    match Faker::default().fake(&bundle.with_defs(schema), rng) {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `len` random alphanumeric characters.
fn token(rng: &mut impl Rng, len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..len)
        .map(|_| char::from(CHARS[rng.random_range(0..CHARS.len())]))
        .collect()
}

#[cfg(test)]
mod tests {

    use super::*;

    fn bundle() -> Bundle {
        mb_import::compile(
            br##"
openapi: 3.0.3
info: {title: t, version: "1"}
paths:
  /mail/send:
    post:
      operationId: send
      responses:
        "202":
          description: ok
          headers:
            X-Message-Id: {schema: {type: string}}
            X-RateLimit-Limit: {schema: {type: integer}}
            X-RateLimit-Remaining: {schema: {type: integer}}
            X-RateLimit-Reset: {schema: {type: integer}}
        "429":
          description: slow down
          headers:
            Retry-After: {schema: {type: integer}}
            X-RateLimit-Remaining: {schema: {type: integer}}
            X-Trace: {schema: {type: string, enum: [abc]}}
"##,
        )
        .unwrap()
    }

    fn get<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
        &headers.iter().find(|(n, _)| n == name).unwrap().1
    }

    #[test]
    fn rate_limit_headers_agree_and_ids_are_tokens() {
        let b = bundle();
        let op = b.operation("send").unwrap();
        let ok = declared_headers(&b, op, 202);
        assert_eq!(ok.len(), 4);
        assert_eq!(get(&ok, "X-Message-Id").len(), 22);
        let limit: u64 = get(&ok, "X-RateLimit-Limit").parse().unwrap();
        let remaining: u64 = get(&ok, "X-RateLimit-Remaining").parse().unwrap();
        assert!(remaining < limit);
        let reset: i64 = get(&ok, "X-RateLimit-Reset").parse().unwrap();
        assert!(reset > chrono::Utc::now().timestamp());

        let limited = declared_headers(&b, op, 429);
        let retry: u64 = get(&limited, "Retry-After").parse().unwrap();
        assert!((1..=60).contains(&retry));
        assert_eq!(get(&limited, "X-RateLimit-Remaining"), "0");
        assert_eq!(get(&limited, "X-Trace"), "abc");
        assert!(declared_headers(&b, op, 500).is_empty());
    }

    #[test]
    fn a_created_records_header_is_its_id_header_else_its_location() {
        let b = mb_import::compile(
            br##"
openapi: 3.0.3
info: {title: t, version: "1"}
paths:
  /a:
    post:
      operationId: a
      responses:
        "201":
          description: ok
          headers:
            X-Request-Id: {schema: {type: string}}
            Location: {schema: {type: string}}
            Resource-ID: {schema: {type: integer}}
  /b:
    post:
      operationId: b
      responses:
        "201":
          description: ok
          headers:
            X-Request-Id: {schema: {type: string}}
            Location: {schema: {type: string}}
  /c:
    post:
      operationId: c
      responses:
        "201":
          description: ok
          headers:
            X-Trace-Id: {schema: {type: string}}
            Id: {schema: {type: string}}
            item_id: {schema: {type: string, enum: [x]}}
  /d:
    post:
      operationId: d
      responses:
        "201":
          description: ok
          headers:
            ItemId: {schema: {type: string}}
            Other-Id: {schema: {type: string}}
"##,
        )
        .unwrap();
        let found =
            |op: &str| identity_header(b.operation(op).unwrap(), 201).map(|h| (h.name, h.kind));
        assert_eq!(
            found("a"),
            Some(("Resource-ID".into(), IdentityHeaderKind::Id))
        );
        assert_eq!(
            found("b"),
            Some(("Location".into(), IdentityHeaderKind::Location))
        );
        // A trace id, a bare `Id`, and an enum are no record's id.
        assert_eq!(found("c"), None);
        // Two id headers: which is the record's is not known.
        assert_eq!(found("d"), None);
        assert_eq!(identity_header(b.operation("a").unwrap(), 200), None);
    }
}
