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
  pullRequests(headRefName: String, states: [IssueState!], first: Int, after: String): PullRequestConnection!
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
type PageInfo { hasNextPage: Boolean! endCursor: String }
type PullRequestConnection { edges: [PullRequestEdge] totalCount: Int! pageInfo: PageInfo! }
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
    serve_sdl(SDL, script).await
}

async fn serve_sdl(sdl: &str, script: Vec<Value>) -> String {
    let provider = Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ));
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(sdl.as_bytes()).unwrap()),
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

/// Stored edges page by `first` and `after`, with a cursor per edge, even
/// when the generated connection has no edges.
#[tokio::test]
async fn stored_edges_page_with_cursors() {
    let prs = |edges: Vec<Value>| {
        json!({"__typename": "Repository", "id": "R_1", "pullRequests": {
            "__typename": "PullRequestConnection", "totalCount": edges.len(),
            "pageInfo": {"__typename": "PageInfo", "hasNextPage": false, "endCursor": null},
            "edges": edges}})
    };
    let edge = |id: &str, n: u64| json!({"__typename": "PullRequestEdge", "cursor": "g", "node": pull(id, n, "main")});
    let base = serve(vec![
        prs(vec![edge("PR_1", 1), edge("PR_2", 2)]),
        prs(vec![]),
        prs(vec![]),
    ])
    .await;
    let page = |after: &str| {
        format!(
            r#"{{ repository(owner: "o", name: "r") {{ id pullRequests(first: 1{after}) {{
                totalCount edges {{ cursor node {{ id }} }} pageInfo {{ hasNextPage endCursor }} }} }} }}"#
        )
    };
    gql(&base, &page("")).await;
    let first = gql(&base, &page("")).await;
    let prs = &first["repository"]["pullRequests"];
    assert_eq!(prs["totalCount"], 2, "{first}");
    assert_eq!(prs["edges"].as_array().unwrap().len(), 1);
    let cursor = prs["edges"][0]["cursor"].as_str().unwrap().to_string();
    assert_eq!(prs["pageInfo"]["hasNextPage"], true);
    assert_eq!(prs["pageInfo"]["endCursor"], cursor.as_str());

    let next = gql(&base, &page(&format!(r#", after: "{cursor}""#))).await;
    let prs = &next["repository"]["pullRequests"];
    assert_eq!(prs["edges"].as_array().unwrap().len(), 1, "{next}");
    assert_ne!(
        prs["edges"][0]["node"]["id"],
        first["repository"]["pullRequests"]["edges"][0]["node"]["id"]
    );
    assert_eq!(prs["pageInfo"]["hasNextPage"], false);
}

/// A repository read without its id still holds what is listed under it:
/// gh's `issue list` selects no `repository { id }`.
#[tokio::test]
async fn a_record_read_without_its_id_keeps_its_list() {
    let list =
        r#"{ repository(owner: "o", name: "r") { issues(first: 10) { nodes { id number } } } }"#;
    let repo = |issues: Vec<Value>| json!({"__typename": "Repository", "issues": {"__typename": "IssueConnection", "totalCount": issues.len(), "nodes": issues}});
    let base = serve(vec![
        repo(vec![issue("I_1", 1, "Seeded", "OPEN")]),
        json!({"__typename": "Repository", "id": "R_generated"}),
        json!({"__typename": "CreateIssuePayload", "issue": issue("I_2", 2, "Created", "OPEN")}),
        repo(vec![issue("I_7", 7, "Made up", "OPEN")]),
    ])
    .await;
    gql(&base, list).await;
    let repo_id = gql(&base, r#"{ repository(owner: "o", name: "r") { id } }"#).await;
    let repo_id = repo_id["repository"]["id"].as_str().unwrap().to_string();
    assert_ne!(repo_id, "R_generated");
    gql_with(
        &base,
        &format!(
            r#"mutation {{ createIssue(input: {{repositoryId: "{repo_id}", title: "Created"}}) {{ issue {{ id }} }} }}"#
        ),
        &[("x-mockingbird-seed", "1")],
    )
    .await;
    let listed = gql(&base, list).await;
    let mut ids: Vec<&str> = listed["repository"]["issues"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["I_1", "I_2"], "{listed}");
}

/// Items listed without their ids (gh's `issue list`) are stored under
/// their parent by their lookup key (`number`), once each.
#[tokio::test]
async fn items_listed_without_ids_are_stored_by_their_lookup_keys() {
    let list = r#"{ repository(owner: "o", name: "r") { id issues(first: 10) { nodes { number title } } } }"#;
    let untitled =
        |n: u64, title: &str| json!({"__typename": "Issue", "number": n, "title": title});
    let repo = |issues: Vec<Value>| json!({"__typename": "Repository", "id": "R_1", "issues": {"__typename": "IssueConnection", "totalCount": issues.len(), "nodes": issues}});
    let base = serve(vec![
        repo(vec![untitled(1, "One"), untitled(2, "Two")]),
        repo(vec![untitled(5, "Made up")]),
        json!({"__typename": "Repository", "id": "R_1", "issue": untitled(9, "Made up")}),
    ])
    .await;
    gql(&base, list).await;
    let again = gql(&base, list).await;
    let titles: Vec<&str> = again["repository"]["issues"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles.len(), 2, "{again}");
    assert!(
        titles.contains(&"One") && titles.contains(&"Two"),
        "{again}"
    );
    let one = gql(
        &base,
        r#"{ repository(owner: "o", name: "r") { id issue(number: 2) { number title } } }"#,
    )
    .await;
    assert_eq!(one["repository"]["issue"]["title"], "Two", "{one}");
}

const SHOP: &str = r#"
interface Node { id: ID! }
type Product implements Node { id: ID! handle: String! title: String! }
type ProductEdge { cursor: String! node: Product! }
type ProductConnection { edges: [ProductEdge!]! }
type Query {
  products(first: Int, query: String): ProductConnection!
  product(id: ID, handle: String): Product
}
"#;

/// A product listed under `edges[].node` is the one a later read by its
/// handle or by its id answers (nextjs-commerce's storefront).
#[tokio::test]
async fn a_listed_connection_item_is_read_back_by_its_arguments() {
    let product = |id: &str, handle: &str, title: &str| json!({"__typename": "Product", "id": id, "handle": handle, "title": title});
    let base = serve_sdl(
        SHOP,
        vec![
            json!({"__typename": "ProductConnection", "edges": [
                {"__typename": "ProductEdge", "cursor": "a", "node": product("P_1", "tee", "Organic tee")},
                {"__typename": "ProductEdge", "cursor": "b", "node": product("P_2", "mug", "Mug")}]}),
            product("P_9", "tee", "Made up"),
            product("P_8", "other", "Made up too"),
        ],
    )
    .await;
    gql(
        &base,
        r#"{ products(first: 2) { edges { node { id handle title } } } }"#,
    )
    .await;
    let by_handle = gql(&base, r#"{ product(handle: "tee") { id title } }"#).await;
    assert_eq!(
        by_handle["product"],
        json!({"id": "P_1", "title": "Organic tee"})
    );
    let by_id = gql(&base, r#"{ product(id: "P_2") { id handle title } }"#).await;
    assert_eq!(
        by_id["product"],
        json!({"id": "P_2", "handle": "mug", "title": "Mug"})
    );
}
