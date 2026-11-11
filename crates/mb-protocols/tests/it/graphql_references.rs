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

const ACTOR_SDL: &str = r#"
interface Actor { login: String! }
type User implements Actor { id: ID! login: String! name: String! }
type Bot implements Actor { id: ID! login: String! }
type Repository { id: ID! name: String! pullRequest(number: Int!): PullRequest }
type PullRequest { id: ID! number: Int! title: String! author: Actor }
type Query { repository(owner: String!, name: String!): Repository }
"#;

/// Serves `sdl` over a store holding `records`, the model answering `answers`.
async fn serve(sdl: &str, records: Vec<(&str, &str, Value)>, answers: Vec<Value>) -> String {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let key = SessionKey {
        workspace: "default".into(),
        mock: "gh".into(),
        session: "s1".into(),
    };
    for (t, id, data) in records {
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
    let provider = Arc::new(ScriptedProvider::new(
        answers.into_iter().map(|a| Ok(a.to_string())).collect(),
    ));
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(sdl.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        store,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

async fn query(base: &str, query: &str) -> Value {
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

/// A reference to a record the session does not hold is faked under its id,
/// whether or not the selection asks for the id.
#[tokio::test]
async fn a_reference_to_a_record_not_held_is_faked_under_its_id() {
    let answer = || {
        json!({"__typename": "Repository", "id": "R_1", "pullRequest": {"__typename": "PullRequest",
            "id": "PR_1", "number": 5, "title": "Generated", "headRepository": null}})
    };
    for selected in ["name", "id name"] {
        let base = serve(
            SDL,
            vec![
                (
                    "repository",
                    "R_1",
                    json!({"__typename": "Repository", "id": "R_1", "name": "trellis", "nameWithOwner": "o/trellis"}),
                ),
                (
                    "pullrequest",
                    "PR_1",
                    json!({"__typename": "PullRequest", "id": "PR_1", "number": 5, "title": "Fix panic",
                           "headRepository": {"__typename": "Repository", "id": "R_FORK"}}),
                ),
            ],
            vec![answer()],
        )
        .await;
        let body = query(
            &base,
            &format!(
                r#"{{ repository(owner: "o", name: "trellis") {{ id
                pullRequest(number: 5) {{ headRepository {{ {selected} }} }} }} }}"#
            ),
        )
        .await;
        let head = &body["data"]["repository"]["pullRequest"]["headRepository"];
        assert!(head["name"].is_string(), "{body}");
        if selected.starts_with("id") {
            assert_eq!(head["id"], "R_FORK", "{body}");
        }
    }
}

/// What the model answered for a reference the session does not hold stands
/// over the faker's.
#[tokio::test]
async fn an_answered_object_stands_for_a_record_not_held() {
    let base = serve(
        SDL,
        vec![(
            "pullrequest",
            "PR_1",
            json!({"__typename": "PullRequest", "id": "PR_1", "number": 5, "title": "Fix panic",
                   "headRepository": {"__typename": "Repository", "id": "R_FORK"}}),
        )],
        vec![
            json!({"__typename": "Repository", "id": "R_1", "pullRequest": {
            "__typename": "PullRequest", "id": "PR_1", "number": 5, "title": "Generated",
            "headRepository": {"__typename": "Repository", "id": "R_FORK", "name": "trellis-fork",
                               "nameWithOwner": "someone/trellis-fork"}}}),
        ],
    )
    .await;
    let body = query(
        &base,
        r#"{ repository(owner: "o", name: "trellis") { id
        pullRequest(number: 5) { headRepository { name nameWithOwner } } } }"#,
    )
    .await;
    let head = &body["data"]["repository"]["pullRequest"]["headRepository"];
    assert_eq!(head["name"], "trellis-fork", "{body}");
    assert_eq!(head["nameWithOwner"], "someone/trellis-fork", "{body}");
}

/// A reference with no `__typename` to an interface is the concrete record the
/// session holds under its id.
#[tokio::test]
async fn a_reference_to_an_interface_is_the_concrete_record_held() {
    let base = serve(
        ACTOR_SDL,
        vec![
            (
                "user",
                "U_1",
                json!({"__typename": "User", "id": "U_1", "login": "mira", "name": "Mira"}),
            ),
            (
                "pullrequest",
                "PR_1",
                json!({"__typename": "PullRequest", "id": "PR_1", "number": 5, "title": "Fix panic",
                       "author": {"id": "U_1"}}),
            ),
        ],
        vec![json!({"__typename": "Repository", "id": "R_1", "pullRequest": {
            "__typename": "PullRequest", "id": "PR_1", "number": 5, "title": "Generated", "author": null}})],
    )
    .await;
    let body = query(
        &base,
        r#"{ repository(owner: "o", name: "trellis") { id
        pullRequest(number: 5) { author { __typename login ... on User { name } } } } }"#,
    )
    .await;
    let author = &body["data"]["repository"]["pullRequest"]["author"];
    assert_eq!(author["__typename"], "User", "{body}");
    assert_eq!(author["login"], "mira", "{body}");
    assert_eq!(author["name"], "Mira", "{body}");
}

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
