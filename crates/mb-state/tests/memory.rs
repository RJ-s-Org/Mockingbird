#![allow(clippy::unwrap_used)]
use std::time::Duration;

use mb_state::{ChangeKind, MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::json;

fn key(s: &str) -> SessionKey {
    SessionKey {
        workspace: "w".into(),
        mock: "m".into(),
        session: s.into(),
    }
}

fn res(t: &str, id: &str, v: serde_json::Value) -> StoredResource {
    StoredResource {
        resource_type: t.into(),
        id: id.into(),
        data: v,
    }
}

#[tokio::test]
async fn upsert_then_get_and_list() {
    let st = MemoryStore::new(Duration::from_secs(60));
    let k = key("a");
    st.upsert(&k, res("order", "1", json!({"n": 1}))).await;
    st.upsert(&k, res("order", "2", json!({"n": 2}))).await;
    st.upsert(&k, res("pet", "9", json!({}))).await;
    assert_eq!(
        st.get(&k, "order", "1").await.unwrap().data,
        json!({"n": 1})
    );
    let l = st.list(&k, "order", 10).await;
    assert_eq!(
        l.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["2", "1"]
    );
    assert_eq!(st.list(&k, "order", 1).await.len(), 1);
}

#[tokio::test]
async fn sessions_are_isolated() {
    let st = MemoryStore::new(Duration::from_secs(60));
    st.upsert(&key("a"), res("order", "1", json!("A"))).await;
    st.upsert(&key("b"), res("order", "1", json!("B"))).await;
    assert_eq!(
        st.get(&key("a"), "order", "1").await.unwrap().data,
        json!("A")
    );
    assert_eq!(
        st.get(&key("b"), "order", "1").await.unwrap().data,
        json!("B")
    );
}

#[tokio::test]
async fn delete_removes_and_reports() {
    let st = MemoryStore::new(Duration::from_secs(60));
    let k = key("a");
    st.upsert(&k, res("order", "1", json!({}))).await;
    assert!(st.delete(&k, "order", "1").await);
    assert!(st.get(&k, "order", "1").await.is_none());
    assert!(!st.delete(&k, "order", "1").await);
}

#[tokio::test]
async fn reset_clears_session() {
    let st = MemoryStore::new(Duration::from_secs(60));
    let k = key("a");
    st.upsert(&k, res("order", "1", json!({}))).await;
    st.upsert(&key("b"), res("order", "1", json!({}))).await;
    st.reset(&k).await;
    assert!(st.list(&k, "order", 10).await.is_empty());
    assert!(st.get(&key("b"), "order", "1").await.is_some());
}

#[tokio::test(start_paused = true)]
async fn expired_session_is_invisible() {
    let st = MemoryStore::new(Duration::from_millis(50));
    let k = key("a");
    st.upsert(&k, res("order", "1", json!({}))).await;
    assert!(st.get(&k, "order", "1").await.is_some());
    tokio::time::advance(Duration::from_millis(80)).await;
    assert!(st.get(&k, "order", "1").await.is_none());
    assert!(st.list(&k, "order", 10).await.is_empty());
}

#[tokio::test]
async fn publishes_changes() {
    let st = MemoryStore::new(Duration::from_secs(60));
    let mut rx = st.subscribe();
    let k = key("a");
    st.upsert(&k, res("order", "1", json!({}))).await;
    st.delete(&k, "order", "1").await;
    let a = rx.recv().await.unwrap();
    assert_eq!(
        (a.kind, a.id.as_str(), a.session.clone()),
        (ChangeKind::Upserted, "1", k.clone())
    );
    let b = rx.recv().await.unwrap();
    assert_eq!(b.kind, ChangeKind::Deleted);
}
