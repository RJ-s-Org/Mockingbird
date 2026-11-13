//! An action the description sets nothing for moves its target's state as its verb
//! says, when no model answers it.
#![cfg(feature = "http-rest")]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::MemoryStore;
use serde_json::{Value, json};
use tower::ServiceExt;

const ORDERS: &str = r##"
openapi: 3.0.0
info: {title: orders, version: "1"}
paths:
  /orders:
    post:
      operationId: createOrder
      requestBody: {content: {application/json: {schema: {type: object, properties: {item: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{id}:
    get:
      operationId: getOrder
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{id}/cancel:
    post:
      operationId: cancelOrder
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{id}/review:
    post:
      operationId: reviewOrder
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
components:
  schemas:
    Order:
      type: object
      required: [id, item, status]
      properties:
        id: {type: string}
        item: {type: string}
        status: {type: string, enum: [pending, canceled]}
"##;

/// A router of [`ORDERS`] over its tier-0 world and `world`, its provider answering
/// `script` when given, else none (offline).
fn router(world: Value, script: Option<Vec<Value>>) -> axum::Router {
    router_of(ORDERS, world, script)
}

/// [`router`] of `spec`.
fn router_of(spec: &str, world: Value, script: Option<Vec<Value>>) -> axum::Router {
    let provider = script.map(|script| {
        Arc::new(ScriptedProvider::new(
            script.into_iter().map(|v| Ok(v.to_string())).collect(),
        )) as Arc<dyn Provider>
    });
    let rt = MockRuntime::new(
        "orders",
        Arc::new(compile(spec.as_bytes()).unwrap()),
        Arc::new(Engine::new(provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let rt = crate::support::with_effective(rt, world, false);
    http_rest::router(Arc::new(rt))
}

/// The answer to `method` `uri` with `body`, and its `x-mockingbird-source`.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (String, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-mockingbird-session", "s1")
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
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
    (
        source,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The id of an order created on `router`, with its status.
async fn created(router: &axum::Router) -> (String, Value) {
    let (_, order) = call(router, "POST", "/orders", Some(json!({"item": "book"}))).await;
    (
        order["id"].as_str().unwrap_or_default().to_owned(),
        order["status"].clone(),
    )
}

#[tokio::test]
async fn a_verb_sets_the_state_it_names_and_reads_show_it() {
    let router = router(json!({}), None);
    let (id, status) = created(&router).await;
    assert_eq!(status, "pending");
    let (source, cancelled) = call(&router, "POST", &format!("/orders/{id}/cancel"), None).await;
    assert_eq!(source, "state", "{cancelled}");
    assert_eq!(cancelled["status"], "canceled", "{cancelled}");
    let (_, read) = call(&router, "GET", &format!("/orders/{id}"), None).await;
    assert_eq!(read["status"], "canceled", "{read}");
}

#[tokio::test]
async fn a_verb_naming_no_state_leaves_the_action_undescribed() {
    let router = router(json!({}), None);
    let (id, _) = created(&router).await;
    let (source, _) = call(&router, "POST", &format!("/orders/{id}/review"), None).await;
    assert_ne!(source, "state");
    let (_, read) = call(&router, "GET", &format!("/orders/{id}"), None).await;
    assert_eq!(read["status"], "pending", "{read}");
}

#[tokio::test]
async fn with_a_model_the_action_is_generated() {
    // The create's fill (asked for `item`'s free text), then the cancel's answer.
    let router = router(
        json!({}),
        Some(vec![
            json!({"item": "book"}),
            json!({"id": "x", "item": "book", "status": "pending"}),
        ]),
    );
    let (id, _) = created(&router).await;
    let (source, _) = call(&router, "POST", &format!("/orders/{id}/cancel"), None).await;
    assert_ne!(source, "state");
}

#[tokio::test]
async fn a_described_transition_wins_over_the_verb() {
    let world = json!({
        "types": {"orders": {"lifecycle": {"member": "status", "states": ["pending", "canceled"]}}},
        "operations": {"cancelOrder": {"transition": {"status": "pending"}}}
    });
    let router = router(world, None);
    let (id, _) = created(&router).await;
    let (_, cancelled) = call(&router, "POST", &format!("/orders/{id}/cancel"), None).await;
    assert_eq!(cancelled["status"], "pending", "{cancelled}");
}

const ISSUES: &str = r##"
openapi: 3.0.0
info: {title: issues, version: "1"}
paths:
  /issues:
    post:
      operationId: createIssue
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}}
  /issues/{id}:
    get:
      operationId: getIssue
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}}
  /issues/{id}/close:
    post:
      operationId: closeIssue
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}}
  /issues/{id}/reopen:
    post:
      operationId: reopenIssue
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}}
components:
  schemas:
    Issue:
      type: object
      required: [id, title, state, closed]
      properties:
        id: {type: string}
        title: {type: string}
        state: {type: string, enum: [open, closed]}
        closed: {type: boolean}
        is_closed: {type: boolean}
        closed_at: {type: string, format: date-time, nullable: true}
        closedAt: {type: integer, nullable: true}
"##;

/// The members named for a state follow it: a close sets `closed` and `closed_at`, a
/// reopen clears them.
#[tokio::test]
async fn the_members_named_for_a_state_follow_it() {
    let router = router_of(ISSUES, json!({}), None);
    let (_, issue) = call(&router, "POST", "/issues", Some(json!({"title": "T"}))).await;
    let id = issue["id"].as_str().unwrap_or_default().to_owned();
    assert_eq!(issue["state"], "open", "{issue}");
    let (_, closed) = call(&router, "POST", &format!("/issues/{id}/close"), None).await;
    assert_eq!(closed["state"], "closed", "{closed}");
    assert_eq!(closed["closed"], true, "{closed}");
    let at = closed["closed_at"].as_str().unwrap_or_default();
    assert!(chrono::DateTime::parse_from_rfc3339(at).is_ok(), "{closed}");
    assert_eq!(closed["is_closed"], true, "{closed}");
    // An integer one holds unix seconds.
    assert!(
        closed["closedAt"].as_i64().is_some_and(|s| s > 0),
        "{closed}"
    );
    let (_, read) = call(&router, "GET", &format!("/issues/{id}"), None).await;
    assert_eq!(read["closed"], true, "{read}");
    assert_eq!(read["closed_at"], closed["closed_at"], "{read}");
    let (_, reopened) = call(&router, "POST", &format!("/issues/{id}/reopen"), None).await;
    assert_eq!(reopened["state"], "open", "{reopened}");
    assert_eq!(reopened["closed"], false, "{reopened}");
    assert_eq!(reopened["closed_at"], Value::Null, "{reopened}");
    assert_eq!(reopened["is_closed"], false, "{reopened}");
    assert_eq!(reopened["closedAt"], Value::Null, "{reopened}");
}
