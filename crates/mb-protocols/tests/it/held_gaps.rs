//! A held record lacking members its read requires is completed from state, its
//! stored values kept, rather than generated whole: a required member that may be
//! `null` is `null`, any other is filled locally, and what was filled is stored.
#![cfg(feature = "http-rest")]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};
use tower::ServiceExt;

const ISSUES: &str = r##"
openapi: 3.0.3
info: {title: issues, version: "1"}
paths:
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
      required: [number, title, assignee, closed_at, comments]
      properties:
        number: {type: integer}
        title: {type: string}
        assignee:
          nullable: true
          type: object
          required: [login]
          properties: {login: {type: string}}
        closed_at: {type: string, format: date-time, nullable: true}
        comments: {type: integer, minimum: 0}
"##;

/// A runtime of `contract` named `name` on `state`, its provider answering `script`
/// (an empty one fails every call: nothing is generated then).
fn runtime(
    name: &str,
    contract: &str,
    script: Vec<Value>,
    state: Arc<dyn StateStore>,
    shares: bool,
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
    if shares {
        rt.shares = Some("gh".into());
    }
    Arc::new(rt)
}

/// The answer to a GET of `uri`, and its `x-mockingbird-source`.
async fn get(router: &axum::Router, uri: &str) -> (String, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-mockingbird-session", "s1")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let source = resp
        .headers()
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (source, serde_json::from_slice(&bytes).unwrap())
}

fn session(mock: &str) -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: mock.into(),
        session: "s1".into(),
    }
}

/// A held issue stored without `assignee`, `closed_at` (both nullable) and `comments`
/// (an integer) is read from state: the nullable ones `null`, `comments` filled
/// locally, the stored title kept, and nothing generated. What was filled is stored,
/// so a second read answers the same.
#[tokio::test]
async fn a_held_record_lacking_required_members_is_completed_from_state() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store
        .upsert(
            &session("issues"),
            StoredResource {
                resource_type: "issues".into(),
                id: "7".into(),
                data: json!({"number": 7, "title": "Stored title"}),
            },
        )
        .await
        .unwrap();
    let router = http_rest::router(runtime("issues", ISSUES, vec![], Arc::clone(&store), false));

    let (source, read) = get(&router, "/issues/7").await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["number"], 7, "{read}");
    assert_eq!(read["title"], "Stored title", "{read}");
    assert_eq!(read["assignee"], Value::Null, "{read}");
    assert_eq!(read["closed_at"], Value::Null, "{read}");
    let comments = read["comments"].as_u64();
    assert!(comments.is_some(), "{read}");

    let held = store.get(&session("issues"), "issues", "7").await.unwrap();
    let held = held.unwrap().data;
    assert_eq!(held["title"], "Stored title", "{held}");
    assert_eq!(held["comments"].as_u64(), comments, "{held}");
    assert_eq!(held["assignee"], Value::Null, "{held}");

    let (source, again) = get(&router, "/issues/7").await;
    assert_eq!(source, "state", "{again}");
    assert_eq!(again, read);
}

/// What a held object lacks is completed below the top too, its stored members kept.
#[tokio::test]
async fn a_held_object_lacking_required_members_is_completed_in_place() {
    let contract = ISSUES.replace(
        "properties: {login: {type: string}}",
        "properties: {login: {type: string}, id: {type: integer}, site_admin: {type: boolean}}\n          required: [login, id, site_admin]",
    );
    let contract = contract.replace("          required: [login]\n", "");
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store
        .upsert(
            &session("issues"),
            StoredResource {
                resource_type: "issues".into(),
                id: "7".into(),
                data: json!({"number": 7, "title": "Stored title", "comments": 2,
                             "closed_at": null, "assignee": {"login": "mira"}}),
            },
        )
        .await
        .unwrap();
    let router = http_rest::router(runtime(
        "issues",
        &contract,
        vec![],
        Arc::clone(&store),
        false,
    ));

    let (source, read) = get(&router, "/issues/7").await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["assignee"]["login"], "mira", "{read}");
    assert!(read["assignee"]["id"].is_i64(), "{read}");
    assert!(read["assignee"]["site_admin"].is_boolean(), "{read}");
    assert_eq!(read["comments"], 2, "{read}");
}

