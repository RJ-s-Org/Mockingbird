//! `/api/v1/workspaces/{ws}/api-keys…`: create, list and revoke API keys.
//! The token is returned once, at creation; only its hash is stored.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use mb_auth::{ApiKeys, AuthError, KeyScope, NewKeyRequest, Permission, Role};
use mb_store::ApiKey;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::AdminState;
use crate::access::WsAccess;
use crate::audit::snapshot;
use crate::problem::{ApiResult, Json, Path, Problem};
use crate::workspaces::RoleSchema;

/// The OpenAPI schema of [`KeyScope`] (which lives in `mb-auth`).
#[derive(ToSchema)]
#[schema(as = KeyScope, rename_all = "kebab-case")]
pub enum KeyScopeSchema {
    Admin,
    DataPlane,
}

/// Keeps the schema in step with [`KeyScope`]: a new scope fails to compile
/// here until the schema has it too.
impl From<KeyScope> for KeyScopeSchema {
    fn from(s: KeyScope) -> Self {
        match s {
            KeyScope::Admin => Self::Admin,
            KeyScope::DataPlane => Self::DataPlane,
        }
    }
}

/// A new API key.
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct CreateApiKey {
    /// 1–128 characters.
    pub name: String,
    /// At most the creator's own role in the workspace.
    #[serde(default = "viewer")]
    #[schema(value_type = RoleSchema, default = "viewer")]
    pub role: Role,
    /// Empty or absent means `["admin"]`.
    #[serde(default)]
    #[schema(value_type = Vec<KeyScopeSchema>)]
    pub scopes: Vec<KeyScope>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Acts as a sysadmin; only a sysadmin can create such a key.
    #[serde(default)]
    pub sysadmin: bool,
}

fn viewer() -> Role {
    Role::Viewer
}

/// An API key, without its secret.
#[derive(Serialize, ToSchema, Debug)]
pub struct ApiKeyView {
    pub key_id: String,
    pub name: String,
    /// `viewer`, `editor`, `admin` or `owner`.
    pub role: String,
    /// `admin` and/or `data-plane`.
    pub scopes: Vec<String>,
    pub sysadmin: bool,
    /// Subject of the principal that created the key.
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl From<ApiKey> for ApiKeyView {
    fn from(k: ApiKey) -> Self {
        Self {
            key_id: k.key_id,
            name: k.name,
            role: k.role,
            scopes: k.scopes,
            sysadmin: k.sysadmin,
            created_by: k.created_by,
            created_at: k.created_at,
            expires_at: k.expires_at,
            last_used_at: k.last_used_at,
            revoked_at: k.revoked_at,
        }
    }
}

/// A created key and its token, which is never shown again.
#[derive(Serialize, ToSchema, Debug)]
pub struct CreatedApiKey {
    pub key: ApiKeyView,
    /// `mbk_<key_id>_<secret>`.
    pub token: String,
}

/// Path of one key.
#[derive(Deserialize, Debug)]
pub struct KeyPath {
    pub key_id: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/api-keys",
    tag = "api-keys",
    params(("ws" = String, Path, description = "Workspace name")),
    responses(
        (status = 200, description = "Newest first, revoked keys included", body = Vec<ApiKeyView>),
        (status = 403, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
) -> ApiResult<Json<Vec<ApiKeyView>>> {
    access.require(Permission::ManageApiKeys)?;
    let keys = st.store.list_api_keys(&access.ws).await?;
    Ok(Json(keys.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/api-keys",
    tag = "api-keys",
    params(("ws" = String, Path, description = "Workspace name")),
    request_body = CreateApiKey,
    responses(
        (status = 201, description = "The token is shown only here", body = CreatedApiKey),
        (status = 403, description = "A role above the caller's, a sysadmin key by a non-sysadmin, or a caller that is itself an API key", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid name", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn create(
    State(st): State<AdminState>,
    access: WsAccess,
    Json(body): Json<CreateApiKey>,
) -> ApiResult<impl IntoResponse> {
    access.require(Permission::ManageApiKeys)?;
    let (record, token) = ApiKeys::new(st.store.clone())
        .create(
            &access.principal,
            &access.ws,
            NewKeyRequest {
                name: body.name,
                role: body.role,
                scopes: body.scopes,
                expires_at: body.expires_at,
                sysadmin: body.sysadmin,
            },
        )
        .await
        .map_err(|e| match e {
            AuthError::Invalid(d) => Problem::unprocessable(d),
            e => e.into(),
        })?;
    let view = ApiKeyView::from(record);
    access
        .audit(
            &st.audit,
            "api_key.create",
            format!("api_key:{}", view.key_id),
            None,
            snapshot(&view),
        )
        .await;
    Ok((
        StatusCode::CREATED,
        Json(CreatedApiKey { key: view, token }),
    ))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{ws}/api-keys/{key_id}",
    tag = "api-keys",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("key_id" = String, Path, description = "Key id"),
    ),
    responses(
        (status = 204, description = "Revoked"),
        (status = 403, body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such key in the workspace, or already revoked", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn revoke(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(KeyPath { key_id }): Path<KeyPath>,
) -> ApiResult<StatusCode> {
    access.require(Permission::ManageApiKeys)?;
    let missing = || Problem::not_found(format!("no active API key {key_id:?} in this workspace"));
    let before = st
        .store
        .get_api_key(&key_id)
        .await?
        .filter(|k| k.workspace == access.ws && k.revoked_at.is_none())
        .ok_or_else(missing)?;
    if !st.store.revoke_api_key(&access.ws, &key_id).await? {
        return Err(missing());
    }
    // This node refuses the key from now on; other nodes within their
    // cache TTL (`mb_auth::DEFAULT_API_KEY_CACHE_TTL`).
    ApiKeys::invalidate_caches();
    let after = st.store.get_api_key(&key_id).await?.map(ApiKeyView::from);
    access
        .audit(
            &st.audit,
            "api_key.revoke",
            format!("api_key:{key_id}"),
            snapshot(&ApiKeyView::from(before)),
            after.as_ref().and_then(snapshot),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}
