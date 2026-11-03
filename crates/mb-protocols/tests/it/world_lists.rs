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
        - {name: topic_id, in: query, schema: {type: integer}}
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

/// A name that only holds a page-size word (`top` in `topic_id`) is no page size.
#[tokio::test]
async fn a_word_inside_a_name_caps_nothing() {
    let api = api_of(LISTS, true, vec![three_messages()]);
    let body = api.get("/messages?topic_id=2").await;
    assert_eq!(sids(&body), ["SM1", "SM2", "SM3"]);
}

/// Issues whose `state` is plain text, filtered by an optional parameter with an
/// `enum` of its own, and by a `name`.
const ISSUES: &str = r##"
openapi: 3.0.0
info: {title: Issues, version: "1"}
paths:
  /issues:
    get:
      operationId: listIssues
      parameters:
        - {name: state, in: query, schema: {type: string, enum: [opened, closed, all]}}
        - {name: name, in: query, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Issue"}}}}}
    post:
      operationId: createIssue
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
  /issues/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getIssue
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
    delete:
      operationId: deleteIssue
      responses:
        "204": {description: gone}
components:
  schemas:
    Issue:
      type: object
      required: [id, state, name]
      properties: {id: {type: string}, state: {type: string}, name: {type: string}}
"##;

fn issue(id: &str, state: &str, name: &str) -> Value {
    json!({"id": id, "state": state, "name": name})
}

fn ids(body: &Value) -> Vec<&str> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect()
}

/// A value of the parameter's own `enum` (`all`) says nothing of a plain-text member.
#[tokio::test]
async fn a_parameter_enum_beside_plain_text_filters_nothing() {
    let answer = json!([issue("i5", "opened", "a"), issue("i7", "closed", "b")]);
    let api = api_of(ISSUES, true, vec![Ok(answer.to_string())]);
    let body = api.get("/issues?state=all").await;
    assert_eq!(ids(&body), ["i5", "i7"]);
}

/// A plain-text member keeps the items holding the value, in any case.
#[tokio::test]
async fn a_plain_text_filter_keeps_what_holds_the_value() {
    let answer = json!([
        issue("i1", "opened", "Anna"),
        issue("i2", "opened", "Bob"),
        issue("i3", "opened", "joanne"),
    ]);
    let api = api_of(ISSUES, true, vec![Ok(answer.to_string())]);
    let body = api.get("/issues?name=Ann").await;
    assert_eq!(ids(&body), ["i1", "i3"]);
}

/// A plain-text value holding a digit (an id, a phone number, a code) matches whole,
/// ignoring case: `u1` keeps `U1`, not `u10`.
#[tokio::test]
async fn a_plain_text_value_with_a_digit_matches_whole() {
    let answer = json!([issue("i1", "opened", "U1"), issue("i2", "opened", "u10")]);
    let api = api_of(ISSUES, true, vec![Ok(answer.to_string())]);
    let body = api.get("/issues?name=u1").await;
    assert_eq!(ids(&body), ["i1"]);
}

/// An unencoded `+` decodes to a space: the value is trimmed and read as the client
/// meant, `+1555` and not `+15550`. A value only of spaces is no filter.
#[tokio::test]
async fn a_value_is_trimmed_and_an_unencoded_plus_kept() {
    let answer = json!([
        issue("i1", "opened", "+1555"),
        issue("i2", "opened", "+15550"),
        issue("i3", "opened", "1555"),
    ]);
    let api = api_of(
        ISSUES,
        true,
        vec![Ok(answer.to_string()), Ok(answer.to_string())],
    );
    let body = api.get("/issues?name=+1555").await;
    assert_eq!(ids(&body), ["i1", "i3"]);
    let body = api.get("/issues?name=%20%20").await;
    assert_eq!(ids(&body), ["i1", "i2", "i3"]);
}

