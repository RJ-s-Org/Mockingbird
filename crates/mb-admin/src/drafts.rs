//! `/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/draft|approve|preview`:
//! review, edit, approve and preview imported draft contract versions.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mb_auth::Permission;
use mb_engine::{EngineError, GenerationContext, OperationCall};
use mb_ext::{CallInfo, UsageEvent};
use mb_import::smart::assemble::{DraftIssue, compile_draft, diff_bundles, refresh_meta};
use mb_import::smart::model::DraftMeta;
use mb_mcm::Binding;
use mb_store::{ContractStatus, ContractVersion, Mock, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::access::WsAccess;
use crate::audit::snapshot;
use crate::contracts::{ContractVersionSummary, VersionPath, method_name, version};
use crate::imports::ImportKindSchema;
use crate::problem::{ApiResult, Json, PROBLEM_CONTENT_TYPE, Path, Problem, RequireJson};
use crate::{AdminState, VersionStatus, ensure_api_managed};

/// Where in the source an operation was found.
#[derive(Serialize, ToSchema, Debug)]
pub struct ProvenanceView {
    /// The documentation chunk (absent for HAR and Postman imports).
    pub chunk: Option<u32>,
    /// e.g. `entry 2`, a heading, or a collection item.
    pub location: String,
    /// At most 300 characters of the source.
    pub excerpt: String,
}

/// One operation of a draft with its review metadata.
#[derive(Serialize, ToSchema, Debug)]
pub struct DraftOperationView {
    pub id: String,
    /// Upper-case HTTP method.
    pub method: String,
    /// Path template with `{param}` placeholders.
    pub path: String,
    pub summary: Option<String>,
    /// 0..=1; 1.0 for operations recorded in traffic or edited by a person.
    pub confidence: f32,
    /// Added or changed by an edit of the draft.
    pub edited: bool,
    pub provenance: Vec<ProvenanceView>,
}

/// A problem the importer noted.
#[derive(Serialize, ToSchema, Debug)]
pub struct DraftWarningView {
    /// e.g. `chunk_skipped`, `invalid_schema`, `ungrounded_operation`.
    pub code: String,
    pub message: String,
    /// The target (`GET /pets`) of the operation the warning is about.
    pub operation: Option<String>,
}

/// LLM tokens spent by the import.
#[derive(Serialize, ToSchema, Debug)]
pub struct TokenUsageView {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// How a draft was produced.
#[derive(Serialize, ToSchema, Debug)]
pub struct DraftMetaView {
    pub source_kind: ImportKindSchema,
    pub source_name: Option<String>,
    pub source_url: Option<String>,
    pub chunk_count: u32,
    pub chunks_processed: u32,
    pub warnings: Vec<DraftWarningView>,
    pub usage: TokenUsageView,
    /// Whether the draft was edited through the API.
    pub edited: bool,
}

/// What a draft changes compared with the active version.
#[derive(Serialize, ToSchema, Debug)]
pub struct DraftDiffView {
    pub against_version: i64,
    /// Operation targets (`GET /pets/{petId}`) only in the draft.
    pub added: Vec<String>,
    /// Operation targets only in the active version.
    pub removed: Vec<String>,
    /// Operation targets in both whose definition differs.
    pub changed: Vec<String>,
}

/// An imported contract version for review.
#[derive(Serialize, ToSchema, Debug)]
pub struct DraftView {
    pub version: i64,
    pub status: VersionStatus,
    /// The OpenAPI 3.1 document of the draft.
    #[schema(value_type = Object)]
    pub document: Value,
    pub meta: DraftMetaView,
    /// Sorted by path, then method.
    pub operations: Vec<DraftOperationView>,
    /// Against the active version; absent when there is none or this version
    /// is active.
    pub diff: Option<DraftDiffView>,
}

/// A replacement draft document.
#[derive(Deserialize, ToSchema, Debug)]
pub struct PutDraft {
    /// An OpenAPI 3.x document.
    #[schema(value_type = Object)]
    pub document: Value,
}

/// One problem in a draft document.
#[derive(Serialize, ToSchema, Debug)]
pub struct DraftIssueView {
    /// JSON pointer into the submitted document.
    pub pointer: String,
    pub message: String,
}

/// 422 body: a Problem plus `errors`.
#[derive(Serialize, ToSchema, Debug)]
pub struct DraftProblem {
    /// Always `about:blank`.
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    pub errors: Vec<DraftIssueView>,
}

/// What to preview.
#[derive(Deserialize, ToSchema, Debug)]
pub struct PreviewRequest {
    pub operation_id: String,
    /// A declared status to generate instead of the success response.
    pub status: Option<u16>,
}

/// A generated response; no session state is read or written.
#[derive(Serialize, ToSchema, Debug)]
pub struct PreviewResponse {
    pub status: u16,
    pub content_type: Option<String>,
    #[schema(value_type = Object)]
    pub body: Option<Value>,
    /// `llm`, `repaired` or `fallback`.
    pub source: String,
}

/// The error of a draft edit: a problem, or the issues of an invalid draft.
#[derive(Debug)]
pub enum DraftError {
    Problem(Problem),
    Invalid(Vec<DraftIssue>),
}

impl From<Problem> for DraftError {
    fn from(p: Problem) -> Self {
        Self::Problem(p)
    }
}

impl From<StoreError> for DraftError {
    fn from(e: StoreError) -> Self {
        Self::Problem(e.into())
    }
}

impl IntoResponse for DraftError {
    fn into_response(self) -> Response {
        match self {
            Self::Problem(p) => p.into_response(),
            Self::Invalid(issues) => {
                let status = StatusCode::UNPROCESSABLE_ENTITY;
                let body = DraftProblem {
                    kind: "about:blank".into(),
                    title: status.canonical_reason().unwrap_or("Error").into(),
                    status: status.as_u16(),
                    detail: format!("the draft has {} problem(s)", issues.len()),
                    errors: issues
                        .into_iter()
                        .map(|i| DraftIssueView {
                            pointer: i.pointer,
                            message: i.message,
                        })
                        .collect(),
                };
                let body = serde_json::to_vec(&body).unwrap_or_default();
                (status, [(header::CONTENT_TYPE, PROBLEM_CONTENT_TYPE)], body).into_response()
            }
        }
    }
}

fn internal(detail: String) -> Problem {
    Problem::new(StatusCode::INTERNAL_SERVER_ERROR, detail)
}

/// The draft metadata of `v`, or a 404 when it was not imported.
fn draft_meta(mock: &str, v: &ContractVersion) -> ApiResult<DraftMeta> {
    let meta = v.draft_meta.clone().ok_or_else(|| {
        Problem::not_found(format!(
            "contract version {} of mock {mock:?} is not an imported draft",
            v.version
        ))
    })?;
    serde_json::from_value(meta).map_err(|e| {
        internal(format!(
            "the draft metadata of contract version {} is invalid: {e}",
            v.version
        ))
    })
}

fn meta_view(meta: &DraftMeta) -> DraftMetaView {
    DraftMetaView {
        source_kind: ImportKindSchema::from_job_kind(meta.source_kind.as_str()),
        source_name: meta.source_name.clone(),
        source_url: meta.source_url.clone(),
        chunk_count: meta.chunk_count,
        chunks_processed: meta.chunks_processed,
        warnings: meta
            .warnings
            .iter()
            .map(|w| DraftWarningView {
                code: w.code.clone(),
                message: w.message.clone(),
                operation: w.operation.clone(),
            })
            .collect(),
        usage: TokenUsageView {
            prompt_tokens: meta.usage.prompt_tokens,
            completion_tokens: meta.usage.completion_tokens,
        },
        edited: meta.edited,
    }
}

fn operation_views(v: &ContractVersion, meta: &DraftMeta) -> Vec<DraftOperationView> {
    let mut ops: Vec<DraftOperationView> = v
        .bundle
        .operations
        .iter()
        .map(|op| {
            let (method, path) = match &op.binding {
                Binding::Http { method, path } => (method_name(*method), path.clone()),
                _ => ("", String::new()),
            };
            let review = meta.operations.get(&op.id);
            DraftOperationView {
                id: op.id.clone(),
                method: method.into(),
                path,
                summary: op.summary.clone(),
                confidence: review.map_or(1.0, |r| r.confidence),
                edited: review.is_some_and(|r| r.edited),
                provenance: review
                    .map(|r| {
                        r.provenance
                            .iter()
                            .map(|p| ProvenanceView {
                                chunk: p.chunk,
                                location: p.location.clone(),
                                excerpt: p.excerpt.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .collect();
    ops.sort_by(|a, b| (&a.path, &a.method).cmp(&(&b.path, &b.method)));
    ops
}

async fn draft_view(
    st: &AdminState,
    mock: &Mock,
    v: &ContractVersion,
    meta: &DraftMeta,
) -> ApiResult<DraftView> {
    let document: Value = serde_json::from_slice(&v.source).map_err(|e| {
        internal(format!(
            "the source of contract version {} is not JSON: {e}",
            v.version
        ))
    })?;
    let diff = match st.store.active_contract(mock.id).await? {
        Some(active) if active.version != v.version => {
            let d = diff_bundles(&active.bundle, &v.bundle);
            Some(DraftDiffView {
                against_version: active.version,
                added: d.added,
                removed: d.removed,
                changed: d.changed,
            })
        }
        _ => None,
    };
    Ok(DraftView {
        version: v.version,
        status: VersionStatus::from(&v.status),
        document,
        meta: meta_view(meta),
        operations: operation_views(v, meta),
        diff,
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/draft",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = i64, Path, description = "Contract version number"),
    ),
    responses(
        (status = 200, body = DraftView),
        (status = 404, description = "No such version, or not an imported draft", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_draft(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(VersionPath {
        mock,
        version: number,
    }): Path<VersionPath>,
) -> ApiResult<Json<DraftView>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = version(&st, &m, number).await?;
    let meta = draft_meta(&mock, &v)?;
    Ok(Json(draft_view(&st, &m, &v, &meta).await?))
}

#[utoipa::path(
    put,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/draft",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = i64, Path, description = "Contract version number"),
    ),
    request_body = PutDraft,
    responses(
        (status = 200, description = "The recompiled draft; changed and added operations are marked edited", body = DraftView),
        (status = 404, description = "No such version, or not an imported draft", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Not a draft, or managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 415, body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The document does not compile", body = DraftProblem, content_type = "application/problem+json"),
    )
)]
pub async fn put_draft(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(VersionPath {
        mock,
        version: number,
    }): Path<VersionPath>,
    Json(body): Json<PutDraft>,
) -> Result<Json<DraftView>, DraftError> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    let v = version(&st, &m, number).await?;
    let meta = draft_meta(&mock, &v)?;
    if v.status != ContractStatus::Draft {
        return Err(Problem::conflict(format!(
            "contract version {number} of mock {mock:?} is not a draft and cannot be edited"
        ))
        .into());
    }
    // Compiling a large document is CPU-bound: keep it off the runtime.
    let document = body.document;
    let compiled = tokio::task::spawn_blocking(move || compile_draft(&document))
        .await
        .map_err(|e| internal(format!("draft compilation did not finish: {e}")))?;
    let (source, bundle) = compiled.map_err(DraftError::Invalid)?;
    let new_meta = refresh_meta(&meta, &v.bundle, &bundle);
    let meta_value = serde_json::to_value(&new_meta)
        .map_err(|e| internal(format!("the draft metadata is invalid: {e}")))?;
    let updated = st
        .store
        .update_draft_contract_version(m.id, number, source, bundle, meta_value)
        .await?;
    access
        .audit(
            &st.audit,
            "contract.draft_update",
            format!("contract:{mock}@{number}"),
            snapshot(&ContractVersionSummary::from(&v)),
            snapshot(&ContractVersionSummary::from(&updated)),
        )
        .await;
    Ok(Json(draft_view(&st, &m, &updated, &new_meta).await?))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/approve",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = i64, Path, description = "Contract version number"),
    ),
    responses(
        (status = 200, description = "Approved; activate it to serve it", body = ContractVersionSummary),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Not a draft, or managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "Content-Type is not application/json (the body is ignored)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn approve(
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
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    let v = version(&st, &m, number).await?;
    let approved = st.store.approve_contract_version(m.id, number).await?;
    let view = ContractVersionSummary::from(&approved);
    access
        .audit(
            &st.audit,
            "contract.approve",
            format!("contract:{mock}@{number}"),
            snapshot(&ContractVersionSummary::from(&v)),
            snapshot(&view),
        )
        .await;
    Ok(Json(view))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/preview",
    tag = "contracts",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("version" = i64, Path, description = "Contract version number"),
    ),
    request_body = PreviewRequest,
    responses(
        (status = 200, description = "A generated response (may spend LLM tokens); no session state is read or written", body = PreviewResponse),
        (status = 404, description = "No such version or operation", body = Problem, content_type = "application/problem+json"),
        (status = 415, body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The status is not declared by the operation, or it has no response", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "The quota refused the preview (another status is possible when a quota extension uses one)", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds to wait, when the quota says"))),
        (status = 502, description = "The mock's LLM is unavailable", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn preview(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(VersionPath {
        mock,
        version: number,
    }): Path<VersionPath>,
    headers: HeaderMap,
    _json: RequireJson,
    Json(body): Json<PreviewRequest>,
) -> ApiResult<Json<PreviewResponse>> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    let v = version(&st, &m, number).await?;
    let op = v
        .bundle
        .operations
        .iter()
        .find(|op| op.id == body.operation_id)
        .ok_or_else(|| {
            Problem::not_found(format!(
                "contract version {number} of mock {mock:?} has no operation {:?}",
                body.operation_id
            ))
        })?;
    // Admitted and reported like a data-plane call of the operation.
    let tenancy = crate::tenancy(&access.ws);
    let call_info = CallInfo {
        mock: &mock,
        operation_id: &op.id,
        headers: &headers,
    };
    st.ext
        .quota
        .admit(&tenancy, &call_info)
        .await
        .map_err(Problem::from)?;
    let engine = st
        .llm
        .preview_engine(&m)
        .await
        .map_err(|e| Problem::new(StatusCode::BAD_GATEWAY, e))?;
    let started = std::time::Instant::now();
    let call = OperationCall {
        operation_id: body.operation_id.clone(),
        ..Default::default()
    };
    let ctx = GenerationContext {
        forced_status: body.status,
        ..Default::default()
    };
    let generated = engine
        .generate(&v.bundle, op, &call, ctx)
        .await
        .map_err(|e| match e {
            EngineError::UndeclaredStatus(_) | EngineError::NoOutput => {
                Problem::unprocessable(e.to_string())
            }
            EngineError::Schema(_) => internal(e.to_string()),
        })?;
    st.ext.usage.record(&UsageEvent {
        tenancy,
        mock: mock.clone(),
        operation_id: op.id.clone(),
        status: generated.status,
        source: generated.source.as_str().to_string(),
        latency: started.elapsed(),
        prompt_tokens: generated.usage.prompt_tokens,
        completion_tokens: generated.usage.completion_tokens,
    });
    Ok(Json(PreviewResponse {
        status: generated.status,
        content_type: generated.content_type,
        body: generated.body,
        source: generated.source.as_str().into(),
    }))
}
