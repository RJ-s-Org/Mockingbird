//! Fault injection (spec §7.1): per mock, or per scenario while it applies,
//! an error rate, a latency jitter and a stream drop rate.
//!
//! `call::plan` asks the applying scenario's [`ChaosRuntime`]
//! (else the mock's) for one [`ChaosDecision`] per call. An injected error
//! uses the operation's first declared status of `503`, `500`, the smallest
//! other `5xx`, or `429`; when none is declared, each adapter answers its
//! protocol's native "unavailable" (REST `503` problem, GraphQL
//! `UNAVAILABLE` field error, gRPC `UNAVAILABLE`, SOAP Server/Receiver
//! Fault with HTTP `503`). A status already forced by a header or the
//! scenario wins: chaos then injects no error.

use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use mb_mcm::{Operation, StatusSelector};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

/// Request header bypassing chaos (`off`, when header overrides are
/// enabled); response header (`error`) marking an injected error. gRPC
/// carries it as metadata.
pub const CHAOS_HEADER: &str = "x-mockingbird-chaos";

/// Environment variable seeding every chaos random generator, for
/// reproducible runs.
pub const CHAOS_SEED_ENV: &str = "MB_CHAOS_SEED";

/// Counter of injected faults, labelled `mock` and `kind`
/// (`error`, `latency` or `drop`).
pub const CHAOS_METRIC: &str = "mb_chaos_injected_total";

/// The detail (REST, SOAP, gRPC) or message (GraphQL) of a protocol-native
/// injected error.
pub const CHAOS_ERROR_DETAIL: &str = "injected by chaos";

/// The gRPC message and GraphQL error message of a dropped stream.
pub const STREAM_DROPPED: &str = "stream dropped by chaos";

/// One mock's (or scenario's) fault injection settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChaosSpec {
    /// Probability (0–1) that a call answers with an injected error.
    pub error_rate: f64,
    /// Bounds of a uniformly random latency added to every call.
    pub latency_jitter: Option<(Duration, Duration)>,
    /// Probability (0–1) that a streamed response is cut short.
    pub drop_stream_rate: f64,
}

/// What chaos does to one call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChaosDecision {
    /// Answer with an injected error instead of a generated response.
    pub inject_error: bool,
    /// Latency added to the planned delay (the total is capped at 30 s).
    pub extra_latency: Duration,
    /// Streams only: items sent before the stream is dropped.
    pub drop_after: Option<usize>,
}

/// [`ChaosSpec`] with its random generator.
pub struct ChaosRuntime {
    pub settings: ChaosSpec,
    rng: Mutex<StdRng>,
}

