//! `/api/v1/workspaces/{ws}/mocks/{mock}/imports…`: queue imports of API
//! documentation, HAR recordings and Postman collections, follow and cancel
//! them. The in-process workers in [`worker`] turn each job into a draft
//! contract version.

pub mod llm;
pub mod worker;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_ext::CallInfo;
use mb_import::smart::fetch::{FetchPolicy, validate_url};
use mb_import::smart::redact::redacted_url;
use mb_import::smart::{ImportKind, MAX_IMPORT_BYTES, SmartError, detect_kind};
use mb_store::{ImportJob, ImportJobStatus, Mock, NewImportJob};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::AdminState;
use crate::access::WsAccess;
use crate::audit::snapshot;
use crate::ensure_api_managed;
use crate::mocks::MockPath;
use crate::problem::{ApiResult, Json, Path, Problem, media_type, unsupported_media_type};

/// Largest accepted HTTP body of `POST …/imports` (base64 and multipart
/// framing around a [`MAX_IMPORT_BYTES`] input).
pub const MAX_IMPORT_REQUEST_BYTES: usize = 28 * 1024 * 1024;
/// Header a multipart upload must carry. Multipart is CORS-safelisted; a
/// custom header forces a preflight, so a cross-site form cannot import.
pub const UPLOAD_HEADER: &str = "x-mockingbird-upload";

/// Media types `POST …/imports` accepts.
const ACCEPTED_MEDIA: &str = "application/json, multipart/form-data";
/// Jobs listed by `GET …/imports`.
const LIST_LIMIT: u32 = 50;
/// The job kind of an import from a URL.
const URL_KIND: &str = "url";
/// The operation id import jobs are admitted under and their LLM calls
/// reported with (`CallInfo::operation_id`, `UsageEvent::operation_id` and
/// `UsageEvent::source`).
pub const IMPORT_OPERATION: &str = "import";

/// How imports run (`import:` in the config file).
#[derive(Clone, Debug)]
pub struct ImportSettings {
    /// Worker tasks claiming jobs.
    pub workers: usize,
    /// Tokens one job's LLM calls may spend.
    pub max_tokens: u64,
    /// Limits and allowlist of URL imports.
    pub fetch: FetchPolicy,
    /// A running job not updated for this long is failed.
    pub stale_after: Duration,
    /// How often an idle worker looks for jobs (it is also woken on create).
    pub poll_interval: Duration,
    /// How long finished jobs are kept.
    pub retention: Duration,
    /// Most queued plus running jobs one mock may have; past it, creating a
    /// job is refused with 429.
    pub max_active_jobs_per_mock: u32,
}

/// Default [`ImportSettings::max_active_jobs_per_mock`].
pub const DEFAULT_MAX_ACTIVE_JOBS_PER_MOCK: u32 = 10;

impl Default for ImportSettings {
    fn default() -> Self {
        Self {
            workers: 2,
            max_tokens: mb_import::smart::DEFAULT_IMPORT_TOKENS,
            fetch: FetchPolicy::default(),
            stale_after: Duration::from_secs(10 * 60),
            poll_interval: Duration::from_secs(2),
            retention: Duration::from_secs(7 * 24 * 60 * 60),
            max_active_jobs_per_mock: DEFAULT_MAX_ACTIVE_JOBS_PER_MOCK,
        }
    }
}

/// Import settings and the signal that wakes idle workers.
#[derive(Clone, Default)]
pub struct Imports {
    pub settings: ImportSettings,
    pub wake: Arc<tokio::sync::Notify>,
}

/// What an import reads.
#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportKindSchema {
    Markdown,
    Html,
    Pdf,
    Text,
    Har,
    Postman,
    /// A document fetched from a URL (its kind is decided after fetching).
    Url,
}

impl ImportKindSchema {
    /// The document kind; `None` for `url`.
    fn document_kind(self) -> Option<ImportKind> {
        match self {
            Self::Markdown => Some(ImportKind::Markdown),
            Self::Html => Some(ImportKind::Html),
            Self::Pdf => Some(ImportKind::Pdf),
            Self::Text => Some(ImportKind::Text),
            Self::Har => Some(ImportKind::Har),
            Self::Postman => Some(ImportKind::Postman),
            Self::Url => None,
        }
    }

    pub(crate) fn from_job_kind(kind: &str) -> Self {
        match ImportKind::parse(kind) {
            Some(ImportKind::Markdown) => Self::Markdown,
            Some(ImportKind::Html) => Self::Html,
            Some(ImportKind::Pdf) => Self::Pdf,
            Some(ImportKind::Text) => Self::Text,
            Some(ImportKind::Har) => Self::Har,
            Some(ImportKind::Postman) => Self::Postman,
            // Jobs are only created with the kinds above or `url`.
            None => Self::Url,
        }
    }