/// Deleting an `opened` issue marks only its identity: an optional filter's value is
/// no key, so other opened issues stay.
#[tokio::test]
async fn a_delete_does_not_mark_an_optional_filter_value() {
    let answer = json!([
        issue("i5", "opened", "a"),
        issue("i6", "opened", "b"),
        issue("i7", "closed", "c"),
    ]);
    let contract = ISSUES.replace(
        "state: {type: string}",
        "state: {type: string, enum: [opened, closed]}",
    );
    let api = api_of(
        &contract,
        true,
        vec![
            Ok(issue("i1", "opened", "x").to_string()),
            Ok(answer.to_string()),
        ],
    );
    let (status, _) = api
        .call("POST", "/issues", Some(json!({"state": "opened"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = api.call("DELETE", "/issues/i1", None).await;
    assert!(status.is_success(), "{status}");
    let body = api.get("/issues?state=opened").await;
    assert_eq!(ids(&body), ["i5", "i6"]);
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

/// Tickets: a list beside a create, a read and an update, with array members, a
/// nullable reference, nested objects the summary declares less of, and parameters
/// that name no member.
const TICKETS: &str = r##"
openapi: 3.1.0
info: {title: Tickets, version: "1"}
paths:
  /tickets:
    get:
      operationId: listTickets
      parameters:
        - {name: status, in: query, schema: {type: string, enum: [open, closed, all]}}
        - {name: labels, in: query, schema: {type: string}}
        - {name: tag_name, in: query, schema: {type: string}}
        - {name: exclude_labels, in: query, schema: {type: string}}
        - {name: without_tags, in: query, schema: {type: string}}
        - {name: tag_ids, in: query, schema: {type: string}}
        - {name: assignee_id, in: query, schema: {type: integer}}
        - {name: before, in: query, schema: {type: string}}
        - {name: inclusive, in: query, schema: {type: boolean}}
        - {name: page, in: query, schema: {type: integer}}
        - {name: q, in: query, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/TicketItem"}}}}}
    post:
      operationId: createTicket
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}
  /tickets/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTicket
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}
    patch:
      operationId: updateTicket
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}
components:
  schemas:
    Ticket:
      type: object
      required: [id, status, title]
      properties:
        id: {type: string}
        status: {type: string, enum: [open, closed, pending]}
        title: {type: string}
        labels: {type: array, items: {type: string}}
        tags: {type: array, items: {type: string}}
        assignee_id: {type: [integer, "null"]}
        reply_count: {type: integer}
        internal_note: {type: string}
        owner: {$ref: "#/components/schemas/Person"}
        watchers: {type: array, items: {$ref: "#/components/schemas/Person"}}
    Person:
      type: object
      properties: {id: {type: string}, name: {type: string}, email: {type: string}}
    PersonSummary:
      type: object
      additionalProperties: false
      properties: {id: {type: string}, name: {type: string}}
    TicketItem:
      type: object
      required: [id, status, title]
      properties:
        id: {type: string}
        status: {type: string, enum: [open, closed]}
        title: {type: string}
        labels: {type: array, items: {type: string}}
        tags: {type: array, items: {type: string}}
        assignee_id: {type: [integer, "null"]}
        reply_count: {type: integer}
        owner: {$ref: "#/components/schemas/PersonSummary"}
        watchers: {type: array, items: {$ref: "#/components/schemas/PersonSummary"}}
"##;

fn ticket(id: &str, status: &str) -> Value {
    json!({"id": id, "status": status, "title": format!("T {id}"), "reply_count": 0})
}

fn with(mut item: Value, member: &str, value: Value) -> Value {
    item[member] = value;
    item
}

/// Creates ticket `t1` and updates its status to `to`.
async fn created_then_updated(api: &Api, to: &str) {
    let (status, _) = api
        .call("POST", "/tickets", Some(json!({"title": "T t1"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = api
        .call("PATCH", "/tickets/t1", Some(json!({"status": to})))
        .await;
    assert_eq!(status, StatusCode::OK);
}

fn person(id: &str) -> Value {
    json!({"id": id, "name": format!("P {id}"), "email": format!("{id}@example.com")})
}

fn updated_script(list: Value) -> Vec<Result<String, LlmError>> {
    let created = with(
        with(
            with(ticket("t1", "open"), "internal_note", json!("secret")),
            "owner",
            person("o1"),
        ),
        "watchers",
        json!([person("w1"), person("w2")]),
    );
    let updated = with(created.clone(), "status", json!("closed"));
    vec![
        Ok(created.to_string()),
        Ok(updated.to_string()),
        Ok(list.to_string()),
    ]
}

/// A list item the session holds shows the stored record's members, those the item
/// declares only; another item is as generated, and the store is not changed.
#[tokio::test]
async fn a_listed_item_shows_its_stored_record() {
    let list = json!([ticket("t1", "open"), ticket("t2", "open")]);
    let api = api_of(TICKETS, true, updated_script(list));
    created_then_updated(&api, "closed").await;
    let held = api.get("/tickets/t1").await;
    assert_eq!(held["status"], "closed");
    assert_eq!(held["internal_note"], "secret");
    let body = api.get("/tickets").await;
    assert_eq!(body[0]["status"], "closed", "{body}");
    assert_eq!(body[0]["title"], held["title"], "{body}");
    assert!(body[0].get("internal_note").is_none(), "{body}");
    assert_eq!(body[1], ticket("t2", "open"));
    // The list changed no stored record.
    assert_eq!(api.get("/tickets/t1").await, held);
    assert_eq!(api.provider.requests().len(), 3);
}

/// What a nested object or an array's objects declare is all an overlay brings, at any
/// depth: the owner's and watchers' `email` stays out, under `additionalProperties:
/// false`, and the rest of the overlay still holds.
#[tokio::test]
async fn an_overlay_brings_no_nested_member_the_item_does_not_declare() {
    let list = json!([ticket("t1", "open")]);
    let api = api_of(TICKETS, true, updated_script(list));
    created_then_updated(&api, "closed").await;
    let held = api.get("/tickets/t1").await;
    assert_eq!(held["owner"]["email"], "o1@example.com", "{held}");
    let body = api.get("/tickets").await;
    assert_eq!(body[0]["status"], "closed", "{body}");
    assert_eq!(
        body[0]["owner"],
        json!({"id": "o1", "name": "P o1"}),
        "{body}"
    );
    assert_eq!(
        body[0]["watchers"],
        json!([{"id": "w1", "name": "P w1"}, {"id": "w2", "name": "P w2"}]),
        "{body}"
    );
}

/// A stored record the item's schema does not allow (`pending` is no item status)
/// leaves the item as generated.
#[tokio::test]
async fn a_stored_record_the_item_does_not_allow_changes_nothing() {
    let list = json!([ticket("t1", "open")]);
    let mut script = updated_script(list);
    script[1] = Ok(ticket("t1", "pending").to_string());
    let api = api_of(TICKETS, true, script);
    created_then_updated(&api, "pending").await;
    assert_eq!(api.get("/tickets/t1").await["status"], "pending");
    let body = api.get("/tickets").await;
    assert_eq!(body, json!([ticket("t1", "open")]));
}

/// A stored record is overlaid before the filters: `status=open` drops the ticket the
/// session closed.
#[tokio::test]
async fn a_filter_sees_the_stored_record() {
    let list = json!([ticket("t1", "open"), ticket("t2", "open")]);
    let api = api_of(TICKETS, true, updated_script(list));
    created_then_updated(&api, "closed").await;
    let body = api.get("/tickets?status=open").await;
    assert_eq!(ids(&body), ["t2"]);
}

/// With consistency off, a list is as the model wrote it.
#[tokio::test]
async fn consistency_off_overlays_nothing() {
    let list = json!([ticket("t1", "open")]);
    let api = api_of(TICKETS, false, updated_script(list));
    created_then_updated(&api, "closed").await;
    let body = api.get("/tickets").await;
    assert_eq!(body, json!([ticket("t1", "open")]));
}

/// Tasks created under a project, listed under a project.
const TASKS: &str = r##"
openapi: 3.1.0
info: {title: Tasks, version: "1"}
paths:
  /projects/{project_id}/tasks:
    parameters: [{name: project_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listTasks
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Task"}}}}}
    post:
      operationId: createTask
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
  /tasks/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTask
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
components:
  schemas:
    Task:
      type: object
      required: [id, project_id, title]
      properties: {id: {type: string}, project_id: {type: string}, title: {type: string}}
"##;

fn task(id: &str, project: &str, title: &str) -> Value {
    json!({"id": id, "project_id": project, "title": title})
}

/// A generated item that holds the path's value under a member another stored record's
/// value contradicts (`project_id`) is not overlaid: it stays as generated, as which of
/// the two is right is a guess. Under its own parent it is overlaid.
#[tokio::test]
async fn a_stored_record_of_another_parent_is_not_overlaid() {
    let listed = json!([task("tk-0001", "p2", "made up"), task("tk-0002", "p2", "B")]);
    let api = api_of(
        TASKS,
        true,
        vec![
            Ok(task("tk-0001", "p1", "A").to_string()),
            Ok(listed.to_string()),
            Ok(json!([task("tk-0001", "p1", "made up")]).to_string()),
        ],
    );
    let (status, _) = api
        .call("POST", "/projects/p1/tasks", Some(json!({"title": "A"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let body = api.get("/projects/p2/tasks").await;
    assert_eq!(body, listed);
    let body = api.get("/projects/p1/tasks").await;
    assert_eq!(body, json!([task("tk-0001", "p1", "A")]));
}

/// Users with followers and events, and lists with members: routes whose path
/// parameter names the items' own identity, takes an alias, or names one of many
/// parents.
const PEOPLE: &str = r##"
openapi: 3.1.0
info: {title: People, version: "1"}
paths:
  /users:
    post:
      operationId: createUser
      requestBody: {required: true, content: {application/json: {schema: {type: object, additionalProperties: true}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
  /users/{user_id}:
    parameters: [{name: user_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getUser
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
  /users/{user_id}/followers:
    parameters: [{name: user_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listFollowers
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/User"}}}}}
  /users/{user_id}/events:
    parameters: [{name: user_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listEvents
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Event"}}}}}
  /events:
    post:
      operationId: createEvent
      requestBody: {required: true, content: {application/json: {schema: {type: object, additionalProperties: true}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Event"}}}}
  /events/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getEvent
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Event"}}}}
  /lists/{list_id}/members:
    parameters: [{name: list_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listMembers
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Member"}}}}}
    post:
      operationId: addMember
      requestBody: {required: true, content: {application/json: {schema: {type: object, additionalProperties: true}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Member"}}}}
  /members/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getMember
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Member"}}}}
components:
  schemas:
    User:
      type: object
      required: [user_id]
      properties: {user_id: {type: string}, name: {type: string}}
    Event:
      type: object
      required: [id, user_id]
      properties: {id: {type: string}, user_id: {type: string}, kind: {type: string}}
    Member:
      type: object
      required: [id, list_id]
      properties: {id: {type: string}, list_id: {type: string}, name: {type: string}}
"##;

/// A follower is a user, whose identity the path's `user_id` also names: no conflict,
/// so the stored user is overlaid and stays.
#[tokio::test]
async fn a_path_naming_the_items_identity_is_no_parent() {
    let api = api_of(
        PEOPLE,
        true,
        vec![
            Ok(json!({"user_id": "u2", "name": "Bea"}).to_string()),
            Ok(json!([{"user_id": "u2", "name": "stale"}]).to_string()),
        ],
    );
    let (status, _) = api
        .call("POST", "/users", Some(json!({"name": "Bea"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let body = api.get("/users/u1/followers").await;
    assert_eq!(body, json!([{"user_id": "u2", "name": "Bea"}]));
}

/// `/users/me/events`: the generated item holds the user's id, not the alias, so
/// nothing contradicts and the stored event is overlaid.
#[tokio::test]
async fn an_alias_in_the_path_is_no_parent() {
    let event = json!({"id": "ev-0001", "user_id": "u1", "kind": "login"});
    let api = api_of(
        PEOPLE,
        true,
        vec![
            Ok(event.to_string()),
            Ok(json!([{"id": "ev-0001", "user_id": "u1", "kind": "stale"}]).to_string()),
        ],
    );
    let (status, _) = api
        .call("POST", "/events", Some(json!({"kind": "login"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let body = api.get("/users/me/events").await;
    assert_eq!(body, json!([event]));
}

/// A member of list A listed under B (many to many) stays in B's list, as generated.
#[tokio::test]
async fn a_member_of_two_lists_stays_in_both() {
    let generated = json!([{"id": "h1", "list_id": "B", "name": "made up"}]);
    let api = api_of(
        PEOPLE,
        true,
        vec![
            Ok(json!({"id": "h1", "list_id": "A", "name": "Hal"}).to_string()),
            Ok(generated.to_string()),
        ],
    );
    let (status, _) = api
        .call("POST", "/lists/A/members", Some(json!({"name": "Hal"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let body = api.get("/lists/B/members").await;
    assert_eq!(body, generated);
}

/// Notes whose summary declares its `owner` in two `allOf` parts.
const NOTES: &str = r##"
openapi: 3.1.0
info: {title: Notes, version: "1"}
paths:
  /notes:
    get:
      operationId: listNotes
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/NoteItem"}}}}}
    post:
      operationId: createNote
      requestBody: {required: true, content: {application/json: {schema: {type: object, additionalProperties: true}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
  /notes/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getNote
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
components:
  schemas:
    Note:
      type: object
      required: [id]
      properties:
        id: {type: string}
        owner: {type: object, properties: {id: {type: string}, name: {type: string}, email: {type: string}}}
    NoteItem:
      allOf:
        - type: object
          required: [id]
          properties:
            id: {type: string}
            owner: {type: object, properties: {id: {type: string}}}
        - type: object
          properties:
            owner: {type: object, properties: {name: {type: string}}}
"##;

/// Two `allOf` parts declaring the same member: the overlay keeps what either declares.
#[tokio::test]
async fn all_of_parts_of_a_member_are_merged() {
    let api = api_of(
        NOTES,
        true,
        vec![
            Ok(
                json!({"id": "n1", "owner": {"id": "o1", "name": "Olu", "email": "o@x"}})
                    .to_string(),
            ),
            Ok(json!([{"id": "n1", "owner": {"id": "o1"}}]).to_string()),
        ],
    );
    let (status, _) = api.call("POST", "/notes", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let body = api.get("/notes").await;
    assert_eq!(
        body,
        json!([{"id": "n1", "owner": {"id": "o1", "name": "Olu"}}])
    );
}

fn labelled() -> Value {
    json!([
        with(ticket("t1", "open"), "labels", json!(["bug", "Urgent"])),
        with(ticket("t2", "open"), "labels", json!([" bug "])),
        with(ticket("t3", "open"), "labels", json!(["bugfix", "urgent"])),
        ticket("t4", "open"),
        with(ticket("t5", "open"), "labels", json!(["docs"])),
    ])
}

/// A text parameter naming an array of strings keeps the items whose array holds any
/// of its comma-separated parts, as an `enum` set keeps any of its values: whole,
/// trimmed and ignoring case. An item without the array stays.
#[tokio::test]
async fn an_array_filter_keeps_items_holding_any_part() {
    let api = api_of(
        TICKETS,
        true,
        vec![Ok(labelled().to_string()), Ok(labelled().to_string())],
    );
    let body = api.get("/tickets?labels=bug,urgent").await;
    assert_eq!(ids(&body), ["t1", "t2", "t3", "t4"]);
    let body = api.get("/tickets?labels=bug").await;
    assert_eq!(ids(&body), ["t1", "t2", "t4"]);
}

/// Only a parameter whose whole name names an array filters it: `tag_name`,
/// `exclude_labels`, `without_tags` and `tag_ids` change nothing.
#[tokio::test]
async fn a_word_of_a_parameter_names_no_array() {
    let list = json!([
        with(
            with(ticket("t1", "open"), "tags", json!(["prod"])),
            "labels",
            json!(["bug"])
        ),
        with(ticket("t2", "open"), "tags", json!(["staging"])),
        with(ticket("t3", "open"), "tags", json!([])),
    ]);
    let script = (0..4).map(|_| Ok(list.to_string())).collect();
    let api = api_of(TICKETS, true, script);
    for uri in [
        "/tickets?tag_name=prod",
        "/tickets?exclude_labels=bug",
        "/tickets?without_tags=prod",
        "/tickets?tag_ids=prod",
    ] {
        assert_eq!(api.get(uri).await, list, "{uri}");
    }
}

/// A reference filter drops an item whose reference is `null`; one without the member
/// stays.
#[tokio::test]
async fn a_null_reference_contradicts_a_value() {
    let list = json!([
        with(ticket("t1", "open"), "assignee_id", json!(7)),
        with(ticket("t2", "open"), "assignee_id", Value::Null),
        with(ticket("t3", "open"), "assignee_id", json!(8)),
        ticket("t4", "open"),
    ]);
    let api = api_of(TICKETS, true, vec![Ok(list.to_string())]);
    let body = api.get("/tickets?assignee_id=7").await;
    assert_eq!(ids(&body), ["t1", "t4"]);
}

fn four() -> Value {
    json!([
        ticket("tk-001", "open"),
        ticket("tk-002", "open"),
        ticket("tk-0010", "open"),
        ticket("2", "open"),
    ])
}

/// A parameter naming no member changes nothing, whatever its value: a bound naming an
/// item (`before=tk-001`, with or without `inclusive`), a number (`page=2`), a search
/// (`q`, even for an item's id), and a parameter's own `enum` word (`status=all`).
#[tokio::test]
async fn parameters_naming_nothing_change_nothing() {
    let uris = [
        "/tickets?before=tk-001",
        "/tickets?before=tk-001&inclusive=true",
        "/tickets?page=2",
        "/tickets?q=bug",
        "/tickets?q=tk-001",
        "/tickets?status=all",
        "/tickets?labels=",
    ];
    let script = uris.iter().map(|_| Ok(four().to_string())).collect();
    let api = api_of(TICKETS, true, script);
    for uri in uris {
        let body = api.get(uri).await;
        assert_eq!(body, four(), "{uri}");
    }
}

/// Lists paged by a token: a top-level one named as its query parameter, one inside
/// an object beside the list whose name ends with its parameter, a constant one, and
/// a list with no token at all.
const PAGES: &str = r##"
openapi: 3.1.0
info: {title: Pages, version: "1"}
paths:
  /users:
    get:
      operationId: listUsers
      parameters:
        - {name: page_size, in: query, schema: {type: integer}}
        - {name: next_page_token, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [users]
                properties:
                  users: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next_page_token: {type: string}
                  total_records: {type: integer}
  /users/{id}:
    get:
      operationId: getUser
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
  /deals:
    get:
      operationId: listDeals
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: cursor, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  additional_data:
                    type: object
                    properties:
                      next_cursor: {type: [string, "null"]}
  /frozen:
    get:
      operationId: listFrozen
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: cursor, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  items: {type: array, items: {$ref: "#/components/schemas/User"}}
                  cursor: {type: string, const: ""}
  /linked:
    get:
      operationId: listLinked
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: page, in: query, schema: {type: integer}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  items: {type: array, items: {$ref: "#/components/schemas/User"}}
                  links: {type: object, properties: {next: {type: string}}}
components:
  schemas:
    User:
      type: object
      required: [id]
      properties: {id: {type: string}, name: {type: string}}
"##;

fn users(n: usize) -> Vec<Value> {
    (1..=n)
        .map(|i| json!({"id": format!("u{i}"), "name": format!("N{i}")}))
        .collect()
}

fn ids_in<'a>(body: &'a Value, member: &str) -> Vec<&'a str> {
    body[member]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["id"].as_str().unwrap())
        .collect()
}

/// A page cut short gets a token the next request serves the rest by, page by page,
/// with no model call; the last page has none, and the total is as generated.
#[tokio::test]
async fn a_cut_page_mints_a_token_for_the_rest() {
    let answer = json!({"users": users(5), "next_page_token": "model-token", "total_records": 5});
    let api = api_of(PAGES, true, vec![Ok(answer.to_string())]);
    let first = api.get("/users?page_size=2").await;
    assert_eq!(ids_in(&first, "users"), ["u1", "u2"]);
    let token = first["next_page_token"].as_str().unwrap().to_string();
    assert!(!token.is_empty() && token != "model-token", "{first}");
    let second = api
        .get(&format!("/users?page_size=2&next_page_token={token}"))
        .await;
    assert_eq!(ids_in(&second, "users"), ["u3", "u4"], "{second}");
    assert_eq!(second["total_records"], 5);
    let again = second["next_page_token"].as_str().unwrap().to_string();
    assert!(!again.is_empty() && again != token, "{second}");
    // No size asked: the first page's.
    let last = api.get(&format!("/users?next_page_token={again}")).await;
    assert_eq!(ids_in(&last, "users"), ["u5"], "{last}");
    assert!(
        last.get("next_page_token")
            .is_none_or(|t| t.as_str() == Some("")),
        "{last}"
    );
    // The first page again is the same page, with a token that still serves.
    assert_eq!(api.get("/users?page_size=2").await, first);
    assert_eq!(api.provider.requests().len(), 1);
}

/// The rest of a cut page is the session's: a read of one of its items is answered
/// from state.
#[tokio::test]
async fn the_rest_of_a_cut_page_is_stored() {
    let answer = json!({"users": users(3)});
    let api = api_of(PAGES, true, vec![Ok(answer.to_string())]);
    api.get("/users?page_size=1").await;
    let user = api.get("/users/u3").await;
    assert_eq!(user["name"], "N3");
    assert_eq!(api.provider.requests().len(), 1);
}

/// A token inside an object beside the list, named `next_cursor` for `cursor`, and a
/// last page whose token is `null`.
#[tokio::test]
async fn a_nested_cursor_pages_the_rest() {
    let answer = json!({"data": users(3), "additional_data": {"next_cursor": null}});
    let api = api_of(PAGES, true, vec![Ok(answer.to_string())]);
    let first = api.get("/deals?limit=2").await;
    assert_eq!(ids_in(&first, "data"), ["u1", "u2"]);
    let cursor = first["additional_data"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let last = api.get(&format!("/deals?limit=2&cursor={cursor}")).await;
    assert_eq!(ids_in(&last, "data"), ["u3"]);
    assert_eq!(
        last["additional_data"]["next_cursor"],
        Value::Null,
        "{last}"
    );
    assert_eq!(api.provider.requests().len(), 1);
}

/// A token the session did not mint is the model's to answer.
#[tokio::test]
async fn an_unknown_token_is_generated() {
    let page = json!({"users": users(1)});
    let api = api_of(
        PAGES,
        true,
        vec![Ok(page.to_string()), Ok(page.to_string())],
    );
    api.get("/users?page_size=2&next_page_token=0123456789abcdef0123456789abcdef01234567")
        .await;
    api.get("/users?page_size=2&next_page_token=elsewhere")
        .await;
    assert_eq!(api.provider.requests().len(), 2);
}

/// A page that is not cut keeps the model's token, a token the schema holds constant
/// is not minted, and a list with no token member pairing a parameter only is cut.
#[tokio::test]
async fn no_token_where_the_contract_has_no_place_for_one() {
    let whole = json!({"users": users(2), "next_page_token": "model-token"});
    let frozen = json!({"items": users(3), "cursor": ""});
    let linked = json!({"items": users(3), "links": {"next": "/linked?page=2"}});
    let api = api_of(
        PAGES,
        true,
        vec![
            Ok(whole.to_string()),
            Ok(frozen.to_string()),
            Ok(linked.to_string()),
        ],
    );
    assert_eq!(api.get("/users?page_size=2").await, whole);
    let body = api.get("/frozen?limit=2").await;
    assert_eq!(body["cursor"], "");
    assert_eq!(ids_in(&body, "items"), ["u1", "u2"]);
    let body = api.get("/linked?limit=2").await;
    assert_eq!(ids_in(&body, "items"), ["u1", "u2"]);
    assert_eq!(body["links"], json!({"next": "/linked?page=2"}));
}

/// With consistency off, nothing is cut or minted.
#[tokio::test]
async fn consistency_off_mints_nothing() {
    let answer = json!({"users": users(3), "next_page_token": "model-token"});
    let api = api_of(PAGES, false, vec![Ok(answer.to_string())]);
    assert_eq!(api.get("/users?page_size=2").await, answer);
}
