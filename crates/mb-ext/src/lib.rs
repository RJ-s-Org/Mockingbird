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
//!
//! # Example
//!
//! Replace one default, here the quota guard, and keep the others:
//!
//! ```
//! use std::sync::Arc;
//!
//! use async_trait::async_trait;
//! use mb_ext::{CallInfo, Extensions, QuotaGuard, Rejection, Tenancy};
//!
//! /// Refuses every call to the `billing` mock.
//! struct NoBilling;
//!
//! #[async_trait]
//! impl QuotaGuard for NoBilling {
//!     async fn admit(&self, _tenancy: &Tenancy, call: &CallInfo<'_>) -> Result<(), Rejection> {
//!         if call.mock == "billing" {
//!             return Err(Rejection::new(403, "Forbidden", "billing is closed"));
//!         }
//!         Ok(())
//!     }
//! }
//!
//! let extensions = Extensions::default().with_quota(Arc::new(NoBilling));
//! # let _ = extensions;
//! ```
//!
//! [`examples/custom-ext`](https://github.com/rj-s-org/mockingbird/tree/main/examples/custom-ext)
//! is a complete server binary built this way.
#![deny(missing_docs)]
#![warn(missing_debug_implementations, clippy::missing_errors_doc)]

use std::fmt;
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
pub use secrets::{EnvFileSecrets, SecretStore, check_secret_file, in_key_store};
pub use tenancy::{CallInfo, SingleTenant, Tenancy, TenantResolver};
pub use usage::{PrometheusUsage, UsageEvent, UsageSink};
pub use vault::{DefaultSecrets, VaultHttp, VaultSecrets};

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
    ///
    /// ```
    /// use std::time::Duration;
    /// use mb_ext::Rejection;
    ///
    /// let rejection = Rejection::new(429, "Quota exceeded", "100 calls a day")
    ///     .with_retry_after(Duration::from_secs(60));
    /// assert_eq!(rejection.to_string(), "429 Quota exceeded: 100 calls a day");
    /// ```
    pub fn new(status: u16, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status,
            title: title.into(),
            detail: detail.into(),
            retry_after: None,
        }
    }

    /// Like [`Rejection::new`], with the status as an [`http::StatusCode`].
    ///
    /// ```
    /// use mb_ext::Rejection;
    ///
    /// let rejection = Rejection::from_status(http::StatusCode::FORBIDDEN, "Forbidden", "no plan");
    /// assert_eq!(rejection.status, 403);
    /// ```
    pub fn from_status(
        status: http::StatusCode,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(status.as_u16(), title, detail)
    }

    /// Sets the delay rendered as a `Retry-After` header.
    #[must_use]
    pub fn with_retry_after(mut self, delay: Duration) -> Self {
        self.retry_after = Some(delay);
        self
    }
}

/// `<status> <title>: <detail>`, or `<status> <title>` when the detail is
/// empty.
impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status, self.title)?;
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        Ok(())
    }
}

impl std::error::Error for Rejection {}

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
    pub fn with_tenants(mut self, resolver: Arc<dyn TenantResolver>) -> Self {
        self.tenants = resolver;
        self
    }

    /// Replaces the quota guard.
    #[must_use]
    pub fn with_quota(mut self, guard: Arc<dyn QuotaGuard>) -> Self {
        self.quota = guard;
        self
    }

    /// Replaces the usage sink.
    #[must_use]
    pub fn with_usage(mut self, sink: Arc<dyn UsageSink>) -> Self {
        self.usage = sink;
        self
    }

    /// Replaces the secret store.
    #[must_use]
    pub fn with_secrets(mut self, store: Arc<dyn SecretStore>) -> Self {
        self.secrets = store;
        self
    }

    /// Replaces the provider policy.
    #[must_use]
    pub fn with_providers(mut self, policy: Arc<dyn ProviderPolicy>) -> Self {
        self.providers = policy;
        self
    }

    /// Replaces the admin hooks.
    #[must_use]
    pub fn with_admin_hooks(mut self, hooks: Arc<dyn AdminHooks>) -> Self {
        self.admin_hooks = hooks;
        self
    }
}

/// Names the type only: the extensions are trait objects that need not
/// implement `Debug`.
impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions").finish_non_exhaustive()
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
