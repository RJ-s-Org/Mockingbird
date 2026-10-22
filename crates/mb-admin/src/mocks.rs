//! `/api/v1/workspaces/{ws}/mocks` and `…/mocks/{mock}`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_store::{
    ChaosSettings, Consistency, DataPlaneAuthConfig, Mock, MockBehavior, MockLimits, NewMock,
    ResourceSource, StoreError, UnknownIds, routes_collide, validate_hosts, validate_route_prefix,
};
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::access::WsAccess;
use crate::audit::snapshot;
use crate::problem::{ApiResult, Json, Path, Problem};
use crate::{AdminState, Source, ensure_api_managed};

/// The OpenAPI schema of [`DataPlaneAuthConfig`] (which lives in `mb-store`).
#[derive(ToSchema)]
#[schema(as = DataPlaneAuthConfig)]
#[allow(dead_code)]
pub struct DataPlaneAuthSchema {
    /// How callers of the mock's data plane authenticate.
    #[schema(default = "none")]
    mode: DataPlaneAuthModeSchema,
    /// Lowercase header carrying the key in `api_key` mode (the key may also
    /// be sent as `?api_key=`).
    #[schema(default = "x-api-key")]
    header: String,
}

/// The OpenAPI schema of `DataPlaneAuthMode`.
#[derive(ToSchema)]
#[schema(as = DataPlaneAuthMode, rename_all = "snake_case")]
#[allow(dead_code)]
pub enum DataPlaneAuthModeSchema {
    None,
    ApiKey,
    PassthroughValidate,
}

/// The OpenAPI schema of [`ChaosSettings`] (which lives in `mb-store`).
#[derive(ToSchema)]
#[schema(as = ChaosSettings)]
#[allow(dead_code)]
pub struct ChaosSchema {
    /// Probability (0–1) that a call answers with an injected error.
    #[serde(default)]
    #[schema(minimum = 0.0, maximum = 1.0)]
    error_rate: f64,
    /// Uniformly random added latency.
    latency_jitter: Option<LatencyJitterSchema>,
    /// Probability (0–1) that a streamed response is cut short.
    #[serde(default)]
    #[schema(minimum = 0.0, maximum = 1.0)]
    drop_stream_rate: f64,
}

/// The OpenAPI schema of `LatencyJitter`.
#[derive(ToSchema)]
#[schema(as = LatencyJitter)]
#[allow(dead_code)]
pub struct LatencyJitterSchema {
    min_ms: u64,
    /// At most 30000 and at least `min_ms`.
    #[schema(maximum = 30000)]
    max_ms: u64,
}

/// The OpenAPI schema of [`MockLimits`] (which lives in `mb-store`). Unset
/// fields fall back to the global limits (or to no limit).
#[derive(ToSchema)]
#[schema(as = MockLimits)]
#[allow(dead_code)]
pub struct MockLimitsSchema {
    rate_limit: Option<RateLimitSchema>,
    /// Tokens per UTC day across all replicas; at least 1.
    #[schema(minimum = 1)]
    daily_token_budget: Option<u64>,
    /// In bytes, 1024–67108864.
    #[schema(minimum = 1024, maximum = 67108864)]
    max_request_bytes: Option<u64>,
    /// In bytes, 1024–16777216.
    #[schema(minimum = 1024, maximum = 16777216)]
    max_response_bytes: Option<u64>,
}

/// The OpenAPI schema of `RateLimit`: a token bucket.
#[derive(ToSchema)]
#[schema(as = RateLimit)]
#[allow(dead_code)]
pub struct RateLimitSchema {
    /// Greater than 0, at most 100000.
    #[schema(maximum = 100000)]
    requests_per_second: f64,
    /// 1–100000.
    #[schema(minimum = 1, maximum = 100000)]
    burst: u32,
}

