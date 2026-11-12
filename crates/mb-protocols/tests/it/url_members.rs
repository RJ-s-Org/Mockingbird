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

    const MERGES_SDL: &str = r#"
"""An RFC 3986, RFC 3987, and RFC 6570 (level 4) compliant URI string."""
scalar URI
type Project { id: ID! name: String! url: URI }
type MergeRequest { id: ID! number: Int! url: URI! }
input CreateMergeRequestInput { projectId: ID! }
type CreateMergeRequestPayload { mergeRequest: MergeRequest }
type Query { node(id: ID!): MergeRequest }
type Mutation { createMergeRequest(input: CreateMergeRequestInput!): CreateMergeRequestPayload }
"#;

    /// The REST mock of the same API: merges read by path, their page link laid out
    /// under another host, with the collection in the singular.
    const MERGES_REST: &str = r##"
openapi: 3.1.0
info: {title: Tracker, version: "1"}
servers: [{url: "https://api.tracker.test"}]
paths:
  /projects/{owner}/{repo}/merges/{merge_number}:
    get:
      operationId: getMerge
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: merge_number, in: path, required: true, schema: {type: integer}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/MergeRequest"}
components:
  schemas:
    MergeRequest:
      type: object
      required: [id, number, url, html_url]
      properties:
        id: {type: integer}
        node_id: {type: string}
        number: {type: integer}
        url: {type: string, format: uri, example: "https://api.tracker.test/projects/acme/roadmap/merges/12"}
        html_url: {type: string, format: uri, example: "https://tracker.test/acme/roadmap/merge/12"}
