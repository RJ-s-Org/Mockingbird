//! Per-mock admission limits: a token-bucket rate limit and the request
//! size cap (spec §11.3).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

/// Counter of calls refused by a mock's rate limit, labelled by `mock`.
pub const RATE_LIMITED_METRIC: &str = "mb_rate_limited_total";

/// The default request size cap (1 MiB), when no limit is configured.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// The most query pairs, or form or multipart fields, an HTTP request may
/// carry. They are decoded before admission and auth, so a request with
/// more is refused with a 400 problem before any further work.
pub const MAX_REQUEST_PARAMS: usize = 1_000;

/// A token bucket: `burst` tokens, refilled at `requests_per_second`.
#[derive(Debug)]
pub struct RateLimiter {
    requests_per_second: f64,
    burst: u32,
    bucket: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// A full bucket of `burst` tokens (at least one) refilled at
    /// `requests_per_second` (a non-positive or non-finite rate never
    /// refills).
    pub fn new(requests_per_second: f64, burst: u32) -> Self {
        let burst = burst.max(1);
        let requests_per_second = if requests_per_second.is_finite() {
            requests_per_second.max(0.0)
        } else {
            0.0
        };
        Self {
            requests_per_second,
            burst,
            bucket: Mutex::new(Bucket {
                tokens: f64::from(burst),
                last: Instant::now(),
            }),
        }
    }

    /// The sustained rate.
    pub fn requests_per_second(&self) -> f64 {
        self.requests_per_second
    }

    /// The bucket size.
    pub fn burst(&self) -> u32 {
        self.burst
    }

    /// Takes one token, or answers how long until one is available.
    ///
    /// # Errors
    ///
    /// The wait until the next token, when the bucket is empty.
    pub fn try_acquire(&self) -> Result<(), Duration> {
        let mut bucket = self.bucket.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens =
            (bucket.tokens + elapsed * self.requests_per_second).min(f64::from(self.burst));
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        let deficit = 1.0 - bucket.tokens;
        if self.requests_per_second <= 0.0 {
            return Err(Duration::MAX);
        }
        Err(
            Duration::try_from_secs_f64(deficit / self.requests_per_second)
                .unwrap_or(Duration::MAX),
        )
    }
}

/// A mock's admission limits as the adapters apply them.
#[derive(Clone, Debug)]
pub struct RuntimeLimits {
    /// The mock's rate limit; `None` admits every call.
    pub rate: Option<Arc<RateLimiter>>,
    /// The largest request body (or message) accepted, in bytes.
    pub max_request_bytes: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            rate: None,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn bucket_allows_burst_then_refills() {
        let limiter = RateLimiter::new(2.0, 3);
        for _ in 0..3 {
            assert!(limiter.try_acquire().is_ok());
        }
        assert!(limiter.try_acquire().is_err());
        // Half a second refills one token at 2/s.
        tokio::time::advance(Duration::from_millis(500)).await;
        assert!(limiter.try_acquire().is_ok());
        assert!(limiter.try_acquire().is_err());
        // A long pause refills to the burst, never past it.
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..3 {
            assert!(limiter.try_acquire().is_ok());
        }
        assert!(limiter.try_acquire().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_matches_deficit() {
        let limiter = RateLimiter::new(4.0, 1);
        assert!(limiter.try_acquire().is_ok());
        // Empty bucket: one token at 4/s is 250 ms away.
        assert_eq!(limiter.try_acquire(), Err(Duration::from_millis(250)));
        tokio::time::advance(Duration::from_millis(100)).await;
        let Err(wait) = limiter.try_acquire() else {
            panic!("the bucket should be empty");
        };
        assert!(
            (wait.as_secs_f64() - 0.15).abs() < 1e-9,
            "expected 150ms, got {wait:?}"
        );
        tokio::time::advance(wait).await;
        assert!(limiter.try_acquire().is_ok());
        assert_eq!(limiter.requests_per_second(), 4.0);
        assert_eq!(limiter.burst(), 1);
    }
}