/// Path of one mock.
#[derive(Deserialize, Debug)]
pub struct MockPath {
    pub mock: String,
}

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
    #[schema(value_type = DataPlaneAuthSchema)]
    pub data_plane_auth: DataPlaneAuthConfig,
    /// Fault injection; absent when none is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ChaosSchema>)]
    pub chaos: Option<ChaosSettings>,
    #[schema(value_type = MockLimitsSchema)]
    pub limits: MockLimits,
    /// `on` (default): repeated reads in a session get the same answer and
    /// listed items are served by id; `off` generates every answer anew.
    #[schema(value_type = String, example = "on")]
    pub consistency: Consistency,
    /// `generate` (default) or `404`: how a read of an id the session does
    /// not hold is answered.
    #[schema(value_type = String, example = "generate")]
    pub unknown_ids: UnknownIds,
    /// The scenario applied when a call names none; absent when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_scenario: Option<String>,
    /// Host names the mock answers for; empty answers for any host.
    pub hosts: Vec<String>,
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
            data_plane_auth: m.data_plane_auth,
            chaos: m.chaos,
            limits: m.limits,
            consistency: m.behavior.consistency,
            unknown_ids: m.behavior.unknown_ids,
            default_scenario: m.behavior.default_scenario,
            hosts: m.hosts,
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
    /// Defaults to `{"mode": "none"}`.
    #[serde(default)]
    #[schema(value_type = Option<DataPlaneAuthSchema>)]
    pub data_plane_auth: DataPlaneAuthConfig,
    /// Fault injection; none by default.
    #[serde(default)]
    #[schema(value_type = Option<ChaosSchema>)]
    pub chaos: Option<ChaosSettings>,
    /// Defaults to `{}` (the global limits).
    #[serde(default)]
    #[schema(value_type = Option<MockLimitsSchema>)]
    pub limits: MockLimits,
    /// `on` (default) or `off`.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub consistency: Consistency,
    /// `generate` (default) or `404`.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub unknown_ids: UnknownIds,
    /// Host names the mock answers for (lowercase, no port); none by
    /// default, which answers for any host. Mocks with different hosts may
    /// share a route prefix.
    #[serde(default)]
    pub hosts: Vec<String>,
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
    #[schema(value_type = Option<DataPlaneAuthSchema>)]
    pub data_plane_auth: Option<DataPlaneAuthConfig>,
    /// `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<ChaosSchema>)]
    pub chaos: Option<Option<ChaosSettings>>,
    /// Replaces the whole object: a `null` member clears that limit, and
    /// `null` clears them all.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<MockLimitsSchema>)]
    pub limits: Option<Option<MockLimits>>,
    /// `on` or `off`.
    #[schema(value_type = Option<String>)]
    pub consistency: Option<Consistency>,
    /// `generate` or `404`.
    #[schema(value_type = Option<String>)]
    pub unknown_ids: Option<UnknownIds>,
    /// A scenario of the mock; `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub default_scenario: Option<Option<String>>,
    /// Replaces the host list; `[]` clears it.
    pub hosts: Option<Vec<String>>,
}

/// Distinguishes an explicit `null` (`Some(None)`) from an absent field (`None`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Title of the 409 for a mock name already taken in the workspace.
const MOCK_EXISTS: &str = "Mock already exists";

fn mock_exists(ws: &str, name: &str) -> Problem {
    Problem::titled(
        StatusCode::CONFLICT,
        MOCK_EXISTS,
        format!("a mock named {name:?} already exists in workspace {ws:?}"),
    )
}

/// A 409 problem when a mock other than `except` (workspace, name), in any
/// workspace, already serves `prefix` on an overlapping host set (see
/// [`routes_collide`]): routes are global.
async fn ensure_route_free(
    st: &AdminState,
    prefix: &str,
    hosts: &[String],
    except: Option<(&str, &str)>,
) -> ApiResult<()> {
    let holder = st.store.list_all_mocks().await?.into_iter().find(|m| {
        routes_collide(prefix, hosts, &m.route_prefix, &m.hosts)
            && Some((m.workspace.as_str(), m.name.as_str())) != except
    });
    let Some(holder) = holder else {
        return Ok(());
    };
    Err(Problem::conflict(
        match hosts.iter().find(|h| holder.hosts.contains(h)) {
            Some(host) => format!(
                "route prefix {prefix:?} on host {host:?} is already used by mock {}/{}",
                holder.workspace, holder.name
            ),
            None => format!(
                "route prefix {prefix:?} is already used by mock {}/{}",
                holder.workspace, holder.name
            ),
        },
    ))
}

