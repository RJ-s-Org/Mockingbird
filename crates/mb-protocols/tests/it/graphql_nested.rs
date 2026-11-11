//! Fields below a query's root answered from the session's records.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, graphql};
use mb_state::MemoryStore;
use serde_json::{Value, json};

const SDL: &str = r#"
interface Node { id: ID! }
enum IssueState { OPEN CLOSED }
type Repository implements Node {
  id: ID!
  name: String!
  issue(number: Int!): Issue
  issueOrPullRequest(number: Int!): IssueOrPullRequest
  issues(states: [IssueState!], first: Int): IssueConnection!
  pullRequests(headRefName: String, states: [IssueState!], first: Int): PullRequestConnection!
}
type Issue implements Node {
  id: ID!
  number: Int!
  title: String!
  state: IssueState!
  repository: Repository!
  comments(first: Int): IssueCommentConnection!
}
type IssueComment implements Node { id: ID! body: String! issue: Issue! }
type PullRequest implements Node {
  id: ID!
  number: Int!
  title: String!
  state: IssueState!
  headRefName: String!
  repository: Repository!
}
type IssueConnection { nodes: [Issue] totalCount: Int! }
type IssueCommentConnection { nodes: [IssueComment] totalCount: Int! }
type PullRequestEdge { cursor: String! node: PullRequest }
type PullRequestConnection { edges: [PullRequestEdge] totalCount: Int! }
union IssueOrPullRequest = Issue | PullRequest
input CreateIssueInput { repositoryId: ID! title: String! }
input CloseIssueInput { issueId: ID! }
input AddCommentInput { subjectId: ID! body: String! }
type CreateIssuePayload { issue: Issue }
type CloseIssuePayload { issue: Issue }
type AddCommentPayload { comment: IssueComment }
type Query { repository(owner: String!, name: String!): Repository }
type Mutation {
  createIssue(input: CreateIssueInput!): CreateIssuePayload
  closeIssue(input: CloseIssueInput!): CloseIssuePayload
  addComment(input: AddCommentInput!): AddCommentPayload
}
"#;

async fn serve(script: Vec<Value>) -> String {
    let provider = Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ));
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

async fn gql(base: &str, query: &str) -> Value {
    gql_with(base, query, &[]).await
}

/// [`gql`] with extra headers.
async fn gql_with(base: &str, query: &str, headers: &[(&str, &str)]) -> Value {
    let resp = reqwest::Client::new()
        .post(format!("{base}/graphql"))
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1")
        .headers(
            headers
                .iter()
                .map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap()))
                .collect(),
        )
        .body(json!({ "query": query }).to_string())
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    body["data"].clone()
}

fn issue(id: &str, number: u64, title: &str, state: &str) -> Value {
    json!({"__typename": "Issue", "id": id, "number": number, "title": title, "state": state})
}

fn pull(id: &str, number: u64, head: &str) -> Value {
    json!({"__typename": "PullRequest", "id": id, "number": number, "title": "Generated",
           "state": "OPEN", "headRefName": head})
}

const LIST: &str = r#"{ repository(owner: "o", name: "r") {
    id issues(first: 10, states: [OPEN]) { totalCount nodes { id number title state } } } }"#;

