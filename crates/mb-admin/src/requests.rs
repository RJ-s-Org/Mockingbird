//! `/api/v1/workspaces/{ws}/mocks/{mock}/requests`: the request log.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_store::{RequestKind, RequestLogEntry, RequestLogFilter, RequestLogSummary};
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
    /// `summary`: entries without request and response bodies, with their
    /// sizes instead.
    pub fields: Option<String>,
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
    /// Why a `fallback` response did not use the LLM's answer.
    pub fallback_reason: Option<String>,
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
            fallback_reason: e.fallback_reason,
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

/// One recorded request without its bodies (`fields=summary`).
#[derive(Serialize, ToSchema, Debug)]
pub struct RequestLogSummaryView {
    pub id: i64,
    pub operation_id: Option<String>,
    pub session: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub source: Option<String>,
    /// Why a `fallback` response did not use the LLM's answer.
    pub fallback_reason: Option<String>,
    pub latency_ms: u64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub at: DateTime<Utc>,
    /// `http`, `graphql`, `grpc`, `soap` or `event`.
    pub kind: String,
    /// Size of the stored (redacted) request as JSON.
    pub request_bytes: u64,
    /// Size of the stored (redacted) response as JSON; 0 without one.
    pub response_bytes: u64,
}

impl From<RequestLogEntry> for RequestLogSummaryView {
    fn from(e: RequestLogEntry) -> Self {
        RequestLogSummary::from(&e).into()
    }
}

impl From<RequestLogSummary> for RequestLogSummaryView {
    fn from(e: RequestLogSummary) -> Self {
        Self {
            id: e.id,
            request_bytes: e.request_bytes,
            response_bytes: e.response_bytes,
            operation_id: e.operation_id,
            session: e.session,
            method: e.method,
            path: e.path,
            status: e.status,
            source: e.source,
            fallback_reason: e.fallback_reason,
            latency_ms: e.latency_ms,
            prompt_tokens: e.prompt_tokens,
            completion_tokens: e.completion_tokens,
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
        (status = 200, description = "Newest first; with `fields=summary`, `RequestLogSummaryView` items", body = Vec<RequestLogView>),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
    Query(q): Query<RequestsQuery>,
) -> ApiResult<Response> {
    access.require(Permission::Read)?;
    let summary = match q.fields.as_deref() {
        None => false,
        Some("summary") => true,
        Some(v) => {
            return Err(Problem::bad_request(format!(
                "unknown fields value {v:?}; use summary"
            )));
        }
    };
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
    Ok(if summary {
        // The store measures the bodies without returning them.
        let entries = st.store.list_request_log_summaries(m.id, &filter).await?;
        Json(
            entries
                .into_iter()
                .map(RequestLogSummaryView::from)
                .collect::<Vec<_>>(),
        )
        .into_response()
    } else {
        let entries = st.store.list_request_log(m.id, &filter).await?;
        Json(
            entries
                .into_iter()
                .map(RequestLogView::from)
                .collect::<Vec<_>>(),
        )
        .into_response()
    })
}

#[derive(Deserialize, Debug)]
pub struct RequestPath {
    pub mock: String,
    pub id: i64,
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/requests/{id}",
    tag = "requests",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("id" = i64, Path, description = "Request log id"),
    ),
    responses(
        (status = 200, description = "One recorded request with its bodies", body = RequestLogView),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_one(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(RequestPath { mock, id }): Path<RequestPath>,
) -> ApiResult<Json<RequestLogView>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let not_found = || Problem::not_found(format!("no request {id} in mock {mock:?}"));
    let before_id = id.checked_add(1).ok_or_else(not_found)?;
    let filter = RequestLogFilter {
        operation_id: None,
        session: None,
        source: None,
        limit: 1,
        before_id: Some(before_id),
        kind: None,
    };
    let entry = st
        .store
        .list_request_log(m.id, &filter)
        .await?
        .into_iter()
        .next()
        .filter(|e| e.id == id)
        .ok_or_else(not_found)?;
    Ok(Json(entry.into()))
}
