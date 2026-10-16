//! Credential redaction shared by the HAR and Postman importers.

use serde_json::Value;

/// What a redacted value is replaced with.
pub const REDACTED: &str = "<redacted>";

const SENSITIVE: [&str; 11] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "authorization",
    "cookie",
    "session",
    "credential",
];

/// Whether a key, header or parameter name may hold a credential
/// (case-insensitive substring match).
pub fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE.iter().any(|s| lower.contains(s))
}

/// Replaces, recursively, the value under every sensitive object key with
/// [`REDACTED`].
pub fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if is_sensitive_name(key) {
                    *v = Value::String(REDACTED.to_string());
                } else {
                    redact(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// What a query value of a redacted URL is replaced with.
pub const REDACTED_QUERY_VALUE: &str = "REDACTED";

/// `url` as it may be stored, shown or audited: every query value replaced
/// with [`REDACTED_QUERY_VALUE`] (the keys kept) and the fragment dropped,
/// so a signed URL's secrets stay out of job records and drafts.
pub fn redacted_url(url: &url::Url) -> String {
    let mut out = url.clone();
    out.set_fragment(None);
    if url.query().is_some() {
        let keys: Vec<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
        out.query_pairs_mut()
            .clear()
            .extend_pairs(keys.iter().map(|k| (k.as_str(), REDACTED_QUERY_VALUE)));
    }
    out.to_string()
}
