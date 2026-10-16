//! `/api/v1/workspaces/{ws}/mocks/{mock}/scenarios…`.

use axum::extract::State;
use axum::http::StatusCode;
use mb_auth::Permission;
use mb_store::{ResourceSource, Scenario, ScenarioRules};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::access::WsAccess;
use crate::audit::snapshot;
use crate::mocks::MockPath;
use crate::problem::{ApiResult, Json, Path, Problem};
use crate::{AdminState, Source, ensure_api_managed, valid_identifier};

/// A named scenario of a mock.
#[derive(Serialize, ToSchema, Debug)]
pub struct ScenarioView {
    pub name: String,
    /// Extra instructions for the LLM.
    pub instructions: Option<String>,
    /// Forced response status.
    pub status: Option<u16>,
    /// Added latency.
    pub latency_ms: Option<u64>,
    /// Fields forced into the response body.
    #[schema(value_type = Option<Object>)]
    pub set: Option<Value>,
    /// Operation ids the scenario applies to; empty means all.
    pub operations: Vec<String>,
    pub source: Source,
}

impl From<Scenario> for ScenarioView {
    fn from(s: Scenario) -> Self {
        Self {
            source: Source::from(&s.source),
            name: s.name,
            instructions: s.instructions,
            status: s.rules.status,
            latency_ms: s.rules.latency_ms,
            set: s.rules.set,
            operations: s.rules.operations,
        }
    }
}

/// The full scenario; absent fields are cleared.
#[derive(Deserialize, ToSchema, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct PutScenario {
    pub instructions: Option<String>,
    /// 100–599.
    pub status: Option<u16>,
    pub latency_ms: Option<u64>,
    /// A JSON object of fields forced into the response body.
    #[schema(value_type = Option<Object>)]
    pub set: Option<Value>,
    /// Operation ids the scenario applies to; empty means all. When the
    /// mock has an active contract, each must be one of its operations.
    #[serde(default)]
    pub operations: Vec<String>,
}

/// Path of one scenario.
#[derive(Deserialize, Debug)]
pub struct ScenarioPath {
    pub mock: String,
    pub name: String,
}

async fn find(st: &AdminState, mock_id: uuid::Uuid, name: &str) -> ApiResult<Option<Scenario>> {
    Ok(st
        .store
        .list_scenarios(mock_id)
        .await?
        .into_iter()
        .find(|s| s.name == name))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/scenarios",
    tag = "scenarios",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    responses(
        (status = 200, body = Vec<ScenarioView>),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
) -> ApiResult<Json<Vec<ScenarioView>>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let scenarios = st.store.list_scenarios(m.id).await?;
    Ok(Json(scenarios.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    put,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/scenarios/{name}",
    tag = "scenarios",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("name" = String, Path, description = "Scenario name"),
    ),
    request_body = PutScenario,
    responses(
        (status = 200, body = ScenarioView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid scenario, or operations unknown to the active contract", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn put(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(ScenarioPath { mock, name }): Path<ScenarioPath>,
    Json(body): Json<PutScenario>,
) -> ApiResult<Json<ScenarioView>> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    if !valid_identifier(&name) {
        return Err(Problem::unprocessable(
            "scenario names are 1-128 characters of [A-Za-z0-9._-]",
        ));
    }
    let existing = find(&st, m.id, &name).await?;
    if let Some(existing) = &existing {
        ensure_api_managed(&existing.source, format_args!("scenario {name:?}"))?;
    }
    if body.status.is_some_and(|s| !(100..=599).contains(&s)) {
        return Err(Problem::unprocessable("status must be between 100 and 599"));
    }
    if body.set.as_ref().is_some_and(|s| !s.is_object()) {
        return Err(Problem::unprocessable("set must be a JSON object"));
    }
    // Without an active contract there is nothing to check against yet.
    if let Some(contract) = st.store.active_contract(m.id).await? {
        let mut unknown: Vec<&str> = body
            .operations
            .iter()
            .map(String::as_str)
            .filter(|op| contract.bundle.operation(op).is_none())
            .collect();
        if !unknown.is_empty() {
            unknown.sort_unstable();
            unknown.dedup();
            return Err(Problem::unprocessable(format!(
                "the active contract of mock {mock:?} has no operations {unknown:?}"
            )));
        }
    }
    let scenario = Scenario {
        mock_id: m.id,
        name,
        instructions: body.instructions,
        rules: ScenarioRules {
            status: body.status,
            latency_ms: body.latency_ms,
            set: body.set,
            operations: body.operations,
        },
        source: ResourceSource::Api,
    };
    st.store.put_scenario(&scenario).await?;
    let view = ScenarioView::from(scenario);
    access
        .audit(
            &st.audit,
            "scenario.put",
            format!("scenario:{mock}/{}", view.name),
            existing.map(ScenarioView::from).as_ref().and_then(snapshot),
            snapshot(&view),
        )
        .await;
    Ok(Json(view))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/scenarios/{name}",
    tag = "scenarios",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("name" = String, Path, description = "Scenario name"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn delete(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(ScenarioPath { mock, name }): Path<ScenarioPath>,
) -> ApiResult<StatusCode> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    let missing = || Problem::not_found(format!("mock {mock:?} has no scenario {name:?}"));
    let existing = find(&st, m.id, &name).await?.ok_or_else(missing)?;
    ensure_api_managed(&existing.source, format_args!("scenario {name:?}"))?;
    let before = snapshot(&ScenarioView::from(existing));
    if st.store.delete_scenario(m.id, &name).await? {
        access
            .audit(
                &st.audit,
                "scenario.delete",
                format!("scenario:{mock}/{name}"),
                before,
                None,
            )
            .await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(missing())
    }
}