async fn view(st: &AdminState, m: Mock) -> ApiResult<MockView> {
    let active = st.store.active_contract(m.id).await?.map(|c| c.version);
    Ok(MockView::new(m, active))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks",
    tag = "mocks",
    params(("ws" = String, Path, description = "Workspace name")),
    responses((status = 200, body = Vec<MockView>))
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
) -> ApiResult<Json<Vec<MockView>>> {
    access.require(Permission::Read)?;
    let mut out = Vec::new();
    for m in st.store.list_mocks(&access.ws).await? {
        out.push(view(&st, m).await?);
    }
    Ok(Json(out))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/mocks",
    tag = "mocks",
    params(("ws" = String, Path, description = "Workspace name")),
    request_body = CreateMock,
    responses(
        (status = 201, body = MockView),
        (status = 409, description = "The mock already exists (title \"Mock already exists\"), or another mock has its route prefix on an overlapping host set", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid name, route prefix, hosts, chaos or limits", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn create(
    State(st): State<AdminState>,
    access: WsAccess,
    Json(body): Json<CreateMock>,
) -> ApiResult<impl IntoResponse> {
    access.require(Permission::Edit)?;
    // The name first: a duplicate's default prefix `/<name>` is taken too,
    // and the name is the conflict the caller needs to hear about.
    if st.store.get_mock(&access.ws, &body.name).await?.is_some() {
        return Err(mock_exists(&access.ws, &body.name));
    }
    let prefix = body
        .route_prefix
        .clone()
        .unwrap_or_else(|| format!("/{}", body.name));
    validate_hosts(&body.hosts)?;
    ensure_route_free(&st, &prefix, &body.hosts, None).await?;
    let name = body.name.clone();
    let created = st
        .store
        .create_mock(
            &access.ws,
            NewMock {
                name: body.name,
                route_prefix: body.route_prefix,
                persona: body.persona,
                llm_profile: body.llm_profile,
                header_overrides: body.header_overrides,
                source: ResourceSource::Api,
                data_plane_auth: body.data_plane_auth,
                chaos: body.chaos,
                limits: body.limits,
                behavior: MockBehavior {
                    consistency: body.consistency,
                    unknown_ids: body.unknown_ids,
                    default_scenario: None,
                },
                hosts: body.hosts,
            },
        )
        .await
        .map_err(|e| match e {
            // Created concurrently since the check above.
            StoreError::Conflict(_) => mock_exists(&access.ws, &name),
            e => e.into(),
        })?;
    let view = MockView::new(created, None);
    access
        .audit(
            &st.audit,
            "mock.create",
            format!("mock:{}", view.name),
            None,
            snapshot(&view),
        )
        .await;
    Ok((StatusCode::CREATED, Json(view)))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}",
    tag = "mocks",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    responses(
        (status = 200, body = MockView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_one(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
) -> ApiResult<Json<MockView>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    Ok(Json(view(&st, m).await?))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}",
    tag = "mocks",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    request_body = PatchMock,
    responses(
        (status = 200, body = MockView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file, or another mock has the route prefix", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid route prefix, chaos or limits, or a default scenario the mock does not have", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file, or another mock has its route prefix on an overlapping host set", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid route prefix, hosts, chaos or limits", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn patch(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
    Json(body): Json<PatchMock>,
) -> ApiResult<Json<MockView>> {
    access.require(Permission::Edit)?;
    let mut m = st.mock(&access.ws, &mock).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    let before = snapshot(&view(&st, m.clone()).await?);
    if let Some(hosts) = &body.hosts {
        validate_hosts(hosts)?;
    }
    if let Some(prefix) = &body.route_prefix {
        validate_route_prefix(prefix)?;
    }
    if body.route_prefix.is_some() || body.hosts.is_some() {
        let prefix = body.route_prefix.as_deref().unwrap_or(&m.route_prefix);
        let hosts = body.hosts.as_deref().unwrap_or(&m.hosts);
        ensure_route_free(&st, prefix, hosts, Some((&m.workspace, &m.name))).await?;
    }
    if let Some(prefix) = body.route_prefix {
        m.route_prefix = prefix;
    }
    if let Some(hosts) = body.hosts {
        m.hosts = hosts;
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
    if let Some(auth) = body.data_plane_auth {
        m.data_plane_auth = auth;
    }
    if let Some(chaos) = body.chaos {
        m.chaos = chaos;
    }
    if let Some(limits) = body.limits {
        m.limits = limits.unwrap_or_default();
    }
    if let Some(consistency) = body.consistency {
        m.behavior.consistency = consistency;
    }
    if let Some(unknown_ids) = body.unknown_ids {
        m.behavior.unknown_ids = unknown_ids;
    }
    if let Some(default_scenario) = body.default_scenario {
        if let Some(name) = &default_scenario
            && !st
                .store
                .list_scenarios(m.id)
                .await?
                .iter()
                .any(|s| &s.name == name)
        {
            return Err(Problem::invalid(
                "Invalid mock",
                format!("mock {mock:?} has no scenario {name:?}"),
            ));
        }
        m.behavior.default_scenario = default_scenario;
    }
    let updated = view(&st, st.store.update_mock(&m).await?).await?;
    access
        .audit(
            &st.audit,
            "mock.update",
            format!("mock:{mock}"),
            before,
            snapshot(&updated),
        )
        .await;
    Ok(Json(updated))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}",
    tag = "mocks",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    responses(
        (status = 204, description = "Deleted with its versions, scenarios, request log and session state"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Session state could not be reset; the mock was not deleted", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn delete(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
) -> ApiResult<StatusCode> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    let before = snapshot(&view(&st, m).await?);
    // Its sessions go first, so a new mock of the same name starts empty and
    // a failed reset (503) leaves the mock in place for a retry.
    st.state.reset_mock(&access.ws, &mock).await?;
    if !st.store.delete_mock(&access.ws, &mock).await? {
        return Err(Problem::not_found(format!("mock {mock:?} does not exist")));
    }
    access
        .audit(
            &st.audit,
            "mock.delete",
            format!("mock:{mock}"),
            before,
            None,
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}
