//! Mockingbird Admin REST API: mocks, contract versions, scenarios, LLM
//! profiles, sessions and the request log under `/api/v1`, described by the
//! OpenAPI document at `/api/openapi.json`. Every error is an RFC 9457
//! `application/problem+json` document.

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use mb_state::StateStore;
use mb_store::{ContractStatus, ControlStore, DEFAULT_WORKSPACE, Mock, ResourceSource};
use serde::Serialize;
use utoipa::ToSchema;

mod contracts;
mod mocks;
mod openapi;
pub mod problem;
mod profiles;
mod requests;
mod scenarios;
mod sessions;

pub use openapi::ApiDoc;
use problem::ApiResult;
pub use problem::Problem;

/// Admin API base path.
pub const API_BASE: &str = "/api/v1";
/// Where the OpenAPI document is served.
pub const OPENAPI_PATH: &str = "/api/openapi.json";

#[derive(Clone)]
pub struct AdminState {
    pub store: Arc<dyn ControlStore>,
    pub state: Arc<dyn StateStore>,
}

/// Mounts `/api/v1/*` and `/api/openapi.json`.
pub fn router(st: AdminState) -> Router {
    let api = Router::new()
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

    Router::new()
        .nest(API_BASE, api)
        .route(OPENAPI_PATH, get(openapi::serve))
        .fallback(problem::not_found)
        .method_not_allowed_fallback(problem::method_not_allowed)
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
    /// The mock named `name`, or a 404 problem.
    async fn mock(&self, name: &str) -> ApiResult<Mock> {
        self.store
            .get_mock(DEFAULT_WORKSPACE, name)
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
