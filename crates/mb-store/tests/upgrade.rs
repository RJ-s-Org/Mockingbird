//! Upgrading a P2 database (migration 0001) or a P3 one (0001–0003) to the
//! current schema, which adds P5's `0100_events` on top: existing request
//! log rows become `http` events-wise, and the event tables work.
#![cfg(test)]

use std::path::Path;

use chrono::Utc;
use mb_store::{
    ChannelSettings, ControlStore, NewRequestLogEntry, RequestKind, RequestLogFilter,
    ResourceSource,
};
use serde_json::json;
use uuid::Uuid;

const MOCK_ID: &str = "6f1c1d1e-0000-4000-8000-000000000001";

/// The first `count` migrations of `backend` copied into `dir`.
fn first_migrations(backend: &str, count: usize, dir: &Path) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    let mut names: Vec<_> = std::fs::read_dir(&src)
        .expect("read migrations")
        .map(|e| e.expect("entry").file_name())
        .collect();
    names.sort();
    assert!(names.len() > count, "{names:?}");
    for name in names.into_iter().take(count) {
        std::fs::copy(src.join(&name), dir.join(&name)).expect("copy migration");
    }
}

/// A legacy mock and one request-log row, valid for both backends.
const LEGACY_ROWS: [&str; 2] = [
    "INSERT INTO mocks (id, workspace, name, route_prefix, header_overrides, source, created_at, updated_at) \
     VALUES ('6f1c1d1e-0000-4000-8000-000000000001', 'default', 'legacy', '/legacy', FALSE, 'api', \
     '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00')",
    "INSERT INTO request_log (mock_id, operation_id, session, method, path, status, source, \
     latency_ms, prompt_tokens, completion_tokens, request, response, at) \
     VALUES ('6f1c1d1e-0000-4000-8000-000000000001', 'listOrders', 'default', 'GET', '/orders', \
     200, 'llm', 1, 0, 0, '{}', NULL, '2026-01-01T00:00:00+00:00')",
];

/// The upgraded store keeps the legacy row (as `http`) and supports the
/// P3 and P5 additions.
async fn check_upgraded(store: &dyn ControlStore) {
    let mock_id = Uuid::parse_str(MOCK_ID).unwrap();
    let mock = store
        .get_mock("default", "legacy")
        .await
        .expect("get")
        .expect("legacy mock kept");
    assert_eq!(mock.id, mock_id);
    assert!(
        store.get_workspace("default").await.expect("ws").is_some(),
        "P3 workspaces backfilled"
    );

    let rows = store
        .list_request_log(mock_id, &RequestLogFilter::default())
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, RequestKind::Http);

    store
        .append_request_log(
            NewRequestLogEntry {
                mock_id,
                operation_id: Some("orderCreated".into()),
                session: "default".into(),
                method: "PUBLISH".into(),
                path: "orders.created".into(),
                status: 200,
                source: Some("fallback".into()),
                fallback_reason: None,
                latency_ms: 1,
                prompt_tokens: 0,
                completion_tokens: 0,
                request: json!({}),
                response: Some(json!({"id": 1})),
                at: Utc::now(),
                kind: RequestKind::Event,
            },
            100,
        )
        .await
        .expect("append event");
    let events = store
        .list_request_log(
            mock_id,
            &RequestLogFilter {
                kind: Some(RequestKind::Event),
                ..RequestLogFilter::default()
            },
        )
        .await
        .expect("list events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].method, "PUBLISH");

    store
        .put_channel_settings(&ChannelSettings {
            mock_id,
            channel_id: "orderCreated".into(),
            enabled: false,
            triggers: None,
            source: ResourceSource::Api,
            updated_at: Utc::now(),
        })
        .await
        .expect("channel settings");
    assert_eq!(store.list_channel_settings(mock_id).await.unwrap().len(), 1);
}

async fn sqlite_upgrade(applied: usize) {
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}", dir.path().join("old.db").display());
    {
        let migrations = dir.path().join("migrations");
        std::fs::create_dir(&migrations).expect("mkdir");
        first_migrations("sqlite", applied, &migrations);
        let options = SqliteConnectOptions::from_str(&url)
            .expect("url")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .expect("pool");
        let mut migrator = sqlx::migrate::Migrator::new(migrations.as_path())
            .await
            .expect("migrator");
        migrator.dangerous_set_table_name("_mb_store_migrations");
        migrator.run(&pool).await.expect("old migrations");
        for sql in LEGACY_ROWS {
            sqlx::query(sql).execute(&pool).await.expect("legacy row");
        }
        pool.close().await;
    }
    let store = mb_store::connect(&url).await.expect("upgrade");
    check_upgraded(store.as_ref()).await;
}

#[tokio::test]
async fn sqlite_upgrades_from_p2() {
    sqlite_upgrade(1).await;
}

#[tokio::test]
async fn sqlite_upgrades_from_p3() {
    sqlite_upgrade(3).await;
}

async fn postgres_upgrade(applied: usize) {
    use sqlx::Connection;
    use sqlx::postgres::{PgConnection, PgPoolOptions};

    let Ok(base) = std::env::var("MB_TEST_POSTGRES_URL") else {
        println!("skipped: MB_TEST_POSTGRES_URL not set");
        return;
    };
    let schema = format!("mb_{}", Uuid::new_v4().simple());
    let mut conn = PgConnection::connect(&base).await.expect("connect");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&mut conn)
        .await
        .expect("create schema");
    let sep = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{sep}options=-c%20search_path%3D{schema}");
    {
        let dir = tempfile::tempdir().expect("tempdir");
        first_migrations("postgres", applied, dir.path());
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("pool");
        let mut migrator = sqlx::migrate::Migrator::new(dir.path())
            .await
            .expect("migrator");
        migrator.dangerous_set_table_name("_mb_store_migrations");
        migrator.run(&pool).await.expect("old migrations");
        for sql in LEGACY_ROWS {
            sqlx::query(sql).execute(&pool).await.expect("legacy row");
        }
        pool.close().await;
    }
    let store = mb_store::connect(&url).await.expect("upgrade");
    check_upgraded(store.as_ref()).await;
    drop(store);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&mut conn)
        .await
        .expect("drop schema");
}

#[tokio::test]
async fn postgres_upgrades_from_p2() {
    postgres_upgrade(1).await;
}

#[tokio::test]
async fn postgres_upgrades_from_p3() {
    postgres_upgrade(3).await;
}
