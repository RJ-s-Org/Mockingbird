//! Extension contract between the open-source Mockingbird core and `mockingbird-cloud`.
//!
//! Every public trait in this crate is part of the semver-stable surface that
//! `mockingbird-cloud` builds against (spec §3.2). Each trait ships a default
//! implementation used by the self-hosted binary; [`Extensions::default`] wires
//! them all together.
//!
//! # Stability policy
//!
//! `mb-ext` follows semantic versioning from 1.0 on. Within 1.x:
//!
//! - Traits ([`TenantResolver`], [`QuotaGuard`], [`UsageSink`],
//!   [`SecretStore`], [`ProviderPolicy`], [`AdminHooks`]) only gain methods
//!   that have a default body, so existing implementations keep compiling.
//! - Types the core constructs and hands to extensions ([`CallInfo`],
//!   [`UsageEvent`], [`Extensions`], [`ExtError`]) are `#[non_exhaustive]`:
//!   they may gain fields or variants in a minor release. Build them with
//!   their constructors ([`CallInfo::new`], [`UsageEvent::new`],
//!   [`Extensions::default`] plus the `with_*` builders) and match
//!   [`ExtError`] with a wildcard arm.
//! - Types extensions construct and return to the core ([`Rejection`],
//!   [`Tenancy`]) stay exhaustive, so implementations can build them with
//!   struct literals. Adding a field to either is a 2.0 change.
//! - Types from `axum` 0.8 (the [`AdminHooks::routes`] router) and `http` 1
//!   (the [`CallInfo::headers`] map) are part of this crate's public API: a
//!   major upgrade of either is a major release of `mb-ext`.
//!
//! # Secret references
//!
//! [`DefaultSecrets`], the self-hosted [`SecretStore`], understands:
//!
//! - `env:NAME`: the environment variable `NAME`;
//! - `file:/path`: the file's contents, surrounding whitespace trimmed;
//! - `vault:<mount>/<path>#<field>`: field `<field>` of the HashiCorp Vault
//!   KV v2 secret `<path>` under mount `<mount>`, enabled when `VAULT_ADDR`
//!   is set (see [`VaultSecrets`]).
//!
//! AWS, GCP and Azure secret managers are not implemented natively. On
//! Kubernetes, use the Secrets Store CSI driver with the provider for your
//! cloud: it mounts each secret as a file in the pod, which a `file:`
//! reference then reads.
#![deny(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

mod hooks;
mod policy;
mod quota;
mod secrets;
mod tenancy;
mod usage;
mod vault;

pub use hooks::{AdminHooks, NoHooks};
pub use policy::{ConfiguredProfile, ProviderPolicy};
pub use quota::{AllowAll, QuotaGuard};
pub use secrets::{EnvFileSecrets, SecretStore};
pub use tenancy::{CallInfo, SingleTenant, Tenancy, TenantResolver};
pub use usage::{PrometheusUsage, UsageEvent, UsageSink};
pub use vault::{DefaultSecrets, VaultSecrets};

/// A refusal to serve a call, rendered by the caller as an RFC 9457 problem.
///
/// Exhaustive on purpose: implementations build it, so adding a field is a
/// 2.0 change.
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

impl Rejection {
    /// A rejection with the given status, title and detail, and no
    /// `Retry-After`.
    pub fn new(status: u16, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status,
            title: title.into(),
            detail: detail.into(),
            retry_after: None,
        }
    }

    /// Sets the delay rendered as a `Retry-After` header.
    #[must_use]
    pub fn with_retry_after(mut self, d: Duration) -> Self {
        self.retry_after = Some(d);
        self
    }
}

/// Errors raised by extension implementations.
///
/// Messages name the secret reference or variable, never a resolved value.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum ExtError {
    /// The referenced secret (variable, file, or Vault secret/field) does not exist.
    #[error("secret {0} not found")]
    SecretNotFound(String),
    /// The reference is malformed or uses a scheme no configured store handles.
    #[error("invalid secret reference {0:?}")]
    BadSecretRef(String),
    /// Any other failure (I/O, network, permission, unexpected response).
    #[error("{0}")]
    Other(String),
}

/// The full set of extension points, as handed to the binary's wiring code.
///
/// Start from [`Extensions::default`] and replace individual extensions with
/// the `with_*` builders.
#[derive(Clone)]
#[non_exhaustive]
pub struct Extensions {
    /// Maps calls to a tenant and workspace.
    pub tenants: Arc<dyn TenantResolver>,
    /// Admits or rejects calls before generation.
    pub quota: Arc<dyn QuotaGuard>,
    /// Receives one event per completed call.
    pub usage: Arc<dyn UsageSink>,
    /// Resolves secret references.
    pub secrets: Arc<dyn SecretStore>,
    /// Chooses the LLM profile a call may use.
    pub providers: Arc<dyn ProviderPolicy>,
    /// Contributes extra admin routes.
    pub admin_hooks: Arc<dyn AdminHooks>,
}

impl Extensions {
    /// Replaces the tenant resolver.
    #[must_use]
    pub fn with_tenants(mut self, t: Arc<dyn TenantResolver>) -> Self {
        self.tenants = t;
        self
    }

    /// Replaces the quota guard.
    #[must_use]
    pub fn with_quota(mut self, q: Arc<dyn QuotaGuard>) -> Self {
        self.quota = q;
        self
    }

    /// Replaces the usage sink.
    #[must_use]
    pub fn with_usage(mut self, u: Arc<dyn UsageSink>) -> Self {
        self.usage = u;
        self
    }

    /// Replaces the secret store.
    #[must_use]
    pub fn with_secrets(mut self, s: Arc<dyn SecretStore>) -> Self {
        self.secrets = s;
        self
    }

    /// Replaces the provider policy.
    #[must_use]
    pub fn with_providers(mut self, p: Arc<dyn ProviderPolicy>) -> Self {
        self.providers = p;
        self
    }

    /// Replaces the admin hooks.
    #[must_use]
    pub fn with_admin_hooks(mut self, h: Arc<dyn AdminHooks>) -> Self {
        self.admin_hooks = h;
        self
    }
}

impl Default for Extensions {
    /// The self-hosted defaults: single tenant, no quotas, Prometheus usage,
    /// env/file/Vault secrets ([`DefaultSecrets::from_env`]), the mock's
    /// configured profile, and no extra admin routes.
    fn default() -> Self {
        Self {
            tenants: Arc::new(SingleTenant),
            quota: Arc::new(AllowAll),
            usage: Arc::new(PrometheusUsage),
            secrets: Arc::new(DefaultSecrets::from_env()),
            providers: Arc::new(ConfiguredProfile),
            admin_hooks: Arc::new(NoHooks),
        }
    }
}
