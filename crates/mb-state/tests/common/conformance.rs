//! Backend-neutral `StateStore` conformance suite. Every backend's test file
//! calls [`run_all`] so all backends run identical assertions.

use std::sync::Arc;
use std::time::Duration;

use mb_state::{ChangeKind, SessionKey, StateChanged, StateStore, StoredResource};
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// Runs every behaviour except TTL expiry, which needs a store built with a
/// short TTL (see [`ttl_expiry`]). Each check uses its own mock name, so they
/// can share one store.
pub async fn run_all(store: Arc<dyn StateStore>) {
    store.ping().await.expect("ping");
    upsert_then_get_and_list(&store).await;
    sessions_are_isolated(&store).await;
    delete_removes_and_reports(&store).await;
    reset_clears_session(&store).await;
    publishes_changes(&store).await;
    upsert_replaces_and_moves_to_front(&store).await;
    reads_do_not_create_sessions(&store).await;
    sessions_lists_with_counts(&store).await;
    export_then_seed_round_trips(&store).await;
}

fn key(mock: &str, session: &str) -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: mock.into(),
        session: session.into(),
    }
}

fn res(t: &str, id: &str, v: Value) -> StoredResource {
    StoredResource {
        resource_type: t.into(),
        id: id.into(),
        data: v,
    }
}

fn ids(items: &[StoredResource]) -> Vec<&str> {
    items.iter().map(|r| r.id.as_str()).collect()
}

async fn upsert_then_get_and_list(st: &Arc<dyn StateStore>) {
    let k = key("crud", "a");
    st.upsert(&k, res("order", "1", json!({"n": 1})))
        .await
        .expect("upsert");
    st.upsert(&k, res("order", "2", json!({"n": 2})))
        .await
        .expect("upsert");
    st.upsert(&k, res("pet", "9", json!({})))
        .await
        .expect("upsert");
    let got = st.get(&k, "order", "1").await.expect("get").expect("found");
    assert_eq!(got, res("order", "1", json!({"n": 1})));
    assert!(st.get(&k, "order", "9").await.expect("get").is_none());
    let l = st.list(&k, "order", 10).await.expect("list");
    assert_eq!(ids(&l), ["2", "1"], "most recent first");
    assert_eq!(st.list(&k, "order", 1).await.expect("list").len(), 1);
    assert_eq!(ids(&st.list(&k, "pet", 10).await.expect("list")), ["9"]);
}

async fn sessions_are_isolated(st: &Arc<dyn StateStore>) {
    st.upsert(&key("iso", "a"), res("order", "1", json!("A")))
        .await
        .expect("upsert");
    st.upsert(&key("iso", "b"), res("order", "1", json!("B")))
        .await
        .expect("upsert");
    st.upsert(&key("iso-other", "a"), res("order", "1", json!("C")))
        .await
        .expect("upsert");
    let get = |m: &'static str, s: &'static str| {
        let st = st.clone();
        async move {
            st.get(&key(m, s), "order", "1")
                .await
                .expect("get")
                .expect("found")
                .data
        }
    };
    assert_eq!(get("iso", "a").await, json!("A"));
    assert_eq!(get("iso", "b").await, json!("B"));
    assert_eq!(get("iso-other", "a").await, json!("C"));
}

