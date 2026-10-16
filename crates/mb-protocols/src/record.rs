//! Per-call request records, handed to a [`RequestRecorder`] after each
//! response is built (the request log, spec §14).

use std::time::Duration;

use serde_json::Value;

/// What kind of call a [`RequestRecord`] records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    Http,
    Graphql,
    Grpc,
    Soap,
    /// A produced or consumed event-channel message.
    Event,
}

impl RequestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestKind::Http => "http",
            RequestKind::Graphql => "graphql",
            RequestKind::Grpc => "grpc",
            RequestKind::Soap => "soap",
            RequestKind::Event => "event",
        }
    }
}

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
    /// Byte length of the raw request body, when known, so a recorder can
    /// bound a large body without serializing it.
    pub request_body_bytes: Option<usize>,
    /// The response body, when it is JSON.
    pub response: Option<Value>,
    pub kind: RequestKind,
}

/// Sink for [`RequestRecord`]s. Called on the request path, so it must not
/// block or do heavy work (serializing, redacting): implementors hand the
/// record to a background task.
pub trait RequestRecorder: Send + Sync {
    fn record(&self, r: RequestRecord);
}
