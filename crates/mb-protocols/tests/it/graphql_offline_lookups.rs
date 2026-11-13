//! Records read by their arguments with no model configured.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_protocols::{MockRuntime, graphql};
use mb_state::MemoryStore;
use serde_json::{Value, json};

const SDL: &str = r#"
interface Node { id: ID! }
interface RepositoryOwner { id: ID! login: String! }
type User implements Node & RepositoryOwner { id: ID! login: String! }
type Organization implements Node & RepositoryOwner { id: ID! login: String! }
type Repository implements Node {
  id: ID!
  name: String!
  description: String
  owner: RepositoryOwner!
}
type Query {
  repository(owner: String!, name: String!): Repository
  repositoryOwner(login: String!): RepositoryOwner
}
"#;

async fn serve() -> String {
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

async fn gql(base: &str, query: &str) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/graphql"))
        .json(&json!({ "query": query }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn an_owner_argument_names_the_owner_and_one_record() {
    let base = serve().await;
    let first = gql(
        &base,
        r#"{ repository(owner: "fernwood", name: "trellis") { id name owner { login } } }"#,
    )
    .await;
    let repo = &first["data"]["repository"];
    assert_eq!(repo["name"], "trellis", "{first}");
    assert_eq!(repo["owner"]["login"], "fernwood", "{first}");
    // Another shape of the same read names the same record.
    let again = gql(
        &base,
        r#"{ repository(owner: "fernwood", name: "trellis") { id description } }"#,
    )
    .await;
    assert_eq!(again["data"]["repository"]["id"], repo["id"], "{again}");
    // Another owner's repository of the same name is another record.
    let other = gql(
        &base,
        r#"{ repository(owner: "elsewhere", name: "trellis") { id owner { login } } }"#,
    )
    .await;
    assert_eq!(
        other["data"]["repository"]["owner"]["login"], "elsewhere",
        "{other}"
    );
    assert_ne!(other["data"]["repository"]["id"], repo["id"], "{other}");
}

const ISSUES_SDL: &str = r#"
interface Node { id: ID! }
enum IssueState { CLOSED OPEN }
type Repository implements Node { id: ID! name: String! issue(number: Int!): Issue label(name: String!): Label }
type Issue implements Node { id: ID! number: Int! title: String! state: IssueState! repository: Repository! }
type Label implements Node { id: ID! name: String! repository: Repository! }
input CreateIssueInput { repositoryId: ID! title: String! }
type CreateIssuePayload { issue: Issue }
input CreateLabelInput { repositoryId: ID! name: String! }
type CreateLabelPayload { label: Label }
type Query { repository(owner: String!, name: String!): Repository node(id: ID!): Node }
type Mutation {
  createIssue(input: CreateIssueInput!): CreateIssuePayload
  createLabel(input: CreateLabelInput!): CreateLabelPayload
}
"#;

/// With no model, an issue created with no state named starts open, not in its enum's
/// first value (`CLOSED`).
#[tokio::test]
async fn a_created_issue_starts_open_with_no_model() {
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(ISSUES_SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    let repo = gql(&base, r#"{ repository(owner: "o", name: "r") { id } }"#).await;
    let id = repo["data"]["repository"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let created = gql(
        &base,
        &format!(
            r#"mutation {{ createIssue(input: {{repositoryId: "{id}", title: "T"}}) {{ issue {{ number state }} }} }}"#
        ),
    )
    .await;
    assert_eq!(
        created["data"]["createIssue"]["issue"]["state"], "OPEN",
        "{created}"
    );
}

/// With no model, GraphQL creates number their records within the record their input
/// names (`repositoryId`), one past the highest that repository holds, as a REST
/// create under its parent's path does: 1, 2 under one repository, 1 under another.
#[tokio::test]
async fn created_issues_number_within_their_repository() {
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(ISSUES_SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    let mut numbers = Vec::new();
    for repo in ["r", "r", "s"] {
        let read = gql(
            &base,
            &format!(r#"{{ repository(owner: "o", name: "{repo}") {{ id }} }}"#),
        )
        .await;
        let id = read["data"]["repository"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let created = gql(
            &base,
            &format!(
                r#"mutation {{ createIssue(input: {{repositoryId: "{id}", title: "T"}}) {{ issue {{ number }} }} }}"#
            ),
        )
        .await;
        numbers.push(created["data"]["createIssue"]["issue"]["number"].clone());
    }
    assert_eq!(numbers, [json!(1), json!(2), json!(1)]);
}

/// A GraphQL create numbers within the repository its input names even when that
/// repository was never read and the answer selects no number; a lookup argument that
/// is no number (a label's `name`) is unique within nothing: two repositories' labels
/// of one name are both created.
#[tokio::test]
async fn unread_parents_number_apart_and_names_are_not_keys() {
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(ISSUES_SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    // The first create selects no number; the next ones show it counted.
    let quiet = gql(
        &base,
        r#"mutation { createIssue(input: {repositoryId: "R_x", title: "T"}) { issue { id } } }"#,
    )
    .await;
    assert!(
        quiet["data"]["createIssue"]["issue"]["id"].is_string(),
        "{quiet}"
    );
    let mut numbers = Vec::new();
    for repo in ["R_x", "R_y"] {
        let created = gql(
            &base,
            &format!(
                r#"mutation {{ createIssue(input: {{repositoryId: "{repo}", title: "T"}}) {{ issue {{ number }} }} }}"#
            ),
        )
        .await;
        numbers.push(created["data"]["createIssue"]["issue"]["number"].clone());
    }
    assert_eq!(numbers, [json!(2), json!(1)]);
    for repo in ["R_x", "R_y"] {
        let label = gql(
            &base,
            &format!(
                r#"mutation {{ createLabel(input: {{repositoryId: "{repo}", name: "bug"}}) {{ label {{ name }} }} }}"#
            ),
        )
        .await;
        assert_eq!(
            label["data"]["createLabel"]["label"]["name"], "bug",
            "{label}"
        );
    }
}

const CLOSING_SDL: &str = r#"
interface Node { id: ID! }
enum IssueState { CLOSED OPEN }
type Repository implements Node { id: ID! name: String! issue(number: Int!): Issue }
type Issue implements Node { id: ID! number: Int! title: String! state: IssueState! }
input CreateIssueInput { repositoryId: ID! title: String! }
input CloseIssueInput { issueId: ID! }
input ReopenIssueInput { issueId: ID! }
type CreateIssuePayload { issue: Issue }
type CloseIssuePayload { issue: Issue }
type ReopenIssuePayload { issue: Issue }
type Query { repository(owner: String!, name: String!): Repository node(id: ID!): Node }
type Mutation {
  createIssue(input: CreateIssueInput!): CreateIssuePayload
  closeIssue(input: CloseIssueInput!): CloseIssuePayload
  reopenIssue(input: ReopenIssueInput!): ReopenIssuePayload
}
"#;

/// With no model and nothing described, an action's verb moves its target's state:
/// `closeIssue` closes the issue and `reopenIssue` opens it again, as later reads show.
#[tokio::test]
async fn an_action_s_verb_moves_the_state_with_no_model() {
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(CLOSING_SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    let created = gql(
        &base,
        r#"mutation { createIssue(input: {repositoryId: "R_x", title: "T"}) { issue { id state } } }"#,
    )
    .await;
    let issue = &created["data"]["createIssue"]["issue"];
    assert_eq!(issue["state"], "OPEN", "{created}");
    let id = issue["id"].as_str().unwrap_or_default().to_string();
    let read = |base: String, id: String| async move {
        gql(
            &base,
            &format!(r#"{{ node(id: "{id}") {{ ... on Issue {{ state }} }} }}"#),
        )
        .await
    };
    let closed = gql(
        &base,
        &format!(
            r#"mutation {{ closeIssue(input: {{issueId: "{id}"}}) {{ issue {{ state }} }} }}"#
        ),
    )
    .await;
    assert_eq!(
        closed["data"]["closeIssue"]["issue"]["state"], "CLOSED",
        "{closed}"
    );
    let now = read(base.clone(), id.clone()).await;
    assert_eq!(now["data"]["node"]["state"], "CLOSED", "{now}");
    let reopened = gql(
        &base,
        &format!(
            r#"mutation {{ reopenIssue(input: {{issueId: "{id}"}}) {{ issue {{ state }} }} }}"#
        ),
    )
    .await;
    assert_eq!(
        reopened["data"]["reopenIssue"]["issue"]["state"], "OPEN",
        "{reopened}"
    );
    let now = read(base.clone(), id.clone()).await;
    assert_eq!(now["data"]["node"]["state"], "OPEN", "{now}");
}
