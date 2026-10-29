#![cfg(test)]

/// The conformance suite on one store in a fresh schema, every check run
/// even when an earlier one fails, so one panic never hides the others.
///
/// Unlike SQLite (one test per check, each on its own database), the checks
/// share a store here: a store migrates under sqlx's advisory lock, which is
/// per database, so one schema per check would serialise 60-odd migration
/// runs (measured: 2.2 s wall and 25 s summed, against 0.5 s for this test).
/// Sharing also keeps proving the checks coexist in one store.
mod conformance {
    macro_rules! postgres_suite {
        ($($check:ident),* $(,)?) => {
            #[tokio::test]
            async fn postgres_conformance() {
                let Some(base) = crate::support::postgres_url() else {
                    return;
                };
                let (url, schema) = crate::support::url_with_fresh_schema(&base).await;
                let store = mb_store::connect(&url).await.expect("connect");
                let mut failed: Vec<&str> = Vec::new();
                $(
                    let shared = store.clone();
                    // A panicking check fails its task, not this test.
                    let run = tokio::spawn(async move {
                        crate::conformance::$check(&shared).await;
                    });
                    if run.await.is_err() {
                        failed.push(stringify!($check));
                    }
                )*
                drop(store);
                crate::support::drop_schema(&base, &schema).await;
                assert!(failed.is_empty(), "failed checks (panics above): {failed:?}");
            }
        };
    }
    crate::conformance::conformance_checks!(postgres_suite);
}
