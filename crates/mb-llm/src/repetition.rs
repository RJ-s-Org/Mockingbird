//! Spotting a reply that has fallen into a loop ([`RepetitionGuard`]).
//!
//! A model that loops writes a few short pieces over and over until
//! `max_tokens`: `0_0_0_…`, the same `"id": "…",` line hundreds of times, or
//! `"status", "status", "id", "status", …` with the pieces in no fixed order.
//! A reply cut there is lost anyway, after tens of seconds and the whole
//! token limit. The guard reads the text as it streams in and says when it
//! has become such a loop, so the call can be stopped early.
//!
//! The signal is how many distinct 8-byte sequences an 8 KiB window
//! ([`WINDOW`]) holds. A short piece repeated back to back holds one per
//! offset in the piece (at most 200 for a 200-byte piece); a handful of such
//! pieces shuffled holds a few hundred more. Most real data keeps writing
//! bytes it has not written before, so it holds thousands, even an array of
//! objects alike but for their ids (about a tenth of the window's
//! sequences are distinct there), and base64 or long numbers nearly all.
//! A window holding at most [`MAX_DISTINCT`] (about 3%) is of low variety.
//!
//! Some real answers are of low variety too: a seat map, an array of a few
//! enum values, a bitmap of booleans, placeholder items. They end, though,
//! and a loop runs on to `max_tokens` (50–60 KB). So the guard fires only
//! once the text has been of low variety throughout a sustained span
//! ([`DEFAULT_SPAN`], 32 KiB, by default): every window ending in its last
//! `span − WINDOW` bytes held at most [`MAX_DISTINCT`]. It knows nothing
//! about JSON, the API or the model.

use std::collections::HashMap;

/// The bytes one variety count looks at; text shorter than this never
/// fires.
pub const WINDOW: usize = 8 * 1024;

/// How long the text must have been of low variety for the guard to fire,
/// by default.
pub const DEFAULT_SPAN: usize = 32 * 1024;

/// The length of the byte sequences counted.
const GRAM: usize = 8;

/// The most distinct 8-byte sequences a [`WINDOW`] may hold to be of low
/// variety.
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
    /// Where the low-variety stretch the text ends in starts (the start of
    /// its first low-variety window); `None` when the last window was not.
    low_from: Option<u64>,
    /// How long that stretch must be to fire.
    span: u64,
    /// Whether the guard has fired (it stays fired).
    fired: bool,
}

