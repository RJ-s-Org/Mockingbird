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
        content: {application/json: {schema: {type: object, properties: {name: {type: string}, email: {type: string}, status: {type: string}}}}}
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
        content: {application/json: {schema: {type: object, properties: {name: {type: string}, parent: {type: object, properties: {id: {type: string}}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
        "409": {description: An item with the same name already exists in the parent., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /folders/{folder_id}:
    get:
      operationId: getFolder
      parameters: [{name: folder_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
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
    Folder:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        parent: {type: object, properties: {id: {type: string}}}
    Error:
      type: object
      required: [name, message]
      properties: {name: {type: string}, message: {type: string}}
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

/// A harness over the in-memory store, as a single node serves it.
fn harness(contract: &str, script: Vec<Value>) -> Harness {
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store) as Arc<dyn StateStore>,
    );
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

impl Harness {
    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
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
    assert_eq!(prompt, fresh.prompt(0), "the prompt is unchanged");
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
