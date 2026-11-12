//! The URL members of a record a create stores show the place the call names, under the
//! mock's host, whatever the faker drew or the model wrote: a REST record follows its read
//! by path (and the contract's example host for a member that has one), a GraphQL record
//! its parent's URL.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support;

const CONTRACT: &str = r##"
openapi: 3.1.0
info: {title: Tracker, version: "1"}
servers: [{url: "https://api.tracker.test/v1"}]
paths:
  /projects/{project}/tickets:
    post:
      operationId: createTicket
      parameters:
        - {name: project, in: path, required: true, schema: {type: string}}
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [title]
              properties: {title: {type: string}, url: {type: string, format: uri}}
      responses:
        "201":
          description: created
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Ticket"}
  /projects/{project}/tickets/{number}:
    get:
      operationId: getTicket
      parameters:
        - {name: project, in: path, required: true, schema: {type: string}}
        - {name: number, in: path, required: true, schema: {type: integer}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Ticket"}
components:
  schemas:
    Ticket:
      type: object
      required: [number, title, url, html_url]
      properties:
        number: {type: integer}
        title: {type: string}
        url: {type: string, format: uri}
        html_url: {type: string, example: "https://tracker.test/alpha/tickets/1"}
"##;

fn world() -> Value {
    json!({
        "types": {"ticket": {"identity": {"member": "number"}}},
        "operations": {
            "createTicket": {"kind": "create", "type": "ticket"},
            "getTicket": {"kind": "read", "type": "ticket", "target": {"path_param": "number"}}
        }
    })
}

fn tracker(script: Vec<Value>) -> support::DescribedHarness {
    support::harness(
        CONTRACT,
        script,
        &support::HarnessOptions::default(),
        |rt| {
            let mut rt = support::with_effective(rt, world(), false);
            rt.hosts = vec!["api.tracker.test".into()];
            rt.base_paths = vec!["/v1".into()];
            rt
        },
    )
}

/// The API link is the read path under the contract's server; the page link the same
/// path under the host the contract's example uses, as the example lays it out.
fn links(number: i64) -> (String, String) {
    (
        format!("https://api.tracker.test/v1/projects/alpha/tickets/{number}"),
        format!("https://tracker.test/alpha/tickets/{number}"),
    )
}

#[tokio::test]
async fn an_executor_create_s_urls_follow_its_read_path_and_the_mock_s_host() {
    let h = tracker(Vec::new());
    let (status, headers, body) = h
        .send(
            "POST",
            "/projects/alpha/tickets",
            Some(json!({"title": "Router panics"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(headers["x-mockingbird-source"], "state", "{body}");
    let number = body["number"].as_i64().unwrap_or_else(|| panic!("{body}"));
    let (url, html_url) = links(number);
    assert_eq!(body["url"], json!(url), "{body}");
    assert_eq!(body["html_url"], json!(html_url), "{body}");
    let stored = h
        .stored("ticket", &number.to_string())
        .await
        .unwrap_or_else(|| panic!("not stored: {body}"));
    assert_eq!(stored["url"], json!(url), "{stored}");
    assert_eq!(stored["html_url"], json!(html_url), "{stored}");
    let (status, _, read) = h
        .send(
            "GET",
            &format!("/projects/alpha/tickets/{number}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["url"], json!(url), "{read}");
}

#[tokio::test]
async fn a_url_the_request_sends_is_kept() {
    let h = tracker(Vec::new());
    let sent = "https://example.com/mine";
    let (status, _, body) = h
        .send(
            "POST",
            "/projects/alpha/tickets",
            Some(json!({"title": "Router panics", "url": sent})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["url"], sent, "{body}");
    let number = body["number"].as_i64().unwrap_or_else(|| panic!("{body}"));
    assert_eq!(body["html_url"], json!(links(number).1), "{body}");
    let stored = h.stored("ticket", &number.to_string()).await.unwrap();
    assert_eq!(stored["url"], sent, "{stored}");
}

#[tokio::test]
async fn a_generated_create_s_urls_are_moved_to_the_place_the_call_names() {
    // The model's links name another project and number, under placeholder hosts.
    let answer = json!({
        "number": 7,
        "title": "Router panics",
        "url": "https://example.invalid/tickets/142",
        "html_url": "https://example.com/nectar/tickets/142"
    });
    let h = tracker(vec![answer]);
    let (status, _, body) = h
        .seeded(
            "POST",
            "/projects/alpha/tickets",
            Some(json!({"title": "Router panics"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["number"], 7, "{body}");
    let (url, html_url) = links(7);
    assert_eq!(body["url"], json!(url), "{body}");
    assert_eq!(body["html_url"], json!(html_url), "{body}");
    let stored = h
        .stored("ticket", "7")
        .await
        .unwrap_or_else(|| panic!("not stored: {body}"));
    assert_eq!(stored["url"], json!(url), "{stored}");
    assert_eq!(stored["html_url"], json!(html_url), "{stored}");
}

#[cfg(feature = "graphql")]
mod graphql {
    use std::sync::Arc;
    use std::time::Duration;

    use mb_engine::{Engine, EngineConfig};
    use mb_import::compile;
    use mb_protocols::MockRuntime;
    use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
    use serde_json::{Value, json};

    const SDL: &str = r#"
type Project { id: ID! name: String! url: String! }
type Ticket { id: ID! number: Int! title: String! url: String! }
input CreateTicketInput { projectId: ID! title: String! }
type CreateTicketPayload { ticket: Ticket }
type Query { node(id: ID!): Ticket }
type Mutation { createTicket(input: CreateTicketInput!): CreateTicketPayload }
"#;

    #[tokio::test]
    async fn a_graphql_create_s_url_is_placed_under_its_parent_s_on_the_mock_s_host() {
        let doc: mb_mcm::world::World = serde_json::from_value(json!({
            "version": 1,
            "types": {"ticket": {"identity": {"member": "id"}}},
            "operations": {
                "Query.node": {"kind": "read", "type": "ticket", "target": {"member": "id"}},
                "Mutation.createTicket": {"kind": "create", "type": "ticket", "answer": {"record": "/ticket"}}
            }
        }))
        .unwrap();
        let bundle = compile(SDL.as_bytes()).unwrap();
        let base = mb_mcm::world::tier0(&bundle);
        let (world, refused) = mb_mcm::world::effective(&bundle, &base, &doc);
        assert!(refused.is_empty(), "{refused:?}");
        let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
        let key = SessionKey {
            workspace: "default".into(),
            mock: "tracker".into(),
            session: "s1".into(),
        };
        store
            .upsert(
                &key,
                StoredResource {
                    resource_type: "project".into(),
                    id: "P_1".into(),
                    data: json!({"__typename": "Project", "id": "P_1", "name": "roadmap",
                        "url": "https://tracker.test/acme/roadmap"}),
                },
            )
            .await
            .unwrap();
        // The model's link names another ticket, under a placeholder host.
        let provider = Arc::new(mb_llm::ScriptedProvider::new(vec![Ok(json!({
            "__typename": "CreateTicketPayload",
            "ticket": {"__typename": "Ticket", "id": "T_1", "number": 142,
                "url": "https://example.invalid/tickets/7"}
        })
        .to_string())]));
        let mut rt = MockRuntime::new(
            "tracker",
            Arc::new(bundle),
            Arc::new(Engine::new(
                Some(provider as Arc<dyn mb_llm::Provider>),
                EngineConfig::default(),
            )),
            Arc::clone(&store),
        )
        .with_world(&base, &world, 1);
        rt.hosts = vec!["tracker.test".into()];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = mb_protocols::graphql::try_router(Arc::new(rt)).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let query = r#"mutation { createTicket(input: {projectId: "P_1", title: "Router panics"}) {
            ticket { id number url } } }"#;
        let body: Value = reqwest::Client::new()
            .post(format!("http://{addr}/graphql"))
            .header("content-type", "application/json")
            .header("x-mockingbird-session", "s1")
            // A seeded call takes generation: the model answers the payload.
            .header("x-mockingbird-seed", "1")
            .body(json!({ "query": query }).to_string())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(body.get("errors").is_none(), "{body}");
        let ticket = &body["data"]["createTicket"]["ticket"];
        assert_eq!(ticket["number"], 142, "{body}");
        let url = "https://tracker.test/acme/roadmap/tickets/142";
        assert_eq!(ticket["url"], url, "{body}");
        let id = ticket["id"].as_str().unwrap();
        let held = store.get(&key, "ticket", id).await.unwrap().unwrap();
        assert_eq!(held.data["url"], url, "{}", held.data);
    }

    const URI_SDL: &str = r#"
"""An RFC 3986, RFC 3987, and RFC 6570 (level 4) compliant URI string."""
scalar URI
type Project { id: ID! name: String! url: URI }
type Ticket { id: ID! number: Int! url: URI! }
input CreateTicketInput { projectId: ID! title: String! }
type CreateTicketPayload { ticket: Ticket }
type Query { node(id: ID!): Ticket }
type Mutation { createTicket(input: CreateTicketInput!): CreateTicketPayload }
"#;

    /// A `URI` scalar member of a record the executor creates, filled locally with no
    /// model, holds a link under the parent's, not a drawn word.
    #[tokio::test]
    async fn an_executor_create_s_uri_scalar_is_placed_under_its_parent_s() {
        let doc: mb_mcm::world::World = serde_json::from_value(json!({
            "version": 1,
            "types": {"ticket": {"identity": {"member": "id"}}},
            "operations": {
                "Query.node": {"kind": "read", "type": "ticket", "target": {"member": "id"}},
                "Mutation.createTicket": {"kind": "create", "type": "ticket", "answer": {"record": "/ticket"}}
            }
        }))
        .unwrap();
        let bundle = compile(URI_SDL.as_bytes()).unwrap();
        let base = mb_mcm::world::tier0(&bundle);
        let (world, refused) = mb_mcm::world::effective(&bundle, &base, &doc);
        assert!(refused.is_empty(), "{refused:?}");
        let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
        let key = SessionKey {
            workspace: "default".into(),
            mock: "tracker".into(),
            session: "s1".into(),
        };
        store
            .upsert(
                &key,
                StoredResource {
                    resource_type: "project".into(),
                    id: "P_1".into(),
                    data: json!({"__typename": "Project", "id": "P_1", "name": "roadmap",
                        "url": "https://tracker.test/acme/roadmap"}),
                },
            )
            .await
            .unwrap();
        let rt = MockRuntime::new(
            "tracker",
            Arc::new(bundle),
            Arc::new(Engine::new(None, EngineConfig::default())),
            Arc::clone(&store),
        )
        .with_world(&base, &world, 1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = mb_protocols::graphql::try_router(Arc::new(rt)).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let query = r#"mutation { createTicket(input: {projectId: "P_1", title: "Router panics"}) {
            ticket { id number url } } }"#;
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
            body["extensions"]["mockingbird"]["sources"]["createTicket"], "state",
            "{body}"
        );
        let ticket = &body["data"]["createTicket"]["ticket"];
        let number = ticket["number"].as_i64().unwrap();
        let url = format!("https://tracker.test/acme/roadmap/tickets/{number}");
        assert_eq!(ticket["url"], url.as_str(), "{body}");
        let id = ticket["id"].as_str().unwrap();
        let held = store.get(&key, "ticket", id).await.unwrap().unwrap();
        assert_eq!(held.data["url"], url.as_str(), "{}", held.data);
    }
}