impl std::fmt::Debug for RepetitionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepetitionGuard")
            .field("seen", &self.seen)
            .field("distinct", &self.counts.len())
            .field("low_from", &self.low_from)
            .field("span", &self.span)
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
    /// A guard that has seen nothing, with the [`DEFAULT_SPAN`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_span(DEFAULT_SPAN)
    }

    /// A guard that fires once the text has been of low variety for `span`
    /// bytes (at least [`WINDOW`]).
    #[must_use]
    pub fn with_span(span: usize) -> Self {
        Self {
            ring: vec![0; WINDOW].into_boxed_slice(),
            seen: 0,
            gram: 0,
            counts: HashMap::with_capacity(WINDOW),
            low_from: None,
            span: span.max(WINDOW) as u64,
            fired: false,
        }
    }

    /// The span it fires after, in bytes.
    #[must_use]
    pub fn span(&self) -> usize {
        usize::try_from(self.span).unwrap_or(usize::MAX)
    }

    /// Adds `text` (the next streamed delta) and says whether the text is a
    /// loop: it has been of low variety (at most [`MAX_DISTINCT`] distinct
    /// 8-byte sequences per [`WINDOW`]) throughout its last
    /// [`Self::span`] bytes. Once it fires it stays fired.
    pub fn push(&mut self, text: &str) -> bool {
        for &byte in text.as_bytes() {
            if self.fired {
                break;
            }
            self.push_byte(byte);
        }
        self.fired
    }

    fn push_byte(&mut self, byte: u8) {
        // The ring index only needs `seen` modulo the window.
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
        if self.seen < WINDOW as u64 {
            return;
        }
        if self.counts.len() <= MAX_DISTINCT {
            let from = *self.low_from.get_or_insert(self.seen - WINDOW as u64);
            self.fired = self.seen - from >= self.span;
        } else {
            self.low_from = None;
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
    use super::{DEFAULT_SPAN, RepetitionGuard, WINDOW};
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
        fires_at_with(RepetitionGuard::new(), text, chunk)
    }

    fn fires_at_with(mut guard: RepetitionGuard, text: &str, chunk: usize) -> Option<u64> {
        let chars: Vec<char> = text.chars().collect();
        for piece in chars.chunks(chunk) {
            let piece: String = piece.iter().collect();
            if guard.push(&piece) {
                return Some(guard.bytes());
            }
        }
        None
    }

    /// Fires on `text`, for any chunking, and early: within the sustained
    /// span (and a little) of where its loop started.
    fn assert_fires(name: &str, text: &str, loop_start: usize) {
        let Some(at) = fires_at(text, 7) else {
            panic!("{name}: the guard did not fire on {} bytes", text.len());
        };
        let at = usize::try_from(at).unwrap();
        assert!(
            at <= loop_start + DEFAULT_SPAN + 512,
            "{name}: fired late, at byte {at} of a loop from {loop_start}"
        );
        for chunk in [1, 3, 64, 1000] {
            assert!(fires_at(text, chunk).is_some(), "{name}: chunk {chunk}");
        }
    }

    fn assert_quiet(name: &str, text: &str) {
        assert!(text.len() > WINDOW, "{name}: too short to mean anything");
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
        let text = format!("{start}{}", "_0".repeat(30_000));
        assert_fires("0_0_0", &text, start.len());

        // The same line, again and again.
        let start = r#"{"data": [{"type": "events", "id": "evt_1", "attributes": {"#;
        let text = format!(
            "{start}{}",
            "\n\"id\": \"2026-11-04T18:29:32Z\",".repeat(2000)
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
        while text.len() < 60_000 {
            text.push_str(pieces[rng.below(5)]);
        }
        assert_fires("shuffled pieces", &text, start.len());

        // Whitespace, and closing punctuation, until the limit.
        let start = r#"{"id": "a1", "name": "Ledger"}"#;
        let text = format!("{start}{}", "\n \n\n \n".repeat(12_000));
        assert_fires("whitespace", &text, start.len());
        let text = format!("{start}{}", "}\"".repeat(30_000));
        assert_fires("closers", &text, start.len());

        // A 200-character piece, the longest the guard is meant for.
        let mut rng = Rng(7);
        let piece: String = (0..200)
            .map(|_| char::from(b'a' + u8::try_from(rng.below(26)).unwrap()))
            .collect();
        let text = format!("{start}{}", piece.repeat(300));
        assert_fires("200-byte piece", &text, start.len());

        // Multi-byte characters.
        let text = format!("{start}{}", "ۋ  ".repeat(12_000));
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

    /// Real answers of little variety, found by review: each holds fewer
    /// than 256 distinct 8-byte sequences per 8 KiB, but none runs on for
    /// the sustained span.
    fn low_variety_answers() -> Vec<(&'static str, String)> {
        let mut rng = Rng(1);
        let seats: Vec<_> = (0..30)
            .map(|r| {
                let seats: Vec<_> = (0..30)
                    .map(|_| ["available", "taken"][rng.below(2)])
                    .collect();
                json!({"row": r, "seats": seats})
            })
            .collect();
        let statuses: Vec<_> = (0..2000)
            .map(|_| ["active", "inactive", "pending"][rng.below(3)])
            .collect();
        let bitmap: Vec<_> = (0..2000).map(|_| rng.below(2) == 1).collect();
        let placeholders: Vec<_> = (0..150)
            .map(|_| json!({"name": "", "description": "", "value": null}))
            .collect();
        vec![
            ("seat map", json!({"rows": seats}).to_string()),
            ("enum array", json!({"statuses": statuses}).to_string()),
            ("bitmap", json!({"availability": bitmap}).to_string()),
            (
                "placeholder items",
                serde_json::to_string_pretty(&json!({"items": placeholders})).unwrap(),
            ),
        ]
    }

    #[test]
    fn quiet_on_low_variety_answers_shorter_than_the_span() {
        for (name, text) in low_variety_answers() {
            assert_quiet(name, &text);
        }
    }

    /// The span is what tells them from a loop: with an 8 KiB span they
    /// would fire.
    #[test]
    fn the_sustained_span_is_configurable() {
        for (name, text) in low_variety_answers() {
            let at = fires_at_with(RepetitionGuard::with_span(WINDOW), &text, 64);
            assert!(at.is_some(), "{name}");
        }
        assert_eq!(RepetitionGuard::with_span(1).span(), WINDOW);
        assert_eq!(RepetitionGuard::new().span(), DEFAULT_SPAN);
    }

    /// Variety coming back resets the span: low-variety stretches with real
    /// data between them never add up.
    #[test]
    fn low_variety_must_hold_throughout_the_span() {
        let mut rng = Rng(9);
        let mut text = String::new();
        for _ in 0..6 {
            text.push_str(&"0_".repeat(10_000));
            let end = text.len() + 9_000;
            while text.len() < end {
                let (id, v) = (rng.below(1 << 40), rng.below(1 << 30));
                text.push_str(&format!("{{\"id\": {id}, \"v\": {v}}}, "));
            }
        }
        assert!(text.len() > 100_000);
        assert_eq!(fires_at(&text, 11), None);
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
