//! `telemetry::init_with` installs the process-global tracing subscriber,
//! whose stderr writer libtest does not capture, so every later test in
//! the process would log through it. This test is therefore a binary of
//! its own rather than a module of `tests/it` (moved from
//! `observability.rs`).
#![cfg(test)]

use mockingbird::telemetry;

#[test]
fn otlp_disabled_without_endpoint() {
    let guard = telemetry::init_with("mockingbird-test", None).unwrap();
    assert!(!guard.otlp_enabled());
}
