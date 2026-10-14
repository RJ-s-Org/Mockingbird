//! Extension contract between the open-source Mockingbird core and `mockingbird-cloud`.
//!
//! Every public trait in this crate is part of the semver-stable surface that
//! `mockingbird-cloud` builds against (spec §3.2). Each trait ships a default
//! implementation used by the self-hosted binary; [`Extensions::default`] wires
//! them all together.

use std::sync::Arc;
use std::time::Duration;

mod hooks;
mod policy;
mod quota;
mod secrets;
mod tenancy;
mod usage;

pub use hooks::{AdminHooks, NoHooks};
pub use policy::{ConfiguredProfile, ProviderPolicy};
pub use quota::{AllowAll, QuotaGuard};
pub use secrets::{EnvFileSecrets, SecretStore};
pub use tenancy::{CallInfo, SingleTenant, Tenancy, TenantResolver};
pub use usage::{PrometheusUsage, UsageEvent, UsageSink};

/// A refusal to serve a call, rendered by the caller as an RFC 9457 problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// HTTP status code to answer with (e.g. 403, 429).
    pub status: u16,
    /// Short, human-readable summary (problem `title`).
    pub title: String,
    /// Explanation specific to this occurrence (problem `detail`).
    pub detail: String,
    /// When set, rendered as a `Retry-After` header.
    pub retry_after: Option<Duration>,
}

/// Errors raised by extension implementations.
#[derive(thiserror::Error, Debug)]
pub enum ExtError {
    #[error("secret {0} not found")]
    SecretNotFound(String),
    #[error("invalid secret reference {0:?}")]
    BadSecretRef(String),
    #[error("{0}")]
    Other(String),
}

/// The full set of extension points, as handed to the binary's wiring code.
#[derive(Clone)]
pub struct Extensions {
    pub tenants: Arc<dyn TenantResolver>,
    pub quota: Arc<dyn QuotaGuard>,
    pub usage: Arc<dyn UsageSink>,
    pub secrets: Arc<dyn SecretStore>,
    pub providers: Arc<dyn ProviderPolicy>,
    pub admin_hooks: Arc<dyn AdminHooks>,
}

impl Default for Extensions {
    /// The self-hosted defaults: single tenant, no quotas, Prometheus usage,
    /// env/file secrets, the mock's configured profile, and no extra admin routes.
    fn default() -> Self {
        Self {
            tenants: Arc::new(SingleTenant),
            quota: Arc::new(AllowAll),
            usage: Arc::new(PrometheusUsage),
            secrets: Arc::new(EnvFileSecrets),
            providers: Arc::new(ConfiguredProfile),
            admin_hooks: Arc::new(NoHooks),
        }
    }
}
