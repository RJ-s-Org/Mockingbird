//! A create that repeats a record of its own type by identity, or whose effects reach
//! one: a second `POST /customers` asking for the client-chosen `id` of a stored
//! customer, and a second payment whose answer copies its request onto the first. The
//! create is shown the record it repeats and offered its declared refusal; answered as
//! a success anyway, it stores nothing over the record. A create's effects never change
//! another record of its own type that its request does not name. Driven over HTTP with
//! a scripted LLM, asserting the prompts it was sent and what the store holds.
//!
//! Step 6 deletes these tests: they check complete()'s create internals (seeded creates).
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Customers created under an id the client chooses, whose create declares a 400 (and
/// that a `PUT` the tests make a create upserts), payments whose ids the API mints, and
/// labels a repository names by a name its create chooses.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Billing, version: "1"}
paths:
  /customers:
    post:
      operationId: createCustomer
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {id: {type: string}, first_name: {type: string}, email: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}
        "400": {description: The request is invalid., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /customers/{customer_id}:
    get:
      operationId: getCustomer
      parameters: [{name: customer_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}
  /payments:
    post:
      operationId: createPayment
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {amount: {type: number}, description: {type: string}, reply_to: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}
  /payments/{payment_id}:
    get:
      operationId: getPayment
      parameters: [{name: payment_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}
  /ledgers/{ledger_id}:
    get:
      operationId: getLedger
      parameters: [{name: ledger_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ledger"}}}}
  /ledgers/{ledger_id}/payments:
    post:
      operationId: createLedgerPayment
      parameters: [{name: ledger_id, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {amount: {type: number}, description: {type: string}, quantity: {type: integer}, parent_id: {type: string}, is_default: {type: boolean}, in_reply_to: {type: integer}, fields: {type: object, properties: {parent: {type: object, properties: {id: {type: string}}}}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}
  /profiles:
    put:
      operationId: upsertCustomer
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {id: {type: string}, first_name: {type: string}, email: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}
  /repos/{repo}/labels:
    post:
      operationId: createLabel
      parameters: [{name: repo, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}, color: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Label"}}}}
        "422": {description: Validation failed., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /repos/{repo}/labels/{name}:
    get:
      operationId: getLabel
      parameters:
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: name, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Label"}}}}
components:
  schemas:
    Customer:
      type: object
      required: [id, first_name]
      properties:
        id: {type: string}
        first_name: {type: string}
        email: {type: string}
    Payment:
      type: object
      required: [id, amount]
      properties:
        id: {type: string}
        amount: {type: number}
        description: {type: string}
        reply_to: {type: string}
        replies: {type: integer}
        quantity: {type: integer}
        parent_id: {type: string}
        is_default: {type: boolean}
        in_reply_to: {type: integer}
        fields: {type: object, properties: {parent: {type: object, properties: {id: {type: string}}}}}
    Label:
      type: object
      required: [name, color]
      properties:
        name: {type: string}
        color: {type: string}
    Ledger:
      type: object
      required: [id, total]
      properties:
        id: {type: string}
        total: {type: number}
    Error:
      type: object
      required: [message]
      properties: {message: {type: string}}
"##;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<dyn StateStore>,
}

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "default".into(),
    }
}

fn harness(contract: &str, script: Vec<Value>) -> Harness {
    harness_with(contract, script, |_| {})
}

/// A harness over `contract` compiled, then changed by `change`.
fn harness_with(
    contract: &str,
    script: Vec<Value>,
    change: impl FnOnce(&mut mb_mcm::Bundle),
) -> Harness {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut bundle = compile(contract.as_bytes()).unwrap();
    change(&mut bundle);
    let rt = MockRuntime::new(
        "m",
        Arc::new(bundle),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store),
    );
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

impl Harness {
    async fn send(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        for (name, value) in headers {
            req = req.header(*name, *value);
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

    /// [`Self::send`] of a create, seeded: a seeded create takes the generated path,
    /// which stores the record the model answered under the id it chose, as these
    /// tests expect.
    async fn seeded(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut headers = headers.to_vec();
        headers.push(crate::support::SEED);
        self.send(method, uri, body, &headers).await
    }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.seeded("POST", uri, Some(body), &[]).await
    }

    /// The user message of the `n`th LLM call.
    fn prompt(&self, n: usize) -> String {
        let requests = self.provider.requests();
        requests[n].messages.last().unwrap().content.clone()
    }

    fn format_schema(&self, n: usize) -> Value {
        match &self.provider.requests()[n].response_format {
            mb_llm::ResponseFormat::JsonSchema { schema, .. } => schema.clone(),
            other => panic!("{other:?}"),
        }
    }

    async fn held(&self, t: &str, id: &str) -> Value {
        crate::support::find_stored(&*self.store, &session(), t, id)
            .await
            .map_or(Value::Null, |r| r.data)
    }

    async fn count(&self, t: &str) -> usize {
        self.store
            .export(&session())
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.resource_type == t)
            .count()
    }
}

/// The text between a block's tags, or `None` without one.
fn block<'a>(prompt: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = prompt.find(&open)? + open.len();
    let end = prompt.find(&format!("</{tag}>"))?;
    Some(&prompt[start..end])
}

fn customer(id: &str, first_name: &str, email: &str) -> Value {
    json!({"id": id, "first_name": first_name, "email": email})
}

const ANA: &str = "cust_000001";

/// A second create asking for the id of a stored customer is shown that customer under
/// its identity member, and offered the declared 400; refused, it stores nothing.
#[tokio::test]
async fn a_create_asking_for_a_taken_id_is_shown_its_holder_and_offered_the_refusal() {
    let h = harness(
        CONTRACT,
        vec![
            customer(ANA, "Ana", "ana@example.com"),
            json!({"effects": [], "response": null, "refusal": 400}),
        ],
    );
    let (status, _) = h
        .post("/customers", customer(ANA, "Ana", "ana@example.com"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!h.prompt(0).contains("refusal"), "{}", h.prompt(0));

    let (status, body) = h
        .post("/customers", customer(ANA, "Rui", "rui@example.com"))
        .await;
    let prompt = h.prompt(1);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body} {prompt}");
    let same = block(&prompt, "same_values").unwrap_or_default();
    assert!(
        same.contains(&format!("\"id\": \"{ANA}\"")) && same.contains("\"member\": \"id\""),
        "{prompt}"
    );
    assert!(prompt.contains("its type's identity"), "{prompt}");
    assert_eq!(
        h.format_schema(1)["properties"]["refusal"],
        json!({"type": "integer", "enum": [400]})
    );
    assert_eq!(h.held("customers", ANA).await["first_name"], "Ana");
    assert_eq!(h.count("customers").await, 1);
}

/// Answered as a success for the taken id anyway, the create does not replace the stored
/// customer, and answers the declared refusal it was offered, not the model's body: a
/// read still serves Ana, and a later create with Rui's email is shown no holder.
#[tokio::test]
async fn a_success_for_a_taken_id_never_replaces_the_stored_record() {
    let h = harness(
        CONTRACT,
        vec![
            customer(ANA, "Ana", "ana@example.com"),
            json!({"effects": [], "response": customer(ANA, "Rui", "rui@example.com")}),
            customer("cust_000002", "Rui", "rui@example.com"),
        ],
    );
    h.post("/customers", customer(ANA, "Ana", "ana@example.com"))
        .await;
    let (status, body) = h
        .post("/customers", customer(ANA, "Rui", "rui@example.com"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].is_string(), "the contract's 400: {body}");
    assert!(!body.to_string().contains("Rui"), "{body}");
    assert_eq!(
        h.held("customers", ANA).await,
        customer(ANA, "Ana", "ana@example.com"),
        "the stored record is kept"
    );
    assert_eq!(h.count("customers").await, 1);
    let (status, read) = h.send("GET", &format!("/customers/{ANA}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["first_name"], "Ana", "{read}");

    // Rui's email is no stored customer's: a create with it is shown no holder.
    let (status, _) = h
        .post(
            "/customers",
            customer("cust_000002", "Rui", "rui@example.com"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let prompt = h.prompt(2);
    assert!(
        block(&prompt, "same_values").is_none_or(|s| !s.contains(ANA)),
        "{prompt}"
    );
    assert_eq!(h.count("customers").await, 2);
}

/// Ordinary creates are as they were: a create asking for a new id, or for none, is
/// stored, and offered no refusal.
#[tokio::test]
async fn creates_of_new_ids_are_stored_as_before() {
    let h = harness(
        CONTRACT,
        vec![
            customer(ANA, "Ana", "ana@example.com"),
            customer("cust_000002", "Rui", "rui@example.com"),
            customer("cust_000003", "Eva", "eva@example.com"),
        ],
    );
    h.post("/customers", customer(ANA, "Ana", "ana@example.com"))
        .await;
    h.post(
        "/customers",
        customer("cust_000002", "Rui", "rui@example.com"),
    )
    .await;
    h.post(
        "/customers",
        json!({"first_name": "Eva", "email": "eva@example.com"}),
    )
    .await;
    for n in 1..3 {
        assert!(!h.prompt(n).contains("same_values"), "{}", h.prompt(n));
    }
    assert_eq!(
        h.held("customers", "cust_000002").await["first_name"],
        "Rui"
    );
    assert_eq!(
        h.held("customers", "cust_000003").await["first_name"],
        "Eva"
    );
    assert_eq!(h.count("customers").await, 3);
}

/// A create carrying a declared idempotency key may be a retry the API answers as the
/// first: it is offered no refusal, and is shown nothing under its identity.
#[tokio::test]
async fn an_idempotent_retry_of_a_client_chosen_id_is_as_before() {
    let contract = CONTRACT.replacen(
        "      operationId: createCustomer\n",
        "      operationId: createCustomer\n      parameters:\n        - {name: Idempotency-Key, in: header, schema: {type: string}}\n",
        1,
    );
    assert_ne!(contract, CONTRACT);
    let h = harness(
        &contract,
        vec![
            customer(ANA, "Ana", "ana@example.com"),
            customer(ANA, "Ana", "ana.retry@example.com"),
        ],
    );
    let key = [("Idempotency-Key", "key-000001")];
    let body = customer(ANA, "Ana", "ana@example.com");
    h.seeded("POST", "/customers", Some(body.clone()), &key)
        .await;
    let (status, _) = h.seeded("POST", "/customers", Some(body), &key).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!h.prompt(1).contains("refusal"), "{}", h.prompt(1));
    assert!(!h.prompt(1).contains("same_values"), "{}", h.prompt(1));
    assert_eq!(h.count("customers").await, 1);
}

fn payment(id: &str, amount: f64, description: &str) -> Value {
    json!({"id": id, "amount": amount, "description": description})
}

/// A second payment's answer whose effects copy its request onto the first payment
/// changes nothing on it: a create makes its own record, not a sibling's. The effect is
/// told rejected, and the new payment is stored.
#[tokio::test]
async fn a_create_s_effect_never_rewrites_a_sibling_it_does_not_name() {
    let h = harness(
        CONTRACT,
        vec![
            payment("tr_000001", 25.0, "Order X-1"),
            json!({
                "effects": [{"type": "payments", "id": "tr_000001", "set": {"amount": 9.99, "description": "Order X-2"}}],
                "response": payment("tr_000002", 9.99, "Order X-2"),
            }),
        ],
    );
    h.post(
        "/ledgers/lg_000001/payments",
        json!({"amount": 25.0, "description": "Order X-1"}),
    )
    .await;
    let (status, body) = h
        .post(
            "/ledgers/lg_000001/payments",
            json!({"amount": 9.99, "description": "Order X-2"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body} {}", h.prompt(1));
    let first = h.held("payments", "tr_000001").await;
    assert_eq!(first["amount"], 25.0, "{first}");
    assert_eq!(first["description"], "Order X-1", "{first}");
    assert_eq!(
        h.held("payments", "tr_000002").await["amount"],
        9.99,
        "{body} {}",
        h.prompt(1)
    );
}

/// A create may still change a record of its own type its request names (the payment
/// it replies to), and a record of another type (the ledger its path names).
#[tokio::test]
async fn a_create_s_effects_on_what_it_names_still_apply() {
    let h = harness(
        CONTRACT,
        vec![
            payment("tr_000001", 25.0, "Order X-1"),
            json!({
                "effects": [{"type": "payments", "id": "tr_000001", "set": {"replies": 1}}],
                "response": payment("tr_000002", 1.0, "Reply"),
            }),
            json!({"id": "lg_000001", "total": 0}),
            json!({
                "effects": [{"type": "ledgers", "id": "lg_000001", "set": {"total": 5}}],
                "response": payment("tr_000003", 5.0, "Ledger"),
            }),
        ],
    );
    h.post(
        "/payments",
        json!({"amount": 25.0, "description": "Order X-1"}),
    )
    .await;
    h.post(
        "/payments",
        json!({"amount": 1.0, "description": "Reply", "reply_to": "tr_000001"}),
    )
    .await;
    assert_eq!(
        h.held("payments", "tr_000001").await["replies"],
        1,
        "{}",
        h.prompt(1)
    );
    h.send("GET", "/ledgers/lg_000001", None, &[]).await;
    let (status, _) = h
        .post(
            "/ledgers/lg_000001/payments",
            json!({"amount": 5.0, "description": "Ledger"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{}", h.prompt(3));
    assert_eq!(
        h.held("ledgers", "lg_000001").await["total"],
        5,
        "{}",
        h.prompt(3)
    );
}

/// With no refusal declared, a success for a taken id answers the held record, as an
/// idempotent create would: the answer agrees with the store, never the model's
/// replacement.
#[tokio::test]
async fn a_success_for_a_taken_id_with_no_refusal_answers_the_held_record() {
    let contract = CONTRACT.replacen(
        "        \"400\": {description: The request is invalid., content: {application/json: {schema: {$ref: \"#/components/schemas/Error\"}}}}\n",
        "",
        1,
    );
    assert_ne!(contract, CONTRACT);
    let h = harness(
        &contract,
        vec![
            customer(ANA, "Ana", "ana@example.com"),
            json!({
                "effects": [{"type": "customers", "id": ANA, "set": {"email": "rui@example.com"}}],
                "response": customer(ANA, "Rui", "rui@example.com"),
            }),
        ],
    );
    h.post("/customers", customer(ANA, "Ana", "ana@example.com"))
        .await;
    let (status, body) = h
        .post("/customers", customer(ANA, "Rui", "rui@example.com"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body} {}", h.prompt(1));
    assert!(h.prompt(1).contains("\"effects\""), "{}", h.prompt(1));
    assert_eq!(body, customer(ANA, "Ana", "ana@example.com"));
    assert_eq!(
        h.held("customers", ANA).await,
        customer(ANA, "Ana", "ana@example.com")
    );
    assert_eq!(h.count("customers").await, 1);
}

/// Declaring several refusal statuses, a repeated create answered as a success answers
/// the `409`, before the `422`, never the `400`.
#[tokio::test]
async fn a_repeat_answers_the_409_first() {
    let contract = CONTRACT.replacen(
        "        \"400\": {description: The request is invalid., content: {application/json: {schema: {$ref: \"#/components/schemas/Error\"}}}}\n",
        "        \"400\": {description: The request is invalid., content: {application/json: {schema: {$ref: \"#/components/schemas/Error\"}}}}\n        \"422\": {description: Validation failed., content: {application/json: {schema: {$ref: \"#/components/schemas/Error\"}}}}\n        \"409\": {description: Conflict., content: {application/json: {schema: {$ref: \"#/components/schemas/Error\"}}}}\n",
        1,
    );
    assert_ne!(contract, CONTRACT);
    let h = harness(
        &contract,
        vec![
            customer(ANA, "Ana", "ana@example.com"),
            json!({"effects": [], "response": customer(ANA, "Rui", "rui@example.com")}),
        ],
    );
    h.post("/customers", customer(ANA, "Ana", "ana@example.com"))
        .await;
    let (status, body) = h
        .post("/customers", customer(ANA, "Rui", "rui@example.com"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(h.held("customers", ANA).await["first_name"], "Ana");
}

/// A forced status decides the answer: a repeated create keeps it, is not refused, and
/// answers the held record, which it does not replace.
#[tokio::test]
async fn a_forced_status_is_kept_on_a_repeat() {
    let h = harness(
        CONTRACT,
        vec![
            customer(ANA, "Ana", "ana@example.com"),
            json!({"effects": [], "response": customer(ANA, "Rui", "rui@example.com")}),
        ],
    );
    h.post("/customers", customer(ANA, "Ana", "ana@example.com"))
        .await;
    let (status, body) = h
        .send(
            "POST",
            "/customers",
            Some(customer(ANA, "Rui", "rui@example.com")),
            &[("x-mockingbird-status", "200")],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["first_name"], "Ana", "{body}");
    assert_eq!(h.held("customers", ANA).await["first_name"], "Ana");
}

/// A label's name is taken only in its own repository: `bug` in another one is a new
/// label, shown nothing and stored, and refused nowhere.
#[tokio::test]
async fn a_taken_id_counts_only_under_the_same_parent() {
    let label = |color: &str| json!({"name": "bug", "color": color});
    let h = harness(
        CONTRACT,
        vec![
            label("red"),
            json!({"effects": [], "response": label("blue")}),
            label("blue"),
        ],
    );
    let (status, _) = h.post("/repos/a/labels", label("red")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = h.post("/repos/b/labels", label("blue")).await;
    let prompt = h.prompt(1);
    assert_eq!(status, StatusCode::CREATED, "{body} {prompt}");
    assert!(!prompt.contains("same_values"), "{prompt}");
    assert!(!prompt.contains("\"refusal\""), "{prompt}");
    assert_eq!(body["color"], "blue", "{body}");
    // Not stored over repository a's label, which still reads from state: each is
    // stored under its repository.
    assert_eq!(h.held("labels", "a\u{1f}bug").await["color"], "red");
    assert_eq!(h.held("labels", "b\u{1f}bug").await["color"], "blue");
    let calls = h.provider.requests().len();
    let (status, read) = h.send("GET", "/repos/a/labels/bug", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["color"], "red", "{read}");
    assert_eq!(h.provider.requests().len(), calls, "served from state");
}

/// An `id` the request's schema does not declare is no client's choice: a payment sent
/// with the first one's id is a new payment under a fresh id, and the first is kept.
#[tokio::test]
async fn an_undeclared_body_id_is_minted_over() {
    let h = harness(
        CONTRACT,
        vec![
            payment("tr_000001", 25.0, "Order X-1"),
            payment("tr_000001", 7.0, "Order X-2"),
        ],
    );
    h.post(
        "/payments",
        json!({"amount": 25.0, "description": "Order X-1"}),
    )
    .await;
    let (status, body) = h
        .post(
            "/payments",
            json!({"id": "tr_000001", "amount": 7.0, "description": "Order X-2"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_ne!(body["id"], "tr_000001", "{body}");
    assert_eq!(body["amount"], 7.0, "{body}");
    assert_eq!(h.held("payments", "tr_000001").await["amount"], 25.0);
    assert_eq!(h.count("payments").await, 2);
}

/// An upsert by `PUT`, even one a bundle declares a create, replaces what it names, as
/// it always did.
#[tokio::test]
async fn a_put_create_still_upserts() {
    let h = harness_with(
        CONTRACT,
        vec![
            customer(ANA, "Ana", "ana@example.com"),
            json!({"effects": [], "response": customer(ANA, "Rui", "rui@example.com")}),
        ],
        |bundle| {
            let op = bundle
                .operations
                .iter_mut()
                .find(|op| op.id == "upsertCustomer")
                .unwrap();
            op.kind = mb_mcm::OperationKind::Command(mb_mcm::Effect::Create);
            op.resource_type = Some("customers".into());
            op.identity = Some(mb_mcm::Identity {
                path_param: None,
                field: "id".into(),
                input_field: None,
                path_field: None,
                record: None,
            });
        },
    );
    h.post("/customers", customer(ANA, "Ana", "ana@example.com"))
        .await;
    let (status, body) = h
        .send(
            "PUT",
            "/profiles",
            Some(customer(ANA, "Rui", "rui@example.com")),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.held("customers", ANA).await["first_name"], "Rui");
}

/// A create may change a sibling its request names by a number or under a nested
/// identity member, repeating the very member that names it, and may change one it
/// does not name in a way the request does not say (the old default stops being the
/// default).
#[tokio::test]
async fn a_create_s_effects_on_named_or_unrepeated_siblings_apply() {
    let h = harness(
        CONTRACT,
        vec![
            payment("1234567", 25.0, "Order X-1"),
            payment("tr_000001", 30.0, "Order X-2"),
            json!({
                "effects": [{"type": "payments", "id": "1234567", "set": {"in_reply_to": 1_234_567, "replies": 1}}],
                "response": payment("tr_000002", 1.0, "Reply"),
            }),
            json!({
                "effects": [{"type": "payments", "id": "tr_000001", "set": {"fields": {"parent": {"id": "tr_000001"}}, "replies": 2}}],
                "response": payment("tr_000003", 2.0, "Child"),
            }),
            json!({
                "effects": [{"type": "payments", "id": "tr_000003", "set": {"is_default": false}}],
                "response": payment("tr_000004", 3.0, "Default"),
            }),
        ],
    );
    let uri = "/ledgers/lg_000001/payments";
    h.post(uri, json!({"amount": 25.0, "description": "Order X-1"}))
        .await;
    h.post(uri, json!({"amount": 30.0, "description": "Order X-2"}))
        .await;
    h.post(
        uri,
        json!({"amount": 1.0, "description": "Reply", "in_reply_to": 1_234_567}),
    )
    .await;
    assert_eq!(
        h.held("payments", "1234567").await["replies"],
        1,
        "{}",
        h.prompt(2)
    );
    h.post(
        uri,
        json!({"amount": 2.0, "description": "Child", "fields": {"parent": {"id": "tr_000001"}}}),
    )
    .await;
    assert_eq!(
        h.held("payments", "tr_000001").await["replies"],
        2,
        "{}",
        h.prompt(3)
    );
    h.post(
        uri,
        json!({"amount": 3.0, "description": "Default", "is_default": true}),
    )
    .await;
    assert_eq!(
        h.held("payments", "tr_000003").await["is_default"],
        false,
        "{}",
        h.prompt(4)
    );
}

/// A create answering its record in an envelope (`data`) beside another member of its
/// own type keyed by the taken id stores neither over the held record, and serves the
/// held record in the envelope.
#[tokio::test]
async fn a_repeat_s_wrapped_record_is_not_stored_either() {
    let envelope = "{type: object, properties: {data: {$ref: \"#/components/schemas/Customer\"}, customer: {$ref: \"#/components/schemas/Customer\"}}}";
    let contract = CONTRACT
        .replacen(
            "        \"200\": {description: ok, content: {application/json: {schema: {$ref: \"#/components/schemas/Customer\"}}}}\n        \"400\": {description: The request is invalid., content: {application/json: {schema: {$ref: \"#/components/schemas/Error\"}}}}\n",
            &format!("        \"200\": {{description: ok, content: {{application/json: {{schema: {envelope}}}}}}}\n"),
            1,
        )
        .replacen(
            "  /customers/{customer_id}:\n",
            "  /customers/{customer_id}/list:\n    get:\n      operationId: listCustomers\n      parameters: [{name: customer_id, in: path, required: true, schema: {type: string}}]\n      responses:\n        \"200\": {description: ok, content: {application/json: {schema: {type: object, properties: {customers: {type: array, items: {$ref: \"#/components/schemas/Customer\"}}}}}}}\n  /customers/{customer_id}:\n",
            1,
        );
    assert!(contract.contains("customer: {$ref") && contract.contains("listCustomers"));
    let ana = customer(ANA, "Ana", "ana@example.com");
    let rui = customer(ANA, "Rui", "rui@example.com");
    let h = harness_with(
        &contract,
        vec![
            json!({"data": ana, "customer": ana}),
            json!({"effects": [], "response": {"data": rui, "customer": rui}}),
        ],
        |bundle| {
            let op = bundle
                .operations
                .iter_mut()
                .find(|op| op.id == "createCustomer")
                .unwrap();
            op.identity = Some(mb_mcm::Identity {
                path_param: None,
                field: "id".into(),
                input_field: None,
                path_field: None,
                record: Some(mb_mcm::RecordAt::Member("data".into())),
            });
        },
    );
    h.post("/customers", ana.clone()).await;
    assert_eq!(h.held("customers", ANA).await, ana);
    let (status, body) = h.post("/customers", rui.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body} {}", h.prompt(1));
    assert_eq!(body["data"], ana, "{body}");
    assert_eq!(h.held("customers", ANA).await, ana, "{}", h.prompt(1));
}

/// Mollie's shape with a small id: a second payment whose amount equals the first
/// payment's id, and whose answer copies its request onto that payment, leaves it as it
/// was. The amount naming it excuses nothing beside the description copied too.
#[tokio::test]
async fn a_small_id_a_copied_amount_equals_is_no_name() {
    let h = harness(
        CONTRACT,
        vec![
            payment("1", 25.0, "Order X-1"),
            json!({
                "effects": [{"type": "payments", "id": "1", "set": {"amount": 1, "description": "Order X-2"}}],
                "response": payment("2", 1.0, "Order X-2"),
            }),
        ],
    );
    let uri = "/ledgers/lg_000001/payments";
    h.post(uri, json!({"amount": 25.0, "description": "Order X-1"}))
        .await;
    let (status, body) = h
        .post(uri, json!({"amount": 1, "description": "Order X-2"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body} {}", h.prompt(1));
    assert!(h.prompt(1).contains("\"effects\""), "{}", h.prompt(1));
    let first = h.held("payments", "1").await;
    assert_eq!(first["amount"], 25.0, "{first}");
    assert_eq!(first["description"], "Order X-1", "{first}");
}

/// The amount alone copied onto payment `1`, whose id the amount equals, leaves it as it
/// was: the payment stores an amount, so the copy is no reference.
#[tokio::test]
async fn a_copied_amount_alone_never_rewrites_a_sibling() {
    let h = harness(
        CONTRACT,
        vec![
            payment("1", 25.0, "Order X-1"),
            json!({
                "effects": [{"type": "payments", "id": "1", "set": {"amount": 1}}],
                "response": payment("2", 1.0, "Order X-2"),
            }),
        ],
    );
    let uri = "/ledgers/lg_000001/payments";
    h.post(uri, json!({"amount": 25.0, "description": "Order X-1"}))
        .await;
    let (status, body) = h
        .post(uri, json!({"amount": 1, "description": "Order X-2"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body} {}", h.prompt(1));
    assert!(h.prompt(1).contains("\"effects\""), "{}", h.prompt(1));
    assert_eq!(h.held("payments", "1").await["amount"], 25.0);
}

/// A nested object of the request copied onto the sibling it names lands none of its
/// members there (`note`), while the rest of the effect (`replies`) applies.
#[tokio::test]
async fn a_copied_nested_object_lands_none_of_its_members() {
    let h = harness(
        CONTRACT,
        vec![
            payment("tr_000001", 25.0, "Order X-1"),
            json!({
                "effects": [{"type": "payments", "id": "tr_000001", "set": {
                    "fields": {"parent": {"id": "tr_000001"}, "note": "copied"},
                    "replies": 2,
                }}],
                "response": payment("tr_000002", 2.0, "Child"),
            }),
        ],
    );
    let uri = "/ledgers/lg_000001/payments";
    h.post(uri, json!({"amount": 25.0, "description": "Order X-1"}))
        .await;
    let (status, body) = h
        .post(
            uri,
            json!({"amount": 2.0, "description": "Child", "fields": {"parent": {"id": "tr_000001"}, "note": "copied"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body} {}", h.prompt(1));
    let first = h.held("payments", "tr_000001").await;
    assert_eq!(first["replies"], 2, "{first} {}", h.prompt(1));
    assert!(first.get("fields").is_none(), "{first}");
}

/// A request value that happens to equal a sibling's id, or names it, is copied onto it
/// as much as any: a `quantity` of `1` does not land on payment `1`, and a `parent_id`
/// does not make the payment it names its own parent, though neither stores that member
/// yet.
#[tokio::test]
async fn a_value_naming_a_sibling_is_still_a_copy() {
    let first = json!({"id": "1", "amount": 25.0, "description": "Order X-1"});
    let parent = json!({"id": "tr_000001", "amount": 9.0, "description": "Parent"});
    let h = harness(
        CONTRACT,
        vec![
            first.clone(),
            parent.clone(),
            json!({
                "effects": [{"type": "payments", "id": "1", "set": {"quantity": 1}}],
                "response": payment("2", 2.0, "Order X-2"),
            }),
            json!({
                "effects": [{"type": "payments", "id": "tr_000001", "set": {"parent_id": "tr_000001"}}],
                "response": payment("3", 3.0, "Child"),
            }),
        ],
    );
    let uri = "/ledgers/lg_000001/payments";
    h.post(uri, json!({"amount": 25.0, "description": "Order X-1"}))
        .await;
    h.post(uri, json!({"amount": 9.0, "description": "Parent"}))
        .await;
    let (status, body) = h
        .post(
            uri,
            json!({"amount": 2, "quantity": 1, "description": "Order X-2"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body} {}", h.prompt(2));
    assert!(h.prompt(2).contains("\"effects\""), "{}", h.prompt(2));
    let held = h.held("payments", "1").await;
    assert!(held.get("quantity").is_none(), "{held}");
    let (status, body) = h
        .post(
            uri,
            json!({"amount": 3, "parent_id": "tr_000001", "description": "Child"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body} {}", h.prompt(3));
    let held = h.held("payments", "tr_000001").await;
    assert!(held.get("parent_id").is_none(), "{held}");
}

/// Templates of a server, whose create gives an alias and name and may name a layout
/// template by its alias; the API mints the integer id.
const TEMPLATES: &str = r##"
openapi: 3.0.0
info: {title: Mail, version: "1"}
paths:
  /servers/{server}/templates:
    post:
      operationId: createTemplate
      parameters: [{name: server, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {Alias: {type: string}, Name: {type: string}, TemplateType: {type: string, enum: [Standard, Layout]}, LayoutTemplate: {type: string, nullable: true}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}
  /templates/{TemplateId}:
    get:
      operationId: getTemplate
      parameters: [{name: TemplateId, in: path, required: true, schema: {type: integer}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}
components:
  schemas:
    Template:
      type: object
      required: [TemplateId, Alias, Name]
      properties:
        TemplateId: {type: integer}
        Alias: {type: string}
        Name: {type: string}
        TemplateType: {type: string, enum: [Standard, Layout]}
        LayoutTemplate: {type: string, nullable: true}
        Active: {type: boolean}
"##;

/// A create whose answer reuses a stored template's id and describes the new template
/// in an effect on the stored one (its alias and name, a `null` layout) leaves the
/// stored template as it was: the whole effect is the new record, its defaults too.
#[tokio::test]
async fn a_create_s_effect_describing_the_new_record_changes_no_sibling() {
    let template = |id: u64, alias: &str, kind: &str, layout: Value| json!({"TemplateId": id, "Alias": alias, "Name": alias, "TemplateType": kind, "LayoutTemplate": layout, "Active": true});
    let h = harness(
        TEMPLATES,
        vec![
            template(1001, "base-layout", "Layout", Value::Null),
            template(1002, "welcome", "Standard", json!("base-layout")),
            json!({
                "effects": [{"type": "templates", "id": "1002", "set": {
                    "Alias": "receipt", "Name": "receipt", "LayoutTemplate": null,
                    "TemplateType": "Standard", "Active": true,
                }}],
                "response": template(1002, "receipt", "Standard", Value::Null),
            }),
        ],
    );
    let uri = "/servers/srv_1/templates";
    h.post(
        uri,
        json!({"Alias": "base-layout", "Name": "base-layout", "TemplateType": "Layout"}),
    )
    .await;
    h.post(
        uri,
        json!({"Alias": "welcome", "Name": "welcome", "LayoutTemplate": "base-layout"}),
    )
    .await;
    let (status, body) = h
        .post(uri, json!({"Alias": "receipt", "Name": "receipt"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body} {}", h.prompt(2));
    let welcome = h.held("templates", "1002").await;
    assert_eq!(welcome["Alias"], "welcome", "{welcome} {}", h.prompt(2));
    assert_eq!(welcome["LayoutTemplate"], "base-layout", "{welcome}");
}
