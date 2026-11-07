//! Related writes: a create marks the records it is made under or refers to as
//! behind, so their next read catches up and is shown what was created since; an
//! update's query and form inputs count as named when its answer settles on the
//! stored record. Driven over HTTP with a scripted LLM, asserting the prompts sent
//! and how many calls it took.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{CachingStateStore, MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Where the write memory lives.
const MEMORY: &str = "__mockingbird.earlier_requests";

/// Payments with refunds made under them or naming them, messages answering
/// other messages by a value under no `_id` name, and cards updated by query or form.
const RELATED: &str = r##"
openapi: 3.0.0
info: {title: Related, version: "1"}
paths:
  /payments:
    post:
      operationId: createPayment
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}
  /payments/{payment_id}:
    get:
      operationId: getPayment
      parameters: [{name: payment_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}
  /payments/{payment_id}/refunds:
    post:
      operationId: refundPayment
      parameters: [{name: payment_id, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: false
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /refunds:
    post:
      operationId: createRefund
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /refunds/{refund_id}:
    get:
      operationId: getRefund
      parameters: [{name: refund_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /messages:
    post:
      operationId: postMessage
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Message"}}}}
  /messages/{message_id}:
    get:
      operationId: getMessage
      parameters: [{name: message_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Message"}}}}
  /cards/{card_id}:
    parameters: [{name: card_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getCard
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Card"}}}}
    put:
      operationId: updateCard
      parameters:
        - {name: name, in: query, schema: {type: string}}
        - {name: idList, in: query, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Card"}}}}
    patch:
      operationId: patchCard
      requestBody:
        required: true
        content:
          application/x-www-form-urlencoded:
            schema: {type: object, properties: {name: {type: string}, idList: {type: string}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Card"}}}}
components:
  schemas:
    Payment:
      type: object
      required: [id, amount, amount_refunded, status]
      properties:
        id: {type: string}
        amount: {type: integer}
        amount_refunded: {type: integer}
        status: {type: string}
    Refund:
      type: object
      required: [id, amount]
      properties:
        id: {type: string}
        amount: {type: integer}
        payment_id: {type: string}
    Message:
      type: object
      required: [id, text, reply_count]
      properties:
        id: {type: string}
        text: {type: string}
        reply_count: {type: integer}
        thread: {type: string}
    Card:
      type: object
      required: [id, name, idList, closed]
      properties:
        id: {type: string}
        name: {type: string}
        idList: {type: string}
        closed: {type: boolean}
"##;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<MemoryStore>,
}

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "default".into(),
    }
}

fn related(script: Vec<Value>) -> Harness {
    related_with(RELATED, script)
}

fn related_with(contract: &str, script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        CachingStateStore::wrap(
            Arc::clone(&store) as Arc<dyn StateStore>,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
    );
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

impl Harness {
    async fn send(&self, req: Request<Body>) -> (StatusCode, String, Value) {
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let source = resp
            .headers()
            .get("x-mockingbird-source")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, source, body)
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Value) {
        self.call_with(method, uri, body, &[]).await
    }

    /// [`Self::call`] of a create, seeded: a seeded create takes the generated path,
    /// which stores the record the model answered under the id it chose, as these
    /// tests expect.
    async fn seeded(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Value) {
        self.call_with(method, uri, body, &[crate::support::SEED])
            .await
    }

    async fn call_with(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, String, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        self.send(req).await
    }

    async fn form(&self, method: &str, uri: &str, form: &str) -> (StatusCode, String, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form.to_owned()))
            .unwrap();
        self.send(req).await
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    /// The user message of the `n`th LLM call.
    fn prompt(&self, n: usize) -> String {
        let requests = self.provider.requests();
        requests[n].messages.last().unwrap().content.clone()
    }

    /// The write memory's documents for records, by key.
    async fn memory(&self) -> Vec<StoredResource> {
        let mut held = self.store.list(&session(), MEMORY, 100).await.unwrap();
        held.retain(|doc| !doc.id.starts_with('#'));
        held
    }
}

/// The text between a block's tags, or `None` without one.
fn block<'a>(prompt: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = prompt.find(&open)? + open.len();
    let end = prompt.find(&format!("</{tag}>"))?;
    Some(&prompt[start..end])
}

fn payment(id: &str, refunded: i64, status: &str) -> Value {
    json!({"id": id, "amount": 100, "amount_refunded": refunded, "status": status})
}

/// An answer to a read of a record behind a create naming it: what it lists as
/// effects on the record, and a body that also says otherwise, which goes.
fn by_effects(id: &str, set: Value, response: Value) -> Value {
    json!({"effects": [{"type": "payments", "id": id, "set": set}], "response": response})
}

/// A refund naming its payment by value, made at a route that says nothing of the
/// payment: the payment's next read is told once, shown the refund as new, and
/// changes by the effects it lists only.
#[tokio::test]
async fn a_refund_naming_its_payment_brings_the_payments_read_up_to_date() {
    let h = related(vec![
        payment("pay_100001", 0, "captured"),
        json!({"id": "re_200001", "amount": 40, "payment_id": "pay_100001"}),
        by_effects(
            "pay_100001",
            json!({"amount_refunded": 40, "status": "partially_refunded"}),
            json!({"id": "pay_100001", "amount": 7, "amount_refunded": 40, "status": "partially_refunded"}),
        ),
    ]);
    h.seeded("POST", "/payments", Some(json!({"amount": 100})))
        .await;
    let (_, source, _) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "state");

    let (status, _, _) = h
        .seeded(
            "POST",
            "/refunds",
            Some(json!({"payment_id": "pay_100001", "amount": 40})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(h.calls(), 2);

    let (_, source, read) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "llm", "the payment is behind the refund");
    assert_eq!(read["amount_refunded"], 40);
    assert_eq!(read["status"], "partially_refunded");
    assert_eq!(read["amount"], 100, "what no effect lists stays as stored");
    assert_eq!(h.calls(), 3, "one call for the read");
    let prompt = h.prompt(2);
    assert!(prompt.contains(r#""effects""#), "{prompt}");
    let earlier = block(&prompt, "earlier_requests").expect("told the refund's request");
    assert!(earlier.contains("createRefund"), "{prompt}");
    let new = block(&prompt, "new_resources").expect("shown the refund as new");
    assert!(new.contains("re_200001"), "{prompt}");
    assert!(
        block(&prompt, "related_resources").is_none_or(|r| !r.contains("re_200001")),
        "the refund is not among what stays unchanged: {prompt}"
    );

    // Caught up: state answers, and the refund's own read needed nothing.
    let (_, source, again) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "state");
    assert_eq!(again, read);
    let (_, source, _) = h.call("GET", "/refunds/re_200001", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 3);
}

/// A refund made under its payment's path: the payment's read is told of it, as
/// before, and now shown the refund as new, not among what stays unchanged.
#[tokio::test]
async fn a_refund_under_a_payment_is_shown_to_it_as_new() {
    let h = related(vec![
        payment("pay_100001", 0, "captured"),
        json!({"id": "re_200001", "amount": 100}),
        payment("pay_100001", 100, "refunded"),
    ]);
    h.seeded("POST", "/payments", Some(json!({"amount": 100})))
        .await;
    h.call("GET", "/payments/pay_100001", None).await;
    let (status, _, _) = h.seeded("POST", "/payments/pay_100001/refunds", None).await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, source, read) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["status"], "refunded");
    assert_eq!(h.calls(), 3);
    let prompt = h.prompt(2);
    assert!(
        block(&prompt, "earlier_requests").is_some_and(|e| e.contains("refundPayment")),
        "{prompt}"
    );
    let new = block(&prompt, "new_resources").expect("shown the refund as new");
    assert!(new.contains("re_200001"), "{prompt}");
    assert!(
        block(&prompt, "related_resources").is_none_or(|r| !r.contains("re_200001")),
        "{prompt}"
    );
    let (_, source, _) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 3);
}

/// A reply names the message it answers by its id under a member no rule by name
/// reads: that message's next read catches up, so its count can move. The reply
/// itself is not behind anything.
#[tokio::test]
async fn a_reply_brings_the_message_it_answers_up_to_date() {
    let h = related(vec![
        json!({"id": "msg_100001", "text": "hello", "reply_count": 0}),
        json!({"id": "msg_100002", "text": "hi back", "reply_count": 0, "thread": "msg_100001"}),
        json!({
            "effects": [{"type": "messages", "id": "msg_100001", "set": {"reply_count": 1}}],
            "response": {"id": "msg_100001", "text": "rewritten", "reply_count": 1},
        }),
    ]);
    h.seeded("POST", "/messages", Some(json!({"text": "hello"})))
        .await;
    let (_, source, _) = h.call("GET", "/messages/msg_100001", None).await;
    assert_eq!(source, "state");
    h.seeded(
        "POST",
        "/messages",
        Some(json!({"text": "hi back", "thread": "msg_100001"})),
    )
    .await;
    assert_eq!(h.calls(), 2);

    let (_, source, read) = h.call("GET", "/messages/msg_100001", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["reply_count"], 1);
    assert_eq!(read["text"], "hello", "only the effect changes it");
    assert_eq!(h.calls(), 3);
    let prompt = h.prompt(2);
    let new = block(&prompt, "new_resources").expect("shown the reply as new");
    assert!(new.contains("msg_100002"), "{prompt}");

    let (_, source, _) = h.call("GET", "/messages/msg_100002", None).await;
    assert_eq!(source, "state", "the reply is not behind itself");
    let (_, source, _) = h.call("GET", "/messages/msg_100001", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 3);
}

/// A record the create does not refer to is not marked: its read answers from
/// state, at no cost, and only the referenced payment has a memory.
#[tokio::test]
async fn an_unrelated_record_is_not_marked() {
    let h = related(vec![
        payment("pay_100001", 0, "captured"),
        payment("pay_100002", 0, "captured"),
        json!({"id": "re_200001", "amount": 40, "payment_id": "pay_100001"}),
    ]);
    h.seeded("POST", "/payments", Some(json!({"amount": 100})))
        .await;
    h.seeded("POST", "/payments", Some(json!({"amount": 100})))
        .await;
    h.seeded(
        "POST",
        "/refunds",
        Some(json!({"payment_id": "pay_100001", "amount": 40})),
    )
    .await;
    assert_eq!(h.calls(), 3);

    let (_, source, read) = h.call("GET", "/payments/pay_100002", None).await;
    assert_eq!(source, "state");
    assert_eq!(read["amount_refunded"], 0);
    assert_eq!(h.calls(), 3);
    let memory = h.memory().await;
    let keys: Vec<&str> = memory.iter().map(|doc| doc.id.as_str()).collect();
    assert_eq!(keys, ["payments/pay_100001"], "{memory:?}");
}

/// A record the create's answer says changed, through an applied effect, already
/// shows it: no catch-up.
#[tokio::test]
async fn an_applied_effect_on_the_referenced_record_costs_no_call() {
    let h = related(vec![
        payment("pay_100001", 0, "captured"),
        json!({
            "effects": [{"type": "payments", "id": "pay_100001", "set": {"amount_refunded": 40}}],
            "response": {"id": "re_200001", "amount": 40, "payment_id": "pay_100001"},
        }),
    ]);
    h.seeded("POST", "/payments", Some(json!({"amount": 100})))
        .await;
    h.seeded(
        "POST",
        "/refunds",
        Some(json!({"payment_id": "pay_100001", "amount": 40})),
    )
    .await;
    let (_, source, read) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "state");
    assert_eq!(read["amount_refunded"], 40);
    assert_eq!(h.calls(), 2);
}

/// A read with nothing created since its target was stored has no new block.
#[tokio::test]
async fn a_catch_up_with_nothing_created_shows_no_new_block() {
    let h = related(vec![
        json!({"id": "card_100001", "name": "old", "idList": "list_100001", "closed": false}),
    ]);
    h.call("GET", "/cards/card_100001", None).await;
    let prompt = h.prompt(0);
    assert!(!prompt.contains("new_resources"), "{prompt}");
}

fn card(name: &str, list: &str) -> Value {
    json!({"id": "card_100001", "name": name, "idList": list, "closed": false})
}

/// An update sent in its query (Trello's `PUT /cards/{id}?name=…&idList=…`) moves the
/// card: the members it names take the answer's values over the stored copy's.
#[tokio::test]
async fn an_update_by_query_moves_the_record() {
    let h = related(vec![
        card("old", "list_100001"),
        card("new name", "list_100002"),
    ]);
    h.call("GET", "/cards/card_100001", None).await;
    let (status, _, answered) = h
        .call(
            "PUT",
            "/cards/card_100001?name=new%20name&idList=list_100002",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answered["name"], "new name");
    assert_eq!(answered["idList"], "list_100002");

    let (_, source, read) = h.call("GET", "/cards/card_100001", None).await;
    assert_eq!(source, "state");
    assert_eq!(read["name"], "new name");
    assert_eq!(read["idList"], "list_100002");
    assert_eq!(read["closed"], false);
    // The state-first executor sets what the query names, with no call.
    assert_eq!(h.calls(), 1);
}

/// What the query does not name stays as stored, whatever the answer says.
#[tokio::test]
async fn an_update_by_query_keeps_what_it_does_not_name() {
    let h = related(vec![
        card("old", "list_100001"),
        json!({"id": "card_100001", "name": "new name", "idList": "list_100009", "closed": true}),
    ]);
    h.call("GET", "/cards/card_100001", None).await;
    let (_, _, answered) = h
        .call("PUT", "/cards/card_100001?name=new%20name", None)
        .await;
    assert_eq!(answered["name"], "new name");
    assert_eq!(answered["idList"], "list_100001");
    assert_eq!(answered["closed"], false);
}

/// A form body names its members as a JSON body does.
#[tokio::test]
async fn an_update_by_form_moves_the_record() {
    let h = related(vec![
        card("old", "list_100001"),
        card("formed", "list_100002"),
    ]);
    h.call("GET", "/cards/card_100001", None).await;
    let (status, _, answered) = h
        .form(
            "PATCH",
            "/cards/card_100001",
            "name=formed&idList=list_100002",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answered["name"], "formed");
    assert_eq!(answered["idList"], "list_100002");
    let (_, _, read) = h.call("GET", "/cards/card_100001", None).await;
    assert_eq!(read["idList"], "list_100002");
}

// The review's probes, each a test.

/// A read of a record a create named by value changes by no member its answer only
/// says: the stored amount and status stand.
#[tokio::test]
async fn probe_a_catch_up_does_not_rewrite_the_amount() {
    let h = related(vec![
        payment("pay_100001", 0, "captured"),
        json!({"id": "re_200001", "amount": 40, "payment_id": "pay_100001"}),
        json!({"id": "pay_100001", "amount": 7, "amount_refunded": 40, "status": "weird"}),
    ]);
    h.seeded("POST", "/payments", Some(json!({"amount": 100})))
        .await;
    h.seeded(
        "POST",
        "/refunds",
        Some(json!({"payment_id": "pay_100001", "amount": 40})),
    )
    .await;
    let (_, source, read) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "llm", "told the refund once");
    assert_eq!(read, payment("pay_100001", 0, "captured"));
    let (_, source, again) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "state");
    assert_eq!(again, read);
    assert_eq!(h.calls(), 3);
}

/// A message whose request names a payment under any member tells the payment, which
/// its read's answer still cannot rewrite.
#[tokio::test]
async fn probe_a_message_naming_a_payment_does_not_rewrite_it() {
    let h = related(vec![
        payment("pay_100001", 0, "captured"),
        json!({"id": "msg_100002", "text": "x", "reply_count": 0, "thread": "pay_100001"}),
        json!({"id": "pay_100001", "amount": 999, "amount_refunded": 0, "status": "captured"}),
    ]);
    h.seeded("POST", "/payments", Some(json!({"amount": 100})))
        .await;
    h.seeded(
        "POST",
        "/messages",
        Some(json!({"text": "x", "thread": "pay_100001"})),
    )
    .await;
    let (_, _, read) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(read["amount"], 100);
    let held = h.store.get(&session(), "payments", "pay_100001").await;
    assert_eq!(held.unwrap().unwrap().data["amount"], 100);
}

/// A value only the generated answer holds names nothing: a count equal to a
/// payment's id marks no payment.
#[tokio::test]
async fn probe_a_number_in_the_answer_marks_nothing() {
    let h = related(vec![
        json!({"id": "100001", "amount": 100, "amount_refunded": 0, "status": "captured"}),
        json!({"id": "msg_100002", "text": "x", "reply_count": 100001}),
        json!({"id": "100001", "amount": 1, "amount_refunded": 0, "status": "changed"}),
    ]);
    h.seeded("POST", "/payments", Some(json!({"amount": 100})))
        .await;
    h.seeded("POST", "/messages", Some(json!({"text": "x"})))
        .await;
    let (_, source, read) = h.call("GET", "/payments/100001", None).await;
    assert_eq!(source, "state");
    assert_eq!(read["amount"], 100);
    assert_eq!(h.calls(), 2);
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);
}

/// Five refunds, each read back: each read is told once (one call), and the payment
/// changes by effects only, of which these answers list none.
#[tokio::test]
async fn probe_a_create_heavy_flow_costs_one_call_per_affected_read() {
    let mut script = vec![payment("pay_100001", 0, "captured")];
    for i in 0..5 {
        script.push(json!({"id": format!("re_20000{i}"), "amount": 1, "payment_id": "pay_100001"}));
        script.push(payment("pay_100001", i + 1, "partially_refunded"));
    }
    let h = related(script);
    h.seeded("POST", "/payments", Some(json!({"amount": 100})))
        .await;
    for _ in 0..5 {
        h.seeded(
            "POST",
            "/refunds",
            Some(json!({"payment_id": "pay_100001", "amount": 1})),
        )
        .await;
        let (_, _, read) = h.call("GET", "/payments/pay_100001", None).await;
        assert_eq!(read, payment("pay_100001", 0, "captured"));
    }
    assert_eq!(h.calls(), 11);
}

/// What the query names is stored as the query says, whatever the answer says, and
/// what it does not name stays.
#[tokio::test]
async fn probe_a_query_member_is_stored_as_the_query_says() {
    let h = related(vec![
        card("old", "list_100001"),
        json!({"id": "card_100001", "name": "llm-renamed", "idList": "list_100009", "closed": true}),
    ]);
    h.call("GET", "/cards/card_100001", None).await;
    let (_, _, answered) = h
        .call("PUT", "/cards/card_100001?idList=list_100001", None)
        .await;
    assert_eq!(answered, card("old", "list_100001"));
    let (_, _, read) = h.call("GET", "/cards/card_100001", None).await;
    assert_eq!(read, card("old", "list_100001"));
}

/// Payments updated in place, by a `PATCH` answering an acknowledgement, and by an
/// action, beside refunds naming them by value.
const LEDGER: &str = r##"
openapi: 3.0.0
info: {title: Ledger, version: "1"}
paths:
  /payments:
    post:
      operationId: createPayment
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}
  /payments/{payment_id}:
    parameters: [{name: payment_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getPayment
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}
    put:
      operationId: putPayment
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}
    patch:
      operationId: patchPayment
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}
  /payments/{payment_id}/capture:
    post:
      operationId: capturePayment
      parameters: [{name: payment_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Payment"}}}}
  /refunds:
    post:
      operationId: createRefund
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
components:
  schemas:
    Payment:
      type: object
      required: [id, amount, amount_refunded, status]
      properties:
        id: {type: string}
        amount: {type: integer}
        amount_refunded: {type: integer}
        status: {type: string}
    Refund:
      type: object
      required: [id, amount]
      properties:
        id: {type: string}
        amount: {type: integer}
        payment_id: {type: string}
    Ack:
      type: object
      required: [ok]
      properties:
        ok: {type: boolean}
"##;

/// A payment and then a refund naming it, so the payment's memory only refers to it.
async fn referenced_payment(h: &Harness, status: &str) {
    h.seeded("POST", "/payments", Some(json!({"amount": 100})))
        .await;
    let (_, source, read) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["status"], status);
    h.seeded(
        "POST",
        "/refunds",
        Some(json!({"payment_id": "pay_100001", "amount": 40})),
    )
    .await;
}

fn refund() -> Value {
    json!({"id": "re_200001", "amount": 40, "payment_id": "pay_100001"})
}

/// A `PUT` of a payment a refund only referred to settles as a plain update: what it
/// names wins, what the answer guessed from the refund goes, it is told nothing of the
/// refund, and the payment's next read still changes by effects alone.
#[tokio::test]
async fn a_put_on_a_referenced_record_settles() {
    let h = related_with(
        LEDGER,
        vec![
            payment("pay_100001", 0, "captured"),
            refund(),
            json!({"id": "pay_100001", "amount": 150, "amount_refunded": 40, "status": "partially_refunded"}),
            by_effects(
                "pay_100001",
                json!({"amount_refunded": 40}),
                json!({"id": "pay_100001", "amount": 7, "amount_refunded": 40, "status": "refunded"}),
            ),
        ],
    );
    referenced_payment(&h, "captured").await;
    let (status, _, put) = h
        .call("PUT", "/payments/pay_100001", Some(json!({"amount": 150})))
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!(
        put,
        json!({"id": "pay_100001", "amount": 150, "amount_refunded": 0, "status": "captured"})
    );
    let prompt = h.prompt(2);
    assert!(block(&prompt, "earlier_requests").is_none(), "{prompt}");

    let (_, source, read) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "llm");
    assert_eq!(
        read,
        json!({"id": "pay_100001", "amount": 150, "amount_refunded": 40, "status": "captured"})
    );
    assert!(h.prompt(3).contains("createRefund"));
    assert_eq!(h.calls(), 4);
}

