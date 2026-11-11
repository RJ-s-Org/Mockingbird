//! Mocks of one API sharing their sessions (`shares`): a record one stores,
//! the other reads, in its own shape.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, graphql, http_rest};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

const REST: &str = r##"
openapi: 3.0.3
info: {title: issues, version: "1"}
paths:
  /issues:
    post:
      operationId: createIssue
      requestBody:
        required: true
        content:
          application/json:
            schema: {type: object, required: [title], properties: {title: {type: string}}}
      responses:
        "201":
          description: created
          content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}
  /issues/{number}:
    get:
      operationId: getIssue
      parameters: [{name: number, in: path, required: true, schema: {type: integer}}]
      responses:
        "200":
          description: the issue
          content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}
        "404": {description: missing}
components:
  schemas:
    Issue:
      type: object
      required: [number, title, state, created_at]
      properties:
        number: {type: integer}
        title: {type: string}
        state: {type: string, enum: [open, closed]}
        created_at: {type: string, format: date-time}
"##;

const SDL: &str = r#"
enum IssueState { OPEN CLOSED }
type Issue { id: ID! number: Int! title: String! state: IssueState! createdAt: String }
type Query { issueByNumber(number: Int!): Issue }
type Mutation { createIssue(title: String!): Issue }
"#;

fn runtime(
    name: &str,
    contract: &str,
    script: Vec<Value>,
    state: Arc<dyn StateStore>,
) -> Arc<MockRuntime> {
    let provider = Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ));
    let mut rt = MockRuntime::new(
        name,
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        state,
    );
    rt.shares = Some("gh".into());
    Arc::new(rt)
}

