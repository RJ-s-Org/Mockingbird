//! Output PII scrubber: replaces personal-data look-alikes in generated JSON
//! strings with synthetic values of the same shape.
//!
//! Every builtin replacement lies inside that pattern's excluded (reserved,
//! documentation or test) range, and excluded matches are never replaced, so
//! scrubbing already-scrubbed output changes nothing. Custom patterns run
//! once after the builtins and never touch text a builtin matched.

use std::collections::BTreeMap;
use std::ops::Range;

use rand::{Rng, RngExt};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Strings longer than this many bytes are left as they are.
pub const MAX_SCRUB_BYTES: usize = 64 * 1024;
/// Most custom patterns one scrubber accepts.
pub const MAX_CUSTOM_PATTERNS: usize = 32;
/// Compiled-size limit for every pattern regex.
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;
/// Builtin passes repeat until nothing changes, at most this often.
const MAX_PASSES: usize = 8;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinPattern {
    Email,
    Phone,
    CreditCard,
    UsSsn,
    Iban,
    Ipv4,
}

impl BuiltinPattern {
    pub const ALL: [BuiltinPattern; 6] = [
        BuiltinPattern::Email,
        BuiltinPattern::Phone,
        BuiltinPattern::CreditCard,
        BuiltinPattern::UsSsn,
        BuiltinPattern::Iban,
        BuiltinPattern::Ipv4,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BuiltinPattern::Email => "email",
            BuiltinPattern::Phone => "phone",
            BuiltinPattern::CreditCard => "credit_card",
            BuiltinPattern::UsSsn => "us_ssn",
            BuiltinPattern::Iban => "iban",
            BuiltinPattern::Ipv4 => "ipv4",
        }
    }

    /// Candidate regex; [`classify`] validates each candidate.
    fn regex(self) -> &'static str {
        match self {
            BuiltinPattern::Email => r"[A-Za-z0-9._%+-]+@(?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,}",
            BuiltinPattern::Phone => {
                r"(?:\+1[ -]?)?(?:\(\d{3}\) ?|\d{3}[-.])\d{3}[-.]\d{4}|\+[1-9]\d{7,14}"
            }
            BuiltinPattern::CreditCard => r"\d{13,19}|\d{1,6}(?:[ -]\d{1,6})+",
            BuiltinPattern::UsSsn => r"\b\d{3}-\d{2}-\d{4}\b",
            BuiltinPattern::Iban => {
                r"\b[A-Z]{2}\d{2}(?:[A-Z0-9]{11,30}|(?: [A-Z0-9]{4}){2,7}(?: [A-Z0-9]{1,4})?)\b"
            }
            BuiltinPattern::Ipv4 => r"\b(?:\d{1,3}\.){3}\d{1,3}\b",
        }
    }
}

