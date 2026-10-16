mod common;

use std::str::FromStr;

use mb_store::{DEFAULT_WORKSPACE, DataPlaneAuthConfig, NewMock, ResourceSource, connect};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

#[tokio::test]
async fn sqlite_memory_conformance() {
    let store = connect("sqlite::memory:").await.expect("connect");
    common::conformance::run_all(store).await;
}

#[tokio::test]
async fn sqlite_file_url_creates_db() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("control.db");
    let url = format!("sqlite://{}", path.display());
    {
        let store = connect(&url).await.expect("connect");
        store
            .create_mock(
                DEFAULT_WORKSPACE,
                NewMock {
                    name: "persisted".into(),
                    route_prefix: None,
                    persona: None,
                    llm_profile: None,
                    header_overrides: false,
                    source: ResourceSource::Api,
                    data_plane_auth: Default::default(),
                },
            )
            .await
            .expect("create");
    }
    assert!(path.exists(), "database file is created");
    let store = connect(&url).await.expect("reconnect");
    let mock = store
        .get_mock(DEFAULT_WORKSPACE, "persisted")
        .await
        .expect("get")
        .expect("mock persisted across reconnect");
    assert_eq!(mock.route_prefix, "/persisted");
}

/// Migration 0002 registers every workspace already used by P2 data, plus
/// `default`, and gives existing mocks the default data-plane auth.
#[tokio::test]
async fn sqlite_migration_backfills_workspaces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("legacy.db");
    let url = format!("sqlite://{}", path.display());
    {
        // A P2 database: only migration 0001 applied.
        let only_first = dir.path().join("migrations");
        std::fs::create_dir(&only_first).expect("mkdir");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/migrations/sqlite/0001_init.sql"
            ),
            only_first.join("0001_init.sql"),
        )
        .expect("copy");
        let options = SqliteConnectOptions::from_str(&url)
            .expect("url")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .expect("pool");
        let mut migrator = sqlx::migrate::Migrator::new(only_first.as_path())
            .await
            .expect("migrator");
        migrator.dangerous_set_table_name("_mb_store_migrations");
        migrator.run(&pool).await.expect("0001");
        sqlx::query(
            "INSERT INTO mocks (id, workspace, name, route_prefix, header_overrides, source, created_at, updated_at) \
             VALUES ('6f1c1d1e-0000-4000-8000-000000000001', 'legacy', 'old', '/old', 0, 'api', \
             '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00')",
        )
        .execute(&pool)
        .await
        .expect("legacy mock");
        sqlx::query(
            "INSERT INTO llm_profiles (workspace, name, base_url, model, json_schema, temperature, \
             timeout_ms, max_tokens, source) VALUES ('profiles-only', 'p', 'http://x', 'm', 0, 0, 1, 1, 'api')",
        )
        .execute(&pool)
        .await
        .expect("legacy profile");
        pool.close().await;
    }
    let store = connect(&url).await.expect("migrate to latest");
    let names: Vec<String> = store
        .list_workspaces()
        .await
        .expect("list")
        .into_iter()
        .map(|w| w.name)
        .collect();
    assert_eq!(names, vec!["default", "legacy", "profiles-only"]);
    let old = store
        .get_mock("legacy", "old")
        .await
        .expect("get")
        .expect("legacy mock kept");
    assert_eq!(old.data_plane_auth, DataPlaneAuthConfig::default());
}
