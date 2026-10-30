//! Mockingbird Admin REST API: workspaces, and in each workspace mocks,
//! contract versions, scenarios, LLM profiles, sessions and the request log
//! under `/api/v1/workspaces/{ws}` (the P2 paths under `/api/v1` act on
//! `default`), plus each workspace's API keys and audit log and the global
//! audit log at `/api/v1/audit`, the live request tail over SSE and the
//! caller's identity at `/api/v1/me`, described by the OpenAPI document at
//! `/api/openapi.json`, plus the OIDC browser login under `/auth`.
//! Every successful mutation writes one audit event.
//! Every `/api/v1` request is authenticated and checked against the caller's
//! workspace role. Every error is an RFC 9457 `application/problem+json`
//! document ([`Problem`]).
//!
//! [`router`] serves the whole API from an [`AdminState`]; the state's
//! defaults ([`AdminState::new`]) are `auth.mode: none`, no LLM and no
//! webhook:
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! let store = mb_store::connect("sqlite::memory:").await?;
//! let state = Arc::new(mb_state::MemoryStore::new(Duration::from_secs(3600)));
//! let app = mb_admin::router(mb_admin::AdminState::new(store, state));
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:9090").await?;
//! axum::serve(listener, app).await?;
//! # Ok(())
//! # }
//! ```

// No `clippy::missing_errors_doc`: the public handlers (`channels`,
// `imports`, `webhook_deliveries`) list their error answers in their
// `#[utoipa::path]` responses, and a doc comment on them would become the
// OpenAPI operation's description.
#![warn(missing_debug_implementations, clippy::missing_panics_doc)]

use std::sync::Arc;

use axum::Router;
use axum::routing::{any, get, post};
use mb_auth::{Authenticate, LocalAuth};
use mb_ext::{Extensions, Tenancy};
use mb_state::StateStore;
use mb_store::{ContractStatus, ControlStore, Mock, ResourceSource};
use serde::Serialize;
use utoipa::ToSchema;

pub mod access;
mod apikeys;
mod audit;
mod auth_routes;
pub mod channels;
mod contracts;
mod drafts;
pub mod imports;
mod live;
mod login_limit;
mod mocks;
mod openapi;
pub mod problem;
mod profiles;
mod requests;
mod scenarios;
mod sessions;
mod webhook;
pub mod webhook_deliveries;
mod workspaces;

pub use access::{CurrentPrincipal, WsAccess, authenticate};
pub use apikeys::{ApiKeyView, CreateApiKey, CreatedApiKey};
pub use audit::{AuditEventView, Auditor, retention_cutoff, spawn_retention};
pub use auth_routes::{
    LOGIN_STATE_COOKIE, MeView, MeWorkspace, WebAuth, auth_router, console_gate,
};
pub use imports::llm::{LlmAccessError, LlmResolver, OfflineLlm};
pub use imports::worker::{ImportWorkers, run_next_import, spawn_import_workers};
pub use imports::{
    CreateImport, DEFAULT_MAX_ACTIVE_JOBS_PER_MOCK, IMPORT_OPERATION, ImportJobView,
    ImportKindSchema, ImportProgressView, ImportSettings, ImportStatusSchema, Imports,
    MAX_IMPORT_REQUEST_BYTES, UPLOAD_HEADER,
};
pub use live::{LiveRequestView, LiveSubscribe, StreamQuery};
pub use login_limit::{LoginFailureLimiter, SUPPRESSED_METRIC as LOGIN_FAILED_SUPPRESSED_METRIC};
pub use openapi::{ApiDoc, openapi_document};
use problem::ApiResult;
pub use problem::Problem;
pub use scenarios::{invalid_scenario_set, undeclared_scenario_status};
pub use webhook::{WebhookSink, signature};
pub use workspaces::{CreateWorkspace, DeleteParams, PatchWorkspace, WorkspaceView};

/// Admin API base path.
pub const API_BASE: &str = "/api/v1";
/// Where the OpenAPI document is served.
pub const OPENAPI_PATH: &str = "/api/openapi.json";

/// Default `server.api_secret_refs`: the secret references an LLM profile
/// created or updated through the API may use.
pub const DEFAULT_API_SECRET_REFS: [&str; 2] = ["env:MB_LLM_*", "env:*_API_KEY"];

/// Default `server.api_webhook_secret_refs`: the secret references a mock's
/// webhook target set through the API may use.
pub const DEFAULT_API_WEBHOOK_SECRET_REFS: [&str; 2] = ["env:*_WEBHOOK_SECRET", "env:MB_WEBHOOK_*"];

