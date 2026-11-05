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
    patch:
      operationId: patchTemplate
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
  /users:
    post:
      operationId: createUser
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {username: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
  /users/{id}:
    get:
      operationId: getUser
      parameters:
        - {name: id, in: path, required: true, description: "The user's id. A username is not accepted.", schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
  /orders:
    post:
      operationId: createOrder
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {orderNumber: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}
  /orders/{orderId}:
    get:
      operationId: getOrder
      parameters:
        - {name: orderId, in: path, required: true, description: "The order's id, not its order number.", schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}
  /widgets:
    post:
      operationId: createWidget
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
  /widgets/{widgetId}:
    get:
      operationId: getWidget
      parameters:
        - {name: widgetId, in: path, required: true, description: "The widget to read; the answer shows its name.", schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
  /memberships:
    post:
      operationId: createMembership
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {user: {type: string}, organization: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Membership"}}}}
  /memberships/{membershipId}:
    get:
      operationId: getMembership
      parameters:
        - {name: membershipId, in: path, required: true, description: "The ID of the user or organization membership.", schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Membership"}}}}
components:
  schemas:
    Membership:
      type: object
      properties:
        id: {type: string}
        user: {type: string}
        organization: {type: string}
    User:
      type: object
      properties:
        id: {type: string}
        username: {type: string}
    Order:
      type: object
      properties:
        id: {type: string}
        number: {type: integer}
        orderNumber: {type: string}
    Widget:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
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
    router_for(CONTRACT, answers)
}

fn router_for(contract: &str, answers: Vec<Value>) -> axum::Router {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let provider = Arc::new(Scripted {
        answers: Mutex::new(answers.into()),
    });
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
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

/// An edit reached by the template's id, where its alias is its key, keeps the alias:
/// the identity member is the held record's key, not the path value. By PUT and by
/// PATCH, with the model's answer and without one.
#[tokio::test]
async fn an_edit_by_the_alternate_member_keeps_the_key() {
    for (method, answer) in [
        (
            "PUT",
            json!({"TemplateId": 7001, "Name": "Welcome", "Alias": "7001", "Subject": "Hello"}),
        ),
        ("PUT", Value::Null),
        (
            "PATCH",
            json!({"TemplateId": 7001, "Name": "Welcome", "Alias": "welcome", "Subject": "Hello"}),
        ),
        ("PATCH", Value::Null),
    ] {
        let r = router(vec![
            json!({"TemplateId": 7001, "Name": "Welcome", "Alias": "welcome"}),
            answer,
        ]);
        send(
            &r,
            "POST",
            "/templates",
            Some(json!({"Name": "Welcome", "Alias": "welcome"})),
        )
        .await;
        let (status, _, edited) = send(
            &r,
            method,
            "/templates/7001",
            Some(json!({"Subject": "Hello"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{method}: {edited}");
        assert_eq!(edited["Alias"], "welcome", "{method}: {edited}");
        assert_eq!(edited["TemplateId"], 7001, "{method}: {edited}");
        for path in ["/templates/welcome", "/templates/7001"] {
            let (_, headers, read) = send(&r, "GET", path, None).await;
            assert_eq!(source(&headers), "state", "{method} {path}: {read}");
            assert_eq!(read["Alias"], "welcome", "{method} {path}: {read}");
            assert_eq!(read["TemplateId"], 7001, "{method} {path}: {read}");
            assert_eq!(read["Subject"], "Hello", "{method} {path}: {read}");
        }
    }
}

/// A description that mentions another member without offering it as the value (a
/// negation, a passing mention), or an id parameter whose description names a member,
/// gives no other way to read the record: each read misses state.
#[tokio::test]
async fn a_mention_or_a_negation_is_no_alternate_key() {
    let r = router(vec![
        json!({"id": "u_1", "username": "alice"}),
        json!({"id": "o_1", "number": 5, "orderNumber": "A-1"}),
        json!({"id": "w_1", "name": "gizmo"}),
    ]);
    send(&r, "POST", "/users", Some(json!({"username": "alice"}))).await;
    send(&r, "POST", "/orders", Some(json!({"orderNumber": "A-1"}))).await;
    send(&r, "POST", "/widgets", Some(json!({"name": "gizmo"}))).await;
    // Each held record reads from state by its id.
    for path in ["/users/u_1", "/orders/o_1", "/widgets/w_1"] {
        let (_, headers, read) = send(&r, "GET", path, None).await;
        assert_eq!(source(&headers), "state", "{path}: {read}");
    }
    for path in ["/users/alice", "/orders/A-1", "/orders/5", "/widgets/gizmo"] {
        let (_, headers, read) = send(&r, "GET", path, None).await;
        assert_ne!(source(&headers), "state", "{path}: {read}");
    }
}

/// An either-or phrase whose two sides are members other than the identity ("the ID
/// of the user or organization membership") offers no other way to name the record:
/// a read by the user's value misses state.
#[tokio::test]
async fn an_either_or_without_the_identity_is_no_alternate_key() {
    let r = router(vec![
        json!({"id": "m_1", "user": "alice", "organization": "acme"}),
    ]);
    send(
        &r,
        "POST",
        "/memberships",
        Some(json!({"user": "alice", "organization": "acme"})),
    )
    .await;
    let (_, headers, read) = send(&r, "GET", "/memberships/m_1", None).await;
    assert_eq!(source(&headers), "state", "{read}");
    for path in ["/memberships/alice", "/memberships/acme"] {
        let (_, headers, read) = send(&r, "GET", path, None).await;
        assert_ne!(source(&headers), "state", "{path}: {read}");
    }
}

/// Other spellings of an either-or: recipes keyed by `{slug}`, "the slug or id", with
/// a `recipeId`; playlists the same but with only a foreign `ownerId`; spaces read by
/// a `{key}` that is "the space's slug or id"; fonts by `{idOrName}`.
const SPELLINGS: &str = r##"
openapi: 3.0.0
info: {title: Kitchen, version: "1"}
paths:
  /recipes:
    post:
      operationId: createRecipe
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {slug: {type: string}, title: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Recipe"}}}}
  /recipes/{slug}:
    get:
      operationId: getRecipe
      parameters:
        - {name: slug, in: path, required: true, description: "The recipe's slug or id.", schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Recipe"}}}}
  /playlists:
    post:
      operationId: createPlaylist
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {slug: {type: string}, title: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Playlist"}}}}
  /playlists/{slug}:
    get:
      operationId: getPlaylist
      parameters:
        - {name: slug, in: path, required: true, description: "The playlist's slug or id.", schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Playlist"}}}}
  /spaces:
    post:
      operationId: createSpace
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {slug: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Space"}}}}
  /spaces/{key}:
    get:
      operationId: getSpace
      parameters:
        - {name: key, in: path, required: true, description: "The space's slug or id.", schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Space"}}}}
  /fonts:
    post:
      operationId: createFont
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Font"}}}}
  /fonts/{idOrName}:
    get:
      operationId: getFont
      parameters:
        - {name: idOrName, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Font"}}}}
components:
  schemas:
    Recipe:
      type: object
      properties:
        slug: {type: string}
        recipeId: {type: string}
        title: {type: string}
    Playlist:
      type: object
      properties:
        slug: {type: string}
        ownerId: {type: string}
        title: {type: string}
    Space:
      type: object
      properties:
        id: {type: string}
        slug: {type: string}
    Font:
      type: object
      properties:
        id: {type: integer}
        name: {type: string}
"##;

/// A record keyed by its slug reads by the id that names its own kind (`recipeId`).
#[tokio::test]
async fn a_slug_keyed_record_reads_by_its_own_id() {
    let r = router_for(
        SPELLINGS,
        vec![json!({"slug": "pho", "recipeId": "r_9", "title": "Pho"})],
    );
    send(
        &r,
        "POST",
        "/recipes",
        Some(json!({"slug": "pho", "title": "Pho"})),
    )
    .await;
    for path in ["/recipes/pho", "/recipes/r_9"] {
        let (status, headers, read) = send(&r, "GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {read}");
        assert_eq!(source(&headers), "state", "{path}: {read}");
        assert_eq!(read["slug"], "pho", "{path}: {read}");
    }
}

/// A slug-keyed record whose only `*Id` member names another kind (its owner): a bare
/// `id` in "slug or id" is not that foreign key, so a read by the owner's id misses.
#[tokio::test]
async fn a_foreign_key_is_no_other_id() {
    let r = router_for(
        SPELLINGS,
        vec![json!({"slug": "mix", "ownerId": "u_4", "title": "Mix"})],
    );
    send(
        &r,
        "POST",
        "/playlists",
        Some(json!({"slug": "mix", "title": "Mix"})),
    )
    .await;
    let (_, headers, read) = send(&r, "GET", "/playlists/mix", None).await;
    assert_eq!(source(&headers), "state", "{read}");
    let (_, headers, read) = send(&r, "GET", "/playlists/u_4", None).await;
    assert_ne!(source(&headers), "state", "{read}");
}

/// A `{key}` its description calls the slug or id, and an `{idOrName}`: each reads the
/// record by either member.
#[tokio::test]
async fn other_spellings_offer_the_alternate_key() {
    let r = router_for(
        SPELLINGS,
        vec![
            json!({"id": "sp_1", "slug": "ops"}),
            json!({"id": 42, "name": "Inter"}),
        ],
    );
    send(&r, "POST", "/spaces", Some(json!({"slug": "ops"}))).await;
    send(&r, "POST", "/fonts", Some(json!({"name": "Inter"}))).await;
    for (path, member, want) in [
        ("/spaces/sp_1", "slug", json!("ops")),
        ("/spaces/ops", "id", json!("sp_1")),
        ("/fonts/42", "name", json!("Inter")),
        ("/fonts/Inter", "id", json!(42)),
    ] {
        let (status, headers, read) = send(&r, "GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {read}");
        assert_eq!(source(&headers), "state", "{path}: {read}");
        assert_eq!(read[member], want, "{path}: {read}");
    }
}
