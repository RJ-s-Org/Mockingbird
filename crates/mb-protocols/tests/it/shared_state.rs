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
use mb_protocols::{MockRuntime, SharedTypes, graphql, http_rest};
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

/// A record whose owner reference holds an id (`repository: {id: 5}`) still
/// names its owner: a read under a repository it does not hold is not answered
/// from it. Only a reference naming nothing (`{}`) says nothing of the owner.
#[tokio::test]
async fn an_owner_reference_holding_an_id_still_hides_the_record() {
    const REPOS: &str = r##"
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
      required: [number, title]
      properties:
        number: {type: integer}
        title: {type: string}
        repository: {type: object, properties: {id: {type: integer}, name: {type: string}}}
"##;
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let key = mb_state::SessionKey {
        workspace: "default".into(),
        mock: "github-rest".into(),
        session: "s1".into(),
    };
    state
        .upsert(
            &key,
            mb_state::StoredResource {
                resource_type: "issues".into(),
                id: "7".into(),
                data: json!({"number": 7, "title": "Another repository's", "repository": {"id": 5}}),
            },
        )
        .await
        .unwrap();
    let world = serde_json::from_value::<mb_mcm::world::World>(json!({"version": 1,
        "types": {"issues": {"identity": {"member": "number"}}}}))
    .unwrap();
    let mut rt = Arc::try_unwrap(runtime("github-rest", REPOS, vec![], Arc::clone(&state)))
        .ok()
        .unwrap();
    rt.shares = None;
    let router = http_rest::router(Arc::new(rt.with_world(&world, &world, 1)));
    let read = rest(&router, "GET", "/repos/fernwood/trellis/issues/7", None).await;
    assert_ne!(read["title"], "Another repository's", "{read}");
}

const PULLS: &str = r##"
openapi: 3.0.3
info: {title: pulls, version: "1"}
paths:
  /repos/{owner}/{repo}/pulls:
    post:
      operationId: createPull
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
      requestBody:
        required: true
        content:
          application/json:
            schema: {type: object, required: [title], properties: {title: {type: string}}}
      responses:
        "201":
          description: created
          content: {application/json: {schema: {$ref: "#/components/schemas/pull-request"}}}
  /repos/{owner}/{repo}/pulls/{pull_number}:
    get:
      operationId: getPull
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: pull_number, in: path, required: true, schema: {type: integer}}
      responses:
        "200":
          description: the pull request
          content: {application/json: {schema: {$ref: "#/components/schemas/pull-request"}}}
        "404": {description: missing}
components:
  schemas:
    pull-request:
      type: object
      required: [id, node_id, number, title, state]
      properties:
        id: {type: integer}
        node_id: {type: string}
        number: {type: integer}
        title: {type: string}
        state: {type: string, enum: [open, closed]}
"##;

const PULLS_SDL: &str = r#"
enum PullRequestState { OPEN CLOSED MERGED }
type PullRequest { id: ID! number: Int! title: String! state: PullRequestState! }
type Query { pullRequestByNumber(number: Int!): PullRequest }
type Mutation { createPullRequest(title: String!): PullRequest }
"#;

/// [`runtime`] joined to `types`, the record types of its `shares` group, with
/// `world` when given.
fn joined(
    (name, contract): (&str, &str),
    script: Vec<Value>,
    state: &Arc<dyn StateStore>,
    types: &SharedTypes,
    world: Option<&mb_mcm::world::World>,
) -> Arc<MockRuntime> {
    let mut rt = Arc::try_unwrap(runtime(name, contract, script, Arc::clone(state)))
        .ok()
        .unwrap();
    if let Some(world) = world {
        rt = rt.with_world(world, world, 1);
    }
    rt.shared_types = types.clone();
    rt.shared_types.join(&rt);
    Arc::new(rt)
}

fn pull_request(number: u64, title: &str) -> Value {
    json!({"__typename": "PullRequest", "id": format!("PR_{number}"), "number": number,
           "title": title, "state": "OPEN"})
}

