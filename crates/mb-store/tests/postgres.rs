mod common;

use sqlx::Connection;
use sqlx::postgres::PgConnection;
use uuid::Uuid;

const URL_VAR: &str = "MB_TEST_POSTGRES_URL";

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
    let store = mb_store::connect(&url).await.expect("connect");
    common::conformance::run_all(store).await;
    drop_schema(&base, &schema).await;
}

/// Two mocks whose inserts strictly alternate are both pruned (pruning is
/// counted per mock, not keyed on the shared id sequence).
#[tokio::test]
async fn postgres_retention_prunes_interleaved_mocks() {
    use mb_store::{
        DEFAULT_WORKSPACE, NewMock, NewRequestLogEntry, RequestLogFilter, ResourceSource,
    };

    let Ok(base) = std::env::var(URL_VAR) else {
        println!("skipped: MB_TEST_POSTGRES_URL not set");
        return;
    };
    let (url, schema) = url_with_fresh_schema(&base).await;
    let store = mb_store::connect(&url).await.expect("connect");
    let mut ids = Vec::new();
    for name in ["even", "odd"] {
        let mock = store
            .create_mock(
                DEFAULT_WORKSPACE,
                NewMock {
                    name: name.into(),
                    route_prefix: None,
                    persona: None,
                    llm_profile: None,
                    header_overrides: false,
                    source: ResourceSource::Api,
                },
            )
            .await
            .expect("create mock");
        ids.push(mock.id);
    }
    for i in 0..500 {
        let mock_id = ids[i % 2];
        store
            .append_request_log(
                NewRequestLogEntry {
                    mock_id,
                    operation_id: None,
                    session: "s".into(),
                    method: "GET".into(),
                    path: "/".into(),
                    status: 200,
                    source: None,
                    latency_ms: 1,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    request: serde_json::json!({}),
                    response: None,
                    at: chrono::Utc::now(),
                },
                10,
            )
            .await
            .expect("append");
    }
    for mock_id in ids {
        let rows = store
            .list_request_log(
                mock_id,
                &RequestLogFilter {
                    limit: 500,
                    ..RequestLogFilter::default()
                },
            )
            .await
            .expect("list");
        assert!(
            (10..=110).contains(&rows.len()),
            "{} rows remain",
            rows.len()
        );
    }
    drop_schema(&base, &schema).await;
}
