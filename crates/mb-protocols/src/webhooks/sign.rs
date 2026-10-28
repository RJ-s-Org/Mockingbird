//! Webhook signatures: Stripe's `Stripe-Signature` and Mockingbird's own
//! `X-Mockingbird-Signature`.

use std::fmt::Write;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// How a webhook body is signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signing {
    /// `Stripe-Signature: t=<ts>,v1=<hex HMAC-SHA256(secret, "<ts>.<body>")>`.
    Stripe,
    /// `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>`.
    HmacSha256,
    /// No signature header.
    None,
}

pub const STRIPE_SIGNATURE_HEADER: &str = "stripe-signature";
pub const MOCKINGBIRD_SIGNATURE_HEADER: &str = "x-mockingbird-signature";

/// Lowercase hex HMAC-SHA256 of `message` under `secret`.
pub fn hmac_sha256_hex(secret: &[u8], message: &[u8]) -> String {
    // HMAC accepts keys of any length, so this never fails.
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret) {
        Ok(mac) => mac,
        Err(_) => return String::new(),
    };
    mac.update(message);
    let digest = mac.finalize().into_bytes();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Stripe's header value: `t=<timestamp>,v1=<hex HMAC-SHA256(secret, "<timestamp>.<body>")>`.
pub fn stripe_signature(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let signed = [timestamp.to_string().as_bytes(), b".", body].concat();
    format!("t={timestamp},v1={}", hmac_sha256_hex(secret, &signed))
}

/// `sha256=<lowercase hex HMAC-SHA256(secret, body)>`.
pub fn mockingbird_signature(secret: &[u8], body: &[u8]) -> String {
    format!("sha256={}", hmac_sha256_hex(secret, body))
}

/// The header name and value `signing` adds to a delivery of `body` at
/// `timestamp` (unix seconds); `None` for [`Signing::None`].
pub fn signature_header(
    signing: Signing,
    secret: &[u8],
    timestamp: i64,
    body: &[u8],
) -> Option<(&'static str, String)> {
    match signing {
        Signing::Stripe => Some((
            STRIPE_SIGNATURE_HEADER,
            stripe_signature(secret, timestamp, body),
        )),
        Signing::HmacSha256 => Some((
            MOCKINGBIRD_SIGNATURE_HEADER,
            mockingbird_signature(secret, body),
        )),
        Signing::None => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    const BODY: &[u8] = br#"{"id":"evt_1","type":"customer.created"}"#;

    #[test]
    fn stripe_signature_test_vector() {
        assert_eq!(
            stripe_signature(b"whsec_test_secret", 1_700_000_000, BODY),
            "t=1700000000,v1=2146c3ca883db3d5b33f638513d4322fcf955eeb8624ee63937a742c023fe30b"
        );
    }

    /// Stripe's documented verification: split on ',', take t= and v1=,
    /// recompute HMAC-SHA256(secret, "<t>.<payload>") and compare.
    #[test]
    fn stripe_signature_verifies_with_stripes_algorithm() {
        let header = stripe_signature(b"whsec_test_secret", 1_700_000_123, BODY);
        let mut t = None;
        let mut v1 = None;
        for part in header.split(',') {
            match part.split_once('=').unwrap() {
                ("t", v) => t = Some(v.to_string()),
                ("v1", v) => v1 = Some(v.to_string()),
                _ => {}
            }
        }
        let signed = [t.unwrap().as_bytes(), b".", BODY].concat();
        assert_eq!(v1.unwrap(), hmac_sha256_hex(b"whsec_test_secret", &signed));
    }

    #[test]
    fn mockingbird_signature_test_vector() {
        assert_eq!(
            mockingbird_signature(b"whsec_test_secret", BODY),
            "sha256=fd41b171babb49da71ea3fab7b7da430fd8fc07d35a82924971cc9d6d758d58a"
        );
    }

    #[test]
    fn signature_header_per_scheme() {
        assert_eq!(signature_header(Signing::None, b"k", 1, b"{}"), None);
        assert_eq!(
            signature_header(Signing::Stripe, b"k", 1, b"{}").unwrap().0,
            "stripe-signature"
        );
        assert_eq!(
            signature_header(Signing::HmacSha256, b"k", 1, b"{}")
                .unwrap()
                .0,
            "x-mockingbird-signature"
        );
    }
}
