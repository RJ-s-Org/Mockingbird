//! Spotting a reply that has fallen into a loop ([`RepetitionGuard`]).
//!
//! A model that loops writes a few short pieces over and over until
//! `max_tokens`: `0_0_0_…`, the same `"id": "…",` line hundreds of times, or
//! `"status", "status", "id", "status", …` with the pieces in no fixed order.
//! A reply cut there is lost anyway, after tens of seconds and the whole
//! token limit. The guard reads the text as it streams in and says when its
//! tail has become such a loop, so the call can be stopped early.
//!
//! The signal is how many distinct 8-byte sequences the last
//! [`WINDOW`] bytes hold. A short piece repeated back to back holds one per
//! offset in the piece (at most 200 for a 200-byte piece); a handful of such
//! pieces shuffled holds a few hundred more. Real data keeps writing bytes
//! it has not written before, so it holds thousands, even an array of
//! objects alike but for their ids (about a tenth of the window's
//! sequences are distinct there), and base64 or long numbers nearly all.
//! The guard fires at most [`MAX_DISTINCT`] (about 3% of the window). It
//! knows nothing about JSON, the API or the model.

use std::collections::HashMap;

/// The bytes of the tail the guard looks at; text shorter than this never
/// fires.
pub const WINDOW: usize = 8 * 1024;

/// The length of the byte sequences counted.
const GRAM: usize = 8;

/// The most distinct 8-byte sequences the last [`WINDOW`] bytes may
/// hold for the tail to count as a loop.
pub const MAX_DISTINCT: usize = 256;

/// Incremental loop detection over streamed text (see the [module
/// docs](self)). Memory is bounded by [`WINDOW`]; each byte costs a constant
/// amount of work.
pub struct RepetitionGuard {
    /// The last [`WINDOW`] bytes, as a ring indexed by position.
    ring: Box<[u8]>,
    /// The bytes pushed so far.
    seen: u64,
    /// The last [`GRAM`] bytes, packed.
    gram: u64,
    /// How often each [`GRAM`]-byte sequence starting in the window occurs.
    counts: HashMap<u64, u32>,
    /// Whether the guard has fired (it stays fired).
    fired: bool,
}

impl std::fmt::Debug for RepetitionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepetitionGuard")
            .field("seen", &self.seen)
            .field("distinct", &self.counts.len())
            .field("fired", &self.fired)
            .finish_non_exhaustive()
    }
}

impl Default for RepetitionGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl RepetitionGuard {
    /// A guard that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ring: vec![0; WINDOW].into_boxed_slice(),
            seen: 0,
            gram: 0,
            counts: HashMap::with_capacity(WINDOW),
            fired: false,
        }
    }

    /// Adds `text` (the next streamed delta) and says whether the tail is a
    /// loop: at least [`WINDOW`] bytes have arrived, and the last [`WINDOW`]
    /// hold at most [`MAX_DISTINCT`] distinct 8-byte sequences. Once it
    /// fires it stays fired.
    pub fn push(&mut self, text: &str) -> bool {
        if self.fired {
            return true;
        }
        for &byte in text.as_bytes() {
            self.push_byte(byte);
        }
        self.fired = self.seen >= WINDOW as u64 && self.counts.len() <= MAX_DISTINCT;
        self.fired
    }

    fn push_byte(&mut self, byte: u8) {
        // `seen` fits a usize for any text held in memory; the ring index
        // only needs it modulo the window.
        let at = (self.seen % WINDOW as u64) as usize;
        if self.seen >= WINDOW as u64 {
            // The sequence starting at the byte this one replaces leaves the
            // window.
            let leaving = (0..GRAM).fold(0u64, |acc, k| {
                (acc << 8) | u64::from(self.ring[(at + k) % WINDOW])
            });
            if let Some(count) = self.counts.get_mut(&leaving) {
                *count -= 1;
                if *count == 0 {
                    self.counts.remove(&leaving);
                }
            }
        }
        self.ring[at] = byte;
        self.gram = (self.gram << 8) | u64::from(byte);
        self.seen += 1;
        if self.seen >= GRAM as u64 {
            *self.counts.entry(self.gram).or_insert(0) += 1;
        }
    }

    /// The bytes pushed so far.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.seen
    }
}

