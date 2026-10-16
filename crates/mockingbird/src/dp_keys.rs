//! Data-plane API keys verified against the control store.

use std::sync::Arc;

use async_trait::async_trait;
use mb_auth::{ApiKeys, KeyScope};
use mb_protocols::DataPlaneKeys;
use mb_store::ControlStore;

/// Accepts a non-revoked, unexpired key with scope `data-plane` that
/// belongs to the mock's workspace or is a sysadmin key.
pub struct StoreDataPlaneKeys {
    keys: ApiKeys,
}

impl StoreDataPlaneKeys {
    pub fn new(store: Arc<dyn ControlStore>) -> Arc<Self> {
        Arc::new(Self {
            keys: ApiKeys::new(store),
        })
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
