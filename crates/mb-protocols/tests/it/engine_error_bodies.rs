//! Error bodies the mock makes itself: a request that fails validation, and a read of
//! a record the session does not hold. Their members come from the call and the
//! contract instead of the faker: the violated field goes into a path-like member and
//! the reasons into the message-like ones, a code-like member is written once by the
//! LLM for each operation, status and cause, and a missing record answers the status
//! the operation declares for it. Synthetic contracts only.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, UnknownIds, http_rest};
use mb_state::MemoryStore;
use serde_json::{Value, json};
use tower::ServiceExt;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Shelf, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewItem"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "400": {description: Invalid., content: {application/json: {schema: {$ref: "#/components/schemas/Listed"}}}}
  /widgets/{widget_id}:
    get:
      operationId: getWidget
      parameters: [{name: widget_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "401": {description: No key., content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
        "422": {description: Refused., content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
  /gadgets:
    post:
      operationId: createGadget
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewItem"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "422": {description: Invalid., content: {application/json: {schema: {$ref: "#/components/schemas/Pointed"}}}}
  /gizmos:
    post:
      operationId: createGizmo
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewItem"}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "400": {description: Invalid., content: {application/json: {schema: {$ref: "#/components/schemas/Typed"}}}}
  /gizmos/{gizmo_id}:
    get:
      operationId: getGizmo
      parameters: [{name: gizmo_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "404": {description: Not found., content: {application/json: {schema: {$ref: "#/components/schemas/Typed"}}}}
components:
  schemas:
    NewItem:
      type: object
      required: [name]
      properties:
        name: {type: string, minLength: 1}
        size: {type: integer}
    Item:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
    Listed:
      type: object
      required: [message, errors]
      properties:
        message: {type: string}
        errors:
          type: array
          minItems: 1
          items:
            type: object
            required: [path, message]
            properties:
              path: {type: string}
              message: {type: string}
    Pointed:
      type: object
      required: [errors]
      properties:
        errors:
          type: array
          minItems: 1
          items:
            type: object
            required: [source, detail]
            properties:
              source: {type: object, required: [pointer], properties: {pointer: {type: string}}}
              detail: {type: string}
    Coded:
      type: object
      required: [ErrorCode, Message]
      properties:
        ErrorCode: {type: integer}
        Message: {type: string}
    Typed:
      type: object
      required: [error]
      properties:
        error:
          type: object
          required: [type, messages]
          properties:
            type: {type: string}
            messages: {type: array, minItems: 1, items: {type: string}}
"##;

struct Harness {
    router: axum::Router,
    provider: Option<Arc<ScriptedProvider>>,
}

fn harness(script: Option<Vec<Value>>) -> Harness {
    let provider = script.map(|s| {
        Arc::new(ScriptedProvider::new(
            s.into_iter().map(|v| Ok(v.to_string())).collect(),
        ))
    });
    let dyn_provider = provider.clone().map(|p| p as Arc<dyn Provider>);
    let mut rt = MockRuntime::new(
        "shelf",
        Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(dyn_provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    rt.unknown_ids = UnknownIds::NotFound;
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
    }
}

fn post_json(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

async fn send(h: &Harness, req: Request<Body>) -> Response {
    h.router.clone().oneshot(req).await.unwrap()
}

async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A path-like member of an item of the error's array names the violated field, and
/// the item's message says why: for a value that breaks a keyword and for a missing
/// required member alike.
#[tokio::test]
async fn a_refused_body_names_the_violated_field() {
    let h = harness(None);
    let resp = send(&h, post_json("/widgets", r#"{"name": ""}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["errors"][0]["path"], "name", "{body}");
    let message = body["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("minLength"), "{body}");

    let resp = send(&h, post_json("/widgets", r#"{"size": 3}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["errors"][0]["path"], "name", "{body}");
}

/// A member named as a pointer takes the violated field as a JSON pointer.
#[tokio::test]
async fn a_pointer_member_takes_a_json_pointer() {
    let h = harness(None);
    let resp = send(&h, post_json("/gadgets", r#"{"name": 7}"#)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["errors"][0]["source"]["pointer"], "/name", "{body}");
    let detail = body["errors"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("type"), "{body}");
}

/// A code-like member is the LLM's, asked once for the operation, status and cause;
/// the reasons still go into the message array, the call's own.
#[tokio::test]
async fn a_code_member_is_written_once_by_the_llm() {
    let written = json!({"error": {"type": "invalid_parameters", "messages": ["bad"]}});
    let h = harness(Some(vec![written]));
    for body in [r#"{"name": ""}"#, r#"{"size": 1}"#] {
        let resp = send(&h, post_json("/gizmos", body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["type"], "invalid_parameters", "{body}");
        let first = body["error"]["messages"][0].as_str().unwrap();
        assert!(first.contains("name"), "{body}");
    }
    let requests = h.provider.as_ref().unwrap().requests();
    assert_eq!(requests.len(), 1);
    let user = &requests[0].messages.last().unwrap().content;
    assert!(user.contains("400"), "{user}");
}

/// Offline, a code-like member keeps the faker's value: no LLM is asked.
#[tokio::test]
async fn offline_a_code_member_is_the_fakers() {
    let h = harness(None);
    let resp = send(&h, post_json("/gizmos", r#"{"name": ""}"#)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(body["error"]["type"].is_string(), "{body}");
}

/// A read of a record the session does not hold answers the declared 404, its
/// messages saying what is missing, and its code the LLM's.
#[tokio::test]
async fn a_missing_record_answers_the_declared_404() {
    let written = json!({"error": {"type": "not_found", "messages": ["gone"]}});
    let h = harness(Some(vec![written]));
    for id in ["g1", "g2"] {
        let resp = send(&h, get(&format!("/gizmos/{id}"))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["type"], "not_found", "{body}");
        let first = body["error"]["messages"][0].as_str().unwrap();
        assert!(first.contains(id), "{body}");
    }
    assert_eq!(h.provider.as_ref().unwrap().requests().len(), 1);
}

/// With no 404 declared, a missing record answers the operation's one declared 4xx
/// that is not about who asks (a 401 is not): its code the LLM's, its message the
/// mock's.
#[tokio::test]
async fn a_missing_record_answers_the_one_declared_4xx() {
    let written = json!({"ErrorCode": 1101, "Message": "x"});
    let h = harness(Some(vec![written]));
    let resp = send(&h, get("/widgets/w9")).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["ErrorCode"], 1101, "{body}");
    assert!(body["Message"].as_str().unwrap().contains("w9"), "{body}");

    // Offline too, with the faker's code.
    let h = harness(None);
    let resp = send(&h, get("/widgets/w9")).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert!(body["Message"].as_str().unwrap().contains("w9"), "{body}");
}
