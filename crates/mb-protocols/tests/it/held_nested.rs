//! A record the session holds, shown inside a generated answer that has no target,
//! shows what the session stored for the members it has: a ticket a create stored,
//! answered again among a project's tickets with another title and `hidden` flag,
//! keeps the stored ones. Members the stored record lacks stay as generated.

#[cfg(feature = "graphql")]
mod graphql {
    use std::sync::Arc;
    use std::time::Duration;

    use mb_engine::{Engine, EngineConfig};
    use mb_import::compile;
    use mb_llm::{Provider, ScriptedProvider};
    use mb_protocols::{MockRuntime, graphql};
    use mb_state::MemoryStore;
    use serde_json::{Value, json};

    const SDL: &str = r#"
type Person { id: ID! login: String! }
type Ticket { id: ID! title: String! hidden: Boolean! createdAt: String author: Person }
type TicketConnection { nodes: [Ticket] totalCount: Int! }
type Project { id: ID! name: String! tickets(first: Int): TicketConnection! }
input CreateTicketInput { projectKey: String! title: String! }
type CreateTicketPayload { ticket: Ticket }
type Query { project(key: String!): Project }
type Mutation { createTicket(input: CreateTicketInput!): CreateTicketPayload }
"#;

    async fn serve(script: Vec<Value>) -> String {
        let provider = Arc::new(ScriptedProvider::new(
            script.into_iter().map(|v| Ok(v.to_string())).collect(),
        ));
        let rt = MockRuntime::new(
            "tickets",
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

    /// `query`'s data; with `seeded`, a create keeps its generated id.
    async fn gql(base: &str, query: &str, seeded: bool) -> Value {
        let mut request = reqwest::Client::new()
            .post(format!("{base}/graphql"))
            .header("content-type", "application/json")
            .header("x-mockingbird-session", "s1");
        if seeded {
            request = request.header("x-mockingbird-seed", "1");
        }
        let resp = request
            .body(json!({ "query": query }).to_string())
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        assert!(body.get("errors").is_none(), "{body}");
        body["data"].clone()
    }

    const READ: &str = r#"{ project(key: "p") { id name tickets(first: 10) {
        totalCount nodes { id title hidden createdAt author { login } } } } }"#;

    /// The held ticket, generated again with another title, `hidden` and author
    /// inside a project's connection, shows the stored ones on every read.
    #[tokio::test]
    async fn a_held_record_in_a_generated_connection_shows_its_stored_members() {
        // Generated reads give the held ticket's id with members that differ from
        // read to read.
        let read = |title: &str, hidden: bool, login: &str| {
            json!({"__typename": "Project", "id": "P_1", "name": "Project", "tickets": {"__typename": "TicketConnection", "totalCount": 2, "nodes": [
                {"__typename": "Ticket", "id": "T_1", "title": title, "hidden": hidden, "createdAt": "2026-01-02T00:00:00Z",
                 "author": {"__typename": "Person", "login": login}},
                {"__typename": "Ticket", "id": "T_9", "title": "Another", "hidden": false, "createdAt": null, "author": null}
            ]}})
        };
        let base = serve(vec![
            json!({"__typename": "CreateTicketPayload", "ticket": {"__typename": "Ticket", "id": "T_1",
                   "title": "Stored", "hidden": false,
                   "author": {"__typename": "Person", "id": "U_1", "login": "alice"}}}),
            read("Made up", true, "bob"),
            read("Made up again", true, "carol"),
        ])
        .await;
        let created = gql(
            &base,
            r#"mutation { createTicket(input: {projectKey: "p", title: "Stored"}) {
                ticket { id title hidden author { id login } } } }"#,
            true,
        )
        .await;
        assert_eq!(created["createTicket"]["ticket"]["id"], "T_1", "{created}");

        for _ in 0..2 {
            let read = gql(&base, READ, false).await;
            let nodes = read["project"]["tickets"]["nodes"].as_array().unwrap();
            let held = nodes.iter().find(|n| n["id"] == "T_1").unwrap();
            assert_eq!(held["title"], "Stored", "{read}");
            assert_eq!(held["hidden"], false, "{read}");
            assert_eq!(held["author"]["login"], "alice", "{read}");
            // What the stored record lacks stays as generated.
            assert_eq!(held["createdAt"], "2026-01-02T00:00:00Z", "{read}");
            // A record the session does not hold stays as generated.
            let other = nodes.iter().find(|n| n["id"] == "T_9").unwrap();
            assert_eq!(other["title"], "Another", "{read}");
        }
    }
}

