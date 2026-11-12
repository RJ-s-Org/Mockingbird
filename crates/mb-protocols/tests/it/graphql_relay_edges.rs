//! Relay creates that answer their record through a connection edge (as GitHub's
//! `addComment` answers `{commentEdge {node}}`) and name its parent by an id of an
//! interface type (`subjectId` naming a `Node`): with no world description written,
//! the executor makes the record whole, links it to the record the id names, and a
//! later read of that record's connection lists it from state.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_protocols::{MockRuntime, graphql};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};

use crate::support::Records;

const SDL: &str = r#"
scalar URI
interface Node { id: ID! }
type Ticket implements Node { id: ID! number: Int! notes(first: Int): NoteConnection! }
type Note implements Node { id: ID! body: String! hidden: Boolean! url: URI ticket: Ticket! }
type NoteEdge { cursor: String! node: Note }
type NoteConnection { nodes: [Note] edges: [NoteEdge] totalCount: Int! }
input AddNoteInput { subjectId: ID! body: String! clientMutationId: String }
type AddNotePayload { clientMutationId: String noteEdge: NoteEdge subject: Node }
type Query { ticket(number: Int!): Ticket node(id: ID!): Node }
type Mutation { addNote(input: AddNoteInput!): AddNotePayload }
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
async fn a_note_added_through_an_edge_is_held_whole_under_the_subject_its_id_names() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let key = SessionKey {
        workspace: "default".into(),
        mock: "tickets".into(),
        session: "s1".into(),
    };
    store
        .upsert(
            &key,
            StoredResource {
                resource_type: "ticket".into(),
                id: "T_1".into(),
                data: json!({"__typename": "Ticket", "id": "T_1", "number": 412}),
            },
        )
        .await
        .unwrap();
    let records = Arc::new(Records::default());
    let mut rt = MockRuntime::new(
        "tickets",
        Arc::new(compile(SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&store),
    );
    rt.recorder = Some(records.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");

    // The client selects the note's url, as `gh issue comment` does, and the edge's
    // cursor, which the executor draws beside the note.
    let added = gql(
        &base,
        r#"mutation { addNote(input: {subjectId: "T_1", body: "Seen on 2.3 too"}) { noteEdge { cursor node { url } } } }"#,
    )
    .await;
    assert_eq!(
        added["extensions"]["mockingbird"]["sources"]["addNote"], "state",
        "{added}"
    );
    let edge = &added["data"]["addNote"]["noteEdge"];
    assert!(edge["node"].is_object(), "{added}");
    assert!(edge["cursor"].is_string(), "{added}");

    let read = r#"{ ticket(number: 412) { id notes(first: 10) { nodes { id body hidden } } } }"#;
    let first = gql(&base, read).await;
    let second = gql(&base, read).await;
    let nodes = first["data"]["ticket"]["notes"]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("{first}"));
    let note = nodes
        .iter()
        .find(|n| n["body"] == "Seen on 2.3 too")
        .unwrap_or_else(|| panic!("{first}"));
    assert!(note["hidden"].is_boolean(), "{first}");
    assert_eq!(first, second);
    // Stored whole, not as the members the create selected.
    let id = note["id"].as_str().unwrap();
    let held = store.get(&key, "note", id).await.unwrap().unwrap();
    assert!(held.data["hidden"].is_boolean(), "{}", held.data);
    assert_eq!(held.data["ticket"]["id"], "T_1", "{}", held.data);

    let recs = records.wait(3).await;
    let add = recs
        .iter()
        .find(|r| r.operation_id.as_deref() == Some("Mutation.addNote"))
        .unwrap_or_else(|| panic!("{recs:?}"));
    assert_eq!(add.request.get("world_gap"), None, "{}", add.request);
    assert_ne!(
        add.request["exec_fallback"].as_str(),
        Some("no_identity"),
        "{}",
        add.request
    );
}
