//! Session-scoped state store for mock resources.

use async_trait::async_trait;
use serde_json::Value;

pub mod memory;

pub use memory::MemoryStore;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub workspace: String,
    pub mock: String,
    pub session: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredResource {
    pub resource_type: String,
    pub id: String,
    pub data: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Upserted,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateChanged {
    pub session: SessionKey,
    pub resource_type: String,
    pub id: String,
    pub kind: ChangeKind,
}

#[async_trait]
pub trait StateStore: Send + Sync {
    async fn get(&self, s: &SessionKey, resource_type: &str, id: &str) -> Option<StoredResource>;
    /// Most recent first.
    async fn list(&self, s: &SessionKey, resource_type: &str, limit: usize) -> Vec<StoredResource>;
    async fn upsert(&self, s: &SessionKey, r: StoredResource);
    async fn delete(&self, s: &SessionKey, resource_type: &str, id: &str) -> bool;
    async fn reset(&self, s: &SessionKey);
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<StateChanged>;
}
