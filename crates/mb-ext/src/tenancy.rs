//! Tenant resolution: the `TenantResolver` trait is part of the semver-stable
//! surface `mockingbird-cloud` builds against.

use async_trait::async_trait;

use crate::Rejection;

/// The tenant and workspace a call is attributed to.
///
/// Exhaustive on purpose: implementations build it, so adding a field is a
/// 2.0 change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tenancy {
    /// Tenant id (`default` when self-hosted).
    pub tenant: String,
    /// Workspace name within the tenant (`default` unless set).
    pub workspace: String,
}

impl Tenancy {
    /// A tenancy for the given tenant and workspace.
    ///
    /// ```
    /// use mb_ext::Tenancy;
    ///
    /// let tenancy = Tenancy::new("acme", "default");
    /// assert_eq!(tenancy.tenant, "acme");
    /// ```
    pub fn new(tenant: impl Into<String>, workspace: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            workspace: workspace.into(),
        }
    }
}

/// What an extension can see about an inbound data-plane call.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct CallInfo<'a> {
    /// Name of the mock being called.
    pub mock: &'a str,
    /// Operation id within the mock's contract.
    pub operation_id: &'a str,
    /// The inbound request headers (`http` 1 types are public API).
    pub headers: &'a http::HeaderMap,
}

impl<'a> CallInfo<'a> {
    /// Describes a call to `operation_id` on `mock` with the given headers.
    pub fn new(mock: &'a str, operation_id: &'a str, headers: &'a http::HeaderMap) -> Self {
        Self {
            mock,
            operation_id,
            headers,
        }
    }
}

/// Maps an inbound call to the tenant and workspace it belongs to.
#[async_trait]
pub trait TenantResolver: Send + Sync {
    /// The tenancy of `call`, or a rejection (e.g. 401/403) when it has none.
    ///
    /// # Errors
    ///
    /// A [`Rejection`] when the call cannot be attributed to a tenant; the
    /// caller answers it as an RFC 9457 problem.
    async fn resolve(&self, call: &CallInfo<'_>) -> Result<Tenancy, Rejection>;
}

/// Self-hosted default: every call belongs to tenant `default`, workspace `default`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SingleTenant;

#[async_trait]
impl TenantResolver for SingleTenant {
    async fn resolve(&self, _call: &CallInfo<'_>) -> Result<Tenancy, Rejection> {
        Ok(Tenancy::new("default", "default"))
    }
}