#[cfg(feature = "http-rest")]
mod rest {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::support;

    const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Tickets, version: "1"}
paths:
  /tickets/{ticketId}:
    parameters: [{name: ticketId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTicket
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}
  /projects/{project_id}:
    parameters: [{name: project_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getProject
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
  /projects/{project_id}/tickets:
    parameters: [{name: project_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listProjectTickets
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Ticket"}}}}}
  /people/{personId}:
    parameters: [{name: personId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getPerson
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Person"}}}}
  /projects/{project_id}/summary:
    parameters: [{name: project_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getProjectSummary
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Summary"}}}}
components:
  schemas:
    Ticket:
      type: object
      required: [id, title, hidden]
      properties:
        id: {type: string}
        title: {type: string}
        hidden: {type: boolean}
        priority: {type: integer}
        created_at: {type: string}
        project_id: {type: string}
        assignee: {$ref: "#/components/schemas/Person"}
    Person:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
    Project:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        tickets: {type: array, items: {$ref: "#/components/schemas/Ticket"}}
    Summary:
      type: object
      properties:
        latest: {$ref: "#/components/schemas/Ticket"}
"##;

    /// A held ticket among a project's generated tickets (and as a member of an
    /// answer that is no record) shows its stored members, where they fit: a stored
    /// value of another type than the schema declares stays as generated.
    #[tokio::test]
    async fn a_held_record_nested_in_a_generated_read_shows_its_stored_members() {
        let generated = |title: &str| {
            json!({"id": "T_1", "title": title, "hidden": true, "priority": 3,
                   "created_at": "2026-01-02T00:00:00Z"})
        };
        let h = support::harness(
            CONTRACT,
            vec![
                json!({"id": "P_1", "name": "Project", "tickets": [
                    generated("Made up"),
                    {"id": "T_9", "title": "Another", "hidden": false}
                ]}),
                json!({"latest": generated("Made up again")}),
            ],
            &support::HarnessOptions::default(),
            |rt| rt,
        );
        // `priority` is held as text, which the schema does not allow there.
        h.hold(
            "tickets",
            &[json!({"id": "T_1", "title": "Stored", "hidden": false, "priority": "high"})],
        )
        .await;

        let (status, _, body) = h.send("GET", "/projects/P_1", None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let tickets = body["tickets"].as_array().unwrap();
        let held = tickets.iter().find(|t| t["id"] == "T_1").unwrap();
        assert_eq!(held["title"], "Stored", "{body}");
        assert_eq!(held["hidden"], false, "{body}");
        assert_eq!(held["priority"], 3, "{body}");
        assert_eq!(held["created_at"], "2026-01-02T00:00:00Z", "{body}");
        let other = tickets.iter().find(|t| t["id"] == "T_9").unwrap();
        assert_eq!(other["title"], "Another", "{body}");

        let (status, _, body) = h.send("GET", "/projects/P_1/summary", None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["latest"]["title"], "Stored", "{body}");
        assert_eq!(body["latest"]["hidden"], false, "{body}");
    }

    /// In a list read, a held record nested in an item shows its stored members, while
    /// the list's own items stay with the list's overlay: an item whose stored record
    /// names another parent than the path's stays as generated.
    #[tokio::test]
    async fn a_list_reads_nested_held_record_shows_its_stored_members() {
        let h = support::harness(
            CONTRACT,
            vec![json!([
                {"id": "T_2", "project_id": "P_1", "title": "Generated", "hidden": true,
                 "assignee": {"id": "U_1", "name": "Bob"}}
            ])],
            &support::HarnessOptions::default(),
            |rt| rt,
        );
        h.hold("people", &[json!({"id": "U_1", "name": "Alice"})])
            .await;
        h.hold(
            "tickets",
            &[json!({"id": "T_2", "project_id": "P_2", "title": "Elsewhere", "hidden": false})],
        )
        .await;

        let (status, _, body) = h.send("GET", "/projects/P_1/tickets", None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let item = &body[0];
        assert_eq!(item["assignee"]["name"], "Alice", "{body}");
        assert_eq!(item["title"], "Generated", "{body}");
        assert_eq!(item["project_id"], "P_1", "{body}");
    }
}