/// Default [`AdminState::live_recheck`].
pub const DEFAULT_LIVE_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// Everything the Admin API handlers share. Its `Debug` output shows the
/// settings, not the stores or extension points.
#[derive(Clone)]
pub struct AdminState {
    pub store: Arc<dyn ControlStore>,
    pub state: Arc<dyn StateStore>,
    /// Globs (`*` matches any run of characters) a profile's `secret_ref`
    /// must match when written through the API (`server.api_secret_refs`).
    pub api_secret_refs: Arc<Vec<String>>,
    /// Globs a webhook target's `secret` must match when written through
    /// the API (`server.api_webhook_secret_refs`).
    pub api_webhook_secret_refs: Arc<Vec<String>>,
    /// Resolves the caller of every `/api/v1` request.
    pub auth: Arc<dyn Authenticate>,
    /// Records every successful mutation.
    pub audit: Auditor,
    /// Subscribes to served data-plane calls for the live request tail;
    /// `None` answers the tail with 503.
    pub live: Option<LiveSubscribe>,
    /// How often an open live tail re-authenticates its caller and re-checks
    /// its Read permission, ending the stream when either fails
    /// ([`DEFAULT_LIVE_RECHECK`]).
    pub live_recheck: std::time::Duration,
    /// Browser login (`/auth/*`); `None` in `auth.mode: none`.
    pub web_auth: Option<WebAuth>,
    /// Rate limit for `auth.login_failed` audit events.
    pub login_failures: Arc<LoginFailureLimiter>,
    /// LLM access of imports and draft previews ([`OfflineLlm`] by default).
    pub llm: Arc<dyn LlmResolver>,
    /// Import settings and the worker wake-up signal.
    pub imports: Imports,
    /// The extension points: import jobs and draft previews are admitted by
    /// `quota` and their LLM calls reported to `usage`, like data-plane
    /// calls ([`Extensions::default`] by default).
    pub ext: Extensions,
}

impl std::fmt::Debug for AdminState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminState")
            .field("api_secret_refs", &self.api_secret_refs)
            .field("api_webhook_secret_refs", &self.api_webhook_secret_refs)
            .field("audit", &self.audit)
            .field("live", &self.live.is_some())
            .field("live_recheck", &self.live_recheck)
            .field("web_auth", &self.web_auth)
            .field("imports", &self.imports)
            .finish_non_exhaustive()
    }
}

impl AdminState {
    /// A state with [`LocalAuth`] (`auth.mode: none`), the
    /// [`DEFAULT_API_SECRET_REFS`], an [`Auditor`] without a webhook, no
    /// live request feed, no browser login, no LLM ([`OfflineLlm`]) and the
    /// default [`ImportSettings`].
    pub fn new(store: Arc<dyn ControlStore>, state: Arc<dyn StateStore>) -> Self {
        Self {
            audit: Auditor::new(store.clone(), None),
            store,
            state,
            api_secret_refs: Arc::new(
                DEFAULT_API_SECRET_REFS
                    .iter()
                    .map(|r| r.to_string())
                    .collect(),
            ),
            api_webhook_secret_refs: Arc::new(
                DEFAULT_API_WEBHOOK_SECRET_REFS
                    .iter()
                    .map(|r| r.to_string())
                    .collect(),
            ),
            auth: Arc::new(LocalAuth),
            live: None,
            live_recheck: DEFAULT_LIVE_RECHECK,
            web_auth: None,
            login_failures: Arc::new(LoginFailureLimiter::new()),
            llm: Arc::new(OfflineLlm),
            imports: Imports::default(),
            ext: Extensions::default(),
        }
    }
}

/// The tenant admin-side LLM work is attributed to when no request is there
/// to resolve one from (an import job queued on another node or before a
/// restart). Imports and draft previews otherwise ask the
/// [`TenantResolver`](mb_ext::TenantResolver), which self-hosted
/// ([`SingleTenant`](mb_ext::SingleTenant)) answers with this tenant too.
pub const DEFAULT_TENANT: &str = "default";

/// The tenancy of a call in workspace `ws` with no request to resolve it
/// from: tenant [`DEFAULT_TENANT`].
pub(crate) fn tenancy(ws: &str) -> Tenancy {
    Tenancy {
        tenant: DEFAULT_TENANT.to_string(),
        workspace: ws.to_string(),
    }
}

