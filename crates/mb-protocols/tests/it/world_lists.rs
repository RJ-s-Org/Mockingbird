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
use mb_state::{CachingStateStore, MemoryStore, StateStore};
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
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    api_over(contract, consistency, script, store)
}

/// [`api_of`] over a store that caches answers, as a served mock's does.
fn cached_api_of(contract: &str, script: Vec<Result<String, LlmError>>) -> Api {
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600))) as Arc<dyn StateStore>;
    let cached = CachingStateStore::wrap(
        store,
        Duration::from_secs(3600),
        mb_state::DEFAULT_CACHE_ENTRIES,
    );
    api_over(contract, true, script, cached)
}

fn api_over(
    contract: &str,
    consistency: bool,
    script: Vec<Result<String, LlmError>>,
    store: Arc<dyn StateStore>,
) -> Api {
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut rt = MockRuntime::new(
        "lists",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        store,
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

    /// [`Api::get`] in session `session`.
    async fn get_in(&self, session: &str, uri: &str) -> Value {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("x-mockingbird-session", session)
            .body(Body::empty())
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
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
        - {name: name, in: query, schema: {type: string}}
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
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getUser
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
    patch:
      operationId: updateUser
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
    delete:
      operationId: deleteUser
      responses:
        "204": {description: gone}
  /feed:
    get:
      operationId: listFeed
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
                  has_more: {type: boolean}
                  next_cursor: {type: string}
  /orders:
    get:
      operationId: listOrders
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: status, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  status: {type: string}
                  data: {type: array, items: {$ref: "#/components/schemas/Order"}}
  /search:
    get:
      operationId: search
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: query, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  query: {type: string}
                  results: {type: array, items: {$ref: "#/components/schemas/User"}}
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
    Order:
      type: object
      required: [id]
      properties: {id: {type: string}, status: {type: string}}
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
    let api = cached_api_of(PAGES, vec![Ok(answer.to_string())]);
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

/// The first page of five users, two to a page, and its token.
async fn first_of_five(api: &Api, query: &str) -> String {
    let first = api.get(&format!("/users?page_size=2{query}")).await;
    assert_eq!(ids_in(&first, "users"), ["u1", "u2"], "{first}");
    first["next_page_token"].as_str().unwrap().to_string()
}

fn five() -> Result<String, LlmError> {
    Ok(json!({"users": users(5)}).to_string())
}

/// A record deleted after the cut leaves the page it would be served on, and one
/// deleted before is not kept for a later page at all.
#[tokio::test]
async fn a_deleted_item_leaves_a_served_page() {
    let api = api_of(PAGES, true, vec![five(), Ok(String::new())]);
    let token = first_of_five(&api, "").await;
    let (status, _) = api.call("DELETE", "/users/u3", None).await;
    assert!(status.is_success(), "{status}");
    let second = api
        .get(&format!("/users?page_size=2&next_page_token={token}"))
        .await;
    assert_eq!(ids_in(&second, "users"), ["u4"], "{second}");

    let api = api_of(PAGES, true, vec![Ok(String::new()), five()]);
    let (status, _) = api.call("DELETE", "/users/u4", None).await;
    assert!(status.is_success(), "{status}");
    let token = first_of_five(&api, "").await;
    let second = api
        .get(&format!("/users?page_size=2&next_page_token={token}"))
        .await;
    assert_eq!(ids_in(&second, "users"), ["u3", "u5"], "{second}");
}

/// A served page shows what the session wrote since, and the first request's filters
/// apply to it: `u3` renamed is no `name=N` user.
#[tokio::test]
async fn a_served_page_is_overlaid_and_filtered() {
    let renamed = json!({"id": "u3", "name": "Z"});
    let api = api_of(PAGES, true, vec![five(), Ok(renamed.to_string())]);
    let token = first_of_five(&api, "&name=N").await;
    let (status, _) = api
        .call("PATCH", "/users/u3", Some(json!({"name": "Z"})))
        .await;
    assert_eq!(status, StatusCode::OK);
    let second = api
        .get(&format!(
            "/users?page_size=2&name=N&next_page_token={token}"
        ))
        .await;
    assert_eq!(ids_in(&second, "users"), ["u4"], "{second}");
    assert_eq!(api.provider.requests().len(), 2);
}

/// A token is bound to the first request's filters: with others it is the model's.
#[tokio::test]
async fn a_token_with_other_filters_is_generated() {
    let other = json!({"users": [{"id": "x1", "name": "X"}]});
    let api = api_of(PAGES, true, vec![five(), Ok(other.to_string())]);
    let token = first_of_five(&api, "&name=N").await;
    let body = api
        .get(&format!(
            "/users?page_size=2&name=X&next_page_token={token}"
        ))
        .await;
    assert_eq!(ids_in(&body, "users"), ["x1"], "{body}");
    assert_eq!(api.provider.requests().len(), 2);
}

/// The same token twice serves the same page, with the same next token.
#[tokio::test]
async fn a_retried_token_serves_the_same_page() {
    let api = api_of(PAGES, true, vec![five()]);
    let token = first_of_five(&api, "").await;
    let uri = format!("/users?page_size=2&next_page_token={token}");
    let once = api.get(&uri).await;
    let twice = api.get(&uri).await;
    assert_eq!(once, twice);
    assert_eq!(ids_in(&once, "users"), ["u3", "u4"]);
    assert_eq!(api.provider.requests().len(), 1);
}

/// A member named as a filter of the items (`status`) is no cursor, nor is one that
/// echoes the request (`query`): neither gets a token, and the list is only cut.
#[tokio::test]
async fn a_filter_or_an_echo_is_no_cursor() {
    let orders = json!({
        "status": "ok",
        "data": [{"id": "o1", "status": "paid"}, {"id": "o2", "status": "paid"}],
    });
    let found = json!({"query": "ann", "results": users(3)});
    let api = api_of(
        PAGES,
        true,
        vec![Ok(orders.to_string()), Ok(found.to_string())],
    );
    let body = api.get("/orders?limit=1").await;
    assert_eq!(body["status"], "ok", "{body}");
    assert_eq!(ids_in(&body, "data"), ["o1"]);
    let body = api.get("/search?limit=1&query=ann").await;
    assert_eq!(body["query"], "ann", "{body}");
    assert_eq!(ids_in(&body, "results"), ["u1"]);
}

/// A `has_more` beside the cursor that the first page set is `false` on the last.
#[tokio::test]
async fn the_last_page_has_no_more() {
    let answer = json!({"data": users(3), "has_more": true, "next_cursor": "made-up"});
    let api = api_of(PAGES, true, vec![Ok(answer.to_string())]);
    let first = api.get("/feed?limit=2").await;
    assert_eq!(first["has_more"], true, "{first}");
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    assert_ne!(cursor, "made-up");
    let last = api.get(&format!("/feed?limit=2&cursor={cursor}")).await;
    assert_eq!(ids_in(&last, "data"), ["u3"]);
    assert_eq!(last["has_more"], false, "{last}");
    assert_eq!(api.provider.requests().len(), 1);
}

/// An empty cursor (`cursor=`) asks for the first page, which mints as without one.
#[tokio::test]
async fn an_empty_cursor_is_no_cursor() {
    let answer = json!({"data": users(3), "next_cursor": ""});
    let api = api_of(PAGES, true, vec![Ok(answer.to_string())]);
    let first = api.get("/feed?limit=2&cursor=").await;
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    assert!(!cursor.is_empty(), "{first}");
    let last = api.get(&format!("/feed?limit=2&cursor={cursor}")).await;
    assert_eq!(ids_in(&last, "data"), ["u3"]);
    assert_eq!(api.provider.requests().len(), 1);
}

/// Tasks a client creates, then lists one to a page: by a cursor, or by page number.
fn stored_pages(list: &str) -> String {
    format!(
        r##"
openapi: 3.1.0
info: {{title: Stored pages, version: "1"}}
paths:
  /tasks:
{list}
    post:
      operationId: createTask
      requestBody:
        required: true
        content: {{application/json: {{schema: {{type: object, additionalProperties: true}}}}}}
      responses:
        "201": {{description: made, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Task"}}}}}}}}
  /tasks/{{id}}:
    parameters: [{{name: id, in: path, required: true, schema: {{type: string}}}}]
    get:
      operationId: getTask
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Task"}}}}}}}}
components:
  schemas:
    Task:
      type: object
      required: [id, title, status]
      properties:
        id: {{type: string}}
        title: {{type: string}}
        status: {{type: string, enum: [open, done]}}
"##
    )
}

const BY_CURSOR: &str = r##"    get:
      operationId: listTasks
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: cursor, in: query, schema: {type: string}}
        - {name: status, in: query, schema: {type: string, enum: [open, done]}}
        - {name: kind, in: query, schema: {type: string, enum: [title, other]}}
        - {name: fields, in: query, schema: {type: string}}
        - {name: offset, in: query, schema: {type: integer, minimum: 0}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [data]
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  next_cursor: {type: [string, "null"]}
                  has_more: {type: boolean}
                  total: {type: integer}"##;

const BY_NUMBER: &str = r##"    get:
      operationId: listTasks
      parameters:
        - {name: per_page, in: query, schema: {type: integer}}
        - {name: page, in: query, schema: {type: integer}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [data]
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  page: {type: integer}
                  total_pages: {type: integer}"##;

fn stored_task(id: &str, title: &str, status: &str) -> Value {
    json!({"id": id, "title": title, "status": status})
}

/// Creates `t1` and `t2` through the API.
async fn two_tasks(api: &Api) {
    for task in [json!({"title": "A"}), json!({"title": "B"})] {
        let (status, body) = api.call("POST", "/tasks", Some(task)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
}

/// The answers creating `t1` (open) and `t2` (with status `second`).
fn created(second: &str) -> Vec<Result<String, LlmError>> {
    vec![
        Ok(stored_task("t1", "A", "open").to_string()),
        Ok(stored_task("t2", "B", second).to_string()),
    ]
}

/// A full page of one, with a `null` token, while the session holds another matching
/// task: the answer gets a token, the total counts both, and the next page is the
/// other task, served from the store.
#[tokio::test]
async fn a_full_page_with_a_stored_rest_mints_a_token() {
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null, "total": 1});
    script.push(Ok(page.to_string()));
    let api = cached_api_of(&stored_pages(BY_CURSOR), script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1").await;
    assert_eq!(ids_in(&first, "data"), ["t1"], "{first}");
    let cursor = first["next_cursor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!cursor.is_empty(), "{first}");
    assert_eq!(first["total"], 2, "{first}");
    let second = api.get(&format!("/tasks?limit=1&cursor={cursor}")).await;
    assert_eq!(
        second["data"],
        json!([stored_task("t2", "B", "open")]),
        "{second}"
    );
    assert_eq!(second["next_cursor"], Value::Null, "{second}");
    // The first page again is the same page, its token included.
    assert_eq!(api.get("/tasks?limit=1").await, first);
    assert_eq!(api.provider.requests().len(), 3);
}

/// The model's served item stays, whatever the store holds, and an empty token is no
/// token either: both stored tasks the page does not show come next.
#[tokio::test]
async fn a_made_up_item_stays_and_the_stored_ones_follow() {
    let mut script = created("open");
    let page = json!({"data": [stored_task("x9", "X", "open")], "next_cursor": ""});
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1&cursor=").await;
    assert_eq!(ids_in(&first, "data"), ["x9"], "{first}");
    let cursor = first["next_cursor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!cursor.is_empty(), "{first}");
    let second = api.get(&format!("/tasks?limit=1&cursor={cursor}")).await;
    let next = second["next_cursor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!next.is_empty(), "{second}");
    let third = api.get(&format!("/tasks?limit=1&cursor={next}")).await;
    let mut seen = [ids_in(&second, "data"), ids_in(&third, "data")].concat();
    seen.sort_unstable();
    assert_eq!(seen, ["t1", "t2"]);
    assert_eq!(third["next_cursor"], Value::Null, "{third}");
    assert_eq!(api.provider.requests().len(), 3);
}

/// A filter the other stored task does not hold leaves nothing for a next page, and a
/// page that is not full mints nothing either.
#[tokio::test]
async fn a_filter_excluding_the_stored_rest_mints_nothing() {
    let mut script = created("done");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    assert_eq!(api.get("/tasks?limit=1&status=open").await, page);
    assert_eq!(api.get("/tasks?limit=2").await, page);
}

const PROJECT_PAGES: &str = r##"
openapi: 3.1.0
info: {title: Project pages, version: "1"}
paths:
  /projects/{project_id}/tasks:
    parameters: [{name: project_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listTasks
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
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  next_cursor: {type: [string, "null"]}
    post:
      operationId: createTask
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
components:
  schemas:
    Task:
      type: object
      required: [id, project_id, title]
      properties: {id: {type: string}, project_id: {type: string}, title: {type: string}}
"##;

/// A task of another project is no rest of this project's page; one of its own is.
#[tokio::test]
async fn a_stored_rest_is_of_the_page_s_parent() {
    let first = json!({"data": [task("t1", "p1", "A")], "next_cursor": null});
    let api = api_of(
        PROJECT_PAGES,
        true,
        vec![
            Ok(task("t1", "p1", "A").to_string()),
            Ok(task("t2", "p2", "B").to_string()),
            Ok(first.to_string()),
            Ok(task("t3", "p1", "C").to_string()),
            Ok(first.to_string()),
        ],
    );
    for (project, title) in [("p1", "A"), ("p2", "B")] {
        let (status, _) = api
            .call(
                "POST",
                &format!("/projects/{project}/tasks"),
                Some(json!({"title": title})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    assert_eq!(api.get("/projects/p1/tasks?limit=1").await, first);
    let (status, _) = api
        .call("POST", "/projects/p1/tasks", Some(json!({"title": "C"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let body = api.get("/projects/p1/tasks?limit=1&cursor=").await;
    let cursor = body["next_cursor"].as_str().unwrap_or_default().to_string();
    assert!(!cursor.is_empty(), "{body}");
    let next = api
        .get(&format!("/projects/p1/tasks?limit=1&cursor={cursor}"))
        .await;
    assert_eq!(next["data"], json!([task("t3", "p1", "C")]), "{next}");
}

/// Page numbers have no token to mint: the answer is the model's, and so is the next
/// page.
#[tokio::test]
async fn page_numbers_are_unchanged_by_the_store() {
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "page": 1, "total_pages": 1});
    let next = json!({"data": [stored_task("t2", "B", "open")], "page": 2, "total_pages": 2});
    script.push(Ok(page.to_string()));
    script.push(Ok(next.to_string()));
    let api = api_of(&stored_pages(BY_NUMBER), true, script);
    two_tasks(&api).await;
    assert_eq!(api.get("/tasks?per_page=1&page=1").await, page);
    assert_eq!(api.get("/tasks?per_page=1&page=2").await, next);
    assert_eq!(api.provider.requests().len(), 4);
}

/// A second list of the type with no path parameter (`/me/tasks`, beside `/tasks`)
/// may hold only some of its records: neither list is paged from the store.
#[tokio::test]
async fn a_list_beside_another_unscoped_one_is_not_paged_from_the_store() {
    let mine = BY_CURSOR.replace("operationId: listTasks", "operationId: listMyTasks");
    let contract = stored_pages(BY_CURSOR).replace(
        "  /tasks/{id}:",
        &format!("  /me/tasks:\n{mine}\n  /tasks/{{id}}:"),
    );
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    script.push(Ok(page.to_string()));
    let api = api_of(&contract, true, script);
    two_tasks(&api).await;
    assert_eq!(api.get("/me/tasks?limit=1").await, page);
    assert_eq!(api.get("/tasks?limit=1").await, page);
}

/// A list under another name whose items are the same type (`/todos` of `Task`s) is a
/// second list of that type: neither is paged from the store.
#[tokio::test]
async fn a_list_of_the_same_items_under_another_name_counts() {
    let todos = BY_CURSOR.replace("operationId: listTasks", "operationId: listTodos");
    let contract = stored_pages(BY_CURSOR).replace(
        "  /tasks/{id}:",
        &format!("  /todos:\n{todos}\n  /tasks/{{id}}:"),
    );
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    let api = api_of(&contract, true, script);
    two_tasks(&api).await;
    assert_eq!(api.get("/tasks?limit=1").await, page);
}

/// A token the model gave stays, and so does its count, when no stored record is left
/// for a next page: the other task is `done`, which the filter leaves out.
#[tokio::test]
async fn the_model_s_own_token_stays() {
    let mut script = created("done");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": "abc", "total": 50});
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    assert_eq!(api.get("/tasks?limit=1&status=open").await, page);
}

/// A minted token comes with the boolean beside it saying there is more, and the last
/// page says there is none.
#[tokio::test]
async fn a_minted_token_says_there_is_more() {
    let mut script = created("open");
    let page =
        json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null, "has_more": false});
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1").await;
    assert_eq!(first["has_more"], true, "{first}");
    let cursor = first["next_cursor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let last = api.get(&format!("/tasks?limit=1&cursor={cursor}")).await;
    assert_eq!(ids_in(&last, "data"), ["t2"], "{last}");
    assert_eq!(last["has_more"], false, "{last}");
}

/// A value of a parameter with an `enum` is no list of fields, even when it names a
/// member (`kind=title`): no stored task shows it, so nothing is paged. A free text
/// parameter naming members (`fields=title,status`) is one.
#[tokio::test]
async fn an_enum_value_naming_a_member_is_no_projection() {
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    assert_eq!(api.get("/tasks?limit=1&kind=title").await, page);
    let body = api.get("/tasks?limit=1&fields=title,status").await;
    assert!(
        body["next_cursor"].as_str().is_some_and(|c| !c.is_empty()),
        "{body}"
    );
}

/// [`BY_CURSOR`] with a second boolean beside the token.
fn with_livemode() -> String {
    stored_pages(&BY_CURSOR.replace(
        "has_more: {type: boolean}",
        "has_more: {type: boolean}\n                  livemode: {type: boolean}",
    ))
}

/// With two booleans beside the token, which says there is more is not known: a
/// minted token flips neither, and `livemode` stays `false`.
#[tokio::test]
async fn a_minted_token_flips_no_boolean_beside_another() {
    let mut script = created("open");
    let page = json!({
        "data": [stored_task("t1", "A", "open")],
        "next_cursor": null,
        "has_more": false,
        "livemode": false,
    });
    script.push(Ok(page.to_string()));
    let api = api_of(&with_livemode(), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1").await;
    assert!(
        first["next_cursor"].as_str().is_some_and(|c| !c.is_empty()),
        "{first}"
    );
    assert_eq!(first["livemode"], false, "{first}");
    assert_eq!(first["has_more"], false, "{first}");
}

/// The last page leaves two booleans beside its token as the first page had them.
#[tokio::test]
async fn the_last_page_flips_no_boolean_beside_another() {
    let page = json!({
        "data": [stored_task("t1", "A", "open"), stored_task("t2", "B", "open")],
        "next_cursor": null,
        "has_more": true,
        "livemode": true,
    });
    let api = api_of(&with_livemode(), true, vec![Ok(page.to_string())]);
    let first = api.get("/tasks?limit=1").await;
    let cursor = first["next_cursor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!cursor.is_empty(), "{first}");
    let last = api.get(&format!("/tasks?limit=1&cursor={cursor}")).await;
    assert_eq!(ids_in(&last, "data"), ["t2"], "{last}");
    assert_eq!(last["has_more"], true, "{last}");
    assert_eq!(last["livemode"], true, "{last}");
}

/// Stripe's shape, a page cut to size: the one boolean beside the token says there is
/// more, and the last page says there is none.
#[tokio::test]
async fn a_cut_page_s_one_boolean_says_there_is_more() {
    let answer = json!({"data": users(3), "has_more": false, "next_cursor": null});
    let api = api_of(PAGES, true, vec![Ok(answer.to_string())]);
    let first = api.get("/feed?limit=2").await;
    assert_eq!(first["has_more"], true, "{first}");
    let cursor = first["next_cursor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let last = api.get(&format!("/feed?limit=2&cursor={cursor}")).await;
    assert_eq!(ids_in(&last, "data"), ["u3"]);
    assert_eq!(last["has_more"], false, "{last}");
}

/// A number at its parameter's `minimum` (`offset=0`) asks for nothing; another value
/// leaves the page as the model answered.
#[tokio::test]
async fn a_number_at_its_minimum_constrains_nothing() {
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    let body = api.get("/tasks?limit=1&offset=0").await;
    assert!(
        body["next_cursor"].as_str().is_some_and(|c| !c.is_empty()),
        "{body}"
    );
    assert_eq!(api.get("/tasks?limit=1&offset=3").await, page);
}

/// Zoho's shape: a page number from 1 beside `per_page`, and a `page_context` that
/// echoes them and says whether more pages follow.
const BY_PAGE: &str = r##"    get:
      operationId: listTasks
      parameters:
        - {name: per_page, in: query, schema: {type: integer, minimum: 1, maximum: 200, default: 200}}
        - {name: page, in: query, schema: {type: integer, minimum: 1, default: 1}}
        - {name: status, in: query, schema: {type: string, enum: [open, done]}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [code, data, page_context]
                properties:
                  code: {type: integer}
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  page_context:
                    type: object
                    required: [page, per_page, has_more_page]
                    properties:
                      page: {type: integer}
                      per_page: {type: integer}
                      has_more_page: {type: boolean}"##;

/// A page of `tasks` in [`BY_PAGE`]'s shape.
fn zoho_page(tasks: &[Value], page: u64, more: bool) -> Value {
    json!({
        "code": 0,
        "data": tasks,
        "page_context": {"page": page, "per_page": 1, "has_more_page": more},
    })
}

/// Page 1 of one, while the session holds another task: page 1 is as the model
/// answered it, and page 2 is the other task, served from the store, saying it is page
/// 2. The model saw one full page, so its `has_more_page` says nothing of the store's
/// records: it stays as given.
#[tokio::test]
async fn page_two_by_number_is_the_stored_rest() {
    let mut script = created("open");
    script.push(Ok(
        zoho_page(&[stored_task("t1", "A", "open")], 1, true).to_string()
    ));
    let api = cached_api_of(&stored_pages(BY_PAGE), script);
    two_tasks(&api).await;
    let first = api.get("/tasks?per_page=1&page=1").await;
    assert_eq!(first, zoho_page(&[stored_task("t1", "A", "open")], 1, true));
    let second = api.get("/tasks?per_page=1&page=2").await;
    assert_eq!(
        second,
        zoho_page(&[stored_task("t2", "B", "open")], 2, true),
        "{second}"
    );
    // The first page again is the same page.
    assert_eq!(api.get("/tasks?per_page=1&page=1").await, first);
    assert_eq!(api.provider.requests().len(), 3);
}

/// A page past what the run holds, and a page with other filter values, are the
/// model's.
#[tokio::test]
async fn a_page_past_the_run_or_of_other_filters_is_the_model_s() {
    let mut script = created("open");
    let first = zoho_page(&[stored_task("t1", "A", "open")], 1, false);
    let past = zoho_page(&[], 3, false);
    let other = zoho_page(&[stored_task("t9", "Z", "open")], 2, false);
    script.push(Ok(first.to_string()));
    script.push(Ok(past.to_string()));
    script.push(Ok(other.to_string()));
    let api = api_of(&stored_pages(BY_PAGE), true, script);
    two_tasks(&api).await;
    api.get("/tasks?per_page=1").await;
    assert_eq!(api.get("/tasks?per_page=1&page=3").await, past);
    assert_eq!(api.get("/tasks?per_page=1&page=2&status=open").await, other);
    assert_eq!(api.provider.requests().len(), 5);
}

/// WooCommerce's shape, a bare array: a page cut to its size leaves the rest for later
/// pages, and a larger page size starts where it says.
#[tokio::test]
async fn a_cut_page_by_number_serves_the_rest() {
    let list = r##"    get:
      operationId: listTasks
      parameters:
        - {name: per_page, in: query, schema: {type: integer, minimum: 1, maximum: 100, default: 10}}
        - {name: page, in: query, schema: {type: integer, minimum: 1, default: 1}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {type: array, items: {$ref: "#/components/schemas/Task"}}"##;
    let answer = json!([
        stored_task("t1", "A", "open"),
        stored_task("t2", "B", "open"),
        stored_task("t3", "C", "done"),
    ]);
    let api = api_of(&stored_pages(list), true, vec![Ok(answer.to_string())]);
    let first = api.get("/tasks?per_page=1&page=1").await;
    assert_eq!(first, json!([stored_task("t1", "A", "open")]));
    let second = api.get("/tasks?per_page=1&page=2").await;
    assert_eq!(second, json!([stored_task("t2", "B", "open")]));
    let third = api.get("/tasks?page=3&per_page=1").await;
    assert_eq!(third, json!([stored_task("t3", "C", "done")]));
    // Two to a page, the second starts at the third item.
    let wide = api.get("/tasks?per_page=2&page=2").await;
    assert_eq!(wide, json!([stored_task("t3", "C", "done")]));
    assert_eq!(api.provider.requests().len(), 1);
}

/// Box's shape: an offset from 0 beside `limit`, with a total and an echo of both.
const BY_OFFSET: &str = r##"    get:
      operationId: listTasks
      parameters:
        - {name: offset, in: query, schema: {type: integer, format: int64, minimum: 0, default: 0}}
        - {name: limit, in: query, schema: {type: integer, minimum: 1, maximum: 1000, default: 100}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [total_count, entries, offset, limit]
                properties:
                  total_count: {type: integer}
                  entries: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  offset: {type: integer}
                  limit: {type: integer}"##;

/// An offset past the first page is the stored rest, and the total counts both.
#[tokio::test]
async fn an_offset_reaches_the_stored_rest() {
    let mut script = created("open");
    let page = json!({
        "total_count": 1,
        "entries": [stored_task("t1", "A", "open")],
        "offset": 0,
        "limit": 1,
    });
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(BY_OFFSET), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?offset=0&limit=1").await;
    assert_eq!(ids_in(&first, "entries"), ["t1"], "{first}");
    assert_eq!(first["total_count"], 2, "{first}");
    let next = api.get("/tasks?offset=1&limit=1").await;
    assert_eq!(
        next,
        json!({
            "total_count": 2,
            "entries": [stored_task("t2", "B", "open")],
            "offset": 1,
            "limit": 1,
        }),
        "{next}"
    );
    assert_eq!(api.provider.requests().len(), 3);
}

/// Page 2 of `list` by its page parameter `param` is the model's: the parameter is not
/// known by its schema.
async fn page_two_is_generated(list: &str, param: &str) {
    let mut script = created("open");
    let first = zoho_page(&[stored_task("t1", "A", "open")], 1, false);
    let second = zoho_page(&[stored_task("t1", "A", "open")], 2, false);
    script.push(Ok(first.to_string()));
    script.push(Ok(second.to_string()));
    let api = api_of(&stored_pages(list), true, script);
    two_tasks(&api).await;
    assert_eq!(
        api.get(&format!("/tasks?per_page=1&{param}=1")).await,
        first
    );
    assert_eq!(
        api.get(&format!("/tasks?per_page=1&{param}=2")).await,
        second
    );
    assert_eq!(api.provider.requests().len(), 4);
}

/// A page number with a `minimum` but no `default` is not known to be one.
#[tokio::test]
async fn a_page_with_no_default_is_unchanged() {
    let list = BY_PAGE.replace(
        "{type: integer, minimum: 1, default: 1}",
        "{type: integer, minimum: 1}",
    );
    page_two_is_generated(&list, "page").await;
}

/// A `page` whose schema counts from 0 (Trello's) says two things: nothing changes.
#[tokio::test]
async fn a_page_counting_from_zero_is_unchanged() {
    let list = BY_PAGE.replace(
        "{type: integer, minimum: 1, default: 1}",
        "{type: integer, minimum: 0, default: 0}",
    );
    page_two_is_generated(&list, "page").await;
}

/// A second parameter that could be the page number (`copies`).
const COPIES: &str = "        - {name: copies, in: query, schema: {type: integer, minimum: 1, default: 1}}\n        - {name: status,";

/// Two parameters that could each be the page number, neither named so: which one is
/// not known, and nothing changes.
#[tokio::test]
async fn two_page_numbers_are_unchanged() {
    let list = BY_PAGE
        .replace("name: page,", "name: sheet,")
        .replace("        - {name: status,", COPIES);
    page_two_is_generated(&list, "sheet").await;
}

/// Of two parameters that could be the page number, the one named `page` is.
#[tokio::test]
async fn the_one_named_page_is_the_page_number() {
    let list = BY_PAGE.replace("        - {name: status,", COPIES);
    let mut script = created("open");
    script.push(Ok(
        zoho_page(&[stored_task("t1", "A", "open")], 1, false).to_string()
    ));
    let api = api_of(&stored_pages(&list), true, script);
    two_tasks(&api).await;
    api.get("/tasks?per_page=1&page=1").await;
    let second = api.get("/tasks?per_page=1&page=2").await;
    assert_eq!(ids_in(&second, "data"), ["t2"], "{second}");
    assert_eq!(api.provider.requests().len(), 3);
}

/// With consistency off, page 2 is the model's.
#[tokio::test]
async fn consistency_off_serves_no_page_number() {
    let answer = json!({
        "code": 0,
        "data": [stored_task("t1", "A", "open"), stored_task("t2", "B", "open")],
        "page_context": {"page": 1, "per_page": 1, "has_more_page": false},
    });
    let second = zoho_page(&[stored_task("t1", "A", "open")], 2, false);
    let api = api_of(
        &stored_pages(BY_PAGE),
        false,
        vec![Ok(answer.to_string()), Ok(second.to_string())],
    );
    assert_eq!(api.get("/tasks?per_page=1&page=1").await, answer);
    assert_eq!(api.get("/tasks?per_page=1&page=2").await, second);
}

/// A list of tasks under `per_page` and `params`, answering `schema`.
fn list_of(params: &str, schema: &str) -> String {
    format!(
        r##"    get:
      operationId: listTasks
      parameters:
        - {{name: per_page, in: query, schema: {{type: integer, minimum: 1, maximum: 100, default: 10}}}}
{params}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {schema}"##
    )
}

const TASK_ARRAY: &str = r##"{type: array, items: {$ref: "#/components/schemas/Task"}}"##;

/// A bare array of three, cut to one, then asked again with `query`: an integer with
/// page-like bounds but no evidence it is a page is no page, so the model answers.
async fn a_bounded_integer_is_no_page(param: &str, query: &str) {
    let three = json!([
        stored_task("t1", "A", "open"),
        stored_task("t2", "B", "open"),
        stored_task("t3", "C", "open"),
    ]);
    let other = json!([stored_task("t9", "Z", "open")]);
    let api = api_of(
        &stored_pages(&list_of(param, TASK_ARRAY)),
        true,
        vec![Ok(three.to_string()), Ok(other.to_string())],
    );
    assert_eq!(
        api.get("/tasks?per_page=1").await,
        json!([stored_task("t1", "A", "open")])
    );
    assert_eq!(api.get(&format!("/tasks?per_page=1&{query}")).await, other);
    assert_eq!(api.provider.requests().len(), 2);
}

/// `min_rating` from 0 is no offset.
#[tokio::test]
async fn a_minimum_from_zero_is_no_offset() {
    a_bounded_integer_is_no_page(
        "        - {name: min_rating, in: query, schema: {type: integer, minimum: 0, default: 0}}",
        "min_rating=2",
    )
    .await;
}

/// `depth` from 1 is no page number.
#[tokio::test]
async fn a_depth_from_one_is_no_page_number() {
    a_bounded_integer_is_no_page(
        "        - {name: depth, in: query, schema: {type: integer, minimum: 1, default: 1}}",
        "depth=2",
    )
    .await;
}

const PAGE_PARAM: &str =
    "        - {name: page, in: query, schema: {type: integer, minimum: 1, default: 1}}";

/// Page 1 of a cut answer `answer` to `schema`, then page 2: both as `first` and
/// `second`, with one model call.
async fn pages_of(schema: &str, answer: Value, first: Value, second: Value) {
    let api = api_of(
        &stored_pages(&list_of(PAGE_PARAM, schema)),
        true,
        vec![Ok(answer.to_string())],
    );
    assert_eq!(api.get("/tasks?per_page=1&page=1").await, first);
    assert_eq!(api.get("/tasks?per_page=1&page=2").await, second);
    assert_eq!(api.provider.requests().len(), 1);
}

/// A boolean beside the list with no echo of the page beside it (`success`) is no more
/// flag: neither page changes it.
#[tokio::test]
async fn a_boolean_with_no_page_echo_is_no_more_flag() {
    let schema = r##"
                type: object
                properties:
                  success: {type: boolean}
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}"##;
    let (t1, t2) = (
        stored_task("t1", "A", "open"),
        stored_task("t2", "B", "open"),
    );
    pages_of(
        schema,
        json!({"success": true, "data": [t1, t2]}),
        json!({"success": true, "data": [t1]}),
        json!({"success": true, "data": [t2]}),
    )
    .await;
}

/// A boolean in another object than the page's echo (`filters.include_archived`) is no
/// more flag; the echo says the page served.
#[tokio::test]
async fn a_boolean_away_from_the_echo_is_no_more_flag() {
    let schema = r##"
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  page: {type: integer}
                  filters:
                    type: object
                    properties: {include_archived: {type: boolean}}"##;
    let (t1, t2) = (
        stored_task("t1", "A", "open"),
        stored_task("t2", "B", "open"),
    );
    let filters = json!({"include_archived": true});
    pages_of(
        schema,
        json!({"data": [t1, t2], "page": 1, "filters": filters}),
        json!({"data": [t1], "page": 1, "filters": filters}),
        json!({"data": [t2], "page": 2, "filters": filters}),
    )
    .await;
}

/// A flag beside the echo that says the other way (`isLast`): page 1 keeps the
/// model's `false`, and the last page, as page 1 never said `true`, keeps it too.
#[tokio::test]
async fn an_inverted_flag_is_never_flipped() {
    let schema = r##"
                type: object
                properties:
                  values: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  page_context:
                    type: object
                    properties: {page: {type: integer}, isLast: {type: boolean}}"##;
    let (t1, t2) = (
        stored_task("t1", "A", "open"),
        stored_task("t2", "B", "open"),
    );
    pages_of(
        schema,
        json!({"values": [t1, t2], "page_context": {"page": 1, "isLast": false}}),
        json!({"values": [t1], "page_context": {"page": 1, "isLast": false}}),
        json!({"values": [t2], "page_context": {"page": 2, "isLast": false}}),
    )
    .await;
}

/// An offset that is no multiple of the page size starts where it says.
#[tokio::test]
async fn an_offset_off_the_page_grid_starts_where_it_says() {
    let tasks: Vec<Value> = ["t1", "t2", "t3", "t4"]
        .iter()
        .map(|id| stored_task(id, "T", "open"))
        .collect();
    let answer = json!({"total_count": 4, "entries": tasks, "offset": 0, "limit": 2});
    let api = api_of(&stored_pages(BY_OFFSET), true, vec![Ok(answer.to_string())]);
    let first = api.get("/tasks?offset=0&limit=2").await;
    assert_eq!(ids_in(&first, "entries"), ["t1", "t2"], "{first}");
    let shifted = api.get("/tasks?offset=1&limit=2").await;
    assert_eq!(ids_in(&shifted, "entries"), ["t2", "t3"], "{shifted}");
    assert_eq!(shifted["offset"], 1, "{shifted}");
    assert_eq!(shifted["total_count"], 4, "{shifted}");
    let tail = api.get("/tasks?offset=3&limit=2").await;
    assert_eq!(ids_in(&tail, "entries"), ["t4"], "{tail}");
    assert_eq!(api.provider.requests().len(), 1);
}

/// A run is the session's: another session's page 2 is the model's.
#[tokio::test]
async fn another_session_s_page_two_is_the_model_s() {
    let mut script = created("open");
    script.push(Ok(
        zoho_page(&[stored_task("t1", "A", "open")], 1, true).to_string()
    ));
    let theirs = zoho_page(&[stored_task("x1", "X", "open")], 2, false);
    script.push(Ok(theirs.to_string()));
    let api = api_of(&stored_pages(BY_PAGE), true, script);
    two_tasks(&api).await;
    api.get("/tasks?per_page=1&page=1").await;
    assert_eq!(
        api.get_in("other", "/tasks?per_page=1&page=2").await,
        theirs
    );
    let ours = api.get("/tasks?per_page=1&page=2").await;
    assert_eq!(ids_in(&ours, "data"), ["t2"], "{ours}");
    assert_eq!(api.provider.requests().len(), 4);
}

/// A new page 1 that keeps nothing (here, one not full) ends the run: page 2 is the
/// model's again.
#[tokio::test]
async fn a_new_first_page_with_no_rest_ends_the_run() {
    let api = api_of(
        &stored_pages(&list_of(PAGE_PARAM, TASK_ARRAY)),
        true,
        vec![
            Ok(json!([
                stored_task("t1", "A", "open"),
                stored_task("t2", "B", "open")
            ])
            .to_string()),
            Ok(json!([]).to_string()),
            Ok(json!([stored_task("t7", "G", "open")]).to_string()),
        ],
    );
    api.get("/tasks?per_page=1&page=1").await;
    assert_eq!(api.get("/tasks?page=1&per_page=1").await, json!([]));
    assert_eq!(
        api.get("/tasks?per_page=1&page=2").await,
        json!([stored_task("t7", "G", "open")])
    );
}

/// [`BY_PAGE`]'s tasks, which can also be updated and deleted.
fn editable_pages() -> String {
    stored_pages(BY_PAGE).replace(
        "    get:\n      operationId: getTask",
        r##"    patch:
      operationId: updateTask
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
    delete:
      operationId: deleteTask
      responses:
        "204": {description: gone}
    get:
      operationId: getTask"##,
    )
}

/// A task deleted between page 1 and page 2 is not on page 2, which starts with the
/// next; one updated meanwhile shows its update.
#[tokio::test]
async fn page_two_sees_a_delete_and_an_update_since_page_one() {
    let mut script = created("open");
    script.push(Ok(stored_task("t3", "C", "open").to_string()));
    script.push(Ok(
        zoho_page(&[stored_task("t1", "A", "open")], 1, true).to_string()
    ));
    script.push(Ok(stored_task("t3", "C2", "done").to_string()));
    let api = api_of(&editable_pages(), true, script);
    two_tasks(&api).await;
    let (status, _) = api
        .call("POST", "/tasks", Some(json!({"title": "C"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let first = api.get("/tasks?per_page=1&page=1").await;
    assert_eq!(ids_in(&first, "data"), ["t1"], "{first}");
    let (status, _) = api
        .call(
            "PATCH",
            "/tasks/t3",
            Some(json!({"title": "C2", "status": "done"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api.call("DELETE", "/tasks/t2", None).await;
    assert!(status.is_success(), "{status}");
    let second = api.get("/tasks?per_page=1&page=2").await;
    assert_eq!(
        second,
        zoho_page(&[stored_task("t3", "C2", "done")], 2, true),
        "{second}"
    );
    // The delete needs no model call.
    assert_eq!(api.provider.requests().len(), 5);
}

const PROJECT_NUMBERED: &str = r##"
openapi: 3.1.0
info: {title: Project pages, version: "1"}
paths:
  /projects/{project_id}/tasks:
    parameters: [{name: project_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listTasks
      parameters:
        - {name: per_page, in: query, schema: {type: integer, minimum: 1, maximum: 100, default: 10}}
        - {name: page, in: query, schema: {type: integer, minimum: 1, default: 1}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {type: array, items: {$ref: "#/components/schemas/Task"}}
    post:
      operationId: createTask
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
components:
  schemas:
    Task:
      type: object
      required: [id, project_id, title]
      properties: {id: {type: string}, project_id: {type: string}, title: {type: string}}
"##;

/// Page 2 of a project's tasks is its own other task, not another project's; another
/// project's page 2 is the model's.
#[tokio::test]
async fn page_two_is_of_the_page_s_parent() {
    let theirs = json!([task("t8", "p2", "H")]);
    let api = api_of(
        PROJECT_NUMBERED,
        true,
        vec![
            Ok(task("t1", "p1", "A").to_string()),
            Ok(task("t2", "p2", "B").to_string()),
            Ok(task("t3", "p1", "C").to_string()),
            Ok(json!([task("t1", "p1", "A")]).to_string()),
            Ok(theirs.to_string()),
        ],
    );
    for (project, title) in [("p1", "A"), ("p2", "B"), ("p1", "C")] {
        let (status, _) = api
            .call(
                "POST",
                &format!("/projects/{project}/tasks"),
                Some(json!({"title": title})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    api.get("/projects/p1/tasks?per_page=1&page=1").await;
    assert_eq!(
        api.get("/projects/p1/tasks?per_page=1&page=2").await,
        json!([task("t3", "p1", "C")])
    );
    assert_eq!(
        api.get("/projects/p2/tasks?per_page=1&page=2").await,
        theirs
    );
    assert_eq!(api.provider.requests().len(), 5);
}

/// A page the model answered longer than its size, saying there is more: the last page
/// says there is none.
#[tokio::test]
async fn a_cut_page_s_more_flag_ends_on_the_last_page() {
    let (t1, t2) = (
        stored_task("t1", "A", "open"),
        stored_task("t2", "B", "open"),
    );
    let answer = json!({
        "code": 0,
        "data": [t1, t2],
        "page_context": {"page": 1, "per_page": 1, "has_more_page": true},
    });
    let api = api_of(&stored_pages(BY_PAGE), true, vec![Ok(answer.to_string())]);
    let first = api.get("/tasks?per_page=1&page=1").await;
    assert_eq!(first, zoho_page(std::slice::from_ref(&t1), 1, true));
    let last = api.get("/tasks?per_page=1&page=2").await;
    assert_eq!(last, zoho_page(&[t2], 2, false));
    assert_eq!(api.provider.requests().len(), 1);
}

/// C2: a boolean in the envelope beside a top-level echo of the page (`success`) is no
/// more flag, even on the last page of a cut.
#[tokio::test]
async fn a_boolean_beside_a_top_level_echo_is_no_more_flag() {
    let schema = r##"
                type: object
                properties:
                  success: {type: boolean}
                  page: {type: integer}
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}"##;
    let (t1, t2) = (
        stored_task("t1", "A", "open"),
        stored_task("t2", "B", "open"),
    );
    pages_of(
        schema,
        json!({"success": true, "page": 1, "data": [t1, t2]}),
        json!({"success": true, "page": 1, "data": [t1]}),
        json!({"success": true, "page": 2, "data": [t2]}),
    )
    .await;
}

/// C3: of a run the store's records made, the model saw one full page, so the flag
/// beside the echo (`meta.isLast`, which the model set `true`) stays as given.
#[tokio::test]
async fn a_stored_run_leaves_the_flag_as_given() {
    let schema = r##"
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  meta:
                    type: object
                    properties: {page: {type: integer}, isLast: {type: boolean}}"##;
    let mut script = created("open");
    let page =
        json!({"data": [stored_task("t1", "A", "open")], "meta": {"page": 1, "isLast": true}});
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(&list_of(PAGE_PARAM, schema)), true, script);
    two_tasks(&api).await;
    assert_eq!(api.get("/tasks?per_page=1&page=1").await, page);
    assert_eq!(
        api.get("/tasks?per_page=1&page=2").await,
        json!({"data": [stored_task("t2", "B", "open")], "meta": {"page": 2, "isLast": true}})
    );
    assert_eq!(api.provider.requests().len(), 3);
}

/// Lists whose next page is a URL in the answer: HAL's `_links.next.href`, a bare
/// `next` beside `values`, JSON:API's `links.next`, each with the cursor in its query,
/// and lists where no URL may be written.
const LINKED: &str = r##"
openapi: 3.1.0
info: {title: Linked pages, version: "1"}
paths:
  /hal:
    get:
      operationId: listHal
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: from, in: query, description: "The item to start at, as the `next` link carries it.", schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [data, _links]
                properties:
                  count: {type: integer}
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  _links:
                    type: object
                    required: [self, previous, next]
                    properties:
                      self: {$ref: "#/components/schemas/Link"}
                      previous: {$ref: "#/components/schemas/NullableLink"}
                      next: {$ref: "#/components/schemas/NullableLink"}
                      documentation: {$ref: "#/components/schemas/Link"}
  /bare:
    get:
      operationId: listBare
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: page, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [values]
                properties:
                  values: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next: {type: string, format: uri, description: "The next page's URL, with its `page`."}
                  previous: {type: string, format: uri}
  /jsonapi:
    get:
      operationId: listJsonApi
      parameters:
        - {name: filter, in: query, schema: {type: string}}
        - {name: "page[size]", in: query, schema: {type: integer}}
        - {name: "page[cursor]", in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [data, links]
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  links:
                    type: object
                    required: [self]
                    properties:
                      self: {type: string, format: uri}
                      prev: {type: [string, "null"], format: uri}
                      next:
                        type: [string, "null"]
                        description: The next page's URL; null on the last page.
  /unconfirmed:
    get:
      operationId: listUnconfirmed
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: from, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next: {type: string, format: uri}
  /ambiguous:
    get:
      operationId: listAmbiguous
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: from, in: query, schema: {type: string}}
        - {name: after, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next: {type: string, format: uri, description: "With `from` or `after`."}
  /laravel:
    get:
      operationId: listLaravel
      parameters:
        - {name: page, in: query, schema: {type: integer}}
        - {name: per_page, in: query, schema: {type: integer}}
        - {name: search, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next_page_url: {type: [string, "null"], format: uri, description: "The next page, with the `search` asked."}
  /numbered:
    get:
      operationId: listNumbered
      parameters:
        - {name: page, in: query, schema: {type: integer, minimum: 1, default: 1}}
        - {name: per_page, in: query, schema: {type: integer}}
        - {name: search, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next_page_url: {type: [string, "null"], format: uri, description: "The next page, with the `search` asked."}
  /since:
    get:
      operationId: listSince
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: since, in: query, schema: {type: string, format: date-time}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next: {type: string, format: uri, description: "The next page, with `since`."}
  /typed:
    get:
      operationId: listTyped
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: type, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  _links:
                    type: object
                    properties:
                      self: {$ref: "#/components/schemas/DescribedLink"}
                      next: {$ref: "#/components/schemas/DescribedLink"}
  /searched:
    get:
      operationId: listSearched
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: q, in: query, description: "Search text, sent again with the next request.", schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next: {type: string, format: uri}
  /filtered:
    get:
      operationId: listFiltered
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: status, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  self: {type: string, format: uri}
                  next: {type: string, format: uri}
  /keyed:
    get:
      operationId: listKeyed
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: from, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next: {type: string, format: uri, description: "The next page, as in ?prefrom=x."}
  /notfirst:
    get:
      operationId: listNotFirst
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: from, in: query, description: "As `page_next` carries it.", schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  page_next: {type: string, format: uri}
  /paged:
    get:
      operationId: listPaged
      parameters:
        - {name: page, in: query, schema: {type: string}}
        - {name: "page[size]", in: query, schema: {type: integer}}
        - {name: "page[cursor]", in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  links:
                    type: object
                    properties:
                      self: {type: string, format: uri}
                      next: {type: [string, "null"], format: uri}
  /things/{id}/items:
    get:
      operationId: listThingItems
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
        - {name: limit, in: query, schema: {type: integer}}
        - {name: from, in: query, description: "As the `next` URL carries it.", schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next: {type: string, format: uri}
components:
  schemas:
    User:
      type: object
      required: [id]
      properties: {id: {type: string}, name: {type: string}}
    Link:
      type: object
      required: [href, type]
      properties: {href: {type: string, format: uri}, type: {type: string}}
    NullableLink:
      type: [object, "null"]
      required: [href, type]
      properties: {href: {type: string, format: uri}, type: {type: string}}
    DescribedLink:
      type: [object, "null"]
      description: "A link, with its `href` and media `type`."
      properties:
        href: {type: string, format: uri, description: "The URL, with its `type`."}
        type: {type: string}
"##;

/// The path and query of absolute URL `url`, as a client following it asks for them.
fn followed(url: &str) -> &str {
    let host = url.find("://").map_or(0, |at| at + 3);
    &url[host + url[host..].find('/').unwrap_or(0)..]
}

/// [`Api::get`] with a `Host` header.
async fn get_at(api: &Api, host: &str, uri: &str) -> Value {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("host", host)
        .body(Body::empty())
        .unwrap();
    let resp = api.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A HAL page cut short gets a `_links.next` (a copy of `_links.self`, as the model's
/// is `null`) whose URL is the mock's own, as the request reached it, never the host
/// the model wrote, carrying the cursor under `from` (which the contract ties to the
/// `next` link) and the page size; following it serves the rest with no model call,
/// and the last page's `next` is `null`.
#[tokio::test]
async fn a_hal_next_link_carries_the_cursor() {
    let answer = json!({
        "count": 3,
        "data": users(3),
        "_links": {
            "documentation": {"href": "https://docs.example.org/pages", "type": "text/html"},
            "next": null,
            "previous": null,
            "self": {"href": "https://api.example.org/hal?limit=2", "type": "application/hal+json"},
        },
    });
    let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
    let first = get_at(&api, "mock.test:9000", "/hal?limit=2").await;
    assert_eq!(ids_in(&first, "data"), ["u1", "u2"], "{first}");
    let next = &first["_links"]["next"];
    assert_eq!(next["type"], "application/hal+json", "{first}");
    let href = next["href"].as_str().unwrap_or_default();
    assert!(href.starts_with("http://mock.test:9000/hal?"), "{href}");
    assert!(href.contains("limit=2") && href.contains("from="), "{href}");
    let last = api.get(followed(href)).await;
    assert_eq!(ids_in(&last, "data"), ["u3"], "{last}");
    assert_eq!(last["_links"]["next"], Value::Null, "{last}");
    assert_eq!(api.provider.requests().len(), 1);
}

/// A HAL list of tasks, its `from` tied to the `next` link.
const HAL_TASKS: &str = r##"    get:
      operationId: listTasks
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: from, in: query, description: "As the `next` link carries it.", schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [data, _links]
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  _links:
                    type: object
                    properties:
                      self: {type: object, required: [href], properties: {href: {type: string, format: uri}}}
                      next: {type: [object, "null"], required: [href], properties: {href: {type: string, format: uri}}}"##;

/// The Mollie case: a full page of one stored task, its `_links.next` `null`, while the
/// session holds another: the link leads to it, served from the store.
#[tokio::test]
async fn a_hal_next_link_leads_to_the_stored_rest() {
    let mut script = created("open");
    let page = json!({
        "data": [stored_task("t1", "A", "open")],
        "_links": {"self": {"href": "https://api.example.org/tasks?limit=1"}, "next": null},
    });
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(HAL_TASKS), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1").await;
    let href = first["_links"]["next"]["href"].as_str().unwrap_or_default();
    assert!(href.contains("from="), "{first}");
    let second = api.get(followed(href)).await;
    assert_eq!(
        second["data"],
        json!([stored_task("t2", "B", "open")]),
        "{second}"
    );
    assert_eq!(second["_links"]["next"], Value::Null, "{second}");
    assert_eq!(api.provider.requests().len(), 3);
}

/// A next link the model gave, carrying the cursor, is its own next page's when no
/// stored record is left for one: the page stays as answered, and following it is the
/// model's.
#[tokio::test]
async fn the_model_s_own_next_link_stays() {
    let mut script = vec![Ok(stored_task("t1", "A", "open").to_string())];
    let next = "https://api.example.org/tasks?limit=1&from=t2";
    let page = json!({
        "data": [stored_task("t1", "A", "open")],
        "_links": {"self": {"href": "https://api.example.org/tasks?limit=1"}, "next": {"href": next}},
    });
    script.push(Ok(page.to_string()));
    script.push(Ok(json!({"data": [stored_task("t2", "B", "open")], "_links": {"self": {"href": next}, "next": null}}).to_string()));
    let api = api_of(&stored_pages(HAL_TASKS), true, script);
    let (status, body) = api
        .call("POST", "/tasks", Some(json!({"title": "A"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(api.get("/tasks?limit=1").await, page);
    api.get(followed(next)).await;
    assert_eq!(api.provider.requests().len(), 3);
}

/// A bare `next` URL beside `values`, the model leaving it out: the mock writes one
/// carrying the cursor under `page`, which its description names; the last page leaves
/// it out.
#[tokio::test]
async fn a_bare_next_url_carries_the_cursor() {
    let answer = json!({"values": users(3)});
    let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
    let first = api.get("/bare?limit=2").await;
    assert_eq!(ids_in(&first, "values"), ["u1", "u2"], "{first}");
    let next = first["next"].as_str().unwrap_or_default();
    assert!(
        next.starts_with("http://localhost/bare?") && next.contains("page="),
        "{first}"
    );
    let last = api.get(followed(next)).await;
    assert_eq!(ids_in(&last, "values"), ["u3"], "{last}");
    assert!(last.get("next").is_none(), "{last}");
    assert_eq!(api.provider.requests().len(), 1);
}

/// JSON:API's `links.next`, a URL by its description. The model's `self` carries both
/// `filter` and `page[cursor]`, so both are confirmed: `page[cursor]` is the cursor, as
/// it shares its first word with the page size `page[size]`.
#[tokio::test]
async fn a_json_api_next_link_carries_the_cursor() {
    let answer = json!({
        "data": users(3),
        "links": {
            "self": "https://a.example.org/jsonapi?filter=&page%5Bcursor%5D=&page%5Bsize%5D=2",
            "prev": null,
            "next": null,
        },
    });
    let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
    let first = api.get("/jsonapi?page%5Bsize%5D=2").await;
    assert_eq!(ids_in(&first, "data"), ["u1", "u2"], "{first}");
    let next = first["links"]["next"].as_str().unwrap_or_default();
    assert!(
        next.starts_with("http://localhost/jsonapi?") && next.contains("page%5Bcursor%5D="),
        "{first}"
    );
    let last = api.get(followed(next)).await;
    assert_eq!(ids_in(&last, "data"), ["u3"], "{last}");
    assert_eq!(last["links"]["next"], Value::Null, "{last}");
    assert_eq!(api.provider.requests().len(), 1);
}

/// A path value holding `/` stays one segment in the next URL.
#[tokio::test]
async fn a_next_url_keeps_a_path_value_encoded() {
    let answer = json!({"data": users(3)});
    let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
    let first = api.get("/things/a%2Fb/items?limit=2").await;
    let next = first["next"].as_str().unwrap_or_default();
    assert!(
        next.starts_with("http://localhost/things/a%2Fb/items?"),
        "{first}"
    );
    assert!(next.contains("from="), "{first}");
}

/// No next URL is written, and the page is only cut: for a lone text parameter nothing
/// ties to the URL (`unconfirmed`), two that are both tied with no page size to choose
/// between them (`ambiguous`), or one a `format` shapes (`since`, a date-time).
#[tokio::test]
async fn no_next_url_without_one_confirmed_cursor() {
    for route in ["unconfirmed", "ambiguous", "since"] {
        let answer = json!({"data": users(3)});
        let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
        let first = api.get(&format!("/{route}?limit=2")).await;
        assert_eq!(first, json!({"data": users(2)}), "{route}");
    }
}

/// Ordinary filters are not taken for the cursor: `type`, which only a link schema the
/// `next` member shares names (not the member's own description); `q`, whose
/// description says "next" in prose; and `status`, which the model's `self` carries
/// with a value the request did not send. Nor is a member whose name has `next` only as
/// a later word (`page_next`) a next link, though `from`'s description names it.
#[tokio::test]
async fn ordinary_filters_are_no_cursor() {
    let cases = [
        (
            "typed",
            json!({"data": users(3), "_links": {"self": {"href": "https://api.example.org/typed?limit=2"}, "next": null}}),
        ),
        ("searched", json!({"data": users(3)})),
        (
            "filtered",
            json!({"data": users(3), "self": "https://api.example.org/filtered?limit=2&status=open"}),
        ),
        ("notfirst", json!({"data": users(3)})),
        // A next link is another page's: what it carries empty ties nothing.
        (
            "filtered",
            json!({"data": users(3), "next": "https://api.example.org/filtered?limit=2&status="}),
        ),
        // A link that does not show the request's values (`limit=2`) is not its own.
        (
            "filtered",
            json!({"data": users(3), "self": "https://api.example.org/filtered?status="}),
        ),
        // `from=` inside a longer name (`prefrom=`) is no query key.
        ("keyed", json!({"data": users(3)})),
    ];
    let mut taken = Vec::new();
    for (route, answer) in cases {
        let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
        let first = api.get(&format!("/{route}?limit=2")).await;
        let mut expected = answer.clone();
        expected["data"] = json!(users(2));
        if first != expected {
            taken.push(format!("{route}: {first}"));
        }
    }
    assert!(taken.is_empty(), "{taken:#?}");
}

/// Of the parameters a link of the request carries empty, a bare `page` shares the page
/// size's first word but is one word: `page[cursor]` is the cursor.
#[tokio::test]
async fn a_one_word_parameter_is_not_the_page_size_s() {
    let answer = json!({
        "data": users(3),
        "links": {"self": "https://a.example.org/paged?page=&page%5Bcursor%5D=&page%5Bsize%5D=2", "next": null},
    });
    let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
    let first = api.get("/paged?page%5Bsize%5D=2").await;
    let next = first["links"]["next"].as_str().unwrap_or_default();
    assert!(next.contains("page%5Bcursor%5D="), "{first}");
}

/// With no link of the request carrying it, `page[cursor]` is the cursor as the one
/// parameter of several words sharing the page size's first word.
#[tokio::test]
async fn the_page_size_s_first_word_picks_the_cursor() {
    let answer = json!({
        "data": users(3),
        "links": {"self": "https://a.example.org/paged?page%5Bsize%5D=2", "next": null},
    });
    let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
    let first = api.get("/paged?page%5Bsize%5D=2").await;
    let next = first["links"]["next"].as_str().unwrap_or_default();
    assert!(next.contains("page%5Bcursor%5D="), "{first}");
}

/// A next URL the model wrote to another page of the route (`page=2`) stays, though a
/// text parameter is tied to it; and a list paged by number takes no URL of the cursor:
/// its pages are a run's.
#[tokio::test]
async fn a_numbered_next_url_is_not_overwritten() {
    let next = "https://api.example.org/laravel?page=2&per_page=2";
    let answer = json!({"data": users(3), "next_page_url": next});
    let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
    let first = api.get("/laravel?per_page=2").await;
    assert_eq!(first["next_page_url"], next, "{first}");
    let answer = json!({"data": users(3), "next_page_url": null});
    let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
    let first = api.get("/numbered?per_page=2").await;
    assert_eq!(ids_in(&first, "data"), ["u1", "u2"], "{first}");
    assert_eq!(first["next_page_url"], Value::Null, "{first}");
}

/// A list whose cursor is named as a credential, and lists whose parameters are
/// credentials that members of the answer are named as.
const CURSORS: &str = r##"
openapi: 3.1.0
info: {title: Cursors, version: "1"}
paths:
  /meetings:
    get:
      operationId: listMeetings
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
                properties:
                  meetings: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next_page_token: {type: string}
  /sessions:
    get:
      operationId: listSessions
      parameters:
        - {name: access_token, in: query, schema: {type: string}}
        - {name: refresh_token, in: query, schema: {type: string}}
        - {name: token, in: query, schema: {type: string}}
        - {name: client_secret, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  sessions: {type: array, items: {$ref: "#/components/schemas/User"}}
                  token: {type: string}
                  auth:
                    type: object
                    properties:
                      access_token: {type: string}
                      new_refresh_token: {type: string}
                      client_secret: {type: string}
  /webinars:
    get:
      operationId: listWebinars
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
                properties:
                  webinars: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next_page_token: {type: string}
  /scoped:
    get:
      operationId: listScoped
      parameters:
        - {name: page_token, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  items: {type: array, items: {$ref: "#/components/schemas/User"}}
                  next_page_token: {type: string}
components:
  securitySchemes:
    key: {type: apiKey, in: query, name: page_token}
  schemas:
    User:
      type: object
      required: [id]
      properties: {id: {type: string}, name: {type: string}}
"##;

/// A `next_page_token` the session's answer served reaches the next request's prompt;
/// one the client made up does not, nor one another operation served.
#[tokio::test]
async fn a_served_page_token_reaches_the_prompt() {
    let script = vec![
        Ok(json!({"meetings": users(1), "next_page_token": "tok-served-123"}).to_string()),
        Ok(json!({"meetings": users(1), "next_page_token": ""}).to_string()),
        Ok(json!({"meetings": users(1), "next_page_token": ""}).to_string()),
    ];
    let api = api_of(CURSORS, true, script);
    api.get("/meetings?page_size=1").await;
    api.get("/meetings?page_size=1&next_page_token=tok-served-123")
        .await;
    assert!(
        api.prompt(1).contains("tok-served-123"),
        "{}",
        api.prompt(1)
    );
    api.get("/meetings?page_size=1&next_page_token=client-made-456")
        .await;
    assert!(
        !api.prompt(2).contains("client-made-456"),
        "{}",
        api.prompt(2)
    );
    api.get("/webinars?page_size=1&next_page_token=tok-served-123")
        .await;
    assert!(
        !api.prompt(3).contains("tok-served-123"),
        "{}",
        api.prompt(3)
    );
}

/// Credentials a client sends stay out of the prompt though members of the answer are
/// named as them (`access_token`, `refresh_token`, a bare `token`, `client_secret`):
/// their values are the client's, never one an answer served. A parameter a security
/// scheme carries in the query stays out even carrying the value an answer served at
/// the member it pairs.
#[tokio::test]
async fn credentials_named_as_answer_members_stay_hidden() {
    let served = json!({
        "sessions": users(1),
        "token": "srv-token-1",
        "auth": {"access_token": "srv-access-1", "new_refresh_token": "srv-refresh-1", "client_secret": "srv-secret-1"},
    });
    let script = vec![
        Ok(served.to_string()),
        Ok(served.to_string()),
        Ok(json!({"items": users(1), "next_page_token": "srv-scoped-1"}).to_string()),
        Ok(json!({"items": users(1)}).to_string()),
    ];
    let api = api_of(CURSORS, true, script);
    api.get("/sessions").await;
    api.get("/sessions?access_token=cli-access-1&refresh_token=cli-refresh-1&token=cli-token-1&client_secret=cli-secret-1")
        .await;
    let prompt = api.prompt(1);
    for secret in [
        "cli-access-1",
        "cli-refresh-1",
        "cli-token-1",
        "cli-secret-1",
    ] {
        assert!(!prompt.contains(secret), "{secret}: {prompt}");
    }
    api.get("/scoped?page_token=cli-key-1").await;
    api.get("/scoped?page_token=srv-scoped-1").await;
    assert!(!api.prompt(3).contains("srv-scoped-1"), "{}", api.prompt(3));
}

/// A page cut short keeps the next link the model gave to its own next page, carrying
/// the cursor: no cut serves it.
#[tokio::test]
async fn a_cut_page_keeps_the_model_s_next_link() {
    let next = "https://api.example.org/hal?limit=2&from=u3";
    let answer = json!({
        "data": users(3),
        "_links": {
            "next": {"href": next, "type": "application/hal+json"},
            "previous": null,
            "self": {"href": "https://api.example.org/hal?limit=2", "type": "application/hal+json"},
        },
    });
    let api = api_of(LINKED, true, vec![Ok(answer.to_string())]);
    let first = api.get("/hal?limit=2").await;
    assert_eq!(ids_in(&first, "data"), ["u1", "u2"], "{first}");
    assert_eq!(first["_links"]["next"]["href"], next, "{first}");
}

/// Tasks of an organization named in the query of every call, a create's too, listed
/// with a free-text search, a choice with a `default`, and an expression the mock cannot
/// read.
const SCOPED_PAGES: &str = r##"
openapi: 3.1.0
info: {title: Scoped pages, version: "1"}
paths:
  /tasks:
    parameters:
      - {name: org, in: query, required: true, schema: {type: string}}
    get:
      operationId: listTasks
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: cursor, in: query, schema: {type: string}}
        - {name: q, in: query, description: Tasks whose text holds it., schema: {type: string}}
        - {name: view, in: query, schema: {type: string, enum: [all, mine], default: all}}
        - {name: expr, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [data]
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  next_cursor: {type: [string, "null"]}
    post:
      operationId: createTask
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
components:
  schemas:
    Task:
      type: object
      required: [id, title, status]
      properties:
        id: {type: string}
        title: {type: string}
        status: {type: string, enum: [open, done]}
"##;

/// Creates tasks `(id, title, org)` in order, each under its `org`, through the API.
async fn scoped_tasks(api: &Api, tasks: &[(&str, &str, &str)]) {
    for (_, title, org) in tasks {
        let (status, body) = api
            .call(
                "POST",
                &format!("/tasks?org={org}"),
                Some(json!({"title": title})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
}

/// The answers creating `tasks`, each open.
fn scoped_answers(tasks: &[(&str, &str, &str)]) -> Vec<Result<String, LlmError>> {
    tasks
        .iter()
        .map(|(id, title, _)| Ok(stored_task(id, title, "open").to_string()))
        .collect()
}

/// The next page a minted token on `first` leads to, under `query`.
async fn next_of(api: &Api, first: &Value, query: &str) -> Value {
    let cursor = first["next_cursor"].as_str().unwrap_or_default();
    assert!(!cursor.is_empty(), "{first}");
    api.get(&format!("/tasks?{query}&cursor={cursor}")).await
}

/// A scope value no record holds (`org=acme`), which the create of each record carried
/// as well, excludes none of them: the next page is the other task the organization's
/// create made, and not the one another organization's made, though its title holds
/// `acme`.
#[tokio::test]
async fn a_value_the_create_carried_excludes_no_stored_record() {
    let tasks = [
        ("t1", "A", "acme"),
        ("t2", "B", "acme"),
        ("t3", "acme rival", "other"),
    ];
    let mut script = scoped_answers(&tasks);
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    let api = api_of(SCOPED_PAGES, true, script);
    scoped_tasks(&api, &tasks).await;
    let first = api.get("/tasks?org=acme&limit=1").await;
    let second = next_of(&api, &first, "org=acme&limit=1").await;
    assert_eq!(
        second["data"],
        json!([stored_task("t2", "B", "open")]),
        "{second}"
    );
    assert_eq!(second["next_cursor"], Value::Null, "{second}");
    assert_eq!(api.provider.requests().len(), 4);
}

/// A free-text search (`q=Walk`) keeps a stored record whose text holds it, ignoring
/// case, and leaves one whose text does not.
#[tokio::test]
async fn a_free_text_value_matches_a_stored_record_by_part() {
    let tasks = [
        ("t1", "Walk dog", "acme"),
        ("t2", "Feed fish", "acme"),
        ("t3", "walk cat", "acme"),
    ];
    let mut script = scoped_answers(&tasks);
    let page = json!({"data": [stored_task("t1", "Walk dog", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    let api = api_of(SCOPED_PAGES, true, script);
    scoped_tasks(&api, &tasks).await;
    let first = api.get("/tasks?org=acme&limit=1&q=Walk").await;
    let second = next_of(&api, &first, "org=acme&limit=1&q=Walk").await;
    assert_eq!(
        second["data"],
        json!([stored_task("t3", "walk cat", "open")]),
        "{second}"
    );
    assert_eq!(second["next_cursor"], Value::Null, "{second}");
}

/// A choice at its parameter's `default` (`view=all`) asks what leaving it out asks; a
/// choice of another value (`view=mine`) no record shows, which pages nothing.
#[tokio::test]
async fn a_choice_at_its_default_excludes_no_stored_record() {
    let tasks = [("t1", "A", "acme"), ("t2", "B", "acme")];
    let mut script = scoped_answers(&tasks);
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    script.push(Ok(page.to_string()));
    let api = api_of(SCOPED_PAGES, true, script);
    scoped_tasks(&api, &tasks).await;
    assert_eq!(api.get("/tasks?org=acme&limit=1&view=mine").await, page);
    let first = api.get("/tasks?org=acme&limit=1&view=all").await;
    let second = next_of(&api, &first, "org=acme&limit=1&view=all").await;
    assert_eq!(ids_in(&second, "data"), ["t2"], "{second}");
}

/// An expression the mock cannot read (`expr=any(title,["B"])`) is held by no record
/// whole: no stored record is served for it, and the page stays as the model answered.
#[tokio::test]
async fn an_expression_the_mock_cannot_read_pages_nothing() {
    let tasks = [("t1", "A", "acme"), ("t2", "B", "acme")];
    let mut script = scoped_answers(&tasks);
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    let api = api_of(SCOPED_PAGES, true, script);
    scoped_tasks(&api, &tasks).await;
    let body = api
        .get("/tasks?org=acme&limit=1&expr=any(title,%5B%22B%22%5D)")
        .await;
    assert_eq!(body, page);
}

/// A token the model gave, while the session holds a matching record the page does not
/// show, gives way to a minted one: the next page is the stored record, and it carries
/// the model's token on to the model's own next page.
#[tokio::test]
async fn a_stored_rest_comes_before_the_model_s_token() {
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": "abc", "total": 50});
    script.push(Ok(page.to_string()));
    let third =
        json!({"data": [stored_task("x3", "X", "open")], "next_cursor": "def", "total": 50});
    script.push(Ok(third.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1").await;
    assert_ne!(first["next_cursor"], "abc", "{first}");
    assert_eq!(first["total"], 50, "{first}");
    let cursor = first["next_cursor"].as_str().unwrap_or_default();
    let second = api.get(&format!("/tasks?limit=1&cursor={cursor}")).await;
    assert_eq!(ids_in(&second, "data"), ["t2"], "{second}");
    assert_eq!(second["next_cursor"], "abc", "{second}");
    assert_eq!(api.provider.requests().len(), 3);
    // The model's token is the model's: its next page is generated.
    assert_eq!(api.get("/tasks?limit=1&cursor=abc").await, third);
    assert_eq!(api.provider.requests().len(), 4);
}

/// A next link the model gave, carrying the cursor, gives way to the mock's own while
/// the session holds a matching record the page does not show.
#[tokio::test]
async fn a_stored_rest_replaces_the_model_s_next_link() {
    let mut script = created("open");
    let next = "https://api.example.org/tasks?limit=1&from=zz";
    let page = json!({
        "data": [stored_task("t1", "A", "open")],
        "_links": {"self": {"href": "https://api.example.org/tasks?limit=1"}, "next": {"href": next}},
    });
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(HAL_TASKS), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1").await;
    let href = first["_links"]["next"]["href"].as_str().unwrap_or_default();
    assert_ne!(href, next, "{first}");
    let second = api.get(followed(href)).await;
    assert_eq!(ids_in(&second, "data"), ["t2"], "{second}");
    // The last stored page leads on to the model's own next page.
    assert_eq!(second["_links"]["next"]["href"], next, "{second}");
    assert_eq!(api.provider.requests().len(), 3);
}

/// A record the create made under `org=acme`, whose title holds `other`, is no record
/// of `org=other`: the scope its create carried decides, not its text.
#[tokio::test]
async fn review_a_record_of_another_scope_does_not_leak_by_containment() {
    let tasks = [
        ("t1", "Another", "acme"),
        ("t2", "X", "other"),
        ("t3", "Y", "other"),
    ];
    let mut script = scoped_answers(&tasks);
    let page = json!({"data": [stored_task("t2", "X", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    let api = api_of(SCOPED_PAGES, true, script);
    scoped_tasks(&api, &tasks).await;
    let first = api.get("/tasks?org=other&limit=1").await;
    let second = next_of(&api, &first, "org=other&limit=1").await;
    assert_eq!(ids_in(&second, "data"), ["t3"], "{second}");
}

/// After the stored rest is served, the model's own next page (total 50) still follows.
#[tokio::test]
async fn review_the_model_s_continuation_survives_the_stored_rest() {
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": "abc", "total": 50});
    script.push(Ok(page.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1").await;
    let cursor = first["next_cursor"].as_str().unwrap_or_default();
    let second = api.get(&format!("/tasks?limit=1&cursor={cursor}")).await;
    assert_eq!(ids_in(&second, "data"), ["t2"], "{second}");
    assert!(
        second["next_cursor"].is_string(),
        "model's continuation lost: {second}"
    );
}

const DEFAULT_CHOICE: &str = r##"
openapi: 3.1.0
info: {title: Default choice, version: "1"}
paths:
  /tasks:
    get:
      operationId: listTasks
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: cursor, in: query, schema: {type: string}}
        - {name: show, in: query, schema: {type: string, enum: [open, done], default: open}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [data]
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  next_cursor: {type: [string, "null"]}
    post:
      operationId: createTask
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
components:
  schemas:
    Task:
      type: object
      required: [id, title, status]
      properties:
        id: {type: string}
        title: {type: string}
        status: {type: string, enum: [open, done]}
"##;

/// `show=open` is the default, but it is a choice records hold: a done task does not
/// follow on the next page.
#[tokio::test]
async fn review_a_default_that_records_hold_still_excludes() {
    let mut script = created("done");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    let api = api_of(DEFAULT_CHOICE, true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1&show=open").await;
    assert!(
        first["next_cursor"].is_null(),
        "done task paged under show=open: {first}"
    );
}

/// A free-text value is looked for in the items' declared plain text only: an email
/// the items do not declare (`b@example.org`) does not match `q=example`.
#[tokio::test]
async fn review_free_text_matches_only_declared_text() {
    let tasks = [("t1", "A", "acme"), ("t2", "B", "acme")];
    let mut script = vec![
        Ok(json!({"id": "t1", "title": "A", "status": "open"}).to_string()),
        Ok(
            json!({"id": "t2", "title": "B", "status": "open", "owner_email": "b@example.org"})
                .to_string(),
        ),
    ];
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    let api = api_of(SCOPED_PAGES, true, script);
    scoped_tasks(&api, &tasks).await;
    let first = api.get("/tasks?org=acme&limit=1&q=example").await;
    assert!(
        first["next_cursor"].is_null(),
        "undeclared email matched q: {first}"
    );
}

/// The model's page reached by its own token, carried on the last stored page, does
/// not serve again a record the stored pages served.
#[tokio::test]
async fn review2_model_page_after_stored_rest_does_not_repeat_it() {
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": "abc", "total": 3});
    script.push(Ok(page.to_string()));
    let model2 =
        json!({"data": [stored_task("t2", "B", "open")], "next_cursor": "def", "total": 3});
    script.push(Ok(model2.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1").await;
    let cursor = first["next_cursor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let second = api.get(&format!("/tasks?limit=1&cursor={cursor}")).await;
    assert_eq!(ids_in(&second, "data"), ["t2"], "{second}");
    let then = second["next_cursor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let third = api.get(&format!("/tasks?limit=1&cursor={then}")).await;
    let mut all: Vec<String> = Vec::new();
    for p in [&first, &second, &third] {
        all.extend(ids_in(p, "data").into_iter().map(|s| s.to_string()));
    }
    let mut dedup = all.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(all.len(), dedup.len(), "duplicate across pages: {all:?}");
}

const PATCH_BLOCK: &str = r##"  /tasks/{id}:
    parameters:
      - {name: id, in: path, required: true, schema: {type: string}}
    patch:
      operationId: updateTask
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
components:
"##;

/// An update of an acme task keeps its scope: its title (`Another`) does not let it
/// into `org=other`'s next page by containment.
#[tokio::test]
async fn review2_an_update_does_not_reopen_the_scope_leak() {
    let tasks = [
        ("t1", "Another", "acme"),
        ("t2", "X", "other"),
        ("t3", "Y", "other"),
    ];
    let mut script = scoped_answers(&tasks);
    script.push(Ok(
        json!({"id": "t1", "title": "Another", "status": "done"}).to_string(),
    ));
    let page = json!({"data": [stored_task("t2", "X", "open")], "next_cursor": null});
    script.push(Ok(page.to_string()));
    let contract = SCOPED_PAGES.replacen("components:\n", PATCH_BLOCK, 1);
    let api = api_of(&contract, true, script);
    scoped_tasks(&api, &tasks).await;
    let (status, body) = api
        .call("PATCH", "/tasks/t1", Some(json!({"status": "done"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let first = api.get("/tasks?org=other&limit=1").await;
    let second = next_of(&api, &first, "org=other&limit=1").await;
    assert_eq!(ids_in(&second, "data"), ["t3"], "{second}");
    assert_eq!(second["next_cursor"], Value::Null, "{second}");
}

/// The model's page for its own token, carried on after a stored rest, that holds only
/// what the rest served ends the list: no token, rather than an empty page with one.
#[tokio::test]
async fn a_model_page_holding_only_served_records_ends_the_list() {
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": "abc"});
    script.push(Ok(page.to_string()));
    let model2 = json!({"data": [stored_task("t2", "B", "open")], "next_cursor": "def"});
    script.push(Ok(model2.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1").await;
    let cursor = first["next_cursor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let second = api.get(&format!("/tasks?limit=1&cursor={cursor}")).await;
    assert_eq!(second["next_cursor"], "abc", "{second}");
    let third = api.get("/tasks?limit=1&cursor=abc").await;
    assert_eq!(third["data"], json!([]), "{third}");
    assert!(
        third.get("next_cursor").is_none_or(Value::is_null),
        "{third}"
    );
}

/// A token the model gives again (`abc`) on a later first page of the route that keeps
/// it ends the earlier walk's note: the model's page for it drops nothing.
#[tokio::test]
async fn a_reused_model_token_finds_no_earlier_walk() {
    let mut script = created("open");
    let page = json!({"data": [stored_task("t1", "A", "open")], "next_cursor": "abc"});
    script.push(Ok(page.to_string()));
    // A full page showing both tasks: no stored rest, so the model's `abc` stays.
    let later = json!({
        "data": [stored_task("t1", "A", "open"), stored_task("t2", "B", "open")],
        "next_cursor": "abc",
    });
    script.push(Ok(later.to_string()));
    let model2 = json!({"data": [stored_task("t2", "B", "open")], "next_cursor": null});
    script.push(Ok(model2.to_string()));
    let api = api_of(&stored_pages(BY_CURSOR), true, script);
    two_tasks(&api).await;
    let first = api.get("/tasks?limit=1").await;
    assert_ne!(first["next_cursor"], "abc", "{first}");
    assert_eq!(api.get("/tasks?limit=2").await, later);
    let third = api.get("/tasks?limit=1&cursor=abc").await;
    assert_eq!(ids_in(&third, "data"), ["t2"], "{third}");
}
