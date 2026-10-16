//! The OpenAPI document served at `/api/openapi.json`: the canonical
//! `/api/v1/workspaces/{ws}/…` paths, plus the P2 alias paths for
//! `default`, marked deprecated.

use std::sync::LazyLock;

use axum::http::header;
use axum::response::{IntoResponse, Response};
use utoipa::OpenApi;
use utoipa::openapi::path::{Operation, ParameterIn, PathItem};
use utoipa::openapi::{Deprecated, RefOr};

use crate::{
    API_BASE, Source, VersionStatus, apikeys, audit, auth_routes, contracts, live, mocks, problem,
    profiles, requests, scenarios, sessions, workspaces,
};

#[derive(OpenApi)]
#[openapi(
    info(title = "Mockingbird Admin API", description = "Manage workspaces and, in each workspace, mocks, contract versions, scenarios, LLM profiles, sessions, the request log and its live SSE tail, API keys and the audit log. Every successful mutation writes one audit event; `GET /api/v1/audit` lists every workspace's events (sysadmin only); `GET /api/v1/me` describes the caller. The `/api/v1/mocks…` and `/api/v1/llm-profiles…` paths are deprecated aliases for workspace `default`. Every `/api/v1` request needs credentials (401 otherwise) and a sufficient role in the workspace (403 otherwise). Errors are RFC 9457 `application/problem+json`."),
    paths(
        workspaces::list,
        workspaces::create,
        workspaces::get_one,
        workspaces::patch,
        workspaces::delete,
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
        live::stream,
        apikeys::list,
        apikeys::create,
        apikeys::revoke,
        audit::list,
        audit::list_global,
        auth_routes::me,
    ),
    components(schemas(
        problem::Problem,
        workspaces::WorkspaceView,
        workspaces::CreateWorkspace,
        workspaces::PatchWorkspace,
        workspaces::RoleSchema,
        mocks::DataPlaneAuthSchema,
        mocks::DataPlaneAuthModeSchema,
        Source,
        VersionStatus,
        mocks::MockView,
        mocks::CreateMock,
        mocks::PatchMock,
        contracts::UploadResult,
        contracts::ContractVersionSummary,
        contracts::ContractVersionDetail,
        contracts::OperationSummary,
        scenarios::ScenarioView,
        scenarios::PutScenario,
        profiles::LlmProfileView,
        profiles::PutProfile,
        profiles::ProfileWriteResult,
        profiles::ProfileDeleteResult,
        sessions::SessionInfoView,
        sessions::StoredResourceView,
        requests::RequestLogView,
        live::LiveRequestView,
        apikeys::KeyScopeSchema,
        apikeys::CreateApiKey,
        apikeys::ApiKeyView,
        apikeys::CreatedApiKey,
        audit::AuditEventView,
        audit::AuditFormat,
        auth_routes::MeView,
        auth_routes::MeWorkspace,
        auth_routes::PrincipalKindSchema,
    )),
    tags(
        (name = "workspaces"),
        (name = "mocks"),
        (name = "contracts"),
        (name = "scenarios"),
        (name = "llm-profiles"),
        (name = "sessions"),
        (name = "requests"),
        (name = "api-keys"),
        (name = "audit"),
        (name = "me"),
    )
)]
pub struct ApiDoc;

/// Where the canonical workspace resource paths live.
const WORKSPACE_PREFIX: &str = "/api/v1/workspaces/{ws}";

/// Resources whose P2 paths stay as aliases for `default`.
const ALIASED: [&str; 2] = ["/mocks", "/llm-profiles"];

/// The admin API's OpenAPI document: [`ApiDoc`] plus, for every canonical
/// `mocks`/`llm-profiles` path, its deprecated `/api/v1/…` alias without
/// the `ws` parameter and with operation ids suffixed `_default`.
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    let mut doc = ApiDoc::openapi();
    let aliases: Vec<(String, PathItem)> = doc
        .paths
        .paths
        .iter()
        .filter_map(|(path, item)| {
            let rest = path.strip_prefix(WORKSPACE_PREFIX)?;
            ALIASED
                .iter()
                .any(|r| rest.starts_with(r))
                .then(|| (format!("{API_BASE}{rest}"), alias(item.clone())))
        })
        .collect();
    doc.paths.paths.extend(aliases);
    doc
}

/// `item` as a deprecated alias for workspace `default`.
fn alias(mut item: PathItem) -> PathItem {
    item.parameters = item.parameters.take().map(without_ws);
    for op in [
        &mut item.get,
        &mut item.put,
        &mut item.post,
        &mut item.delete,
        &mut item.options,
        &mut item.head,
        &mut item.patch,
        &mut item.trace,
        &mut item.query,
    ]
    .into_iter()
    .flatten()
    {
        alias_operation(op);
    }
    item.additional_operations
        .values_mut()
        .for_each(alias_operation);
    item
}

fn alias_operation(op: &mut Operation) {
    op.parameters = op.parameters.take().map(without_ws);
    op.deprecated = Some(Deprecated::True);
    op.operation_id = op.operation_id.take().map(|id| format!("{id}_default"));
}

fn without_ws(
    params: Vec<RefOr<utoipa::openapi::path::Parameter>>,
) -> Vec<RefOr<utoipa::openapi::path::Parameter>> {
    params
        .into_iter()
        .filter(
            |p| !matches!(p, RefOr::T(p) if p.name == "ws" && p.parameter_in == ParameterIn::Path),
        )
        .collect()
}

/// The document, serialised once.
static DOCUMENT: LazyLock<String> =
    LazyLock::new(|| openapi_document().to_json().unwrap_or_else(|_| "{}".into()));

pub async fn serve() -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        DOCUMENT.as_str(),
    )
        .into_response()
}
