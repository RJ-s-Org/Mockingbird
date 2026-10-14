//! Per-call request records, handed to a [`RequestRecorder`] after each
//! response is built (the request log, spec §14).

use std::time::Duration;

use serde_json::Value;

/// One served data-plane call.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestRecord {
    pub mock: String,
    /// `None` when no operation matched (404/405).
    pub operation_id: Option<String>,
    pub session: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    /// `X-Mockingbird-Source` of a generated response; `None` for problems.
    pub source: Option<String>,
    /// Time to build the response, plus any injected latency.
    pub latency: Duration,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// `{query, headers, body}`; sensitive headers are left out.
    pub request: Value,
    /// The response body, when it is JSON.
    pub response: Option<Value>,
}

/// Sink for [`RequestRecord`]s. Called on the request path, so it must not
/// block: implementors that do I/O spawn it.
pub trait RequestRecorder: Send + Sync {
    fn record(&self, r: RequestRecord);
}
