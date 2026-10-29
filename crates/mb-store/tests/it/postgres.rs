#![cfg(test)]

use crate::support::{POSTGRES_URL_VAR, drop_schema, url_with_fresh_schema};

#[tokio::test]
async fn postgres_conformance() {
    let Ok(base) = std::env::var(POSTGRES_URL_VAR) else {
        println!("skipped: MB_TEST_POSTGRES_URL not set");
        return;
    };
    let (url, schema) = url_with_fresh_schema(&base).await;
    let store = mb_store::connect(&url).await.expect("connect");
    crate::conformance::run_all(store).await;
    drop_schema(&base, &schema).await;
}
