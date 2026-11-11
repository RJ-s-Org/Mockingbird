//! Relay-style mutations (one `input:` argument whose members hold the ids, as
//! GitHub's): with no world description written, the record such a mutation
//! names inside its `input` is the one it acts on, and its answer is stored over it.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, graphql};
use mb_state::MemoryStore;
use serde_json::{Value, json};

use crate::support::Records;

const SDL: &str = r#"
enum TicketState { OPEN CLOSED }
type Ticket { id: ID! title: String! state: TicketState! }
input CreateTicketInput { title: String! clientMutationId: String }
type CreateTicketPayload { clientMutationId: String ticket: Ticket }
input CloseTicketInput { ticketId: ID! clientMutationId: String }
type CloseTicketPayload { clientMutationId: String ticket: Ticket }
type Query { ticket(id: ID!): Ticket }
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

/// `closeTicket(input: $in)` with `{ticketId}` acts on the ticket its input names: the model is
/// told the held ticket, its answer's `CLOSED` is stored over it, so a read of it says
/// so, and the request log marks no missing identity or target.
#[tokio::test]
async fn a_relay_mutation_acts_on_the_record_its_input_names() {
    let ticket = |state: &str| json!({"__typename": "Ticket", "id": "T_1", "title": "Crash on start", "state": state});
    let provider = Arc::new(ScriptedProvider::new(
        [
            json!({"__typename": "CreateTicketPayload", "clientMutationId": null, "ticket": ticket("OPEN")}),
            json!({"__typename": "CloseTicketPayload", "clientMutationId": null, "ticket": ticket("CLOSED")}),
        ]
        .into_iter()
        .map(|v| Ok(v.to_string()))
        .collect(),
    ));
    let records = Arc::new(Records::default());
    let mut rt = MockRuntime::new(
        "tickets",
        Arc::new(compile(SDL.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider.clone() as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    rt.recorder = Some(records.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    let session = [("x-mockingbird-session", "s1")];
    let mut seeded = session.to_vec();
    seeded.push(crate::support::SEED);

    let (created, _) = post(
        &base,
        json!({"query": r#"mutation { createTicket(input: {title: "Crash on start"}) { ticket { id state } } }"#}),
        &seeded,
    )
    .await;
    assert!(created.get("errors").is_none(), "{created}");
    assert_eq!(created["data"]["createTicket"]["ticket"]["id"], "T_1");

    let (closed, _) = post(
        &base,
        json!({
            "query": "mutation($in: CloseTicketInput!) { closeTicket(input: $in) { ticket { id state } } }",
            "variables": {"in": {"ticketId": "T_1"}},
        }),
        &session,
    )
    .await;
    assert!(closed.get("errors").is_none(), "{closed}");
    assert_eq!(closed["data"]["closeTicket"]["ticket"]["state"], "CLOSED");
    // The model was told the ticket the input names, as it is stored.
    let prompt: String = provider.requests()[1]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(prompt.contains("Crash on start"), "{prompt}");

    let (read, source) = post(
        &base,
        json!({"query": r#"{ ticket(id: "T_1") { id title state } }"#}),
        &session,
    )
    .await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["data"]["ticket"]["state"], "CLOSED", "{read}");
    assert_eq!(read["data"]["ticket"]["title"], "Crash on start", "{read}");
    assert_eq!(provider.requests().len(), 2);

    let recs = records.wait(3).await;
    let close = recs
        .iter()
        .find(|r| r.operation_id.as_deref() == Some("Mutation.closeTicket"))
        .unwrap_or_else(|| panic!("{recs:?}"));
    assert_eq!(close.request.get("world_gap"), None, "{}", close.request);
    let fallback = close.request["exec_fallback"].as_str();
    assert!(
        !matches!(fallback, Some("no_identity" | "no_target")),
        "{}",
        close.request
    );
}
