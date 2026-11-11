//! A record a GraphQL mutation writes through the executor is linked to the
//! records its id arguments name, as a generated answer's records are:
//! `createIssue(input: {repositoryId})` makes the issue the repository's.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_protocols::{MockRuntime, graphql};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};

const SDL: &str = r#"
type Repository { id: ID! name: String! issues(first: Int): IssueConnection! }
type Milestone { id: ID! title: String! }
enum IssueState { OPEN CLOSED }
type Issue { id: ID! number: Int! title: String! state: IssueState repository: Repository! milestone: Milestone }
type IssueConnection { nodes: [Issue] totalCount: Int! }
input CreateIssueInput { repositoryId: ID! title: String! milestoneId: ID }
input CloseIssueInput { issueId: ID! milestoneId: ID }
type CreateIssuePayload { issue: Issue }
type CloseIssuePayload { issue: Issue }
type Query { repository(owner: String!, name: String!): Repository issue(id: ID!): Issue }
type Mutation {
  createIssue(input: CreateIssueInput!): CreateIssuePayload
  closeIssue(input: CloseIssueInput!): CloseIssuePayload
}
"#;

async fn gql(base: &str, query: &str) -> Value {
    let body: Value = reqwest::Client::new()
        .post(format!("{base}/graphql"))
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
    body
}

#[tokio::test]
async fn an_executor_create_is_linked_to_the_record_its_id_argument_names() {
    let world: mb_mcm::world::World = serde_json::from_value(json!({
        "version": 1,
        "types": {"issue": {"identity": {"member": "id"}, "lifecycle": {"member": "state", "states": ["OPEN", "CLOSED"], "initial": "OPEN"}}},
        "operations": {
            "Query.issue": {"kind": "read", "type": "issue", "target": {"member": "id"}},
            "Mutation.createIssue": {"kind": "create", "type": "issue", "answer": {"record": "/issue"}},
            "Mutation.closeIssue": {"kind": "action", "type": "issue",
                "target": {"member": "input.issueId"}, "transition": {"state": "CLOSED"},
                "answer": {"record": "/issue"}}
        }
    }))
    .unwrap();
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let key = SessionKey {
        workspace: "default".into(),
        mock: "gh".into(),
        session: "s1".into(),
    };
    store
        .upsert(
            &key,
            StoredResource {
                resource_type: "repository".into(),
                id: "R_1".into(),
                data: json!({"__typename": "Repository", "id": "R_1", "name": "trellis"}),
            },
        )
        .await
        .unwrap();
    store
        .upsert(
            &key,
            StoredResource {
                resource_type: "milestone".into(),
                id: "M_1".into(),
                data: json!({"__typename": "Milestone", "id": "M_1", "title": "v1"}),
            },
        )
        .await
        .unwrap();
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
    let base = format!("http://{addr}");

    let made = gql(
        &base,
        r#"mutation { createIssue(input: {repositoryId: "R_1", title: "Router panics"}) { issue { id } } }"#,
    )
    .await;
    assert_eq!(
        made["extensions"]["mockingbird"]["sources"]["createIssue"], "state",
        "{made}"
    );
    let id = made["data"]["createIssue"]["issue"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let held = store.get(&key, "issue", &id).await.unwrap().unwrap();
    assert_eq!(held.data["repository"]["id"], "R_1", "{}", held.data);
    // So the repository lists it.
    let listed = gql(
        &base,
        r#"{ repository(owner: "o", name: "trellis") { id issues(first: 10) { nodes { id } } } }"#,
    )
    .await;
    let nodes = listed["data"]["repository"]["issues"]["nodes"]
        .as_array()
        .unwrap();
    assert!(nodes.iter().any(|n| n["id"] == id.as_str()), "{listed}");
    // An action the executor answers wrote what it meant to: an id argument
    // naming a record links nothing it left unset.
    let closed = gql(
        &base,
        &format!(
            r#"mutation {{ closeIssue(input: {{issueId: "{id}", milestoneId: "M_1"}}) {{ issue {{ id state }} }} }}"#
        ),
    )
    .await;
    assert_eq!(
        closed["extensions"]["mockingbird"]["sources"]["closeIssue"], "state",
        "{closed}"
    );
    let held = store.get(&key, "issue", &id).await.unwrap().unwrap();
    assert_eq!(held.data["state"], "CLOSED", "{}", held.data);
    assert!(held.data["milestone"].is_null(), "{}", held.data);
}