"##;

    /// The GraphQL mock, holding project `P_1` (whose `url` is
    /// `https://tracker.test/acme/roadmap`), served; in a `shares` group with the REST
    /// mock when `twin`, which is returned.
    struct Merges {
        addr: std::net::SocketAddr,
        store: Arc<dyn StateStore>,
        key: SessionKey,
        rest: Option<MockRuntime>,
    }

    /// The REST mock beside the GraphQL one: none, one whose type is inferred to be
    /// the same (`merges`), or one whose world names it as the GraphQL one does.
    #[derive(Clone, Copy, PartialEq)]
    enum Peer {
        Absent,
        Inferred,
        SameName,
    }

    async fn merges(peer: Peer) -> Merges {
        let doc: mb_mcm::world::World = serde_json::from_value(json!({
            "version": 1,
            "types": {"mergerequest": {"identity": {"member": "id"}}},
            "operations": {
                "Query.node": {"kind": "read", "type": "mergerequest", "target": {"member": "id"}},
                "Mutation.createMergeRequest": {"kind": "create", "type": "mergerequest",
                    "answer": {"record": "/mergeRequest"}}
            }
        }))
        .unwrap();
        let bundle = compile(MERGES_SDL.as_bytes()).unwrap();
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
        let types = mb_protocols::SharedTypes::default();
        let mut rt = MockRuntime::new(
            "tracker",
            Arc::new(bundle),
            Arc::new(Engine::new(None, EngineConfig::default())),
            Arc::clone(&store),
        )
        .with_world(&base, &world, 1);
        rt.hosts = vec!["api.tracker.test".into()];
        rt.shares = Some("tracker".into());
        rt.shared_types = types.clone();
        rt.shared_types.join(&rt);
        let rest = (peer != Peer::Absent).then(|| {
            let bundle = compile(MERGES_REST.as_bytes()).unwrap();
            let mut rest = MockRuntime::new(
                "tracker-rest",
                Arc::new(bundle.clone()),
                Arc::new(Engine::new(None, EngineConfig::default())),
                Arc::clone(&store),
            );
            if peer == Peer::SameName {
                let doc: mb_mcm::world::World = serde_json::from_value(json!({
                    "version": 1,
                    "types": {"mergerequest": {"identity": {"member": "number"}}},
                    "operations": {"getMerge": {"kind": "read", "type": "mergerequest",
                        "target": {"path_param": "merge_number"}}}
                }))
                .unwrap();
                let base = mb_mcm::world::tier0(&bundle);
                let (world, refused) = mb_mcm::world::effective(&bundle, &base, &doc);
                assert!(refused.is_empty(), "{refused:?}");
                rest = rest.with_world(&base, &world, 1);
            }
            rest.hosts = vec!["api.tracker.test".into()];
            rest.shares = Some("tracker".into());
            rest.shared_types = types.clone();
            rest.shared_types.join(&rest);
            rest
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = mb_protocols::graphql::try_router(Arc::new(rt)).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Merges {
            addr,
            store,
            key,
            rest,
        }
    }

    /// Creates a merge request under `P_1`: the URL the record holds, and its number.
    async fn create_merge_request(m: &Merges) -> (String, i64) {
        let Merges {
            addr, store, key, ..
        } = m;
        let query = r#"mutation { createMergeRequest(input: {projectId: "P_1"}) {
            mergeRequest { id number url } } }"#;
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
        let made = &body["data"]["createMergeRequest"]["mergeRequest"];
        let number = made["number"].as_i64().unwrap_or_else(|| panic!("{body}"));
        let url = made["url"].as_str().unwrap_or_else(|| panic!("{body}"));
        let id = made["id"].as_str().unwrap();
        let held = store.get(key, "mergerequest", id).await.unwrap().unwrap();
        assert_eq!(held.data["url"], url, "{}", held.data);
        (url.to_owned(), number)
    }

    /// A GraphQL type whose REST twin (one `shares` group) lays its page link out
    /// builds its own link that way: the parent's owner and name, the twin's
    /// collection as its example spells it, the record's number last.
    #[tokio::test]
    async fn a_graphql_create_s_url_follows_its_rest_twin_s_page_link() {
        let mut m = merges(Peer::Inferred).await;
        let (url, number) = create_merge_request(&m).await;
        assert_eq!(
            url,
            format!("https://tracker.test/acme/roadmap/merge/{number}")
        );
        // The twin leaving the group, the plural name is back.
        let mut rest = m.rest.take().unwrap();
        rest.shares = None;
        rest.shared_types.join(&rest);
        let (url, number) = create_merge_request(&m).await;
        assert_eq!(
            url,
            format!("https://tracker.test/acme/roadmap/mergerequests/{number}")
        );
    }

    /// With no twin, the link goes under the parent's, by the type's plural name.
    #[tokio::test]
    async fn with_no_twin_a_graphql_create_s_url_takes_the_plural_type_name() {
        let (url, number) = create_merge_request(&merges(Peer::Absent).await).await;
        assert_eq!(
            url,
            format!("https://tracker.test/acme/roadmap/mergerequests/{number}")
        );
    }

    /// A twin storing the type under the GraphQL mock's own name is found too.
    #[tokio::test]
    async fn a_twin_of_the_same_type_name_lays_the_url_out() {
        let (url, number) = create_merge_request(&merges(Peer::SameName).await).await;
        assert_eq!(
            url,
            format!("https://tracker.test/acme/roadmap/merge/{number}")
        );
    }

    /// A neutral tracker with no world description and no `shares` group: projects
    /// are looked up by owner and name, tickets hold an absolute link and a path.
    const LOOKUP_SDL: &str = r#"