/// A created issue is listed, read by number and by `issueOrPullRequest`,
/// shows its comment, and a closed one leaves the open list for the closed
/// one; a list filter no record matches is empty.
#[tokio::test]
async fn nested_fields_answer_from_state() {
    let repo = |issues: Vec<Value>| json!({"__typename": "Repository", "id": "R_1", "issues": {"__typename": "IssueConnection", "totalCount": issues.len(), "nodes": issues}});
    let base = serve(vec![
        // The first list is generated: one open issue, one the filter rules out.
        repo(vec![issue("I_1", 1, "Seeded", "OPEN"), issue("I_9", 9, "Closed one", "CLOSED")]),
        json!({"__typename": "CreateIssuePayload", "issue": issue("I_2", 2, "Created", "OPEN")}),
        // Later reads of the repository are generated wrong; state wins.
        repo(vec![issue("I_7", 7, "Made up", "OPEN")]),
        json!({"__typename": "Repository", "id": "R_x", "issue": issue("I_8", 2, "Wrong", "OPEN")}),
        json!({"__typename": "Repository", "id": "R_x",
               "issueOrPullRequest": pull("PR_5", 2, "main")}),
        json!({"__typename": "AddCommentPayload", "comment": {"__typename": "IssueComment", "id": "C_1", "body": "Reproduced"}}),
        json!({"__typename": "Repository", "id": "R_x",
               "issue": {"__typename": "Issue", "id": "I_2", "comments": {"__typename": "IssueCommentConnection", "totalCount": 3, "nodes": [
                   {"__typename": "IssueComment", "id": "C_9", "body": "Made up"}]}}}),
        json!({"__typename": "CloseIssuePayload", "issue": {"__typename": "Issue", "id": "I_2", "state": "CLOSED"}}),
        repo(vec![issue("I_2", 2, "Created", "OPEN")]),
        json!({"__typename": "Repository", "id": "R_x", "closed": {"__typename": "IssueConnection", "totalCount": 0, "nodes": []},
               "prs": {"__typename": "PullRequestConnection", "totalCount": 1, "edges": [
                   {"__typename": "PullRequestEdge", "cursor": "c", "node": pull("PR_6", 6, "feature")}]}}),
    ])
    .await;

    let first = gql(&base, LIST).await;
    let nodes = &first["repository"]["issues"]["nodes"];
    assert_eq!(nodes.as_array().unwrap().len(), 1, "{first}");
    assert_eq!(nodes[0]["id"], "I_1");

    // Seeded: the record is stored under the id the answer chose.
    let created = gql_with(
        &base,
        r#"mutation { createIssue(input: {repositoryId: "R_1", title: "Created"}) { issue { id } } }"#,
        &[("x-mockingbird-seed", "1")],
    )
    .await;
    assert_eq!(created["createIssue"]["issue"]["id"], "I_2");

    let listed = gql(&base, LIST).await;
    assert_eq!(listed["repository"]["id"], "R_1", "{listed}");
    let ids: Vec<&str> = listed["repository"]["issues"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"I_1") && ids.contains(&"I_2"), "{listed}");
    assert!(!ids.contains(&"I_7"), "{listed}");
    assert_eq!(listed["repository"]["issues"]["totalCount"], ids.len());

    let by_number = gql(
        &base,
        r#"{ repository(owner: "o", name: "r") { id issue(number: 2) { id title state } } }"#,
    )
    .await;
    assert_eq!(by_number["repository"]["id"], "R_1");
    assert_eq!(by_number["repository"]["issue"]["id"], "I_2");
    assert_eq!(by_number["repository"]["issue"]["title"], "Created");

    let either = gql(
        &base,
        r#"{ repository(owner: "o", name: "r") { issueOrPullRequest(number: 2) {
            __typename ... on Issue { id title } ... on PullRequest { id headRefName } } } }"#,
    )
    .await;
    let found = &either["repository"]["issueOrPullRequest"];
    assert_eq!(found["__typename"], "Issue", "{either}");
    assert_eq!(found["title"], "Created");

    gql(
        &base,
        r#"mutation { addComment(input: {subjectId: "I_2", body: "Reproduced"}) { comment { id } } }"#,
    )
    .await;
    let comments = gql(
        &base,
        r#"{ repository(owner: "o", name: "r") { issue(number: 2) { id
            comments(first: 10) { totalCount nodes { id body } } } } }"#,
    )
    .await;
    let comments = &comments["repository"]["issue"]["comments"];
    assert_eq!(comments["totalCount"], 1, "{comments}");
    assert_eq!(comments["nodes"][0]["body"], "Reproduced");

    gql(
        &base,
        r#"mutation { closeIssue(input: {issueId: "I_2"}) { issue { id state } } }"#,
    )
    .await;
    let open = gql(&base, LIST).await;
    let open_ids: Vec<&str> = open["repository"]["issues"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert!(!open_ids.contains(&"I_2"), "{open}");

    let closed = gql(
        &base,
        r#"{ repository(owner: "o", name: "r") {
            closed: issues(states: [CLOSED]) { totalCount nodes { id state } }
            prs: pullRequests(headRefName: "fix", states: [OPEN]) { totalCount edges { node { id } } } } }"#,
    )
    .await;
    let closed_nodes = &closed["repository"]["closed"]["nodes"];
    assert_eq!(closed_nodes.as_array().unwrap().len(), 1, "{closed}");
    assert_eq!(closed_nodes[0]["id"], "I_2");
    assert_eq!(closed_nodes[0]["state"], "CLOSED");
    // No pull request is stored yet: the generated one is kept unless the
    // filter rules it out, as here.
    assert_eq!(closed["repository"]["prs"]["edges"], json!([]), "{closed}");
}
