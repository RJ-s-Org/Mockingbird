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
    API_BASE, Source, VersionStatus, apikeys, audit, auth_routes, console_sessions, contracts,
    drafts, imports, live, mocks, problem, profiles, requests, scenarios, sessions, workspaces,
};

#[derive(OpenApi)]
#[openapi(
    info(title = "Mockingbird Admin API", description = "Manage workspaces and, in each workspace, mocks, contract versions, import jobs, scenarios, LLM profiles, sessions, the request log and its live SSE tail, API keys and the audit log. Every successful mutation writes one audit event; `GET /api/v1/audit` lists every workspace's events (sysadmin only); `GET /api/v1/me` describes the caller; `/api/v1/console-sessions` lists and ends console login sessions. The `/api/v1/mocks…` and `/api/v1/llm-profiles…` paths are deprecated aliases for workspace `default`. Every `/api/v1` request needs credentials (401 otherwise) and a sufficient role in the workspace (403 otherwise). Errors are RFC 9457 `application/problem+json`."),
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
        contracts::delete,
        drafts::get_draft,
        drafts::put_draft,
        drafts::approve,
        drafts::preview,
        imports::create,
        imports::list,
        imports::get_one,
        imports::cancel,
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
        requests::get_one,
        live::stream,
        apikeys::list,
        apikeys::create,
        apikeys::revoke,
        audit::list,
        audit::list_global,
        auth_routes::me,
        console_sessions::list,
        console_sessions::revoke,
        console_sessions::revoke_all,
    ),
    components(schemas(
        problem::Problem,
        workspaces::WorkspaceView,
        workspaces::CreateWorkspace,
        workspaces::PatchWorkspace,
        workspaces::RoleSchema,
        mocks::DataPlaneAuthSchema,
        mocks::DataPlaneAuthModeSchema,
        mocks::ChaosSchema,
        mocks::PathRewriteSchema,
        mocks::LatencyJitterSchema,
        mocks::MockLimitsSchema,
        mocks::RateLimitSchema,
        Source,
        VersionStatus,
        mocks::MockView,
        mocks::CreateMock,
        mocks::PatchMock,
        contracts::UploadResult,
        contracts::ContractVersionSummary,
        contracts::ContractWarningView,
        drafts::ApproveRequest,
        contracts::ContractVersionDetail,
        contracts::OperationSummary,
        drafts::ProvenanceView,
        drafts::DraftOperationView,
        drafts::DraftWarningView,
        drafts::TokenUsageView,
        drafts::DraftMetaView,
        drafts::DraftDiffView,
        drafts::DraftView,
        drafts::PutDraft,
        drafts::DraftIssueView,
        drafts::DraftProblem,
        drafts::PreviewRequest,
        drafts::PreviewResponse,
        imports::ImportKindSchema,
        imports::ImportStatusSchema,
        imports::CreateImport,
        imports::ImportModeSchema,
        imports::ImportDocument,
        imports::ImportProgressView,
        imports::ImportJobView,
        contracts::ChannelSummary,
        scenarios::ScenarioView,
        scenarios::PutScenario,
        profiles::LlmProfileView,
        profiles::PutProfile,
        profiles::OutputMode,
        profiles::ProfileWriteResult,
        profiles::ProfileDeleteResult,
        sessions::SessionInfoView,
        sessions::StoredResourceView,
        requests::RequestLogView,
        requests::RequestLogSummaryView,
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
        console_sessions::ConsoleSessionView,
        console_sessions::RevokedSessions,
    )),
    tags(
        (name = "workspaces"),
        (name = "mocks"),
        (name = "contracts"),
        (name = "imports"),
        (name = "scenarios"),
        (name = "llm-profiles"),
        (name = "sessions"),
        (name = "requests"),
        (name = "api-keys"),
        (name = "audit"),
        (name = "me"),
        (name = "console-sessions"),
    )
)]
#[derive(Debug)]
pub struct ApiDoc;

/// Where the canonical workspace resource paths live.
const WORKSPACE_PREFIX: &str = "/api/v1/workspaces/{ws}";

/// Resources whose P2 paths stay as aliases for `default`.
const ALIASED: [&str; 2] = ["/mocks", "/llm-profiles"];

/// The admin API's OpenAPI document: [`ApiDoc`] plus, for every canonical
/// `mocks`/`llm-profiles` path, its deprecated `/api/v1/…` alias without
/// the `ws` parameter and with operation ids suffixed `_default`.
///
/// Operation ids are `<tag>_<handler>` (the tag in snake case, e.g.
/// `mocks_list`, `llm_profiles_put`), or just the handler when it is named
/// after its tag (`me`): generated clients key operations by id, and
/// several modules share handler names.
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    let mut doc = ApiDoc::openapi()
        .merge_from(crate::channels::ChannelsDoc::openapi())
        .merge_from(crate::webhook_deliveries::WebhookDeliveriesDoc::openapi());
    doc.paths
        .paths
        .values_mut()
        .flat_map(operations_mut)
        .for_each(qualify_operation_id);
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
    operations_mut(&mut item).for_each(alias_operation);
    item
}

/// Every operation of `item`.
fn operations_mut(item: &mut PathItem) -> impl Iterator<Item = &mut Operation> {
    [
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
    .chain(item.additional_operations.values_mut())
}

/// Prefixes `op`'s id with its first tag, unless the id already is the tag.
fn qualify_operation_id(op: &mut Operation) {
    let Some(tag) = op.tags.as_ref().and_then(|t| t.first()) else {
        return;
    };
    let tag = tag.replace('-', "_");
    op.operation_id = op
        .operation_id
        .take()
        .map(|id| if id == tag { id } else { format!("{tag}_{id}") });
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
