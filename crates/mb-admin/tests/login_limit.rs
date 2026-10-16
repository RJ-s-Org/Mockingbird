//! The `auth.login_failed` audit rate limit.
#![allow(clippy::unwrap_used)]

use std::net::IpAddr;
use std::time::{Duration, Instant};

use mb_admin::{LOGIN_FAILED_SUPPRESSED_METRIC, LoginFailureLimiter};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

fn ip(n: u32) -> Option<IpAddr> {
    Some(IpAddr::from(n.to_be_bytes()))
}

#[test]
fn one_per_source_per_ten_seconds() {
    let l = LoginFailureLimiter::new();
    let t0 = Instant::now();
    assert!(l.allow_at(ip(1), t0));
    assert!(!l.allow_at(ip(1), t0 + Duration::from_secs(9)));
    // Another source is independent.
    assert!(l.allow_at(ip(2), t0 + Duration::from_secs(1)));
    assert!(l.allow_at(ip(1), t0 + Duration::from_secs(10)));
    // Unknown sources share one bucket.
    assert!(l.allow_at(None, t0 + Duration::from_secs(10)));
    assert!(!l.allow_at(None, t0 + Duration::from_secs(11)));
}

#[test]
fn at_most_one_hundred_per_minute_overall() {
    let l = LoginFailureLimiter::new();
    let t0 = Instant::now();
    for n in 0..100 {
        assert!(l.allow_at(ip(n), t0 + Duration::from_millis(u64::from(n))));
    }
    assert!(!l.allow_at(ip(1000), t0 + Duration::from_secs(30)));
    assert!(!l.allow_at(ip(1001), t0 + Duration::from_millis(59_999)));
    // The first one leaves the window after 60 s.
    assert!(l.allow_at(ip(1002), t0 + Duration::from_secs(60)));
    assert!(!l.allow_at(ip(1003), t0 + Duration::from_secs(60)));
}

#[test]
fn suppressed_failures_are_counted() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let l = LoginFailureLimiter::new();
    let t0 = Instant::now();
    metrics::with_local_recorder(&recorder, || {
        assert!(l.allow_at(ip(1), t0));
        for s in 1..=3 {
            assert!(!l.allow_at(ip(1), t0 + Duration::from_secs(s)));
        }
    });
    let counted: Vec<_> = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(key, ..)| key.key().name() == LOGIN_FAILED_SUPPRESSED_METRIC)
        .map(|(.., value)| value)
        .collect();
    assert_eq!(counted, vec![DebugValue::Counter(3)]);
}