/// The tenancy admin-side LLM work on `mock` in workspace `ws` is billed
/// to: the tenant the [`TenantResolver`](mb_ext::TenantResolver) resolves
/// for a call of `operation` with the admin request's `headers`, as on the
/// data plane, in the request's workspace.
///
/// # Errors
///
/// The resolver's rejection, as its problem.
pub(crate) async fn resolve_tenancy(
    st: &AdminState,
    ws: &str,
    mock: &str,
    operation: &str,
    headers: &axum::http::HeaderMap,
) -> Result<Tenancy, Problem> {
    let call = mb_ext::CallInfo::new(mock, operation, headers);
    let resolved = st.ext.tenants.resolve(&call).await.map_err(Problem::from)?;
    Ok(Tenancy {
        tenant: resolved.tenant,
        workspace: ws.to_string(),
    })
}

/// Whether `text` matches `pattern`, where `*` matches any run of
/// characters (including none) and every other character itself.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    // Where the last `*` was, and the text position it is matched up to.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// Mounts `/api/v1/*`, `/api/openapi.json` and the browser login routes
/// `/auth/login`, `/auth/callback` and `/auth/logout`.
///
/// The workspace resources are served under `/api/v1/workspaces/{ws}` and,
/// for `default`, under `/api/v1` (the P2 paths). Every `/api/v1` route
/// requires a principal (401 otherwise); `/api/openapi.json` is public, and
/// any other `/api/*` path is a 404 problem. The `/auth/*` routes are
/// public. Other paths are left to the caller's fallback.
///
/// Every mutating request needs a non-CORS-safelisted `Content-Type`
/// (`application/json`, or one of the contract media types for contract
/// uploads, see `contracts::CONTRACT_MEDIA_TYPES`: JSON, YAML, OpenAPI,
/// GraphQL, protobuf, XML/WSDL, zip, gzip or octet-stream; otherwise 415),
/// so a cross-site page cannot reach the API with a simple request.
///
/// Every `/api` response carries `X-Content-Type-Options: nosniff` and the
/// sandboxing [`API_CSP`]; contract sources download as attachments.
pub fn router(st: AdminState) -> Router {
    let import_body_limit = axum::extract::DefaultBodyLimit::max(MAX_IMPORT_REQUEST_BYTES);
    let resources: Router<AdminState> = Router::new()
        .route("/mocks", get(mocks::list).post(mocks::create))
        .route(
            "/mocks/{mock}",
            get(mocks::get_one)
                .patch(mocks::patch)
                .delete(mocks::delete),
        )
        .route(
            "/mocks/{mock}/contracts",
            get(contracts::list).post(contracts::upload),
        )
        .route(
            "/mocks/{mock}/contracts/{version}",
            get(contracts::get_one).delete(contracts::delete),
        )
        .route(
            "/mocks/{mock}/contracts/{version}/source",
            get(contracts::source),
        )
        .route(
            "/mocks/{mock}/contracts/{version}/activate",
            post(contracts::activate),
        )
        .route(
            "/mocks/{mock}/contracts/{version}/draft",
            // Imported drafts are as large as contract uploads, and every
            // console edit PUTs the whole document.
            get(drafts::get_draft).put(drafts::put_draft).layer(
                axum::extract::DefaultBodyLimit::max(contracts::MAX_CONTRACT_BYTES),
            ),
        )
        .route(
            "/mocks/{mock}/contracts/{version}/approve",
            post(drafts::approve),
        )
        .route(
            "/mocks/{mock}/contracts/{version}/preview",
            post(drafts::preview),
        )
        .route("/mocks/{mock}/scenarios", get(scenarios::list))
        .route(
            "/mocks/{mock}/scenarios/{name}",
            axum::routing::put(scenarios::put).delete(scenarios::delete),
        )
        .route("/llm-profiles", get(profiles::list))
        .route(
            "/llm-profiles/{name}",
            get(profiles::get_one)
                .put(profiles::put)
                .delete(profiles::delete),
        )
        .route("/mocks/{mock}/sessions", get(sessions::list))
        .route(
            "/mocks/{mock}/sessions/{session}",
            get(sessions::export)
                .put(sessions::seed)
                .delete(sessions::reset),
        )
        .route("/mocks/{mock}/requests", get(requests::list))
        .route("/mocks/{mock}/requests/stream", get(live::stream))
        .route("/mocks/{mock}/requests/{id}", get(requests::get_one))
        .route(
            "/mocks/{mock}/imports",
            get(imports::list)
                .post(imports::create)
                .layer(import_body_limit),
        )
        .route(
            "/mocks/{mock}/imports/{job}",
            get(imports::get_one).delete(imports::cancel),
        )
        .merge(channels::routes())
        .merge(webhook_deliveries::routes());
    // Workspace-only resources: no P2 alias for `default`.
    let workspace_only: Router<AdminState> = resources
        .clone()
        .route("/api-keys", get(apikeys::list).post(apikeys::create))
        .route("/api-keys/{key_id}", axum::routing::delete(apikeys::revoke))
        .route("/audit", get(audit::list));

    let api = Router::new()
        .route(
            "/workspaces",
            get(workspaces::list).post(workspaces::create),
        )
        .route(
            "/workspaces/{ws}",
            get(workspaces::get_one)
                .patch(workspaces::patch)
                .delete(workspaces::delete),
        )
        .nest("/workspaces/{ws}", workspace_only)
        .merge(resources)
        .route("/audit", get(audit::list_global))
        .route("/me", get(auth_routes::me))
        .route("/brokers", get(channels::brokers))
        .route_layer(axum::middleware::from_fn_with_state(
            st.clone(),
            access::authenticate,
        ));

    Router::new()
        .nest(API_BASE, api)
        .route(OPENAPI_PATH, get(openapi::serve))
        .merge(auth_routes::routes())
        .route("/api/{*rest}", any(problem::not_found))
        .method_not_allowed_fallback(problem::method_not_allowed)
        .layer(axum::middleware::from_fn(problem::log_server_errors))
        .layer(axum::middleware::from_fn(api_security_headers))
        .with_state(st)
}

