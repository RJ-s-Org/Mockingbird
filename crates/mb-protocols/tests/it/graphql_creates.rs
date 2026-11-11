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

/// An issue read through `node(id:): Node`, with a custom scalar and required
/// object and connection members, as GitHub's.
const NODE_SDL: &str = r#"
scalar URI
interface Node { id: ID! }
enum IssueState { OPEN CLOSED }
type Actor { login: String! avatarUrl: URI! }
type Repository implements Node { id: ID! name: String! owner: Actor! }
type IssueComment implements Node { id: ID! body: String! author: Actor! }
type IssueCommentConnection { nodes: [IssueComment!]! totalCount: Int! }
type Issue implements Node {
  id: ID!
  number: Int!
  title: String!
  state: IssueState!
  url: URI!
  labelNames: [String!]!
  repository: Repository!
  author: Actor!
  comments(first: Int): IssueCommentConnection!
}
input CreateIssueInput { repositoryId: ID! title: String! }
type CreateIssuePayload { issue: Issue }
type Query { node(id: ID!): Node }
type Mutation { createIssue(input: CreateIssueInput!): CreateIssuePayload }
"#;

#[tokio::test]
async fn a_graphql_create_fills_its_own_scalars_and_no_nested_record() {
    let world: mb_mcm::world::World = serde_json::from_value(json!({
        "version": 1,
        "types": {"issue": {"lifecycle": {"member": "state", "states": ["OPEN", "CLOSED"], "initial": "OPEN"}}},
        "operations": {
            "Query.node": {"kind": "read", "type": "issue", "target": {"member": "id"}},
            "Mutation.createIssue": {"kind": "create", "type": "issue", "answer": {"record": "/issue"}}
        }
    }))
    .unwrap();
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(NODE_SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&store),
    )
    .with_world(&world, &world, 1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let query = r#"mutation { createIssue(input: {repositoryId: "R_1", title: "Router panics"}) {
        issue { id url } } }"#;
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
    assert_eq!(
        body["extensions"]["mockingbird"]["sources"]["createIssue"], "state",
        "{body}"
    );

    let key = SessionKey {
        workspace: "default".into(),
        mock: "gh".into(),
        session: "s1".into(),
    };
    let held = store.export(&key).await.unwrap();
    let issue = held
        .iter()
        .find(|r| r.resource_type == "issue")
        .unwrap_or_else(|| panic!("{held:?}"));
    let data = &issue.data;
    assert_eq!(data["title"], "Router panics", "{data}");
    assert_eq!(data["state"], "OPEN", "{data}");
    // Its own scalars: a custom scalar and a list of strings among them.
    assert!(data["url"].is_string(), "{data}");
    assert!(data["labelNames"].is_array(), "{data}");
    // No record of another type written below it.
    for nested in ["author", "comments"] {
        assert!(data.get(nested).is_none(), "{nested}: {data}");
    }
}

/// A create left to generation (here a seeded call) whose answer holds the
/// record below its top (`/commentEdge/node`), with only the members its
/// selection asks for, still takes what the world `set`s on it, in the answer
/// and in what is stored.
#[tokio::test]
async fn a_generated_create_takes_its_set_where_the_answer_holds_the_record() {
    const COMMENTS: &str = r#"
type IssueComment { id: ID! body: String! isMinimized: Boolean! url: String! }
type IssueCommentEdge { node: IssueComment }
input AddCommentInput { subjectId: ID! body: String! }
type AddCommentPayload { commentEdge: IssueCommentEdge }
type Query { comment(id: ID!): IssueComment }
type Mutation { addComment(input: AddCommentInput!): AddCommentPayload }
"#;
    let doc: mb_mcm::world::World = serde_json::from_value(json!({
        "version": 1,
        "operations": {
            "Mutation.addComment": {"kind": "create", "type": "issuecomment",
                "answer": {"record": "/commentEdge/node"}, "set": {"isMinimized": "false"}}
        }
    }))
    .unwrap();
    let bundle = compile(COMMENTS.as_bytes()).unwrap();
    let base_world = mb_mcm::world::tier0(&bundle);
    let (world, refused) = mb_mcm::world::effective(&bundle, &base_world, &doc);
    assert!(refused.is_empty(), "{refused:?}");
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let provider = Arc::new(mb_llm::ScriptedProvider::new(vec![Ok(json!({
        "__typename": "AddCommentPayload", "commentEdge": {"__typename": "IssueCommentEdge",
        // Only what the selection asks for: no `body` or `url`, which the type requires.
        "node": {"__typename": "IssueComment", "id": "IC_1", "isMinimized": true}}})
    .to_string())]));
    let rt = MockRuntime::new(
        "gh",
        Arc::new(bundle),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn mb_llm::Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store),
    )
    .with_world(&base_world, &world, 1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let query = r#"mutation { addComment(input: {subjectId: "I_1", body: "Reproduced"}) {
        commentEdge { node { id isMinimized } } } }"#;
    let body: Value = reqwest::Client::new()
        .post(format!("http://{addr}/graphql"))
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1")
        .header("x-mockingbird-seed", "1")
        .body(json!({ "query": query }).to_string())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    assert_eq!(
        body["data"]["addComment"]["commentEdge"]["node"]["isMinimized"], false,
        "{body}"
    );
    let key = SessionKey {
        workspace: "default".into(),
        mock: "gh".into(),
        session: "s1".into(),
    };
    let held = store.export(&key).await.unwrap();
    let comment = held
        .iter()
        .find(|r| r.resource_type == "issuecomment")
        .unwrap_or_else(|| panic!("{held:?}"));
    assert_eq!(comment.data["isMinimized"], false, "{}", comment.data);
}