#[cfg(test)]
mod tests {
    use super::{RepetitionGuard, WINDOW};
    use serde_json::json;

    /// A small deterministic generator (xorshift64*), so the tests need no
    /// `rand`.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> usize {
            usize::try_from(self.next() % n).unwrap()
        }
    }

    /// The byte count at which a guard fed `text` in chunks of `chunk`
    /// characters fires, if it does.
    fn fires_at(text: &str, chunk: usize) -> Option<u64> {
        let mut guard = RepetitionGuard::new();
        let chars: Vec<char> = text.chars().collect();
        for piece in chars.chunks(chunk) {
            let piece: String = piece.iter().collect();
            if guard.push(&piece) {
                return Some(guard.bytes());
            }
        }
        None
    }

    /// Fires on `text`, for any chunking, and early: within a window (and a
    /// little) of where its loop started.
    fn assert_fires(name: &str, text: &str, loop_start: usize) {
        let Some(at) = fires_at(text, 7) else {
            panic!("{name}: the guard did not fire on {} bytes", text.len());
        };
        let at = usize::try_from(at).unwrap();
        assert!(
            at <= loop_start + WINDOW + 512,
            "{name}: fired late, at byte {at} of a loop from {loop_start}"
        );
        for chunk in [1, 3, 64, 1000] {
            assert!(fires_at(text, chunk).is_some(), "{name}: chunk {chunk}");
        }
    }

    fn assert_quiet(name: &str, text: &str) {
        assert!(
            text.len() > 2 * WINDOW,
            "{name}: too short to mean anything"
        );
        for chunk in [1, 13, 4096] {
            assert_eq!(fires_at(text, chunk), None, "{name}: fired (chunk {chunk})");
        }
    }

    /// The loops recorded in a full benchmark run, rebuilt: each starts
    /// after a normal beginning and goes on until the token limit.
    #[test]
    fn fires_on_the_recorded_loops() {
        // `"id": "id:folder_2_0_0_0_…`
        let start = "{\n\"metadata\": {\n\"id\": \"id:folder_2";
        let text = format!("{start}{}", "_0".repeat(8000));
        assert_fires("0_0_0", &text, start.len());

        // The same line, again and again.
        let start = r#"{"data": [{"type": "events", "id": "evt_1", "attributes": {"#;
        let text = format!(
            "{start}{}",
            "\n\"id\": \"2026-11-04T18:29:32Z\",".repeat(1500)
        );
        assert_fires("same line", &text, start.len());

        // A few pieces in no fixed order: `"status", "status", "id", …`.
        let start = "{\n\"effects\": [\n{\"id\": \"111, \" ,\"type\": \"users\",\n\"set\": {\"firstName \",\n";
        let pieces = [
            "\"status\", ",
            "\"status\", ",
            "\"status\", ",
            "\"id\",\n",
            "\"firstName \" ,\n",
        ];
        let mut text = start.to_string();
        let mut rng = Rng(42);
        while text.len() < 50_000 {
            text.push_str(pieces[rng.below(5)]);
        }
        assert_fires("shuffled pieces", &text, start.len());

        // Whitespace, and closing punctuation, until the limit.
        let start = r#"{"id": "a1", "name": "Ledger"}"#;
        let text = format!("{start}{}", "\n \n\n \n".repeat(6000));
        assert_fires("whitespace", &text, start.len());
        let text = format!("{start}{}", "}\"".repeat(16_000));
        assert_fires("closers", &text, start.len());

        // A 200-character piece, the longest the guard is meant for.
        let mut rng = Rng(7);
        let piece: String = (0..200)
            .map(|_| char::from(b'a' + u8::try_from(rng.below(26)).unwrap()))
            .collect();
        let text = format!("{start}{}", piece.repeat(200));
        assert_fires("200-byte piece", &text, start.len());

        // Multi-byte characters.
        let text = format!("{start}{}", "ۋ  ".repeat(4000));
        assert_fires("multibyte", &text, start.len());
    }

    /// An array of objects alike but for their ids: the closest real data
    /// comes to a loop.
    #[test]
    fn quiet_on_an_array_of_objects_alike_but_for_their_ids() {
        let items: Vec<_> = (0..600)
            .map(|i| {
                json!({
                    "id": 1000 + i,
                    "status": "active",
                    "type": "widget",
                    "created_at": "2026-11-04T18:29:32Z",
                })
            })
            .collect();
        assert_quiet("compact", &serde_json::to_string(&items).unwrap());
        assert_quiet("pretty", &serde_json::to_string_pretty(&items).unwrap());
        let ids: Vec<_> = (0..900)
            .map(|i| json!({"id": format!("{}", 100 + i), "currency": "usd"}))
            .collect();
        assert_quiet(
            "short objects",
            &serde_json::to_string_pretty(&ids).unwrap(),
        );
    }

    #[test]
    fn quiet_on_realistic_records() {
        let mut rng = Rng(3);
        let names = ["Ada", "Grace", "Alan", "Edsger", "Barbara", "Donald"];
        let users: Vec<_> = (0..300)
            .map(|i| {
                json!({
                    "id": format!("usr_{:06}", rng.below(1_000_000)),
                    "name": format!("{} {i}", names[rng.below(6)]),
                    "email": format!("user{i}@example.com"),
                    "active": i % 3 != 0,
                    "score": rng.below(100),
                    "created_at": format!(
                        "2026-11-{:02}T{:02}:{:02}:00Z",
                        1 + i % 28,
                        i % 24,
                        i % 60
                    ),
                    "tags": ["billing", "vat"],
                    "address": {"city": "Lisbon", "country": "PT"},
                })
            })
            .collect();
        let text = serde_json::to_string_pretty(&json!({"data": users})).unwrap();
        assert_quiet("users", &text);
    }

    #[test]
    fn quiet_on_base64_and_long_numbers() {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut rng = Rng(11);
        let blob: String = (0..30_000)
            .map(|_| char::from(ALPHABET[rng.below(64)]))
            .collect();
        let text = json!({"name": "logo.png", "data": blob}).to_string();
        assert_quiet("base64", &text);
        let digits: String = (0..30_000)
            .map(|_| char::from(b'0' + u8::try_from(rng.below(10)).unwrap()))
            .collect();
        assert_quiet("digits", &json!({"value": digits}).to_string());
        let counting: Vec<u32> = (0..5000).collect();
        assert_quiet("counting", &json!({"ids": counting}).to_string());
    }

    /// A loop shorter than the window is left alone: the answer may still
    /// end there.
    #[test]
    fn quiet_below_the_window() {
        let text = format!("{{\"samples\": [{}0]}}", "0, ".repeat(2000));
        assert!(text.len() < WINDOW);
        assert_eq!(fires_at(&text, 5), None);
    }

    /// A repeated stretch shorter than the window inside real data does not
    /// fire: the window moves on past it.
    #[test]
    fn a_repeated_stretch_inside_real_data_is_left_alone() {
        let mut rng = Rng(5);
        let mut text = String::new();
        while text.len() < 20_000 {
            let (id, v) = (rng.below(1 << 40), rng.below(1 << 30));
            text.push_str(&format!("{{\"id\": {id}, \"v\": {v}}}, "));
        }
        text.push_str(&"0".repeat(WINDOW / 2));
        while text.len() < 40_000 {
            let id = rng.below(1 << 40);
            text.push_str(&format!("{{\"id\": {id}}}, "));
        }
        assert_eq!(fires_at(&text, 9), None);
    }
}
