//! Write memory: what a 2xx write's answer did not say reaches the next
//! generation for its resource, once. Driven over HTTP with a scripted LLM,
//! asserting the prompt it was sent and how many calls it took.
#![cfg(test)]
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{LlmError, Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, RequestRecord, RequestRecorder, http_rest};
use mb_state::{
    CachingStateStore, ChangeKind, MemoryStore, SessionKey, StateStore, StoredResource,
};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Where the memory lives: a resource type no contract names.
const MEMORY: &str = "__mockingbird.earlier_requests";

/// A generic API: a create answering only its id, a full create, a 204 action,
/// an update, a delete, creates under a parent, and a route with an owner.
const THINGS: &str = r##"
openapi: 3.0.0
info: {title: Things, version: "1"}
paths:
  /items:
    get:
      operationId: listItems
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}
    post:
      operationId: createItem
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Created"}}}}
  /items/{item_id}:
    parameters: [{name: item_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getItem
      parameters: [{name: view, in: query, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
    patch:
      operationId: patchItem
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
    delete:
      operationId: deleteItem
      responses:
        "204": {description: gone}
  /items/{item_id}/close:
    parameters: [{name: item_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: closeItem
      requestBody:
        required: false
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "204": {description: closed}
  /notes:
    post:
      operationId: createNote
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
  /notes/{note_id}:
    get:
      operationId: getNote
      parameters: [{name: note_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
  /posts:
    get:
      operationId: listPosts
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Post"}}}}}
  /posts/{post_id}:
    get:
      operationId: getPost
      parameters: [{name: post_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Post"}}}}
  /posts/{post_id}/comments:
    post:
      operationId: createComment
      parameters: [{name: post_id, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Comment"}}}}
  /comments/{comment_id}:
    get:
      operationId: getComment
      parameters: [{name: comment_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Comment"}}}}
  /invoices/{invoice_id}:
    get:
      operationId: getInvoice
      parameters: [{name: invoice_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Invoice"}}}}
  /invoices/{invoice_id}/pay:
    post:
      operationId: payInvoice
      parameters: [{name: invoice_id, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: false
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "204": {description: paid}
  /pets:
    post:
      operationId: createPet
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Pet"}}}}
  /pets/{pet_id}:
    parameters: [{name: pet_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getPet
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Pet"}}}}
    put:
      operationId: putPet
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "204": {description: replaced}
  /pets/{pet_id}/birthday:
    parameters: [{name: pet_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: petBirthday
      requestBody:
        required: false
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "204": {description: done}
  /widgets/{widget_id}:
    parameters: [{name: widget_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getWidget
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
    put:
      operationId: putWidget
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/WidgetAck"}}}}
  /widgets/{widget_id}/poke:
    parameters: [{name: widget_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: pokeWidget
      responses:
        "204": {description: poked}
  /tokens/{token_value}/revoke:
    parameters: [{name: token_value, in: path, required: true, schema: {type: string}}]
    post:
      operationId: revokeToken
      requestBody:
        required: false
        content:
          application/json:
            schema:
              type: object
              additionalProperties: true
              properties:
                passphrase: {type: string, format: password}
                recovery: {type: string, writeOnly: true}
      responses:
        "204": {description: revoked}
  /captures/{id}/refunds/{refund_id}:
    parameters:
      - {name: id, in: path, required: true, schema: {type: string}}
      - {name: refund_id, in: path, required: true, schema: {type: string}}
    put:
      operationId: putRefund
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
    delete:
      operationId: deleteRefund
      responses:
        "204": {description: gone}
  /captures/{id}/refunds/{refund_id}/approve:
    parameters:
      - {name: id, in: path, required: true, schema: {type: string}}
      - {name: refund_id, in: path, required: true, schema: {type: string}}
    post:
      operationId: approveRefund
      responses:
        "204": {description: approved}
  /receipts:
    post:
      operationId: createReceipt
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/ReceiptSummary"}}}}
  /receipts/{receipt_id}:
    get:
      operationId: getReceipt
      parameters: [{name: receipt_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Receipt"}}}}
  /captures:
    post:
      operationId: createCapture
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Capture"}}}}
  /captures/{id}:
    get:
      operationId: getCapture
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Capture"}}}}
  /captures/{id}/refunds:
    post:
      operationId: refundCapture
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: false
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /refunds/{refund_id}:
    get:
      operationId: getRefund
      parameters: [{name: refund_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /orgs/{org}:
    get:
      operationId: getOrg
      parameters: [{name: org, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Org"}}}}
  /orgs/{org}/teams:
    post:
      operationId: createTeam
      parameters: [{name: org, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Team"}}}}
  /orgs/{org}/teams/{slug}/archive:
    parameters:
      - {name: org, in: path, required: true, schema: {type: string}}
      - {name: slug, in: path, required: true, schema: {type: string}}
    post:
      operationId: archiveTeam
      requestBody:
        required: false
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "204": {description: archived}
  /orgs/{org}/teams/{slug}/members:
    parameters:
      - {name: org, in: path, required: true, schema: {type: string}}
      - {name: slug, in: path, required: true, schema: {type: string}}
    post:
      operationId: addMember
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Member"}}}}
  /members/{member_id}:
    get:
      operationId: getMember
      parameters: [{name: member_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Member"}}}}
  /orgs/{org}/teams/{slug}:
    parameters:
      - {name: org, in: path, required: true, schema: {type: string}}
      - {name: slug, in: path, required: true, schema: {type: string}}
    get:
      operationId: getTeam
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Team"}}}}
    put:
      operationId: replaceTeam
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Team"}}}}
components:
  schemas:
    Created:
      type: object
      additionalProperties: false
      required: [id]
      properties: {id: {type: string}}
    Item:
      type: object
      required: [id, title, status]
      properties:
        id: {type: string}
        title: {type: string}
        status: {type: string}
    Note:
      type: object
      required: [id, text]
      properties:
        id: {type: string}
        text: {type: string}
        pinned: {type: boolean}
    Post:
      type: object
      required: [id, body, status]
      properties:
        id: {type: string}
        body: {type: string}
        status: {type: string}
    Comment:
      type: object
      required: [id, body]
      properties:
        id: {type: string}
        body: {type: string}
    Invoice:
      type: object
      required: [id, amount, status]
      properties:
        id: {type: string}
        amount: {type: integer}
        status: {type: string}
    Pet:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        version: {type: integer, readOnly: true}
        headers:
          type: array
          items:
            type: object
            properties: {name: {type: string}, value: {type: string}}
        metadata:
          type: object
          additionalProperties: {type: string}
        form:
          type: object
          properties: {kind: {type: string}}
          oneOf:
            - {required: [kind], properties: {kind: {enum: [dog]}, bark: {type: boolean}}}
            - {required: [kind], properties: {kind: {enum: [cat]}, meow: {type: boolean}}}
        shape:
          oneOf:
            - {type: object, required: [kind], properties: {kind: {type: string, enum: [dog]}, bark: {type: boolean}}}
            - {type: object, required: [kind], properties: {kind: {type: string, enum: [cat]}, meow: {type: boolean}}}
        meta:
          type: object
          properties:
            age: {type: integer}
            tag: {type: string}
    Widget:
      type: object
      required: [id, colour]
      properties:
        id: {type: string}
        colour: {type: string}
    WidgetAck:
      type: object
      required: [id]
      properties:
        id: {type: string}
    ReceiptSummary:
      type: object
      required: [id, version]
      properties:
        id: {type: string}
        version: {type: integer}
    Receipt:
      type: object
      required: [id, version, count]
      properties:
        id: {type: string}
        version: {type: integer}
        count: {type: integer}
    Capture:
      type: object
      required: [id, amount, status]
      properties:
        id: {type: string}
        amount: {type: integer}
        status: {type: string}
    Refund:
      type: object
      required: [id, amount]
      properties:
        id: {type: string}
        amount: {type: integer}
    Org:
      type: object
      required: [id, login]
      properties:
        id: {type: string}
        login: {type: string}
    Member:
      type: object
      required: [id, login]
      properties:
        id: {type: string}
        login: {type: string}
    Team:
      type: object
      required: [id, slug, name, url]
      properties:
        id: {type: string}
        slug: {type: string}
        name: {type: string}
        url: {type: string}
"##;

fn jira_contract() -> Vec<u8> {
    let path = format!(
        "{}/../../examples/apis/jira/jira.openapi.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(path).unwrap()
}

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

fn harness_with(
    contract: &[u8],
    script: Vec<Result<String, LlmError>>,
    change: impl FnOnce(&mut MockRuntime),
) -> Harness {
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        // As a server runs it, with the response cache.
        CachingStateStore::wrap(
            Arc::clone(&store) as Arc<dyn StateStore>,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
    );
    change(&mut rt);
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

fn harness(contract: &[u8], script: Vec<Result<String, LlmError>>) -> Harness {
    harness_with(contract, script, |_| {})
}

fn things(script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    harness(THINGS.as_bytes(), script)
}

impl Harness {
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

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    /// The user message of the `n`th LLM call.
    fn prompt(&self, n: usize) -> String {
        let requests = self.provider.requests();
        requests[n].messages.last().unwrap().content.clone()
    }

    async fn memory(&self) -> Vec<StoredResource> {
        self.store.list(&session(), MEMORY, 100).await.unwrap()
    }
}

/// The text between the block's tags, or `None` without one.
fn block(prompt: &str) -> Option<&str> {
    let start = prompt.find("<earlier_requests>")? + "<earlier_requests>".len();
    let end = prompt.find("</earlier_requests>")?;
    Some(&prompt[start..end])
}

fn created_issue() -> Value {
    json!({
        "id": "10001",
        "key": "PLNT-1",
        "self": "https://juniper.atlassian.net/rest/api/3/issue/10001",
    })
}

/// A valid `IssueBean` of the Jira contract.
fn issue_bean(summary: &str, status: &str, labels: &[&str]) -> Value {
    let user = json!({
        "self": "https://juniper.atlassian.net/rest/api/3/user?accountId=712020:12345678-1234-1234-1234-123456789abc",
        "accountId": "712020:12345678-1234-1234-1234-123456789abc",
        "accountType": "atlassian",
        "displayName": "Ann Lee",
        "active": true,
        "timeZone": "America/Los_Angeles",
        "avatarUrls": {"16x16": "https://a/16", "24x24": "https://a/24", "32x32": "https://a/32", "48x48": "https://a/48"},
    });
    json!({
        "expand": "renderedFields,names,schema,operations,editmeta,changelog,versionedRepresentations",
        "id": "10001",
        "self": "https://juniper.atlassian.net/rest/api/3/issue/10001",
        "key": "PLNT-1",
        "fields": {
            "summary": summary,
            "description": {"type": "doc", "version": 1, "content": [{"type": "paragraph", "content": [{"type": "text", "text": "It stops."}]}]},
            "status": {
                "self": "https://juniper.atlassian.net/rest/api/3/status/1",
                "id": if status == "Done" { "10001" } else { "10000" },
                "name": status,
                "statusCategory": {"self": "https://a/1", "id": 2, "key": "new", "colorName": "blue-gray", "name": "To Do"},
            },
            "issuetype": {"self": "https://a/t", "id": "10001", "name": "Bug", "subtask": false, "hierarchyLevel": 0},
            "project": {"self": "https://juniper.atlassian.net/rest/api/3/project/10000", "id": "10000", "key": "PLNT", "name": "Plant Ops", "projectTypeKey": "software"},
            "priority": {"self": "https://a/pr", "id": "1", "name": "High"},
            "reporter": user,
            "assignee": user,
            "labels": labels,
            "created": "2026-10-27T09:14:03.512-07:00",
            "updated": "2026-10-27T09:14:03.512-07:00",
        },
    })
}

fn jira(script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    harness(&jira_contract(), script)
}

fn create_issue_request() -> Value {
    json!({"fields": {
        "summary": "Pump fails after restart",
        "labels": ["pump-bug", "field-report"],
        "project": {"key": "PLNT"},
        "issuetype": {"name": "Bug"},
    }})
}

/// Jira's create answers `id`, `key` and `self`: the read after it is told what
/// the request said, and the next read needs no LLM.
#[tokio::test]
async fn a_minimal_create_reaches_its_later_read() {
    let bean = issue_bean(
        "Pump fails after restart",
        "To Do",
        &["pump-bug", "field-report"],
    );
    let h = jira(vec![created_issue(), bean.clone()]);
    let (status, _, _) = h
        .call("POST", "/rest/api/3/issue", Some(create_issue_request()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(h.calls(), 1);

    let (status, source, body) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source, "llm");
    assert_eq!(h.calls(), 2, "one generation for the read");
    let prompt = h.prompt(1);
    let earlier = block(&prompt).expect("the read's prompt holds the block");
    assert!(earlier.contains("Pump fails after restart"), "{prompt}");
    assert!(earlier.contains("pump-bug"), "{prompt}");
    assert!(earlier.contains("createIssue"), "{prompt}");
    assert!(earlier.contains("/rest/api/3/issue"), "{prompt}");
    assert_eq!(body["fields"]["summary"], "Pump fails after restart");

    let (_, source, again) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 2, "the next read answers from state");
    assert_eq!(again, body);
}

/// Jira's transition answers 204, so no LLM call happens; the read after it
/// generates once, with the request in its prompt, and the next does not.
#[tokio::test]
async fn a_204_action_reaches_its_later_read_once() {
    let h = jira(vec![
        created_issue(),
        issue_bean("Pump fails", "To Do", &[]),
        issue_bean("Pump fails", "Done", &[]),
    ]);
    h.call("POST", "/rest/api/3/issue", Some(create_issue_request()))
        .await;
    let (_, _, first) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(first["fields"]["status"]["name"], "To Do");
    assert_eq!(h.calls(), 2);

    let (status, _, _) = h
        .call(
            "POST",
            "/rest/api/3/issue/PLNT-1/transitions",
            Some(json!({"transition": {"id": "31"}})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(h.calls(), 2, "a 204 calls no LLM");

    let (_, source, read) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(
        source, "llm",
        "the stored copy does not reflect the transition"
    );
    assert_eq!(h.calls(), 3);
    let prompt = h.prompt(2);
    let earlier = block(&prompt).expect("the read's prompt holds the block");
    assert!(earlier.contains("doTransition"), "{prompt}");
    assert!(earlier.contains("\"31\""), "{prompt}");
    assert!(
        earlier.contains("/rest/api/3/issue/{issueIdOrKey}/transitions"),
        "{prompt}"
    );
    assert!(
        !earlier.contains("createIssue"),
        "a request already reflected is not shown again: {prompt}"
    );
    assert!(prompt.contains("<existing_resource>"), "{prompt}");
    assert_eq!(read["fields"]["status"]["name"], "Done");

    let (_, source, again) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 3, "the next read answers from state");
    assert_eq!(again["fields"]["status"]["name"], "Done");
}

/// A 204 with no request body is remembered too: the path says what it did.
#[tokio::test]
async fn a_204_with_no_body_is_remembered() {
    let h = things(vec![
        json!({"id": "i1", "title": "t", "status": "open"}),
        json!({"id": "i1", "title": "t", "status": "closed"}),
    ]);
    // Stored by a read, which makes it what the session holds.
    h.call("GET", "/items/i1", None).await;
    assert_eq!(h.calls(), 1);
    let (status, _, _) = h.call("POST", "/items/i1/close", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, source, read) = h.call("GET", "/items/i1", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["status"], "closed");
    let prompt = h.prompt(1);
    assert!(
        block(&prompt).unwrap().contains("/items/{item_id}/close"),
        "{prompt}"
    );
    let (_, source, _) = h.call("GET", "/items/i1", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 2);
}

/// A create under a parent reaches the parent's next read, once. The refund has
/// no request body here: its path is all that tells the capture.
#[tokio::test]
async fn a_create_under_a_parent_shows_on_the_parents_read() {
    let h = things(vec![
        json!({"id": "cap1", "amount": 100, "status": "COMPLETED"}),
        json!({"id": "ref1", "amount": 100}),
        json!({"id": "cap1", "amount": 100, "status": "REFUNDED"}),
    ]);
    h.call("POST", "/captures", Some(json!({"amount": 100})))
        .await;
    let (_, source, _) = h.call("GET", "/captures/cap1", None).await;
    assert_eq!(source, "state");

    let (status, _, _) = h.call("POST", "/captures/cap1/refunds", None).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(h.calls(), 2);

    let (_, source, read) = h.call("GET", "/captures/cap1", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["status"], "REFUNDED");
    assert_eq!(h.calls(), 3);
    let prompt = h.prompt(2);
    let earlier = block(&prompt).expect("the parent's read holds the block");
    assert!(earlier.contains("refundCapture"), "{prompt}");
    assert!(earlier.contains("/captures/{id}/refunds"), "{prompt}");

    let (_, source, _) = h.call("GET", "/captures/cap1", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 3);

    // The refund's own read needs nothing: its answer was the whole of it.
    let (_, source, _) = h.call("GET", "/refunds/ref1", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 3);
}

/// A create under a parent the session does not hold remembers nothing for it.
#[tokio::test]
async fn a_parent_the_session_does_not_hold_is_not_remembered() {
    let h = things(vec![json!({"id": "ref1", "amount": 5})]);
    h.call("POST", "/captures/nope/refunds", Some(json!({"amount": 5})))
        .await;
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);
}

/// A write whose answer carried the change through an applied effect has
/// already reflected: no extra call.
#[tokio::test]
async fn an_applied_effect_on_the_parent_costs_no_extra_call() {
    let h = things(vec![
        json!({"id": "cap1", "amount": 100, "status": "COMPLETED"}),
        json!({
            "effects": [{"type": "captures", "id": "cap1", "set": {"status": "REFUNDED"}}],
            "response": {"id": "ref1", "amount": 100},
        }),
    ]);
    h.call("POST", "/captures", Some(json!({"amount": 100})))
        .await;
    // `capture_id` shows the capture to the answer, so it can list a change.
    let (status, _, _) = h
        .call(
            "POST",
            "/captures/cap1/refunds",
            Some(json!({"amount": 100, "capture_id": "cap1"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let held = h.store.get(&session(), "captures", "cap1").await.unwrap();
    assert_eq!(
        held.unwrap().data["status"],
        "REFUNDED",
        "the effect applied"
    );

    let (_, source, read) = h.call("GET", "/captures/cap1", None).await;
    assert_eq!(source, "state", "the effect already reflected it");
    assert_eq!(read["status"], "REFUNDED");
    assert_eq!(h.calls(), 2);
}

/// A create answering its read's own schema was written with the request in
/// front of it, as a read would be: nothing to remember, even for what it left
/// out, which that schema may have no member for.
#[tokio::test]
async fn a_create_answering_as_its_read_remembers_nothing() {
    let h = things(vec![json!({"id": "n1", "text": "hello", "pinned": true})]);
    h.call(
        "POST",
        "/notes",
        Some(json!({"text": "hello", "pinned": true, "colour": "red"})),
    )
    .await;
    assert!(h.memory().await.is_empty());
    let (_, source, _) = h.call("GET", "/notes/n1", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 1);
}

/// A shorter answer that holds what the request said leaves nothing to remember.
#[tokio::test]
async fn a_short_answer_holding_the_request_remembers_nothing() {
    let h = things(vec![json!({"id": "r1", "version": 2})]);
    h.call("POST", "/receipts", Some(json!({"version": 2})))
        .await;
    assert!(h.memory().await.is_empty());
}

/// A number or a flag only counts under its own name: `1` here is not `1` there.
#[tokio::test]
async fn a_small_value_elsewhere_does_not_count_as_carried() {
    let h = things(vec![json!({"id": "r1", "version": 1})]);
    h.call("POST", "/receipts", Some(json!({"count": 1}))).await;
    assert_eq!(h.memory().await.len(), 1);
}

/// An update whose answer replaced the target needs no regeneration, and what
/// was waiting for that target was shown to it and is done.
#[tokio::test]
async fn an_update_that_replaced_the_target_causes_no_extra_call() {
    let h = things(vec![
        json!({"id": "i1", "title": "t", "status": "open"}),
        json!({"id": "i1", "title": "new", "status": "closed"}),
    ]);
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;
    assert_eq!(h.memory().await.len(), 1);

    let (status, _, _) = h
        .call("PATCH", "/items/i1", Some(json!({"title": "new"})))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.calls(), 2);
    let prompt = h.prompt(1);
    assert!(
        block(&prompt).unwrap().contains("closeItem"),
        "the update is told of the close: {prompt}"
    );
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);

    let (_, source, read) = h.call("GET", "/items/i1", None).await;
    assert_eq!(source, "state");
    assert_eq!(read["title"], "new");
    assert_eq!(h.calls(), 2, "no regeneration");
}

/// A delete takes its target's memory with it.
#[tokio::test]
async fn a_delete_clears_its_targets_memory() {
    let h = things(vec![json!({"id": "i1", "title": "t", "status": "open"})]);
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;
    assert_eq!(h.memory().await.len(), 1);
    let (status, _, _) = h.call("DELETE", "/items/i1", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.memory().await.is_empty());
}

/// Under `consistency: off` none of this happens.
#[tokio::test]
async fn consistency_off_remembers_nothing() {
    let h = harness_with(
        &jira_contract(),
        vec![
            Ok(created_issue().to_string()),
            Ok(issue_bean("invented", "To Do", &[]).to_string()),
        ],
        |rt| rt.consistency = false,
    );
    h.call("POST", "/rest/api/3/issue", Some(create_issue_request()))
        .await;
    h.call("GET", "/rest/api/3/issue/10001", None).await;
    assert_eq!(h.calls(), 2);
    assert!(block(&h.prompt(1)).is_none(), "{}", h.prompt(1));
    assert!(h.memory().await.is_empty());

    let h = harness_with(
        THINGS.as_bytes(),
        vec![Ok(
            json!({"id": "i1", "title": "t", "status": "open"}).to_string()
        )],
        |rt| rt.consistency = false,
    );
    h.store
        .upsert(
            &session(),
            StoredResource {
                resource_type: "items".into(),
                id: "i1".into(),
                data: json!({"id": "i1", "title": "t", "status": "open"}),
            },
        )
        .await
        .unwrap();
    h.call("POST", "/items/i1/close", None).await;
    assert!(h.memory().await.is_empty());
    let (_, source, _) = h.call("GET", "/items/i1", None).await;
    assert_eq!(source, "state", "as it always answered");
    assert_eq!(h.calls(), 0);
}

fn offline() -> (axum::Router, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(&jira_contract()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&store) as Arc<dyn StateStore>,
    );
    (http_rest::router(Arc::new(rt)), store)
}

/// An offline mock remembers nothing and invents no change.
#[tokio::test]
async fn offline_changes_nothing() {
    let (router, store) = offline();
    let bean = issue_bean("as seeded", "To Do", &[]);
    store
        .upsert(
            &session(),
            StoredResource {
                resource_type: "issue".into(),
                id: "10001".into(),
                data: bean.clone(),
            },
        )
        .await
        .unwrap();
    let post = Request::post("/rest/api/3/issue/PLNT-1/transitions")
        .header("content-type", "application/json")
        .body(Body::from(json!({"transition": {"id": "31"}}).to_string()))
        .unwrap();
    let resp = router.clone().oneshot(post).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(store.list(&session(), MEMORY, 10).await.unwrap().is_empty());

    let get = Request::get("/rest/api/3/issue/PLNT-1")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(get).await.unwrap();
    assert_eq!(resp.headers()["x-mockingbird-source"], "state");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body, bean);
}

/// However long a session runs, one resource keeps the newest few under a byte
/// cap, and a read is told of them in order.
#[tokio::test]
async fn a_long_session_stays_within_the_caps() {
    let h = things(vec![
        json!({"id": "i1", "title": "t", "status": "open"}),
        json!({"id": "i1", "title": "t", "status": "closed"}),
    ]);
    h.call("GET", "/items/i1", None).await;
    for i in 0..40 {
        let filler = "x".repeat(1024);
        let (status, _, _) = h
            .call(
                "POST",
                "/items/i1/close",
                Some(json!({"round": i, "note": format!("round-{i}-{filler}")})),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let memory = h.memory().await;
    assert_eq!(memory.len(), 1, "one document per resource");
    let requests = memory[0].data["requests"].as_array().unwrap();
    assert!(requests.len() <= 5, "{} kept", requests.len());
    assert!(memory[0].data.to_string().len() <= 16 * 1024);

    h.call("GET", "/items/i1", None).await;
    assert_eq!(h.calls(), 2, "one generation however many writes");
    let prompt = h.prompt(1);
    let earlier = block(&prompt).unwrap();
    assert!(earlier.contains("round-39-"), "the newest is kept");
    assert!(!earlier.contains("round-0-"), "the oldest went");
    assert!(
        earlier.find("round-38-").unwrap() < earlier.find("round-39-").unwrap(),
        "oldest first"
    );
    assert!(h.memory().await.is_empty());
}

/// One large body is cut, not stored whole.
#[tokio::test]
async fn a_large_body_is_cut() {
    let h = things(vec![json!({"id": "i1", "title": "t", "status": "open"})]);
    h.call("GET", "/items/i1", None).await;
    let big = "y".repeat(100 * 1024);
    h.call("POST", "/items/i1/close", Some(json!({"note": big})))
        .await;
    let memory = h.memory().await;
    assert!(memory[0].data.to_string().len() <= 16 * 1024);
}

/// A session reset clears the memory: the read that follows is told nothing.
#[tokio::test]
async fn a_session_reset_clears_it() {
    let h = things(vec![
        json!({"id": "i1", "title": "t", "status": "open"}),
        json!({"id": "i1", "title": "t", "status": "open"}),
    ]);
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;
    assert_eq!(h.memory().await.len(), 1);
    h.store.reset(&session()).await.unwrap();
    assert!(h.memory().await.is_empty());
    h.call("GET", "/items/i1", None).await;
    assert!(block(&h.prompt(1)).is_none(), "{}", h.prompt(1));
}

/// The memory is no resource: not exported, not counted, not listed under any
/// contract type, not on the change feed, and a fixture cannot plant it.
#[tokio::test]
async fn the_memory_stays_out_of_what_a_session_shows() {
    let h = things(vec![json!({"id": "i1", "title": "t", "status": "open"})]);
    h.call("GET", "/items/i1", None).await;
    let mut changes = h.store.subscribe();
    h.call("POST", "/items/i1/close", Some(json!({"reason": "done"})))
        .await;
    assert_eq!(h.memory().await.len(), 1, "it is remembered");
    while let Ok(change) = changes.try_recv() {
        assert_ne!(change.resource_type, MEMORY, "{change:?}");
    }

    let exported = h.store.export(&session()).await.unwrap();
    assert!(
        exported.iter().all(|r| r.resource_type != MEMORY),
        "{exported:?}"
    );
    assert_eq!(exported.len(), 1);
    let sessions = h.store.sessions("default", "m").await.unwrap();
    assert_eq!(sessions[0].resources, 1, "the memory is not counted");
    let listed = h.store.list(&session(), "items", 10).await.unwrap();
    assert_eq!(listed.len(), 1);

    // Seeding replaces the session's contents, the memory with them.
    h.store.seed(&session(), exported).await.unwrap();
    assert!(h.memory().await.is_empty());
}

/// A delete of the memory is on no feed either.
#[tokio::test]
async fn forgetting_is_on_no_change_feed() {
    let h = things(vec![
        json!({"id": "i1", "title": "t", "status": "open"}),
        json!({"id": "i1", "title": "t", "status": "closed"}),
    ]);
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;
    let mut changes = h.store.subscribe();
    h.call("GET", "/items/i1", None).await;
    let mut upserted = 0;
    while let Ok(change) = changes.try_recv() {
        assert_ne!(change.resource_type, MEMORY, "{change:?}");
        assert_eq!(change.kind, ChangeKind::Upserted);
        upserted += 1;
    }
    assert_eq!(upserted, 1, "the regenerated resource, once");
}

/// Stored bodies go through the scrubbers of the request log and the effects: a
/// credential-named key is redacted, and the PII scrubber runs over the rest.
#[tokio::test]
async fn a_secret_in_a_request_body_is_scrubbed() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        Ok(json!({"id": "i1", "title": "t", "status": "open"}).to_string()),
        Ok(json!({"id": "i1", "title": "t", "status": "closed"}).to_string()),
    ]));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let scrubber =
        mb_engine::pii::Scrubber::new(&mb_engine::pii::ScrubberConfig::default()).unwrap();
    let engine = Engine::new(
        Some(Arc::clone(&provider) as Arc<dyn Provider>),
        EngineConfig::default(),
    )
    .with_guards(mb_engine::EngineGuards {
        scrubber: Some(Arc::new(scrubber)),
        ..Default::default()
    });
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(THINGS.as_bytes()).unwrap()),
        Arc::new(engine),
        Arc::clone(&store) as Arc<dyn StateStore>,
    );
    let h = Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    };
    h.call("GET", "/items/i1", None).await;
    h.call(
        "POST",
        "/items/i1/close",
        Some(json!({
            "reason": "paid",
            "password": "hunter2-do-not-keep",
            "nested": {"apiKey": "sk-live-do-not-keep"},
            "contact": "ann.lee@corp.example.org",
        })),
    )
    .await;
    let stored = h.memory().await[0].data.to_string();
    assert!(!stored.contains("hunter2-do-not-keep"), "{stored}");
    assert!(!stored.contains("sk-live-do-not-keep"), "{stored}");
    assert!(!stored.contains("ann.lee@corp.example.org"), "{stored}");
    assert!(stored.contains("[redacted]"), "{stored}");
    assert!(stored.contains("paid"), "{stored}");

    h.call("GET", "/items/i1", None).await;
    let prompt = h.prompt(1);
    let earlier = block(&prompt).unwrap();
    assert!(!earlier.contains("hunter2-do-not-keep"), "{prompt}");
    assert!(earlier.contains("paid"), "{prompt}");
}

/// Keeps every record.
#[derive(Default)]
struct Records(Mutex<Vec<RequestRecord>>);

impl RequestRecorder for Records {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

/// Redacts one key, as a configured request log would.
#[derive(Default)]
struct RedactsReason(Mutex<usize>);

impl RequestRecorder for RedactsReason {
    fn record(&self, _r: RequestRecord) {}

    fn redact(&self, value: &mut Value) {
        *self.0.lock().unwrap() += 1;
        if let Some(reason) = value.get_mut("reason") {
            *reason = Value::from("[redacted]");
        }
    }
}

/// The request log's own redaction (its configured `redact_fields`) applies.
#[tokio::test]
async fn the_request_logs_redaction_applies() {
    let recorder = Arc::new(RedactsReason::default());
    let held = Arc::clone(&recorder);
    let h = harness_with(
        THINGS.as_bytes(),
        vec![Ok(
            json!({"id": "i1", "title": "t", "status": "open"}).to_string()
        )],
        move |rt| rt.recorder = Some(held),
    );
    h.call("GET", "/items/i1", None).await;
    h.call(
        "POST",
        "/items/i1/close",
        Some(json!({"reason": "private words"})),
    )
    .await;
    let stored = h.memory().await[0].data.to_string();
    assert!(!stored.contains("private words"), "{stored}");
    assert!(*recorder.0.lock().unwrap() > 0);
}

/// An LLM failure on the read is not marked as reflected and is not cached, so
/// the next read tries again. With a stored copy that can be served, that copy
/// answers: a faker must not invent a change.
#[tokio::test]
async fn an_llm_failure_on_the_read_retries_next_time() {
    let h = harness(
        THINGS.as_bytes(),
        vec![
            Ok(json!({"id": "i1", "title": "t", "status": "open"}).to_string()),
            Err(LlmError::Transport("down".into())),
            Ok(json!({"id": "i1", "title": "t", "status": "closed"}).to_string()),
        ],
    );
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;

    let (status, source, read) = h.call("GET", "/items/i1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source, "state", "the stored copy, not a faker's invention");
    assert_eq!(read["status"], "open");
    assert_eq!(h.calls(), 2, "the read tried the LLM");
    assert_eq!(h.memory().await.len(), 1, "not marked as reflected");

    let (_, source, read) = h.call("GET", "/items/i1", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["status"], "closed");
    assert_eq!(h.calls(), 3);
    assert!(h.memory().await.is_empty());

    let (_, source, _) = h.call("GET", "/items/i1", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 3);
}

/// With no stored copy to serve, a failure falls back to the faker as it always
/// did, and the request stays remembered for the next read.
#[tokio::test]
async fn an_llm_failure_with_no_servable_copy_falls_back_and_retries() {
    let h = harness(
        &jira_contract(),
        vec![
            Ok(created_issue().to_string()),
            Err(LlmError::Transport("down".into())),
            Ok(issue_bean("Pump fails after restart", "To Do", &[]).to_string()),
        ],
    );
    h.call("POST", "/rest/api/3/issue", Some(create_issue_request()))
        .await;
    let (_, source, _) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(source, "fallback");
    assert_eq!(h.memory().await.len(), 1);
    let (_, source, read) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(source, "llm", "the failure was not cached");
    assert_eq!(read["fields"]["summary"], "Pump fails after restart");
    assert_eq!(h.calls(), 3);
    assert!(
        block(&h.prompt(2))
            .unwrap()
            .contains("Pump fails after restart")
    );
    assert!(h.memory().await.is_empty());
}

/// A faker's write that stood in for a failed LLM call says nothing of the
/// request, so the request is kept for when the LLM is back.
#[tokio::test]
async fn a_write_answered_by_the_faker_is_remembered() {
    let h = harness(
        THINGS.as_bytes(),
        vec![
            Ok(json!({"id": "i1", "title": "t", "status": "open"}).to_string()),
            Err(LlmError::Transport("down".into())),
            Ok(json!({"id": "i1", "title": "new", "status": "open"}).to_string()),
        ],
    );
    h.call("GET", "/items/i1", None).await;
    let (_, source, _) = h
        .call("PATCH", "/items/i1", Some(json!({"title": "new"})))
        .await;
    assert_eq!(source, "fallback");
    assert_eq!(h.memory().await.len(), 1);
    let (_, source, read) = h.call("GET", "/items/i1", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["title"], "new");
    assert!(h.memory().await.is_empty());
}

/// The update's answer stands over the stored copy when it was told of a write
/// that copy does not show, or marking that write done would lose it.
#[tokio::test]
async fn an_update_told_of_earlier_requests_keeps_what_it_answered() {
    let h = things(vec![
        json!({"id": "i1", "title": "t", "status": "open"}),
        json!({"id": "i1", "title": "new", "status": "closed"}),
    ]);
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;
    let (_, _, patched) = h
        .call("PATCH", "/items/i1", Some(json!({"title": "new"})))
        .await;
    assert_eq!(patched["status"], "closed");
    let (_, source, read) = h.call("GET", "/items/i1", None).await;
    assert_eq!(source, "state");
    assert_eq!(read["status"], "closed");
}

/// Jira's edit answers 204 too: what the read's schema declares goes into the
/// stored copy at once, and reading by key finds it with no LLM call.
#[tokio::test]
async fn a_204_update_merges_into_the_copy_found_by_key() {
    let h = jira(vec![
        created_issue(),
        issue_bean("Pump fails", "To Do", &[]),
    ]);
    h.call("POST", "/rest/api/3/issue", Some(create_issue_request()))
        .await;
    h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    let (status, _, _) = h
        .call(
            "PUT",
            "/rest/api/3/issue/PLNT-1",
            Some(json!({"fields": {"summary": "Pump fails badly"}})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);
    let (_, source, read) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(source, "state");
    assert_eq!(read["key"], "PLNT-1");
    assert_eq!(read["fields"]["summary"], "Pump fails badly");
    assert_eq!(read["fields"]["status"]["name"], "To Do", "the rest stands");
    assert_eq!(h.calls(), 2);
    assert_eq!(
        h.store.list(&session(), "issue", 10).await.unwrap().len(),
        1,
        "no second issue appears under the key"
    );
}

fn pet(name: &str) -> Value {
    json!({"id": "p1", "name": name, "meta": {"age": 1, "tag": "calm"}})
}

/// A plain 204 update: readable, listable, exported and served with the LLM down,
/// from the moment it was made, with no call.
#[tokio::test]
async fn a_plain_204_update_applies_at_write_time() {
    let h = things(vec![pet("old")]);
    h.call("POST", "/pets", Some(json!({"name": "old"}))).await;
    let (status, _, _) = h
        .call(
            "PUT",
            "/pets/p1",
            Some(json!({"name": "new", "meta": {"age": 2}})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);
    let exported = h.store.export(&session()).await.unwrap();
    assert_eq!(exported[0].data["name"], "new", "{exported:?}");
    assert_eq!(exported[0].data["meta"]["tag"], "calm", "a deep merge");
    assert_eq!(exported[0].data["meta"]["age"], 2);
    let (_, source, read) = h.call("GET", "/pets/p1", None).await;
    assert_eq!(source, "state");
    assert_eq!(read["name"], "new");
    assert_eq!(h.calls(), 1, "no LLM call for a value the client sent");
}

/// Only what could not be applied is remembered.
#[tokio::test]
async fn a_204_update_remembers_only_what_it_could_not_apply() {
    let h = things(vec![
        pet("old"),
        json!({"id": "p1", "name": "new", "meta": {"age": 1, "tag": "calm"}}),
    ]);
    h.call("POST", "/pets", Some(json!({"name": "old"}))).await;
    h.call(
        "PUT",
        "/pets/p1",
        Some(json!({"name": "new", "transition": {"id": "31"}})),
    )
    .await;
    let stored = h
        .store
        .get(&session(), "pets", "p1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.data["name"], "new", "applied");
    let memory = h.memory().await;
    assert_eq!(memory.len(), 1, "{memory:?}");
    let body = &memory[0].data["requests"][0]["body"];
    assert_eq!(body, &json!({"transition": {"id": "31"}}), "{memory:?}");
    h.call("GET", "/pets/p1", None).await;
    assert_eq!(h.calls(), 2);
}

/// A value the read's schema refuses is not applied, and stays to be told.
#[tokio::test]
async fn a_204_update_that_breaks_the_schema_is_remembered_not_merged() {
    let h = things(vec![pet("old")]);
    h.call("POST", "/pets", Some(json!({"name": "old"}))).await;
    h.call(
        "PUT",
        "/pets/p1",
        Some(json!({"name": 7, "meta": {"age": 3}})),
    )
    .await;
    let stored = h
        .store
        .get(&session(), "pets", "p1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.data["name"], "old");
    assert_eq!(
        stored.data["meta"]["age"], 3,
        "the member that fits applies"
    );
    let memory = h.memory().await;
    assert_eq!(memory[0].data["requests"][0]["body"], json!({"name": 7}));
}

/// A 204 update repeating what the copy holds has nothing to tell.
#[tokio::test]
async fn a_204_update_the_copy_already_holds_remembers_nothing() {
    let h = things(vec![pet("same")]);
    h.call("POST", "/pets", Some(json!({"name": "same"}))).await;
    h.call("PUT", "/pets/p1", Some(json!({"name": "same"})))
        .await;
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);
}

/// An unheld `PUT` with a whole valid body answers from state, with no call.
#[tokio::test]
async fn an_unheld_put_with_a_full_body_answers_from_state() {
    let h = things(vec![]);
    let (status, _, _) = h
        .call("PUT", "/pets/p9", Some(json!({"id": "p9", "name": "new"})))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);
    let (_, source, read) = h.call("GET", "/pets/p9", None).await;
    assert_eq!(source, "state");
    assert_eq!(read["name"], "new");
    assert_eq!(h.calls(), 0);
}

/// Catching up gives way after two attempts: the LLM down, one failed call each,
/// then state with no call until a later write adds a request.
#[tokio::test]
async fn a_catch_up_stops_trying_after_the_cap() {
    let down = || Err(LlmError::Transport("down".into()));
    let records = Arc::new(Records::default());
    let held = Arc::clone(&records);
    let h = harness_with(
        THINGS.as_bytes(),
        vec![
            Ok(json!({"id": "i1", "title": "t", "status": "open"}).to_string()),
            down(),
            down(),
            Ok(json!({"id": "i1", "title": "t", "status": "closed"}).to_string()),
        ],
        move |rt| rt.recorder = Some(held),
    );
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;
    for _ in 0..2 {
        let (_, source, _) = h.call("GET", "/items/i1", None).await;
        assert_eq!(source, "state");
    }
    assert_eq!(h.calls(), 3, "two attempts");
    for _ in 0..3 {
        let (_, source, read) = h.call("GET", "/items/i1", None).await;
        assert_eq!(source, "state");
        assert_eq!(read["status"], "open");
    }
    assert_eq!(h.calls(), 3, "none after the cap");
    let last = records.0.lock().unwrap().last().unwrap().request.clone();
    assert!(
        last.to_string().contains("gave up"),
        "the record says so: {last}"
    );

    // A later write adds a request, and the count starts over.
    h.call("POST", "/items/i1/close", None).await;
    let (_, source, read) = h.call("GET", "/items/i1", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["status"], "closed");
    assert_eq!(h.calls(), 4);
    assert!(h.memory().await.is_empty());
}

/// An answer that never validates costs two calls an attempt, for two attempts.
#[tokio::test]
async fn a_catch_up_failing_validation_is_capped_too() {
    let bad = || Ok(json!({"nonsense": true}).to_string());
    let h = harness(
        THINGS.as_bytes(),
        vec![
            Ok(json!({"id": "i1", "title": "t", "status": "open"}).to_string()),
            bad(),
            bad(),
            bad(),
            bad(),
            bad(),
            bad(),
            bad(),
            bad(),
        ],
    );
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;
    h.call("GET", "/items/i1", None).await;
    let one_attempt = h.calls() - 1;
    for _ in 0..5 {
        let (_, source, _) = h.call("GET", "/items/i1", None).await;
        assert_eq!(source, "state");
    }
    assert_eq!(h.calls(), 1 + 2 * one_attempt, "two attempts, then none");
}

/// A stored copy that cannot be served still generates, as it always did, with
/// its faker answer cached once the cap is reached: no call per read for ever.
#[tokio::test]
async fn a_capped_short_copy_stops_calling_too() {
    let down = || Err(LlmError::Transport("down".into()));
    let h = harness(
        &jira_contract(),
        vec![Ok(created_issue().to_string()), down(), down()],
    );
    h.call("POST", "/rest/api/3/issue", Some(create_issue_request()))
        .await;
    for _ in 0..2 {
        let (_, source, _) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
        assert_eq!(source, "fallback");
    }
    let tried = h.calls();
    assert_eq!(tried, 3);
    let (_, source, _) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(source, "fallback", "a repeat of the faker's, as ever");
    assert_eq!(h.calls(), tried, "no call per read for ever");
}

/// A catch-up keeps what its answer left out at any depth, and what identifies
/// the resource.
#[tokio::test]
async fn a_catch_up_keeps_nested_members() {
    let h = things(vec![
        pet("Rex"),
        json!({"id": "other", "name": "Rex", "meta": {"age": 3}}),
    ]);
    h.call("POST", "/pets", Some(json!({"name": "Rex"}))).await;
    h.call("POST", "/pets/p1/birthday", None).await;
    let (_, source, read) = h.call("GET", "/pets/p1", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["id"], "p1", "identity pinned");
    assert_eq!(read["meta"]["age"], 3, "the answer stands");
    assert_eq!(read["meta"]["tag"], "calm", "nested member kept");
    let (_, source, again) = h.call("GET", "/pets/p1", None).await;
    assert_eq!(source, "state");
    assert_eq!(again, read);
}

async fn hold(h: &Harness, resource_type: &str, id: &str, data: Value) {
    h.store
        .upsert(
            &session(),
            StoredResource {
                resource_type: resource_type.into(),
                id: id.into(),
                data,
            },
        )
        .await
        .unwrap();
}

/// A child's body is about the child. Its members may share names with its
/// parent's, and the parent's stored values stand.
#[tokio::test]
async fn a_childs_body_is_never_overlaid_on_its_parent() {
    let post = json!({"id": "po1", "body": "post text", "status": "open"});
    let h = things(vec![
        json!({"id": "c1", "body": "child text"}),
        post.clone(),
    ]);
    hold(&h, "posts", "po1", post.clone()).await;
    h.call(
        "POST",
        "/posts/po1/comments",
        Some(json!({"body": "child text", "status": "pending"})),
    )
    .await;
    let (_, source, read) = h.call("GET", "/posts/po1", None).await;
    assert_eq!(source, "llm");
    assert!(
        block(&h.prompt(1)).unwrap().contains("child text"),
        "still told"
    );
    assert_eq!(read["body"], "post text");
    assert_eq!(read["status"], "open");
}

/// An action's body is what it was asked, not what its target became.
#[tokio::test]
async fn an_actions_body_is_never_overlaid_on_its_target() {
    let h = things(vec![json!({"id": "in1", "amount": 100, "status": "paid"})]);
    hold(
        &h,
        "invoices",
        "in1",
        json!({"id": "in1", "amount": 100, "status": "open"}),
    )
    .await;
    h.call("POST", "/invoices/in1/pay", Some(json!({"amount": 5})))
        .await;
    let (_, source, read) = h.call("GET", "/invoices/in1", None).await;
    assert_eq!(source, "llm");
    assert!(block(&h.prompt(0)).unwrap().contains("\"amount\":5"));
    assert_eq!(read["amount"], 100);
    assert_eq!(read["status"], "paid");
}

/// A null the schema does not allow is not applied, so it is told.
#[tokio::test]
async fn a_null_on_a_non_nullable_member_is_remembered() {
    let h = things(vec![pet("old")]);
    h.call("POST", "/pets", Some(json!({"name": "old"}))).await;
    for body in [
        json!({"name": null}),
        json!({"meta": null}),
        json!({"nickname": null}),
    ] {
        h.call("PUT", "/pets/p1", Some(body.clone())).await;
        let memory = h.memory().await;
        let requests = memory[0].data["requests"].as_array().unwrap();
        assert_eq!(requests.last().unwrap()["body"], body, "{memory:?}");
    }
    let stored = h
        .store
        .get(&session(), "pets", "p1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.data["name"], "old");
}

/// A member counts as shown only at its own path: `"x"` elsewhere is not it.
#[tokio::test]
async fn a_value_elsewhere_in_the_copy_is_not_the_member() {
    let h = things(vec![json!({"id": "p1", "name": "x"})]);
    h.call("POST", "/pets", Some(json!({"name": "x"}))).await;
    h.call("PUT", "/pets/p1", Some(json!({"alias": "x"}))).await;
    let memory = h.memory().await;
    assert_eq!(memory[0].data["requests"][0]["body"], json!({"alias": "x"}));
}

/// A redacted member was never compared, so it is not taken as shown.
#[tokio::test]
async fn a_redacted_member_is_not_taken_as_shown() {
    let h = things(vec![pet("old")]);
    h.call("POST", "/pets", Some(json!({"name": "old"}))).await;
    h.call("PUT", "/pets/p1", Some(json!({"token_count": 5})))
        .await;
    assert_eq!(h.memory().await.len(), 1);
}

/// What was redacted is not a value: not overlaid, stored or served, however deep.
#[tokio::test]
async fn a_redacted_value_is_never_overlaid() {
    let headers = json!([{"name": "Authorization", "value": "abc"}]);
    let answer = json!({"id": "p1", "name": "x", "headers": headers});
    let h = things(vec![pet("old"), answer]);
    h.call("POST", "/pets", Some(json!({"name": "old"}))).await;
    h.call(
        "PUT",
        "/pets/p1",
        Some(json!({"headers": headers, "client_ref": "abc"})),
    )
    .await;
    let (_, source, read) = h.call("GET", "/pets/p1", None).await;
    assert_eq!(source, "llm");
    assert!(!read.to_string().contains("[redacted]"), "{read}");
    let stored = h
        .store
        .get(&session(), "pets", "p1")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !stored.data.to_string().contains("[redacted]"),
        "{stored:?}"
    );
}

/// What an update applied stands in the catch-up its remainder causes, even from
/// an answer that went by an older copy.
#[tokio::test]
async fn a_catch_up_cannot_revert_what_the_update_applied() {
    let h = things(vec![pet("old"), pet("old")]);
    h.call("POST", "/pets", Some(json!({"name": "old"}))).await;
    h.call(
        "PUT",
        "/pets/p1",
        Some(json!({"name": "new", "client_ref": "abc"})),
    )
    .await;
    let (_, source, read) = h.call("GET", "/pets/p1", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["name"], "new");
    let (_, source, again) = h.call("GET", "/pets/p1", None).await;
    assert_eq!(source, "state");
    assert_eq!(again["name"], "new");
}

/// A newer update, fully applied, is not reverted by an older one's memory.
#[tokio::test]
async fn an_older_body_cannot_revert_a_newer_update() {
    let h = things(vec![pet("old"), pet("old")]);
    h.call("POST", "/pets", Some(json!({"name": "old"}))).await;
    h.call(
        "PUT",
        "/pets/p1",
        Some(json!({"name": "first", "client_ref": "abc"})),
    )
    .await;
    h.call("PUT", "/pets/p1", Some(json!({"name": "second"})))
        .await;
    let (_, _, read) = h.call("GET", "/pets/p1", None).await;
    assert_eq!(read["name"], "second");
}

/// An update's own body counts on a copy it could not be merged into.
#[tokio::test]
async fn an_updates_own_body_stands_on_a_short_copy() {
    let h = jira(vec![
        created_issue(),
        issue_bean("what the model wrote", "To Do", &[]),
    ]);
    h.call("POST", "/rest/api/3/issue", Some(create_issue_request()))
        .await;
    h.call(
        "PUT",
        "/rest/api/3/issue/PLNT-1",
        Some(json!({"fields": {"summary": "what the client sent"}})),
    )
    .await;
    let (_, _, read) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(read["fields"]["summary"], "what the client sent");
}

/// Nothing merges across `oneOf`: a branch's members could mix with another's.
#[tokio::test]
async fn a_union_member_is_remembered_not_merged() {
    let h = things(vec![
        json!({"id": "p1", "name": "x", "shape": {"kind": "dog", "bark": true}}),
    ]);
    h.call("POST", "/pets", Some(json!({"name": "x"}))).await;
    h.call("PUT", "/pets/p1", Some(json!({"shape": {"meow": true}})))
        .await;
    let stored = h
        .store
        .get(&session(), "pets", "p1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.data["shape"], json!({"kind": "dog", "bark": true}));
    let memory = h.memory().await;
    assert_eq!(
        memory[0].data["requests"][0]["body"],
        json!({"shape": {"meow": true}})
    );
}

/// So too where the union sits beside `properties`: its stale members would mix.
#[tokio::test]
async fn a_member_beside_a_union_is_remembered_not_merged() {
    let h = things(vec![
        json!({"id": "p1", "name": "x", "form": {"kind": "dog", "bark": true}}),
    ]);
    h.call("POST", "/pets", Some(json!({"name": "x"}))).await;
    h.call("PUT", "/pets/p1", Some(json!({"form": {"kind": "cat"}})))
        .await;
    let stored = h
        .store
        .get(&session(), "pets", "p1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.data["form"], json!({"kind": "dog", "bark": true}));
    assert_eq!(h.memory().await.len(), 1);
}

/// A map's members are declared by its `additionalProperties`.
#[tokio::test]
async fn a_map_member_merges_at_write_time() {
    let h = things(vec![
        json!({"id": "p1", "name": "x", "metadata": {"a": "1"}}),
    ]);
    h.call("POST", "/pets", Some(json!({"name": "x"}))).await;
    h.call("PUT", "/pets/p1", Some(json!({"metadata": {"b": "2"}})))
        .await;
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);
    let (_, source, read) = h.call("GET", "/pets/p1", None).await;
    assert_eq!(source, "state");
    assert_eq!(read["metadata"], json!({"a": "1", "b": "2"}));
    assert_eq!(h.calls(), 1);
}

/// A merge leaves identity and `readOnly` members as they are.
#[tokio::test]
async fn a_merge_leaves_identity_and_read_only_members() {
    let h = things(vec![json!({"id": "p1", "name": "x", "version": 1})]);
    h.call("POST", "/pets", Some(json!({"name": "x"}))).await;
    h.call(
        "PUT",
        "/pets/p1",
        Some(json!({"id": "p2", "version": 99, "name": "y"})),
    )
    .await;
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);
    let (_, source, read) = h.call("GET", "/pets/p1", None).await;
    assert_eq!(source, "state");
    assert_eq!(read, json!({"id": "p1", "name": "y", "version": 1}));
}

/// Telling a parent changes nothing it holds, so it publishes nothing and keeps
/// its place in a list.
#[tokio::test]
async fn a_childs_create_does_not_republish_its_parent() {
    let post = |id: &str| json!({"id": id, "body": "text", "status": "open"});
    let h = things(vec![json!({"id": "c1", "body": "hi"}), post("po1")]);
    hold(&h, "posts", "po1", post("po1")).await;
    hold(&h, "posts", "po2", post("po2")).await;
    let order =
        |listed: Vec<StoredResource>| -> Vec<String> { listed.into_iter().map(|r| r.id).collect() };
    let mut changes = h.store.subscribe();
    h.call("POST", "/posts/po1/comments", Some(json!({"body": "hi"})))
        .await;
    while let Ok(change) = changes.try_recv() {
        assert_ne!(change.resource_type, "posts", "{change:?}");
    }
    let listed = h.store.list(&session(), "posts", 10).await.unwrap();
    assert_eq!(order(listed), ["po2", "po1"]);

    // A catch-up that changes nothing publishes nothing either.
    let (_, source, _) = h.call("GET", "/posts/po1", None).await;
    assert_eq!(source, "llm");
    while let Ok(change) = changes.try_recv() {
        assert_ne!(change.resource_type, "posts", "{change:?}");
    }
    let listed = h.store.list(&session(), "posts", 10).await.unwrap();
    assert_eq!(order(listed), ["po2", "po1"]);
    assert!(h.memory().await.is_empty());
}

/// Another node reading a parent gets the catch-up with no publish to hear: the
/// read looks in the shared store before it serves.
#[tokio::test]
async fn a_replica_catches_up_on_a_parent_with_no_publish() {
    let post = json!({"id": "po1", "body": "text", "status": "open"});
    let shared = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let node = |script: Vec<Value>| {
        let provider = Arc::new(ScriptedProvider::new(
            script.into_iter().map(|v| Ok(v.to_string())).collect(),
        ));
        let rt = MockRuntime::new(
            "m",
            Arc::new(compile(THINGS.as_bytes()).unwrap()),
            Arc::new(Engine::new(
                Some(Arc::clone(&provider) as Arc<dyn Provider>),
                EngineConfig::default(),
            )),
            CachingStateStore::wrap(
                Arc::clone(&shared) as Arc<dyn StateStore>,
                Duration::from_secs(3600),
                mb_state::DEFAULT_CACHE_ENTRIES,
            ),
        );
        Harness {
            router: http_rest::router(Arc::new(rt)),
            provider,
            store: Arc::clone(&shared),
        }
    };
    let writer = node(vec![json!({"id": "c1", "body": "hi"})]);
    let reader = node(vec![post.clone()]);
    hold(&writer, "posts", "po1", post.clone()).await;
    let (_, source, _) = reader.call("GET", "/posts/po1", None).await;
    assert_eq!(source, "state");
    writer
        .call("POST", "/posts/po1/comments", Some(json!({"body": "hi"})))
        .await;
    let (_, source, _) = reader.call("GET", "/posts/po1", None).await;
    assert_eq!(source, "llm", "told, on the other node");
    assert!(block(&reader.prompt(0)).unwrap().contains("createComment"));
}

/// The key a path reads by is pinned, whatever the regeneration wrote.
#[tokio::test]
async fn a_catch_up_pins_the_key_its_path_reads_by() {
    let mut rewritten = issue_bean("Pump fails", "Done", &[]);
    rewritten["key"] = json!("WEB-9");
    let h = jira(vec![
        created_issue(),
        issue_bean("Pump fails", "To Do", &[]),
        rewritten,
    ]);
    h.call("POST", "/rest/api/3/issue", Some(create_issue_request()))
        .await;
    h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    h.call(
        "POST",
        "/rest/api/3/issue/PLNT-1/transitions",
        Some(json!({"transition": {"id": "31"}})),
    )
    .await;
    let (_, _, read) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(read["key"], "PLNT-1");
    let (_, source, _) = h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
    assert_eq!(source, "state", "still found by its key");
}

/// An answer that would make the resource another owner's is a failed attempt:
/// the stored copy answers, the memory stays, and the cap applies.
#[tokio::test]
async fn a_catch_up_cannot_move_a_resource_to_another_owner() {
    let h = things(vec![
        team("octo", "Core"),
        team("acme", "Stolen"),
        team("acme", "Stolen"),
    ]);
    octos_team(&h).await;
    h.call(
        "POST",
        "/orgs/octo/teams/core/archive",
        Some(json!({"reason": "old"})),
    )
    .await;
    for _ in 0..2 {
        let (status, source, read) = h.call("GET", "/orgs/octo/teams/core", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(source, "state", "{read}");
        assert_eq!(read, team("octo", "Core"));
        assert_eq!(h.memory().await.len(), 1, "kept");
    }
    assert_eq!(h.calls(), 3, "two attempts");
    let (_, source, read) = h.call("GET", "/orgs/octo/teams/core", None).await;
    assert_eq!(source, "state");
    assert_eq!(read, team("octo", "Core"));
    assert_eq!(h.calls(), 3, "none after the cap");
    assert_eq!(h.memory().await.len(), 1, "kept");
}

async fn a_capture_and_its_refund(h: &Harness) {
    h.call("POST", "/captures", Some(json!({"amount": 100})))
        .await;
    h.call("POST", "/captures/cap1/refunds", None).await;
    let (_, source, _) = h.call("GET", "/captures/cap1", None).await;
    assert_eq!(source, "llm", "the create told the capture");
    assert!(h.memory().await.is_empty());
}

/// Updating or deleting a child costs its parent nothing.
#[tokio::test]
async fn a_nested_put_or_delete_does_not_tell_the_parent() {
    let capture = json!({"id": "cap1", "amount": 100, "status": "COMPLETED"});
    let h = things(vec![
        capture.clone(),
        json!({"id": "ref1", "amount": 100}),
        capture.clone(),
        json!({"id": "ref1", "amount": 50}),
    ]);
    a_capture_and_its_refund(&h).await;
    let before = h.calls();
    h.call(
        "PUT",
        "/captures/cap1/refunds/ref1",
        Some(json!({"amount": 50})),
    )
    .await;
    let (_, source, _) = h.call("GET", "/captures/cap1", None).await;
    assert_eq!(source, "state", "a PUT with its own answer");
    h.call("DELETE", "/captures/cap1/refunds/ref1", None).await;
    let (_, source, _) = h.call("GET", "/captures/cap1", None).await;
    assert_eq!(source, "state", "a DELETE");
    assert_eq!(h.calls(), before + 1, "only the PUT's own generation");
}

/// A body-less action on a child does tell its parent: nothing else will.
#[tokio::test]
async fn a_nested_body_less_action_tells_the_parent() {
    let capture = json!({"id": "cap1", "amount": 100, "status": "COMPLETED"});
    let h = things(vec![
        capture.clone(),
        json!({"id": "ref1", "amount": 100}),
        capture.clone(),
        capture.clone(),
    ]);
    a_capture_and_its_refund(&h).await;
    h.call("POST", "/captures/cap1/refunds/ref1/approve", None)
        .await;
    let (_, source, _) = h.call("GET", "/captures/cap1", None).await;
    assert_eq!(source, "llm");
    let last = h.calls() - 1;
    assert!(block(&h.prompt(last)).unwrap().contains("approveRefund"));
}

/// An update answering less than its read does not replace its target's story:
/// earlier requests stay, and so does a request its answer does not hold.
#[tokio::test]
async fn an_update_answering_short_is_treated_like_a_create() {
    let h = things(vec![
        json!({"id": "w1", "colour": "red"}),
        json!({"id": "w1"}),
        json!({"id": "w1", "colour": "blue"}),
    ]);
    h.call("GET", "/widgets/w1", None).await;
    h.call("POST", "/widgets/w1/poke", None).await;
    assert_eq!(h.memory().await.len(), 1);
    h.call("PUT", "/widgets/w1", Some(json!({"colour": "blue"})))
        .await;
    let memory = h.memory().await;
    let requests = memory[0].data["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2, "kept, and added to: {memory:?}");
    assert_eq!(requests[0]["operation"], "pokeWidget");
    assert_eq!(requests[1]["body"], json!({"colour": "blue"}));
}

/// Two runs of one journey send the same block: a snapshot of the first replays
/// for the second. `<now>` aside, which a replayer already normalises.
#[tokio::test]
async fn a_recorded_prompt_replays() {
    let run = || async {
        let h = jira(vec![
            created_issue(),
            issue_bean("Pump fails", "To Do", &[]),
            issue_bean("Pump fails", "Done", &[]),
        ]);
        h.call("POST", "/rest/api/3/issue", Some(create_issue_request()))
            .await;
        h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
        h.call(
            "POST",
            "/rest/api/3/issue/PLNT-1/transitions",
            Some(json!({"transition": {"id": "31"}})),
        )
        .await;
        h.call("GET", "/rest/api/3/issue/PLNT-1", None).await;
        let prompts: Vec<String> = (1..3).map(|n| h.prompt(n)).collect();
        prompts
    };
    let first = run().await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let second = run().await;
    for (one, two) in first.iter().zip(&second) {
        assert_eq!(block(one), block(two));
        assert!(!block(one).unwrap().contains("\"at\""), "{one}");
    }
}

/// A remembered 204 is a change of its resource: replicas drop cached reads of it
/// and feeds and triggers hear of it, at write time.
#[tokio::test]
async fn a_remembered_or_merged_write_publishes_a_change() {
    let h = things(vec![
        json!({"id": "i1", "title": "t", "status": "open"}),
        pet("old"),
    ]);
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/pets", Some(json!({"name": "old"}))).await;
    let mut changes = h.store.subscribe();
    h.call("POST", "/items/i1/close", None).await;
    let change = changes.try_recv().expect("the remembered write");
    assert_eq!(
        (change.resource_type.as_str(), change.id.as_str()),
        ("items", "i1")
    );
    assert_eq!(change.kind, ChangeKind::Upserted);
    assert!(changes.try_recv().is_err(), "once");

    h.call("PUT", "/pets/p1", Some(json!({"name": "new"})))
        .await;
    let change = changes.try_recv().expect("the merged write");
    assert_eq!(change.resource_type, "pets");
    assert!(changes.try_recv().is_err(), "once");
}

/// A path value is no key and no text of the memory: it may be the secret.
#[tokio::test]
async fn a_path_value_is_not_stored() {
    let h = things(vec![]);
    h.call("POST", "/tokens/sk_live_abc123/revoke", None).await;
    let memory = h.memory().await;
    assert_eq!(memory.len(), 1, "{memory:?}");
    let all = format!("{memory:?}");
    assert!(!all.contains("sk_live_abc123"), "{all}");
    assert!(all.contains("/tokens/{token_value}/revoke"), "{all}");
}

/// What the body says is a credential goes, however it says so; what only looks
/// like one stays.
#[tokio::test]
async fn credentials_go_however_they_are_named() {
    let h = things(vec![]);
    h.call(
        "POST",
        "/tokens/t1/revoke",
        Some(json!({
            "auth": "kept-out-1",
            "private_key": "kept-out-2",
            "headers": [{"name": "Authorization", "value": "Bearer kept-out-3"}],
            "passphrase": "kept-out-4",
            "recovery": "kept-out-5",
            "PRIVATEKEY": "kept-out-6",
            "clientCredentials": "kept-out-7",
            "sessionId": "kept-out-8",
            "ssh_key": "kept-out-9",
            "signingKey": "kept-out-10",
            "cvv": "kept-out-11",
            "bearer": "kept-out-12",
            "user_pin": "kept-out-13",
            "max_tokens": 100,
            "pinned": "kept-in-3",
            "author": "kept-in-1",
            "note": "kept-in-2",
        })),
    )
    .await;
    let stored = h.memory().await[0].data.to_string();
    assert!(!stored.contains("kept-out"), "{stored}");
    assert!(stored.contains("kept-in-1"), "{stored}");
    assert!(stored.contains("kept-in-2"), "{stored}");
    assert!(stored.contains("kept-in-3"), "{stored}");
    assert!(stored.contains("\"max_tokens\":100"), "{stored}");
    assert!(stored.contains("Authorization"), "{stored}");
}

/// A read naming a seed is told, but is not forced and settles nothing.
#[tokio::test]
async fn a_seed_read_neither_forces_nor_settles() {
    let h = things(vec![pet("Rex")]);
    h.call("POST", "/pets", Some(json!({"name": "Rex"}))).await;
    h.call("POST", "/pets/p1/birthday", None).await;
    let req = Request::get("/pets/p1")
        .header("x-mockingbird-seed", "7")
        .body(Body::empty())
        .unwrap();
    let resp = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.headers()["x-mockingbird-source"], "state");
    assert_eq!(h.calls(), 1);
    assert_eq!(h.memory().await.len(), 1, "still to settle");
}

const SCENARIO_NAME: &str = "busy";

/// A read through a scenario generates and is told, but settles nothing.
#[tokio::test]
async fn a_scenario_read_is_told_and_settles_nothing() {
    let h = harness_with(
        THINGS.as_bytes(),
        vec![Ok(pet("Rex").to_string()), Ok(pet("Rex").to_string())],
        |rt| {
            rt.scenarios.insert(
                SCENARIO_NAME.to_string(),
                mb_protocols::ScenarioRuntime {
                    spec: mb_engine::ScenarioSpec {
                        name: SCENARIO_NAME.to_string(),
                        instructions: Some("Be terse.".to_string()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
        },
    );
    h.call("POST", "/pets", Some(json!({"name": "Rex"}))).await;
    h.call("POST", "/pets/p1/birthday", None).await;
    let req = Request::get("/pets/p1")
        .header("x-mockingbird-scenario", SCENARIO_NAME)
        .body(Body::empty())
        .unwrap();
    h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(h.calls(), 2);
    assert!(block(&h.prompt(1)).unwrap().contains("petBirthday"));
    assert_eq!(h.memory().await.len(), 1, "still to settle");
}

/// A list after a 204 is shown what the store holds, is not told, and leaves the
/// request for the resource's own read.
#[tokio::test]
async fn a_list_after_a_204_is_not_told() {
    let h = things(vec![
        json!({"id": "i1", "title": "t", "status": "open"}),
        json!([{"id": "i1", "title": "t", "status": "open"}]),
    ]);
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;
    let (status, _, _) = h.call("GET", "/items", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(block(&h.prompt(1)).is_none(), "{}", h.prompt(1));
    assert_eq!(h.memory().await.len(), 1);
}

/// A contract cannot name the reserved type.
#[test]
fn a_contract_naming_the_reserved_type_is_warned_of() {
    let contract = format!(
        r#"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /x/{{id}}:
    get:
      operationId: getX
      x-mockingbird-resource: {{type: "{}"}}
      parameters: [{{name: id, in: path, required: true, schema: {{type: string}}}}]
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{id: {{type: string}}}}}}}}}}}}
"#,
        mb_state::INTERNAL_TYPE
    );
    let (bundle, warnings) = mb_import::compile_with_warnings(contract.as_bytes()).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.code == "invalid_resource_extension"),
        "{warnings:?}"
    );
    assert_ne!(
        bundle.operations[0].resource_type.as_deref(),
        Some(mb_state::INTERNAL_TYPE)
    );
}

/// A read with a query catches up like a plain one, since its schema is the
/// whole resource: it settles, and later reads of any query come from state.
#[tokio::test]
async fn a_read_with_a_query_settles_the_memory() {
    let h = things(vec![
        json!({"id": "i1", "title": "t", "status": "open"}),
        json!({"id": "i1", "title": "t", "status": "closed"}),
    ]);
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;

    let (_, source, read) = h.call("GET", "/items/i1?view=short", None).await;
    assert_eq!(source, "llm");
    assert_eq!(read["status"], "closed");
    assert!(block(&h.prompt(1)).is_some());
    assert!(h.memory().await.is_empty(), "settled");

    for uri in ["/items/i1?view=short", "/items/i1?view=long", "/items/i1"] {
        let (_, source, read) = h.call("GET", uri, None).await;
        assert_eq!(source, "state", "{uri}");
        assert_eq!(read["status"], "closed");
    }
    assert_eq!(h.calls(), 2);
}

/// Sessions do not share a memory.
#[tokio::test]
async fn another_session_is_told_nothing() {
    let h = things(vec![
        json!({"id": "i1", "title": "t", "status": "open"}),
        json!({"id": "i1", "title": "t", "status": "open"}),
    ]);
    h.call("GET", "/items/i1", None).await;
    h.call("POST", "/items/i1/close", None).await;
    let other = Request::get("/items/i1")
        .header("x-mockingbird-session", "other")
        .body(Body::empty())
        .unwrap();
    h.router.clone().oneshot(other).await.unwrap();
    assert!(block(&h.prompt(1)).is_none(), "{}", h.prompt(1));
}

fn team(org: &str, name: &str) -> Value {
    json!({
        "id": "t1",
        "slug": "core",
        "name": name,
        "url": format!("https://api.example.com/orgs/{org}/teams/core"),
    })
}

/// `octo`'s team `core`, through a create at that route so the session knows
/// whose it is.
async fn octos_team(h: &Harness) {
    let (status, _, _) = h
        .call("POST", "/orgs/octo/teams", Some(json!({"name": "Core"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);
}

/// A write refused for another owner's key remembers nothing, though the same
/// write by the owner does.
#[tokio::test]
async fn a_write_for_another_owners_key_remembers_nothing() {
    let h = things(vec![team("octo", "Core")]);
    octos_team(&h).await;
    // The key holds octo's team, so a write through acme's route is refused it.
    h.store
        .upsert(
            &session(),
            StoredResource {
                resource_type: "teams".into(),
                id: "core".into(),
                data: team("octo", "Core"),
            },
        )
        .await
        .unwrap();
    // The org the path names is held: only the refusal keeps it untold.
    h.store
        .upsert(
            &session(),
            StoredResource {
                resource_type: "orgs".into(),
                id: "acme".into(),
                data: json!({"id": "acme", "login": "acme"}),
            },
        )
        .await
        .unwrap();
    let (status, _, _) = h
        .call(
            "POST",
            "/orgs/acme/teams/core/archive",
            Some(json!({"reason": "not theirs"})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);

    let (status, _, _) = h
        .call(
            "POST",
            "/orgs/octo/teams/core/archive",
            Some(json!({"reason": "theirs"})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        h.memory().await.iter().any(|m| m.id.contains("team")),
        "the owner's write is remembered: {:?}",
        h.memory().await
    );
}

/// A create under a parent remembers only a parent the session holds for the
/// owner the path names.
#[tokio::test]
async fn a_create_under_another_owners_parent_remembers_nothing() {
    let h = things(vec![
        team("octo", "Core"),
        json!({"id": "m1", "login": "ann"}),
        json!({"id": "m2", "login": "bob"}),
    ]);
    octos_team(&h).await;
    h.call(
        "POST",
        "/orgs/acme/teams/core/members",
        Some(json!({"login": "ann"})),
    )
    .await;
    assert!(h.memory().await.is_empty(), "{:?}", h.memory().await);

    h.call(
        "POST",
        "/orgs/octo/teams/core/members",
        Some(json!({"login": "bob"})),
    )
    .await;
    let memory = h.memory().await;
    assert_eq!(memory.len(), 1, "{memory:?}");
    assert!(memory[0].data.to_string().contains("addMember"));
}
