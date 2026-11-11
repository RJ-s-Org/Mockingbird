//! The ids a mock mints are drawn from its faker seed: the same seed and the same calls
//! in a fresh runtime mint the same ids, while successive mints in a session and the
//! mints of another session differ, and every id a session is given is its own.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_protocols::{MockRuntime, http_rest};
use mb_state::MemoryStore;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Things answer their id only in a header, in a short pattern; orders answer a record
/// with no id, whose lines hold ids of their own.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Shop, version: "1"}
paths:
  /things:
    post:
      operationId: createThing
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201":
          description: created
          headers:
            Thing-Id: {schema: {type: string}}
  /things/{thingId}:
    get:
      operationId: getThing
      parameters: [{name: thingId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}
  /orders:
    post:
      operationId: createOrder
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewOrder"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}
  /orders/{orderId}:
    get:
      operationId: getOrder
      parameters: [{name: orderId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}
components:
  schemas:
    Thing:
      type: object
      properties:
        id: {type: string, pattern: "^[a-z]{3}$"}
        name: {type: string}
    NewOrder:
      type: object
      properties:
        note: {type: string}
        lines:
          type: array
          items: {type: object, properties: {sku: {type: string}}}
    Order:
      type: object
      required: [id]
      properties:
        id: {type: string, format: uuid}
        note: {type: string}
        lines:
          type: array
          items:
            type: object
            properties:
              id: {type: string, format: uuid}
              sku: {type: string}
"##;

/// A fresh runtime of [`CONTRACT`] with no model, its faker seeded by `seed`.
fn router(seed: u64) -> axum::Router {
    let rt = MockRuntime::new(
        "shop",
        Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default()).with_faker_seed(seed)),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    http_rest::router(Arc::new(rt))
}

async fn post(router: &axum::Router, session: &str, uri: &str, body: Value) -> (String, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-mockingbird-session", session)
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "POST {uri}");
    let id = resp
        .headers()
        .get("thing-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (id, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The ids `session` is minted for `n` things, in order.
async fn things(router: &axum::Router, session: &str, n: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let (id, _) = post(router, session, "/things", json!({"name": format!("t{i}")})).await;
        assert!(!id.is_empty(), "thing {i} was minted no id");
        ids.push(id);
    }
    ids
}

/// The ids an order and its lines are minted.
async fn order(router: &axum::Router, session: &str) -> Value {
    let (_, body) = post(
        router,
        session,
        "/orders",
        json!({"note": "n", "lines": [{"sku": "a"}, {"sku": "b"}]}),
    )
    .await;
    let lines: Vec<Value> = body["lines"]
        .as_array()
        .map(|l| l.iter().map(|line| line["id"].clone()).collect())
        .unwrap_or_default();
    assert!(body["id"].is_string(), "{body}");
    json!({"id": body["id"], "lines": lines})
}

#[tokio::test]
async fn the_same_seed_and_calls_mint_the_same_ids_in_a_fresh_runtime() {
    let (a, b) = (router(1), router(1));
    let first = (things(&a, "s1", 5).await, order(&a, "s1").await);
    let again = (things(&b, "s1", 5).await, order(&b, "s1").await);
    assert_eq!(first, again);
    // Another seed mints others.
    let other = router(2);
    assert_ne!(things(&other, "s1", 5).await, first.0);
}

/// Each session counts its own mints: its ids differ from another session's and from
/// its own earlier ones, and repeat in a fresh runtime for the same calls whatever the
/// sessions are named (a client's session names often hold the time).
#[tokio::test]
async fn successive_mints_and_other_sessions_differ() {
    let (a, b) = (router(1), router(1));
    let one = things(&a, "s1", 5).await;
    let two = things(&a, "s2", 5).await;
    assert_ne!(one, two);
    let first = order(&a, "s1").await;
    let second = order(&a, "s1").await;
    assert_ne!(first["id"], second["id"]);
    assert_ne!(first["lines"][0], first["lines"][1]);
    // The same calls in a fresh runtime, in sessions named otherwise: the same ids.
    assert_eq!(things(&b, "run2-s1", 5).await, one);
    assert_eq!(things(&b, "run2-s2", 5).await, two);
    assert_eq!(order(&b, "run2-s1").await, first);
}

#[tokio::test]
async fn a_thousand_short_ids_stay_unique_and_repeat_with_the_seed() {
    let (a, b) = (router(1), router(1));
    let ids = things(&a, "s1", 1000).await;
    let mut held = ids.clone();
    held.sort();
    held.dedup();
    assert_eq!(held.len(), 1000, "a session was minted one id twice");
    assert!(ids.iter().all(|id| id.len() == 3), "{ids:?}");
    assert_eq!(things(&b, "s1", 1000).await, ids);
}

/// The ids `session` is minted for `n` things named after `prefix`, in order.
async fn things_named(router: &axum::Router, session: &str, prefix: &str, n: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let name = format!("{prefix}{i}");
        let (id, _) = post(router, session, "/things", json!({ "name": name })).await;
        assert!(!id.is_empty(), "thing {name} was minted no id");
        ids.push(id);
    }
    ids
}

