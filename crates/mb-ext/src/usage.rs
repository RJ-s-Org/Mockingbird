//! Usage reporting: the `UsageSink` trait is part of the semver-stable surface
//! `mockingbird-cloud` builds against.

use std::time::Duration;

use crate::Tenancy;

/// One completed data-plane call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEvent {
    pub tenancy: Tenancy,
    pub mock: String,
    pub operation_id: String,
    pub status: u16,
    /// Where the response came from (e.g. `llm`, `faker`, `scenario`).
    pub source: String,
    pub latency: Duration,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// Records tokens, calls and latency per tenant and mock. Called on the
/// request path, so implementations must not block.
pub trait UsageSink: Send + Sync {
    fn record(&self, event: &UsageEvent);
}

/// Self-hosted default: emits through the `metrics` facade (exported as Prometheus).
///
/// - `mb_requests_total{mock,operation,status,source}` counter
/// - `mb_request_duration_seconds{mock,operation}` histogram
/// - `mb_llm_tokens_total{mock,kind="prompt"|"completion"}` counter
#[derive(Debug, Clone, Copy, Default)]
pub struct PrometheusUsage;

impl UsageSink for PrometheusUsage {
    fn record(&self, event: &UsageEvent) {
        metrics::counter!(
            "mb_requests_total",
            "mock" => event.mock.clone(),
            "operation" => event.operation_id.clone(),
            "status" => event.status.to_string(),
            "source" => event.source.clone(),
        )
        .increment(1);
        metrics::histogram!(
            "mb_request_duration_seconds",
            "mock" => event.mock.clone(),
            "operation" => event.operation_id.clone(),
        )
        .record(event.latency.as_secs_f64());
        if event.prompt_tokens > 0 {
            metrics::counter!(
                "mb_llm_tokens_total",
                "mock" => event.mock.clone(),
                "kind" => "prompt",
            )
            .increment(u64::from(event.prompt_tokens));
        }
        if event.completion_tokens > 0 {
            metrics::counter!(
                "mb_llm_tokens_total",
                "mock" => event.mock.clone(),
                "kind" => "completion",
            )
            .increment(u64::from(event.completion_tokens));
        }
    }
}
