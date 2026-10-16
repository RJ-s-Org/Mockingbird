mod common;

use std::sync::Arc;
use std::time::Duration;

use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::json;

#[tokio::test]
async fn memory_conformance() {
    common::conformance::run_all(Arc::new(MemoryStore::new(Duration::from_secs(60)))).await;
}

#[tokio::test]
async fn memory_ttl_expiry() {
    common::conformance::ttl_expiry(Arc::new(MemoryStore::new(Duration::from_secs(1)))).await;
}

#[tokio::test]
async fn connect_memory_url() {
    let store = mb_state::connect("memory", Duration::from_secs(60))
        .await
        .expect("connect");
    common::conformance::run_all(store).await;
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
