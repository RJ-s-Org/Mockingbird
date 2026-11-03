//! Whether a name holds a credential: the last resort, when a contract marks
//! nothing (`writeOnly`, `format: password`). No list of names can be complete.
//! Shared by what keeps bodies and what records requests.

/// Names holding a credential wherever they appear, once normalized (see
/// [`normalize_into`]): `privatekey` is `privateKey`, `PRIVATE_KEY` and
/// `private-key`.
const ANYWHERE: [&str; 14] = [
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "authorization",
    "cookie",
    "session",
    "credential",
    "privatekey",
    "sshkey",
    "signingkey",
    "cvv",
    "bearer",
];

/// Names too short to count inside another word: `auth` is not `author`, and `pin`
/// is not `shipping`.
const WHOLE_WORD: [&str; 2] = ["auth", "pin"];

/// Those of [`ANYWHERE`] that say little inside the name of a member an API returns:
/// `session_title`.
const WEAK: [&str; 1] = ["session"];

/// Replaces `out` with `name` ASCII-lowercased without `_`, `-` and `.`.
pub fn normalize_into(name: &str, out: &mut String) {
    out.clear();
    out.extend(
        name.chars()
            .filter(|c| !matches!(c, '_' | '-' | '.'))
            .map(|c| c.to_ascii_lowercase()),
    );
}

/// Whether `word`, lowercase, is a whole word of `name`, in any casing or separator
/// (`user_pin`, `userPin`, `x-auth`), and not a part of one (`pinned`, `authored`).
fn has_word(name: &str, word: &str) -> bool {
    let chars: Vec<char> = name.chars().collect();
    let len = word.chars().count();
    (0..chars.len().saturating_sub(len - 1)).any(|at| {
        let here = &chars[at..at + len];
        let same = here
            .iter()
            .zip(word.chars())
            .all(|(c, w)| c.to_ascii_lowercase() == w);
        let starts = at == 0
            || !chars[at - 1].is_alphanumeric()
            || (chars[at - 1].is_lowercase() && here[0].is_uppercase());
        let ends = chars.get(at + len).is_none_or(|next| {
            !next.is_alphanumeric() || (here[len - 1].is_lowercase() && next.is_uppercase())
        });
        same && starts && ends
    })
}

/// Whether a member, header or parameter called `name` may hold a credential.
#[must_use]
pub fn is_credential_name(name: &str) -> bool {
    let mut normalized = String::with_capacity(name.len());
    normalize_into(name, &mut normalized);
    ANYWHERE.iter().any(|n| normalized.contains(n)) || WHOLE_WORD.iter().any(|w| has_word(name, w))
}

/// Whether `name` hides a member a contract says an API returns, where a word alone
/// says less. Credential words hide it as whole words of the name, plural too
/// (`secret_key`, `password_hash`, `tokens`, not `secretary` or `tokenizer`), and
/// compound key names wherever they appear (`api_key_id`). The weak words `session`,
/// `auth` and `pin` hide it only as the name's last word: `user_pin`, not
/// `session_title` or `pin_id`.
#[must_use]
pub fn hides_a_returned_member(name: &str) -> bool {
    let mut normalized = String::with_capacity(name.len());
    normalize_into(name, &mut normalized);
    let strong = ANYWHERE.iter().filter(|n| !WEAK.contains(n)).any(|n| {
        has_word(name, n)
            || has_word(name, &format!("{n}s"))
            || (n.ends_with("key") && normalized.contains(n))
    });
    strong
        || WEAK
            .iter()
            .chain(&WHOLE_WORD)
            .any(|w| normalized.ends_with(w) && has_word(name, w))
}

/// What context shown to a model beyond its own request hides on top of the names above,
/// wherever they appear: `<earlier_answers>`, `<earlier_writes>` and matched records. Only
/// there: these names are too ordinary for what imports, headers and recordings go by.
const CONTEXT_ANYWHERE: [&str; 10] = [
    "pwd",
    "passphrase",
    "jwt",
    "hmac",
    "signature",
    "licensekey",
    "cardnumber",
    "creditcard",
    "recoverycode",
    "connectionstring",
];

/// Those of [`CONTEXT_ANYWHERE`] that no word of theirs says alone.
const CONTEXT_COMPOUND: [&str; 5] = [
    "licensekey",
    "cardnumber",
    "creditcard",
    "recoverycode",
    "connectionstring",
];

/// Names for such context too short to count inside another word (`pass` is not
/// `passenger`), which in a member an API returns count only as its last word
/// (`mfa_enabled` says nothing).
const CONTEXT_WORDS: [&str; 6] = ["pass", "otp", "mfa", "dsn", "ssn", "salt"];

