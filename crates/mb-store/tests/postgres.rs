#![cfg(test)]
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