/// A generated create whose record names another type in `__typename` and does
/// not fit its own takes no `set`.
#[tokio::test]
async fn a_generated_record_naming_another_type_takes_no_set() {
    const COMMENTS: &str = r#"
type IssueComment { id: ID! body: String! isMinimized: Boolean! url: String! }
type IssueCommentEdge { node: IssueComment }
input AddCommentInput { subjectId: ID! body: String! }
type AddCommentPayload { commentEdge: IssueCommentEdge }
type Query { comment(id: ID!): IssueComment }
type Mutation { addComment(input: AddCommentInput!): AddCommentPayload }
"#;
    let doc: mb_mcm::world::World = serde_json::from_value(json!({
        "version": 1,
        "operations": {
            "Mutation.addComment": {"kind": "create", "type": "issuecomment",
                "answer": {"record": "/commentEdge/node"}, "set": {"isMinimized": "false"}}
        }
    }))
    .unwrap();
    let bundle = compile(COMMENTS.as_bytes()).unwrap();
    let base_world = mb_mcm::world::tier0(&bundle);
    let (world, refused) = mb_mcm::world::effective(&bundle, &base_world, &doc);
    assert!(refused.is_empty(), "{refused:?}");
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let provider = Arc::new(mb_llm::ScriptedProvider::new(vec![Ok(json!({
        "__typename": "AddCommentPayload", "commentEdge": {"__typename": "IssueCommentEdge",
        "node": {"__typename": "Notice", "id": "IC_1", "isMinimized": true}}})
    .to_string())]));
    let rt = MockRuntime::new(
        "gh",
        Arc::new(bundle),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn mb_llm::Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store),
    )
    .with_world(&base_world, &world, 1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let query = r#"mutation { addComment(input: {subjectId: "I_1", body: "Reproduced"}) {
        commentEdge { node { id isMinimized } } } }"#;
    let _: Value = reqwest::Client::new()
        .post(format!("http://{addr}/graphql"))
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1")
        .header("x-mockingbird-seed", "1")
        .body(json!({ "query": query }).to_string())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key = SessionKey {
        workspace: "default".into(),
        mock: "gh".into(),
        session: "s1".into(),
    };
    let held = store.export(&key).await.unwrap();
    let set = held
        .iter()
        .filter(|r| r.resource_type == "issuecomment")
        .any(|r| r.data["isMinimized"] == false);
    assert!(!set, "{held:?}");
}

/// A `set` writing a whole GraphQL connection need not spell `__typename` at every
/// level: the schema fixes it, so the value is stored and answered with it, not
/// dropped for not fitting.
#[tokio::test]
async fn a_set_object_takes_the_typenames_its_schema_fixes() {
    const CARTS: &str = r#"
type CartLine { id: ID! quantity: Int! }
type CartLineEdge { node: CartLine! }
type CartLineConnection { edges: [CartLineEdge!]! }
type Cart { id: ID! totalQuantity: Int! lines: CartLineConnection! }
type CartCreatePayload { cart: Cart }
type Query { cart(id: ID!): Cart }
type Mutation { cartCreate: CartCreatePayload }
"#;
    let doc: mb_mcm::world::World = serde_json::from_value(json!({
        "version": 1,
        "operations": {
            "Mutation.cartCreate": {"kind": "create", "type": "cart",
                "answer": {"record": "/cart"},
                "set": {
                    "totalQuantity": "2",
                    "lines": "{'edges': [{'node': {'id': 'gid://shopify/CartLine/1', 'quantity': 2}}]}"
                }}
        }
    }))
    .unwrap();
    let bundle = compile(CARTS.as_bytes()).unwrap();
    let base_world = mb_mcm::world::tier0(&bundle);
    let (world, refused) = mb_mcm::world::effective(&bundle, &base_world, &doc);
    assert!(refused.is_empty(), "{refused:?}");
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let provider = Arc::new(mb_llm::ScriptedProvider::new(vec![Ok(json!({
        "__typename": "CartCreatePayload", "cart": {"__typename": "Cart", "id": "gid://shopify/Cart/c1",
        "totalQuantity": 0, "lines": {"__typename": "CartLineConnection", "edges": []}}})
    .to_string())]));
    let rt = MockRuntime::new(
        "shop",
        Arc::new(bundle),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn mb_llm::Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store),
    )
    .with_world(&base_world, &world, 1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let query = "mutation { cartCreate { cart { id totalQuantity lines { edges { node { id quantity } } } } } }";
    let body: Value = reqwest::Client::new()
        .post(format!("http://{addr}/graphql"))
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1")
        .header("x-mockingbird-seed", "1")
        .body(json!({ "query": query }).to_string())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    let cart = &body["data"]["cartCreate"]["cart"];
    assert_eq!(cart["totalQuantity"], 2, "{body}");
    assert_eq!(cart["lines"]["edges"][0]["node"]["quantity"], 2, "{body}");
    let key = SessionKey {
        workspace: "default".into(),
        mock: "shop".into(),
        session: "s1".into(),
    };
    let held = store.export(&key).await.unwrap();
    let stored = held
        .iter()
        .find(|r| r.resource_type == "cart")
        .unwrap_or_else(|| panic!("{held:?}"));
    let lines = &stored.data["lines"];
    assert_eq!(lines["__typename"], "CartLineConnection", "{lines}");
    assert_eq!(lines["edges"][0]["__typename"], "CartLineEdge", "{lines}");
    assert_eq!(
        lines["edges"][0]["node"]["__typename"], "CartLine",
        "{lines}"
    );
}