/// Names for such context that count only as a name's last word, plural too, as they
/// are as often an identity as a credential: `key` and `account_sid` hold one, `key_id`
/// does not (see [`may_be_an_identity`]).
const IDENTITY_WORDS: [&str; 2] = ["key", "sid"];

/// Whether a member called `name`, which no schema declares, is kept out of context shown
/// to a model beyond its own request.
#[must_use]
pub fn hides_in_context(name: &str) -> bool {
    let mut normalized = String::with_capacity(name.len());
    normalize_into(name, &mut normalized);
    is_credential_name(name)
        || CONTEXT_ANYWHERE.iter().any(|n| normalized.contains(n))
        || CONTEXT_WORDS.iter().any(|w| has_word(name, w))
        || may_be_an_identity(name)
}

/// [`hides_in_context`] for a member a contract says an API returns, where a word alone
/// says less (see [`hides_a_returned_member`]).
#[must_use]
pub fn hides_returned_in_context(name: &str) -> bool {
    let mut normalized = String::with_capacity(name.len());
    normalize_into(name, &mut normalized);
    hides_a_returned_member(name)
        || CONTEXT_ANYWHERE
            .iter()
            .any(|n| has_word(name, n) || has_word(name, &format!("{n}s")))
        || CONTEXT_COMPOUND.iter().any(|n| normalized.contains(n))
        || CONTEXT_WORDS
            .iter()
            .any(|w| normalized.ends_with(w) && has_word(name, w))
        || may_be_an_identity(name)
}

/// Whether `name` hides only by a word that is as often an identity (`key`, `sid`): such a
/// member stays visible where the contract makes it its type's identity.
#[must_use]
pub fn may_be_an_identity(name: &str) -> bool {
    let mut normalized = String::with_capacity(name.len());
    normalize_into(name, &mut normalized);
    IDENTITY_WORDS.iter().any(|w| {
        let plural = format!("{w}s");
        (normalized.ends_with(w) && has_word(name, w))
            || (normalized.ends_with(&plural) && has_word(name, &plural))
    })
}

/// Whether a value holds a credential by its shape, whatever it is called: a URL with a
/// password or a credential-named parameter in it, a JWT, or a `Bearer ` header's value.
/// For the same context as [`hides_in_context`].
#[must_use]
pub fn is_credential_value(text: &str) -> bool {
    let bearer = text
        .trim_start()
        .get(..7)
        .is_some_and(|start| start.eq_ignore_ascii_case("bearer "));
    bearer
        || text
            .split_whitespace()
            .any(|word| is_jwt(word) || is_secret_url(word))
}

/// Whether a bare word of prose (quotes and punctuation around it ignored) is shaped
/// like a generated token, by its shape alone and with no list of vendors:
///
/// - 20 or more ASCII letters and digits, both present, at least 10 distinct, that
///   either switch between letters and digits at least 4 times or have no lowercase
///   letter (`AKIAIOSFODNN7EXAMPLE`); a camelCase name (`createOrderRequestV2Body`)
///   is not;
/// - a short letter prefix (2 to 8) and `_` or `-`, with a later run of 12 or more
///   letters and digits that mixes them, or of 20 or more that mixes cases
///   (`sk_live_51HxYzAbCdEf`, `ghp_…`, `xoxb-…`); a snake_case name is not.
///
/// Example ids of that shape count too: in prose shown to a model that costs little.
#[must_use]
pub fn is_token_shaped(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    is_long_token(word) || is_prefixed_token(word)
}

fn is_long_token(word: &str) -> bool {
    if word.len() < 20 || !word.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return false;
    }
    let letters = word.bytes().any(|b| b.is_ascii_alphabetic());
    let digits = word.bytes().any(|b| b.is_ascii_digit());
    let distinct = word
        .bytes()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let switches = word
        .as_bytes()
        .windows(2)
        .filter(|w| w[0].is_ascii_digit() != w[1].is_ascii_digit())
        .count();
    let lower = word.bytes().any(|b| b.is_ascii_lowercase());
    letters && digits && distinct >= 10 && (switches >= 4 || !lower)
}

fn is_prefixed_token(word: &str) -> bool {
    let Some(at) = word.find(['_', '-']) else {
        return false;
    };
    let prefix = &word[..at];
    if !(2..=8).contains(&prefix.len()) || !prefix.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    let rest = &word[at + 1..];
    rest.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && rest.split(['_', '-']).any(|run| {
            let mixed = run.bytes().any(|b| b.is_ascii_digit())
                && run.bytes().any(|b| b.is_ascii_alphabetic());
            let cased = run.bytes().any(|b| b.is_ascii_uppercase())
                && run.bytes().any(|b| b.is_ascii_lowercase());
            (run.len() >= 12 && mixed) || (run.len() >= 20 && cased)
        })
}

/// `eyJ…`, base64url JSON, then at least two more dot-separated parts.
fn is_jwt(word: &str) -> bool {
    let mut parts = word.split('.');
    parts.next().is_some_and(|head| head.starts_with("eyJ"))
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some()
}

