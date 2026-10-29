use std::sync::Arc;
use std::time::Duration;

use mb_state::{
    MemoryStore, SessionInfo, SessionKey, StateChanged, StateError, StateStore, StoredResource,
};
use serde_json::json;
use tokio::sync::broadcast;

#[tokio::test]
async fn memory_conformance() {
    crate::conformance::run_all(Arc::new(MemoryStore::new(Duration::from_secs(60)))).await;
}

#[tokio::test]
async fn state_session_count_default_matches_sessions() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(60)));
    crate::conformance::session_count_default_matches_sessions(&store, "default").await;
    // The trait's default (a store that only lists sessions).
    let store: Arc<dyn StateStore> = Arc::new(ListOnly(MemoryStore::new(Duration::from_secs(60))));
    crate::conformance::session_count_default_matches_sessions(&store, "default").await;
}

/// Forwards everything but `session_count`, which keeps the default.
struct ListOnly(MemoryStore);

#[async_trait::async_trait]
impl StateStore for ListOnly {
    async fn get(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        self.0.get(s, resource_type, id).await
    }
    async fn list(
        &self,
        s: &SessionKey,
        resource_type: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        self.0.list(s, resource_type, limit).await
    }
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        self.0.upsert(s, r).await
    }
    async fn delete(
        &self,
        s: &SessionKey,
        resource_type: &str,
        id: &str,
    ) -> Result<bool, StateError> {
        self.0.delete(s, resource_type, id).await
    }
    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        self.0.reset(s).await
    }
    async fn reset_mock(&self, workspace: &str, mock: &str) -> Result<(), StateError> {
        self.0.reset_mock(workspace, mock).await
    }
    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.0.touch(s).await
    }
    async fn sessions(&self, workspace: &str, mock: &str) -> Result<Vec<SessionInfo>, StateError> {
        self.0.sessions(workspace, mock).await
    }
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        self.0.export(s).await
    }
    async fn seed(&self, s: &SessionKey, resources: Vec<StoredResource>) -> Result<(), StateError> {
        self.0.seed(s, resources).await
    }
    async fn ping(&self) -> Result<(), StateError> {
        self.0.ping().await
    }
    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.0.subscribe()
    }
}

#[tokio::test]
async fn memory_ttl_expiry() {
    crate::conformance::ttl_expiry(Arc::new(MemoryStore::new(Duration::from_secs(1)))).await;
}

#[tokio::test]
async fn connect_memory_url() {
    let store = mb_state::connect("memory", Duration::from_secs(60))
        .await
        .expect("connect");
    crate::conformance::run_all(store).await;
}

#[tokio::test(start_paused = true)]
async fn expired_session_is_invisible() {
    let st = MemoryStore::new(Duration::from_millis(50));
    let k = SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "a".into(),
    };
    let r = StoredResource {
        resource_type: "order".into(),
        id: "1".into(),
        data: json!({}),
    };
    st.upsert(&k, r.clone()).await.expect("upsert");
    assert!(st.get(&k, "order", "1").await.expect("get").is_some());
    tokio::time::advance(Duration::from_millis(80)).await;
    assert!(st.get(&k, "order", "1").await.expect("get").is_none());
    assert!(st.list(&k, "order", 10).await.expect("list").is_empty());
    assert!(
        st.sessions("default", "m")
            .await
            .expect("sessions")
            .is_empty()
    );
    // A write after expiry starts a fresh session.
    st.upsert(
        &k,
        StoredResource {
            id: "2".into(),
            ..r
        },
    )
    .await
    .expect("upsert");
    assert!(st.get(&k, "order", "1").await.expect("get").is_none());
    let s = st.sessions("default", "m").await.expect("sessions");
    assert_eq!((s.len(), s[0].resources), (1, 1));
}
