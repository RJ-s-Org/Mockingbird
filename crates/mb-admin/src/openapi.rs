//! The OpenAPI document served at `/api/openapi.json`.

use std::sync::LazyLock;

use axum::http::header;
use axum::response::{IntoResponse, Response};
use utoipa::OpenApi;

use crate::{
    Source, VersionStatus, contracts, mocks, problem, profiles, requests, scenarios, sessions,
};

#[derive(OpenApi)]
#[openapi(
    info(title = "Mockingbird Admin API", description = "Manage mocks, contract versions, scenarios, LLM profiles, sessions and the request log. Errors are RFC 9457 `application/problem+json`."),
    paths(
        mocks::list,
        mocks::create,
        mocks::get_one,
        mocks::patch,
        mocks::delete,
        contracts::upload,
        contracts::list,
        contracts::get_one,
        contracts::source,
        contracts::activate,
        scenarios::list,
        scenarios::put,
        scenarios::delete,
        profiles::list,
        profiles::get_one,
        profiles::put,
        profiles::delete,
        sessions::list,
        sessions::export,
        sessions::seed,
        sessions::reset,
        requests::list,
    ),
    components(schemas(
        problem::Problem,
        Source,
        VersionStatus,
        mocks::MockView,
        mocks::CreateMock,
        mocks::PatchMock,
        contracts::UploadResult,
        contracts::ContractVersionSummary,
        contracts::ContractVersionDetail,
        contracts::OperationSummary,
        contracts::ChannelSummary,
        scenarios::ScenarioView,
        scenarios::PutScenario,
        profiles::LlmProfileView,
        profiles::PutProfile,
        profiles::ProfileWriteResult,
        profiles::ProfileDeleteResult,
        sessions::SessionInfoView,
        sessions::StoredResourceView,
        requests::RequestLogView,
    )),
    tags(
        (name = "mocks"),
        (name = "contracts"),
        (name = "scenarios"),
        (name = "llm-profiles"),
        (name = "sessions"),
        (name = "requests"),
    )
)]
pub struct ApiDoc;

/// The document, serialised once.
static DOCUMENT: LazyLock<String> = LazyLock::new(|| {
    ApiDoc::openapi()
        .merge_from(crate::channels::ChannelsDoc::openapi())
        .to_json()
        .unwrap_or_else(|_| "{}".into())
});

pub async fn serve() -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        DOCUMENT.as_str(),
    )
        .into_response()
}
