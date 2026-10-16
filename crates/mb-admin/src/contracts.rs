//! `/api/v1/workspaces/{ws}/mocks/{mock}/contracts…`: upload, list, inspect and activate
//! contract versions.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_mcm::{Binding, Bundle, Effect, HttpMethod, OperationKind};
use mb_store::{ContractStatus, ContractVersion, Mock, NewContractVersion};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::access::WsAccess;
use crate::mocks::MockPath;
use crate::problem::{
    ApiResult, Json, Path, Problem, Query, RequireJson, media_type, unsupported_media_type,
};
use crate::{AdminState, VersionStatus, ensure_api_managed};

/// Largest accepted contract upload.
pub const MAX_CONTRACT_BYTES: usize = 10 * 1024 * 1024;

/// Media types a contract upload may declare. None is CORS-safelisted, so
/// a cross-site page cannot upload without a preflight.
pub const CONTRACT_MEDIA_TYPES: [&str; 7] = [
    "application/json",
    "application/yaml",
    "application/x-yaml",
    "text/yaml",
    "application/vnd.oai.openapi",
    "application/vnd.oai.openapi+json",
    "application/vnd.oai.openapi+yaml",
];

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct UploadParams {
    /// Activate the new version. The first version of a mock (or any upload
    /// while no version is active) is activated regardless.
    pub activate: Option<bool>,
}

/// Path of one contract version.
#[derive(Deserialize, Debug)]
pub struct VersionPath {
    pub mock: String,
    pub version: i64,
}

/// The outcome of a contract upload.
#[derive(Serialize, ToSchema, Debug)]
pub struct UploadResult {
    pub version: i64,
    pub status: VersionStatus,
    /// Number of operations in the compiled contract.
    pub operations: usize,
}

/// A contract version without its source or compiled bundle.
#[derive(Serialize, ToSchema, Debug)]
pub struct ContractVersionSummary {
    pub version: i64,
    pub status: VersionStatus,
    /// Hex sha256 of the source.
    pub source_digest: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Number of operations in the compiled contract.
    pub operations: usize,
}

/// One operation of a compiled contract.
#[derive(Serialize, ToSchema, Debug, PartialEq)]
pub struct OperationSummary {
    pub id: String,
    /// Upper-case HTTP method.
    pub method: String,
    /// Path template with `{param}` placeholders.
    pub path: String,
    /// `query`, `create`, `update`, `delete` or `other`.
    pub kind: String,
}

/// A contract version's metadata and operations.
#[derive(Serialize, ToSchema, Debug)]
pub struct ContractVersionDetail {
    pub version: i64,
    pub status: VersionStatus,
    pub source_digest: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    /// The contract's `info.title`.
    pub title: String,
    /// The contract's `info.version`.
    pub api_version: String,
    pub operations: Vec<OperationSummary>,
}

impl From<&ContractVersion> for ContractVersionSummary {
    fn from(v: &ContractVersion) -> Self {
        Self {
            version: v.version,
            status: VersionStatus::from(&v.status),
            source_digest: v.source_digest.clone(),
            created_by: v.created_by.clone(),
            created_at: v.created_at,
            operations: v.bundle.operations.len(),
        }
    }
}

fn operations(bundle: &Bundle) -> Vec<OperationSummary> {
    bundle
        .operations
        .iter()
        .map(|op| {
            let (method, path) = match &op.binding {
                Binding::Http { method, path } => (method_name(*method), path.clone()),
                _ => ("", String::new()),
            };
            let kind = match &op.kind {
                OperationKind::Query => "query",
                OperationKind::Command(Effect::Create) => "create",
                OperationKind::Command(Effect::Update) => "update",
                OperationKind::Command(Effect::Delete) => "delete",
                OperationKind::Command(Effect::Other) => "other",
            };
            OperationSummary {
                id: op.id.clone(),
                method: method.into(),
                path,
                kind: kind.into(),
            }
        })
        .collect()
}

fn method_name(m: HttpMethod) -> &'static str {
    match m {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Head => "HEAD",
        HttpMethod::Options => "OPTIONS",
    }
}

