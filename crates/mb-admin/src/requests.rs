//! `/api/v1/workspaces/{ws}/mocks/{mock}/requests`: the request log.

use axum::extract::State;
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_store::{RequestKind, RequestLogEntry, RequestLogFilter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::access::WsAccess;
use crate::mocks::MockPath;
use crate::problem::{ApiResult, Json, Path, Problem, Query};

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct RequestsQuery {
    /// Operation id.
    pub operation: Option<String>,
    /// Session id.
    pub session: Option<String>,
    /// Response source, e.g. `llm`, `repaired`, `fallback`.
    pub source: Option<String>,
    /// 1–500; default 100.
    pub limit: Option<u32>,
    /// Only entries with a smaller id (keyset pagination).
    pub before: Option<i64>,
    /// Request kind: `http`, `graphql`, `grpc`, `soap` or `event`.
    pub kind: Option<String>,
}

/// One recorded request, with redacted request and response bodies.
#[derive(Serialize, ToSchema, Debug)]
pub struct RequestLogView {
    pub id: i64,
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
    pub at: DateTime<Utc>,
    /// `http`, `graphql`, `grpc`, `soap` or `event`.
    pub kind: String,
}

impl From<RequestLogEntry> for RequestLogView {
    fn from(e: RequestLogEntry) -> Self {
        Self {
            id: e.id,
            operation_id: e.operation_id,
            session: e.session,
            method: e.method,
            path: e.path,
            status: e.status,
            source: e.source,
            latency_ms: e.latency_ms,
            prompt_tokens: e.prompt_tokens,
            completion_tokens: e.completion_tokens,
            request: e.request,
            response: e.response,
            at: e.at,
            kind: e.kind.as_str().to_string(),
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/requests",
    tag = "requests",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        RequestsQuery,
    ),
    responses(
        (status = 200, description = "Newest first", body = Vec<RequestLogView>),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
    Query(q): Query<RequestsQuery>,
) -> ApiResult<Json<Vec<RequestLogView>>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let kind = q
        .kind
        .as_deref()
        .map(|k| {
            RequestKind::parse(k)
                .ok_or_else(|| Problem::bad_request(format!("unknown request kind {k:?}")))
        })
        .transpose()?;
    let filter = RequestLogFilter {
        operation_id: q.operation,
        session: q.session,
        source: q.source,
        limit: q.limit.unwrap_or(RequestLogFilter::DEFAULT_LIMIT),
        before_id: q.before,
        kind,
    };
    let entries = st.store.list_request_log(m.id, &filter).await?;
    Ok(Json(entries.into_iter().map(Into::into).collect()))
}
