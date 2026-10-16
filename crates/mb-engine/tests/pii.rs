#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use mb_engine::pii::{BuiltinPattern, CustomPattern, PiiError, Scrubber, ScrubberConfig};
use proptest::prelude::*;
use regex::Regex;
use serde_json::json;

fn all() -> Scrubber {
    Scrubber::new(&ScrubberConfig {
        builtins: BuiltinPattern::ALL.to_vec(),
        custom: Vec::new(),
    })
    .unwrap()
}

fn only(p: BuiltinPattern) -> Scrubber {
    Scrubber::new(&ScrubberConfig {
        builtins: vec![p],
        custom: Vec::new(),
    })
    .unwrap()
}

fn total(c: &BTreeMap<String, usize>) -> usize {
    c.values().sum()
}

fn digits(s: &str) -> Vec<u32> {
    s.chars().filter_map(|c| c.to_digit(10)).collect()
}

fn luhn_ok(d: &[u32]) -> bool {
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &x)| {
            if i % 2 == 1 {
                let y = x * 2;
                if y > 9 { y - 9 } else { y }
            } else {
                x
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

fn iban_ok(compact: &str) -> bool {
    let rearranged = format!("{}{}", &compact[4..], &compact[..4]);
    let mut r: u32 = 0;
    for c in rearranged.chars() {
        if let Some(d) = c.to_digit(10) {
            r = (r * 10 + d) % 97;
        } else {
            r = (r * 100 + (c as u32 - 'A' as u32 + 10)) % 97;
        }
    }
    r == 1
}

#[test]
fn replaces_email() {
    let s = all();
    let (out, counts) = s.scrub_str("contact jane.doe+x@acme-corp.io today");
    let re = Regex::new(r"^contact (user[a-z]{6})@example\.com today$").unwrap();
    assert!(re.is_match(&out), "{out}");
    assert_eq!(counts.get("email"), Some(&1));
    // excluded domains stay
    for kept in [
        "a@example.com",
        "b@example.org",
        "c@foo.test",
        "d@x.example",
        "e@y.invalid",
    ] {
        let (o, c) = s.scrub_str(kept);
        assert_eq!(o, kept);
        assert_eq!(total(&c), 0);
    }
}

#[test]
fn replaces_nanp_and_e164_phones() {
    let s = all();
    let cases = [
        ("(415) 555-2671", r"^\([2-9]\d\d\) 555-01\d\d$"),
        ("415-555-2671", r"^[2-9]\d\d-555-01\d\d$"),
        ("415.555.2671", r"^[2-9]\d\d\.555\.01\d\d$"),
        ("+1 415-555-2671", r"^\+1 [2-9]\d\d-555-01\d\d$"),
        ("+442071838750", r"^\+1555010\d{5}$"),
    ];
    for (input, shape) in cases {
        let (out, counts) = s.scrub_str(&format!("call {input} now"));
        let inner = out
            .strip_prefix("call ")
            .and_then(|o| o.strip_suffix(" now"))
            .unwrap();
        assert!(
            Regex::new(shape).unwrap().is_match(inner),
            "{input} -> {out}"
        );
        assert_ne!(inner, input);
        assert_eq!(counts.get("phone"), Some(&1), "{input}");
    }
    // the reserved fictional range is kept
    let (o, c) = s.scrub_str("415-555-0123");
    assert_eq!(o, "415-555-0123");
    assert_eq!(total(&c), 0);
}

#[test]
fn replaces_luhn_valid_cards_keeping_separators() {
    let s = all();
    let cases = [
        ("4111 1111 1111 1111", r"^4000 00\d\d \d{4} \d{4}$"),
        ("5500-0000-0000-0004", r"^4000-00\d\d-\d{4}-\d{4}$"),
        ("4111111111111111", r"^400000\d{10}$"),
    ];
    for (input, shape) in cases {
        let (out, counts) = s.scrub_str(&format!("card: {input}."));
        let inner = out
            .strip_prefix("card: ")
            .and_then(|o| o.strip_suffix('.'))
            .unwrap();
        assert!(
            Regex::new(shape).unwrap().is_match(inner),
            "{input} -> {out}"
        );
        assert!(luhn_ok(&digits(inner)), "{inner}");
        assert_eq!(counts.get("credit_card"), Some(&1));
    }
}

#[test]
fn replaces_ssn() {
    let s = all();
    let (out, counts) = s.scrub_str("ssn 123-45-6789");
    let re = Regex::new(r"^ssn 9\d\d-\d\d-\d{4}$").unwrap();
    assert!(re.is_match(&out), "{out}");
    assert!(!out.ends_with("-00-0000"));
    assert_eq!(counts.get("us_ssn"), Some(&1));
    for kept in [
        "000-12-3456",
        "666-12-3456",
        "123-00-4567",
        "123-45-0000",
        "912-34-5678",
    ] {
        let (o, c) = s.scrub_str(kept);
        assert_eq!(o, kept);
        assert_eq!(total(&c), 0);
    }
}

#[test]
fn replaces_valid_iban() {
    let s = all();
    let (out, counts) = s.scrub_str("iban DE89 3704 0044 0532 0130 00 ok");
    let re = Regex::new(r"^iban (DE\d\d 0000 \d{4} \d{4} \d{4} \d\d) ok$").unwrap();
    let caps = re.captures(&out).unwrap_or_else(|| panic!("{out}"));
    let compact: String = caps[1].chars().filter(|c| *c != ' ').collect();
    assert!(iban_ok(&compact), "{compact}");
    assert_eq!(counts.get("iban"), Some(&1));

    let (out, _) = s.scrub_str("GB82WEST12345698765432");
    assert!(
        Regex::new(r"^GB\d\d0000\d{14}$").unwrap().is_match(&out),
        "{out}"
    );
    assert!(iban_ok(&out));

    // invalid check digits: untouched
    let (o, c) = s.scrub_str("DE00 3704 0044 0532 0130 00");
    assert_eq!(o, "DE00 3704 0044 0532 0130 00");
    assert_eq!(total(&c), 0);
}

#[test]
fn replaces_public_ipv4() {
    let s = all();
    let (out, counts) = s.scrub_str("from 8.8.8.8 and 93.184.216.34");
    let re = Regex::new(
        r"^from (192\.0\.2|198\.51\.100|203\.0\.113)\.\d{1,3} and (192\.0\.2|198\.51\.100|203\.0\.113)\.\d{1,3}$",
    )
    .unwrap();
    assert!(re.is_match(&out), "{out}");
    assert_eq!(counts.get("ipv4"), Some(&2));
    for kept in [
        "10.1.2.3",
        "127.0.0.1",
        "172.20.1.1",
        "192.168.1.1",
        "0.1.2.3",
        "192.0.2.10",
        "198.51.100.7",
        "203.0.113.9",
        "300.1.1.1",
    ] {
        let (o, c) = s.scrub_str(kept);
        assert_eq!(o, kept);
        assert_eq!(total(&c), 0);
    }
}

#[test]
fn leaves_uuids_dates_and_order_numbers() {
    let s = all();
    let input = "id 3f2b8c1e-9a4d-4f6b-8c2e-1d3a5b7c9e0f on 2026-10-14 order ORD-1234567890 \
                 version v1.2.3.4 ref 4155552671 uuid 12345678-1234-1234-1234-123456789012";
    let (out, counts) = s.scrub_str(input);
    assert_eq!(out, input);
    assert_eq!(total(&counts), 0);
}

#[test]
fn luhn_invalid_digits_untouched() {
    let s = all();
    for input in [
        "4111 1111 1111 1112",
        "4111111111111112",
        "1234567890123456",
    ] {
        let (o, c) = s.scrub_str(input);
        assert_eq!(o, input);
        assert_eq!(total(&c), 0);
    }
    // all one digit is not a card even though Luhn-valid
    let (o, _) = s.scrub_str("0000000000000000");
    assert_eq!(o, "0000000000000000");
}

#[test]
fn scrub_is_idempotent() {
    let s = all();
    let input = "Jane <jane@acme.io>, (415) 555-2671, +442071838750, 4111 1111 1111 1111, \
                 123-45-6789, DE89370400440532013000, 8.8.8.8";
    let (once, c1) = s.scrub_str(input);
    assert_eq!(total(&c1), 7);
    let (twice, c2) = s.scrub_str(&once);
    assert_eq!(twice, once);
    assert_eq!(total(&c2), 0);
}

#[test]
fn keys_are_not_scrubbed() {
    let s = all();
    let mut v = json!({
        "jane@acme.io": "jane@acme.io",
        "nested": [{"8.8.8.8": ["8.8.8.8", 42, null, true]}]
    });
    let counts = s.scrub_value(&mut v);
    assert_eq!(counts.get("email"), Some(&1));
    assert_eq!(counts.get("ipv4"), Some(&1));
    let obj = v.as_object().unwrap();
    assert!(obj.contains_key("jane@acme.io"));
    assert_ne!(obj["jane@acme.io"], "jane@acme.io");
    let inner = &v["nested"][0]["8.8.8.8"];
    assert_ne!(inner[0], "8.8.8.8");
    assert_eq!(inner[1], 42);
}

#[test]
fn custom_pattern_template() {
    let s = Scrubber::new(&ScrubberConfig {
        builtins: Vec::new(),
        custom: vec![CustomPattern {
            name: "employee_id".into(),
            regex: r"EMP-\d{6}".into(),
            replacement: "EMP-######".into(),
        }],
    })
    .unwrap();
    let (out, counts) = s.scrub_str("by EMP-123456 and EMP-654321");
    let re = Regex::new(r"^by EMP-\d{6} and EMP-\d{6}$").unwrap();
    assert!(re.is_match(&out), "{out}");
    assert_eq!(counts.get("employee_id"), Some(&2));
}

#[test]
fn invalid_custom_regex_names_index() {
    let ok = CustomPattern {
        name: "ok".into(),
        regex: "a+".into(),
        replacement: "b".into(),
    };
    let bad = CustomPattern {
        name: "bad".into(),
        regex: "(unclosed".into(),
        replacement: "x".into(),
    };
    let err = Scrubber::new(&ScrubberConfig {
        builtins: Vec::new(),
        custom: vec![ok, bad],
    })
    .unwrap_err();
    let PiiError::Custom { index, .. } = &err;
    assert_eq!(*index, 1);
    assert!(err.to_string().starts_with("pii.custom[1]: "), "{err}");
}

#[test]
fn disabled_builtins_are_skipped() {
    let s = only(BuiltinPattern::Email);
    let input = "a@acme.io 8.8.8.8 123-45-6789";
    let (out, counts) = s.scrub_str(input);
    assert!(out.ends_with(" 8.8.8.8 123-45-6789"), "{out}");
    assert!(out.starts_with("user"));
    assert_eq!(counts.len(), 1);
}

#[test]
fn builtin_names_round_trip() {
    for p in BuiltinPattern::ALL {
        let v = serde_json::to_value(p).unwrap();
        assert_eq!(v, json!(p.as_str()));
        let back: BuiltinPattern = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }
}

#[test]
fn oversized_strings_are_left_alone() {
    let s = all();
    let big = format!("{} 8.8.8.8", "x".repeat(64 * 1024));
    let (out, counts) = s.scrub_str(&big);
    assert_eq!(out, big);
    assert_eq!(total(&counts), 0);
}

/// Maps every ASCII digit in `s` to the same digit in another Unicode
/// decimal-digit block (`zero` is that block's digit zero).
fn redigit(s: &str, zero: char) -> String {
    s.chars()
        .map(|c| match c.to_digit(10) {
            Some(d) if c.is_ascii_digit() => char::from_u32(zero as u32 + d).unwrap(),
            _ => c,
        })
        .collect()
}

#[test]
fn non_ascii_digits_never_panic_and_are_left_alone() {
    // Shapes that every builtin replaces when written with ASCII digits.
    let shapes: [(BuiltinPattern, &str); 9] = [
        (BuiltinPattern::Email, "jane.doe1@corp-mail.com"),
        (BuiltinPattern::Phone, "(415) 555-2671"),
        (BuiltinPattern::Phone, "+14155552671"),
        (BuiltinPattern::CreditCard, "4111 1111 1111 1111"),
        (BuiltinPattern::CreditCard, "4111111111111111"),
        (BuiltinPattern::UsSsn, "123-45-6789"),
        (BuiltinPattern::Iban, "GB82 WEST 1234 5698 7654 32"),
        (BuiltinPattern::Iban, "DE89370400440532013000"),
        (BuiltinPattern::Ipv4, "8.8.8.8"),
    ];
    // Devanagari, Arabic-Indic and fullwidth digit zeros.
    let zeros = ['\u{0966}', '\u{0660}', '\u{FF10}'];
    for (p, shape) in shapes {
        for zero in zeros {
            let input = redigit(shape, zero);
            for s in [only(p), all()] {
                let (out, counts) = s.scrub_str(&input);
                assert_eq!(out, input, "{p:?} on {input:?}");
                assert_eq!(total(&counts), 0, "{p:?} on {input:?}");
            }
            // Mixed: ASCII and non-ASCII digits in one candidate.
            let mixed = format!("{}{}", redigit(&shape[..4], zero), &shape[4..]);
            let (out, _) = all().scrub_str(&mixed);
            assert_eq!(all().scrub_str(&out).0, out, "{p:?} on {mixed:?}");
        }
    }
    // The reported crash input.
    let input = "DE\u{0969}\u{0969}12345678901";
    let (out, counts) = all().scrub_str(input);
    assert_eq!(out, input);
    assert_eq!(total(&counts), 0);
}

static SHARED: std::sync::LazyLock<Scrubber> = std::sync::LazyLock::new(all);

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]
    #[test]
    fn scrub_never_panics_and_is_idempotent(
        input in prop_oneof![
            "\\PC{0,200}",
            "[0-9 .()+@A-Za-z-]{0,120}",
            "([0-9]{1,6}[ .-]){0,8}[0-9]{0,6}",
            "[0-9\u{0660}-\u{0669}\u{0966}-\u{096F}\u{FF10}-\u{FF19} .()+@A-Z-]{0,80}",
        ]
    ) {
        let s = &*SHARED;
        let (once, _) = s.scrub_str(&input);
        let (twice, c2) = s.scrub_str(&once);
        prop_assert_eq!(&twice, &once);
        prop_assert_eq!(total(&c2), 0);
    }
}
