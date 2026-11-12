//! `unknown_ids` left unset: a read of an id the session does not hold answers the
//! read's declared 404 for a type the contract can create once the session has written
//! a record of it, and generates for any other. Driven over HTTP with a scripted LLM.
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
  /v1/payment_intents:
    post:
      operationId: PostPaymentIntents
      requestBody: {content: {application/json: {schema: {type: object, properties: {amount: {type: integer}}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/payment_intent"}}}}
  /v1/payment_intents/{intent}:
    get:
      operationId: GetPaymentIntentsIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/payment_intent"}}}}
        "404": {description: missing, content: {application/json: {schema: {$ref: "#/components/schemas/error"}}}}
  /v1/checkout/sessions:
    post:
      operationId: PostCheckoutSessions
      requestBody: {content: {application/json: {schema: {type: object, properties: {mode: {type: string}}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/checkout_session"}}}}
  /v1/checkout/sessions/{session}:
    get:
      operationId: GetCheckoutSessionsSession
      parameters: [{name: session, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/checkout_session"}}}}
        "404": {description: missing, content: {application/json: {schema: {$ref: "#/components/schemas/error"}}}}
  /2010-04-01/Accounts/{AccountSid}/Messages.json:
    post:
      operationId: CreateMessage
      parameters: [{name: AccountSid, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {body: {type: string}}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/message"}}}}
  /2010-04-01/Accounts/{AccountSid}/Messages/{Sid}.json:
    parameters:
      - {name: AccountSid, in: path, required: true, schema: {type: string}}
      - {name: Sid, in: path, required: true, schema: {type: string}}
    get:
      operationId: FetchMessage
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/message"}}}}
        "404": {description: missing, content: {application/json: {schema: {$ref: "#/components/schemas/error"}}}}
    delete:
      operationId: DeleteMessage
      responses:
        "204": {description: gone}
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
    payment_intent:
      type: object
      required: [id, amount]
      properties: {id: {type: string}, amount: {type: integer}}
    checkout_session:
      type: object
      required: [id, mode]
      properties: {id: {type: string}, mode: {type: string}}
    message:
      type: object
      required: [sid, body]
      properties: {sid: {type: string}, account_sid: {type: string}, body: {type: string}}
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
    shop_over(store(), unknown_ids, script)
}

fn store() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new(Duration::from_secs(3600)))
}

/// A runtime of [`SHOP`] over `store`: a new one over the same store is a restart, a
/// rebuild or another node, with nothing remembered in memory.
fn shop_over(store: Arc<dyn StateStore>, unknown_ids: UnknownIds, script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut rt = MockRuntime::new(
        "shop",
        Arc::new(compile(SHOP.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        store,
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

/// A creatable type the session created a record of: an id it never made is missing,
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

/// A list (or a search) only reads: its records do not make the type the session's
/// own, so an id it does not hold is still generated.
#[tokio::test]
async fn a_list_alone_does_not_make_a_type_owned() {
    let h = shop(
        UnknownIds::default(),
        vec![json!([customer("cus_1")]), customer("cus_other")],
    );
    let (status, _) = h.call("GET", "/v1/customers", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = h.call("GET", "/v1/customers/cus_1", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = h.call("GET", "/v1/customers/cus_other", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "cus_other", "{body}");
}

/// Before the session writes any record of the type, an id is reference data: generated.
/// A record stored only because a read generated it does not make the type the
/// session's own, so a second such id is generated too.
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

/// A type nothing creates generates an unknown id, listed or not.
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

/// A generated read leaves nothing that makes the type owned on a runtime rebuilt over the
/// same store (a restart, another node); a create does, and it outlives the rebuild.
#[tokio::test]
async fn ownership_is_read_from_the_store() {
    let shared = store();
    let first = shop_over(
        Arc::clone(&shared),
        UnknownIds::default(),
        vec![customer("cus_a")],
    );
    let (status, _) = first.call("GET", "/v1/customers/cus_a", None).await;
    assert_eq!(status, StatusCode::OK);
    let rebuilt = shop_over(
        Arc::clone(&shared),
        UnknownIds::default(),
        vec![customer("cus_b"), customer("cus_1")],
    );
    let (status, body) = rebuilt.call("GET", "/v1/customers/cus_b", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "cus_b", "{body}");

    rebuilt.create_customer().await;
    let again = shop_over(Arc::clone(&shared), UnknownIds::default(), vec![]);
    let (status, body) = again.call("GET", "/v1/customers/cus_unknown", None).await;
    assert_declared_404(status, &body);
}

/// Twilio's flow: a message created and deleted leaves the session holding none, and a
/// fetch of an unknown SID under the nested `{Sid}.json` route still answers 404.
#[tokio::test]
async fn a_session_that_deleted_its_last_message_answers_an_unknown_sid_with_404() {
    let h = shop(
        UnknownIds::default(),
        vec![
            json!({"sid": "SM1", "account_sid": "AC1", "body": "hi"}),
            json!({"error": {"type": "not_found", "message": "No such message"}}),
        ],
    );
    let (status, created) = h
        .call(
            "POST",
            "/2010-04-01/Accounts/AC1/Messages.json",
            Some(json!({"body": "hi"})),
        )
        .await;
    assert!(status.is_success(), "{status} {created}");
    let sid = created["sid"].as_str().unwrap().to_owned();
    let (status, _) = h
        .call(
            "DELETE",
            &format!("/2010-04-01/Accounts/AC1/Messages/{sid}.json"),
            None,
        )
        .await;
    assert!(status.is_success(), "{status}");
    let (status, body) = h
        .call(
            "GET",
            "/2010-04-01/Accounts/AC1/Messages/SM00000000000000000000000000000000.json",
            None,
        )
        .await;
    assert_declared_404(status, &body);
}

/// Stripe's `{intent}` and `{session}`, parameters named for the record they select:
/// after a create of each, an unknown id answers 404, as does one read under
/// `unknown_ids: 404` before any create.
#[tokio::test]
async fn intent_and_session_parameters_select_their_record() {
    let h = shop(
        UnknownIds::default(),
        vec![
            json!({"id": "pi_1", "amount": 500}),
            json!({"id": "cs_1", "mode": "payment"}),
        ],
    );
    let (status, _) = h
        .call("POST", "/v1/payment_intents", Some(json!({"amount": 500})))
        .await;
    assert!(status.is_success());
    let (status, _) = h
        .call(
            "POST",
            "/v1/checkout/sessions",
            Some(json!({"mode": "payment"})),
        )
        .await;
    assert!(status.is_success());
    for uri in [
        "/v1/payment_intents/pi_missing",
        "/v1/checkout/sessions/cs_test_invalid",
    ] {
        let (status, body) = h.call("GET", uri, None).await;
        assert_declared_404(status, &body);
    }
    let h = shop(UnknownIds::NotFound, vec![]);
    for uri in [
        "/v1/payment_intents/pi_missing",
        "/v1/checkout/sessions/cs_test_invalid",
    ] {
        let (status, body) = h.call("GET", uri, None).await;
        assert_declared_404(status, &body);
    }
}