/// A URL with a password in its userinfo, or a credential-named parameter in its query
/// or fragment.
fn is_secret_url(word: &str) -> bool {
    let Some((_, rest)) = word.split_once("://") else {
        return false;
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let password = authority
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'));
    password
        || tail
            .split(['?', '#', '&', ';'])
            .skip(1)
            .filter_map(|param| param.split_once('='))
            .any(|(param, _)| hides_in_context(param))
}

/// Schema parts [`schema_marks_secret`] looks at, at most.
const MAX_SCHEMA_PARTS: usize = 32;

/// Whether the contract marks a value of `schema` secret: `writeOnly` or
/// `format: password` on it (beside a `$ref` too) or on what it is made of, its `$ref`s (into
/// `bundle`'s definitions) followed and its `anyOf`/`oneOf`/`allOf` branches
/// flattened, at most 8 deep and 32 parts.
#[must_use]
pub fn schema_marks_secret(bundle: &crate::Bundle, schema: &serde_json::Value) -> bool {
    fn visit<'a>(
        bundle: &'a crate::Bundle,
        schema: &'a serde_json::Value,
        depth: usize,
        parts: &mut Vec<&'a serde_json::Value>,
    ) {
        if depth > 8 || parts.len() >= MAX_SCHEMA_PARTS {
            return;
        }
        // The schema's own keys count before its reference is followed: `writeOnly`
        // or `format: password` written beside a `$ref` marks it as well.
        parts.push(schema);
        if let Some(def) = schema
            .get("$ref")
            .and_then(serde_json::Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
        {
            if let Some(target) = bundle.defs.get(def) {
                visit(bundle, target, depth + 1, parts);
            }
            return;
        }
        for key in ["anyOf", "oneOf", "allOf"] {
            for branch in schema
                .get(key)
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                visit(bundle, branch, depth + 1, parts);
            }
        }
    }
    let mut parts = Vec::new();
    visit(bundle, schema, 0, &mut parts);
    parts.iter().any(|p| {
        p.get("writeOnly").and_then(serde_json::Value::as_bool) == Some(true)
            || p.get("format").and_then(serde_json::Value::as_str) == Some("password")
    })
}