/// What each item of a held list lacks is completed in place, item by item, the
/// stored members of each kept.
#[tokio::test]
async fn held_list_items_lacking_required_members_are_completed_item_by_item() {
    let contract = ISSUES
        .replace(
            "required: [number, title, assignee, closed_at, comments]",
            "required: [number, title, labels]",
        )
        .replace(
            "        comments: {type: integer, minimum: 0}\n",
            "        comments: {type: integer, minimum: 0}\n        labels:\n          type: array\n          items:\n            type: object\n            required: [name, id, description]\n            properties:\n              name: {type: string}\n              id: {type: integer}\n              description: {type: string, nullable: true}\n",
        );
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store
        .upsert(
            &session("issues"),
            StoredResource {
                resource_type: "issues".into(),
                id: "7".into(),
                data: json!({"number": 7, "title": "Stored title",
                             "labels": [{"name": "bug"}, {"name": "ui"}]}),
            },
        )
        .await
        .unwrap();
    let router = http_rest::router(runtime(
        "issues",
        &contract,
        vec![],
        Arc::clone(&store),
        false,
    ));

    let (source, read) = get(&router, "/issues/7").await;
    assert_eq!(source, "state", "{read}");
    let labels = read["labels"].as_array().cloned().unwrap_or_default();
    assert_eq!(labels.len(), 2, "{read}");
    for (label, name) in labels.iter().zip(["bug", "ui"]) {
        assert_eq!(label["name"], name, "{read}");
        assert_eq!(label["description"], Value::Null, "{read}");
        assert!(label["id"].is_i64(), "{read}");
    }
}

/// A missing member of free text is the model's, asked for that member alone; the
/// fill's tokens are the read's, and what it wrote is stored, so a second read asks
/// nothing.
#[tokio::test]
async fn missing_free_text_is_the_model_s_asked_for_it_alone() {
    let contract = ISSUES
        .replace(
            "required: [number, title, assignee, closed_at, comments]",
            "required: [number, title, body, comments]",
        )
        .replace(
            "        title: {type: string}\n",
            "        title: {type: string}\n        body: {type: string}\n",
        );
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store
        .upsert(
            &session("issues"),
            StoredResource {
                resource_type: "issues".into(),
                id: "7".into(),
                data: json!({"number": 7, "title": "Stored title"}),
            },
        )
        .await
        .unwrap();
    let usage = mb_llm::Usage {
        prompt_tokens: 11,
        completion_tokens: 5,
        reasoning_tokens: 0,
    };
    let provider = Arc::new(ScriptedProvider::with_usage(vec![Ok((
        json!({"body": "Scripted body"}).to_string(),
        usage,
    ))]));
    let mut rt = MockRuntime::new(
        "issues",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store),
    );
    let recorded = Arc::new(crate::support::Records::default());
    rt.recorder = Some(Arc::clone(&recorded) as Arc<dyn mb_protocols::RequestRecorder>);
    let router = http_rest::router(Arc::new(rt));

    let (source, read) = get(&router, "/issues/7").await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["title"], "Stored title", "{read}");
    assert_eq!(read["body"], "Scripted body", "{read}");
    assert!(read["comments"].is_u64(), "{read}");
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let mb_llm::ResponseFormat::JsonSchema { schema, .. } = &requests[0].response_format else {
        panic!(
            "not a structured request: {:?}",
            requests[0].response_format
        );
    };
    let asked = schema.to_string();
    assert!(asked.contains("\"body\""), "{asked}");
    assert!(!asked.contains("\"title\""), "{asked}");
    assert!(!asked.contains("\"comments\""), "{asked}");
    let records = recorded.0.lock().unwrap().clone();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].prompt_tokens, 11, "{records:?}");

    let (source, again) = get(&router, "/issues/7").await;
    assert_eq!(source, "state", "{again}");
    assert_eq!(again, read);
    assert_eq!(provider.requests().len(), 1);
}

#[cfg(feature = "graphql")]
const PULLS: &str = r##"
openapi: 3.0.3
info: {title: pulls, version: "1"}
paths:
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
          content: {application/json: {schema: {$ref: "#/components/schemas/PullRequest"}}}
        "404": {description: missing}
components:
  schemas:
    PullRequest:
      type: object
      required: [number, title, head, base, assignee, additions]
      properties:
        number: {type: integer}
        title: {type: string}
        additions: {type: integer, minimum: 0}
        assignee:
          nullable: true
          type: object
          properties: {login: {type: string}}
        head:
          type: object
          required: [ref, sha]
          properties: {ref: {type: string}, sha: {type: string, pattern: "^[0-9a-f]{40}$"}}
        base:
          type: object
          required: [ref, sha]
          properties: {ref: {type: string}, sha: {type: string, pattern: "^[0-9a-f]{40}$"}}
"##;

