use std::sync::Arc;
use std::time::Duration;

use mb_state::{
    CachedResponse, CachingStateStore, MemoryStore, SessionKey, StateStore, StoredResource,
};
use serde_json::json;

fn key(session: &str) -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "orders".into(),
        session: session.into(),
    }
}

fn answer(n: i64) -> CachedResponse {
    CachedResponse {
        status: 200,
        content_type: None,
        body: Some(json!({"n": n})),
        fallback_reason: None,
    }
}

fn order(id: &str) -> StoredResource {
    StoredResource {
        resource_type: "orders".into(),
        id: id.into(),
        data: json!({"id": id}),
    }
}

fn store() -> Arc<dyn StateStore> {
    CachingStateStore::wrap(
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
        Duration::from_secs(3600),
        2,
    )
}

#[tokio::test]
async fn least_recently_used_entry_is_evicted() {
    let st = store();
    let c = st.response_cache().unwrap();
    c.put(&key("a"), "k1".into(), Some("orders"), answer(1));
    c.put(&key("a"), "k2".into(), Some("orders"), answer(2));
    assert_eq!(c.get(&key("a"), "k1"), Some(answer(1)));
    c.put(&key("a"), "k3".into(), Some("orders"), answer(3));
    assert_eq!(c.get(&key("a"), "k2"), None);
    assert_eq!(c.len(&key("a")), 2);
    assert_eq!(c.get(&key("b"), "k1"), None, "sessions are separate");
}

#[tokio::test]
async fn writes_of_the_type_and_resets_invalidate() {
    let st = store();
    let c = st.response_cache().unwrap();
    c.put(&key("a"), "list".into(), Some("orders"), answer(1));
    c.put(&key("a"), "other".into(), Some("customers"), answer(2));
    st.upsert(&key("a"), order("o1")).await.unwrap();
    assert_eq!(c.get(&key("a"), "list"), None);
    assert_eq!(c.get(&key("a"), "other"), Some(answer(2)));
    st.reset(&key("a")).await.unwrap();
    assert_eq!(c.get(&key("a"), "other"), None);
}

#[tokio::test]
async fn seeded_inserts_do_not_invalidate() {
    let st = store();
    let c = st.response_cache().unwrap();
    c.put(&key("a"), "page1".into(), Some("orders"), answer(1));
    assert!(st.insert_seeded(&key("a"), order("o9")).await.unwrap());
    assert_eq!(c.get(&key("a"), "page1"), Some(answer(1)));
}

#[tokio::test]
async fn changes_made_below_the_wrapper_invalidate() {
    let inner: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let st = CachingStateStore::wrap(Arc::clone(&inner), Duration::from_secs(3600), 500);
    let c = st.response_cache().unwrap();
    c.put(&key("a"), "list".into(), Some("orders"), answer(1));
    inner.upsert(&key("a"), order("o1")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while c.get(&key("a"), "list").is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn idle_sessions_expire() {
    let st = CachingStateStore::wrap(
        Arc::new(MemoryStore::new(Duration::from_secs(60))),
        Duration::from_secs(60),
        500,
    );
    let c = st.response_cache().unwrap();
    c.put(&key("a"), "k".into(), None, answer(1));
    tokio::time::advance(Duration::from_secs(61)).await;
    assert_eq!(c.get(&key("a"), "k"), None);
}

#[tokio::test]
async fn an_answer_older_than_an_invalidation_is_not_cached() {
    let st = store();
    let c = st.response_cache().unwrap();
    // A read misses, then a write (or reset) lands while it generates.
    let before = c.epoch(&key("a"));
    st.upsert(&key("a"), order("o1")).await.unwrap();
    c.put_if_current(&key("a"), "list".into(), Some("orders"), answer(1), before);
    assert_eq!(c.get(&key("a"), "list"), None);

    let before = c.epoch(&key("a"));
    st.reset_mock("default", "orders").await.unwrap();
    c.put_if_current(&key("a"), "list".into(), Some("orders"), answer(1), before);
    assert_eq!(c.get(&key("a"), "list"), None);

    let before = c.epoch(&key("a"));
    c.put_if_current(&key("a"), "list".into(), Some("orders"), answer(2), before);
    assert_eq!(c.get(&key("a"), "list"), Some(answer(2)));
}