    /// The kind with the wire string `s`.
    fn parse(s: &str) -> Option<Self> {
        if s == URL_KIND {
            return Some(Self::Url);
        }
        ImportKind::parse(s).map(|k| Self::from_job_kind(k.as_str()))
    }
}

/// Where an import job is.
#[derive(Serialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatusSchema {
    Queued,
    Running,
    Done,
    Failed,
    Canceled,
}

impl From<&ImportJobStatus> for ImportStatusSchema {
    fn from(s: &ImportJobStatus) -> Self {
        match s {
            ImportJobStatus::Queued => Self::Queued,
            ImportJobStatus::Running => Self::Running,
            ImportJobStatus::Done => Self::Done,
            ImportJobStatus::Failed => Self::Failed,
            ImportJobStatus::Canceled => Self::Canceled,
        }
    }
}

/// A JSON import request: exactly one of `url`, `text` and
/// `content_base64`.
#[derive(Deserialize, ToSchema, Debug)]
pub struct CreateImport {
    /// An http(s) URL to fetch. `kind` must be absent or `url`.
    pub url: Option<String>,
    /// Required with `text` (markdown, html, text, har or postman); optional
    /// with `content_base64` (detected from the content and `filename`).
    pub kind: Option<ImportKindSchema>,
    /// The document itself, at most 20 MiB.
    pub text: Option<String>,
    /// The document, base64-encoded, at most 20 MiB decoded.
    pub content_base64: Option<String>,
    /// The document's file name, shown as the job's source.
    pub filename: Option<String>,
}

/// How far a job has got.
#[derive(Serialize, ToSchema, Debug)]
pub struct ImportProgressView {
    /// `queued`, `fetching`, `extracting`, `analyzing`, `summarizing` or
    /// `assembling`.
    pub phase: String,
    pub done: u32,
    pub total: u32,
}

/// An import job.
#[derive(Serialize, ToSchema, Debug)]
pub struct ImportJobView {
    pub id: Uuid,
    pub mock: String,
    pub kind: ImportKindSchema,
    /// The file name (control characters removed) or URL (query values
    /// replaced by `REDACTED`, fragment dropped) imported.
    pub source_name: Option<String>,
    pub status: ImportStatusSchema,
    pub progress: ImportProgressView,
    /// The draft contract version a `done` job produced.
    pub result_version: Option<i64>,
    /// Why a `failed` job failed.
    pub error: Option<String>,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl From<&ImportJob> for ImportJobView {
    fn from(j: &ImportJob) -> Self {
        Self {
            id: j.id,
            mock: j.mock.clone(),
            kind: ImportKindSchema::from_job_kind(&j.kind),
            source_name: j.source_name.clone(),
            status: ImportStatusSchema::from(&j.status),
            progress: ImportProgressView {
                phase: j.progress.phase.clone(),
                done: j.progress.done,
                total: j.progress.total,
            },
            result_version: j.result_version,
            error: j.error.clone(),
            created_by: j.created_by.clone(),
            created_at: j.created_at,
            started_at: j.started_at,
            finished_at: j.finished_at,
        }
    }
}

/// Path of one import job.
#[derive(Deserialize, Debug)]
pub struct JobPath {
    pub mock: String,
    pub job: Uuid,
}

/// A validated import request.
struct NewJob {
    /// `url`, or the document kind's wire string.
    kind: String,
    /// The document kind; `None` for a URL.
    document: Option<ImportKind>,
    source_name: Option<String>,
    input: Vec<u8>,
}

fn too_large() -> Problem {
    Problem::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        format!(
            "the import is larger than {} MiB",
            MAX_IMPORT_BYTES / (1024 * 1024)
        ),
    )
}

/// `name` without control characters (which Postgres `TEXT` cannot hold
/// in the case of NUL); `None` when nothing is left.
fn clean_name(name: String) -> Option<String> {
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    (!name.is_empty()).then_some(name)
}

/// A job of document `bytes`: the explicit kind, or the detected one.
fn document_job(
    mock: &str,
    kind: Option<ImportKind>,
    filename: Option<String>,
    bytes: Vec<u8>,
) -> ApiResult<NewJob> {
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err(too_large());
    }
    let filename = filename.and_then(clean_name);
    let kind = match kind.or_else(|| detect_kind(filename.as_deref(), &bytes)) {
        Some(kind) => kind,
        None => {
            return Err(Problem::unprocessable(match mb_import::sniff(&bytes) {
                Ok(format) => format!(
                    "this is a {format:?} contract; upload it to …/mocks/{mock}/contracts instead"
                ),
                Err(_) => "cannot tell what kind of document this is; pass \"kind\"".to_string(),
            }));
        }
    };
    Ok(NewJob {
        kind: kind.as_str().to_string(),
        document: Some(kind),
        source_name: filename,
        input: bytes,
    })
}

