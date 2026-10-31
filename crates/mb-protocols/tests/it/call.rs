#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig, SchemaValidator};
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
    actions_of(ACTIONS, script)
}

fn actions_of(contract: &str, script: Vec<Result<String, LlmError>>) -> Actions {
    let provider = Arc::new(ScriptedProvider::new(script));
    let rt = MockRuntime::new(
        "actions",
        Arc::new(compile(contract.as_bytes()).unwrap()),
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

/// The create answers a shape of its own, with a member the read and the
/// action's closed schema do not allow.
const CREATED_APART: &str = r##"
openapi: 3.0.0
info: {title: Actions, version: "1"}
paths:
  /payment_intents:
    post:
      operationId: createIntent
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Created"}}}}
  /payment_intents/{intent}:
    get:
      operationId: getIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}
  /payment_intents/{intent}/confirm:
    post:
      operationId: confirmIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}
components:
  schemas:
    Created:
      type: object
      required: [id, status, client_secret]
      properties: {id: {type: string}, status: {type: string}, client_secret: {type: string}}
    Intent:
      type: object
      additionalProperties: false
      required: [id, status]
      properties: {id: {type: string}, status: {type: string}, note: {type: string}}
"##;

/// A stored resource need not fit the action's schema: it may come from a
/// create of another shape, a seeded list item or a fixture. The answer stays
/// valid, and so does what is stored.
#[tokio::test]
async fn confirm_stays_valid_when_the_stored_resource_does_not_fit() {
    let h = actions_of(
        CREATED_APART,
        vec![
            Ok(
                json!({"id": "pi_1", "status": "new", "client_secret": "s", "note": "kept"})
                    .to_string(),
            ),
            Ok(json!({"id": "pi_1", "status": "succeeded"}).to_string()),
        ],
    );
    let (_, _, created) = h.call("POST", "/payment_intents", None).await;
    assert_eq!(created["client_secret"], "s");

    let (status, source, confirmed) = h.call("POST", "/payment_intents/pi_1/confirm", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source, "llm");
    let expected = json!({"id": "pi_1", "status": "succeeded", "note": "kept"});
    assert_eq!(confirmed, expected);

    let bundle = compile(CREATED_APART.as_bytes()).unwrap();
    let schema = json!({"$ref": "#/$defs/Intent", "$defs": bundle.defs});
    let validator = SchemaValidator::new(&schema).unwrap();
    assert!(validator.is_valid(&confirmed), "{confirmed}");

    let (status, source, read) = h.call("GET", "/payment_intents/pi_1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source, "state");
    assert_eq!(read, expected);
    assert_eq!(h.provider.requests().len(), 2);
}

/// An action whose answer is not an object has nothing to merge.
#[tokio::test]
async fn an_action_keeps_an_answer_that_is_not_an_object() {
    let contract = r##"
openapi: 3.0.0
info: {title: Actions, version: "1"}
paths:
  /jobs:
    post:
      operationId: createJob
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: object, required: [id], properties: {id: {type: string}}}}}
  /jobs/{job}:
    get:
      operationId: getJob
      parameters: [{name: job, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: array, items: {type: string}}}}
  /jobs/{job}/run:
    post:
      operationId: runJob
      parameters: [{name: job, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: array, items: {type: string}}}}
"##;
    let h = actions_of(
        contract,
        vec![
            Ok(json!({"id": "j1"}).to_string()),
            Ok(json!(["started"]).to_string()),
        ],
    );
    h.call("POST", "/jobs", None).await;
    let (status, source, answer) = h.call("POST", "/jobs/j1/run", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source, "llm");
    assert_eq!(answer, json!(["started"]));
}