"""An RFC 3986, RFC 3987, and RFC 6570 (level 4) compliant URI string."""
scalar URI
type User { id: ID! login: String! }
type Project { id: ID! owner: User! name: String! url: URI }
type Ticket { id: ID! number: Int! title: String! url: URI! resourcePath: URI! }
input CreateTicketInput { projectId: ID! title: String! }
type CreateTicketPayload { ticket: Ticket }
type Query { project(owner: String!, name: String!): Project node(id: ID!): Ticket }
type Mutation { createTicket(input: CreateTicketInput!): CreateTicketPayload }
"#;

    /// Creates a ticket under project `P_1` (owned by `acme`, named `roadmap`, holding
    /// `project_url` when given) on a mock served at host `tracker.test` with only its
    /// inferred world; the model answers `answer` when given (a seeded call), else the
    /// executor fills the record. The ticket's `url`, `resourcePath` and `number`, as
    /// answered and as held.
    async fn create_ticket(project_url: Option<&str>, answer: Option<Value>) -> (Value, Value) {
        let bundle = compile(LOOKUP_SDL.as_bytes()).unwrap();
        let base = mb_mcm::world::tier0(&bundle);
        let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
        let key = SessionKey {
            workspace: "default".into(),
            mock: "tracker".into(),
            session: "s1".into(),
        };
        let mut project = json!({"__typename": "Project", "id": "P_1", "name": "roadmap",
            "owner": {"__typename": "User", "id": "U_1", "login": "acme"}});
        if let Some(url) = project_url {
            project["url"] = json!(url);
        }
        store
            .upsert(
                &key,
                StoredResource {
                    resource_type: "project".into(),
                    id: "P_1".into(),
                    data: project,
                },
            )
            .await
            .unwrap();
        let seeded = answer.is_some();
        let provider = answer.map(|a| {
            Arc::new(mb_llm::ScriptedProvider::new(vec![Ok(a.to_string())]))
                as Arc<dyn mb_llm::Provider>
        });
        let mut rt = MockRuntime::new(
            "tracker",
            Arc::new(bundle),
            Arc::new(Engine::new(provider, EngineConfig::default())),
            Arc::clone(&store),
        )
        .with_world(&base, &base, 1);
        rt.hosts = vec!["tracker.test".into()];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = mb_protocols::graphql::try_router(Arc::new(rt)).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let query = r#"mutation { createTicket(input: {projectId: "P_1", title: "Router panics"}) {
            ticket { id number url resourcePath } } }"#;
        let mut request = reqwest::Client::new()
            .post(format!("http://{addr}/graphql"))
            .header("content-type", "application/json")
            .header("x-mockingbird-session", "s1");
        if seeded {
            request = request.header("x-mockingbird-seed", "1");
        }
        let body: Value = request
            .body(json!({ "query": query }).to_string())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(body.get("errors").is_none(), "{body}");
        let ticket = body["data"]["createTicket"]["ticket"].clone();
        let id = ticket["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{body}"))
            .to_owned();
        let held = store.get(&key, "ticket", &id).await.unwrap().unwrap();
        (ticket, held.data)
    }

    /// The links a ticket numbered `number` should hold under `parent_path`: the
    /// absolute `url` on the mock's host and the same path as `resourcePath`.
    fn assert_links(ticket: &Value, held: &Value, parent_path: &str) {
        let number = ticket["number"]
            .as_i64()
            .unwrap_or_else(|| panic!("{ticket}"));
        let path = format!("{parent_path}/tickets/{number}");
        let url = format!("https://tracker.test{path}");
        for record in [ticket, held] {
            assert_eq!(record["url"], url.as_str(), "{record}");
            assert_eq!(record["resourcePath"], path.as_str(), "{record}");
        }
    }

    /// A parent holding no link has its own built first, from the arguments its type
    /// is looked up by (`project(owner:, name:)`), on the mock's host: the created
    /// record's drawn relative links become absolute ones under it, ending with its
    /// number, and its path-like link the same path.
    #[tokio::test]
    async fn a_parent_holding_no_link_has_its_own_built_from_its_lookup() {
        let drawn = json!({
            "__typename": "CreateTicketPayload",
            "ticket": {"__typename": "Ticket", "id": "T_1", "number": 43,
                "title": "Router panics", "url": "/harbor/nectar", "resourcePath": "/sable/sable"}
        });
        let (ticket, held) = create_ticket(None, Some(drawn)).await;
        assert_eq!(ticket["number"], 43, "{ticket}");
        assert_links(&ticket, &held, "/acme/roadmap");
        // The executor's record, filled with no model, the same.
        let (ticket, held) = create_ticket(None, None).await;
        assert_links(&ticket, &held, "/acme/roadmap");
    }

    /// A parent holding a link keeps the placement under it.
    #[tokio::test]
    async fn a_parent_s_held_link_is_still_what_the_record_goes_under() {
        let held_url = "https://tracker.test/orgs/acme/roadmap";
        let drawn = json!({
            "__typename": "CreateTicketPayload",
            "ticket": {"__typename": "Ticket", "id": "T_1", "number": 43,
                "title": "Router panics", "url": "/harbor/nectar", "resourcePath": "/sable/sable"}
        });
        let (ticket, held) = create_ticket(Some(held_url), Some(drawn)).await;
        assert_links(&ticket, &held, "/orgs/acme/roadmap");
        let (ticket, held) = create_ticket(Some(held_url), None).await;
        assert_links(&ticket, &held, "/orgs/acme/roadmap");
    }
}
