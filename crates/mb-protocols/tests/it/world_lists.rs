//! A session's lists and lookups agree with its world: a list answer keeps the items
//! its query's equality filters ask for and its page size, a deleted record does not
//! come back through a lookup by its value, and a create under a parent path is shown
//! what was created there before. Driven over HTTP with a scripted LLM.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{LlmError, Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::MemoryStore;
use serde_json::{Value, json};
use tower::ServiceExt;

const LISTS: &str = r##"
openapi: 3.0.0
info: {title: Lists, version: "1"}
paths:
  /messages:
    get:
      operationId: listMessages
      parameters:
        - {name: To, in: query, schema: {type: string, format: phone-number}}
        - {name: status, in: query, schema: {type: string}}
        - {name: Price, in: query, schema: {type: string}}
        - {name: PageSize, in: query, schema: {type: integer}}
        - {name: q, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [messages, total]
                properties:
                  messages: {type: array, items: {$ref: "#/components/schemas/Message"}}
                  total: {type: integer}
  /tags:
    get:
      operationId: listTags
      parameters: [{name: kind, in: query, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {type: array, minItems: 2, items: {$ref: "#/components/schemas/Tag"}}
components:
  schemas:
    Message:
      type: object
      required: [sid, to, status]
      properties:
        sid: {type: string}
        to: {type: string, format: phone-number}
        status: {type: string, enum: [queued, sent, delivered]}
        price: {type: number}
    Tag:
      type: object
      required: [id, kind]
      properties: {id: {type: string}, kind: {type: string}}
"##;

struct Api {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
}

fn api_of(contract: &str, consistency: bool, script: Vec<Result<String, LlmError>>) -> Api {
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut rt = MockRuntime::new(
        "lists",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    rt.consistency = consistency;
    Api {
        router: http_rest::router(Arc::new(rt)),
        provider,
    }
}

impl Api {
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
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    async fn get(&self, uri: &str) -> Value {
        let (status, body) = self.call("GET", uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        body
    }

    fn prompt(&self, call: usize) -> String {
        self.provider.requests()[call].messages[1].content.clone()
    }
}

fn message(sid: &str, to: &str, status: &str) -> Value {
    json!({"sid": sid, "to": to, "status": status})
}

/// Three messages, two of them to +1555, with a total that counts them.
fn three_messages() -> Result<String, LlmError> {
    Ok(json!({
        "messages": [
            message("SM1", "+1555", "queued"),
            message("SM2", "+1666", "sent"),
            message("SM3", "+1555", "delivered"),
        ],
        "total": 3,
    })
    .to_string())
}

fn sids(body: &Value) -> Vec<&str> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["sid"].as_str().unwrap())
        .collect()
}

/// `To` names the items' `to`: only that recipient's messages stay, and the total
/// counts what stayed.
#[tokio::test]
async fn a_filter_keeps_only_what_it_names_and_fixes_the_count() {
    let api = api_of(LISTS, true, vec![three_messages()]);
    let body = api.get("/messages?To=%2B1555").await;
    assert_eq!(sids(&body), ["SM1", "SM3"]);
    assert_eq!(body["total"], 2);
}

/// A value the member's `enum` does not hold (`all`) is no filter.
#[tokio::test]
async fn a_value_outside_the_enum_changes_nothing() {
    let api = api_of(LISTS, true, vec![three_messages()]);
    let body = api.get("/messages?status=all").await;
    assert_eq!(sids(&body), ["SM1", "SM2", "SM3"]);
    assert_eq!(body["total"], 3);
}

/// A comma-separated value whose every part is in the `enum` is a set.
#[tokio::test]
async fn an_enum_set_keeps_each_of_its_values() {
    let api = api_of(LISTS, true, vec![three_messages()]);
    let body = api.get("/messages?status=queued,sent").await;
    assert_eq!(sids(&body), ["SM1", "SM2"]);
}

/// A page size caps the list's items, and leaves the total, which counts every page.
#[tokio::test]
async fn a_page_size_caps_the_list() {
    let api = api_of(LISTS, true, vec![three_messages()]);
    let body = api.get("/messages?PageSize=2").await;
    assert_eq!(sids(&body), ["SM1", "SM2"]);
    assert_eq!(body["total"], 3);
}

/// A search names no member, and a text parameter beside a number member is no filter.
#[tokio::test]
async fn a_search_or_another_type_changes_nothing() {
    let api = api_of(LISTS, true, vec![three_messages(), three_messages()]);
    let body = api.get("/messages?q=%2B1666").await;
    assert_eq!(sids(&body), ["SM1", "SM2", "SM3"]);
    let body = api.get("/messages?Price=1").await;
    assert_eq!(sids(&body), ["SM1", "SM2", "SM3"]);
}

/// A filter that would leave fewer items than `minItems` leaves the answer as it was.
#[tokio::test]
async fn a_filter_keeps_min_items() {
    let tags = json!([
        {"id": "t1", "kind": "a"},
        {"id": "t2", "kind": "b"},
        {"id": "t3", "kind": "c"},
    ]);
    let api = api_of(LISTS, true, vec![Ok(tags.to_string())]);
    let body = api.get("/tags?kind=a").await;
    assert_eq!(body, tags);
}

/// With consistency off, an answer is as the model wrote it.
#[tokio::test]
async fn consistency_off_filters_nothing() {
    let api = api_of(LISTS, false, vec![three_messages()]);
    let body = api.get("/messages?To=%2B1555&PageSize=1").await;
    assert_eq!(sids(&body), ["SM1", "SM2", "SM3"]);
}

/// Auth0's users, with a lookup by email that a list item type shares.
const USERS: &str = r##"
openapi: 3.0.0
info: {title: Users, version: "1"}
paths:
  /users:
    post:
      operationId: createUser
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
  /users/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getUser
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
    delete:
      operationId: deleteUser
      responses:
        "204": {description: gone}
  /users-by-email:
    get:
      operationId: usersByEmail
      parameters: [{name: email, in: query, required: true, schema: {type: string, format: email}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/User"}}}}
components:
  schemas:
    User:
      type: object
      required: [user_id, email]
      properties:
        user_id: {type: string}
        email: {type: string, format: email}
        name: {type: string}
"##;

fn user(id: &str, email: &str) -> Value {
    json!({"user_id": id, "email": email, "name": "Ana"})
}

/// Creates `u1` with `ana@example.com`, then deletes it.
async fn created_then_deleted(api: &Api) {
    let (status, _) = api
        .call("POST", "/users", Some(json!({"email": "ana@example.com"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = api.call("DELETE", "/users/u1", None).await;
    assert!(status.is_success(), "{status}");
}

/// A user the model makes up under a new id, with the deleted user's email, is not
/// answered by a lookup naming that email.
#[tokio::test]
async fn a_deleted_user_does_not_come_back_by_email() {
    let api = api_of(
        USERS,
        true,
        vec![
            Ok(user("u1", "ana@example.com").to_string()),
            Ok(String::new()),
            Ok(json!([user("u2", "ana@example.com")]).to_string()),
        ],
    );
    created_then_deleted(&api).await;
    let body = api.get("/users-by-email?email=ana@example.com").await;
    assert_eq!(body, json!([]));
}

/// A user made again with that email is stored, which clears the mark: a lookup finds
/// it, and an answer under another id is not taken for the deleted one.
#[tokio::test]
async fn a_recreated_user_is_found_by_email() {
    let api = api_of(
        USERS,
        true,
        vec![
            Ok(user("u1", "ana@example.com").to_string()),
            Ok(String::new()),
            Ok(user("u3", "ana@example.com").to_string()),
            Ok(json!([user("u3", "ana@example.com")]).to_string()),
        ],
    );
    created_then_deleted(&api).await;
    let (status, _) = api
        .call("POST", "/users", Some(json!({"email": "ana@example.com"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let body = api.get("/users-by-email?email=ana@example.com").await;
    assert_eq!(body, json!([user("u3", "ana@example.com")]));

    let api = api_of(
        USERS,
        true,
        vec![
            Ok(user("u1", "ana@example.com").to_string()),
            Ok(String::new()),
            Ok(user("u3", "ana@example.com").to_string()),
            Ok(json!([user("u9", "ana@example.com")]).to_string()),
        ],
    );
    created_then_deleted(&api).await;
    let (status, _) = api
        .call("POST", "/users", Some(json!({"email": "ana@example.com"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let body = api.get("/users-by-email?email=ana@example.com").await;
    assert_eq!(body, json!([user("u9", "ana@example.com")]));
}

/// PayPal's refunds of a capture, created under the capture's path.
const REFUNDS: &str = r##"
openapi: 3.0.0
info: {title: Refunds, version: "1"}
paths:
  /captures/{capture_id}/refund:
    post:
      operationId: refundCapture
      parameters: [{name: capture_id, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /captures/{capture_id}:
    get:
      operationId: getCapture
      parameters: [{name: capture_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Capture"}}}}
  /refunds/{refund_id}:
    get:
      operationId: getRefund
      parameters: [{name: refund_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
components:
  schemas:
    Refund:
      type: object
      required: [id, amount]
      properties: {id: {type: string}, amount: {type: string}}
    Capture:
      type: object
      required: [id, status]
      properties: {id: {type: string}, status: {type: string}}
"##;

/// A capture's read is shown the refunds created under its path, and another
/// capture's is not; the read stores nothing of them.
#[tokio::test]
async fn a_capture_read_sees_its_refunds() {
    let api = api_of(
        REFUNDS,
        true,
        vec![
            Ok(json!({"id": "R1", "amount": "1.00"}).to_string()),
            Ok(json!({"id": "C1", "status": "COMPLETED"}).to_string()),
            Ok(json!({"id": "C2", "status": "COMPLETED"}).to_string()),
        ],
    );
    let (status, _) = api
        .call(
            "POST",
            "/captures/C1/refund",
            Some(json!({"amount": "1.00"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    api.get("/captures/C1").await;
    let prompt = api.prompt(1);
    assert!(
        related_of(&prompt).iter().any(|l| l.contains("\"R1\"")),
        "{prompt}"
    );
    api.get("/captures/C2").await;
    let prompt = api.prompt(2);
    assert!(!prompt.contains("\"R1\""), "{prompt}");
    // Context only: the refund reads back as it was created.
    let refund = api.get("/refunds/R1").await;
    assert_eq!(refund["amount"], "1.00");
    assert_eq!(api.provider.requests().len(), 3);
}

/// The lines of a prompt's `<related_resources>`.
fn related_of(prompt: &str) -> Vec<&str> {
    prompt
        .split_once("<related_resources>\n")
        .and_then(|(_, rest)| rest.split_once("</related_resources>"))
        .map_or_else(Vec::new, |(lines, _)| lines.lines().collect())
}

/// A second refund of a capture is shown the first; one of another capture is not.
#[tokio::test]
async fn a_second_refund_sees_the_first() {
    let refund = |id: &str, amount: &str| Ok(json!({"id": id, "amount": amount}).to_string());
    let api = api_of(
        REFUNDS,
        true,
        vec![
            refund("R1", "1.00"),
            refund("R2", "2.00"),
            refund("R3", "3.00"),
        ],
    );
    let body = json!({"amount": "1.00"});
    let (status, _) = api
        .call("POST", "/captures/C1/refund", Some(body.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = api
        .call("POST", "/captures/C1/refund", Some(body.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = api.prompt(1);
    assert!(
        related_of(&prompt).iter().any(|l| l.contains("\"R1\"")),
        "{prompt}"
    );
    let (status, _) = api.call("POST", "/captures/C2/refund", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = api.prompt(2);
    assert!(!prompt.contains("\"R1\""), "{prompt}");
    assert!(!prompt.contains("\"R2\""), "{prompt}");
}