/// A session is known by the call it first minted in, not by when it started: two
/// sessions that start in the other order in a fresh runtime (as concurrent clients
/// do) mint the same ids each.
#[tokio::test]
async fn sessions_starting_in_either_order_mint_the_same_ids() {
    let (a, b) = (router(1), router(1));
    let first_a = things_named(&a, "run1-a", "a", 3).await;
    let first_b = things_named(&a, "run1-b", "b", 3).await;
    let then_b = things_named(&b, "run2-b", "b", 3).await;
    let then_a = things_named(&b, "run2-a", "a", 3).await;
    assert_eq!(then_a, first_a);
    assert_eq!(then_b, first_b);
    assert_ne!(first_a, first_b);
}

/// Charges answer their id only in a header; their records show its shape in an example.
const CHARGES: &str = r##"
openapi: 3.1.0
info: {title: Charges, version: "1"}
paths:
  /charges:
    post:
      operationId: createCharge
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201":
          description: created
          headers:
            Charge-Id: {schema: {type: string}}
  /charges/{charge}:
    get:
      operationId: getCharge
      parameters: [{name: charge, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Charge"}}}}
components:
  schemas:
    Charge:
      type: object
      properties:
        id: {type: string, examples: [ch_3MtwBwLkdIwHu7ix28a3tqPa]}
        name: {type: string}
"##;

/// The ids `n` charges are minted in `session` of a fresh runtime seeded by `seed`.
async fn charges(seed: u64, session: &str, n: usize) -> Vec<String> {
    let rt = MockRuntime::new(
        "charges",
        Arc::new(compile(CHARGES.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default()).with_faker_seed(seed)),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let router = http_rest::router(Arc::new(rt));
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let req = Request::builder()
            .method("POST")
            .uri("/charges")
            .header("content-type", "application/json")
            .header("x-mockingbird-session", session)
            .body(Body::from(json!({"name": format!("c{i}")}).to_string()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let id = resp
            .headers()
            .get("charge-id")
            .and_then(|v| v.to_str().ok());
        ids.push(id.unwrap_or_default().to_string());
    }
    ids
}

/// An id minted in the shape the contract's example shows stays seeded and unique.
#[tokio::test]
async fn ids_minted_in_an_example_s_shape_repeat_with_the_seed_and_stay_unique() {
    let ids = charges(1, "s1", 200).await;
    assert!(
        ids.iter()
            .all(|id| id.strip_prefix("ch_").is_some_and(|rest| {
                rest.len() == 24 && rest.bytes().all(|b| b.is_ascii_alphanumeric())
            })),
        "{ids:?}"
    );
    let mut held = ids.clone();
    held.sort();
    held.dedup();
    assert_eq!(held.len(), ids.len(), "a session was minted one id twice");
    assert_eq!(charges(1, "run2-s1", 200).await, ids);
    assert_ne!(charges(2, "s1", 200).await, ids);
}

/// Users paged by a token the mock mints for the rest of a page cut short.
const PAGES: &str = r##"
openapi: 3.1.0
info: {title: Pages, version: "1"}
paths:
  /users:
    get:
      operationId: listUsers
      parameters:
        - {name: page_size, in: query, schema: {type: integer}}
        - {name: next_page_token, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [users]
                properties:
                  users: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next_page_token: {type: string}
components:
  schemas:
    User:
      type: object
      required: [id]
      properties: {id: {type: string}, name: {type: string}}
"##;

/// The tokens of a list of five users read two at a time in `session`, seeded by the
/// call, in a fresh runtime whose model answers the list once.
async fn page_tokens(session: &str) -> Vec<String> {
    use mb_llm::{Provider, ScriptedProvider};
    let users: Vec<Value> = (1..=5)
        .map(|i| json!({"id": format!("u{i}"), "name": format!("N{i}")}))
        .collect();
    let answer = json!({"users": users, "next_page_token": "model-token"});
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer.to_string())]));
    let rt = MockRuntime::new(
        "pages",
        Arc::new(compile(PAGES.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let router = http_rest::router(Arc::new(rt));
    let mut tokens = Vec::new();
    let mut uri = "/users?page_size=2".to_string();
    for _ in 0..2 {
        let req = Request::builder()
            .uri(&uri)
            .header("x-mockingbird-session", session)
            .header("x-mockingbird-seed", "7")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let token = body["next_page_token"].as_str().unwrap_or_default();
        assert!(!token.is_empty() && token != "model-token", "{body}");
        tokens.push(token.to_string());
        uri = format!("/users?page_size=2&next_page_token={token}");
    }
    tokens
}

/// A page token the mock mints is drawn from the seed too: the next page's request,
/// whose query seeds its answer, is the same in a fresh runtime.
#[tokio::test]
async fn a_seeded_page_token_repeats_in_a_fresh_runtime() {
    let first = page_tokens("run1-s").await;
    assert_ne!(first[0], first[1]);
    assert_eq!(page_tokens("run2-s").await, first);
}

/// Folders whose `etag` is a described concurrency token.
const FOLDERS: &str = r##"
openapi: 3.0.0
info: {title: Folders, version: "1"}
paths:
  /folders:
    post:
      operationId: createFolder
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
  /folders/{folderId}:
    parameters: [{name: folderId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getFolder
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
    patch:
      operationId: updateFolder
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
        "412": {description: stale}
components:
  schemas:
    Folder:
      type: object
      required: [id]
      properties:
        id: {type: string}
        name: {type: string}
        etag: {type: string, readOnly: true}
"##;

/// The first `etag` of each of three folders created in `session`, in a fresh runtime
/// seeded by `seed` whose world describes the token.
async fn first_etags(seed: u64, session: &str) -> Vec<Value> {
    let rt = MockRuntime::new(
        "folders",
        Arc::new(compile(FOLDERS.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default()).with_faker_seed(seed)),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let world = json!({"types": {"folders": {"concurrency": {"member": "etag", "header": "If-Match", "conflict": 412}}}});
    let router = http_rest::router(Arc::new(crate::support::with_effective(rt, world, false)));
    let mut etags = Vec::new();
    for i in 0..3 {
        let (_, body) = post(
            &router,
            session,
            "/folders",
            json!({"name": format!("f{i}")}),
        )
        .await;
        assert!(body["etag"].is_string(), "{body}");
        etags.push(body["etag"].clone());
    }
    etags
}

#[tokio::test]
async fn a_first_token_repeats_with_the_seed() {
    let first = first_etags(1, "run1").await;
    assert_ne!(first[0], first[1]);
    assert_eq!(first_etags(1, "run2").await, first);
}
