//! Upgrading a P2 database (migration 0001) or a P3 one (0001–0003) to the
//! current schema, which adds P5's `0100_events` on top: existing request
//! log rows become `http` events-wise, and the event tables work.
#![cfg(test)]

use std::path::Path;

use chrono::Utc;
use mb_store::{
    ChannelSettings, ControlStore, DataPlaneAuthConfig, NewRequestLogEntry, RequestKind,
    RequestLogFilter, ResourceSource,
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

/// P2-only rows (before migration 0002 added `workspaces`): a mock and an
/// LLM profile in workspaces that 0002 must register. Valid for both
/// backends.
const P2_ROWS: [&str; 2] = [
    "INSERT INTO mocks (id, workspace, name, route_prefix, header_overrides, source, created_at, updated_at) \
     VALUES ('6f1c1d1e-0000-4000-8000-000000000002', 'legacy', 'old', '/old', FALSE, 'api', \
     '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00')",
    "INSERT INTO llm_profiles (workspace, name, base_url, model, json_schema, temperature, \
     timeout_ms, max_tokens, source) VALUES ('profiles-only', 'p', 'http://x', 'm', FALSE, 0, 1, 1, 'api')",
];

/// The legacy rows a database at `applied` migrations gets.
fn legacy_rows(applied: usize) -> Vec<&'static str> {
    let mut rows = LEGACY_ROWS.to_vec();
    if applied == 1 {
        rows.extend(P2_ROWS);
    }
    rows
}

/// The upgraded store keeps the legacy row (as `http`) and supports the
/// P3 and P5 additions. A P2 database (`applied == 1`) also has every
/// workspace its mocks and profiles used registered (migration 0002).
async fn check_upgraded(store: &dyn ControlStore, applied: usize) {
    let mock_id = Uuid::parse_str(MOCK_ID).unwrap();
    let mock = store
        .get_mock("default", "legacy")
        .await
        .expect("get")
        .expect("legacy mock kept");
    assert_eq!(mock.id, mock_id);
    assert_eq!(mock.data_plane_auth, DataPlaneAuthConfig::default());
    let workspaces: Vec<String> = store
        .list_workspaces()
        .await
        .expect("list workspaces")
        .into_iter()
        .map(|w| w.name)
        .collect();
    if applied == 1 {
        assert_eq!(workspaces, ["default", "legacy", "profiles-only"]);
        let old = store
            .get_mock("legacy", "old")
            .await
            .expect("get")
            .expect("P2 mock in a non-default workspace kept");
        assert_eq!(old.data_plane_auth, DataPlaneAuthConfig::default());
        let profile = store
            .get_profile("profiles-only", "p")
            .await
            .expect("get profile")
            .expect("P2 profile kept");
        assert_eq!(profile.model, "m");
        // Written before `output_mode`: read from `json_schema`.
        assert_eq!(profile.output_mode, mb_store::OutputMode::JsonObject);
    } else {
        assert_eq!(workspaces, ["default"]);
    }

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
        for sql in legacy_rows(applied) {
            sqlx::query(sql).execute(&pool).await.expect("legacy row");
        }
        pool.close().await;
    }
    let store = mb_store::connect(&url).await.expect("upgrade");
    check_upgraded(store.as_ref(), applied).await;
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
    use sqlx::postgres::PgPoolOptions;

    use crate::support::{drop_schema, postgres_url, url_with_fresh_schema};

    let Some(base) = postgres_url() else {
        return;
    };
    let (url, schema) = url_with_fresh_schema(&base).await;
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
        for sql in legacy_rows(applied) {
            sqlx::query(sql).execute(&pool).await.expect("legacy row");
        }
        pool.close().await;
    }
    let store = mb_store::connect(&url).await.expect("upgrade");
    check_upgraded(store.as_ref(), applied).await;
    drop(store);
    drop_schema(&base, &schema).await;
}

#[tokio::test]
async fn postgres_upgrades_from_p2() {
    postgres_upgrade(1).await;
}

#[tokio::test]
async fn postgres_upgrades_from_p3() {
    postgres_upgrade(3).await;
}
