//! `/api/v1/workspaces` and `/api/v1/workspaces/{ws}`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use mb_auth::{Permission, Principal, Role};
use mb_store::{DEFAULT_WORKSPACE, Workspace};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::access::{CurrentPrincipal, WsAccess};
use crate::problem::{ApiResult, Json, Problem, Query};

/// Longest accepted `display_name`, in characters.
const MAX_DISPLAY_NAME: usize = 128;

/// The OpenAPI schema of [`Role`] (which lives in `mb-auth`).
#[derive(ToSchema)]
#[schema(as = Role, rename_all = "snake_case")]
#[allow(dead_code)]
pub enum RoleSchema {
    Viewer,
    Editor,
    Admin,
    Owner,
}

/// A workspace and the caller's role in it.
#[derive(Serialize, ToSchema, Debug)]
pub struct WorkspaceView {
    pub name: String,
    pub display_name: String,
    pub created_at: DateTime<Utc>,
    /// The caller's effective role; `owner` for a sysadmin.
    #[schema(value_type = Option<RoleSchema>)]
    pub role: Option<Role>,
}

impl WorkspaceView {
    fn new(w: Workspace, principal: &Principal) -> Self {
        Self {
            role: principal.role_in(&w.name),
            name: w.name,
            display_name: w.display_name,
            created_at: w.created_at,
        }
    }
}

/// A new workspace.
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspace {
    /// `^[a-z0-9][a-z0-9-]{0,62}$`
    pub name: String,
    /// Defaults to `name`.
    pub display_name: Option<String>,
}

/// The editable fields of a workspace.
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct PatchWorkspace {
    pub display_name: String,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct DeleteParams {
    /// Also delete the workspace's mocks (with their session state) and
    /// LLM profiles.
    pub force: Option<bool>,
}

/// 1–128 characters, not only whitespace.
fn validate_display_name(name: &str) -> ApiResult<()> {
    if name.trim().is_empty() || name.chars().count() > MAX_DISPLAY_NAME {
        return Err(Problem::unprocessable(format!(
            "display_name must be 1-{MAX_DISPLAY_NAME} characters and not blank"
        )));
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces",
    tag = "workspaces",
    responses((status = 200, description = "The workspaces the caller has a role in, by name", body = Vec<WorkspaceView>))
)]
pub async fn list(
    State(st): State<AdminState>,
    CurrentPrincipal(principal): CurrentPrincipal,
) -> ApiResult<Json<Vec<WorkspaceView>>> {
    let visible = st
        .store
        .list_workspaces()
        .await?
        .into_iter()
        .filter(|w| principal.role_in(&w.name).is_some())
        .map(|w| WorkspaceView::new(w, &principal))
        .collect();
    Ok(Json(visible))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces",
    tag = "workspaces",
    request_body = CreateWorkspace,
    responses(
        (status = 201, body = WorkspaceView),
        (status = 403, description = "Not a sysadmin", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The workspace already exists", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid name or display name", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn create(
    State(st): State<AdminState>,
    CurrentPrincipal(principal): CurrentPrincipal,
    Json(body): Json<CreateWorkspace>,
) -> ApiResult<impl IntoResponse> {
    if !principal.sysadmin {
        return Err(Problem::forbidden("only a sysadmin can create workspaces"));
    }
    let display_name = body.display_name.unwrap_or_else(|| body.name.clone());
    validate_display_name(&display_name)?;
    let created = st.store.create_workspace(&body.name, &display_name).await?;
    Ok((
        StatusCode::CREATED,
        Json(WorkspaceView::new(created, &principal)),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}",
    tag = "workspaces",
    params(("ws" = String, Path, description = "Workspace name")),
    responses(
        (status = 200, body = WorkspaceView),
        (status = 403, body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace (sysadmin only)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_one(
    State(st): State<AdminState>,
    access: WsAccess,
) -> ApiResult<Json<WorkspaceView>> {
    access.require(Permission::Read)?;
    let w =
        st.store.get_workspace(&access.ws).await?.ok_or_else(|| {
            Problem::not_found(format!("workspace {:?} does not exist", access.ws))
        })?;
    Ok(Json(WorkspaceView::new(w, &access.principal)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{ws}",
    tag = "workspaces",
    params(("ws" = String, Path, description = "Workspace name")),
    request_body = PatchWorkspace,
    responses(
        (status = 200, body = WorkspaceView),
        (status = 403, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 422, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn patch(
    State(st): State<AdminState>,
    access: WsAccess,
    Json(body): Json<PatchWorkspace>,
) -> ApiResult<Json<WorkspaceView>> {
    access.require(Permission::ManageWorkspace)?;
    validate_display_name(&body.display_name)?;
    let updated = st
        .store
        .update_workspace(&access.ws, &body.display_name)
        .await?;
    Ok(Json(WorkspaceView::new(updated, &access.principal)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{ws}",
    tag = "workspaces",
    params(("ws" = String, Path, description = "Workspace name"), DeleteParams),
    responses(
        (status = 204, description = "Deleted with its API keys (and, with `force`, its mocks and profiles)"),
        (status = 403, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The default workspace, or a non-empty one without `force`", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Session state could not be reset; nothing was deleted", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn delete(
    State(st): State<AdminState>,
    access: WsAccess,
    Query(params): Query<DeleteParams>,
) -> ApiResult<StatusCode> {
    access.require(Permission::ManageWorkspace)?;
    let ws = access.ws.as_str();
    if ws == DEFAULT_WORKSPACE {
        return Err(Problem::conflict("the default workspace cannot be deleted"));
    }
    let force = params.force.unwrap_or(false);
    if force {
        // Session state goes first, so a failed reset (503) leaves the
        // workspace in place for a retry.
        for mock in st.store.list_mocks(ws).await? {
            st.state.reset_mock(ws, &mock.name).await?;
        }
    }
    if st.store.delete_workspace(ws, force).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Problem::not_found(format!(
            "workspace {ws:?} does not exist"
        )))
    }
}