async fn rest(router: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn gql(router: &axum::Router, query: &str, seed: bool) -> Value {
    let mut req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1");
    if seed {
        req = req.header("x-mockingbird-seed", "1");
    }
    let req = req
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    body["data"].clone()
}

#[tokio::test]
async fn rest_and_graphql_read_what_the_other_stored() {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let issue = |number: u64, title: &str| {
        json!({"__typename": "Issue", "id": format!("I_{number}"), "number": number, "title": title,
               "state": "OPEN", "createdAt": "2024-01-01T00:00:00Z"})
    };
    let gh = runtime(
        "github-graphql",
        SDL,
        // Numbered past what a create draws (0 to 99), so the REST create's
        // number never names it.
        vec![issue(7007, "Over GraphQL"), issue(99, "Made up")],
        Arc::clone(&state),
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let rest_rt = runtime(
        "github-rest",
        REST,
        vec![
            json!({"number": 8, "title": "Over REST", "state": "open", "created_at": "2024-02-02T00:00:00Z"}),
            json!({"number": 7, "title": "Made up", "state": "open", "created_at": "2024-02-02T00:00:00Z"}),
        ],
        Arc::clone(&state),
    );
    let rest_router = http_rest::router(rest_rt);

    gql(
        &gql_router,
        r#"mutation { createIssue(title: "Over GraphQL") { id } }"#,
        true,
    )
    .await;
    let read = rest(&rest_router, "GET", "/issues/7007", None).await;
    assert_eq!(read["title"], "Over GraphQL", "{read}");
    assert_eq!(read["state"], "open", "{read}");

    let made = rest(
        &rest_router,
        "POST",
        "/issues",
        Some(json!({"title": "Over REST"})),
    )
    .await;
    let number = made["number"].as_u64().unwrap();
    let back = gql(
        &gql_router,
        &format!("{{ issueByNumber(number: {number}) {{ number title state }} }}"),
        false,
    )
    .await;
    assert_eq!(back["issueByNumber"]["title"], "Over REST", "{back}");
    assert_eq!(back["issueByNumber"]["state"], "OPEN", "{back}");
}

/// Types whose names differ are the same as the world says (`same_as`).
#[tokio::test]
async fn same_as_names_the_other_mock_s_type() {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let sdl = SDL
        .replace("Issue", "Ticket")
        .replace("TicketState", "IssueState");
    let world = |doc: Value| serde_json::from_value::<mb_mcm::world::World>(doc).unwrap();
    let rest_world = world(json!({"version": 1, "types": {"issues": {"same_as": ["ticket"]}}}));
    let gh = runtime(
        "github-graphql",
        &sdl,
        vec![
            json!({"__typename": "Ticket", "id": "T_7", "number": 7, "title": "Over GraphQL",
                    "state": "CLOSED", "createdAt": "2024-01-01T00:00:00Z"}),
        ],
        Arc::clone(&state),
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let rest_rt = runtime("github-rest", REST, vec![], Arc::clone(&state));
    let rest_rt = Arc::try_unwrap(rest_rt)
        .ok()
        .unwrap()
        .with_world(&rest_world, &rest_world, 1);
    let rest_router = http_rest::router(Arc::new(rest_rt));

    gql(
        &gql_router,
        r#"mutation { createTicket(title: "Over GraphQL") { id } }"#,
        true,
    )
    .await;
    let read = rest(&rest_router, "GET", "/issues/7", None).await;
    assert_eq!(read["title"], "Over GraphQL", "{read}");
    assert_eq!(read["state"], "closed", "{read}");
}

/// A read scoped under its owner (`/repos/{owner}/{repo}/issues/{number}`, its
/// identity described as `number`) finds the record the other mock stored, whose
/// owner it holds in its own shape (a `repository` object with an id).
#[tokio::test]
async fn an_owner_scoped_read_finds_the_other_mock_s_record() {
    const SCOPED: &str = r##"
openapi: 3.0.3
info: {title: issues, version: "1"}
paths:
  /repos/{owner}/{repo}/issues/{issue_number}:
    get:
      operationId: getIssue
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: issue_number, in: path, required: true, schema: {type: integer}}
      responses:
        "200":
          description: the issue
          content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}
        "404": {description: missing}
components:
  schemas:
    Issue:
      type: object
      required: [number, title, state]
      properties:
        id: {type: integer}
        number: {type: integer}
        title: {type: string}
        state: {type: string, enum: [open, closed]}
        repository_url: {type: string}
"##;
    const GQL: &str = r#"
enum IssueState { OPEN CLOSED }
type Repository { id: ID! name: String! }
type Issue { id: ID! number: Int! title: String! state: IssueState! repository: Repository! }
input CreateIssueInput { repositoryId: ID! title: String! }
type CreateIssuePayload { issue: Issue }
type Query { repository(owner: String!, name: String!): Repository }
type Mutation { createIssue(input: CreateIssueInput!): CreateIssuePayload }
"#;
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let gh = runtime(
        "github-graphql",
        GQL,
        vec![
            json!({"__typename": "CreateIssuePayload", "issue": {"__typename": "Issue",
            "id": "I_1", "number": 7007, "title": "Over GraphQL", "state": "OPEN",
            "repository": {"__typename": "Repository", "id": "R_1", "name": "trellis"}}}),
        ],
        Arc::clone(&state),
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let world = serde_json::from_value::<mb_mcm::world::World>(json!({"version": 1,
        "types": {"issues": {"identity": {"member": "number"}}}}))
    .unwrap();
    let rest_rt = runtime("github-rest", SCOPED, vec![], Arc::clone(&state));
    let rest_rt = Arc::try_unwrap(rest_rt)
        .ok()
        .unwrap()
        .with_world(&world, &world, 1);
    let rest_router = http_rest::router(Arc::new(rest_rt));

    gql(
        &gql_router,
        r#"mutation { createIssue(input: {repositoryId: "R_1", title: "Over GraphQL"}) { issue { id number } } }"#,
        true,
    )
    .await;
    let read = rest(
        &rest_router,
        "GET",
        "/repos/fernwood/trellis/issues/7007",
        None,
    )
    .await;
    assert_eq!(read["title"], "Over GraphQL", "{read}");
}

/// The other mock's record refers to its owner by an id only (GraphQL's
/// `repository: {id}`): a reference that names no owner says nothing of the one
/// the path names (`{repo}` = `trellis`), so the read still finds the record.
#[tokio::test]
async fn an_owner_reference_by_id_alone_does_not_hide_the_record() {
    const SCOPED: &str = r##"
openapi: 3.0.3
info: {title: issues, version: "1"}
paths:
  /repos/{owner}/{repo}/issues/{issue_number}:
    get:
      operationId: getIssue
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: issue_number, in: path, required: true, schema: {type: integer}}
      responses:
        "200":
          description: the issue
          content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}
        "404": {description: missing}
components:
  schemas:
    Issue:
      type: object
      required: [number, title, state]
      properties:
        number: {type: integer}
        title: {type: string}
        state: {type: string, enum: [open, closed]}
        repository:
          type: object
          properties:
            id: {type: integer}
            name: {type: string}
            full_name: {type: string}
"##;
    const GQL: &str = r#"
enum IssueState { OPEN CLOSED }
type Repository { id: ID! name: String! }
type Issue { id: ID! number: Int! title: String! state: IssueState! repository: Repository! }
input CreateIssueInput { repositoryId: ID! title: String! }
type CreateIssuePayload { issue: Issue }
type Query { repository(owner: String!, name: String!): Repository }
type Mutation { createIssue(input: CreateIssueInput!): CreateIssuePayload }
"#;
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let gh = runtime(
        "github-graphql",
        GQL,
        vec![
            json!({"__typename": "CreateIssuePayload", "issue": {"__typename": "Issue",
            "id": "I_1", "number": 7007, "title": "Over GraphQL", "state": "OPEN",
            "repository": {"__typename": "Repository", "id": "R_1"}}}),
        ],
        Arc::clone(&state),
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let world = serde_json::from_value::<mb_mcm::world::World>(json!({"version": 1,
        "types": {"issues": {"identity": {"member": "number"}}}}))
    .unwrap();
    let rest_rt = runtime("github-rest", SCOPED, vec![], Arc::clone(&state));
    let rest_rt = Arc::try_unwrap(rest_rt)
        .ok()
        .unwrap()
        .with_world(&world, &world, 1);
    let rest_router = http_rest::router(Arc::new(rest_rt));

    gql(
        &gql_router,
        r#"mutation { createIssue(input: {repositoryId: "R_1", title: "Over GraphQL"}) { issue { id number } } }"#,
        true,
    )
    .await;
    let read = rest(
        &rest_router,
        "GET",
        "/repos/fernwood/trellis/issues/7007",
        None,
    )
    .await;
    assert_eq!(read["title"], "Over GraphQL", "{read}");
}
