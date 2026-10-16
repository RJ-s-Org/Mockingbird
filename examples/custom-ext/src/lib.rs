//! A worked `mb-ext` extension: tenants come from the `X-Tenant` header and
//! each tenant may make a fixed number of calls.
//!
//! [`extensions`] builds the [`Extensions`] the example binary
//! (`src/main.rs`) hands to [`mockingbird::server::run_with`].

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use mb_ext::{CallInfo, Extensions, QuotaGuard, Rejection, Tenancy, TenantResolver};

/// Request header naming the caller's tenant.
pub const TENANT_HEADER: &str = "x-tenant";

/// Tenant of a call without a usable [`TENANT_HEADER`].
pub const ANONYMOUS_TENANT: &str = "anonymous";

/// Delay sent as `Retry-After` once a tenant's quota is used up.
pub const QUOTA_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Resolves the tenant from the `X-Tenant` header (`anonymous` when it is
/// missing, empty or not visible ASCII); the workspace is always `default`.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeaderTenants;

#[async_trait]
impl TenantResolver for HeaderTenants {
    async fn resolve(&self, call: &CallInfo<'_>) -> Result<Tenancy, Rejection> {
        let tenant = call
            .headers
            .get(TENANT_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or(ANONYMOUS_TENANT);
        Ok(Tenancy::new(tenant, "default"))
    }
}

/// Admits at most `per_tenant` calls per tenant for the life of the process,
/// then rejects with 429 and `Retry-After: 60`.
///
/// Counts live in memory, so each replica keeps its own; a real deployment
/// would keep them in shared storage and reset them per billing period.
#[derive(Debug, Default)]
pub struct FixedQuota {
    /// Calls each tenant may make.
    pub per_tenant: u64,
    counts: DashMap<String, u64>,
}

impl FixedQuota {
    /// A quota of `per_tenant` calls for every tenant.
    pub fn new(per_tenant: u64) -> Self {
        Self {
            per_tenant,
            counts: DashMap::new(),
        }
    }
}

#[async_trait]
impl QuotaGuard for FixedQuota {
    async fn admit(&self, t: &Tenancy, _call: &CallInfo<'_>) -> Result<(), Rejection> {
        // The entry guard holds the shard lock, so check-and-increment is
        // atomic across concurrent calls from one tenant.
        let mut used = self.counts.entry(t.tenant.clone()).or_insert(0);
        if *used >= self.per_tenant {
            return Err(Rejection::new(
                429,
                "Quota exceeded",
                format!(
                    "tenant {:?} has used all {} calls of its quota",
                    t.tenant, self.per_tenant
                ),
            )
            .with_retry_after(QUOTA_RETRY_AFTER));
        }
        *used += 1;
        Ok(())
    }
}

/// The self-hosted defaults with [`HeaderTenants`] and a [`FixedQuota`] of
/// `per_tenant` calls.
pub fn extensions(per_tenant: u64) -> Extensions {
    Extensions::default()
        .with_tenants(Arc::new(HeaderTenants))
        .with_quota(Arc::new(FixedQuota::new(per_tenant)))
}