/// An action on a payment a refund only referred to is as it was: told of the refund,
/// its answer is the state after it.
#[tokio::test]
async fn an_action_on_a_referenced_record_is_unchanged() {
    let h = related_with(
        LEDGER,
        vec![
            payment("pay_100001", 0, "authorized"),
            refund(),
            payment("pay_100001", 40, "captured"),
        ],
    );
    referenced_payment(&h, "authorized").await;
    let (status, _, captured) = h.call("POST", "/payments/pay_100001/capture", None).await;
    assert_eq!(status, StatusCode::OK, "{captured}");
    assert_eq!(captured["status"], "captured", "{captured}");
    let prompt = h.prompt(2);
    assert!(
        block(&prompt, "earlier_requests").is_some_and(|e| e.contains("createRefund")),
        "{prompt}"
    );
}

/// The review's sequence: a referring create, a whole update, then a short one. The
/// memory holds a reference and an ordinary request; the catch-up whose answer replaces
/// the stored copy is told only the ordinary one, and the reference waits for a read
/// that changes by effects alone. The acknowledgement adds nothing to the payment.
#[tokio::test]
async fn a_mixed_memory_shows_a_catch_up_no_reference() {
    let h = related_with(
        LEDGER,
        vec![
            payment("pay_100001", 0, "captured"),
            refund(),
            payment("pay_100001", 0, "captured"),
            json!({"ok": true}),
            json!({"id": "pay_100001", "amount": 175, "amount_refunded": 0, "status": "captured"}),
            by_effects(
                "pay_100001",
                json!({"amount_refunded": 40}),
                json!({"id": "pay_100001", "amount": 1, "amount_refunded": 99, "status": "refunded"}),
            ),
        ],
    );
    referenced_payment(&h, "captured").await;
    let (status, _, _) = h
        .call("PUT", "/payments/pay_100001", Some(json!({"amount": 100})))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, ack) = h
        .call(
            "PATCH",
            "/payments/pay_100001",
            Some(json!({"amount": 175})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    let held = h
        .store
        .get(&session(), "payments", "pay_100001")
        .await
        .unwrap()
        .unwrap();
    assert!(held.data.get("ok").is_none(), "{}", held.data);

    let (_, source, read) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["amount"], 175, "{read}");
    let prompt = h.prompt(4);
    let earlier = block(&prompt, "earlier_requests").expect("told the patch");
    assert!(earlier.contains("patchPayment"), "{prompt}");
    assert!(!prompt.contains("createRefund"), "{prompt}");
    assert!(block(&prompt, "new_resources").is_none(), "{prompt}");

    let (_, source, read) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "llm", "the reference still waits: {read}");
    assert_eq!(
        read,
        json!({"id": "pay_100001", "amount": 175, "amount_refunded": 40, "status": "captured"})
    );
    assert!(h.prompt(5).contains("createRefund"));
    let (_, source, _) = h.call("GET", "/payments/pay_100001", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 6);
}