impl fmt::Debug for ChaosRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChaosRuntime")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl ChaosRuntime {
    /// Settings `spec`, drawing from a generator seeded with `seed`, else
    /// from the operating system.
    pub fn new(spec: ChaosSpec, seed: Option<u64>) -> Self {
        let rng = match seed {
            Some(seed) => StdRng::seed_from_u64(seed),
            None => rand::make_rng::<StdRng>(),
        };
        Self {
            settings: spec,
            rng: Mutex::new(rng),
        }
    }

    /// The seed in [`CHAOS_SEED_ENV`], when it is set to an integer.
    pub fn seed_from_env() -> Option<u64> {
        std::env::var(CHAOS_SEED_ENV).ok()?.trim().parse().ok()
    }

    /// The faults of one call. `streaming` calls of `planned_items` items
    /// may be dropped after `0..planned_items` of them.
    pub fn decide(&self, streaming: bool, planned_items: usize) -> ChaosDecision {
        let s = &self.settings;
        let mut rng = self.rng.lock().unwrap_or_else(|p| p.into_inner());
        let inject_error = s.error_rate > 0.0 && rng.random::<f64>() < s.error_rate;
        let extra_latency = match s.latency_jitter {
            Some((min, max)) if max > min => {
                let (lo, hi) = (nanos(min), nanos(max));
                Duration::from_nanos(rng.random_range(lo..=hi))
            }
            Some((min, _)) => min,
            None => Duration::ZERO,
        };
        let drop_after = (streaming
            && planned_items > 0
            && s.drop_stream_rate > 0.0
            && rng.random::<f64>() < s.drop_stream_rate)
            .then(|| rng.random_range(0..planned_items));
        ChaosDecision {
            inject_error,
            extra_latency,
            drop_after,
        }
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// The status of an error injected into `op`: its first declared status of
/// `503`, `500`, the smallest other `5xx`, or `429`. `None` when it
/// declares none of them (the protocol-native answer applies).
pub(crate) fn error_status(op: &Operation) -> Option<u16> {
    let declared: Vec<u16> = op
        .outputs
        .iter()
        .filter_map(|o| match o.status {
            StatusSelector::Code(c) => Some(c),
            StatusSelector::Default => None,
        })
        .collect();
    let has = |c: u16| declared.contains(&c);
    [503, 500]
        .into_iter()
        .find(|c| has(*c))
        .or_else(|| {
            declared
                .iter()
                .copied()
                .filter(|c| (500..600).contains(c))
                .min()
        })
        .or_else(|| has(429).then_some(429))
}

/// Counts one injected fault of `kind` (`error`, `latency` or `drop`).
pub(crate) fn count(mock: &str, kind: &'static str) {
    metrics::counter!(CHAOS_METRIC, "mock" => mock.to_string(), "kind" => kind).increment(1);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn spec(error_rate: f64, drop_stream_rate: f64) -> ChaosSpec {
        ChaosSpec {
            error_rate,
            latency_jitter: None,
            drop_stream_rate,
        }
    }

    #[test]
    fn zero_rate_never_injects() {
        let chaos = ChaosRuntime::new(spec(0.0, 0.0), None);
        for _ in 0..10_000 {
            assert_eq!(chaos.decide(true, 5), ChaosDecision::default());
        }
    }

    #[test]
    fn full_rate_always_injects() {
        let chaos = ChaosRuntime::new(spec(1.0, 1.0), None);
        for _ in 0..10_000 {
            let d = chaos.decide(true, 5);
            assert!(d.inject_error);
            assert!(d.drop_after.is_some_and(|n| n < 5), "{d:?}");
        }
        // Only streams are dropped, and only when they plan any item.
        assert_eq!(chaos.decide(false, 5).drop_after, None);
        assert_eq!(chaos.decide(true, 0).drop_after, None);
    }

    #[test]
    fn seeded_half_rate_is_deterministic_and_near_half() {
        let run = || {
            let chaos = ChaosRuntime::new(spec(0.5, 0.5), Some(42));
            (0..10_000)
                .map(|_| chaos.decide(true, 10))
                .collect::<Vec<_>>()
        };
        let first = run();
        assert_eq!(first, run());
        let errors = first.iter().filter(|d| d.inject_error).count();
        let drops = first.iter().filter(|d| d.drop_after.is_some()).count();
        assert!((4_500..=5_500).contains(&errors), "{errors}");
        assert!((4_500..=5_500).contains(&drops), "{drops}");
    }

    #[test]
    fn jitter_within_bounds() {
        let (min, max) = (Duration::from_millis(10), Duration::from_millis(20));
        let chaos = ChaosRuntime::new(
            ChaosSpec {
                latency_jitter: Some((min, max)),
                ..ChaosSpec::default()
            },
            Some(7),
        );
        let draws: Vec<Duration> = (0..1_000)
            .map(|_| chaos.decide(false, 0).extra_latency)
            .collect();
        assert!(draws.iter().all(|d| (min..=max).contains(d)), "{draws:?}");
        assert!(draws.iter().any(|d| *d != draws[0]), "not uniform");

        let fixed = ChaosRuntime::new(
            ChaosSpec {
                latency_jitter: Some((min, min)),
                ..ChaosSpec::default()
            },
            None,
        );
        assert_eq!(fixed.decide(false, 0).extra_latency, min);
    }

    #[test]
    fn error_status_prefers_503_then_500_then_5xx_then_429() {
        let op = |codes: &[u16]| {
            let bundle = mb_import::compile(
                format!(
                    "openapi: 3.0.3\ninfo: {{title: T, version: '1'}}\npaths:\n  /t:\n    get:\n      operationId: t\n      responses:\n{}",
                    codes
                        .iter()
                        .map(|c| format!("        \"{c}\": {{description: x}}\n"))
                        .collect::<String>()
                )
                .as_bytes(),
            )
            .unwrap();
            bundle.operations[0].clone()
        };
        assert_eq!(error_status(&op(&[200, 429, 500, 503])), Some(503));
        assert_eq!(error_status(&op(&[200, 429, 502, 500])), Some(500));
        assert_eq!(error_status(&op(&[200, 429, 504, 502])), Some(502));
        assert_eq!(error_status(&op(&[200, 429])), Some(429));
        assert_eq!(error_status(&op(&[200, 404])), None);
    }
}