#[cfg(feature = "graphql")]
const PULLS_GQL: &str = r#"
type Repository { id: ID! name: String! }
type PullRequest { id: ID! number: Int! title: String! headRefName: String! baseRefName: String! repository: Repository! }
input CreatePullRequestInput { repositoryId: ID! title: String! headRefName: String! baseRefName: String! }
type CreatePullRequestPayload { pullRequest: PullRequest }
type Query { repository(owner: String!, name: String!): Repository }
type Mutation { createPullRequest(input: CreatePullRequestInput!): CreatePullRequestPayload }
"#;

/// A pull request created over GraphQL and read over REST (a `shares` group) answers
/// from state: its head and base branches are the stored `headRefName` and
/// `baseRefName` at REST's `head.ref` and `base.ref`, what REST requires beside them
/// filled locally, with nothing generated. The record stays the GraphQL mock's: no
/// REST copy is stored beside it.
#[cfg(feature = "graphql")]
#[tokio::test]
async fn a_pull_created_over_graphql_reads_over_rest_with_its_stored_branches() {
    use mb_protocols::graphql;

    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let gh = runtime(
        "github-graphql",
        PULLS_GQL,
        vec![
            json!({"__typename": "CreatePullRequestPayload", "pullRequest": {
            "__typename": "PullRequest", "id": "PR_1", "number": 7007,
            "title": "Fix panic on empty path", "headRefName": "fix-empty-path",
            "baseRefName": "main",
            "repository": {"__typename": "Repository", "id": "R_1"}}}),
        ],
        Arc::clone(&state),
        true,
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let world = serde_json::from_value::<mb_mcm::world::World>(json!({"version": 1,
        "types": {"pulls": {"identity": {"member": "number"}, "same_as": ["pullrequest"]}}}))
    .unwrap();
    let rest_rt = runtime("github-rest", PULLS, vec![], Arc::clone(&state), true);
    let rest_rt = Arc::try_unwrap(rest_rt)
        .ok()
        .unwrap()
        .with_world(&world, &world, 1);
    let rest_router = http_rest::router(Arc::new(rest_rt));

    let req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1")
        .header("x-mockingbird-seed", "1")
        .body(Body::from(
            json!({"query": r#"mutation { createPullRequest(input: {repositoryId: "R_1", title: "Fix panic on empty path", headRefName: "fix-empty-path", baseRefName: "main"}) { pullRequest { id number headRefName } } }"#})
                .to_string(),
        ))
        .unwrap();
    let resp = gql_router.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let created: Value = serde_json::from_slice(&bytes).unwrap();
    let number = created["data"]["createPullRequest"]["pullRequest"]["number"].clone();
    assert!(number.is_u64(), "{created}");
    let stored_before = state.list(&session("gh"), "pullrequest", 10).await.unwrap();
    assert_eq!(stored_before.len(), 1, "{stored_before:?}");

    let (source, read) = get(
        &rest_router,
        &format!("/repos/fernwood/trellis/pulls/{number}"),
    )
    .await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["number"], number, "{read}");
    assert_eq!(read["title"], "Fix panic on empty path", "{read}");
    assert_eq!(read["head"]["ref"], "fix-empty-path", "{read}");
    assert_eq!(read["base"]["ref"], "main", "{read}");
    assert_eq!(read["assignee"], Value::Null, "{read}");
    assert!(read["additions"].is_u64(), "{read}");
    assert!(
        read["head"]["sha"].as_str().is_some_and(|s| s.len() == 40),
        "{read}"
    );
    let stored_after = state.list(&session("gh"), "pullrequest", 10).await.unwrap();
    assert_eq!(stored_after, stored_before);
    let copies = state.list(&session("gh"), "pulls", 10).await.unwrap();
    assert!(copies.is_empty(), "{copies:?}");
}

/// A GraphQL mutation `query` sent to `router`, its `data`.
#[cfg(feature = "graphql")]
async fn gql(router: &axum::Router, query: &str) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1")
        .header("x-mockingbird-seed", "1")
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

/// Another mock's record, completed over REST with model-written free text, answers
/// alike on every read with one model call, and no copy of it is stored. Once the
/// GraphQL mock changes it, what it now holds wins and what the completion wrote
/// before fills what it still lacks: nothing is filled anew.
#[cfg(feature = "graphql")]
#[tokio::test]
async fn another_mock_s_record_completes_alike_on_every_read() {
    use mb_protocols::graphql;

    let rest = PULLS
        .replace(
            "required: [number, title, head, base, assignee, additions]",
            "required: [number, title, body, head, base, assignee, additions]",
        )
        .replace(
            "        title: {type: string}\n",
            "        title: {type: string}\n        body: {type: string}\n",
        );
    let sdl = PULLS_GQL
        .replace(
            "title: String! headRefName",
            "title: String! body: String headRefName",
        )
        .replace(
            "type Mutation {",
            "input UpdatePullRequestInput { pullRequestId: ID! body: String }\ntype UpdatePullRequestPayload { pullRequest: PullRequest }\ntype Mutation { updatePullRequest(input: UpdatePullRequestInput!): UpdatePullRequestPayload",
        );
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let pull = json!({"__typename": "PullRequest", "id": "PR_1", "number": 7007,
        "title": "Fix panic on empty path", "headRefName": "fix-empty-path",
        "baseRefName": "main", "repository": {"__typename": "Repository", "id": "R_1"}});
    let mut updated = pull.clone();
    updated["body"] = json!("Set over GraphQL");
    let gh = runtime(
        "github-graphql",
        &sdl,
        vec![
            json!({"__typename": "CreatePullRequestPayload", "pullRequest": pull}),
            json!({"__typename": "UpdatePullRequestPayload", "pullRequest": updated}),
        ],
        Arc::clone(&state),
        true,
    );
    let gql_router = graphql::try_router(gh).unwrap();
    let world = serde_json::from_value::<mb_mcm::world::World>(json!({"version": 1,
        "types": {"pulls": {"identity": {"member": "number"}, "same_as": ["pullrequest"]}}}))
    .unwrap();
    let usage = mb_llm::Usage {
        prompt_tokens: 9,
        completion_tokens: 4,
        reasoning_tokens: 0,
    };
    let provider = Arc::new(ScriptedProvider::with_usage(vec![Ok((
        json!({"body": "Written once"}).to_string(),
        usage,
    ))]));
    let mut rest_rt = MockRuntime::new(
        "github-rest",
        Arc::new(compile(rest.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&state),
    );
    rest_rt.shares = Some("gh".into());
    let rest_router = http_rest::router(Arc::new(rest_rt.with_world(&world, &world, 1)));

    let created = gql(
        &gql_router,
        r#"mutation { createPullRequest(input: {repositoryId: "R_1", title: "Fix panic on empty path", headRefName: "fix-empty-path", baseRefName: "main"}) { pullRequest { id number } } }"#,
    )
    .await;
    let number = created["createPullRequest"]["pullRequest"]["number"].clone();
    let id = created["createPullRequest"]["pullRequest"]["id"].clone();
    let uri = format!("/repos/fernwood/trellis/pulls/{number}");

    let (source, first) = get(&rest_router, &uri).await;
    assert_eq!(source, "state", "{first}");
    assert_eq!(first["body"], "Written once", "{first}");
    assert_eq!(first["head"]["ref"], "fix-empty-path", "{first}");
    let (source, second) = get(&rest_router, &uri).await;
    assert_eq!(source, "state", "{second}");
    assert_eq!(second, first);
    assert_eq!(provider.requests().len(), 1);
    let copies = state.list(&session("gh"), "pulls", 10).await.unwrap();
    assert!(copies.is_empty(), "{copies:?}");

    gql(
        &gql_router,
        &format!(
            r#"mutation {{ updatePullRequest(input: {{pullRequestId: {id}, body: "Set over GraphQL"}}) {{ pullRequest {{ id body }} }} }}"#
        ),
    )
    .await;
    let (source, third) = get(&rest_router, &uri).await;
    assert_eq!(source, "state", "{third}");
    assert_eq!(third["body"], "Set over GraphQL", "{third}");
    assert_eq!(third["head"], first["head"], "{third}");
    assert_eq!(third["additions"], first["additions"], "{third}");
    assert_eq!(provider.requests().len(), 1);
}

/// An optional embedded record and an optional list of objects the held record holds
/// only part of (a GraphQL create's `repository: {id}`) are completed below the top,
/// with no model too; a list of scalars stays as held.
#[tokio::test]
async fn an_optional_embedded_record_held_in_part_is_completed_offline() {
    let contract = ISSUES.replace(
        "        comments: {type: integer, minimum: 0}\n",
        "        comments: {type: integer, minimum: 0}\n        repository: {$ref: \"#/components/schemas/Repository\"}\n        labels: {type: array, items: {$ref: \"#/components/schemas/Label\"}}\n        topics: {type: array, items: {type: string}}\n    Repository:\n      type: object\n      required: [id, name, archive_url]\n      properties:\n        id: {type: integer}\n        name: {type: string}\n        archive_url: {type: string}\n    Label:\n      type: object\n      required: [name, color]\n      properties:\n        name: {type: string}\n        color: {type: string}\n",
    );
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store
        .upsert(
            &session("issues"),
            StoredResource {
                resource_type: "issues".into(),
                id: "7".into(),
                data: json!({"number": 7, "title": "Stored title", "repository": {"id": 5},
                             "labels": [{"name": "bug"}], "topics": ["router"]}),
            },
        )
        .await
        .unwrap();
    let rt = MockRuntime::new(
        "issues",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&store),
    );
    let router = http_rest::router(Arc::new(rt));

    let (source, read) = get(&router, "/issues/7").await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["title"], "Stored title", "{read}");
    assert_eq!(read["repository"]["id"], 5, "{read}");
    assert!(read["repository"]["archive_url"].is_string(), "{read}");
    assert_eq!(read["labels"][0]["name"], "bug", "{read}");
    assert!(read["labels"][0]["color"].is_string(), "{read}");
    assert_eq!(read["topics"], json!(["router"]), "{read}");
}

/// A list and a read by id of items whose answers require more than a seed holds.
const ITEMS: &str = r##"
openapi: 3.0.0
info: {title: items, version: "1"}
paths:
  /items:
    get:
      operationId: listItems
      parameters: [{name: title, in: query, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Item"}}}}
    post:
      operationId: createItem
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /items/{id}:
    get:
      operationId: getItem
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
components:
  schemas:
    Item:
      type: object
      required: [id, title, comments, owner]
      properties:
        id: {type: string}
        title: {type: string}
        comments: {type: integer, minimum: 0}
        owner: {type: object, nullable: true, required: [login], properties: {login: {type: string}}}
"##;

/// Held records lacking what a list's items require are completed for the list, as for
/// a read of one, with no model too: the stored members kept, the page from state, and
/// a later read of one answers what the list completed.
#[tokio::test]
async fn listed_records_lacking_required_members_are_completed() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    for (id, title) in [("i7", "Seeded seven"), ("i8", "Seeded eight")] {
        store
            .upsert(
                &session("items"),
                StoredResource {
                    resource_type: "items".into(),
                    id: id.into(),
                    data: json!({"id": id, "title": title}),
                },
            )
            .await
            .unwrap();
    }
    let rt = MockRuntime::new(
        "items",
        Arc::new(compile(ITEMS.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&store),
    );
    // As a server serves it: over its tier-0 world.
    let rt = crate::support::with_effective(rt, json!({}), false);
    let router = http_rest::router(Arc::new(rt));

    let (source, list) = get(&router, "/items").await;
    assert_eq!(source, "state", "{list}");
    let items = list.as_array().cloned().unwrap_or_default();
    let mut titles: Vec<&str> = items.iter().filter_map(|i| i["title"].as_str()).collect();
    titles.sort_unstable();
    assert_eq!(titles, ["Seeded eight", "Seeded seven"], "{list}");
    for item in &items {
        assert!(item["comments"].is_u64(), "{list}");
        assert_eq!(item["owner"], Value::Null, "{list}");
    }
    let (source, one) = get(&router, "/items/i7").await;
    assert_eq!(source, "state", "{one}");
    let listed = items.iter().find(|i| i["id"] == "i7").cloned();
    assert_eq!(Some(one), listed);
}

/// A record the list's query drops is not completed: nothing is filled or stored for it.
#[tokio::test]
async fn a_record_the_list_drops_is_not_completed() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    for (id, title) in [("i7", "Seeded seven"), ("i8", "Seeded eight")] {
        store
            .upsert(
                &session("items"),
                StoredResource {
                    resource_type: "items".into(),
                    id: id.into(),
                    data: json!({"id": id, "title": title}),
                },
            )
            .await
            .unwrap();
    }
    let rt = MockRuntime::new(
        "items",
        Arc::new(compile(ITEMS.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&store),
    );
    let rt = crate::support::with_effective(rt, json!({}), false);
    let router = http_rest::router(Arc::new(rt));

    let (source, list) = get(&router, "/items?title=Seeded%20seven").await;
    assert_eq!(source, "state", "{list}");
    let ids: Vec<&str> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["id"].as_str())
        .collect();
    assert_eq!(ids, ["i7"], "{list}");
    let dropped = store.get(&session("items"), "items", "i8").await.unwrap();
    assert_eq!(
        dropped.map(|r| r.data),
        Some(json!({"id": "i8", "title": "Seeded eight"}))
    );
}
