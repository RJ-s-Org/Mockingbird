//! Mockingbird Admin REST API: workspaces, and in each workspace mocks,
//! contract versions, scenarios, LLM profiles, sessions and the request log
//! under `/api/v1/workspaces/{ws}` (the P2 paths under `/api/v1` act on
//! `default`), plus each workspace's API keys and audit log and the global
//! audit log at `/api/v1/audit`, the live request tail over SSE and the
//! caller's identity at `/api/v1/me`, described by the OpenAPI document at
//! `/api/openapi.json`, plus the OIDC browser login under `/auth`. Every successful mutation writes one audit event.
//! Every `/api/v1` request is authenticated and checked against the caller's
//! workspace role. Every error is an RFC 9457 `application/problem+json`
//! document.

use std::sync::Arc;

use axum::Router;
use axum::routing::{any, get, post};
use mb_auth::{Authenticate, LocalAuth};
use mb_state::StateStore;
use mb_store::{ContractStatus, ControlStore, Mock, ResourceSource};
use serde::Serialize;
use utoipa::ToSchema;

pub mod access;
mod apikeys;
mod audit;
mod auth_routes;
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
mod workspaces;

pub use access::{CurrentPrincipal, WsAccess, authenticate};
pub use apikeys::{ApiKeyView, CreateApiKey, CreatedApiKey};
pub use audit::{AuditEventView, Auditor, retention_cutoff, spawn_retention};
pub use auth_routes::{
    LOGIN_STATE_COOKIE, MeView, MeWorkspace, WebAuth, auth_router, console_gate,
};
pub use imports::llm::{LlmResolver, OfflineLlm};
pub use imports::worker::{ImportWorkers, run_next_import, spawn_import_workers};
pub use imports::{
    CreateImport, DEFAULT_MAX_ACTIVE_JOBS_PER_MOCK, ImportJobView, ImportKindSchema,
    ImportProgressView, ImportSettings, ImportStatusSchema, Imports, MAX_IMPORT_REQUEST_BYTES,
    UPLOAD_HEADER,
};
pub use live::{LiveRequestView, LiveSubscribe, StreamQuery};
pub use login_limit::{LoginFailureLimiter, SUPPRESSED_METRIC as LOGIN_FAILED_SUPPRESSED_METRIC};
pub use openapi::{ApiDoc, openapi_document};
use problem::ApiResult;
pub use problem::Problem;
pub use webhook::{WebhookSink, signature};
pub use workspaces::{CreateWorkspace, DeleteParams, PatchWorkspace, WorkspaceView};

/// Admin API base path.
pub const API_BASE: &str = "/api/v1";
/// Where the OpenAPI document is served.
pub const OPENAPI_PATH: &str = "/api/openapi.json";

/// Default `server.api_secret_refs`: the secret references an LLM profile
/// created or updated through the API may use.
pub const DEFAULT_API_SECRET_REFS: [&str; 2] = ["env:MB_LLM_*", "env:*_API_KEY"];

/// Default [`AdminState::live_recheck`].
pub const DEFAULT_LIVE_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub struct AdminState {
    pub store: Arc<dyn ControlStore>,
    pub state: Arc<dyn StateStore>,
    /// Globs (`*` matches any run of characters) a profile's `secret_ref`
    /// must match when written through the API (`server.api_secret_refs`).
    pub api_secret_refs: Arc<Vec<String>>,
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
            auth: Arc::new(LocalAuth),
            live: None,
            live_recheck: DEFAULT_LIVE_RECHECK,
            web_auth: None,
            login_failures: Arc::new(LoginFailureLimiter::new()),
            llm: Arc::new(OfflineLlm),
            imports: Imports::default(),
        }
    }
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
/// (`application/json`, or a JSON/YAML/OpenAPI type for contract uploads;
/// otherwise 415), so a cross-site page cannot reach the API with a simple
/// request.
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
        .route("/mocks/{mock}/contracts/{version}", get(contracts::get_one))
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
        .route(
            "/mocks/{mock}/imports",
            get(imports::list)
                .post(imports::create)
                .layer(import_body_limit),
        )
        .route(
            "/mocks/{mock}/imports/{job}",
            get(imports::get_one).delete(imports::cancel),
        );
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
        .with_state(st)
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
