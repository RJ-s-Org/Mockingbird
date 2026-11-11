//! A GraphQL create of a type read only below another type (a repository's pull
//! requests, with required object and connection members): the record stored
//! keeps what the input sent, whatever the mutation selects.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_protocols::{MockRuntime, graphql};
use mb_state::{MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};

const SDL: &str = r#"
enum PullRequestState { OPEN CLOSED MERGED }
type Repository { id: ID! name: String! pullRequest(number: Int!): PullRequest }
type PullRequest {
  id: ID!
  number: Int!
  title: String!
  headRefName: String!
  baseRefName: String!
  state: PullRequestState!
  url: String!
  repository: Repository!
  author: Actor!
  commits(first: Int): PullRequestCommitConnection!
}
type Actor { login: String! avatarUrl: String! }
type PullRequestCommit { id: ID! oid: String! }
type PullRequestCommitConnection { nodes: [PullRequestCommit!]! totalCount: Int! }
input CreatePullRequestInput { repositoryId: ID! title: String! headRefName: String! baseRefName: String! }
type CreatePullRequestPayload { pullRequest: PullRequest }
type Query { repository(owner: String!, name: String!): Repository }
type Mutation { createPullRequest(input: CreatePullRequestInput!): CreatePullRequestPayload }
"#;

#[tokio::test]
async fn a_create_of_a_type_read_only_below_another_keeps_its_input() {
    let world: mb_mcm::world::World = serde_json::from_value(json!({
        "version": 1,
        "types": {"pullrequest": {"lifecycle": {"member": "state", "states": ["OPEN", "CLOSED", "MERGED"], "initial": "OPEN"}}},
        "operations": {
            "Mutation.createPullRequest": {"kind": "create", "type": "pullrequest", "answer": {"record": "/pullRequest"}}
        }
    }))
    .unwrap();
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&store),
    )
    .with_world(&world, &world, 1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    // The mutation selects only the id and url, as `gh pr create` does.
    let query = r#"mutation { createPullRequest(input: {repositoryId: "R_1", title: "Fix panic",
        headRefName: "fix-empty-path", baseRefName: "main"}) { pullRequest { id url } } }"#;
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

    let key = SessionKey {
        workspace: "default".into(),
        mock: "gh".into(),
        session: "s1".into(),
    };
    let held = store.export(&key).await.unwrap();
    let pr = held
        .iter()
        .find(|r| r.resource_type == "pullrequest")
        .unwrap_or_else(|| panic!("{held:?}"));
    assert_eq!(pr.data["title"], "Fix panic", "{}", pr.data);
    assert_eq!(pr.data["headRefName"], "fix-empty-path", "{}", pr.data);
    assert_eq!(pr.data["baseRefName"], "main", "{}", pr.data);
    assert_eq!(pr.data["state"], "OPEN", "{}", pr.data);
}
