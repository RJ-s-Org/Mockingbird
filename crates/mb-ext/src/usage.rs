//! Usage reporting: the `UsageSink` trait is part of the semver-stable surface
//! `mockingbird-cloud` builds against.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use metrics::SharedString;

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
    /// Where the response came from: `llm`, `repaired` or `fallback`, or
    /// `not_generated` for a call answered without generating a body
    /// (refused before generation, or a native error answer); `import` for
    /// smart-import jobs. The `UsageEvent::SOURCE_*` constants name them.
    pub source: String,
    /// Wall-clock time spent serving the call.
    pub latency: Duration,
    /// LLM prompt tokens spent on the call.
    pub prompt_tokens: u32,
    /// LLM completion tokens spent on the call.
    pub completion_tokens: u32,
}

impl UsageEvent {
    /// [`UsageEvent::source`] of an answer the LLM generated.
    pub const SOURCE_LLM: &str = "llm";
    /// [`UsageEvent::source`] of an LLM answer repaired to fit the contract.
    pub const SOURCE_REPAIRED: &str = "repaired";
    /// [`UsageEvent::source`] of a faker or offline answer.
    pub const SOURCE_FALLBACK: &str = "fallback";
    /// [`UsageEvent::source`] of a call answered without generating a body
    /// (refused before generation, or a native error answer).
    pub const SOURCE_NOT_GENERATED: &str = "not_generated";
    /// [`UsageEvent::source`] of a smart-import job.
    pub const SOURCE_IMPORT: &str = "import";

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
/// - `mb_requests_total{mock,operation,status,source}` counter; `source` is
///   `llm`, `repaired`, `fallback`, or `not_generated` for a call answered
///   without generating a body (refused before generation, or a native
///   error answer)
/// - `mb_request_duration_seconds{mock,operation}` histogram
/// - `mb_llm_tokens_total{mock,kind="prompt"|"completion"}` counter
#[derive(Debug, Clone, Copy, Default)]
pub struct PrometheusUsage;

impl UsageSink for PrometheusUsage {
    fn record(&self, event: &UsageEvent) {
        // One shared copy of each label value, however many series use it.
        let mock: Arc<str> = Arc::from(event.mock.as_str());
        let operation: Arc<str> = Arc::from(event.operation_id.as_str());
        metrics::counter!(
            "mb_requests_total",
            "mock" => mock.clone(),
            "operation" => operation.clone(),
            "status" => status_label(event.status),
            "source" => source_label(&event.source),
        )
        .increment(1);
        metrics::histogram!(
            "mb_request_duration_seconds",
            "mock" => mock.clone(),
            "operation" => operation,
        )
        .record(event.latency.as_secs_f64());
        if event.prompt_tokens > 0 {
            metrics::counter!(
                "mb_llm_tokens_total",
                "mock" => mock.clone(),
                "kind" => "prompt",
            )
            .increment(u64::from(event.prompt_tokens));
        }
        if event.completion_tokens > 0 {
            metrics::counter!(
                "mb_llm_tokens_total",
                "mock" => mock,
                "kind" => "completion",
            )
            .increment(u64::from(event.completion_tokens));
        }
    }
}

/// The statuses whose label text is kept, shared, for the life of the
/// process: every HTTP status.
const STATUS_LABELS: std::ops::Range<u16> = 100..600;

/// `status` as label text, without formatting it for an HTTP status.
fn status_label(status: u16) -> SharedString {
    static LABELS: LazyLock<Vec<Arc<str>>> =
        LazyLock::new(|| STATUS_LABELS.map(|s| Arc::from(s.to_string())).collect());
    status
        .checked_sub(STATUS_LABELS.start)
        .and_then(|i| LABELS.get(usize::from(i)))
        .map_or_else(
            || SharedString::from(status.to_string()),
            |label| SharedString::from_shared(label.clone()),
        )
}

/// `source` as label text, without copying one of the sources Mockingbird
/// reports.
fn source_label(source: &str) -> SharedString {
    match source {
        UsageEvent::SOURCE_LLM => SharedString::const_str(UsageEvent::SOURCE_LLM),
        UsageEvent::SOURCE_REPAIRED => SharedString::const_str(UsageEvent::SOURCE_REPAIRED),
        UsageEvent::SOURCE_FALLBACK => SharedString::const_str(UsageEvent::SOURCE_FALLBACK),
        UsageEvent::SOURCE_NOT_GENERATED => {
            SharedString::const_str(UsageEvent::SOURCE_NOT_GENERATED)
        }
        UsageEvent::SOURCE_IMPORT => SharedString::const_str(UsageEvent::SOURCE_IMPORT),
        other => SharedString::from(other.to_string()),
    }
}