/// The `Content-Security-Policy` of every `/api` response.
pub const API_CSP: &str = "default-src 'none'; frame-ancestors 'none'; sandbox";

/// Marks every `/api` response as never to be rendered: `nosniff` and a
/// CSP that sandboxes it, so a stored document (a contract source, an
/// echoed name) cannot run script in the console's origin even if a
/// browser is navigated to it.
async fn api_security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path();
    let api = path == "/api" || path.starts_with("/api/");
    let mut resp = next.run(req).await;
    if api {
        let headers = resp.headers_mut();
        headers.insert(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        );
        headers.insert(
            axum::http::header::CONTENT_SECURITY_POLICY,
            axum::http::HeaderValue::from_static(API_CSP),
        );
    }
    resp
}

/// Where a resource is managed. `file` resources are read-only here.
#[derive(Serialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    File,
    Api,
}

impl From<&ResourceSource> for Source {
    fn from(s: &ResourceSource) -> Self {
        match s {
            ResourceSource::File => Source::File,
            ResourceSource::Api => Source::Api,
        }
    }
}

/// Lifecycle status of a contract version.
#[derive(Serialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VersionStatus {
    Draft,
    Approved,
    Active,
    Archived,
}

impl From<&ContractStatus> for VersionStatus {
    fn from(s: &ContractStatus) -> Self {
        match s {
            ContractStatus::Draft => VersionStatus::Draft,
            ContractStatus::Approved => VersionStatus::Approved,
            ContractStatus::Active => VersionStatus::Active,
            ContractStatus::Archived => VersionStatus::Archived,
        }
    }
}

impl AdminState {
    /// The mock named `name` in workspace `ws`, or a 404 problem.
    async fn mock(&self, ws: &str, name: &str) -> ApiResult<Mock> {
        self.store
            .get_mock(ws, name)
            .await?
            .ok_or_else(|| Problem::not_found(format!("mock {name:?} does not exist")))
    }
}

/// A 409 problem if `source` is `file`.
fn ensure_api_managed(source: &ResourceSource, what: impl std::fmt::Display) -> ApiResult<()> {
    match source {
        ResourceSource::File => Err(Problem::managed_by_file(what)),
        ResourceSource::Api => Ok(()),
    }
}

/// Scenario, profile and session names: 1–128 chars of `[A-Za-z0-9._-]`.
fn valid_identifier(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn globs() {
        assert!(glob_match("env:MB_LLM_*", "env:MB_LLM_API_KEY"));
        assert!(glob_match("env:MB_LLM_*", "env:MB_LLM_"));
        assert!(glob_match("env:*_API_KEY", "env:OPENAI_API_KEY"));
        assert!(glob_match("env:*_API_KEY", "env:A_API_KEY_API_KEY"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("env:*_API_KEY", "env:HOME"));
        assert!(!glob_match("env:*_API_KEY", "env:OPENAI_API_KEYS"));
        assert!(!glob_match("env:MB_LLM_*", "file:/etc/passwd"));
        assert!(!glob_match("env:X", "env:XY"));
    }
}
