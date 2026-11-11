//! A stored record's member that refers to another record by id alone
//! (`headRepository: {id}`) is answered as that record when a selection asks
//! for more of it, whatever the model answered there.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, graphql};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};

const SDL: &str = r#"
type Repository { id: ID! name: String! nameWithOwner: String! pullRequest(number: Int!): PullRequest }
type PullRequest { id: ID! number: Int! title: String! headRepository: Repository }
type Query { repository(owner: String!, name: String!): Repository }
"#;

#[tokio::test]
async fn a_reference_by_id_is_answered_as_the_record_it_names() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let key = SessionKey {
        workspace: "default".into(),
        mock: "gh".into(),
        session: "s1".into(),
    };
    for (t, id, data) in [
        (
            "repository",
            "R_1",
            json!({"__typename": "Repository", "id": "R_1", "name": "trellis", "nameWithOwner": "o/trellis"}),
        ),
        (
            "pullrequest",
            "PR_1",
            json!({"__typename": "PullRequest", "id": "PR_1", "number": 5, "title": "Fix panic",
                   "headRepository": {"__typename": "Repository", "id": "R_1"}}),
        ),
    ] {
        store
            .upsert(
                &key,
                StoredResource {
                    resource_type: t.into(),
                    id: id.into(),
                    data,
                },
            )
            .await
            .unwrap();
    }
    // The model answers the nullable `headRepository` with `null`.
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(json!({
        "__typename": "Repository", "id": "R_1", "pullRequest": {"__typename": "PullRequest",
        "id": "PR_1", "number": 5, "title": "Generated", "headRepository": null}})
    .to_string())]));
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let query = r#"{ repository(owner: "o", name: "trellis") { id
        pullRequest(number: 5) { title headRepository { name nameWithOwner } } } }"#;
    let body: Value = reqwest::Client::new()
        .post(format!("http://{addr}/graphql"))
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1")
        .body(json!({ "query": query }).to_string())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    let pr = &body["data"]["repository"]["pullRequest"];
    assert_eq!(pr["title"], "Fix panic", "{body}");
    assert_eq!(pr["headRepository"]["name"], "trellis", "{body}");
    assert_eq!(pr["headRepository"]["nameWithOwner"], "o/trellis", "{body}");
}
