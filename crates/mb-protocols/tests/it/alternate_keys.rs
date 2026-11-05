//! A path parameter that names a record by either of two of its members (an id or an
//! alias, an id or a slug, as its name or description says): a lookup by the value
//! that misses the record's key finds it by the other member, when exactly one held
//! record has that value there. Driven over HTTP with a scripted LLM.
#![cfg(test)]
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Capabilities, ChatRequest, ChatResponse, LlmError, Provider, ResponseFormat, Usage};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Templates are read, edited and deleted by `templateIdOrAlias`; projects by a
/// `project` its description says is an id or a slug; badges by an id alone.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Letters, version: "1"}
paths:
  /templates:
    post:
      operationId: createTemplate
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewTemplate"}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Brief"}}}}
  /templates/{templateIdOrAlias}:
    parameters:
      - name: templateIdOrAlias
        in: path
        required: true
        description: The template's id or its alias.
        schema: {type: string}
    get:
      operationId: getTemplate
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}
        "404": {description: missing, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
    put:
      operationId: editTemplate
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/NewTemplate"}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}
    delete:
      operationId: deleteTemplate
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /projects:
    post:
      operationId: createProject
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}, slug: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
  /projects/{project}:
    get:
      operationId: getProject
      parameters:
        - {name: project, in: path, required: true, description: The project's id or slug., schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
  /badges:
    post:
      operationId: createBadge
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {label: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Badge"}}}}
  /badges/{badgeId}:
    get:
      operationId: getBadge
      parameters:
        - {name: badgeId, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Badge"}}}}
components:
  schemas:
    NewTemplate:
      type: object
      properties:
        Name: {type: string}
        Alias: {type: string}
        Subject: {type: string}
    Brief:
      type: object
      properties:
        TemplateId: {type: integer}
        Name: {type: string}
        Alias: {type: string}
    Template:
      type: object
      properties:
        TemplateId: {type: integer}
        Name: {type: string}
        Alias: {type: string}
        Subject: {type: string}
    Project:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
        slug: {type: string}
    Badge:
      type: object
      properties:
        id: {type: string}
        label: {type: string}
    Error:
      type: object
      properties:
        ErrorCode: {type: integer}
        Message: {type: string}
"##;

/// Answers each request with the next of its answers (wrapped in the effects
/// envelope when asked); a `null` one, or none left, fails, so a held record is
/// served from state.
struct Scripted {
    answers: Mutex<VecDeque<Value>>,
}

#[async_trait]
impl Provider for Scripted {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let enveloped = match &req.response_format {
            ResponseFormat::JsonSchema { schema, .. } => {
                schema.pointer("/properties/effects").is_some()
            }
            _ => false,
        };
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .filter(|a| !a.is_null())
            .ok_or_else(|| LlmError::Empty("no answer scripted".into()))?;
        let content = match enveloped {
            true => json!({"effects": [], "response": answer}),
            false => answer,
        };
        Ok(ChatResponse {
            content: content.to_string(),
            usage: Usage::default(),
        })
    }
}

fn router(answers: Vec<Value>) -> axum::Router {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let provider = Arc::new(Scripted {
        answers: Mutex::new(answers.into()),
    });
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        store,
    );
    http_rest::router(Arc::new(rt))
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn source(headers: &HeaderMap) -> String {
    header(headers, "x-mockingbird-source")
}

/// The template is created with an alias and answered with a server-made id: it reads
/// the same by either, an edit by its alias shows by its id, and a delete by its id
/// leaves its alias naming nothing held.
#[tokio::test]
async fn a_template_reads_the_same_by_id_and_by_alias() {
    let r = router(vec![
        json!({"TemplateId": 7001, "Name": "Welcome", "Alias": "welcome"}),
        Value::Null,
        Value::Null,
        json!({"TemplateId": 7001, "Name": "Welcome", "Alias": "welcome", "Subject": "Hello"}),
        Value::Null,
        json!({"ErrorCode": 0, "Message": "Deleted."}),
    ]);
    let (status, _, body) = send(
        &r,
        "POST",
        "/templates",
        Some(json!({"Name": "Welcome", "Alias": "welcome", "Subject": "Hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for path in ["/templates/7001", "/templates/welcome"] {
        let (status, headers, read) = send(&r, "GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {read}");
        assert_eq!(source(&headers), "state", "{path}: {read}");
        assert_eq!(read["TemplateId"], 7001, "{path}: {read}");
        assert_eq!(read["Alias"], "welcome", "{path}: {read}");
    }

    let (status, _, _) = send(
        &r,
        "PUT",
        "/templates/welcome",
        Some(json!({"Subject": "Hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, headers, read) = send(&r, "GET", "/templates/7001", None).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["Subject"], "Hello", "{read}");

    let (status, _, _) = send(&r, "DELETE", "/templates/7001", None).await;
    assert_eq!(status, StatusCode::OK);
    // Gone by either: the read answers the declared 404.
    for path in ["/templates/7001", "/templates/welcome"] {
        let (status, _, read) = send(&r, "GET", path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {read}");
    }
}

/// A value that is one template's key and another's id is the first's: the key wins,
/// and the other is found by its own id.
#[tokio::test]
async fn the_key_wins_over_another_records_alternate() {
    let r = router(vec![
        json!({"TemplateId": 12, "Name": "A", "Alias": "first"}),
        json!({"TemplateId": 13, "Name": "B", "Alias": "12"}),
    ]);
    send(
        &r,
        "POST",
        "/templates",
        Some(json!({"Name": "A", "Alias": "first"})),
    )
    .await;
    send(
        &r,
        "POST",
        "/templates",
        Some(json!({"Name": "B", "Alias": "12"})),
    )
    .await;
    // `12` is B's key: the key wins.
    let (_, _, read) = send(&r, "GET", "/templates/12", None).await;
    assert_eq!(read["Name"], "B", "{read}");
    let (_, headers, read) = send(&r, "GET", "/templates/13", None).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["Alias"], "12", "{read}");
}

/// A project read by its slug, where its description says the value is an id or a
/// slug, is the project held under its id.
#[tokio::test]
async fn a_described_slug_finds_the_record() {
    let r = router(vec![json!({"id": "prj_1", "name": "Site", "slug": "site"})]);
    send(
        &r,
        "POST",
        "/projects",
        Some(json!({"name": "Site", "slug": "site"})),
    )
    .await;
    let (status, headers, read) = send(&r, "GET", "/projects/site", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["id"], "prj_1");
}

/// A parameter that names an id alone does not match another member: a badge's label
/// is no way to read it.
#[tokio::test]
async fn an_id_parameter_does_not_match_another_member() {
    let r = router(vec![json!({"id": "b_1", "label": "gold"})]);
    send(&r, "POST", "/badges", Some(json!({"label": "gold"}))).await;
    let (_, headers, read) = send(&r, "GET", "/badges/gold", None).await;
    assert_ne!(source(&headers), "state", "{read}");
}
