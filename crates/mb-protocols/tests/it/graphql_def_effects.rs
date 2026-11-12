//! An effect on a GraphQL type no root field reads by itself: `Ticket` is read only
//! nested (`project { ticket(number:) }`) and through an interface (`node(id:)`), and
//! made inside a payload. Its own definition is in the contract, and the effect is
//! checked against it rather than rejected for having no schema.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, graphql};
use mb_state::{MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};

use crate::support::Records;

const SDL: &str = r#"
interface Node { id: ID! }
enum TicketState { OPEN CLOSED }
type Ticket implements Node { id: ID! number: Int! title: String! state: TicketState! }
type Project implements Node { id: ID! name: String! ticket(number: Int!): Ticket }
input CreateTicketInput { projectId: ID! title: String! clientMutationId: String }
type CreateTicketPayload { clientMutationId: String ticket: Ticket }
input CloseTicketInput { ticketId: ID! clientMutationId: String }
type CloseTicketPayload { clientMutationId: String ticket: Ticket }
type Query { project(name: String!): Project node(id: ID!): Node }
type Mutation {
  createTicket(input: CreateTicketInput!): CreateTicketPayload
  closeTicket(input: CloseTicketInput!): CloseTicketPayload
}
"#;

async fn post(base: &str, body: Value, headers: &[(&str, &str)]) -> (Value, String) {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/graphql"))
        .header("content-type", "application/json")
        .body(body.to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.unwrap();
    let source = resp
        .headers()
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    (resp.json().await.unwrap(), source)
}

/// A mock of [`SDL`] in which ticket `T_1` is created, then closed by an answer
/// carrying `effect`, the provider answering `later` after that: the mock's base URL,
/// its store, its provider and the close's effect outcome.
async fn closed_with(
    effect: Value,
    later: Vec<Value>,
) -> (String, Arc<MemoryStore>, Arc<ScriptedProvider>, Value) {
    let ticket = json!({"__typename": "Ticket", "id": "T_1", "number": 1, "title": "Crash on start", "state": "OPEN"});
    let provider = Arc::new(ScriptedProvider::new(
        [
            json!({"__typename": "CreateTicketPayload", "clientMutationId": null, "ticket": ticket}),
            json!({
                "response": {"__typename": "CloseTicketPayload", "clientMutationId": null, "ticket": null},
                "effects": [effect],
            }),
        ]
        .into_iter()
        .chain(later)
        .map(|v| Ok(v.to_string()))
        .collect(),
    ));
    let records = Arc::new(Records::default());
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "tickets",
        Arc::new(compile(SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider.clone() as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        state.clone(),
    );
    rt.recorder = Some(records.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    let mut seeded = SESSION.to_vec();
    seeded.push(crate::support::SEED);

    let (created, _) = post(
        &base,
        json!({"query": r#"mutation { createTicket(input: {projectId: "P_1", title: "Crash on start"}) { ticket { id number state } } }"#}),
        &seeded,
    )
    .await;
    assert!(created.get("errors").is_none(), "{created}");
    assert_eq!(created["data"]["createTicket"]["ticket"]["id"], "T_1");

    let (closed, _) = post(
        &base,
        json!({
            "query": "mutation($in: CloseTicketInput!) { closeTicket(input: $in) { clientMutationId } }",
            "variables": {"in": {"ticketId": "T_1"}},
        }),
        &SESSION,
    )
    .await;
    assert!(closed.get("errors").is_none(), "{closed}");

    let recs = records.wait(2).await;
    let close = recs
        .iter()
        .find(|r| r.operation_id.as_deref() == Some("Mutation.closeTicket"))
        .unwrap_or_else(|| panic!("{recs:?}"));
    let outcome = close.request["effects"][0].clone();
    (base, state, provider, outcome)
}

const SESSION: [(&str, &str); 1] = [("x-mockingbird-session", "s1")];

/// The ticket the session holds as `T_1`.
async fn held(state: &MemoryStore) -> Value {
    let session = SessionKey {
        workspace: "default".into(),
        mock: "tickets".into(),
        session: "s1".into(),
    };
    state
        .get(&session, "ticket", "T_1")
        .await
        .unwrap()
        .unwrap()
        .data
}

/// `closeTicket(input: {ticketId})` whose answer carries `{type: "ticket", set:
/// {state: "CLOSED"}}`: the effect is applied, checked against `Ticket`'s own
/// definition, and a later read of the ticket says CLOSED.
#[tokio::test]
async fn an_effect_on_a_type_read_only_nested_is_checked_against_its_definition() {
    // The later read is generated with the ticket made up; the stored one wins.
    let later = json!({"__typename": "Project", "id": "P_1", "name": "Core",
        "ticket": {"__typename": "Ticket", "id": "T_9", "number": 1, "title": "Made up", "state": "OPEN"}});
    let (base, state, provider, outcome) = closed_with(
        json!({"type": "ticket", "id": "T_1", "set": {"state": "CLOSED"}}),
        vec![later],
    )
    .await;
    assert_eq!(outcome["outcome"], "applied", "{outcome}");
    let data = held(&state).await;
    assert_eq!(data["state"], "CLOSED", "{data}");
    assert_eq!(data["title"], "Crash on start", "{data}");

    let (read, source) = post(
        &base,
        json!({"query": r#"{ project(name: "Core") { id ticket(number: 1) { id title state } } }"#}),
        &SESSION,
    )
    .await;
    assert_eq!(
        read["data"]["project"]["ticket"]["id"], "T_1",
        "{read} {source}"
    );
    assert_eq!(
        read["data"]["project"]["ticket"]["state"], "CLOSED",
        "{read}"
    );
    assert_eq!(
        read["data"]["project"]["ticket"]["title"], "Crash on start",
        "{read}"
    );
    assert_eq!(provider.requests().len(), 3);
}

/// The definition is a real check: a value its enum does not take is refused, and the
/// ticket stays as it was.
#[tokio::test]
async fn an_effect_breaking_the_definition_is_rejected() {
    let (_, state, _, outcome) = closed_with(
        json!({"type": "ticket", "id": "T_1", "set": {"state": "ARCHIVED"}}),
        Vec::new(),
    )
    .await;
    assert_eq!(outcome["outcome"], "rejected", "{outcome}");
    assert_eq!(
        outcome["reason"], "the merged resource breaks its schema",
        "{outcome}"
    );
    assert_eq!(held(&state).await["state"], "OPEN");
}
