//! `/api/v1/mocks/{mock}/scenarios…`.

use axum::extract::State;
use axum::http::StatusCode;
use mb_store::{ResourceSource, Scenario, ScenarioRules};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

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
    /// Items per streamed response unless `X-Mockingbird-Stream-Items`
    /// overrides it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_items: Option<u32>,
    /// Pause between streamed items, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_item_interval_ms: Option<u64>,
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
            stream_items: s.rules.stream_items,
            stream_item_interval_ms: s.rules.stream_item_interval_ms,
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
    /// Items per streamed response (1–10000) unless
    /// `X-Mockingbird-Stream-Items` overrides it.
    #[serde(default)]
    pub stream_items: Option<u32>,
    /// Pause between streamed items, in milliseconds.
    #[serde(default)]
    pub stream_item_interval_ms: Option<u64>,
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
    path = "/api/v1/mocks/{mock}/scenarios",
    tag = "scenarios",
    params(("mock" = String, Path, description = "Mock name")),
    responses(
        (status = 200, body = Vec<ScenarioView>),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    Path(mock): Path<String>,
) -> ApiResult<Json<Vec<ScenarioView>>> {
    let m = st.mock(&mock).await?;
    let scenarios = st.store.list_scenarios(m.id).await?;
    Ok(Json(scenarios.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    put,
    path = "/api/v1/mocks/{mock}/scenarios/{name}",
    tag = "scenarios",
    params(
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
    Path((mock, name)): Path<(String, String)>,
    Json(body): Json<PutScenario>,
) -> ApiResult<Json<ScenarioView>> {
    let m = st.mock(&mock).await?;
    if !valid_identifier(&name) {
        return Err(Problem::unprocessable(
            "scenario names are 1-128 characters of [A-Za-z0-9._-]",
        ));
    }
    if let Some(existing) = find(&st, m.id, &name).await? {
        ensure_api_managed(&existing.source, format_args!("scenario {name:?}"))?;
    }
    if body.status.is_some_and(|s| !(100..=599).contains(&s)) {
        return Err(Problem::unprocessable("status must be between 100 and 599"));
    }
    if body
        .stream_items
        .is_some_and(|n| !(1..=10_000).contains(&n))
    {
        return Err(Problem::unprocessable(
            "stream_items must be between 1 and 10000",
        ));
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
            stream_items: body.stream_items,
            stream_item_interval_ms: body.stream_item_interval_ms,
        },
        source: ResourceSource::Api,
    };
    st.store.put_scenario(&scenario).await?;
    Ok(Json(scenario.into()))
}

#[utoipa::path(
    delete,
    path = "/api/v1/mocks/{mock}/scenarios/{name}",
    tag = "scenarios",
    params(
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
    Path((mock, name)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let m = st.mock(&mock).await?;
    let missing = || Problem::not_found(format!("mock {mock:?} has no scenario {name:?}"));
    let existing = find(&st, m.id, &name).await?.ok_or_else(missing)?;
    ensure_api_managed(&existing.source, format_args!("scenario {name:?}"))?;
    if st.store.delete_scenario(m.id, &name).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(missing())
    }
}
