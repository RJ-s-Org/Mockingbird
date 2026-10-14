//! `/api/v1/llm-profiles…`. Profiles hold a `secret_ref` (`env:NAME` or
//! `file:/path`), never a raw key, and the API never resolves it.

use axum::extract::State;
use axum::http::StatusCode;
use mb_store::{DEFAULT_WORKSPACE, LlmProfile, ResourceSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::problem::{ApiResult, Json, Path, Problem};
use crate::{AdminState, Source, ensure_api_managed, valid_identifier};

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

async fn find(st: &AdminState, name: &str) -> ApiResult<LlmProfile> {
    st.store
        .get_profile(DEFAULT_WORKSPACE, name)
        .await?
        .ok_or_else(|| Problem::not_found(format!("LLM profile {name:?} does not exist")))
}

#[utoipa::path(
    get,
    path = "/api/v1/llm-profiles",
    tag = "llm-profiles",
    responses((status = 200, body = Vec<LlmProfileView>))
)]
pub async fn list(State(st): State<AdminState>) -> ApiResult<Json<Vec<LlmProfileView>>> {
    let profiles = st.store.list_profiles(DEFAULT_WORKSPACE).await?;
    Ok(Json(profiles.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    get,
    path = "/api/v1/llm-profiles/{name}",
    tag = "llm-profiles",
    params(("name" = String, Path, description = "Profile name")),
    responses(
        (status = 200, body = LlmProfileView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_one(
    State(st): State<AdminState>,
    Path(name): Path<String>,
) -> ApiResult<Json<LlmProfileView>> {
    Ok(Json(find(&st, &name).await?.into()))
}

#[utoipa::path(
    put,
    path = "/api/v1/llm-profiles/{name}",
    tag = "llm-profiles",
    params(("name" = String, Path, description = "Profile name")),
    request_body = PutProfile,
    responses(
        (status = 200, body = LlmProfileView),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid profile, or a raw `api_key`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn put(
    State(st): State<AdminState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<LlmProfileView>> {
    if !valid_identifier(&name) {
        return Err(Problem::unprocessable(
            "profile names are 1-128 characters of [A-Za-z0-9._-]",
        ));
    }
    if let Some(existing) = st.store.get_profile(DEFAULT_WORKSPACE, &name).await? {
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
    let profile = LlmProfile {
        workspace: DEFAULT_WORKSPACE.into(),
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
    Ok(Json(profile.into()))
}

#[utoipa::path(
    delete,
    path = "/api/v1/llm-profiles/{name}",
    tag = "llm-profiles",
    params(("name" = String, Path, description = "Profile name")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn delete(
    State(st): State<AdminState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let existing = find(&st, &name).await?;
    ensure_api_managed(&existing.source, format_args!("LLM profile {name:?}"))?;
    if st.store.delete_profile(DEFAULT_WORKSPACE, &name).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Problem::not_found(format!(
            "LLM profile {name:?} does not exist"
        )))
    }
}
