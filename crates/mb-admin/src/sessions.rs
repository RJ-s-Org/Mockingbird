//! `/api/v1/workspaces/{ws}/mocks/{mock}/sessions…`: list, export, seed and reset session
//! state.

use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_state::{SessionInfo, SessionKey, StoredResource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::access::WsAccess;
use crate::mocks::MockPath;
use crate::problem::{ApiResult, Json, Path, Problem};
use crate::{AdminState, valid_identifier};

/// A live session of a mock.
#[derive(Serialize, ToSchema, Debug)]
pub struct SessionInfoView {
    pub session: String,
    /// Number of stored resources.
    pub resources: usize,
    pub last_touched: DateTime<Utc>,
}

impl From<SessionInfo> for SessionInfoView {
    fn from(s: SessionInfo) -> Self {
        Self {
            session: s.session,
            resources: s.resources,
            last_touched: s.last_touched,
        }
    }
}

/// One stored resource; a session export is a list of these, oldest first.
#[derive(Serialize, Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct StoredResourceView {
    pub resource_type: String,
    pub id: String,
    #[schema(value_type = Object)]
    pub data: Value,
}

impl From<StoredResource> for StoredResourceView {
    fn from(r: StoredResource) -> Self {
        Self {
            resource_type: r.resource_type,
            id: r.id,
            data: r.data,
        }
    }
}

impl From<StoredResourceView> for StoredResource {
    fn from(r: StoredResourceView) -> Self {
        Self {
            resource_type: r.resource_type,
            id: r.id,
            data: r.data,
        }
    }
}

/// Path of one session.
#[derive(Deserialize, Debug)]
pub struct SessionPath {
    pub mock: String,
    pub session: String,
}

/// The key of `session` in an existing mock of `ws`: 404 for an unknown
/// mock, 400 for a malformed session id.
async fn key(st: &AdminState, ws: &str, path: SessionPath) -> ApiResult<SessionKey> {
    let SessionPath { mock, session } = path;
    st.mock(ws, &mock).await?;
    if !valid_identifier(&session) {
        return Err(Problem::bad_request(
            "session ids are 1-128 characters of [A-Za-z0-9._-]",
        ));
    }
    Ok(SessionKey {
        workspace: ws.into(),
        mock,
        session,
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/sessions",
    tag = "sessions",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    responses(
        (status = 200, description = "Most recently touched first", body = Vec<SessionInfoView>),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "State store unavailable", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
) -> ApiResult<Json<Vec<SessionInfoView>>> {
    access.require(Permission::Read)?;
    st.mock(&access.ws, &mock).await?;
    let sessions = st.state.sessions(&access.ws, &mock).await?;
    Ok(Json(sessions.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/sessions/{session}",
    tag = "sessions",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("session" = String, Path, description = "Session id"),
    ),
    responses(
        (status = 200, description = "The session's resources, oldest first (empty for an unknown session)", body = Vec<StoredResourceView>),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "State store unavailable", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn export(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(path): Path<SessionPath>,
) -> ApiResult<Json<Vec<StoredResourceView>>> {
    access.require(Permission::Read)?;
    let key = key(&st, &access.ws, path).await?;
    let resources = st.state.export(&key).await?;
    Ok(Json(resources.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    put,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/sessions/{session}",
    tag = "sessions",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("session" = String, Path, description = "Session id"),
    ),
    request_body(content = Vec<StoredResourceView>, description = "Replaces the session's contents; later entries are more recent"),
    responses(
        (status = 204, description = "Seeded"),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "State store unavailable", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn seed(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(path): Path<SessionPath>,
    Json(body): Json<Vec<StoredResourceView>>,
) -> ApiResult<StatusCode> {
    access.require(Permission::Edit)?;
    let key = key(&st, &access.ws, path).await?;
    let resources = body.len();
    st.state
        .seed(&key, body.into_iter().map(Into::into).collect())
        .await?;
    // The seeded data is not recorded, only how much of it there was.
    access
        .audit(
            &st.audit,
            "session.seed",
            format!("session:{}/{}", key.mock, key.session),
            None,
            Some(serde_json::json!({ "resources": resources })),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/sessions/{session}",
    tag = "sessions",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("session" = String, Path, description = "Session id"),
    ),
    responses(
        (status = 204, description = "Reset"),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "State store unavailable", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn reset(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(path): Path<SessionPath>,
) -> ApiResult<StatusCode> {
    access.require(Permission::Edit)?;
    let key = key(&st, &access.ws, path).await?;
    st.state.reset(&key).await?;
    access
        .audit(
            &st.audit,
            "session.reset",
            format!("session:{}/{}", key.mock, key.session),
            None,
            None,
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}
