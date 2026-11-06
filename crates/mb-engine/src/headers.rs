//! Values for the response headers a contract declares: rate-limit and
//! retry headers get numbers that agree with each other and with the
//! status, ids get random tokens, anything else a faker value of its schema.
//! [`identity_header`] finds the header a create's answer names its new record by.

pub use mb_mcm::identity_headers::{IdentityHeader, IdentityHeaderKind, identity_header};
use mb_mcm::{Bundle, Operation};
use rand::{Rng, RngExt};
use serde_json::Value;

use crate::Faker;

/// Requests per window the rate-limit headers report.
const LIMITS: [u64; 4] = [60, 100, 1000, 5000];

/// The headers `op` declares for `status` (its exact variant, else
/// `default`), each with a value.
/// The values are random; [`crate::Engine::declared_headers`] draws them
/// from the engine's faker seed when it has one.
pub fn declared_headers(bundle: &Bundle, op: &Operation, status: u16) -> Vec<(String, String)> {
    declared_headers_with(bundle, op, status, &mut rand::rng())
}

/// [`declared_headers`], drawing the values from `rng` (a seeded generator
/// repeats them).
pub fn declared_headers_with<R: Rng>(
    bundle: &Bundle,
    op: &Operation,
    status: u16,
    rng: &mut R,
) -> Vec<(String, String)> {
    let Some(variant) = op.output_for_status(status) else {
        return Vec::new();
    };
    if variant.headers.is_empty() {
        return Vec::new();
    }
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
            let value = header_value(bundle, name, schema, status, rate_limit, rng);
            (name.clone(), value)
        })
        .collect()
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
            Resource-Id: {schema: {type: string}}
  /jobs:
    post:
      operationId: job
      responses:
        "202":
          description: ok
          headers:
            X-Account-Id: {schema: {type: string}}
  /exports:
    post:
      operationId: export
      responses:
        "202":
          description: ok
          headers:
            X-Account-Id: {schema: {type: string}}
            X-Export-Id: {schema: {type: string}}
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
        // An id of another kind than the created one is not its id.
        let accepted = |op: &str| identity_header(b.operation(op).unwrap(), 202).map(|h| h.name);
        assert_eq!(accepted("job"), None);
        assert_eq!(accepted("export"), Some("X-Export-Id".into()));
    }
}
