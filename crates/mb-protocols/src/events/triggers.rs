//! Trigger scheduling for publish channels, behind a scheduler lease so
//! only one replica fires each mock's triggers.

use std::time::Duration;

use async_trait::async_trait;

/// Grants the scheduler lease of a mock to one holder at a time.
#[async_trait]
pub trait LeaseProvider: Send + Sync {
    /// Take or renew the lease `key` for `holder` for `ttl`. `Ok(true)`
    /// when `holder` now holds it.
    async fn try_acquire(&self, key: &str, holder: &str, ttl: Duration) -> Result<bool, String>;
}

/// A lease provider for a single replica: every holder always leads.
pub struct AlwaysLeader;

#[async_trait]
impl LeaseProvider for AlwaysLeader {
    async fn try_acquire(&self, _key: &str, _holder: &str, _ttl: Duration) -> Result<bool, String> {
        Ok(true)
    }
}
