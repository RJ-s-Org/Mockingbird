//! `/api/v1/mocks` and `/api/v1/mocks/{mock}`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use mb_store::{DEFAULT_WORKSPACE, Mock, NewMock, ResourceSource, validate_route_prefix};
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::problem::{ApiResult, Json, Path, Problem};
use crate::{AdminState, Source, ensure_api_managed};

/// A mock with the number of its active contract version.
#[derive(Serialize, ToSchema, Debug)]
pub struct MockView {
    pub id: Uuid,
    pub workspace: String,
    pub name: String,
    pub route_prefix: String,
    pub persona: Option<String>,
    pub llm_profile: Option<String>,
    pub header_overrides: bool,
    pub source: Source,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `null` until a contract version is activated.
    pub active_version: Option<i64>,
}

impl MockView {
    fn new(m: Mock, active_version: Option<i64>) -> Self {
        Self {
            id: m.id,
            source: Source::from(&m.source),
            workspace: m.workspace,
            name: m.name,
            route_prefix: m.route_prefix,
            persona: m.persona,
            llm_profile: m.llm_profile,
            header_overrides: m.header_overrides,
            created_at: m.created_at,
            updated_at: m.updated_at,
            active_version,
        }
    }
}

/// A new mock. Mocks created through the API always have `source = api`.
#[derive(Deserialize, ToSchema, Debug)]
pub struct CreateMock {
    /// `^[a-z0-9][a-z0-9-]{0,62}$`
    pub name: String,
    /// Defaults to `/<name>`.
    pub route_prefix: Option<String>,
    pub persona: Option<String>,
    pub llm_profile: Option<String>,
    /// Whether callers may steer responses with `X-Mockingbird-*` headers.
    #[serde(default = "default_true")]
    pub header_overrides: bool,
}

fn default_true() -> bool {
    true
}

/// JSON merge patch of a mock: absent fields are kept, `null` clears.
#[derive(Deserialize, ToSchema, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct PatchMock {
    pub route_prefix: Option<String>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub persona: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub llm_profile: Option<Option<String>>,
    pub header_overrides: Option<bool>,
}

/// Distinguishes an explicit `null` (`Some(None)`) from an absent field (`None`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// A 409 problem when a mock other than `except` already serves `prefix`.
async fn ensure_prefix_free(st: &AdminState, prefix: &str, except: Option<&str>) -> ApiResult<()> {
    let holder = st
        .store
        .list_mocks(DEFAULT_WORKSPACE)
        .await?
        .into_iter()
        .find(|m| m.route_prefix == prefix && Some(m.name.as_str()) != except);
    match holder {
        Some(holder) => Err(Problem::conflict(format!(
            "route prefix {prefix:?} is already used by mock {:?}",
            holder.name
        ))),
        None => Ok(()),
    }
}

async fn view(st: &AdminState, m: Mock) -> ApiResult<MockView> {
    let active = st.store.active_contract(m.id).await?.map(|c| c.version);
    Ok(MockView::new(m, active))
}

#[utoipa::path(
    get,
    path = "/api/v1/mocks",
    tag = "mocks",
    responses((status = 200, body = Vec<MockView>))
)]
pub async fn list(State(st): State<AdminState>) -> ApiResult<Json<Vec<MockView>>> {
    let mut out = Vec::new();
    for m in st.store.list_mocks(DEFAULT_WORKSPACE).await? {
        out.push(view(&st, m).await?);
    }
    Ok(Json(out))
}

#[utoipa::path(
    post,
    path = "/api/v1/mocks",
    tag = "mocks",
    request_body = CreateMock,
    responses(
        (status = 201, body = MockView),
        (status = 409, description = "The mock already exists, or another mock has its route prefix", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid name or route prefix", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn create(
    State(st): State<AdminState>,
    Json(body): Json<CreateMock>,
) -> ApiResult<impl IntoResponse> {
    let prefix = body
        .route_prefix
        .clone()
        .unwrap_or_else(|| format!("/{}", body.name));
    ensure_prefix_free(&st, &prefix, None).await?;
    let created = st
        .store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: body.name,
                route_prefix: body.route_prefix,
                persona: body.persona,
                llm_profile: body.llm_profile,
                header_overrides: body.header_overrides,
                source: ResourceSource::Api,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(MockView::new(created, None))))
}

#[utoipa::path(
    get,
    path = "/api/v1/mocks/{mock}",
    tag = "mocks",
    params(("mock" = String, Path, description = "Mock name")),
    responses(
        (status = 200, body = MockView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_one(
    State(st): State<AdminState>,
    Path(mock): Path<String>,
) -> ApiResult<Json<MockView>> {
    let m = st.mock(&mock).await?;
    Ok(Json(view(&st, m).await?))
}

#[utoipa::path(
    patch,
    path = "/api/v1/mocks/{mock}",
    tag = "mocks",
    params(("mock" = String, Path, description = "Mock name")),
    request_body = PatchMock,
    responses(
        (status = 200, body = MockView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file, or another mock has the route prefix", body = Problem, content_type = "application/problem+json"),
        (status = 422, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn patch(
    State(st): State<AdminState>,
    Path(mock): Path<String>,
    Json(body): Json<PatchMock>,
) -> ApiResult<Json<MockView>> {
    let mut m = st.mock(&mock).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    if let Some(prefix) = body.route_prefix {
        validate_route_prefix(&prefix)?;
        ensure_prefix_free(&st, &prefix, Some(&m.name)).await?;
        m.route_prefix = prefix;
    }
    if let Some(persona) = body.persona {
        m.persona = persona;
    }
    if let Some(profile) = body.llm_profile {
        m.llm_profile = profile;
    }
    if let Some(enabled) = body.header_overrides {
        m.header_overrides = enabled;
    }
    let updated = st.store.update_mock(&m).await?;
    Ok(Json(view(&st, updated).await?))
}

#[utoipa::path(
    delete,
    path = "/api/v1/mocks/{mock}",
    tag = "mocks",
    params(("mock" = String, Path, description = "Mock name")),
    responses(
        (status = 204, description = "Deleted with its versions, scenarios, request log and session state"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Mock deleted, but its session state could not be reset", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn delete(
    State(st): State<AdminState>,
    Path(mock): Path<String>,
) -> ApiResult<StatusCode> {
    let m = st.mock(&mock).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    if !st.store.delete_mock(DEFAULT_WORKSPACE, &mock).await? {
        return Err(Problem::not_found(format!("mock {mock:?} does not exist")));
    }
    // Its sessions go too, so a new mock of the same name starts empty.
    st.state.reset_mock(DEFAULT_WORKSPACE, &mock).await?;
    Ok(StatusCode::NO_CONTENT)
}