/// Builtins run in this order: patterns whose matches contain digit runs
/// (emails, IBANs) claim their text before the purely numeric ones.
const RUN_ORDER: [BuiltinPattern; 6] = [
    BuiltinPattern::Email,
    BuiltinPattern::Iban,
    BuiltinPattern::CreditCard,
    BuiltinPattern::UsSsn,
    BuiltinPattern::Phone,
    BuiltinPattern::Ipv4,
];

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct CustomPattern {
    pub name: String,
    pub regex: String,
    /// Template: `#` → random digit, `?` → random lowercase letter, anything
    /// else literal.
    pub replacement: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScrubberConfig {
    pub builtins: Vec<BuiltinPattern>,
    pub custom: Vec<CustomPattern>,
}

impl Default for ScrubberConfig {
    /// All six builtins, no custom patterns.
    fn default() -> Self {
        Self {
            builtins: BuiltinPattern::ALL.to_vec(),
            custom: Vec::new(),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum PiiError {
    #[error("pii.custom[{index}]: {reason}")]
    Custom { index: usize, reason: String },
}

#[derive(Clone, Debug)]
struct CompiledCustom {
    name: String,
    regex: Regex,
    replacement: String,
}

/// A compiled set of PII patterns.
#[derive(Clone, Debug)]
pub struct Scrubber {
    builtins: Vec<(BuiltinPattern, Regex)>,
    custom: Vec<CompiledCustom>,
}

fn compile(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
}

fn valid_custom_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl Scrubber {
    pub fn new(cfg: &ScrubberConfig) -> Result<Self, PiiError> {
        // The builtin regexes are constants that always compile (covered by
        // the per-builtin tests).
        let builtins = RUN_ORDER
            .into_iter()
            .filter(|kind| cfg.builtins.contains(kind))
            .filter_map(|kind| compile(kind.regex()).ok().map(|re| (kind, re)))
            .collect();
        if cfg.custom.len() > MAX_CUSTOM_PATTERNS {
            return Err(PiiError::Custom {
                index: MAX_CUSTOM_PATTERNS,
                reason: format!("at most {MAX_CUSTOM_PATTERNS} custom patterns are allowed"),
            });
        }
        let mut custom = Vec::with_capacity(cfg.custom.len());
        for (index, c) in cfg.custom.iter().enumerate() {
            if !valid_custom_name(&c.name) {
                return Err(PiiError::Custom {
                    index,
                    reason: format!("name {:?} must match [a-z0-9_]{{1,32}}", c.name),
                });
            }
            let regex = compile(&c.regex).map_err(|e| PiiError::Custom {
                index,
                reason: format!("invalid regex: {e}"),
            })?;
            custom.push(CompiledCustom {
                name: c.name.clone(),
                regex,
                replacement: c.replacement.clone(),
            });
        }
        Ok(Self { builtins, custom })
    }

    /// Replaces matches inside every JSON string (object keys untouched); returns replacements per pattern name.
    pub fn scrub_value(&self, v: &mut Value) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        self.scrub_value_into(v, &mut counts);
        counts
    }

    fn scrub_value_into(&self, v: &mut Value, counts: &mut BTreeMap<String, usize>) {
        match v {
            Value::String(s) => {
                let (out, c) = self.scrub_str(s);
                if !c.is_empty() {
                    *s = out;
                    for (k, n) in c {
                        *counts.entry(k).or_insert(0) += n;
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.scrub_value_into(item, counts);
                }
            }
            Value::Object(map) => {
                for (_, item) in map.iter_mut() {
                    self.scrub_value_into(item, counts);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// Scrubs one string; returns the result and the replacements per
    /// pattern name (patterns with no replacement are absent).
    pub fn scrub_str(&self, s: &str) -> (String, BTreeMap<String, usize>) {
        let mut counts = BTreeMap::new();
        if s.len() > MAX_SCRUB_BYTES || (self.builtins.is_empty() && self.custom.is_empty()) {
            return (s.to_owned(), counts);
        }
        let mut rng = rand::rng();
        let mut text = s.to_owned();
        let mut protected = Vec::new();
        // A replacement can (rarely) turn a neighbouring candidate of an
        // earlier pattern valid; repeating until a pass changes nothing keeps
        // the result a fixed point, so scrubbing it again is a no-op.
        for _ in 0..MAX_PASSES {
            protected.clear();
            let mut replaced = 0;
            for (kind, regex) in &self.builtins {
                let (next, prot, n) = apply(&text, &protected, regex, |t, r| {
                    classify(*kind, t, r, &mut rng)
                });
                text = next;
                protected = prot;
                if n > 0 {
                    *counts.entry(kind.as_str().to_owned()).or_insert(0) += n;
                    replaced += n;
                }
            }
            if replaced == 0 {
                break;
            }
        }
        for c in &self.custom {
            let (next, prot, n) = apply(&text, &protected, &c.regex, |_, r| {
                Some((
                    r.end,
                    Hit::Replace(expand_template(&c.replacement, &mut rng)),
                ))
            });
            text = next;
            protected = prot;
            if n > 0 {
                *counts.entry(c.name.clone()).or_insert(0) += n;
            }
        }
        (text, counts)
    }
}

/// What to do with a validated match.
enum Hit {
    /// In the excluded range: keep it, but claim the text.
    Keep,
    /// Replace with this synthetic value.
    Replace(String),
}

/// Runs one pattern over `text`, skipping matches that overlap `protected`
/// (sorted, disjoint byte ranges). `classify` returns `None` for a candidate
/// that is not PII, or the (possibly shortened) end of the hit and what to do.
/// Returns the new text, its protected ranges, and the number of replacements.
fn apply(
    text: &str,
    protected: &[Range<usize>],
    regex: &Regex,
    mut classify: impl FnMut(&str, Range<usize>) -> Option<(usize, Hit)>,
) -> (String, Vec<Range<usize>>, usize) {
    let mut out = String::with_capacity(text.len());
    let mut out_prot = Vec::with_capacity(protected.len());
    let mut cursor = 0;
    let mut replaced = 0;
    for m in regex.find_iter(text) {
        if m.is_empty() || overlaps(protected, m.start(), m.end()) {
            continue;
        }
        let Some((end, hit)) = classify(text, m.range()) else {
            continue;
        };
        copy_span(text, cursor, m.start(), protected, &mut out, &mut out_prot);
        let start = out.len();
        match hit {
            Hit::Keep => out.push_str(&text[m.start()..end]),
            Hit::Replace(r) => {
                out.push_str(&r);
                replaced += 1;
            }
        }
        out_prot.push(start..out.len());
        cursor = end;
    }
    copy_span(text, cursor, text.len(), protected, &mut out, &mut out_prot);
    (out, out_prot, replaced)
}

fn overlaps(protected: &[Range<usize>], start: usize, end: usize) -> bool {
    let i = protected.partition_point(|r| r.end <= start);
    protected.get(i).is_some_and(|r| r.start < end)
}

/// Copies `text[a..b]` to `out`, carrying over the protected ranges inside it.
fn copy_span(
    text: &str,
    a: usize,
    b: usize,
    protected: &[Range<usize>],
    out: &mut String,
    out_prot: &mut Vec<Range<usize>>,
) {
    let base = out.len();
    let first = protected.partition_point(|r| r.end <= a);
    for r in &protected[first..] {
        if r.start >= b {
            break;
        }
        out_prot.push(r.start - a + base..r.end - a + base);
    }
    out.push_str(&text[a..b]);
}

/// True when `next` (and the char after it) show the candidate continues a
/// longer token: an alphanumeric neighbour, or one of `seps` followed by a
/// digit.
fn continues(mut chars: impl Iterator<Item = char>, seps: &[char]) -> bool {
    match chars.next() {
        None => false,
        Some(c) if c.is_alphanumeric() || c == '_' => true,
        Some(c) if seps.contains(&c) => chars.next().is_some_and(|d| d.is_ascii_digit()),
        Some(_) => false,
    }
}

fn isolated(text: &str, r: &Range<usize>, seps: &[char]) -> bool {
    !continues(text[..r.start].chars().rev(), seps) && !continues(text[r.end..].chars(), seps)
}

fn classify<R: Rng>(
    kind: BuiltinPattern,
    text: &str,
    r: Range<usize>,
    rng: &mut R,
) -> Option<(usize, Hit)> {
    let m = &text[r.clone()];
    let hit = match kind {
        BuiltinPattern::Email => email(m, rng),
        BuiltinPattern::Phone => {
            if !isolated(text, &r, &['-', '.']) {
                return None;
            }
            phone(m, rng)?
        }
        BuiltinPattern::CreditCard => {
            if !isolated(text, &r, &['-', '.']) || text[..r.start].ends_with('+') {
                return None;
            }
            card(m, rng)?
        }
        BuiltinPattern::UsSsn => {
            if !isolated(text, &r, &['-', '.']) {
                return None;
            }
            ssn(m, rng)?
        }
        BuiltinPattern::Iban => return iban(m, rng).map(|(len, hit)| (r.start + len, hit)),
        BuiltinPattern::Ipv4 => {
            if !isolated(text, &r, &['.']) {
                return None;
            }
            ipv4(m, rng)?
        }
    };
    Some((r.end, hit))
}

fn random_digit<R: Rng>(rng: &mut R) -> char {
    char::from(b'0' + rng.random_range(0..10u8))
}

fn random_digits<R: Rng>(rng: &mut R, n: usize) -> String {
    (0..n).map(|_| random_digit(rng)).collect()
}

fn random_lower<R: Rng>(rng: &mut R) -> char {
    char::from(b'a' + rng.random_range(0..26u8))
}

fn expand_template<R: Rng>(template: &str, rng: &mut R) -> String {
    template
        .chars()
        .map(|c| match c {
            '#' => random_digit(rng),
            '?' => random_lower(rng),
            other => other,
        })
        .collect()
}

/// Writes `digits` over the digit positions of `shape`, keeping everything
/// else (separators, parentheses, `+`).
fn reshape(shape: &str, digits: &str) -> String {
    let mut it = digits.chars();
    shape
        .chars()
        .map(|c| {
            if c.is_ascii_digit() {
                it.next().unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

fn email<R: Rng>(m: &str, rng: &mut R) -> Hit {
    let domain = m
        .rsplit_once('@')
        .map(|(_, d)| d.to_ascii_lowercase())
        .unwrap_or_default();
    let excluded = matches!(
        domain.as_str(),
        "example.com" | "example.net" | "example.org"
    ) || [".example", ".test", ".invalid"]
        .iter()
        .any(|suffix| domain.ends_with(suffix));
    if excluded {
        return Hit::Keep;
    }
    let local: String = (0..6).map(|_| random_lower(rng)).collect();
    Hit::Replace(format!("user{local}@example.com"))
}

fn phone<R: Rng>(m: &str, rng: &mut R) -> Option<Hit> {
    let digits: String = m.chars().filter(char::is_ascii_digit).collect();
    let e164 = m
        .strip_prefix('+')
        .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_digit()));
    if e164 {
        let d = digits.as_bytes();
        let excluded = digits.starts_with("1555010")
            || (d.len() == 11 && d[0] == b'1' && &d[4..7] == b"555" && &d[7..9] == b"01");
        if excluded {
            return Some(Hit::Keep);
        }
        let tail = random_digits(rng, digits.len().saturating_sub(7));
        return Some(Hit::Replace(format!("+1555010{tail}")));
    }
    let (prefix, ten) = match digits.len() {
        10 => ("", digits.as_str()),
        11 if digits.starts_with('1') => ("1", &digits[1..]),
        _ => return None,
    };
    let d = ten.as_bytes();
    if !(b'2'..=b'9').contains(&d[0]) || !(b'2'..=b'9').contains(&d[3]) {
        return None;
    }
    if &d[3..6] == b"555" && &d[6..8] == b"01" {
        return Some(Hit::Keep);
    }
    let area = format!(
        "{}{}",
        char::from(b'2' + rng.random_range(0..8u8)),
        random_digits(rng, 2)
    );
    let new = format!("{prefix}{area}55501{}", random_digits(rng, 2));
    Some(Hit::Replace(reshape(m, &new)))
}

fn luhn_valid(digits: &[u8]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &c)| {
            let x = u32::from(c - b'0');
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

fn card<R: Rng>(m: &str, rng: &mut R) -> Option<Hit> {
    let digits: Vec<u8> = m.bytes().filter(u8::is_ascii_digit).collect();
    if !(13..=19).contains(&digits.len())
        || digits.iter().all(|&d| d == digits[0])
        || !luhn_valid(&digits)
    {
        return None;
    }
    if digits.starts_with(b"400000") {
        return Some(Hit::Keep);
    }
    let mut new = format!("400000{}0", random_digits(rng, digits.len() - 7)).into_bytes();
    let last = new.len() - 1;
    for check in b'0'..=b'9' {
        new[last] = check;
        if luhn_valid(&new) {
            break;
        }
    }
    let new = String::from_utf8(new).ok()?;
    Some(Hit::Replace(reshape(m, &new)))
}

fn ssn<R: Rng>(m: &str, rng: &mut R) -> Option<Hit> {
    let mut parts = m.split('-');
    let (area, group, serial) = (parts.next()?, parts.next()?, parts.next()?);
    if area == "000" || area == "666" || group == "00" || serial == "0000" {
        return None;
    }
    if area.starts_with('9') {
        return Some(Hit::Keep);
    }
    Some(Hit::Replace(format!(
        "9{}-{:02}-{:04}",
        random_digits(rng, 2),
        rng.random_range(1..=99u32),
        rng.random_range(1..=9999u32)
    )))
}

/// `(IBAN with its first four characters moved to the end) mod 97`.
fn iban_mod97(compact: &str) -> u32 {
    let (head, bban) = compact.split_at(4);
    let mut r: u32 = 0;
    for c in bban.chars().chain(head.chars()) {
        if let Some(d) = c.to_digit(10) {
            r = (r * 10 + d) % 97;
        } else if c.is_ascii_uppercase() {
            r = (r * 100 + (u32::from(c) - u32::from('A') + 10)) % 97;
        }
    }
    r
}

/// Returns the byte length of the IBAN hit (trailing groups are dropped while
/// the candidate does not validate) and what to do with it.
fn iban<R: Rng>(m: &str, rng: &mut R) -> Option<(usize, Hit)> {
    let mut cand = m;
    loop {
        let compact: String = cand.chars().filter(|c| *c != ' ').collect();
        if (15..=34).contains(&compact.len()) && iban_mod97(&compact) == 1 {
            if compact[4..].starts_with("0000") {
                return Some((cand.len(), Hit::Keep));
            }
            let country = &compact[..2];
            let bban = format!("0000{}", random_digits(rng, compact.len() - 8));
            let check = 98 - iban_mod97(&format!("{country}00{bban}"));
            let new = format!("{country}{check:02}{bban}");
            let mut chars = new.chars();
            let shaped: String = cand
                .chars()
                .map(|c| {
                    if c == ' ' {
                        ' '
                    } else {
                        chars.next().unwrap_or(c)
                    }
                })
                .collect();
            return Some((cand.len(), Hit::Replace(shaped)));
        }
        let (shorter, _) = cand.rsplit_once(' ')?;
        cand = shorter;
    }
}

fn ipv4<R: Rng>(m: &str, rng: &mut R) -> Option<Hit> {
    let mut o = [0u8; 4];
    for (slot, part) in o.iter_mut().zip(m.split('.')) {
        *slot = part.parse().ok()?;
    }
    let excluded = matches!(
        o,
        [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _] | [0 | 10 | 127, ..] | [192, 168, ..]
    ) || (o[0] == 172 && (o[1] & 0xf0) == 16);
    if excluded {
        return Some(Hit::Keep);
    }
    let net = ["192.0.2", "198.51.100", "203.0.113"][rng.random_range(0..3usize)];
    let host = rng.random_range(1..=254u8);
    Some(Hit::Replace(format!("{net}.{host}")))
}
