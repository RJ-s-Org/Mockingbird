//! Mockingbird Admin REST API: workspaces, and in each workspace mocks,
//! contract versions, scenarios, LLM profiles, sessions and the request log
//! under `/api/v1/workspaces/{ws}` (the P2 paths under `/api/v1` act on
//! `default`), described by the OpenAPI document at `/api/openapi.json`.
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
mod contracts;
mod mocks;
mod openapi;
pub mod problem;
mod profiles;
mod requests;
mod scenarios;
mod sessions;
mod workspaces;

pub use access::{CurrentPrincipal, WsAccess};
pub use openapi::{ApiDoc, openapi_document};
use problem::ApiResult;
pub use problem::Problem;
pub use workspaces::{CreateWorkspace, DeleteParams, PatchWorkspace, WorkspaceView};

/// Admin API base path.
pub const API_BASE: &str = "/api/v1";
/// Where the OpenAPI document is served.
pub const OPENAPI_PATH: &str = "/api/openapi.json";

/// Default `server.api_secret_refs`: the secret references an LLM profile
/// created or updated through the API may use.
pub const DEFAULT_API_SECRET_REFS: [&str; 2] = ["env:MB_LLM_*", "env:*_API_KEY"];

#[derive(Clone)]
pub struct AdminState {
    pub store: Arc<dyn ControlStore>,
    pub state: Arc<dyn StateStore>,
    /// Globs (`*` matches any run of characters) a profile's `secret_ref`
    /// must match when written through the API (`server.api_secret_refs`).
    pub api_secret_refs: Arc<Vec<String>>,
    /// Resolves the caller of every `/api/v1` request.
    pub auth: Arc<dyn Authenticate>,
}

impl AdminState {
    /// A state with [`LocalAuth`] (`auth.mode: none`) and the
    /// [`DEFAULT_API_SECRET_REFS`].
    pub fn new(store: Arc<dyn ControlStore>, state: Arc<dyn StateStore>) -> Self {
        Self {
            store,
            state,
            api_secret_refs: Arc::new(
                DEFAULT_API_SECRET_REFS
                    .iter()
                    .map(|r| r.to_string())
                    .collect(),
            ),
            auth: Arc::new(LocalAuth),
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

/// Mounts `/api/v1/*` and `/api/openapi.json`.
///
/// The workspace resources are served under `/api/v1/workspaces/{ws}` and,
/// for `default`, under `/api/v1` (the P2 paths). Every `/api/v1` route
/// requires a principal (401 otherwise); `/api/openapi.json` is public, and
/// any other `/api/*` path is a 404 problem. Paths outside `/api` are left
/// to the caller's fallback.
///
/// Every mutating request needs a non-CORS-safelisted `Content-Type`
/// (`application/json`, or a JSON/YAML/OpenAPI type for contract uploads;
/// otherwise 415), so a cross-site page cannot reach the API with a simple
/// request.
pub fn router(st: AdminState) -> Router {
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
        .route("/mocks/{mock}/requests", get(requests::list));

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
        .nest("/workspaces/{ws}", resources.clone())
        .merge(resources)
        .route_layer(axum::middleware::from_fn_with_state(
            st.clone(),
            access::authenticate,
        ));

    Router::new()
        .nest(API_BASE, api)
        .route(OPENAPI_PATH, get(openapi::serve))
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
