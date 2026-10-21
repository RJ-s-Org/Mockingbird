//! `/api/v1/workspaces/{ws}/llm-profiles…`. Profiles hold a `secret_ref` (`env:NAME` or
//! `file:/path`), never a raw key, and the API never resolves it. A ref
//! written through the API must match one of the `server.api_secret_refs`
//! globs; profiles from the config file are not restricted.
//!
//! Only a sysadmin may bind a secret to an endpoint: setting or changing a
//! profile's `secret_ref` or a `headers` value that embeds a `${…}` secret
//! reference, or changing the `base_url` of a profile that has either.
//! Otherwise a workspace admin could send the server's own LLM key (or any
//! other allowed secret) to a host they control.

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mb_auth::Permission;
use mb_store::{LlmProfile, ResourceSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::access::WsAccess;
use crate::audit::snapshot;
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
    /// Extra provider request headers, as stored (`${…}` references are
    /// never resolved).
    pub headers: BTreeMap<String, String>,
    /// JSON merged into every provider request body.
    #[schema(value_type = Object)]
    pub extra_body: serde_json::Map<String, serde_json::Value>,
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
            headers: p.headers,
            extra_body: p.extra_body,
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
    /// A secret reference, never a raw key. Through the API it must match
    /// the server's `server.api_secret_refs` globs; by default
    /// env:MB_LLM_* or env:*_API_KEY (* matches any characters). Other
    /// `env:` names and `file:` paths are rejected unless the server allows
    /// them.
    pub secret_ref: Option<String>,
    /// Use the provider's `json_schema` response format.
    #[serde(default)]
    pub json_schema: bool,
    #[serde(default = "default_temperature")]
    #[schema(default = 0.7)]
    pub temperature: f32,
    #[serde(default = "default_timeout_ms")]
    #[schema(default = 30000)]
    pub timeout_ms: u64,
    #[serde(default = "default_max_tokens")]
    #[schema(default = 16384)]
    pub max_tokens: u32,
    /// Profile tried when this one fails.
    pub fallback_profile: Option<String>,
    /// Extra request headers sent to the provider, by lowercase name
    /// (Cloudflare AI Gateway's `cf-aig-authorization`, Azure's `api-key`,
    /// OpenRouter's `x-title`). A value may embed secret references as
    /// `${env:NAME}`, each allowed by `server.api_secret_refs`; a header
    /// whose name suggests a credential must use one.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// JSON merged into every request body sent to the provider, under the
    /// fields Mockingbird sets itself (`model`, `messages`, `response_format`,
    /// `temperature`, `max_tokens` and `stream` are rejected): OpenRouter's
    /// `provider` routing or `reasoning` settings, for example. At most
    /// 8 KiB; not for secrets.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub extra_body: serde_json::Map<String, serde_json::Value>,
}