/// With no world, REST `pulls` (answering `pull-request`s, by `number`) and GraphQL
/// `PullRequest` (with a `number` too) are one type: a REST read finds the pull
/// request a GraphQL mutation created, its `id` as REST's `node_id`, and back.
#[tokio::test]
async fn rest_collections_and_graphql_types_of_one_record_are_inferred_the_same() {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let types = SharedTypes::default();
    let gh = joined(
        ("github-graphql", PULLS_SDL),
        vec![
            pull_request(7007, "Over GraphQL"),
            pull_request(99, "Made up"),
        ],
        &state,
        &types,
        None,
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let rest_rt = joined(
        ("github-rest", PULLS),
        vec![
            json!({"id": 8, "node_id": "PR_8", "number": 8, "title": "Over REST", "state": "open"}),
            json!({"id": 7, "node_id": "PR_7", "number": 7, "title": "Made up", "state": "open"}),
        ],
        &state,
        &types,
        None,
    );
    let rest_router = http_rest::router(rest_rt);

    gql(
        &gql_router,
        r#"mutation { createPullRequest(title: "Over GraphQL") { id number } }"#,
        true,
    )
    .await;
    let read = rest(
        &rest_router,
        "GET",
        "/repos/fernwood/trellis/pulls/7007",
        None,
    )
    .await;
    assert_eq!(read["title"], "Over GraphQL", "{read}");
    assert_eq!(read["state"], "open", "{read}");
    assert_eq!(read["node_id"], "PR_7007", "{read}");

    let made = rest(
        &rest_router,
        "POST",
        "/repos/fernwood/trellis/pulls",
        Some(json!({"title": "Over REST"})),
    )
    .await;
    let number = made["number"].as_u64().unwrap();
    let back = gql(
        &gql_router,
        &format!("{{ pullRequestByNumber(number: {number}) {{ number title state }} }}"),
        false,
    )
    .await;
    assert_eq!(back["pullRequestByNumber"]["title"], "Over REST", "{back}");
    assert_eq!(back["pullRequestByNumber"]["state"], "OPEN", "{back}");
}

/// The world's `same_as` wins over what the contracts suggest: REST `pulls` said
/// to be another type is not read as GraphQL's `PullRequest`.
#[tokio::test]
async fn a_world_s_same_as_wins_over_an_inferred_type() {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let types = SharedTypes::default();
    let gh = joined(
        ("github-graphql", PULLS_SDL),
        vec![pull_request(7007, "Over GraphQL")],
        &state,
        &types,
        None,
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let world = serde_json::from_value::<mb_mcm::world::World>(
        json!({"version": 1, "types": {"pulls": {"same_as": ["changeset"]}}}),
    )
    .unwrap();
    let rest_rt = joined(
        ("github-rest", PULLS),
        vec![
            json!({"id": 1, "node_id": "PR_1", "number": 7007, "title": "Made up", "state": "open"}),
        ],
        &state,
        &types,
        Some(&world),
    );
    let rest_router = http_rest::router(rest_rt);

    gql(
        &gql_router,
        r#"mutation { createPullRequest(title: "Over GraphQL") { id number } }"#,
        true,
    )
    .await;
    let read = rest(
        &rest_router,
        "GET",
        "/repos/fernwood/trellis/pulls/7007",
        None,
    )
    .await;
    assert_ne!(read["title"], "Over GraphQL", "{read}");
}

/// Mocks that do not `share` are not linked, whatever their types: GraphQL's
/// pull request is not REST's.
#[tokio::test]
async fn mocks_that_share_nothing_infer_nothing() {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let types = SharedTypes::default();
    let unshared = |rt: Arc<MockRuntime>| {
        let mut rt = Arc::try_unwrap(rt).ok().unwrap();
        rt.shares = None;
        rt.shared_types.join(&rt);
        Arc::new(rt)
    };
    let gh = unshared(joined(
        ("github-graphql", PULLS_SDL),
        vec![pull_request(7007, "Over GraphQL")],
        &state,
        &types,
        None,
    ));
    let gql_router = graphql::try_router(gh).unwrap();
    let rest_rt = unshared(joined(
        ("github-rest", PULLS),
        vec![
            json!({"id": 1, "node_id": "PR_1", "number": 7007, "title": "Made up", "state": "open"}),
        ],
        &state,
        &types,
        None,
    ));
    let rest_router = http_rest::router(rest_rt);

    gql(
        &gql_router,
        r#"mutation { createPullRequest(title: "Over GraphQL") { id number } }"#,
        true,
    )
    .await;
    let read = rest(
        &rest_router,
        "GET",
        "/repos/fernwood/trellis/pulls/7007",
        None,
    )
    .await;
    assert_ne!(read["title"], "Over GraphQL", "{read}");
}

/// The gh trial's run 5: worlds saying `same_as` both ways and unknown ids
/// answered 404. A pull request GraphQL created (its record keyed by its node id)
/// is read over REST by its `number`, not answered "does not exist".
#[tokio::test]
async fn a_rest_read_by_number_finds_a_graphql_created_record_under_unknown_ids_404() {
    let world = |doc: Value| serde_json::from_value::<mb_mcm::world::World>(doc).unwrap();
    let described = (
        world(json!({"version": 1, "types": {"pullrequest": {"same_as": ["pulls"]}}})),
        world(json!({"version": 1, "types": {"pulls": {
            "same_as": ["pullrequest"], "identity": {"member": "number"}}}})),
    );
    read_by_number_under_404(Some(&described)).await;
    // The REST world says `same_as` alone: the path's `{pull_number}` still reads
    // the record's `number`.
    let same_as_only = (
        described.0.clone(),
        world(json!({"version": 1, "types": {"pulls": {"same_as": ["pullrequest"]}}})),
    );
    read_by_number_under_404(Some(&same_as_only)).await;
    // The run itself had no world: the types are inferred.
    read_by_number_under_404(None).await;
}

async fn read_by_number_under_404(worlds: Option<&(mb_mcm::world::World, mb_mcm::world::World)>) {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let types = SharedTypes::default();
    let gh = joined(
        ("github-graphql", PULLS_SDL),
        vec![pull_request(128, "Over GraphQL")],
        &state,
        &types,
        worlds.map(|(gql, _)| gql),
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let mut rest_rt = Arc::try_unwrap(joined(
        ("github-rest", PULLS),
        vec![],
        &state,
        &types,
        worlds.map(|(_, rest)| rest),
    ))
    .ok()
    .unwrap();
    rest_rt.unknown_ids = mb_protocols::UnknownIds::NotFound;
    let rest_router = http_rest::router(Arc::new(rest_rt));

    gql(
        &gql_router,
        r#"mutation { createPullRequest(title: "Over GraphQL") { id number } }"#,
        false,
    )
    .await;
    let read = rest(
        &rest_router,
        "GET",
        "/repos/fernwood/trellis/pulls/128",
        None,
    )
    .await;
    assert_eq!(read["title"], "Over GraphQL", "{read}");
    assert_eq!(read["number"], 128, "{read}");
}

/// A type of the group named like the reading one turns inference off for it:
/// REST `pulls` reads GraphQL's `Pull`, not its `PullRequest`.
#[tokio::test]
async fn a_type_named_alike_in_the_group_turns_inference_off() {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let types = SharedTypes::default();
    let sdl = PULLS_SDL.replace(
        "type Query {",
        "type Pull { id: ID! number: Int! title: String! }\ntype Query { pull(number: Int!): Pull ",
    );
    let gh = joined(
        ("github-graphql", &sdl),
        vec![pull_request(7007, "Over GraphQL")],
        &state,
        &types,
        None,
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let rest_rt = joined(
        ("github-rest", PULLS),
        vec![
            json!({"id": 1, "node_id": "PR_1", "number": 7007, "title": "Made up", "state": "open"}),
        ],
        &state,
        &types,
        None,
    );
    let rest_router = http_rest::router(rest_rt);
    gql(
        &gql_router,
        r#"mutation { createPullRequest(title: "Over GraphQL") { id number } }"#,
        true,
    )
    .await;
    let read = rest(
        &rest_router,
        "GET",
        "/repos/fernwood/trellis/pulls/7007",
        None,
    )
    .await;
    assert_ne!(read["title"], "After leaving", "{read}");
}

/// What a mock adds to the group goes when it leaves it (its `shares` unset, as a
/// server does for a mock deleted or unmounted): what was inferred from it is
/// forgotten.
#[tokio::test]
async fn a_mock_leaving_the_group_takes_its_types_with_it() {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let types = SharedTypes::default();
    let gh = joined(
        ("github-graphql", PULLS_SDL),
        vec![
            pull_request(7007, "Over GraphQL"),
            pull_request(7008, "After leaving"),
        ],
        &state,
        &types,
        None,
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let rest_rt = joined(
        ("github-rest", PULLS),
        vec![
            json!({"id": 1, "node_id": "PR_1", "number": 7008, "title": "Made up", "state": "open"}),
        ],
        &state,
        &types,
        None,
    );
    let rest_router = http_rest::router(rest_rt);
    let create = |title: &str| {
        format!(r#"mutation {{ createPullRequest(title: "{title}") {{ id number }} }}"#)
    };
    gql(&gql_router, &create("Over GraphQL"), true).await;
    let read = rest(
        &rest_router,
        "GET",
        "/repos/fernwood/trellis/pulls/7007",
        None,
    )
    .await;
    assert_eq!(read["title"], "Over GraphQL", "{read}");

    let mut left = Arc::try_unwrap(runtime(
        "github-graphql",
        PULLS_SDL,
        vec![],
        Arc::clone(&state),
    ))
    .ok()
    .unwrap();
    left.shares = None;
    types.join(&left);
    gql(&gql_router, &create("After leaving"), true).await;
    let read = rest(
        &rest_router,
        "GET",
        "/repos/fernwood/trellis/pulls/7008",
        None,
    )
    .await;
    assert_ne!(read["title"], "After leaving", "{read}");
}

/// A `number` is unique only under its repository: each repository's read finds
/// its own pull request #5 (the one whose `repository` names it), not the first one
/// numbered 5. (A read is scoped by the path parameter nearest its target, here the
/// repository's name, so two repositories of one name under two owners are not told
/// apart.)
#[tokio::test]
async fn each_owner_reads_its_own_record_of_a_shared_number() {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let types = SharedTypes::default();
    let sdl = PULLS_SDL
        .replace(
            "type PullRequest { id: ID! number: Int!",
            "type Repository { id: ID! name: String! }\ntype PullRequest { id: ID! repository: Repository number: Int!",
        );
    let rest_contract = PULLS.replace(
        "        number: {type: integer}\n",
        "        number: {type: integer}\n        repository: {type: object, properties: {name: {type: string}}}\n",
    );
    let in_repo = |repo: &str| {
        json!({"__typename": "PullRequest", "id": format!("PR_{repo}5"), "number": 5,
               "title": format!("In {repo}"), "state": "OPEN",
               "repository": {"__typename": "Repository", "id": format!("R_{repo}"), "name": repo}})
    };
    let gh = joined(
        ("github-graphql", &sdl),
        vec![in_repo("one"), in_repo("two")],
        &state,
        &types,
        None,
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let rest_rt = joined(
        ("github-rest", &rest_contract),
        vec![],
        &state,
        &types,
        None,
    );
    let rest_router = http_rest::router(rest_rt);
    for repo in ["one", "two"] {
        gql(
            &gql_router,
            &format!(r#"mutation {{ createPullRequest(title: "In {repo}") {{ id number }} }}"#),
            true,
        )
        .await;
    }
    for (owner, repo) in [("b", "two"), ("a", "one")] {
        let read = rest(
            &rest_router,
            "GET",
            &format!("/repos/{owner}/{repo}/pulls/5"),
            None,
        )
        .await;
        assert_eq!(read["title"], format!("In {repo}"), "{read}");
    }
}

/// `same_as` on one side only: the GraphQL world names REST's type, the REST mock
/// has no world, and its read still finds the GraphQL record (inferred there).
#[tokio::test]
async fn same_as_on_one_side_still_links_both() {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let types = SharedTypes::default();
    let world = serde_json::from_value::<mb_mcm::world::World>(
        json!({"version": 1, "types": {"pullrequest": {"same_as": ["pulls"]}}}),
    )
    .unwrap();
    let gh = joined(
        ("github-graphql", PULLS_SDL),
        vec![
            pull_request(7007, "Over GraphQL"),
            pull_request(99, "Made up"),
        ],
        &state,
        &types,
        Some(&world),
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let rest_rt = joined(
        ("github-rest", PULLS),
        vec![
            json!({"id": 8, "node_id": "PR_8", "number": 8, "title": "Over REST", "state": "open"}),
        ],
        &state,
        &types,
        None,
    );
    let rest_router = http_rest::router(rest_rt);
    gql(
        &gql_router,
        r#"mutation { createPullRequest(title: "Over GraphQL") { id number } }"#,
        true,
    )
    .await;
    let read = rest(
        &rest_router,
        "GET",
        "/repos/fernwood/trellis/pulls/7007",
        None,
    )
    .await;
    assert_eq!(read["title"], "Over GraphQL", "{read}");
    let made = rest(
        &rest_router,
        "POST",
        "/repos/fernwood/trellis/pulls",
        Some(json!({"title": "Over REST"})),
    )
    .await;
    let number = made["number"].as_u64().unwrap();
    let back = gql(
        &gql_router,
        &format!("{{ pullRequestByNumber(number: {number}) {{ title }} }}"),
        false,
    )
    .await;
    assert_eq!(back["pullRequestByNumber"]["title"], "Over REST", "{back}");
}
