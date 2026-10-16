//! `/api/v1/workspaces/{ws}/mocks/{mock}/requests/stream`: the live request
//! tail over server-sent events (spec §14).
//!
//! Each served data-plane call of the mock that passes the query filters is
//! sent as `event: request` with a [`LiveRequestView`]. A subscriber that
//! falls behind the feed gets `event: lagged` with `{"dropped": N}` and the
//! stream continues with the records still buffered.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_protocols::RequestRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::{Stream, StreamExt};
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
        (status = 403, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "No live feed", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn stream(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
    Query(q): Query<StreamQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let subscribe = st.live.as_ref().ok_or_else(|| {
        Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "the live request feed is not available",
        )
    })?;
    let (ws, mock) = (access.ws, m.name);
    let events = BroadcastStream::new(subscribe()).filter_map(move |item| match item {
        Ok(r) if r.workspace == ws && r.mock == mock && q.matches(&r) => {
            request_event(LiveRequestView::from(r))
        }
        Ok(_) => None,
        Err(BroadcastStreamRecvError::Lagged(dropped)) => Some(Ok(Event::default()
            .event("lagged")
            .data(serde_json::json!({ "dropped": dropped }).to_string()))),
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(KEEP_ALIVE)))
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
