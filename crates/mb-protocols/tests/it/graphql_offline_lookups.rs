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