/// An API whose orders mention customers, by a `$ref`.
const WORLD: &str = r##"
openapi: 3.0.0
info: {title: World, version: "1"}
paths:
  /orders/{orderId}:
    get:
      operationId: getOrder
      parameters: [{name: orderId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}
  /orders:
    post:
      operationId: createOrder
      requestBody:
        required: true
        content: {application/json: {schema: {type: object}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}
  /orders/{orderId}/confirm:
    post:
      operationId: confirmOrder
      parameters: [{name: orderId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}
  /customers/{customerId}:
    get:
      operationId: getCustomer
      parameters: [{name: customerId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}
    delete:
      operationId: deleteCustomer
      parameters: [{name: customerId, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  customer: {$ref: "#/components/schemas/Customer"}
  /backlog:
    get:
      operationId: backlog
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  entries: {type: array, items: {$ref: "#/components/schemas/Order"}}
  /summary:
    get:
      operationId: summary
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  customers: {type: array, items: {$ref: "#/components/schemas/Customer"}}
                  orders: {type: array, items: {$ref: "#/components/schemas/Order"}}
  /deep:
    get:
      operationId: deep
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object}}}}
components:
  schemas:
    Customer:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        tier: {type: string, enum: [gold, silver]}
    Order:
      type: object
      required: [id]
      properties:
        id: {type: string}
        status: {type: string}
        customer: {$ref: "#/components/schemas/Customer"}
"##;

fn order(id: &str, customer: Value) -> Result<String, LlmError> {
    Ok(json!({"id": id, "status": "open", "customer": customer}).to_string())
}

/// [`actions_of`] with `consistency: off`.
fn actions_off(contract: &str, script: Vec<Result<String, LlmError>>) -> Actions {
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut rt = MockRuntime::new(
        "actions",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    rt.consistency = false;
    Actions {
        router: http_rest::router(Arc::new(rt)),
        provider,
    }
}

/// What a read returned, with its source.
async fn read(api: &Actions, uri: &str) -> (String, Value) {
    let (status, source, body) = api.call("GET", uri, None).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    (source, body)
}

/// A customer an order's answer mentions is read back as it was answered,
/// with no further LLM call.
#[tokio::test]
async fn a_nested_customer_reads_back() {
    let api = actions_of(WORLD, vec![order("o1", json!({"id": "c1", "name": "Ana"}))]);
    read(&api, "/orders/o1").await;
    let (source, customer) = read(&api, "/customers/c1").await;
    assert_eq!(source, "state");
    assert_eq!(customer["name"], "Ana");
    assert_eq!(api.provider.requests().len(), 1);
}

/// An action's answer seeds like any other.
#[tokio::test]
async fn an_action_answer_seeds_what_it_mentions() {
    let api = actions_of(WORLD, vec![order("o1", json!({"id": "c1", "name": "Ana"}))]);
    let (status, _, _) = api.call("POST", "/orders/o1/confirm", None).await;
    assert_eq!(status, StatusCode::OK);
    let (source, customer) = read(&api, "/customers/c1").await;
    assert_eq!(source, "state");
    assert_eq!(customer["name"], "Ana");
}

/// A later answer repeating an entity leaves the stored one as it was.
#[tokio::test]
async fn a_read_does_not_overwrite_a_stored_entity() {
    let api = actions_of(
        WORLD,
        vec![
            order("o1", json!({"id": "c1", "name": "Ana"})),
            order("o2", json!({"id": "c1", "name": "Other"})),
        ],
    );
    read(&api, "/orders/o1").await;
    read(&api, "/orders/o2").await;
    let (source, customer) = read(&api, "/customers/c1").await;
    assert_eq!(source, "state");
    assert_eq!(customer["name"], "Ana");
}

/// A delete's answer naming what it deleted does not bring it back.
#[tokio::test]
async fn a_delete_answer_does_not_seed_what_it_deleted() {
    let api = actions_of(
        WORLD,
        vec![
            order("o1", json!({"id": "c1", "name": "Ana"})),
            Ok(json!({"customer": {"id": "c1", "name": "Ana"}}).to_string()),
            Ok(json!({"id": "c1", "name": "Generated"}).to_string()),
        ],
    );
    read(&api, "/orders/o1").await;
    let (status, _, _) = api.call("DELETE", "/customers/c1", None).await;
    assert_eq!(status, StatusCode::OK);
    let (source, _) = read(&api, "/customers/c1").await;
    assert_eq!(source, "llm");
}

/// A create mentioning a stored customer leaves that customer as it was.
#[tokio::test]
async fn a_create_mention_leaves_a_stored_entity_unchanged() {
    let api = actions_of(
        WORLD,
        vec![
            order("o1", json!({"id": "c1", "name": "Ana"})),
            order("o2", json!({"id": "c1", "name": "Other"})),
        ],
    );
    read(&api, "/orders/o1").await;
    let (status, _, _) = api.call("POST", "/orders", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (source, customer) = read(&api, "/customers/c1").await;
    assert_eq!(source, "state");
    assert_eq!(customer["name"], "Ana");
}

/// A create's own wrapped member still replaces what the session holds.
#[tokio::test]
async fn a_create_updates_its_own_wrapped_member() {
    let api = actions_of(
        &WORLD.replace(
            "        customer: {$ref: \"#/components/schemas/Customer\"}\n",
            "        customer: {$ref: \"#/components/schemas/Customer\"}\n        order: {type: object}\n",
        ),
        vec![
            order("o1", json!({"id": "c1", "name": "Ana"})),
            Ok(json!({"id": "o2", "order": {"id": "o1", "status": "merged"}}).to_string()),
        ],
    );
    read(&api, "/orders/o1").await;
    let (status, _, _) = api.call("POST", "/orders", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (source, stored) = read(&api, "/orders/o1").await;
    assert_eq!(source, "state");
    assert_eq!(stored["status"], "merged");
}

/// A repeated read answers the same whatever another read mentions meanwhile:
/// the first answer is the stored one.
#[tokio::test]
async fn a_mention_does_not_change_a_repeated_read() {
    let api = actions_of(
        WORLD,
        vec![
            Ok(json!({"id": "c1", "name": "Bob"}).to_string()),
            order("o1", json!({"id": "c1", "name": "Ana"})),
        ],
    );
    let (_, first) = read(&api, "/customers/c1").await;
    assert_eq!(first["name"], "Bob");
    read(&api, "/orders/o1").await;
    let (_, again) = read(&api, "/customers/c1").await;
    assert_eq!(again["name"], "Bob");
    assert_eq!(api.provider.requests().len(), 2);
}

/// A by-id read's answer stays out of state with consistency off.
#[tokio::test]
async fn consistency_off_stores_no_read_answer() {
    let api = actions_off(
        WORLD,
        vec![
            Ok(json!({"id": "c1", "name": "Bob"}).to_string()),
            Ok(json!({"id": "c1", "name": "Generated"}).to_string()),
        ],
    );
    read(&api, "/customers/c1").await;
    let (source, customer) = read(&api, "/customers/c1").await;
    assert_ne!(source, "state");
    assert_eq!(customer["name"], "Generated");
}

/// A faker answer standing in for a failed LLM call is not a by-id read's
/// stored answer.
#[tokio::test]
async fn a_faker_fallback_read_is_not_stored() {
    let api = actions_of(WORLD, vec![]);
    let (source, _) = read(&api, "/customers/c1").await;
    assert_eq!(source, "fallback");
    let (source, _) = read(&api, "/customers/c1").await;
    assert_ne!(source, "state");
}

/// A by-name read's answer is stored under its own id, so another owner's
/// repository of that name is not it.
#[tokio::test]
async fn a_read_by_name_does_not_answer_for_another_owner() {
    let api = actions_of(
        REPOS,
        vec![
            Ok(repo(7, "octo", 42).to_string()),
            Ok(repo(8, "cat", 9).to_string()),
        ],
    );
    let (_, first) = read(&api, "/repos/octo/hello").await;
    assert_eq!(first["stargazers_count"], 42);
    let (_, other) = read(&api, "/repos/cat/hello").await;
    assert_eq!(other["stargazers_count"], 9);
    let (source, again) = read(&api, "/repos/octo/hello").await;
    assert_eq!(source, "state");
    assert_eq!(again["stargazers_count"], 42);
}

/// With consistency off, a stored repository is not found by its name.
#[tokio::test]
async fn consistency_off_finds_nothing_by_name() {
    let api = actions_off(
        REPOS,
        vec![
            Ok(repo(7, "octo", 42).to_string()),
            Ok(repo(7, "octo", 1).to_string()),
        ],
    );
    let (status, _, _) = api.call("POST", "/user/repos", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (source, found) = read(&api, "/repos/octo/hello").await;
    assert_ne!(source, "state");
    assert_eq!(found["stargazers_count"], 1);
}

/// `customers` here has only a list read: an entity its items' schema would
/// refuse stays out of state, and so of the list's prompt.
const LIST_ONLY: &str = r##"
openapi: 3.0.0
info: {title: ListOnly, version: "1"}
paths:
  /customers:
    get:
      operationId: listCustomers
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {type: array, items: {$ref: "#/components/schemas/Customer"}}
  /user:
    get:
      operationId: getCaller
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [id, login]
                properties: {id: {type: integer}, login: {type: string}}
  /deep:
    get:
      operationId: deep
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object}}}}
components:
  schemas:
    Customer:
      type: object
      required: [id]
      properties: {id: {type: string}, name: {type: string}}
"##;

fn prompt_of(api: &Actions, call: usize) -> String {
    api.provider.requests()[call].messages[1].content.clone()
}

/// A type with a list read only checks what it stores against the items.
#[tokio::test]
async fn a_list_only_type_refuses_what_its_items_refuse() {
    let api = actions_of(
        LIST_ONLY,
        vec![
            Ok(json!({"customer": {"id": 5, "name": "junk-name"}}).to_string()),
            Ok(json!([]).to_string()),
        ],
    );
    read(&api, "/deep").await;
    read(&api, "/customers").await;
    assert!(!prompt_of(&api, 1).contains("junk-name"));
}

/// What its items accept is stored and reaches the list's prompt.
#[tokio::test]
async fn a_list_only_type_keeps_what_its_items_accept() {
    let api = actions_of(
        LIST_ONLY,
        vec![
            Ok(json!({"customer": {"id": "c5", "name": "good-name"}}).to_string()),
            Ok(json!([]).to_string()),
        ],
    );
    read(&api, "/deep").await;
    read(&api, "/customers").await;
    assert!(prompt_of(&api, 1).contains("good-name"));
}

/// A singleton read's schema checks what is stored under its type.
#[tokio::test]
async fn a_singleton_type_refuses_what_its_read_refuses() {
    let api = actions_of(
        LIST_ONLY,
        vec![
            Ok(
                json!({"user": {"id": 5}, "other": {"user": {"id": 6, "login": "ok-login"}}})
                    .to_string(),
            ),
            Ok(json!({"id": 1, "login": "me"}).to_string()),
        ],
    );
    read(&api, "/deep").await;
    read(&api, "/user").await;
    let prompt = prompt_of(&api, 1);
    assert!(prompt.contains("ok-login"), "{prompt}");
    assert!(!prompt.contains("\"id\":5"), "{prompt}");
}

/// With consistency off, an answer stores nothing: the read generates anew.
#[tokio::test]
async fn consistency_off_stores_no_entities() {
    let api = actions_off(
        WORLD,
        vec![
            order("o1", json!({"id": "c1", "name": "Ana"})),
            Ok(json!({"id": "c1", "name": "Generated"}).to_string()),
        ],
    );
    read(&api, "/orders/o1").await;
    let (source, customer) = read(&api, "/customers/c1").await;
    assert_ne!(source, "state");
    assert_eq!(customer["name"], "Generated");
}

/// A faker answer standing in for a failed LLM call seeds nothing.
#[tokio::test]
async fn a_faker_fallback_seeds_nothing() {
    let api = actions_of(WORLD, vec![]);
    let (source, order) = read(&api, "/orders/o1").await;
    assert_eq!(source, "fallback");
    let Some(id) = order["customer"]["id"].as_str() else {
        return;
    };
    let (source, _) = read(&api, &format!("/customers/{id}")).await;
    assert_ne!(source, "state");
}

/// An entity its read's schema would refuse is not stored, so the read
/// generates instead of serving something invalid or falling to the faker.
#[tokio::test]
async fn an_entity_its_read_would_refuse_is_not_seeded() {
    let contract = WORLD.replace(
        "customer: {$ref: \"#/components/schemas/Customer\"}",
        "customer: {type: object, properties: {id: {type: string}}}",
    );
    let api = actions_of(
        &contract,
        vec![
            order("o1", json!({"id": "c1"})),
            Ok(json!({"id": "c1", "name": "Generated"}).to_string()),
        ],
    );
    read(&api, "/orders/o1").await;
    let (source, customer) = read(&api, "/customers/c1").await;
    assert_eq!(source, "llm");
    assert_eq!(customer["name"], "Generated");
}

/// An inline member takes the type it is named after.
#[tokio::test]
async fn an_inline_member_takes_the_type_it_names() {
    let contract = WORLD.replace(
        "customer: {$ref: \"#/components/schemas/Customer\"}",
        "customer: {type: object, properties: {id: {type: string}, name: {type: string}}}",
    );
    let api = actions_of(
        &contract,
        vec![order("o1", json!({"id": "c1", "name": "Ana"}))],
    );
    read(&api, "/orders/o1").await;
    let (source, customer) = read(&api, "/customers/c1").await;
    assert_eq!(source, "state");
    assert_eq!(customer["name"], "Ana");
}

/// Depth counts from the answer: a customer six levels down is seeded, one
/// below that is not.
#[tokio::test]
async fn seeding_stops_at_depth_six() {
    fn nested(levels: usize, leaf: Value) -> Value {
        (0..levels).fold(leaf, |inner, _| json!({"wrap": inner}))
    }
    let api = actions_of(
        WORLD,
        vec![
            Ok(
                json!({"a": nested(4, json!({"customer": {"id": "in", "name": "Ana"}}))})
                    .to_string(),
            ),
            Ok(
                json!({"a": nested(5, json!({"customer": {"id": "out", "name": "Bo"}}))})
                    .to_string(),
            ),
            Ok(json!({"id": "out", "name": "Generated"}).to_string()),
        ],
    );
    read(&api, "/deep").await;
    let (source, _) = read(&api, "/customers/in").await;
    assert_eq!(source, "state");
    read(&api, "/deep?again=1").await;
    let (source, _) = read(&api, "/customers/out").await;
    assert_eq!(source, "llm");
}

/// One answer seeds at most 100 entities.
#[tokio::test]
async fn one_answer_seeds_at_most_a_hundred_entities() {
    let entries: Vec<Value> = (0..120)
        .map(|i| json!({"id": format!("o{i}"), "customer": {"id": format!("c{i}"), "name": "Ana"}}))
        .collect();
    let api = actions_of(
        WORLD,
        vec![
            Ok(json!({"entries": entries}).to_string()),
            Ok(json!({"id": "c119", "name": "Generated"}).to_string()),
        ],
    );
    read(&api, "/backlog").await;
    let (source, _) = read(&api, "/customers/c0").await;
    assert_eq!(source, "state");
    let (source, _) = read(&api, "/customers/c119").await;
    assert_eq!(source, "llm");
}

/// GitHub's shape: search items and the detail route share `$ref`
/// `repository`, and the route's `{repo}` names no field.
const REPOS: &str = r##"
openapi: 3.0.0
info: {title: Repos, version: "1"}
paths:
  /search/repositories:
    get:
      operationId: searchRepos
      parameters: [{name: q, in: query, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  total_count: {type: integer}
                  items: {type: array, items: {$ref: "#/components/schemas/repository"}}
  /user/repos:
    get:
      operationId: listMine
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {type: array, items: {$ref: "#/components/schemas/repository"}}
    post:
      operationId: createRepo
      requestBody:
        required: true
        content: {application/json: {schema: {type: object}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/repository"}}}}
  /repos/{owner}/{repo}:
    get:
      operationId: getRepo
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/repository"}}}}
  /user:
    get:
      operationId: getCaller
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object}}}}
  /users/{username}:
    get:
      operationId: getUser
      parameters: [{name: username, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [id]
                properties:
                  id: {type: integer}
                  login: {type: string}
                  full_name: {type: string}
components:
  schemas:
    repository:
      type: object
      required: [id, name, stargazers_count]
      properties:
        id: {type: integer}
        name: {type: string}
        full_name: {type: string}
        stargazers_count: {type: integer}
        user:
          type: object
          properties:
            id: {type: integer}
            login: {type: string}
            full_name: {type: string}
"##;

fn repo(id: u64, owner: &str, stars: u64) -> Value {
    json!({
        "id": id,
        "name": "hello",
        "full_name": format!("{owner}/hello"),
        "stargazers_count": stars,
    })
}

/// A repository a search found is what its detail route answers.
#[tokio::test]
async fn search_items_seed_the_detail_route() {
    let api = actions_of(
        REPOS,
        vec![Ok(
            json!({"total_count": 1, "items": [repo(7, "octo", 42)]}).to_string(),
        )],
    );
    read(&api, "/search/repositories?q=hello").await;
    let (source, found) = read(&api, "/repos/octo/hello").await;
    assert_eq!(source, "state");
    assert_eq!(found["stargazers_count"], 42);
    assert_eq!(found["id"], 7);
    assert_eq!(api.provider.requests().len(), 1);
}

/// Two owners' repositories of one name stay apart, and a third owner's is
/// neither of them.
#[tokio::test]
async fn search_items_of_one_name_stay_with_their_owners() {
    let api = actions_of(
        REPOS,
        vec![
            Ok(
                json!({"total_count": 2, "items": [repo(7, "octo", 42), repo(8, "cat", 9)]})
                    .to_string(),
            ),
            Ok(repo(9, "dog", 1).to_string()),
        ],
    );
    read(&api, "/search/repositories?q=hello").await;
    let (source, found) = read(&api, "/repos/cat/hello").await;
    assert_eq!(source, "state");
    assert_eq!(found["stargazers_count"], 9);
    let (source, found) = read(&api, "/repos/octo/hello").await;
    assert_eq!(source, "state");
    assert_eq!(found["stargazers_count"], 42);
    let (source, _) = read(&api, "/repos/dog/hello").await;
    assert_eq!(source, "llm");
}

/// A `user` member is `/users/{username}`, which a read can name, not the
/// caller's `/user`. A name's check needs a route naming an owner: a
/// person's `full_name` is not a path.
#[tokio::test]
async fn a_nested_owner_reads_back_by_login() {
    let mut item = repo(7, "octo", 42);
    item["user"] = json!({"id": 1, "login": "octo", "full_name": "Octo Cat"});
    let api = actions_of(
        REPOS,
        vec![Ok(json!({"total_count": 1, "items": [item]}).to_string())],
    );
    read(&api, "/search/repositories?q=hello").await;
    let (source, user) = read(&api, "/users/octo").await;
    assert_eq!(source, "state");
    assert_eq!(user["full_name"], "Octo Cat");
}

/// The real GitHub contract: a valid search item, drawn by the faker, reads
/// back from the detail route.
#[tokio::test]
async fn the_github_contract_seeds_its_detail_route() {
    let path = format!(
        "{}/../../examples/apis/github-rest/github-rest.openapi.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let contract = std::fs::read_to_string(path).unwrap();
    let offline = http_rest::router(Arc::new(MockRuntime::new(
        "github",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )));
    let req = Request::get("/search/repositories?q=x")
        .body(Body::empty())
        .unwrap();
    let resp = offline.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let mut search: Value = serde_json::from_slice(&bytes).unwrap();
    search["items"][0]["name"] = json!("hello");
    search["items"][0]["full_name"] = json!("octo/hello");
    search["items"][0]["stargazers_count"] = json!(4242);
    search["items"].as_array_mut().unwrap().truncate(1);

    let api = actions_of(&contract, vec![Ok(search.to_string())]);
    read(&api, "/search/repositories?q=x").await;
    let (source, found) = read(&api, "/repos/octo/hello").await;
    assert_eq!(source, "state");
    assert_eq!(found["stargazers_count"], 4242);
}

/// With consistency off, a create's mention is not stored.
#[tokio::test]
async fn consistency_off_stores_no_create_mention() {
    let api = actions_off(
        WORLD,
        vec![
            order("o2", json!({"id": "c1", "name": "Ana"})),
            Ok(json!({"id": "c1", "name": "Generated"}).to_string()),
        ],
    );
    let (status, _, _) = api.call("POST", "/orders", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (source, customer) = read(&api, "/customers/c1").await;
    assert_ne!(source, "state", "{customer}");
}

/// A faker create standing in for a failed LLM call stores no mention.
#[tokio::test]
async fn a_faker_create_stores_no_mention() {
    let api = actions_of(WORLD, vec![]);
    let (status, _, created) = api.call("POST", "/orders", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["customer"]["id"].as_str().unwrap().to_string();
    let (source, customer) = read(&api, &format!("/customers/{id}")).await;
    assert_ne!(source, "state", "{customer}");
}

/// A create's mention its type refuses does not take the id from a later
/// valid one.
#[tokio::test]
async fn a_refused_create_mention_does_not_take_the_id() {
    let contract = WORLD.replace(
        "customer: {$ref: \"#/components/schemas/Customer\"}",
        "customer: {type: object, properties: {id: {type: string}, name: {type: string}}}",
    );
    let api = actions_of(
        &contract,
        vec![
            order("o2", json!({"id": "c1"})),
            order("o3", json!({"id": "c1", "name": "Ana"})),
        ],
    );
    let (status, _, _) = api.call("POST", "/orders", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    read(&api, "/orders/o3").await;
    let (source, customer) = read(&api, "/customers/c1").await;
    assert_eq!(source, "state");
    assert_eq!(customer["name"], "Ana");
}

/// Issues read by their number, with an `id` of their own, as on GitHub. The
/// delete's answer shows only the `id`, and the lock's a record of its own.
const ISSUES: &str = r##"
openapi: 3.0.0
info: {title: Issues, version: "1"}
paths:
  /issues:
    get:
      operationId: listIssues
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Issue"}}}}}
  /issues/{issue_number}:
    get:
      operationId: getIssue
      parameters: [{name: issue_number, in: path, required: true, schema: {type: integer}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
    delete:
      operationId: deleteIssue
      parameters: [{name: issue_number, in: path, required: true, schema: {type: integer}}]
      responses:
        "200": {description: gone, content: {application/json: {schema: {type: object, properties: {id: {type: integer}, deleted: {type: boolean}}}}}}
  /issues/{issue_number}/lock:
    post:
      operationId: lockIssue
      parameters: [{name: issue_number, in: path, required: true, schema: {type: integer}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {issue_number: {type: integer}, locked: {type: boolean}}}}}}
components:
  schemas:
    Issue:
      type: object
      required: [id, number, title]
      properties: {id: {type: integer}, number: {type: integer}, title: {type: string}}
"##;

/// An issue a list seeded and a delete by number removes stays gone. The
/// delete's answer declares `number`, which is what finds the listed issue.
#[tokio::test]
async fn a_deleted_issue_stays_gone_after_a_read_and_a_list() {
    let issue = json!({"id": 999, "number": 5, "title": "a"});
    let contract = ISSUES.replace(
        "{type: object, properties: {id: {type: integer}, deleted: {type: boolean}}}",
        "{$ref: \"#/components/schemas/Issue\"}",
    );
    let api = actions_of(
        &contract,
        vec![
            Ok(issue.to_string()),
            Ok(json!([issue]).to_string()),
            Ok(issue.to_string()),
            Ok(json!({"id": 1000, "number": 5, "title": "regenerated"}).to_string()),
        ],
    );
    read(&api, "/issues/5").await;
    read(&api, "/issues").await;
    let (status, _, _) = api.call("DELETE", "/issues/5", None).await;
    assert!(status.is_success(), "{status}");
    let (source, body) = read(&api, "/issues/5").await;
    assert_ne!(source, "state", "deleted issue read back: {body}");
    assert_eq!(body["title"], "regenerated");
}

/// A read by number is not stored: its answer's `id` cannot make it another
/// issue's answer.
#[tokio::test]
async fn a_read_by_number_does_not_answer_for_another_issue() {
    let api = actions_of(
        ISSUES,
        vec![
            Ok(json!({"id": 5, "number": 7, "title": "seven"}).to_string()),
            Ok(json!({"id": 50, "number": 5, "title": "five"}).to_string()),
        ],
    );
    read(&api, "/issues/7").await;
    let (source, issue) = read(&api, "/issues/5").await;
    assert_ne!(source, "state");
    assert_eq!(issue["number"], 5);
    assert_eq!(issue["title"], "five");
}

/// A read by number stores nothing a delete could leave behind.
#[tokio::test]
async fn a_read_by_number_leaves_nothing_after_a_delete() {
    let issue = json!({"id": 999, "number": 5, "title": "a"});
    let api = actions_of(
        ISSUES,
        vec![
            Ok(issue.to_string()),
            Ok(json!({"id": 999, "deleted": true}).to_string()),
            Ok(json!({"id": 1000, "number": 5, "title": "regenerated"}).to_string()),
        ],
    );
    read(&api, "/issues/5").await;
    let (status, _, _) = api.call("DELETE", "/issues/5", None).await;
    assert!(status.is_success(), "{status}");
    let (source, body) = read(&api, "/issues/5").await;
    assert_ne!(source, "state", "deleted issue read back: {body}");
}

/// An action's record takes the path value in the member named for its
/// parameter, and gains no member it does not declare.
#[tokio::test]
async fn a_lock_record_keeps_its_own_members() {
    let api = actions_of(
        ISSUES,
        vec![Ok(json!({"issue_number": 7, "locked": true}).to_string())],
    );
    let (status, _, record) = api.call("POST", "/issues/5/lock", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(record, json!({"issue_number": 5, "locked": true}));
}

/// Jira's `issueIdOrKey`: an `id` in the path answers under that `id`, whether
/// or not `key` could hold it.
const JIRA: &str = r##"
openapi: 3.0.0
info: {title: Jira, version: "1"}
paths:
  /issue/{issueIdOrKey}:
    get:
      operationId: getIssue
      parameters: [{name: issueIdOrKey, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
components:
  schemas:
    Issue:
      type: object
      required: [id, key]
      properties: {id: {type: string}, key: {type: string, pattern: "^[A-Z]+-[0-9]+$"}}
"##;

#[tokio::test]
async fn a_jira_read_answers_the_requested_id() {
    for contract in [
        JIRA.to_string(),
        JIRA.replace(", pattern: \"^[A-Z]+-[0-9]+$\"", ""),
    ] {
        let api = actions_of(
            &contract,
            vec![Ok(json!({"id": "777", "key": "ABC-1"}).to_string())],
        );
        let (_, issue) = read(&api, "/issue/10001").await;
        assert_eq!(issue["id"], "10001", "{issue}");
        assert_eq!(issue["key"], "ABC-1", "{issue}");
    }
}

/// Orders whose answers mention receipts, keyed by a `uuid` only.
const RECEIPTS: &str = r##"
openapi: 3.0.0
info: {title: Receipts, version: "1"}
paths:
  /orders:
    post:
      operationId: createOrder
      responses:
        "201": {description: ok, content: {application/json: {schema: {type: object, required: [id], properties: {id: {type: string}, receipt: {type: object, properties: {uuid: {type: string}}}}}}}}
  /receipts:
    get:
      operationId: listReceipts
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {receipts: {type: array, items: {$ref: "#/components/schemas/Receipt"}}}}}}}
  /receipts/{uuid}:
    get:
      operationId: getReceipt
      parameters: [{name: uuid, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Receipt"}}}}
components:
  schemas:
    Receipt:
      type: object
      required: [uuid, total]
      properties: {uuid: {type: string}, total: {type: integer}}
"##;

/// With consistency off, a create's mention keyed by no `id` is not stored.
#[tokio::test]
async fn consistency_off_stores_no_unkeyed_mention() {
    let api = actions_off(
        RECEIPTS,
        vec![
            Ok(json!({"id": "o1", "receipt": {"uuid": "r1", "total": 3}}).to_string()),
            Ok(json!({"uuid": "r1", "total": 9}).to_string()),
        ],
    );
    let (status, _, _) = api.call("POST", "/orders", None).await;
    assert_eq!(status, StatusCode::CREATED);
    let (source, receipt) = read(&api, "/receipts/r1").await;
    assert_ne!(source, "state", "{receipt}");
}

/// A faker create stores no mention keyed by no `id`.
#[tokio::test]
async fn a_faker_create_stores_no_unkeyed_mention() {
    let api = actions_of(RECEIPTS, vec![]);
    let (status, _, created) = api.call("POST", "/orders", None).await;
    assert_eq!(status, StatusCode::CREATED);
    let Some(id) = created["receipt"]["uuid"].as_str() else {
        return;
    };
    let (source, _) = read(&api, &format!("/receipts/{id}")).await;
    assert_ne!(source, "state");
}

/// A mention its type refuses does not overwrite a stored read.
#[tokio::test]
async fn a_refused_unkeyed_mention_leaves_a_stored_read() {
    let api = actions_of(
        RECEIPTS,
        vec![
            Ok(json!({"uuid": "r1", "total": 9}).to_string()),
            Ok(json!({"id": "o1", "receipt": {"uuid": "r1"}}).to_string()),
        ],
    );
    let (_, first) = read(&api, "/receipts/r1").await;
    let (status, _, _) = api.call("POST", "/orders", None).await;
    assert_eq!(status, StatusCode::CREATED);
    let (source, again) = read(&api, "/receipts/r1").await;
    assert_eq!(source, "state");
    assert_eq!(again, first);
}

/// A by-id read with a query is one view of the resource, not the resource:
/// each query keeps its own answer.
#[tokio::test]
async fn a_read_with_a_query_is_not_stored() {
    let api = actions_of(
        WORLD,
        vec![
            Ok(json!({"id": "c1", "name": "One"}).to_string()),
            Ok(json!({"id": "c1", "name": "Two"}).to_string()),
        ],
    );
    let (_, first) = read(&api, "/customers/c1?view=a").await;
    assert_eq!(first["name"], "One");
    let (source, second) = read(&api, "/customers/c1?view=b").await;
    assert_ne!(source, "state");
    assert_eq!(second["name"], "Two");
}
