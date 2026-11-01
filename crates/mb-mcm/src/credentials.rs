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
    fn a_token_count_is_a_count() {
        assert!(is_count_name("maxtokens"));
        assert!(is_count_name("tokencount"));
        assert!(is_count_name("prompttokencount"));
        assert!(!is_count_name("token"));
        assert!(!is_count_name("accesstoken"));
    }
}
