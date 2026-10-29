#![cfg(test)]

use mb_store::{DEFAULT_WORKSPACE, NewMock, ResourceSource, connect};

/// The conformance suite, one test per check, each on its own in-memory
/// database (sqlx names every `sqlite::memory:` database uniquely).
mod conformance {
    macro_rules! sqlite_tests {
        ($($check:ident),* $(,)?) => {
            $(
                #[tokio::test]
                async fn $check() {
                    let store = mb_store::connect("sqlite::memory:").await.expect("connect");
                    crate::conformance::$check(&store).await;
                }
            )*
        };
    }
    crate::conformance::conformance_checks!(sqlite_tests);
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
