//! Usage reporting: the `UsageSink` trait is part of the semver-stable surface
//! `mockingbird-cloud` builds against.

use std::time::Duration;

use crate::Tenancy;

/// One completed data-plane call.
///
/// Build it with [`UsageEvent::new`] and assign the remaining fields; it may
/// gain fields in a minor release.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UsageEvent {
    /// Who the call is attributed to.
    pub tenancy: Tenancy,
    /// Name of the mock that served the call.
    pub mock: String,
    /// Operation id within the mock's contract.
    pub operation_id: String,
    /// Response status (HTTP status, or the protocol's equivalent).
    pub status: u16,
    /// Where the response came from (e.g. `llm`, `faker`, `scenario`).
    pub source: String,
    /// Wall-clock time spent serving the call.
    pub latency: Duration,
    /// LLM prompt tokens spent on the call.
    pub prompt_tokens: u32,
    /// LLM completion tokens spent on the call.
    pub completion_tokens: u32,
}

impl UsageEvent {
    /// An event for `operation_id` on `mock`, with status 0, an empty source,
    /// zero latency and zero tokens.
    pub fn new(tenancy: Tenancy, mock: impl Into<String>, operation_id: impl Into<String>) -> Self {
        Self {
            tenancy,
            mock: mock.into(),
            operation_id: operation_id.into(),
            status: 0,
            source: String::new(),
            latency: Duration::ZERO,
            prompt_tokens: 0,
            completion_tokens: 0,
        }
    }
}

/// Records tokens, calls and latency per tenant and mock. Called on the
/// request path, so implementations must not block.
pub trait UsageSink: Send + Sync {
    /// Records one completed call.
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
