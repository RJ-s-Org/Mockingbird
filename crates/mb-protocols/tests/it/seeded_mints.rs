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
/// its own earlier ones, and repeat in a fresh runtime whatever the other sessions did
/// in between.
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
    // The other order of sessions in a fresh runtime: each session's ids repeat.
    assert_eq!(things(&b, "s2", 5).await, two);
    assert_eq!(things(&b, "s1", 5).await, one);
    assert_eq!(order(&b, "s1").await, first);
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
