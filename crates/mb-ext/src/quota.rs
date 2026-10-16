//! Admission control: the `QuotaGuard` trait is part of the semver-stable
//! surface `mockingbird-cloud` builds against.

use async_trait::async_trait;

use crate::{CallInfo, Rejection, Tenancy};

/// Admits or rejects a call before any generation work is done.
#[async_trait]
pub trait QuotaGuard: Send + Sync {
    /// `Ok(())` admits the call; a rejection is answered to the caller.
    async fn admit(&self, t: &Tenancy, call: &CallInfo<'_>) -> Result<(), Rejection>;
}

/// Self-hosted default: admits every call (per-mock rate limits still apply).
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

#[async_trait]
impl QuotaGuard for AllowAll {
    async fn admit(&self, _t: &Tenancy, _call: &CallInfo<'_>) -> Result<(), Rejection> {
        Ok(())
    }
}