async fn version(st: &AdminState, mock: &Mock, version: i64) -> ApiResult<ContractVersion> {
    st.store
        .get_contract_version(mock.id, version)
        .await?
        .ok_or_else(|| {
            Problem::not_found(format!(
                "mock {:?} has no contract version {version}",
                mock.name
            ))
        })
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        UploadParams,
    ),
    request_body(
        description = "OpenAPI 3.x or Swagger 2.0 document, JSON or YAML, at most 10 MiB. Content-Type must be application/json, application/yaml, application/x-yaml, text/yaml or application/vnd.oai.openapi(+json|+yaml).",
        content(
            (String = "application/json"),
            (String = "application/yaml"),
            (String = "application/x-yaml"),
            (String = "text/yaml"),
            (String = "application/vnd.oai.openapi"),
            (String = "application/vnd.oai.openapi+json"),
            (String = "application/vnd.oai.openapi+yaml"),
        )
    ),
    responses(
        (status = 201, body = UploadResult),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 413, body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "Not a JSON, YAML or OpenAPI media type", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The contract does not compile", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn upload(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
    Query(params): Query<UploadParams>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<impl IntoResponse> {
    access.require(Permission::Edit)?;
    let media = media_type(headers.get(header::CONTENT_TYPE));
    if !media
        .as_deref()
        .is_some_and(|m| CONTRACT_MEDIA_TYPES.contains(&m))
    {
        return Err(unsupported_media_type(
            "a contract upload",
            &CONTRACT_MEDIA_TYPES.join(", "),
        ));
    }
    let m = st.mock(&access.ws, &mock).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    let source = axum::body::to_bytes(body, MAX_CONTRACT_BYTES)
        .await
        .map_err(|e| {
            Problem::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "contract upload exceeds {MAX_CONTRACT_BYTES} bytes or could not be read: {e}"
                ),
            )
        })?
        .to_vec();
    // Compiling a large contract is CPU-bound: keep it off the runtime.
    let (source, compiled) = tokio::task::spawn_blocking(move || {
        let compiled = mb_import::compile(&source);
        (source, compiled)
    })
    .await
    .map_err(|e| {
        Problem::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("contract compilation did not finish: {e}"),
        )
    })?;
    let bundle =
        compiled.map_err(|e| Problem::unprocessable(format!("contract does not compile: {e}")))?;
    let operations = bundle.operations.len();

    let added = st
        .store
        .add_contract_version(
            m.id,
            NewContractVersion {
                source,
                bundle,
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await?;
    let activate =
        params.activate.unwrap_or(false) || st.store.active_contract(m.id).await?.is_none();
    let stored = if activate {
        st.store
            .activate_contract_version(m.id, added.version)
            .await?
    } else {
        added
    };
    Ok((
        StatusCode::CREATED,
        Json(UploadResult {
            version: stored.version,
            status: VersionStatus::from(&stored.status),
            operations,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    responses(
        (status = 200, description = "Newest first", body = Vec<ContractVersionSummary>),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
) -> ApiResult<Json<Vec<ContractVersionSummary>>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let versions = st.store.list_contract_versions(m.id).await?;
    Ok(Json(versions.iter().map(Into::into).collect()))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = i64, Path, description = "Contract version number"),
    ),
    responses(
        (status = 200, body = ContractVersionDetail),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_one(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(VersionPath {
        mock,
        version: number,
    }): Path<VersionPath>,
) -> ApiResult<Json<ContractVersionDetail>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = version(&st, &m, number).await?;
    Ok(Json(ContractVersionDetail {
        version: v.version,
        status: VersionStatus::from(&v.status),
        operations: operations(&v.bundle),
        title: v.bundle.title,
        api_version: v.bundle.version,
        source_digest: v.source_digest,
        created_by: v.created_by,
        created_at: v.created_at,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/source",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = i64, Path, description = "Contract version number"),
    ),
    responses(
        (status = 200, description = "The uploaded source bytes", body = String, content_type = "application/octet-stream"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn source(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(VersionPath {
        mock,
        version: number,
    }): Path<VersionPath>,
) -> ApiResult<Response> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = version(&st, &m, number).await?;
    let content_type = match v.source.iter().find(|b| !b.is_ascii_whitespace()) {
        Some(b'{') => "application/json",
        _ => "application/yaml",
    };
    Ok(([(header::CONTENT_TYPE, content_type)], v.source).into_response())
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/activate",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = i64, Path, description = "Contract version number"),
    ),
    responses(
        (status = 200, description = "Now the only active version; the previous one is approved", body = ContractVersionSummary),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A draft, or managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "Content-Type is not application/json (the body is ignored)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn activate(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(VersionPath {
        mock,
        version: number,
    }): Path<VersionPath>,
    _json: RequireJson,
) -> ApiResult<Json<ContractVersionSummary>> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = version(&st, &m, number).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    if v.status == ContractStatus::Draft {
        return Err(Problem::conflict(format!(
            "contract version {number} of mock {mock:?} is a draft and cannot be activated"
        )));
    }
    let active = st.store.activate_contract_version(m.id, number).await?;
    Ok(Json((&active).into()))
}
