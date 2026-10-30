//! `/api/v1/workspaces/{ws}/mocks/{mock}/webhooks/deliveries` (and its P2
//! alias under `/api/v1/mocks/…`): a mock's recent webhook deliveries.
//!
//! Reading needs the viewer role. The records live in memory on the node
//! that delivered them, handed to the router as an
//! `axum::Extension<SharedDeliveryLog>`; without one the list is empty.

use axum::extract::State;
use axum::routing::get;
use axum::{Extension, Router};
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_protocols::webhooks::log::{DELIVERY_LOG_CAPACITY, DeliveryRecord, SharedDeliveryLog};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::AdminState;
use crate::access::WsAccess;
use crate::mocks::MockPath;
use crate::problem::{ApiResult, Json, Path, Problem, Query};

const DEFAULT_LIMIT: u32 = 50;

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct DeliveriesQuery {
    /// 1–200; default 50.
    pub limit: Option<u32>,
}

/// One event sent (or to be sent) to one webhook target.
#[derive(Serialize, ToSchema, Debug, PartialEq)]
pub struct WebhookDeliveryView {
    pub id: String,
    pub event_id: String,
    /// e.g. `customer.created`.
    pub event_type: String,
    /// The operation whose call fired the event.
    pub operation: String,
    pub session: String,
    /// The target URL without userinfo, query or fragment.
    pub url: String,
    /// `pending`, `delivered`, `failed` or `dropped`.
    pub status: String,
    /// Attempts made so far (at most 4).
    pub attempts: u32,
    /// The last attempt's HTTP status.
    pub response_status: Option<u16>,
    /// Why the last attempt (or the delivery) failed.
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl From<DeliveryRecord> for WebhookDeliveryView {
    fn from(r: DeliveryRecord) -> Self {
        Self {
            id: r.id,
            event_id: r.event_id,
            event_type: r.event_type,
            operation: r.operation,
            session: r.session,
            url: r.url,
            status: r.status.as_str().to_string(),
            attempts: r.attempts,
            response_status: r.response_status,
            error: r.error,
            created_at: r.created_at.into(),
            finished_at: r.finished_at.map(Into::into),
        }
    }
}

#[derive(OpenApi)]
#[openapi(paths(list_deliveries), components(schemas(WebhookDeliveryView)), tags((name = "webhooks")))]
#[derive(Debug)]
pub struct WebhookDeliveriesDoc;

/// The delivery routes of a workspace (relative to `/api/v1/workspaces/{ws}`,
/// and to `/api/v1` for the `default` aliases).
pub fn routes() -> Router<AdminState> {
    Router::new().route("/mocks/{mock}/webhooks/deliveries", get(list_deliveries))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/webhooks/deliveries",
    tag = "webhooks",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        DeliveriesQuery,
    ),
    responses(
        (status = 200, description = "Newest first; kept in memory on the node that delivered them", body = Vec<WebhookDeliveryView>),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "limit outside 1–200", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_deliveries(
    State(st): State<AdminState>,
    access: WsAccess,
    log: Option<Extension<SharedDeliveryLog>>,
    Path(MockPath { mock }): Path<MockPath>,
    Query(q): Query<DeliveriesQuery>,
) -> ApiResult<Json<Vec<WebhookDeliveryView>>> {
    access.require(Permission::Read)?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    let max = u32::try_from(DELIVERY_LOG_CAPACITY).unwrap_or(u32::MAX);
    if !(1..=max).contains(&limit) {
        return Err(Problem::invalid(
            "Invalid limit",
            format!("limit must be between 1 and {max}"),
        ));
    }
    let m = st.mock(&access.ws, &mock).await?;
    let Some(Extension(log)) = log else {
        return Ok(Json(Vec::new()));
    };
    let records = log.list(&m.workspace, &m.name, limit as usize);
    Ok(Json(records.into_iter().map(Into::into).collect()))
}
