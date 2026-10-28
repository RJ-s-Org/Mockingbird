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
                    chaos: None,
                    limits: Default::default(),
                    data_plane_auth: Default::default(),
                    behavior: Default::default(),
                    hosts: vec![],
                    webhooks: Vec::new(),
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

fn cancel_mock(name: String) -> NewMock {
    NewMock {
        name,
        route_prefix: None,
        persona: None,
        llm_profile: None,
        header_overrides: false,
        source: ResourceSource::Api,
        chaos: None,
        limits: Default::default(),
        data_plane_auth: Default::default(),
        behavior: Default::default(),
        hosts: vec![],
        webhooks: Vec::new(),
    }
}

/// Regression: aborting store calls mid-query (a client cancelling admin
/// requests) must never swap the in-memory database for a fresh empty one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_memory_survives_cancelled_operations() {
    let store = connect("sqlite::memory:").await.expect("connect");
    store
        .create_mock(DEFAULT_WORKSPACE, cancel_mock("keep".into()))
        .await
        .expect("create");

    for round in 0..20u32 {
        let mut handles = Vec::new();
        for i in 0..32u32 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    let _ = store
                        .create_mock(DEFAULT_WORKSPACE, cancel_mock(format!("m-{round}-{i}")))
                        .await;
                } else {
                    let _ = store.list_mocks(DEFAULT_WORKSPACE).await;
                }
            }));
        }
        // Let the operations start, then cancel them wherever they are.
        tokio::time::sleep(std::time::Duration::from_micros(200 * u64::from(round % 5))).await;
        for h in &handles {
            h.abort();
        }
        for h in handles {
            let _ = h.await;
        }
        // Also drop futures directly via a tiny timeout.
        for i in 0..8u32 {
            let _ = tokio::time::timeout(
                std::time::Duration::from_micros(50),
                store.create_mock(DEFAULT_WORKSPACE, cancel_mock(format!("t-{round}-{i}"))),
            )
            .await;
        }
    }

    // The schema and the original data are still there.
    let workspaces = store
        .list_workspaces()
        .await
        .expect("workspaces table exists");
    assert!(workspaces.iter().any(|w| w.name == DEFAULT_WORKSPACE));
    assert!(
        store
            .get_mock(DEFAULT_WORKSPACE, "keep")
            .await
            .expect("mocks table exists")
            .is_some(),
        "data written before the cancellations survives"
    );
    // And the store still accepts writes.
    store
        .create_mock(DEFAULT_WORKSPACE, cancel_mock("after".into()))
        .await
        .expect("write after cancellations");
}