/// Whether a number under `name`, once normalized, is a count and no credential:
/// `max_tokens: 100`, `token_count: 5`.
#[must_use]
pub fn is_count_name(normalized: &str) -> bool {
    normalized.ends_with("tokens") || normalized.ends_with("tokencount")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_names_in_any_casing() {
        for name in [
            "password",
            "PASSWD",
            "client_secret",
            "access_token",
            "x-api-key",
            "apiKey",
            "API_KEY",
            "Authorization",
            "set-cookie",
            "sessionId",
            "session_id",
            "clientCredentials",
            "CLIENT_CREDENTIALS",
            "privatekey",
            "PRIVATEKEY",
            "privateKey",
            "private_key",
            "ssh_key",
            "sshKey",
            "signing_key",
            "cvv",
            "CVV",
            "bearer",
            "auth",
            "AUTH",
            "x-auth",
            "authCode",
            "user_auth",
            "pin",
            "PIN",
            "user_pin",
            "pinCode",
            "userPin",
        ] {
            assert!(is_credential_name(name), "{name}");
        }
    }

    #[test]
    fn names_that_only_contain_a_short_word_are_not_credentials() {
        for name in [
            "author",
            "authored",
            "authority",
            "pinned",
            "spin",
            "shipping",
            "mapping",
            "name",
            "status",
            "public_key_id",
        ] {
            assert!(!is_credential_name(name), "{name}");
        }
    }

    #[test]
    fn what_a_returned_member_is_hidden_by() {
        for name in [
            "password",
            "passwordHash",
            "password_hash",
            "client_secret",
            "secret_key",
            "api_key",
            "apiKey",
            "api_key_id",
            "access_token",
            "token_value",
            "tokens",
            "credentials",
            "user_pin",
            "session",
            "auth",
        ] {
            assert!(hides_a_returned_member(name), "{name}");
        }
        for name in [
            "session_title",
            "sessions_count",
            "auth_method",
            "pin_id",
            "tokenizer",
            "secretary",
            "spin",
        ] {
            assert!(!hides_a_returned_member(name), "{name}");
        }
    }

    #[test]
    fn what_imports_and_headers_go_by_is_as_it_was() {
        for name in [
            "pass_count",
            "sort_key",
            "keys",
            "key",
            "sid",
            "account_sid",
            "signature",
            "mfa_enabled",
            "otp_required",
            "credit_card",
            "ssn",
            "salt",
        ] {
            assert!(!is_credential_name(name), "{name}");
            assert!(!hides_a_returned_member(name), "{name}");
        }
    }

    #[test]
    fn context_hides_more_names() {
        for name in [
            "password",
            "pass",
            "user_pass",
            "pwd",
            "userPwd",
            "passphrase",
            "jwt",
            "otp",
            "otp_code",
            "hmac",
            "signature",
            "license_key",
            "licenseKey",
            "card_number",
            "cardNumber",
            "credit_card",
            "creditCard",
            "ssn",
            "salt",
            "mfa",
            "mfa_code",
            "recovery_code",
            "recovery_codes",
            "connection_string",
            "connectionString",
            "dsn",
            "key",
            "keys",
            "KEY",
            "encryption_key",
            "sid",
            "account_sid",
            "accountSid",
        ] {
            assert!(hides_in_context(name), "{name}");
        }
        for name in [
            "passenger",
            "compass",
            "passport",
            "bypass",
            "hotpot",
            "monkey",
            "keyboard",
            "key_id",
            "public_key_id",
            "side",
            "sidebar",
            "residence",
            "pins",
            "salty",
            "lessons",
        ] {
            assert!(!hides_in_context(name), "{name}");
        }
    }

    #[test]
    fn context_hides_more_of_what_an_api_returns() {
        for name in [
            "password",
            "pass",
            "pwd",
            "passphrase",
            "jwt",
            "otp",
            "hmac",
            "signature",
            "license_key",
            "card_number",
            "credit_card",
            "ssn",
            "salt",
            "mfa",
            "recovery_codes",
            "connection_string",
            "dsn",
            "key",
            "keys",
            "sid",
            "account_sid",
        ] {
            assert!(hides_returned_in_context(name), "{name}");
        }
        for name in [
            "mfa_enabled",
            "otp_required",
            "pass_count",
            "key_id",
            "sidebar",
            "monkey",
            "passenger",
            "session_title",
        ] {
            assert!(!hides_returned_in_context(name), "{name}");
        }
    }

    #[test]
    fn key_and_sid_are_words_an_identity_can_be() {
        for name in ["key", "sid", "issue_key", "accountSid"] {
            assert!(may_be_an_identity(name), "{name}");
        }
        for name in ["password", "token", "key_id", "monkey"] {
            assert!(!may_be_an_identity(name), "{name}");
        }
    }

    #[test]
    fn values_shaped_like_credentials() {
        for value in [
            "https://ann:hunter2@example.com/path",
            "postgres://u:p@db.internal:5432/app",
            "redis://:hunter2@cache:6379",
            "https://example.com/in?token=abc",
            "https://example.com/in?v=1&api_key=abc",
            "https://example.com/in?key=abc",
            "https://example.com/in#access_token=abc",
            "https://example.com/s3?X-Amz-Signature=abc",
            "see https://ann:pw@example.com/x for it",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl",
            "the token is eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl",
            "Bearer abc",
            "bearer abc",
            "  Bearer abc",
        ] {
            assert!(is_credential_value(value), "{value}");
        }
        for value in [
            "https://example.com/path",
            "https://ann@example.com/path",
            "https://example.com/in?page=2&q=token",
            "mailto:ann@example.com",
            "ann:not@a-url",
            "a time 12:30@home",
            "eyJust a word",
            "eyJ.only",
            "Bearer",
            "the bearer of news",
            "PROJ-1",
            "1790000000.000100",
        ] {
            assert!(!is_credential_value(value), "{value}");
        }
    }

    #[test]
    fn words_shaped_like_tokens() {
        for word in [
            "sk_live_51HxYzAbCdEf",
            // Split so secret scanners do not take these test shapes for real tokens.
            concat!("sk_", "test_4eC39HqLyjWDarjtT1zdp7dc"),
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "AKIAIOSFODNN7EXAMPLE",
            concat!("xo", "xb-1234567890-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx"),
            "9f8e7d6c5b4a32109f8e",
            "\"sk_live_51HxYzAbCdEf\",",
            "(AKIAIOSFODNN7EXAMPLE).",
        ] {
            assert!(is_token_shaped(word), "{word}");
        }
        for word in [
            "order_created_notification_v2",
            "internationalization",
            "createOrderRequestV2Body",
            "ORDER_STATUS_PENDING",
            "123e4567-e89b-12d3-a456-426614174000",
            "token:read",
            "session_id=123",
            "PROJ-1",
            "1790000000.000100",
            "https://example.com/path",
            "unique",
            "",
        ] {
            assert!(!is_token_shaped(word), "{word}");
        }
    }

    #[test]
    fn a_token_count_is_a_count() {
        assert!(is_count_name("maxtokens"));
        assert!(is_count_name("tokencount"));
        assert!(is_count_name("prompttokencount"));
        assert!(!is_count_name("token"));
        assert!(!is_count_name("accesstoken"));
    }
}
