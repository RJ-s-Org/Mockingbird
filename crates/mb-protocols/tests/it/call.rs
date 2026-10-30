#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{LlmError, Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, STREAM_ITEMS_HEADER, http_rest};
use mb_state::MemoryStore;
use serde_json::{Value, json};
use tower::ServiceExt;

fn runtime() -> MockRuntime {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let bundle = Arc::new(compile(&std::fs::read(path).unwrap()).unwrap());
    MockRuntime::new(
        "orders",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

/// `X-Mockingbird-Stream-Items` must be an integer in 1..=10000.
#[tokio::test]
async fn stream_items_header_validated() {
    let router = http_rest::router(Arc::new(runtime()));
    for bad in ["0", "abc", "10001"] {
        let req = Request::get("/orders/o1")
            .header(STREAM_ITEMS_HEADER, bad)
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "value {bad:?}");
        assert_eq!(resp.headers()["content-type"], "application/problem+json");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["title"], "Invalid override", "value {bad:?}");
    }
    for good in ["1", "10000"] {
        let req = Request::get("/orders/o1")
            .header(STREAM_ITEMS_HEADER, good)
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "value {good:?}");
    }
}

/// An API whose actions either answer with the resource (`confirm`) or with
/// a record of their own (`cancel`, `refund`).
const ACTIONS: &str = r##"
openapi: 3.0.0
info: {title: Actions, version: "1"}
paths:
  /v1/payment_intents:
    post:
      operationId: createIntent
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {amount: {type: integer}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}
  /v1/payment_intents/{intent}:
    get:
      operationId: getIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}
  /v1/payment_intents/{intent}/confirm:
    post:
      operationId: confirmIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}
  /v1/payment_intents/{intent}/cancel:
    post:
      operationId: cancelIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cancellation"}}}}
  /v1/payment_intents/{intent}/refund:
    post:
      operationId: refundIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
components:
  schemas:
    Intent:
      type: object
      required: [id, status, amount]
      properties:
        id: {type: string}
        status: {type: string}
        amount: {type: integer}
        note: {type: string}
    Cancellation:
      type: object
      required: [intent, status]
      properties: {intent: {type: string}, status: {type: string}}
    Refund:
      type: object
      required: [id, amount]
      properties: {id: {type: string}, amount: {type: integer}}
"##;

struct Actions {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
}

fn actions(script: Vec<Result<String, LlmError>>) -> Actions {
    let provider = Arc::new(ScriptedProvider::new(script));
    let rt = MockRuntime::new(
        "actions",
        Arc::new(compile(ACTIONS.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    Actions {
        router: http_rest::router(Arc::new(rt)),
        provider,
    }
}

impl Actions {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let source = resp.headers()["x-mockingbird-source"]
            .to_str()
            .unwrap()
            .to_owned();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, source, serde_json::from_slice(&bytes).unwrap())
    }

    async fn create_intent(&self) -> String {
        let (status, _, body) = self
            .call("POST", "/v1/payment_intents", Some(json!({"amount": 500})))
            .await;
        assert_eq!(status, StatusCode::OK);
        body["id"].as_str().unwrap().to_owned()
    }
}

fn intent(status: &str) -> Result<String, LlmError> {
    Ok(json!({"id": "pi_1", "status": status, "amount": 500}).to_string())
}

/// The answer of `POST .../confirm` becomes what a read returns, on the same
/// resource, with no further LLM call.
#[tokio::test]
async fn confirm_changes_the_resource_a_read_returns() {
    let h = actions(vec![
        intent("requires_confirmation"),
        Ok(json!({"id": "pi_other", "status": "succeeded", "amount": 500}).to_string()),
    ]);
    let id = h.create_intent().await;

    let (status, _, confirmed) = h
        .call("POST", &format!("/v1/payment_intents/{id}/confirm"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(confirmed["status"], "succeeded");
    assert_eq!(confirmed["id"], id.as_str());

    let (status, source, read) = h
        .call("GET", &format!("/v1/payment_intents/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source, "state");
    assert_eq!(read["status"], "succeeded");
    assert_eq!(read["id"], id.as_str());
    assert_eq!(h.provider.requests().len(), 2);
}

/// What an answer leaves out of the resource stays as stored.
#[tokio::test]
async fn confirm_keeps_what_its_answer_leaves_out() {
    let h = actions(vec![
        Ok(json!({"id": "pi_1", "status": "new", "amount": 500, "note": "kept"}).to_string()),
        intent("succeeded"),
    ]);
    let id = h.create_intent().await;
    h.call("POST", &format!("/v1/payment_intents/{id}/confirm"), None)
        .await;
    let (_, _, read) = h
        .call("GET", &format!("/v1/payment_intents/{id}"), None)
        .await;
    assert_eq!(read["status"], "succeeded");
    assert_eq!(read["note"], "kept");
}

/// A faker answer invents its values, so it must not replace the resource's.
#[tokio::test]
async fn a_failed_confirm_leaves_the_resource_as_it_was() {
    let unusable = || Err(LlmError::Timeout);
    let h = actions(vec![
        intent("requires_confirmation"),
        unusable(),
        unusable(),
        unusable(),
    ]);
    let id = h.create_intent().await;
    let (_, source, confirmed) = h
        .call("POST", &format!("/v1/payment_intents/{id}/confirm"), None)
        .await;
    assert_eq!(source, "fallback");
    assert_eq!(confirmed["status"], "requires_confirmation");
    assert_eq!(confirmed["amount"], 500);
    let (_, _, read) = h
        .call("GET", &format!("/v1/payment_intents/{id}"), None)
        .await;
    assert_eq!(read["status"], "requires_confirmation");
    assert_eq!(read["amount"], 500);
}

/// An action with an answer of its own sees its target, names it where the
/// answer has a member for it, and stores nothing.
#[tokio::test]
async fn cancel_sees_its_target_and_leaves_it_stored() {
    let h = actions(vec![
        intent("requires_confirmation"),
        Ok(json!({"intent": "someone_else", "status": "cancelled"}).to_string()),
    ]);
    let id = h.create_intent().await;
    let (status, _, cancelled) = h
        .call("POST", &format!("/v1/payment_intents/{id}/cancel"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["intent"], id.as_str());

    let requests = h.provider.requests();
    let prompt = format!("{:?}", requests[1].messages);
    assert!(prompt.contains("existing_resource"), "{prompt}");
    assert!(prompt.contains("requires_confirmation"), "{prompt}");

    let (_, source, read) = h
        .call("GET", &format!("/v1/payment_intents/{id}"), None)
        .await;
    assert_eq!(source, "state");
    assert_eq!(read["status"], "requires_confirmation");
    assert_eq!(h.provider.requests().len(), 2);
}

/// `POST /captures/{capture_id}/refund` answers a refund: its `id` is the
/// refund's, not the target's.
#[tokio::test]
async fn an_action_answer_keeps_its_own_id() {
    let h = actions(vec![
        intent("succeeded"),
        Ok(json!({"id": "re_1", "amount": 500}).to_string()),
    ]);
    let id = h.create_intent().await;
    let (_, _, refund) = h
        .call("POST", &format!("/v1/payment_intents/{id}/refund"), None)
        .await;
    assert_eq!(refund["id"], "re_1");
    assert_eq!(refund.get("intent"), None);
}