/// A job of a JSON [`CreateImport`].
fn json_job(mock: &str, body: CreateImport) -> ApiResult<NewJob> {
    let given = [
        body.url.is_some(),
        body.text.is_some(),
        body.content_base64.is_some(),
    ];
    if given.iter().filter(|g| **g).count() != 1 {
        return Err(Problem::bad_request(
            "pass exactly one of \"url\", \"text\" and \"content_base64\"",
        ));
    }
    if let Some(url) = body.url {
        if body.kind.is_some_and(|k| k != ImportKindSchema::Url) {
            return Err(Problem::bad_request(
                "a URL import takes no \"kind\" (or \"url\"): the kind is decided after fetching",
            ));
        }
        let url = validate_url(&url).map_err(|e| Problem::bad_request(e.to_string()))?;
        // Only the fetch sees the real URL (parsed, so control characters
        // are percent-encoded); the stored and audited name is redacted.
        return Ok(NewJob {
            kind: URL_KIND.to_string(),
            document: None,
            source_name: Some(redacted_url(&url)),
            input: url.to_string().into_bytes(),
        });
    }
    if let Some(text) = body.text {
        let kind = match body.kind.and_then(ImportKindSchema::document_kind) {
            Some(kind) if kind != ImportKind::Pdf => kind,
            _ => {
                return Err(Problem::bad_request(
                    "\"text\" needs \"kind\": markdown, html, text, har or postman",
                ));
            }
        };
        return document_job(mock, Some(kind), body.filename, text.into_bytes());
    }
    let encoded = body.content_base64.unwrap_or_default();
    let kind =
        match body.kind {
            None => None,
            Some(k) => Some(k.document_kind().ok_or_else(|| {
                Problem::bad_request("\"content_base64\" cannot have kind \"url\"")
            })?),
        };
    // Reject before decoding what cannot fit.
    if encoded.len() / 4 * 3 > MAX_IMPORT_BYTES + 3 {
        return Err(too_large());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| Problem::bad_request(format!("\"content_base64\" is not base64: {e}")))?;
    document_job(mock, kind, body.filename, bytes)
}

/// A job of a multipart upload: part `file`, optional part `kind`.
async fn multipart_job(mock: &str, mut form: Multipart) -> ApiResult<NewJob> {
    let multipart_error = |e: axum::extract::multipart::MultipartError| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            too_large()
        } else {
            Problem::bad_request(format!("malformed multipart body: {}", e.body_text()))
        }
    };
    let mut file: Option<(Option<String>, Vec<u8>)> = None;
    let mut kind: Option<ImportKind> = None;
    while let Some(mut field) = form.next_field().await.map_err(multipart_error)? {
        match field.name() {
            Some("file") => {
                let name = field.file_name().map(str::to_string);
                let mut bytes = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
                    if bytes.len() + chunk.len() > MAX_IMPORT_BYTES {
                        return Err(too_large());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                file = Some((name, bytes));
            }
            Some("kind") => {
                let text = field.text().await.map_err(multipart_error)?;
                kind = Some(
                    ImportKindSchema::parse(text.trim())
                        .and_then(ImportKindSchema::document_kind)
                        .ok_or_else(|| {
                            Problem::bad_request(format!(
                                "unknown kind {:?}: use markdown, html, pdf, text, har or postman",
                                text.trim()
                            ))
                        })?,
                );
            }
            other => {
                return Err(Problem::bad_request(format!(
                    "unexpected part {:?}: send part \"file\" and optionally \"kind\"",
                    other.unwrap_or_default()
                )));
            }
        }
    }
    let (name, bytes) =
        file.ok_or_else(|| Problem::bad_request("the upload has no part \"file\""))?;
    document_job(mock, kind, name, bytes)
}

