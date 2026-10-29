use std::time::Duration;

use sqlx::Connection;
use sqlx::postgres::PgConnection;
use uuid::Uuid;

const URL_VAR: &str = "MB_TEST_POSTGRES_URL";
const LONG_TTL: Duration = Duration::from_secs(3600);

#[tokio::test]
async fn sqlite_memory_conformance() {
    let store = mb_state::connect("sqlite::memory:", LONG_TTL)
        .await
        .expect("connect");
    crate::conformance::run_all(store).await;
}

#[tokio::test]
async fn sqlite_ttl_expiry() {
    let store = mb_state::connect("sqlite::memory:", Duration::from_secs(1))
        .await
        .expect("connect");
    crate::conformance::ttl_expiry(store).await;
}

#[tokio::test]
async fn sqlite_file_persists_across_reconnect() {
    use mb_state::{SessionKey, StoredResource};
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}", dir.path().join("state.db").display());
    let k = SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "a".into(),
    };
    {
        let store = mb_state::connect(&url, LONG_TTL).await.expect("connect");
        store
            .upsert(
                &k,
                StoredResource {
                    resource_type: "order".into(),
                    id: "1".into(),
                    data: serde_json::json!({"n": 1}),
                },
            )
            .await
            .expect("upsert");
    }
    let store = mb_state::connect(&url, LONG_TTL).await.expect("reconnect");
    let got = store.get(&k, "order", "1").await.expect("get");
    assert_eq!(got.expect("persisted").data, serde_json::json!({"n": 1}));
}

#[tokio::test]
async fn unsupported_url_is_rejected() {
    assert!(mb_state::connect("mysql://x", LONG_TTL).await.is_err());
}

/// Creates schema `mb_<uuid>` and returns `base` with its `search_path` set to
/// it, so parallel tests never share tables.
async fn url_with_fresh_schema(base: &str) -> (String, String) {
    let schema = format!("mb_{}", Uuid::new_v4().simple());
    let mut conn = PgConnection::connect(base)
        .await
        .expect("connect to postgres");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&mut conn)
        .await
        .expect("create schema");
    conn.close().await.expect("close");
    let sep = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{sep}options=-c%20search_path%3D{schema}");
    (url, schema)
}

async fn drop_schema(base: &str, schema: &str) {
    let mut conn = PgConnection::connect(base)
        .await
        .expect("connect to postgres");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&mut conn)
        .await
        .expect("drop schema");
}

#[tokio::test]
async fn postgres_conformance() {
    let Ok(base) = std::env::var(URL_VAR) else {
        println!("skipped: MB_TEST_POSTGRES_URL not set");
        return;
    };
    let (url, schema) = url_with_fresh_schema(&base).await;
    let store = mb_state::connect(&url, LONG_TTL).await.expect("connect");
    crate::conformance::run_all(store).await;
    drop_schema(&base, &schema).await;
}

#[tokio::test]
async fn postgres_ttl_expiry() {
    let Ok(base) = std::env::var(URL_VAR) else {
        println!("skipped: MB_TEST_POSTGRES_URL not set");
        return;
    };
    let (url, schema) = url_with_fresh_schema(&base).await;
    let store = mb_state::connect(&url, Duration::from_secs(1))
        .await
        .expect("connect");
    crate::conformance::ttl_expiry(store).await;
    drop_schema(&base, &schema).await;
}

/// Regression: aborting state calls mid-query must never swap the in-memory
/// database for a fresh empty one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_memory_survives_cancelled_operations() {
    use mb_state::{SessionKey, StoredResource};
    let store = mb_state::connect("sqlite::memory:", LONG_TTL)
        .await
        .expect("connect");
    let k = SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "a".into(),
    };
    let res = |id: String| StoredResource {
        resource_type: "order".into(),
        id,
        data: serde_json::json!({"n": 1}),
    };
    store.upsert(&k, res("keep".into())).await.expect("upsert");

    for round in 0..20u32 {
        let mut handles = Vec::new();
        for i in 0..32u32 {
            let (store, k) = (store.clone(), k.clone());
            let r = res(format!("{round}-{i}"));
            handles.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    let _ = store.upsert(&k, r).await;
                } else {
                    let _ = store.list(&k, "order", 50).await;
                }
            }));
        }
        tokio::time::sleep(Duration::from_micros(200 * u64::from(round % 5))).await;
        for h in &handles {
            h.abort();
        }
        for h in handles {
            let _ = h.await;
        }
        for i in 0..8u32 {
            let _ = tokio::time::timeout(
                Duration::from_micros(50),
                store.upsert(&k, res(format!("t-{round}-{i}"))),
            )
            .await;
        }
    }

    let kept = store
        .get(&k, "order", "keep")
        .await
        .expect("schema still exists");
    assert!(
        kept.is_some(),
        "data written before the cancellations survives"
    );
    store
        .upsert(&k, res("after".into()))
        .await
        .expect("write after cancellations");
}
