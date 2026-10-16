//! `/api/v1/workspaces/{ws}/mocks/{mock}/requests/stream`: the live request
//! tail over server-sent events (spec §14).
//!
//! Each served data-plane call of the mock that passes the query filters is
//! sent as `event: request` with a [`LiveRequestView`]. A subscriber that
//! falls behind the feed gets `event: lagged` with `{"dropped": N}` and the
//! stream continues with the records still buffered.
//!
//! The caller is authenticated only once by the middleware, so an open
//! stream re-runs authentication with the request's own headers and
//! re-checks Read on the workspace and that the mock still exists every
//! [`AdminState::live_recheck`]. When any check fails (a revoked or expired
//! key, an ended session, a lost role, a deleted workspace or mock) the
//! stream ends.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_protocols::RequestRecord;
use mb_store::RequestKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::access::WsAccess;
use crate::mocks::MockPath;
use crate::problem::{ApiResult, Path, Problem, Query};

/// Subscribes to the feed of served data-plane calls (every workspace and
/// mock).
pub type LiveSubscribe = Arc<dyn Fn() -> broadcast::Receiver<RequestRecord> + Send + Sync>;

/// How often an idle stream sends a keep-alive comment.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct StreamQuery {
    /// Only this operation id.
    pub operation: Option<String>,
    /// Only this session id.
    pub session: Option<String>,
    /// Only this response source, e.g. `llm`, `repaired`, `fallback`.
    pub source: Option<String>,
    /// Only this request kind: `http`, `graphql`, `grpc`, `soap` or `event`.
    pub kind: Option<String>,
}

/// [`StreamQuery`] with its `kind` parsed (the request log's kinds, which
/// share their wire names with the data plane's).
#[derive(Debug)]
struct StreamFilter {
    query: StreamQuery,
    kind: Option<RequestKind>,
}

impl StreamFilter {
    fn new(query: StreamQuery) -> Result<Self, Problem> {
        let kind = query
            .kind
            .as_deref()
            .map(|k| {
                RequestKind::parse(k)
                    .ok_or_else(|| Problem::bad_request(format!("unknown request kind {k:?}")))
            })
            .transpose()?;
        Ok(Self { query, kind })
    }

    fn matches(&self, r: &RequestRecord) -> bool {
        self.kind.is_none_or(|k| r.kind.as_str() == k.as_str()) && self.query.matches(r)
    }
}

impl StreamQuery {
    fn matches(&self, r: &RequestRecord) -> bool {
        fn eq(want: &Option<String>, got: Option<&str>) -> bool {
            want.as_deref().is_none_or(|w| got == Some(w))
        }
        eq(&self.operation, r.operation_id.as_deref())
            && eq(&self.session, Some(&r.session))
            && eq(&self.source, r.source.as_deref())
    }
}

/// One served call, as sent in an `event: request`.
#[derive(Serialize, ToSchema, Debug)]
pub struct LiveRequestView {
    pub workspace: String,
    pub mock: String,
    pub operation_id: Option<String>,
    pub session: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub source: Option<String>,
    pub latency_ms: u64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[schema(value_type = Object)]
    pub request: Value,
    #[schema(value_type = Option<Object>)]
    pub response: Option<Value>,
    /// Request kind: `http`, `graphql`, `grpc`, `soap` or `event`.
    pub kind: String,
    /// When the record was forwarded to this subscriber.
    pub at: DateTime<Utc>,
}

impl From<RequestRecord> for LiveRequestView {
    fn from(r: RequestRecord) -> Self {
        Self {
            workspace: r.workspace,
            mock: r.mock,
            operation_id: r.operation_id,
            session: r.session,
            method: r.method,
            path: r.path,
            status: r.status,
            source: r.source,
            latency_ms: u64::try_from(r.latency.as_millis()).unwrap_or(u64::MAX),
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            request: r.request,
            response: r.response,
            kind: r.kind.as_str().to_string(),
            at: Utc::now(),
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/requests/stream",
    tag = "requests",
    description = "Server-sent events: `event: request` with one served call per event, `event: lagged` with `{\"dropped\": N}` when this subscriber fell behind; a keep-alive comment every 15 s.",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        StreamQuery,
    ),
    responses(
        (status = 200, description = "An SSE stream of served calls", body = LiveRequestView, content_type = "text/event-stream"),
        (status = 400, description = "Unknown request kind", body = Problem, content_type = "application/problem+json"),
        (status = 403, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "No live feed", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn stream(
    State(st): State<AdminState>,
    access: WsAccess,
    headers: HeaderMap,
    Path(MockPath { mock }): Path<MockPath>,
    Query(q): Query<StreamQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let filter = StreamFilter::new(q)?;
    let subscribe = st.live.as_ref().ok_or_else(|| {
        Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "the live request feed is not available",
        )
    })?;
    let feed = subscribe();
    let (out, events) = mpsc::channel(16);
    tokio::spawn(forward(st, headers, access.ws, m.name, filter, feed, out));
    Ok(Sse::new(ReceiverStream::new(events)).keep_alive(KeepAlive::new().interval(KEEP_ALIVE)))
}

/// Forwards the matching records of `feed` to `out` until the subscriber
/// disconnects, the feed closes, or a periodic [`still_allowed`] check fails.
async fn forward(
    st: AdminState,
    headers: HeaderMap,
    ws: String,
    mock: String,
    q: StreamFilter,
    mut feed: broadcast::Receiver<RequestRecord>,
    out: mpsc::Sender<Result<Event, Infallible>>,
) {
    let period = st.live_recheck.max(Duration::from_millis(10));
    let mut recheck = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let event = tokio::select! {
            () = out.closed() => return,
            _ = recheck.tick() => {
                if still_allowed(&st, &headers, &ws, &mock).await {
                    continue;
                }
                tracing::debug!(workspace = %ws, mock = %mock, "ending a live tail: the caller lost access");
                return;
            }
            item = feed.recv() => match item {
                Ok(r) if r.workspace == ws && r.mock == mock && q.matches(&r) => {
                    match request_event(LiveRequestView::from(r)) {
                        Some(event) => event,
                        None => continue,
                    }
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(dropped)) => Ok(Event::default()
                    .event("lagged")
                    .data(serde_json::json!({ "dropped": dropped }).to_string())),
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };
        if out.send(event).await.is_err() {
            return;
        }
    }
}

/// Whether the credentials in `headers` still authenticate a caller with
/// Read on `ws`, and `ws` and its mock `mock` still exist.
async fn still_allowed(st: &AdminState, headers: &HeaderMap, ws: &str, mock: &str) -> bool {
    let Ok(Some(principal)) = st.auth.authenticate(headers).await else {
        return false;
    };
    if !principal.can(ws, Permission::Read) {
        return false;
    }
    matches!(st.store.get_workspace(ws).await, Ok(Some(_)))
        && matches!(st.store.get_mock(ws, mock).await, Ok(Some(_)))
}

fn request_event(view: LiveRequestView) -> Option<Result<Event, Infallible>> {
    match Event::default().event("request").json_data(&view) {
        Ok(event) => Some(Ok(event)),
        Err(e) => {
            tracing::warn!("cannot serialize a live request record: {e}");
            None
        }
    }
}
