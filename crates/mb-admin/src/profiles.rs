//! `/api/v1/workspaces/{ws}/llm-profiles…`. Profiles hold a `secret_ref` (`env:NAME` or
//! `file:/path`), never a raw key, and the API never resolves it. A ref
//! written through the API must match one of the `server.api_secret_refs`
//! globs; profiles from the config file are not restricted.

use std::collections::{HashMap, HashSet};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mb_auth::Permission;
use mb_store::{LlmProfile, ResourceSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::access::WsAccess;
use crate::problem::{ApiResult, Json, Path, Problem};
use crate::{AdminState, Source, ensure_api_managed, glob_match, valid_identifier};

/// Path of one profile.
#[derive(Deserialize, Debug)]
pub struct ProfilePath {
    pub name: String,
}

/// Body keys that would carry a raw secret.
const RAW_KEY_FIELDS: [&str; 2] = ["api_key", "apiKey"];

/// An LLM profile. `secret_ref` is returned as stored, never resolved.
#[derive(Serialize, ToSchema, Debug)]
pub struct LlmProfileView {
    pub name: String,
    pub base_url: String,
    pub model: String,
    /// `env:NAME` or `file:/path`.
    pub secret_ref: Option<String>,
    pub json_schema: bool,
    pub temperature: f32,
    pub timeout_ms: u64,
    pub max_tokens: u32,
    pub fallback_profile: Option<String>,
    pub source: Source,
}

impl From<LlmProfile> for LlmProfileView {
    fn from(p: LlmProfile) -> Self {
        Self {
            source: Source::from(&p.source),
            name: p.name,
            base_url: p.base_url,
            model: p.model,
            secret_ref: p.secret_ref,
            json_schema: p.json_schema,
            temperature: p.temperature,
            timeout_ms: p.timeout_ms,
            max_tokens: p.max_tokens,
            fallback_profile: p.fallback_profile,
        }
    }
}

/// The full profile. A raw `api_key` is rejected: use `secret_ref`.
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct PutProfile {
    /// OpenAI-compatible base URL.
    pub base_url: String,
    pub model: String,
    /// `env:NAME` or `file:/path`.
    pub secret_ref: Option<String>,
    /// Use the provider's `json_schema` response format.
    #[serde(default)]
    pub json_schema: bool,
    #[serde(default = "default_temperature")]
    #[schema(default = 0.7)]
    pub temperature: f32,
    #[serde(default = "default_timeout_ms")]
    #[schema(default = 3000)]
    pub timeout_ms: u64,
    #[serde(default = "default_max_tokens")]
    #[schema(default = 2048)]
    pub max_tokens: u32,
    /// Profile tried when this one fails.
    pub fallback_profile: Option<String>,
}

fn default_temperature() -> f32 {
    0.7
}

fn default_timeout_ms() -> u64 {
    3000
}

fn default_max_tokens() -> u32 {
    2048
}

/// `env:NAME` or `file:/path` with a non-empty remainder. The rejection
/// never echoes the value, which may be a pasted secret.
fn validate_secret_ref(r: &str) -> ApiResult<()> {
    let rest = r.strip_prefix("env:").or_else(|| r.strip_prefix("file:"));
    match rest {
        Some(rest) if !rest.trim().is_empty() => Ok(()),
        _ => Err(Problem::unprocessable(
            "secret_ref must be `env:NAME` or `file:/path`",
        )),
    }
}

fn validate(name: &str, p: &PutProfile) -> ApiResult<()> {
    if p.base_url.trim().is_empty() || p.model.trim().is_empty() {
        return Err(Problem::unprocessable("base_url and model are required"));
    }
    if let Some(r) = &p.secret_ref {
        validate_secret_ref(r)?;
    }
    if !(p.temperature.is_finite() && (0.0..=2.0).contains(&p.temperature)) {
        return Err(Problem::unprocessable(
            "temperature must be between 0 and 2",
        ));
    }
    if p.timeout_ms == 0 || p.max_tokens == 0 {
        return Err(Problem::unprocessable(
            "timeout_ms and max_tokens must be positive",
        ));
    }
    if p.fallback_profile.as_deref() == Some(name) {
        return Err(Problem::unprocessable(
            "a profile cannot be its own fallback_profile",
        ));
    }
    Ok(())
}

/// The result of a profile write: the profile, plus warnings about mocks
/// the change may break.
#[derive(Serialize, ToSchema, Debug)]
pub struct ProfileWriteResult {
    #[serde(flatten)]
    pub profile: LlmProfileView,
    /// One entry per mock that uses the profile (directly or through a
    /// fallback chain). Such a mock is rebuilt; if that fails it keeps
    /// serving its previous version and `/readyz` reports it.
    pub warnings: Vec<String>,
}

/// The result of deleting a profile that mocks still use.
#[derive(Serialize, ToSchema, Debug)]
pub struct ProfileDeleteResult {
    pub deleted: String,
    /// One entry per mock that still uses the deleted profile.
    pub warnings: Vec<String>,
}

/// Names of the mocks of `ws` whose profile is `name` or falls back to it.
async fn mocks_using(st: &AdminState, ws: &str, name: &str) -> ApiResult<Vec<String>> {
    let fallbacks: HashMap<String, Option<String>> = st
        .store
        .list_profiles(ws)
        .await?
        .into_iter()
        .map(|p| (p.name, p.fallback_profile))
        .collect();
    let mut users = Vec::new();
    for mock in st.store.list_mocks(ws).await? {
        let mut next = mock.llm_profile.clone();
        let mut seen = HashSet::new();
        while let Some(profile) = next {
            if profile == name {
                users.push(mock.name.clone());
                break;
            }
            if !seen.insert(profile.clone()) {
                break;
            }
            next = fallbacks.get(&profile).cloned().flatten();
        }
    }
    Ok(users)
}

async fn find(st: &AdminState, ws: &str, name: &str) -> ApiResult<LlmProfile> {
    st.store
        .get_profile(ws, name)
        .await?
        .ok_or_else(|| Problem::not_found(format!("LLM profile {name:?} does not exist")))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/llm-profiles",
    tag = "llm-profiles",
    params(("ws" = String, Path, description = "Workspace name")),
    responses((status = 200, body = Vec<LlmProfileView>))
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
) -> ApiResult<Json<Vec<LlmProfileView>>> {
    access.require(Permission::Read)?;
    let profiles = st.store.list_profiles(&access.ws).await?;
    Ok(Json(profiles.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/llm-profiles/{name}",
    tag = "llm-profiles",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("name" = String, Path, description = "Profile name"),
    ),
    responses(
        (status = 200, body = LlmProfileView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_one(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(ProfilePath { name }): Path<ProfilePath>,
) -> ApiResult<Json<LlmProfileView>> {
    access.require(Permission::Read)?;
    Ok(Json(find(&st, &access.ws, &name).await?.into()))
}

#[utoipa::path(
    put,
    path = "/api/v1/workspaces/{ws}/llm-profiles/{name}",
    tag = "llm-profiles",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("name" = String, Path, description = "Profile name"),
    ),
    request_body = PutProfile,
    responses(
        (status = 200, body = ProfileWriteResult),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid profile, a raw `api_key`, or a `secret_ref` not allowed by `server.api_secret_refs`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn put(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(ProfilePath { name }): Path<ProfilePath>,
    Json(body): Json<Value>,
) -> ApiResult<Json<ProfileWriteResult>> {
    access.require(Permission::ManageProfiles)?;
    if !valid_identifier(&name) {
        return Err(Problem::unprocessable(
            "profile names are 1-128 characters of [A-Za-z0-9._-]",
        ));
    }
    if let Some(existing) = st.store.get_profile(&access.ws, &name).await? {
        ensure_api_managed(&existing.source, format_args!("LLM profile {name:?}"))?;
    }
    if let Some(obj) = body.as_object()
        && let Some(field) = RAW_KEY_FIELDS.iter().find(|k| obj.contains_key(**k))
    {
        return Err(Problem::unprocessable(format!(
            "`{field}` is not accepted: raw keys are never stored; use secret_ref (`env:NAME` or `file:/path`)"
        )));
    }
    let body: PutProfile =
        serde_json::from_value(body).map_err(|e| Problem::unprocessable(e.to_string()))?;
    validate(&name, &body)?;
    if let Some(r) = &body.secret_ref
        && !st.api_secret_refs.iter().any(|g| glob_match(g, r))
    {
        // The ref is not echoed: it may be a pasted secret.
        return Err(Problem::unprocessable(format!(
            "secret_ref not allowed by server.api_secret_refs (allowed: {})",
            st.api_secret_refs.join(", ")
        )));
    }
    let profile = LlmProfile {
        workspace: access.ws.clone(),
        name,
        base_url: body.base_url,
        model: body.model,
        secret_ref: body.secret_ref,
        json_schema: body.json_schema,
        temperature: body.temperature,
        timeout_ms: body.timeout_ms,
        max_tokens: body.max_tokens,
        fallback_profile: body.fallback_profile,
        source: ResourceSource::Api,
    };
    st.store.put_profile(&profile).await?;
    let warnings = mocks_using(&st, &access.ws, &profile.name)
        .await?
        .into_iter()
        .map(|mock| {
            format!(
                "mock {mock:?} uses LLM profile {:?} and is rebuilt; if that fails it keeps serving its previous version (see /readyz)",
                profile.name
            )
        })
        .collect();
    Ok(Json(ProfileWriteResult {
        profile: profile.into(),
        warnings,
    }))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{ws}/llm-profiles/{name}",
    tag = "llm-profiles",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("name" = String, Path, description = "Profile name"),
    ),
    responses(
        (status = 204, description = "Deleted; no mock used the profile"),
        (status = 200, description = "Deleted, but mocks still use the profile", body = ProfileDeleteResult),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn delete(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(ProfilePath { name }): Path<ProfilePath>,
) -> ApiResult<Response> {
    access.require(Permission::ManageProfiles)?;
    let existing = find(&st, &access.ws, &name).await?;
    ensure_api_managed(&existing.source, format_args!("LLM profile {name:?}"))?;
    if st.store.delete_profile(&access.ws, &name).await? {
        let users = mocks_using(&st, &access.ws, &name).await?;
        if users.is_empty() {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        let warnings = users
            .into_iter()
            .map(|mock| {
                format!(
                    "mock {mock:?} uses the deleted LLM profile {name:?}; it keeps serving its previous version until it is fixed (see /readyz)"
                )
            })
            .collect();
        Ok(Json(ProfileDeleteResult {
            deleted: name,
            warnings,
        })
        .into_response())
    } else {
        Err(Problem::not_found(format!(
            "LLM profile {name:?} does not exist"
        )))
    }
}