fn default_temperature() -> f32 {
    0.7
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn default_max_tokens() -> u32 {
    16_384
}

/// The secret reference forms `globs` (`server.api_secret_refs`) accept,
/// as shown in rejections and the console: for the defaults,
/// "env:MB_LLM_* or env:*_API_KEY (* matches any characters)".
pub fn accepted_secret_refs(globs: &[String]) -> String {
    let forms = match globs {
        [] => return "none (server.api_secret_refs is empty)".into(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    };
    if globs.iter().any(|g| g.contains('*')) {
        format!("{forms} (* matches any characters)")
    } else {
        forms
    }
}

/// `env:NAME` or `file:/path` with a non-empty remainder, matching one of
/// `allowed` (`server.api_secret_refs`). The rejection never echoes the
/// value, which may be a pasted secret, and names the accepted forms.
fn check_secret_ref(r: &str, allowed: &[String]) -> ApiResult<()> {
    let rest = r.strip_prefix("env:").or_else(|| r.strip_prefix("file:"));
    if !rest.is_some_and(|rest| !rest.trim().is_empty()) {
        return Err(Problem::invalid(
            "Invalid secret reference",
            format!(
                "secret_ref must be a secret reference, not a raw key; accepted: {}",
                accepted_secret_refs(allowed)
            ),
        ));
    }
    if !allowed.iter().any(|g| glob_match(g, r)) {
        return Err(Problem::invalid(
            "Secret reference not allowed",
            format!(
                "secret_ref not allowed by server.api_secret_refs; accepted: {}",
                accepted_secret_refs(allowed)
            ),
        ));
    }
    Ok(())
}

/// Title of the 422 for an invalid profile body.
const INVALID_PROFILE: &str = "Invalid LLM profile";

fn validate(name: &str, p: &PutProfile) -> ApiResult<()> {
    if p.base_url.trim().is_empty() || p.model.trim().is_empty() {
        return Err(Problem::invalid(
            INVALID_PROFILE,
            "base_url and model are required",
        ));
    }
    if !(p.temperature.is_finite() && (0.0..=2.0).contains(&p.temperature)) {
        return Err(Problem::invalid(
            INVALID_PROFILE,
            "temperature must be between 0 and 2",
        ));
    }
    if p.timeout_ms == 0 || p.max_tokens == 0 {
        return Err(Problem::invalid(
            INVALID_PROFILE,
            "timeout_ms and max_tokens must be positive",
        ));
    }
    if p.fallback_profile.as_deref() == Some(name) {
        return Err(Problem::invalid(
            INVALID_PROFILE,
            "a profile cannot be its own fallback_profile",
        ));
    }
    mb_store::validate_profile_headers(&p.headers).map_err(|e| match e {
        mb_store::StoreError::Invalid(detail) => Problem::invalid(INVALID_PROFILE, detail),
        other => Problem::invalid(INVALID_PROFILE, other.to_string()),
    })?;
    mb_store::validate_extra_body(&p.extra_body).map_err(|e| match e {
        mb_store::StoreError::Invalid(detail) => Problem::invalid(INVALID_PROFILE, detail),
        other => Problem::invalid(INVALID_PROFILE, other.to_string()),
    })?;
    Ok(())
}

/// Whether any header value embeds a `${…}` secret reference.
fn headers_bind_secrets(headers: &BTreeMap<String, String>) -> bool {
    headers
        .values()
        .any(|v| mb_store::header_secret_refs(v).is_ok_and(|refs| !refs.is_empty()))
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
        (status = 403, description = "Not an admin of the workspace, or not a sysadmin when setting or changing `secret_ref` or the `base_url` of a profile with one", body = Problem, content_type = "application/problem+json"),
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
        return Err(Problem::invalid(
            INVALID_PROFILE,
            "profile names are 1-128 characters of [A-Za-z0-9._-]",
        ));
    }
    let existing = st.store.get_profile(&access.ws, &name).await?;
    if let Some(existing) = &existing {
        ensure_api_managed(&existing.source, format_args!("LLM profile {name:?}"))?;
    }
    if let Some(obj) = body.as_object()
        && let Some(field) = RAW_KEY_FIELDS.iter().find(|k| obj.contains_key(**k))
    {
        return Err(Problem::invalid(
            "Raw API key not accepted",
            format!(
                "`{field}` is not accepted: raw keys are never stored; use secret_ref ({})",
                accepted_secret_refs(&st.api_secret_refs)
            ),
        ));
    }
    let body: PutProfile = serde_json::from_value(body)
        .map_err(|e| Problem::invalid(INVALID_PROFILE, e.to_string()))?;
    if let Some(r) = &body.secret_ref {
        check_secret_ref(r, &st.api_secret_refs)?;
    }
    validate(&name, &body)?;
    for value in body.headers.values() {
        for r in mb_store::header_secret_refs(value).unwrap_or_default() {
            check_secret_ref(r, &st.api_secret_refs)?;
        }
    }
    if !access.principal.sysadmin
        && let Some(r) = &body.secret_ref
    {
        let unchanged = existing.as_ref().is_some_and(|e| {
            e.secret_ref.as_deref() == Some(r.as_str()) && e.base_url == body.base_url
        });
        if !unchanged {
            return Err(Problem::forbidden(
                "only a sysadmin may set or change secret_ref, or change the base_url of a profile with a secret_ref",
            ));
        }
    }
    if !access.principal.sysadmin && headers_bind_secrets(&body.headers) {
        let unchanged = existing
            .as_ref()
            .is_some_and(|e| e.headers == body.headers && e.base_url == body.base_url);
        if !unchanged {
            return Err(Problem::forbidden(
                "only a sysadmin may set or change headers with ${…} secret references, or change the base_url of a profile with them",
            ));
        }
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
        headers: body.headers,
        extra_body: body.extra_body,
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
    let view = LlmProfileView::from(profile);
    access
        .audit(
            &st.audit,
            "profile.put",
            format!("profile:{}", view.name),
            existing
                .map(LlmProfileView::from)
                .as_ref()
                .and_then(snapshot),
            snapshot(&view),
        )
        .await;
    Ok(Json(ProfileWriteResult {
        profile: view,
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
        access
            .audit(
                &st.audit,
                "profile.delete",
                format!("profile:{name}"),
                snapshot(&LlmProfileView::from(existing)),
                None,
            )
            .await;
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
