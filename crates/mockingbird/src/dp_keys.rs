//! Data-plane API keys verified against the control store.

use std::sync::Arc;

use async_trait::async_trait;
use mb_auth::{ApiKeys, KeyScope};
use mb_protocols::DataPlaneKeys;
use mb_store::ControlStore;

/// Accepts a non-revoked, unexpired key with scope `data-plane` that
/// belongs to the mock's workspace or is a sysadmin key.
///
/// [`StoreDataPlaneKeys::new`] reads the store on every call;
/// [`StoreDataPlaneKeys::with_keys`] takes an [`ApiKeys`] that may cache
/// (the server passes one built with [`ApiKeys::with_cache`]).
pub struct StoreDataPlaneKeys {
    keys: ApiKeys,
}

impl std::fmt::Debug for StoreDataPlaneKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreDataPlaneKeys").finish_non_exhaustive()
    }
}

impl StoreDataPlaneKeys {
    /// Verifies against `store`, reading it on every call.
    pub fn new(store: Arc<dyn ControlStore>) -> Arc<Self> {
        Arc::new(Self {
            keys: ApiKeys::new(store),
        })
    }

    /// Verifies through `keys`, sharing its cache, if any.
    pub fn with_keys(keys: ApiKeys) -> Arc<Self> {
        Arc::new(Self { keys })
    }
}

#[async_trait]
impl DataPlaneKeys for StoreDataPlaneKeys {
    async fn verify(&self, workspace: &str, key: &str) -> bool {
        match self.keys.verify(key, KeyScope::DataPlane).await {
            Ok(key) => key.workspace == workspace || key.sysadmin,
            Err(error) => {
                tracing::debug!(%error, "data-plane API key rejected");
                false
            }
        }
    }
}
