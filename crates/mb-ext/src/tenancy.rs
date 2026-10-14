//! Tenant resolution: the `TenantResolver` trait is part of the semver-stable
//! surface `mockingbird-cloud` builds against.

use async_trait::async_trait;

use crate::Rejection;

/// The tenant and workspace a call is attributed to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tenancy {
    pub tenant: String,
    pub workspace: String,
}

/// What an extension can see about an inbound data-plane call.
#[derive(Debug, Clone, Copy)]
pub struct CallInfo<'a> {
    pub mock: &'a str,
    pub operation_id: &'a str,
    pub headers: &'a http::HeaderMap,
}

/// Maps an inbound call to the tenant and workspace it belongs to.
#[async_trait]
pub trait TenantResolver: Send + Sync {
    async fn resolve(&self, call: &CallInfo<'_>) -> Result<Tenancy, Rejection>;
}

/// Self-hosted default: every call belongs to tenant `default`, workspace `default`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SingleTenant;

#[async_trait]
impl TenantResolver for SingleTenant {
    async fn resolve(&self, _call: &CallInfo<'_>) -> Result<Tenancy, Rejection> {
        Ok(Tenancy {
            tenant: "default".to_string(),
            workspace: "default".to_string(),
        })
    }
}
