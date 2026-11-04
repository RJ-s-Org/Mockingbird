//! A create that repeats a record of its own type already stored: a second contact
//! with a taken email, a folder with a taken name in the same parent. The records this
//! runtime stored are indexed by their member values, and a create whose operation
//! declares a refusal status is shown the stored records of the type it creates that
//! hold a value of its request under the same member, and is offered the declared
//! statuses. Driven over HTTP with a scripted LLM, asserting the prompts it was sent
//! and what the store holds.
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

/// Contacts whose create declares a 409 for a taken email, and folders whose create
/// declares one for a name taken in the same parent.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Directory, version: "1"}
paths:
  /contacts:
    post:
      operationId: createContact
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}, email: {type: string}, status: {type: string}, org_id: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}
        "409": {description: A contact with this email already exists., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /contacts/{contact_id}:
    get:
      operationId: getContact
      parameters: [{name: contact_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}
  /folders:
    post:
      operationId: createFolder
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}, note: {type: string}, parent: {type: object, properties: {id: {type: string}}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
        "409": {description: An item with the same name already exists in the parent., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /folders/{folder_id}:
    get:
      operationId: getFolder
      parameters: [{name: folder_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
  /tasks:
    post:
      operationId: createTask
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}, category_id: {type: string}, assignee: {type: object, properties: {id: {type: string}}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
        "422": {description: Validation failed., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /tasks/{task_id}:
    get:
      operationId: getTask
      parameters: [{name: task_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
  /labels:
    post:
      operationId: createLabel
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {displayTitle: {type: string}, board_id: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Label"}}}}
        "409": {description: A label with this display title is already on the board., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /labels/{label_id}:
    get:
      operationId: getLabel
      parameters: [{name: label_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Label"}}}}
  /groups/{group}/contacts:
    post:
      operationId: addGroupContact
      parameters: [{name: group, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}, email: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}
components:
  schemas:
    Contact:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        email: {type: string}
        status: {type: string}
        org_id: {type: string}
    Folder:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        note: {type: string}
        parent: {type: object, properties: {id: {type: string}}}
    Task:
      type: object
      required: [id, title]
      properties:
        id: {type: string}
        title: {type: string}
        category_id: {type: string}
        assignee: {type: object, properties: {id: {type: string}}}
    Label:
      type: object
      required: [id, displayTitle]
      properties: {id: {type: string}, displayTitle: {type: string}, board_id: {type: string}}
    Error:
      type: object
      required: [name, message]
      properties: {name: {type: string}, message: {type: string}}
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

/// A harness over the in-memory store, as a single node serves it.
fn harness(contract: &str, script: Vec<Value>) -> Harness {
    harness_on(
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
        contract,
        script,
    )
}

/// A harness over `store`.
fn harness_on(store: Arc<dyn StateStore>, contract: &str, script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
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
    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.post_with(uri, body, &[]).await
    }

    async fn post_with(
        &self,
        uri: &str,
        body: Value,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let req = req.body(Body::from(body.to_string())).unwrap();
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

    /// The user message of the `n`th LLM call.
    fn prompt(&self, n: usize) -> String {
        let requests = self.provider.requests();
        requests[n].messages.last().unwrap().content.clone()
    }

    /// The schema the `n`th LLM call asked for.
    fn format_schema(&self, n: usize) -> Value {
        match &self.provider.requests()[n].response_format {
            mb_llm::ResponseFormat::JsonSchema { schema, .. } => schema.clone(),
            other => panic!("{other:?}"),
        }
    }

    async fn exported(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .store
            .export(&session())
            .await
            .unwrap()
            .into_iter()
            .map(|r| format!("{}/{}", r.resource_type, r.id))
            .collect();
        ids.sort();
        ids
    }
}

/// The text between a block's tags, or `None` without one.
fn block<'a>(prompt: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = prompt.find(&open)? + open.len();
    let end = prompt.find(&format!("</{tag}>"))?;
    Some(&prompt[start..end])
}

fn contact(id: &str, name: &str, email: &str) -> Value {
    json!({"id": id, "name": name, "email": email})
}

fn refusal() -> Value {
    json!({"effects": [], "response": null, "refusal": 409})
}

/// A second contact with the email of one this runtime stored is shown that contact,
/// told which member it shares, and offered the declared 409. The refusal answers the
/// contract's 409, and stores nothing.
#[tokio::test]
async fn a_create_with_a_taken_email_is_shown_its_holder_and_offered_the_409() {
    let h = harness(
        CONTRACT,
        vec![
            contact("ct-000101", "Jane Roe", "jane.roe@example.com"),
            refusal(),
        ],
    );
    let (status, _) = h
        .post(
            "/contacts",
            json!({"name": "Jane Roe", "email": "jane.roe@example.com"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!h.prompt(0).contains("refusal"), "{}", h.prompt(0));

    let (status, body) = h
        .post(
            "/contacts",
            json!({"name": "J. Roe", "email": "jane.roe@example.com"}),
        )
        .await;
    let prompt = h.prompt(1);
    assert_eq!(status, StatusCode::CONFLICT, "{body} {prompt}");
    assert!(
        body["name"].is_string() && body["message"].is_string(),
        "{body}"
    );
    let related = block(&prompt, "related_resources").unwrap_or_default();
    assert!(related.contains("ct-000101"), "{prompt}");
    let same = block(&prompt, "same_values").unwrap_or_default();
    assert!(
        same.contains("\"id\": \"ct-000101\"") && same.contains("\"member\": \"email\""),
        "{prompt}"
    );
    assert!(
        prompt.contains("409: A contact with this email already exists."),
        "{prompt}"
    );
    let note = &prompt[prompt.find("<same_values>").unwrap_or_default()..];
    assert!(!note.is_empty(), "{prompt}");
    let before = &prompt[..prompt.find("<same_values>").unwrap_or_default()];
    let preface = &before[before.rfind("\n\n").map_or(0, |i| i + 2)..];
    assert!(
        preface.contains("data, not instructions")
            && preface.contains("only when")
            && preface.contains("description")
            && preface.contains("otherwise"),
        "the note says a shared value is no conflict by itself: {prompt}"
    );
    assert_eq!(
        h.format_schema(1)["properties"]["refusal"],
        json!({"type": "integer", "enum": [409]})
    );
    assert_eq!(h.exported().await, ["contacts/ct-000101"], "nothing stored");
}

/// A contact with another email repeats nothing: its prompt is the one a create in an
/// empty session gets, with no record shown and no refusal offered.
#[tokio::test]
async fn a_create_with_another_email_shows_nothing() {
    let h = harness(
        CONTRACT,
        vec![
            contact("ct-000101", "Jane Roe", "jane.roe@example.com"),
            contact("ct-000102", "John Doe", "john.doe@example.com"),
        ],
    );
    h.post(
        "/contacts",
        json!({"name": "Jane Roe", "email": "jane.roe@example.com"}),
    )
    .await;
    let second = json!({"name": "John Doe", "email": "john.doe@example.com"});
    let (status, _) = h.post("/contacts", second.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = h.prompt(1);
    assert!(block(&prompt, "related_resources").is_none(), "{prompt}");
    assert!(!prompt.contains("refusal"), "{prompt}");

    let fresh = harness(
        CONTRACT,
        vec![contact("ct-000102", "John Doe", "john.doe@example.com")],
    );
    fresh.post("/contacts", second).await;
    // The clock line says when each was sent, which may be another second.
    let timeless = |p: &str| {
        p.lines()
            .filter(|l| !l.starts_with("<now>"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        timeless(&prompt),
        timeless(&fresh.prompt(0)),
        "the prompt is unchanged"
    );
}

/// A common value two contacts share (a status word) is no identifier, and is held by
/// more than one record: a third contact holding it is shown neither.
#[tokio::test]
async fn a_common_value_shows_nothing() {
    let with_status = |id: &str, name: &str, email: &str| {
        let mut c = contact(id, name, email);
        c["status"] = json!("active");
        c
    };
    let h = harness(
        CONTRACT,
        vec![
            with_status("ct-000101", "Ann Lee", "ann@example.com"),
            with_status("ct-000102", "Bob Ray", "bob@example.com"),
            with_status("ct-000103", "Cy Fox", "cy@example.com"),
        ],
    );
    for (name, email) in [
        ("Ann Lee", "ann@example.com"),
        ("Bob Ray", "bob@example.com"),
        ("Cy Fox", "cy@example.com"),
    ] {
        let (status, _) = h
            .post(
                "/contacts",
                json!({"name": name, "email": email, "status": "active"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    for n in 1..3 {
        let prompt = h.prompt(n);
        assert!(block(&prompt, "related_resources").is_none(), "{prompt}");
        assert!(!prompt.contains("refusal"), "{prompt}");
    }
}

/// A create whose operation declares no refusal status is shown nothing of its type,
/// though its email is taken: its prompt is as it was.
#[tokio::test]
async fn no_declared_conflict_offers_no_refusal() {
    let contract = CONTRACT.replace(
        "        \"409\": {description: A contact with this email already exists., content: {application/json: {schema: {$ref: \"#/components/schemas/Error\"}}}}\n",
        "",
    );
    assert_ne!(contract, CONTRACT);
    let h = harness(
        &contract,
        vec![
            contact("ct-000101", "Jane Roe", "jane.roe@example.com"),
            contact("ct-000102", "J. Roe", "jane.roe@example.com"),
        ],
    );
    let body = json!({"name": "Jane Roe", "email": "jane.roe@example.com"});
    h.post("/contacts", body.clone()).await;
    let (status, _) = h.post("/contacts", body).await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = h.prompt(1);
    assert!(block(&prompt, "related_resources").is_none(), "{prompt}");
    assert!(!prompt.contains("refusal"), "{prompt}");
}

fn folder(id: &str, parent: &str) -> Value {
    json!({"id": id, "name": "Quarterly Reports", "parent": {"id": parent}})
}

/// A plain name (no identifier) counts when the stored folder holding it agrees with
/// everything else the request sets: the same name in the same parent is shown and
/// offered the 409, the same name in another parent is not.
#[tokio::test]
async fn a_taken_name_counts_only_in_the_same_parent() {
    let h = harness(CONTRACT, vec![folder("fd-000001", "0"), refusal()]);
    let body = json!({"name": "Quarterly Reports", "parent": {"id": "0"}});
    h.post("/folders", body.clone()).await;
    let (status, out) = h.post("/folders", body).await;
    let prompt = h.prompt(1);
    assert_eq!(status, StatusCode::CONFLICT, "{out} {prompt}");
    let same = block(&prompt, "same_values").unwrap_or_default();
    assert!(
        same.contains("fd-000001") && same.contains("\"member\": \"name\""),
        "{prompt}"
    );
    assert_eq!(h.exported().await, ["folders/fd-000001"], "nothing stored");

    let h = harness(
        CONTRACT,
        vec![folder("fd-000001", "0"), folder("fd-000002", "77")],
    );
    h.post(
        "/folders",
        json!({"name": "Quarterly Reports", "parent": {"id": "0"}}),
    )
    .await;
    let (status, _) = h
        .post(
            "/folders",
            json!({"name": "Quarterly Reports", "parent": {"id": "77"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = h.prompt(1);
    assert!(!prompt.contains("fd-000001"), "{prompt}");
    assert!(!prompt.contains("refusal"), "{prompt}");
}

/// A same-email create of a contact under another org, by its body's reference, is no
/// conflict: the referenced parent differs, so the stored contact is not shown.
#[tokio::test]
async fn a_body_reference_to_another_parent_shows_nothing() {
    let mut stored = contact("ct-000101", "Jane Roe", "jane.roe@example.com");
    stored["org_id"] = json!("org-000001");
    let mut second = contact("ct-000102", "Jane Roe", "jane.roe@example.com");
    second["org_id"] = json!("org-000002");
    let h = harness(CONTRACT, vec![stored, second]);
    h.post(
        "/contacts",
        json!({"name": "Jane Roe", "email": "jane.roe@example.com", "org_id": "org-000001"}),
    )
    .await;
    let (status, _) = h
        .post(
            "/contacts",
            json!({"name": "Jane Roe", "email": "jane.roe@example.com", "org_id": "org-000002"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = h.prompt(1);
    assert!(!prompt.contains("ct-000101"), "{prompt}");
    assert!(!prompt.contains("refusal"), "{prompt}");
}

/// A plain value alone, or beside members that are no reference, is no evidence of a
/// conflict: a folder of the same name and note, with no parent, is shown nothing.
#[tokio::test]
async fn a_lone_plain_value_is_no_conflict() {
    for body in [
        json!({"name": "Quarterly Reports"}),
        json!({"name": "Quarterly Reports", "note": "numbers for the board"}),
    ] {
        let mut held = body.clone();
        held["id"] = json!("fd-000001");
        let mut again = body.clone();
        again["id"] = json!("fd-000002");
        let h = harness(CONTRACT, vec![held, again]);
        h.post("/folders", body.clone()).await;
        let (status, _) = h.post("/folders", body.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let prompt = h.prompt(1);
        assert!(block(&prompt, "related_resources").is_none(), "{prompt}");
        assert!(!prompt.contains("same_values"), "{prompt}");
        assert!(!prompt.contains("refusal"), "{prompt}");
    }
}

/// Members of an org and tasks of a project, created under the parent their path
/// names, each declaring a conflict.
const NESTED: &str = r##"
openapi: 3.0.0
info: {title: Nested, version: "1"}
paths:
  /orgs/{org}/members:
    post:
      operationId: addMember
      parameters: [{name: org, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {email: {type: string}}}}}
      responses:
        "201": {description: added, content: {application/json: {schema: {$ref: "#/components/schemas/Member"}}}}
        "409": {description: The email is already a member., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /members/{member_id}:
    get:
      operationId: getMember
      parameters: [{name: member_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Member"}}}}
  /projects/{project}/tasks:
    post:
      operationId: addTask
      parameters: [{name: project, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "201": {description: added, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
        "422": {description: The task is not valid., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /tasks/{task_id}:
    get:
      operationId: getTask
      parameters: [{name: task_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
components:
  schemas:
    Member:
      type: object
      required: [id, email]
      properties: {id: {type: string}, email: {type: string}}
    Task:
      type: object
      required: [id, title]
      properties: {id: {type: string}, title: {type: string}}
    Error:
      type: object
      required: [name, message]
      properties: {name: {type: string}, message: {type: string}}
"##;

/// The same email under another org, or the same title under another project, is in
/// another collection: neither is shown nor offered a refusal.
#[tokio::test]
async fn a_hit_under_another_path_parent_shows_nothing() {
    let h = harness(
        NESTED,
        vec![
            json!({"id": "mb-000001", "email": "jane.roe@example.com"}),
            json!({"id": "mb-000002", "email": "jane.roe@example.com"}),
        ],
    );
    let body = json!({"email": "jane.roe@example.com"});
    h.post("/orgs/acme/members", body.clone()).await;
    let (status, _) = h.post("/orgs/globex/members", body).await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = h.prompt(1);
    assert!(!prompt.contains("mb-000001"), "{prompt}");
    assert!(!prompt.contains("refusal"), "{prompt}");

    // In the same org, the member is a conflict.
    let h = harness(
        NESTED,
        vec![
            json!({"id": "mb-000001", "email": "jane.roe@example.com"}),
            refusal(),
        ],
    );
    let body = json!({"email": "jane.roe@example.com"});
    h.post("/orgs/acme/members", body.clone()).await;
    let (status, _) = h.post("/orgs/acme/members", body).await;
    let prompt = h.prompt(1);
    assert_eq!(status, StatusCode::CONFLICT, "{prompt}");
    let same = block(&prompt, "same_values").unwrap_or_default();
    assert!(same.contains("mb-000001"), "{prompt}");

    let h = harness(
        NESTED,
        vec![
            json!({"id": "tk-000001", "title": "Buy milk"}),
            json!({"id": "tk-000002", "title": "Buy milk"}),
        ],
    );
    let body = json!({"title": "Buy milk"});
    h.post("/projects/A/tasks", body.clone()).await;
    let (status, _) = h.post("/projects/B/tasks", body).await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = h.prompt(1);
    assert!(!prompt.contains("tk-000001"), "{prompt}");
    assert!(!prompt.contains("refusal"), "{prompt}");
}

/// A create carrying a declared idempotency key may be a retry of the one stored, which
/// the API answers again rather than refuses: it is shown nothing of its type. A header
/// known by its description counts as one named so does.
#[tokio::test]
async fn an_idempotent_retry_shows_nothing() {
    for header in [
        "        - {name: Idempotency-Key, in: header, schema: {type: string}}\n",
        "        - {name: X-Request-Ref, in: header, description: A unique idempotency key for safely retrying the request., schema: {type: string}}\n",
    ] {
        let name = if header.contains("Idempotency-Key") {
            "Idempotency-Key"
        } else {
            "X-Request-Ref"
        };
        let contract = CONTRACT.replacen(
            "      operationId: createContact\n",
            &format!("      operationId: createContact\n      parameters:\n{header}"),
            1,
        );
        assert_ne!(contract, CONTRACT);
        let h = harness(
            &contract,
            vec![
                contact("ct-000101", "Jane Roe", "jane.roe@example.com"),
                contact("ct-000102", "Jane Roe", "jane.roe@example.com"),
                refusal(),
            ],
        );
        let body = json!({"name": "Jane Roe", "email": "jane.roe@example.com"});
        h.post_with("/contacts", body.clone(), &[(name, "key-000001")])
            .await;
        let (status, _) = h
            .post_with("/contacts", body.clone(), &[(name, "key-000001")])
            .await;
        assert_eq!(status, StatusCode::CREATED, "{name}");
        let prompt = h.prompt(1);
        assert!(!prompt.contains("refusal"), "{name}: {prompt}");
        // Without the key, the same create is offered the refusal.
        let (status, _) = h.post("/contacts", body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{name}: {}", h.prompt(2));
    }
}

/// A create whose answer has a member named as the refusal is offered none, so it is
/// shown nothing of its type either: the two gates stay in step.
#[tokio::test]
async fn a_create_never_offered_a_refusal_is_shown_nothing() {
    let contract = CONTRACT.replacen(
        "        status: {type: string}\n        org_id: {type: string}\n",
        "        status: {type: string}\n        org_id: {type: string}\n        refusal: {type: string}\n",
        1,
    );
    assert_ne!(contract, CONTRACT);
    let h = harness(
        &contract,
        vec![
            contact("ct-000101", "Jane Roe", "jane.roe@example.com"),
            contact("ct-000102", "J. Roe", "jane.roe@example.com"),
        ],
    );
    let body = json!({"name": "Jane Roe", "email": "jane.roe@example.com"});
    h.post("/contacts", body.clone()).await;
    let (status, _) = h.post("/contacts", body).await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = h.prompt(1);
    assert!(block(&prompt, "related_resources").is_none(), "{prompt}");
}

/// A plain title beside an agreeing reference is still no conflict when no refusal the
/// create declares names the title: "Validation failed." says nothing of duplicates.
#[tokio::test]
async fn a_plain_match_needs_a_refusal_naming_its_member() {
    for body in [
        json!({"title": "Buy milk", "category_id": "groceries"}),
        json!({"title": "Buy milk", "assignee": {"id": "usr-000001"}}),
    ] {
        let mut held = body.clone();
        held["id"] = json!("tk-000001");
        let mut again = body.clone();
        again["id"] = json!("tk-000002");
        let h = harness(CONTRACT, vec![held, again]);
        h.post("/tasks", body.clone()).await;
        let (status, _) = h.post("/tasks", body.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let prompt = h.prompt(1);
        assert!(!prompt.contains("tk-000001"), "{prompt}");
        assert!(!prompt.contains("refusal"), "{prompt}");
    }
}

/// A 409 whose description names the member, by its words (`displayTitle` as "display
/// title"), with an agreeing parent, makes a plain match count.
#[tokio::test]
async fn a_plain_match_counts_when_a_refusal_names_its_member() {
    let label = json!({"id": "lb-000001", "displayTitle": "Urgent work", "board_id": "b1"});
    let h = harness(CONTRACT, vec![label, refusal()]);
    let body = json!({"displayTitle": "Urgent work", "board_id": "b1"});
    h.post("/labels", body.clone()).await;
    let (status, _) = h.post("/labels", body).await;
    let prompt = h.prompt(1);
    assert_eq!(status, StatusCode::CONFLICT, "{prompt}");
    let same = block(&prompt, "same_values").unwrap_or_default();
    assert!(same.contains("\"member\": \"displayTitle\""), "{prompt}");
}

/// A top-level create is no repeat of a record of its type created under a parent
/// path through another route: that one belongs to its own collection.
#[tokio::test]
async fn a_top_level_create_skips_records_of_a_parent_path() {
    let h = harness(
        CONTRACT,
        vec![
            contact("ct-000101", "Jane Roe", "jane.roe@example.com"),
            contact("ct-000102", "Jane Roe", "jane.roe@example.com"),
        ],
    );
    let body = json!({"name": "Jane Roe", "email": "jane.roe@example.com"});
    h.post("/groups/g1/contacts", body.clone()).await;
    let (status, _) = h.post("/contacts", body).await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = h.prompt(1);
    assert!(!prompt.contains("ct-000101"), "{prompt}");
    assert!(!prompt.contains("refusal"), "{prompt}");
}

/// The SQL stores a create's duplicates are found on: the default install's private
/// in-memory SQLite database, and a SQLite file (as the benchmark runs), which another
/// process may write to. The file's directory is kept alive with the store.
async fn sql_stores() -> Vec<(&'static str, Arc<dyn StateStore>, Option<tempfile::TempDir>)> {
    let ttl = Duration::from_secs(3600);
    let memory = mb_state::connect("sqlite::memory:", ttl).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("state.db").display());
    let file = mb_state::connect(&url, ttl).await.unwrap();
    vec![
        ("sqlite::memory:", memory, None),
        ("sqlite file", file, Some(dir)),
    ]
}

/// Duplicates are found on a SQL store as on the in-memory one: a taken email (an
/// identifying value) and a taken folder name in the same parent (a plain one) are shown
/// and offered the 409, a taken name in another parent and a taken email under another
/// path parent are not.
#[tokio::test]
async fn a_sql_store_finds_what_a_create_repeats() {
    for (backend, store, _dir) in sql_stores().await {
        let h = harness_on(
            Arc::clone(&store),
            CONTRACT,
            vec![
                contact("ct-000101", "Jane Roe", "jane.roe@example.com"),
                refusal(),
                folder("fd-000001", "0"),
                refusal(),
                folder("fd-000002", "77"),
            ],
        );
        h.post(
            "/contacts",
            json!({"name": "Jane Roe", "email": "jane.roe@example.com"}),
        )
        .await;
        let (status, _) = h
            .post(
                "/contacts",
                json!({"name": "J. Roe", "email": "jane.roe@example.com"}),
            )
            .await;
        let prompt = h.prompt(1);
        assert_eq!(status, StatusCode::CONFLICT, "{backend}: {prompt}");
        let same = block(&prompt, "same_values").unwrap_or_default();
        assert!(
            same.contains("\"id\": \"ct-000101\"") && same.contains("\"member\": \"email\""),
            "{backend}: {prompt}"
        );

        let body = json!({"name": "Quarterly Reports", "parent": {"id": "0"}});
        h.post("/folders", body.clone()).await;
        let (status, _) = h.post("/folders", body).await;
        let prompt = h.prompt(3);
        assert_eq!(status, StatusCode::CONFLICT, "{backend}: {prompt}");
        let same = block(&prompt, "same_values").unwrap_or_default();
        assert!(
            same.contains("fd-000001") && same.contains("\"member\": \"name\""),
            "{backend}: {prompt}"
        );
        let (status, _) = h
            .post(
                "/folders",
                json!({"name": "Quarterly Reports", "parent": {"id": "77"}}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{backend}");
        let prompt = h.prompt(4);
        assert!(!prompt.contains("same_values"), "{backend}: {prompt}");
        assert!(!prompt.contains("refusal"), "{backend}: {prompt}");
    }

    for (backend, store, _dir) in sql_stores().await {
        let h = harness_on(
            store,
            NESTED,
            vec![
                json!({"id": "mb-000001", "email": "jane.roe@example.com"}),
                json!({"id": "mb-000002", "email": "jane.roe@example.com"}),
            ],
        );
        let body = json!({"email": "jane.roe@example.com"});
        h.post("/orgs/acme/members", body.clone()).await;
        let (status, _) = h.post("/orgs/globex/members", body).await;
        assert_eq!(status, StatusCode::CREATED, "{backend}");
        let prompt = h.prompt(1);
        assert!(!prompt.contains("mb-000001"), "{backend}: {prompt}");
        assert!(!prompt.contains("refusal"), "{backend}: {prompt}");
    }
}

fn stored(t: &str, data: Value) -> mb_state::StoredResource {
    mb_state::StoredResource {
        resource_type: t.into(),
        id: data["id"].as_str().unwrap_or_default().into(),
        data,
    }
}

/// [`CONTRACT`] with no route creating contacts under a parent path: every contact is
/// then in no collection, wherever it came from.
fn flat_contract() -> String {
    let start = CONTRACT.find("  /groups/{group}/contacts:").unwrap();
    let end = CONTRACT.find("components:").unwrap();
    let flat = format!("{}{}", &CONTRACT[..start], &CONTRACT[end..]);
    assert!(!flat.contains("/groups/"));
    flat
}

/// On a store another process writes to, the record that process stored is what the
/// create repeats: it is found by asking the store, not this runtime's index. A plain
/// name two records hold is still no match.
#[tokio::test]
async fn a_shared_store_finds_a_record_another_node_stored() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("state.db").display());
    let ttl = Duration::from_secs(3600);
    let ours = mb_state::connect(&url, ttl).await.unwrap();
    let theirs = mb_state::connect(&url, ttl).await.unwrap();
    assert!(!ours.local_only());
    let held = contact("ct-000901", "Jane Roe", "jane.roe@example.com");
    theirs
        .upsert(&session(), stored("contacts", held))
        .await
        .unwrap();
    let h = harness_on(ours, &flat_contract(), vec![refusal()]);
    let (status, _) = h
        .post(
            "/contacts",
            json!({"name": "Jane Roe", "email": "jane.roe@example.com"}),
        )
        .await;
    let prompt = h.prompt(0);
    assert_eq!(status, StatusCode::CONFLICT, "{prompt}");
    let same = block(&prompt, "same_values").unwrap_or_default();
    assert!(same.contains("ct-000901"), "{prompt}");

    for id in ["fd-000901", "fd-000902"] {
        theirs
            .upsert(&session(), stored("folders", folder(id, "0")))
            .await
            .unwrap();
    }
    let h = harness_on(
        mb_state::connect(&url, ttl).await.unwrap(),
        CONTRACT,
        vec![folder("fd-000903", "0")],
    );
    let (status, _) = h
        .post(
            "/folders",
            json!({"name": "Quarterly Reports", "parent": {"id": "0"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!h.prompt(0).contains("same_values"), "{}", h.prompt(0));
}

/// Two runtimes on one SQLite file: what runtime A created under a parent path
/// (`/groups/g1/contacts`) is in a collection runtime B cannot know, so B's top-level
/// create of the same contact is no repeat of it.
#[tokio::test]
async fn a_top_level_create_skips_another_runtime_s_record_of_a_parent_path() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("state.db").display());
    let ttl = Duration::from_secs(3600);
    let a = harness_on(
        mb_state::connect(&url, ttl).await.unwrap(),
        CONTRACT,
        vec![contact("ct-000101", "Jane Roe", "jane.roe@example.com")],
    );
    let b = harness_on(
        mb_state::connect(&url, ttl).await.unwrap(),
        CONTRACT,
        vec![
            contact("ct-000102", "Jane Roe", "jane.roe@example.com"),
            refusal(),
        ],
    );
    let body = json!({"name": "Jane Roe", "email": "jane.roe@example.com"});
    let (status, _) = a.post("/groups/g1/contacts", body.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = b.post("/contacts", body.clone()).await;
    let prompt = b.prompt(0);
    assert_eq!(status, StatusCode::CREATED, "{prompt}");
    assert!(!prompt.contains("ct-000101"), "{prompt}");
    assert!(!prompt.contains("same_values"), "{prompt}");

    // What B created at the top level, it knows is in no collection: a repeat counts.
    let (status, _) = b.post("/contacts", body).await;
    let prompt = b.prompt(1);
    assert_eq!(status, StatusCode::CONFLICT, "{prompt}");
    let same = block(&prompt, "same_values").unwrap_or_default();
    assert!(
        same.contains("ct-000102") && !same.contains("ct-000101"),
        "{prompt}"
    );
}

/// A record the Admin API seeded or upserted is existing data a create repeats, on
/// every store alike: a create with its email is shown it and offered the 409.
#[tokio::test]
async fn a_seeded_record_counts_on_every_store() {
    let mut stores = sql_stores().await;
    let memory: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    stores.push(("memory", memory, None));
    for (backend, store, _dir) in stores {
        let held = contact("ct-000901", "Jane Roe", "jane.roe@example.com");
        store
            .seed(&session(), vec![stored("contacts", held)])
            .await
            .unwrap();
        let h = harness_on(store, &flat_contract(), vec![refusal()]);
        let (status, _) = h
            .post(
                "/contacts",
                json!({"name": "Jane Roe", "email": "jane.roe@example.com"}),
            )
            .await;
        let prompt = h.prompt(0);
        assert_eq!(status, StatusCode::CONFLICT, "{backend}: {prompt}");
        let same = block(&prompt, "same_values").unwrap_or_default();
        assert!(same.contains("ct-000901"), "{backend}: {prompt}");
    }
}

/// A store whose listing fails when told to.
struct FailingList {
    inner: Arc<dyn StateStore>,
    fail: bool,
}

#[async_trait::async_trait]
impl StateStore for FailingList {
    async fn get(&self, s: &SessionKey, t: &str, id: &str) -> Result<Option<Stored>, Failed> {
        self.inner.get(s, t, id).await
    }
    async fn list(&self, s: &SessionKey, t: &str, limit: usize) -> Result<Vec<Stored>, Failed> {
        if self.fail {
            return Err(Failed::Backend("list failed".into()));
        }
        self.inner.list(s, t, limit).await
    }
    async fn upsert(&self, s: &SessionKey, r: Stored) -> Result<(), Failed> {
        self.inner.upsert(s, r).await
    }
    async fn delete(&self, s: &SessionKey, t: &str, id: &str) -> Result<bool, Failed> {
        self.inner.delete(s, t, id).await
    }
    async fn reset(&self, s: &SessionKey) -> Result<(), Failed> {
        self.inner.reset(s).await
    }
    async fn reset_mock(&self, w: &str, m: &str) -> Result<(), Failed> {
        self.inner.reset_mock(w, m).await
    }
    async fn touch(&self, s: &SessionKey) -> Result<(), Failed> {
        self.inner.touch(s).await
    }
    async fn sessions(&self, w: &str, m: &str) -> Result<Vec<mb_state::SessionInfo>, Failed> {
        self.inner.sessions(w, m).await
    }
    async fn export(&self, s: &SessionKey) -> Result<Vec<Stored>, Failed> {
        self.inner.export(s).await
    }
    async fn seed(&self, s: &SessionKey, r: Vec<Stored>) -> Result<(), Failed> {
        self.inner.seed(s, r).await
    }
    async fn ping(&self) -> Result<(), Failed> {
        self.inner.ping().await
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<mb_state::StateChanged> {
        self.inner.subscribe()
    }
}

type Stored = mb_state::StoredResource;
type Failed = mb_state::StateError;

/// A store holding a contact with a create's email: when its listing answers, the
/// create is refused; when it fails, what the create repeats is unknown, and it is shown
/// nothing and offered no refusal.
#[tokio::test]
async fn a_failed_listing_decides_nothing() {
    for fail in [false, true] {
        let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
        let held = contact("ct-000101", "Jane Roe", "jane.roe@example.com");
        store
            .upsert(&session(), stored("contacts", held))
            .await
            .unwrap();
        let h = harness_on(
            Arc::new(FailingList { inner: store, fail }),
            &flat_contract(),
            vec![if fail {
                contact("ct-000102", "Jane Roe", "jane.roe@example.com")
            } else {
                refusal()
            }],
        );
        let (status, _) = h
            .post(
                "/contacts",
                json!({"name": "Jane Roe", "email": "jane.roe@example.com"}),
            )
            .await;
        let prompt = h.prompt(0);
        if fail {
            assert_eq!(status, StatusCode::CREATED, "{prompt}");
            assert!(!prompt.contains("same_values"), "{prompt}");
            assert!(!prompt.contains("refusal"), "{prompt}");
        } else {
            assert_eq!(status, StatusCode::CONFLICT, "{prompt}");
            assert!(prompt.contains("same_values"), "{prompt}");
        }
    }
}