async fn job_of(st: &AdminState, mock: &Mock, id: Uuid) -> ApiResult<ImportJob> {
    st.store
        .get_import_job(id)
        .await?
        .filter(|j| j.mock_id == mock.id)
        .ok_or_else(|| Problem::not_found(format!("mock {:?} has no import job {id}", mock.name)))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/imports",
    tag = "imports",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    request_body(
        description = "Either `application/json` (`CreateImport`), or `multipart/form-data` with part `file` (its file name becomes the job's source) and optional part `kind`, which must carry the header `X-Mockingbird-Upload` (any value). The input is at most 20 MiB; the request at most 28 MiB. Without a kind, the kind is detected from the content and file name.",
        content(
            (CreateImport = "application/json"),
            (Vec<u8> = "multipart/form-data"),
        )
    ),
    responses(
        (status = 202, description = "Queued; `Location` names the job", body = ImportJobView,
            headers(("Location" = String, description = "The job's URL"))),
        (status = 400, description = "Invalid request", body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 413, body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "Neither JSON nor multipart with the upload header", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unknown kind, a contract (upload it to …/contracts), or a document import without an LLM", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "The mock has `import.max_active_jobs_per_mock` queued or running jobs, or the quota refused the import (another status is possible when a quota extension uses one)", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds to wait, when the quota says"))),
    )
)]
pub async fn create(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
    headers: HeaderMap,
    req: Request,
) -> ApiResult<impl IntoResponse> {
    access.require(Permission::Edit)?;
    let media = media_type(headers.get(header::CONTENT_TYPE));
    let multipart = match media.as_deref() {
        Some("application/json") => false,
        Some("multipart/form-data") if headers.contains_key(UPLOAD_HEADER) => true,
        Some("multipart/form-data") => {
            return Err(Problem::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("a multipart import requires the header {UPLOAD_HEADER}"),
            ));
        }
        _ => return Err(unsupported_media_type("an import", ACCEPTED_MEDIA)),
    };
    let m = st.mock(&access.ws, &mock).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;

    let job = if multipart {
        let form = Multipart::from_request(req, &st)
            .await
            .map_err(|e| Problem::bad_request(e.body_text()))?;
        multipart_job(&mock, form).await?
    } else {
        let bytes = axum::body::to_bytes(req.into_body(), MAX_IMPORT_REQUEST_BYTES)
            .await
            .map_err(|e| {
                Problem::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!(
                        "the request exceeds {MAX_IMPORT_REQUEST_BYTES} bytes or could not be read: {e}"
                    ),
                )
            })?;
        let body: CreateImport = serde_json::from_slice(&bytes)
            .map_err(|e| Problem::bad_request(format!("invalid import request: {e}")))?;
        json_job(&mock, body)?
    };

    if job.document.is_some_and(ImportKind::needs_llm)
        && matches!(st.llm.for_import(&m).await, Ok(None))
    {
        return Err(Problem::unprocessable(SmartError::LlmRequired.to_string()));
    }

    // Checked just before queuing: concurrent creates can overshoot the
    // limit by at most the requests in flight.
    let limit = st.imports.settings.max_active_jobs_per_mock;
    if st.store.count_active_import_jobs(m.id).await? >= u64::from(limit) {
        return Err(Problem::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "mock {mock:?} already has {limit} queued or running import jobs; \
                 wait for one to finish or cancel one"
            ),
        ));
    }

    let call = CallInfo {
        mock: &mock,
        operation_id: IMPORT_OPERATION,
        headers: &headers,
    };
    st.ext
        .quota
        .admit(&crate::tenancy(&access.ws), &call)
        .await
        .map_err(Problem::from)?;

    let created = st
        .store
        .create_import_job(NewImportJob {
            mock_id: m.id,
            kind: job.kind,
            source_name: job.source_name,
            input: job.input,
            created_by: Some(access.principal.subject.clone()),
        })
        .await?;
    st.imports.wake.notify_one();
    let view = ImportJobView::from(&created);
    access
        .audit(
            &st.audit,
            "import.create",
            format!("import:{mock}/{}", created.id),
            None,
            snapshot(&view),
        )
        .await;
    let location = format!(
        "/api/v1/workspaces/{}/mocks/{mock}/imports/{}",
        access.ws, created.id
    );
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, location)],
        Json(view),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/imports",
    tag = "imports",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    responses(
        (status = 200, description = "The newest 50 jobs, newest first", body = Vec<ImportJobView>),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
) -> ApiResult<Json<Vec<ImportJobView>>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let jobs = st.store.list_import_jobs(m.id, LIST_LIMIT).await?;
    Ok(Json(jobs.iter().map(ImportJobView::from).collect()))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/imports/{job}",
    tag = "imports",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("job" = Uuid, Path, description = "Import job id"),
    ),
    responses(
        (status = 200, body = ImportJobView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_one(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(JobPath { mock, job }): Path<JobPath>,
) -> ApiResult<Json<ImportJobView>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let j = job_of(&st, &m, job).await?;
    Ok(Json(ImportJobView::from(&j)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/imports/{job}",
    tag = "imports",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("job" = Uuid, Path, description = "Import job id"),
    ),
    responses(
        (status = 200, description = "Canceled", body = ImportJobView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The job has already finished", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn cancel(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(JobPath { mock, job }): Path<JobPath>,
) -> ApiResult<Json<ImportJobView>> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    let before = job_of(&st, &m, job).await?;
    if !st.store.cancel_import_job(job, Utc::now()).await? {
        return Err(Problem::conflict(format!(
            "import job {job} has already finished"
        )));
    }
    let after = job_of(&st, &m, job).await?;
    let view = ImportJobView::from(&after);
    access
        .audit(
            &st.audit,
            "import.cancel",
            format!("import:{mock}/{job}"),
            snapshot(&ImportJobView::from(&before)),
            snapshot(&view),
        )
        .await;
    Ok(Json(view))
}