async fn delete_removes_and_reports(st: &Arc<dyn StateStore>) {
    let k = key("del", "a");
    st.upsert(&k, res("order", "1", json!({})))
        .await
        .expect("upsert");
    assert!(st.delete(&k, "order", "1").await.expect("delete"));
    assert!(st.get(&k, "order", "1").await.expect("get").is_none());
    assert!(!st.delete(&k, "order", "1").await.expect("delete"));
    // Deleting from an unknown session reports false and creates nothing.
    assert!(
        !st.delete(&key("del", "nope"), "order", "1")
            .await
            .expect("delete")
    );
    let sessions = st.sessions("default", "del").await.expect("sessions");
    assert_eq!(
        sessions
            .iter()
            .map(|s| s.session.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );
}

async fn reset_clears_session(st: &Arc<dyn StateStore>) {
    let k = key("reset", "a");
    st.upsert(&k, res("order", "1", json!({})))
        .await
        .expect("upsert");
    st.upsert(&key("reset", "b"), res("order", "1", json!({})))
        .await
        .expect("upsert");
    st.reset(&k).await.expect("reset");
    assert!(st.list(&k, "order", 10).await.expect("list").is_empty());
    assert!(
        st.get(&key("reset", "b"), "order", "1")
            .await
            .expect("get")
            .is_some()
    );
    let sessions = st.sessions("default", "reset").await.expect("sessions");
    assert_eq!(
        sessions
            .iter()
            .map(|s| s.session.as_str())
            .collect::<Vec<_>>(),
        ["b"],
        "reset removes the session itself"
    );
    // Resetting an unknown session is fine.
    st.reset(&key("reset", "nope")).await.expect("reset");
}

async fn publishes_changes(st: &Arc<dyn StateStore>) {
    let mut rx = st.subscribe();
    let k = key("bus", "a");
    st.upsert(&k, res("order", "1", json!({})))
        .await
        .expect("upsert");
    st.delete(&k, "order", "1").await.expect("delete");
    let a = recv_change(&mut rx).await;
    assert_eq!(
        (a.kind, a.resource_type.as_str(), a.id.as_str(), a.session),
        (ChangeKind::Upserted, "order", "1", k.clone())
    );
    let b = recv_change(&mut rx).await;
    assert_eq!((b.kind, b.id.as_str()), (ChangeKind::Deleted, "1"));
}

async fn recv_change(rx: &mut broadcast::Receiver<StateChanged>) -> StateChanged {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("change published in time")
        .expect("recv")
}

async fn upsert_replaces_and_moves_to_front(st: &Arc<dyn StateStore>) {
    let k = key("replace", "a");
    st.upsert(&k, res("order", "1", json!({"v": 1})))
        .await
        .expect("upsert");
    st.upsert(&k, res("order", "2", json!({"v": 2})))
        .await
        .expect("upsert");
    st.upsert(&k, res("order", "1", json!({"v": 3})))
        .await
        .expect("upsert");
    let l = st.list(&k, "order", 10).await.expect("list");
    assert_eq!(ids(&l), ["1", "2"]);
    assert_eq!(l[0].data, json!({"v": 3}));
}

async fn reads_do_not_create_sessions(st: &Arc<dyn StateStore>) {
    let k = key("reads", "x");
    assert!(st.get(&k, "order", "1").await.expect("get").is_none());
    assert!(st.list(&k, "order", 10).await.expect("list").is_empty());
    assert!(st.export(&k).await.expect("export").is_empty());
    st.touch(&k).await.expect("touch");
    assert!(
        st.sessions("default", "reads")
            .await
            .expect("sessions")
            .is_empty()
    );
}

async fn sessions_lists_with_counts(st: &Arc<dyn StateStore>) {
    let a = key("counts", "a");
    let b = key("counts", "b");
    st.upsert(&a, res("order", "1", json!({})))
        .await
        .expect("upsert");
    st.upsert(&a, res("order", "2", json!({})))
        .await
        .expect("upsert");
    st.upsert(&a, res("pet", "1", json!({})))
        .await
        .expect("upsert");
    tokio::time::sleep(Duration::from_millis(20)).await;
    st.upsert(&b, res("order", "1", json!({})))
        .await
        .expect("upsert");
    // Another mock's sessions are not listed.
    st.upsert(&key("counts-other", "c"), res("order", "1", json!({})))
        .await
        .expect("upsert");

    let s = st.sessions("default", "counts").await.expect("sessions");
    let summary: Vec<(&str, usize)> = s
        .iter()
        .map(|i| (i.session.as_str(), i.resources))
        .collect();
    assert_eq!(summary, [("b", 1), ("a", 3)], "last_touched desc");
    assert!(s[0].last_touched >= s[1].last_touched);

    // Reads do not refresh last_touched.
    tokio::time::sleep(Duration::from_millis(20)).await;
    st.get(&a, "order", "1").await.expect("get");
    st.list(&a, "order", 10).await.expect("list");
    st.export(&a).await.expect("export");
    let s = st.sessions("default", "counts").await.expect("sessions");
    assert_eq!(
        s.iter().map(|i| i.session.as_str()).collect::<Vec<_>>(),
        ["b", "a"],
        "reads must not refresh a session"
    );

    // `touch` does.
    tokio::time::sleep(Duration::from_millis(20)).await;
    st.touch(&a).await.expect("touch");
    let s = st.sessions("default", "counts").await.expect("sessions");
    assert_eq!(
        s.iter().map(|i| i.session.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(s[0].last_touched > s[1].last_touched);
    assert!(
        st.sessions("other-ws", "counts")
            .await
            .expect("sessions")
            .is_empty()
    );
}

async fn export_then_seed_round_trips(st: &Arc<dyn StateStore>) {
    let src = key("fixtures", "src");
    st.upsert(&src, res("order", "1", json!({"n": 1})))
        .await
        .expect("upsert");
    st.upsert(&src, res("pet", "p", json!({"name": "rex"})))
        .await
        .expect("upsert");
    st.upsert(&src, res("order", "2", json!({"n": 2})))
        .await
        .expect("upsert");
    let exported = st.export(&src).await.expect("export");
    assert_eq!(exported.len(), 3);

    // Export is serialisable as a fixture file.
    let fixture = serde_json::to_string(&exported).expect("serialize");
    let parsed: Vec<StoredResource> = serde_json::from_str(&fixture).expect("deserialize");
    assert_eq!(parsed, exported);

    // Seed replaces existing contents.
    let dst = key("fixtures", "dst");
    st.upsert(&dst, res("order", "stale", json!({})))
        .await
        .expect("upsert");
    st.seed(&dst, parsed).await.expect("seed");
    assert!(st.get(&dst, "order", "stale").await.expect("get").is_none());
    let reexported = st.export(&dst).await.expect("export");
    assert_eq!(reexported, exported, "round trip keeps contents and order");
    assert_eq!(
        st.list(&dst, "order", 10).await.expect("list"),
        st.list(&src, "order", 10).await.expect("list"),
        "recency order survives the round trip"
    );

    // Seed creates a session, even with no resources.
    let empty = key("fixtures", "empty");
    st.seed(&empty, Vec::new()).await.expect("seed");
    let s = st.sessions("default", "fixtures").await.expect("sessions");
    let summary: Vec<(&str, usize)> = s
        .iter()
        .map(|i| (i.session.as_str(), i.resources))
        .collect();
    assert!(summary.contains(&("empty", 0)), "{summary:?}");
    assert!(summary.contains(&("dst", 3)), "{summary:?}");
}

/// Real-time TTL check. `store` must have been built with a 1 s TTL.
///
/// `touch` extends a session's life; reads alone do not.
pub async fn ttl_expiry(store: Arc<dyn StateStore>) {
    let k = key("ttl", "a");
    store
        .upsert(&k, res("order", "1", json!({})))
        .await
        .expect("upsert");
    assert!(store.get(&k, "order", "1").await.expect("get").is_some());
    tokio::time::sleep(Duration::from_millis(700)).await;
    store.touch(&k).await.expect("touch");
    tokio::time::sleep(Duration::from_millis(600)).await;
    // 1.3 s after the write, alive thanks to the touch.
    assert!(store.get(&k, "order", "1").await.expect("get").is_some());
    tokio::time::sleep(Duration::from_millis(600)).await;
    // 1.2 s after the touch: the read in between did not extend it.
    assert!(store.get(&k, "order", "1").await.expect("get").is_none());
    assert!(store.list(&k, "order", 10).await.expect("list").is_empty());
    assert!(store.export(&k).await.expect("export").is_empty());
    // Touching an expired session does not revive it.
    store.touch(&k).await.expect("touch");
    assert!(store.get(&k, "order", "1").await.expect("get").is_none());
    assert!(
        store
            .sessions("default", "ttl")
            .await
            .expect("sessions")
            .is_empty()
    );
    // A write after expiry starts a fresh session: old resources stay gone.
    store
        .upsert(&k, res("order", "2", json!({})))
        .await
        .expect("upsert");
    assert!(store.get(&k, "order", "1").await.expect("get").is_none());
    let s = store.sessions("default", "ttl").await.expect("sessions");
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].resources, 1);
}
