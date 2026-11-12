//! `unknown_ids` left unset: a read of an id the session does not hold answers the
//! read's declared 404 for a type the contract can create once the session holds a
//! record of it, and generates for any other. Driven over HTTP with a scripted LLM.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, UnknownIds, http_rest};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Stripe's shape: customers are created, listed, read and deleted by `{customer}`, a
/// path parameter not named like an id; prices are only listed and read by `{price}`.
const SHOP: &str = r##"
openapi: 3.0.3
info: {title: Shop, version: "1"}
paths:
  /v1/customers:
    get:
      operationId: GetCustomers
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/customer"}}}}}
    post:
      operationId: PostCustomers
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/customer"}}}}
  /v1/customers/{customer}:
    parameters: [{name: customer, in: path, required: true, schema: {type: string}}]
    get:
      operationId: GetCustomersCustomer
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/customer"}}}}
        "404": {description: missing, content: {application/json: {schema: {$ref: "#/components/schemas/error"}}}}
    delete:
      operationId: DeleteCustomersCustomer
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}, deleted: {type: boolean}}}}}}
  /v1/prices:
    get:
      operationId: GetPrices
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/price"}}}}}
  /v1/prices/{price}:
    get:
      operationId: GetPricesPrice
      parameters: [{name: price, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/price"}}}}
        "404": {description: missing, content: {application/json: {schema: {$ref: "#/components/schemas/error"}}}}
components:
  schemas:
    customer:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    price:
      type: object
      required: [id, unit_amount]
      properties: {id: {type: string}, unit_amount: {type: integer}}
    error:
      type: object
      required: [error]
      properties:
        error:
          type: object
          required: [type, message]
          properties: {type: {type: string}, message: {type: string}}
"##;

struct Harness {
    router: axum::Router,
}

fn shop(unknown_ids: UnknownIds, script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut rt = MockRuntime::new(
        "shop",
        Arc::new(compile(SHOP.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))) as Arc<dyn StateStore>,
    );
    rt.unknown_ids = unknown_ids;
    Harness {
        router: http_rest::router(Arc::new(rt)),
    }
}

impl Harness {
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Creates a customer; its id.
    async fn create_customer(&self) -> String {
        let (status, created) = self
            .call("POST", "/v1/customers", Some(json!({"name": "Ana"})))
            .await;
        assert!(status.is_success(), "{status} {created}");
        created["id"].as_str().unwrap().to_owned()
    }
}

fn customer(id: &str) -> Value {
    json!({"id": id, "name": "Generated"})
}

fn price(id: &str) -> Value {
    json!({"id": id, "unit_amount": 500})
}

/// The declared 404's body: Stripe's `{error: {type, message}}`.
fn assert_declared_404(status: StatusCode, body: &Value) {
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"]["type"].is_string(), "{body}");
}

/// A creatable type the session holds a record of: an id it never made is missing,
/// answered with the read's declared 404, through a `{customer}` parameter. The created
/// one still reads back.
#[tokio::test]
async fn a_creatable_type_the_session_holds_answers_an_unknown_id_with_its_404() {
    let h = shop(UnknownIds::default(), vec![customer("cus_1")]);
    let id = h.create_customer().await;
    let (status, body) = h.call("GET", "/v1/customers/cus_invalid", None).await;
    assert_declared_404(status, &body);
    let (status, body) = h.call("GET", &format!("/v1/customers/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "Ana", "{body}");
}

/// A listed record counts as held, as a created one does.
#[tokio::test]
async fn a_listed_record_makes_its_creatable_type_held() {
    let h = shop(UnknownIds::default(), vec![json!([customer("cus_1")])]);
    let (status, _) = h.call("GET", "/v1/customers", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = h.call("GET", "/v1/customers/cus_1", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = h.call("GET", "/v1/customers/cus_invalid", None).await;
    assert_declared_404(status, &body);
}

/// Before the session holds any record of the type, an id is reference data: generated.
/// A record stored only because a read generated it does not make the type held, so a
/// second such id is generated too.
#[tokio::test]
async fn a_creatable_type_the_session_holds_nothing_of_generates() {
    let h = shop(
        UnknownIds::default(),
        vec![customer("cus_a"), customer("cus_b")],
    );
    for id in ["cus_a", "cus_b"] {
        let (status, body) = h.call("GET", &format!("/v1/customers/{id}"), None).await;
        assert_eq!(status, StatusCode::OK, "{id}: {body}");
        assert_eq!(body["id"], id, "{body}");
    }
}

/// A type nothing creates generates an unknown id though the session listed some.
#[tokio::test]
async fn a_type_nothing_creates_generates() {
    let h = shop(
        UnknownIds::default(),
        vec![json!([price("price_1")]), price("price_from_config")],
    );
    let (status, _) = h.call("GET", "/v1/prices", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = h.call("GET", "/v1/prices/price_from_config", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "price_from_config", "{body}");
}

/// `unknown_ids: generate` overrides the inference: a held creatable type generates.
#[tokio::test]
async fn generate_overrides_the_inference() {
    let h = shop(
        UnknownIds::Generate,
        vec![customer("cus_1"), customer("cus_other")],
    );
    h.create_customer().await;
    let (status, body) = h.call("GET", "/v1/customers/cus_other", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "cus_other", "{body}");
}

/// `unknown_ids: 404` overrides the inference: a type nothing creates, and a creatable
/// one the session holds nothing of, answer their 404, through a parameter not named
/// like an id.
#[tokio::test]
async fn not_found_overrides_the_inference() {
    let h = shop(UnknownIds::NotFound, vec![]);
    let (status, body) = h.call("GET", "/v1/prices/price_unknown", None).await;
    assert_declared_404(status, &body);
    let (status, body) = h.call("GET", "/v1/customers/cus_unknown", None).await;
    assert_declared_404(status, &body);
}

/// A deleted id answers 404 though the delete left the session holding no customer.
#[tokio::test]
async fn a_deleted_id_still_answers_404() {
    let h = shop(
        UnknownIds::default(),
        vec![
            customer("cus_1"),
            json!({"error": {"type": "invalid_request_error", "message": "No such customer"}}),
        ],
    );
    let id = h.create_customer().await;
    let (status, _) = h.call("DELETE", &format!("/v1/customers/{id}"), None).await;
    assert!(status.is_success(), "{status}");
    let (status, body) = h.call("GET", &format!("/v1/customers/{id}"), None).await;
    assert_declared_404(status, &body);
}
