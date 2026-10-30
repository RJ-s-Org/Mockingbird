//! Seeded faker draws: the same seed, contract and request give the same
//! body.
//!
//! An [`Engine`](crate::Engine) with a faker seed (offline engines have
//! [`DEFAULT_FAKER_SEED`] unless [`Engine::with_faker_seed`](crate::Engine::with_faker_seed)
//! sets another) draws each answer's faker values from a generator seeded by
//! the seed, the contract's source digest, the operation, the status and the
//! call's path parameters, query and body. A call may carry its own seed in
//! [`SEED_HEADER`] (see [`request_seed`]). Without a seed the draws are
//! random.
//!
//! Generated dates and times are drawn back from [`SEEDED_NOW`] instead of
//! the clock, so a seeded answer does not change from one day to the next.
//! The values are reproducible with the same Mockingbird version: a new
//! release may change what a seed draws.

use rand::SeedableRng;
use rand::rngs::StdRng;
use serde_json::{Map, Value};

use crate::OperationCall;

/// The request header (in [`OperationCall::headers`], any case) whose
/// decimal `u64` value seeds this call's faker draws instead of the
/// engine's seed. A value that is not a `u64` is ignored here; the
/// protocol adapters reject it with a 400 first.
pub const SEED_HEADER: &str = "x-mockingbird-seed";

/// The faker seed of an engine without an LLM provider when none is set:
/// offline answers are reproducible by default.
pub const DEFAULT_FAKER_SEED: u64 = 0;

/// The Unix time (2026-01-01T00:00:00Z) seeded draws take as "now":
/// generated moments fall in the 30 days before it.
pub const SEEDED_NOW: i64 = 1_767_225_600;

/// The seed `call` carries in [`SEED_HEADER`], if it is a decimal `u64`.
#[must_use]
pub fn request_seed(call: &OperationCall) -> Option<u64> {
    call.headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(SEED_HEADER))
        .and_then(|(_, value)| value.as_str())
        .and_then(|text| text.trim().parse().ok())
}

/// The faker draws of one answer: the generator, and whether it is seeded
/// (seeded draws take [`SEEDED_NOW`] as the time).
pub(crate) struct Draws {
    pub(crate) rng: StdRng,
    pub(crate) seeded: bool,
}

impl Draws {
    /// Draws seeded by `seed` mixed with `labels` and `parts` (what
    /// identifies the answer), hashed with a fixed function so that they
    /// do not vary between processes.
    pub(crate) fn seeded(seed: u64, labels: &[&str], parts: &[&Value]) -> Self {
        let mut h = Fnv::new();
        h.bytes(&seed.to_le_bytes());
        for label in labels {
            h.str(label);
        }
        for part in parts {
            h.value(part);
        }
        Self {
            rng: StdRng::seed_from_u64(h.finish()),
            seeded: true,
        }
    }

    /// Random draws.
    pub(crate) fn random() -> Self {
        Self {
            rng: rand::make_rng::<StdRng>(),
            seeded: false,
        }
    }

    /// Seeded by `seed` as [`Draws::seeded`], else random.
    pub(crate) fn new(seed: Option<u64>, labels: &[&str], parts: &[&Value]) -> Self {
        seed.map_or_else(Self::random, |s| Self::seeded(s, labels, parts))
    }
}

/// The request inputs of `call` that decide its answer: path parameters,
/// query and body (headers do not).
pub(crate) fn call_parts(call: &OperationCall) -> [Value; 3] {
    [
        Value::Object(call.path_params.clone()),
        Value::Object(call.query.clone()),
        call.body.clone().unwrap_or(Value::Null),
    ]
}

/// 64-bit FNV-1a over a canonical encoding of JSON values (object members
/// in key order, every item length-prefixed).
struct Fnv(u64);

impl Fnv {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn str(&mut self, s: &str) {
        self.bytes(&(s.len() as u64).to_le_bytes());
        self.bytes(s.as_bytes());
    }

    fn value(&mut self, v: &Value) {
        match v {
            Value::Null => self.bytes(b"n"),
            Value::Bool(b) => self.bytes(if *b { b"t" } else { b"f" }),
            Value::Number(n) => {
                self.bytes(b"#");
                self.str(&n.to_string());
            }
            Value::String(s) => {
                self.bytes(b"s");
                self.str(s);
            }
            Value::Array(items) => {
                self.bytes(b"[");
                self.bytes(&(items.len() as u64).to_le_bytes());
                for item in items {
                    self.value(item);
                }
            }
            Value::Object(map) => self.object(map),
        }
    }

    fn object(&mut self, map: &Map<String, Value>) {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        self.bytes(b"{");
        self.bytes(&(keys.len() as u64).to_le_bytes());
        for key in keys {
            self.str(key);
            if let Some(v) = map.get(key) {
                self.value(v);
            }
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngExt;
    use serde_json::json;

    fn draw(seed: u64, parts: &[&Value]) -> u64 {
        Draws::seeded(seed, &["op"], parts).rng.random()
    }

    #[test]
    fn member_order_does_not_change_the_draw() {
        let a = json!({"a": 1, "b": [true, null, "x"]});
        let b = json!({"b": [true, null, "x"], "a": 1});
        assert_eq!(draw(7, &[&a]), draw(7, &[&b]));
    }

    #[test]
    fn seed_and_inputs_change_the_draw() {
        let a = json!({"id": "1"});
        let b = json!({"id": "2"});
        assert_ne!(draw(7, &[&a]), draw(8, &[&a]));
        assert_ne!(draw(7, &[&a]), draw(7, &[&b]));
        // Length prefixes keep ["ab"] and ["a", "b"] apart.
        assert_ne!(draw(7, &[&json!(["ab"])]), draw(7, &[&json!(["a", "b"])]));
    }

    #[test]
    fn the_hash_is_fixed() {
        // FNV-1a of nothing but the seed bytes: a stable, documented value.
        let mut h = Fnv::new();
        h.bytes(&0u64.to_le_bytes());
        assert_eq!(h.finish(), 0xa8c7_f832_281a_39c5);
    }

    #[test]
    fn request_seed_reads_a_decimal_u64_in_any_case() {
        let mut call = OperationCall::default();
        assert_eq!(request_seed(&call), None);
        call.headers
            .insert("X-Mockingbird-Seed".into(), json!(" 42 "));
        assert_eq!(request_seed(&call), Some(42));
        call.headers.clear();
        call.headers.insert(SEED_HEADER.into(), json!("-1"));
        assert_eq!(request_seed(&call), None);
    }
}
