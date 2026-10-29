//! Redis backend. Gated by `MB_TEST_REDIS_URL` (see `scripts/test-redis.sh`);
//! each test uses its own random workspace so tests share one server safely.

use std::sync::Arc;
use std::time::Duration;

use mb_state::{RedisStateStore, SessionKey, StateStore, StoredResource};
use serde_json::json;
use uuid::Uuid;

use crate::support::env_url;

const URL_VAR: &str = "MB_TEST_REDIS_URL";
const LONG_TTL: Duration = Duration::from_secs(3600);

fn workspace() -> String {
    format!("ws-{}", Uuid::new_v4().simple())
}

fn key(ws: &str, mock: &str, session: &str) -> SessionKey {
    SessionKey {
        workspace: ws.into(),
        mock: mock.into(),
        session: session.into(),
    }
}

fn order(id: &str, n: i64) -> StoredResource {
    StoredResource {
        resource_type: "order".into(),
        id: id.into(),
        data: json!({ "n": n }),
    }
}

/// Every key under the workspace's prefix, read straight from Redis.
async fn keys_of(url: &str, ws: &str) -> Vec<String> {
    let client = redis::Client::open(url).expect("client");
    let mut con = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");
    redis::cmd("KEYS")
        .arg(format!("mb:{ws}:*"))
        .query_async(&mut con)
        .await
        .expect("KEYS")
}

#[tokio::test]
async fn redis_conformance() {
    let Some(url) = env_url(URL_VAR) else { return };
    let store = mb_state::connect(&url, LONG_TTL).await.expect("connect");
    crate::conformance::run_all_in(store, &workspace()).await;
}

#[tokio::test]
async fn state_session_count_default_matches_sessions() {
    let Some(url) = env_url(URL_VAR) else { return };
    let store = mb_state::connect(&url, LONG_TTL).await.expect("connect");
    crate::conformance::session_count_default_matches_sessions(&store, &workspace()).await;
}

#[tokio::test]
async fn redis_ttl_expiry() {
    let Some(url) = env_url(URL_VAR) else { return };
    let ttl = Duration::from_secs(1);
    let store = RedisStateStore::connect(&url, ttl).await.expect("connect");
    crate::conformance::ttl_expiry_in(Arc::new(store), &workspace(), ttl).await;
}

#[tokio::test]
async fn redis_reset_mock_removes_all_sessions() {
    let Some(url) = env_url(URL_VAR) else { return };
    let ws = workspace();
    let store = RedisStateStore::connect(&url, LONG_TTL)
        .await
        .expect("connect");
    for session in ["a", "b", "c"] {
        store
            .upsert(&key(&ws, "m", session), order("1", 1))
            .await
            .expect("upsert");
    }
    store
        .seed(&key(&ws, "m", "empty"), Vec::new())
        .await
        .expect("seed");
    store
        .upsert(&key(&ws, "kept", "a"), order("1", 1))
        .await
        .expect("upsert");
    assert_eq!(store.sessions(&ws, "m").await.expect("sessions").len(), 4);

    store.reset_mock(&ws, "m").await.expect("reset_mock");

    assert!(store.sessions(&ws, "m").await.expect("sessions").is_empty());
    let mut keys = keys_of(&url, &ws).await;
    keys.sort();
    let kept: Vec<String> = ["meta", "order", "res"]
        .iter()
        .map(|k| format!("mb:{ws}:kept:a:{k}"))
        .chain([format!("mb:{ws}:kept:sessions")])
        .collect();
    assert_eq!(keys, kept, "only the other mock's keys remain");
}

#[tokio::test]
async fn redis_survives_reconnect() {
    let Some(url) = env_url(URL_VAR) else { return };
    let ws = workspace();
    let k = key(&ws, "m", "a");
    {
        let store = RedisStateStore::connect(&url, LONG_TTL)
            .await
            .expect("connect");
        store.upsert(&k, order("1", 1)).await.expect("upsert");
        store.upsert(&k, order("2", 2)).await.expect("upsert");
    }
    let store = mb_state::connect(&url, LONG_TTL).await.expect("reconnect");
    let got = store.get(&k, "order", "1").await.expect("get");
    assert_eq!(got, Some(order("1", 1)));
    let listed = store.list(&k, "order", 10).await.expect("list");
    assert_eq!(listed, [order("2", 2), order("1", 1)]);
    let sessions = store.sessions(&ws, "m").await.expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].resources, 2);
}

#[tokio::test]
async fn redis_unreachable_fails_to_connect() {
    // Port 1 on loopback refuses connections.
    let result = RedisStateStore::connect("redis://127.0.0.1:1", LONG_TTL).await;
    assert!(result.is_err());
}
