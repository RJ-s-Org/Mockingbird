mod common;

use mb_store::{DEFAULT_WORKSPACE, NewMock, ResourceSource, connect};

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
