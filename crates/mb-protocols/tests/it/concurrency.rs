//! Optimistic concurrency the contract declares: a server-managed body member a write's
//! `409`/`412` names in its description. A stale one is refused before generating (the
//! model writes only its error body, once) and changes nothing; nothing else is ever
//! refused, and nothing else of a request is rewritten. Driven over HTTP with a scripted LLM, counting the calls it was sent.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Docs whose `PUT` names the `readOnly` `version` in its `409`; items whose `409` is a
/// problem with a `title`; projects whose `409` names the `name` a write sets; drafts
/// whose `PUT` declares neither a `409` nor a `412`.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Docs, version: "1"}
paths:
  /docs:
    post:
      operationId: createDoc
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
  /docs/{doc_id}:
    parameters: [{name: doc_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getDoc
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
    put:
      operationId: replaceDoc
      parameters: [{name: version, in: query, schema: {type: integer}}]
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/DocWrite"}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
        "409": {description: "The `version` sent is stale: `error.code` is `version_conflict`. A doc whose `status` is closed conflicts too, as does one whose `state` is live.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /items:
    post:
      operationId: createItem
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
  /items/{item_id}:
    parameters: [{name: item_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getItem
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
    put:
      operationId: replaceItem
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}, notes: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "409": {description: Conflict., content: {application/problem+json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /projects:
    post:
      operationId: createProject
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
  /projects/{project_id}:
    parameters: [{name: project_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getProject
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
    put:
      operationId: replaceProject
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
        "409": {description: "A project with this `name` already exists.", content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
    patch:
      operationId: archiveProject
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {archived: {type: boolean}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}
        "409": {description: "Unarchiving conflicts when another project has the same `name`.", content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
  /drafts:
    post:
      operationId: createDraft
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
  /drafts/{draft_id}:
    parameters: [{name: draft_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getDraft
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
    put:
      operationId: replaceDraft
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/DocWrite"}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
        "422": {description: "The `version` is invalid.", content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
components:
  schemas:
    DocWrite:
      type: object
      properties:
        title: {type: string}
        version: {type: integer}
        status: {type: string, enum: [open, closed]}
        view_count: {type: integer}
        updated: {type: integer}
    Doc:
      type: object
      required: [id, title, version]
      properties:
        id: {type: string}
        title: {type: string}
        version: {type: integer, readOnly: true}
        status: {type: string, enum: [open, closed]}
        view_count: {type: integer}
        updated: {type: integer}
        owner_id: {type: string, readOnly: true}
        state: {type: string, enum: [draft, live], readOnly: true}
    Item:
      type: object
      required: [id, title]
      properties:
        id: {type: string}
        title: {type: string}
        notes: {type: string}
    Project:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        archived: {type: boolean}
    Problem:
      type: object
      required: [title, status]
      properties:
        type: {type: string}
        title: {type: string}
        status: {type: integer}
        detail: {type: string}
    Error:
      type: object
      required: [error]
      properties:
        error:
          type: object
          required: [code, message]
          properties: {code: {type: string}, message: {type: string}}
    Coded:
      type: object
      required: [code]
      properties:
        code: {type: string, enum: [precondition_failed, conflict, gone]}
"##;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<dyn StateStore>,
}

fn session(name: &str) -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: name.into(),
    }
}

fn harness(script: Vec<Value>) -> Harness {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
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
    async fn full(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Value) {
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

    async fn send(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let (status, _, body) = self.full(method, uri, body, headers).await;
        (status, body)
    }

    async fn put(&self, uri: &str, body: Value) -> StatusCode {
        self.send("PUT", uri, Some(body), &[]).await.0
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    async fn held_in(&self, session_name: &str, t: &str, id: &str) -> Value {
        self.store
            .get(&session(session_name), t, id)
            .await
            .unwrap()
            .map_or(Value::Null, |r| r.data)
    }

    async fn held(&self, t: &str, id: &str) -> Value {
        self.held_in("default", t, id).await
    }
}

fn doc(title: &str, version: i64) -> Value {
    json!({"id": "d1", "title": title, "version": version, "owner_id": "u1", "status": "open"})
}

/// An update's answer, in the effects envelope its prompt asks for.
fn answer(body: Value) -> Value {
    json!({"effects": [], "response": body})
}

/// The `version` the `409` names advances with the model's answer, and a `PUT` sending
/// another one is refused with the declared `409` before generating, its body the one
/// the model writes for a stale token. The model's answer moves neither the owner nor the id.
#[tokio::test]
async fn a_stale_version_the_409_names_is_refused_before_the_llm() {
    let mut moved = doc("b", 2);
    moved["owner_id"] = json!("someone_else");
    // The refusal's body is the model's, written once for the stale token's cause.
    let refused = json!({"error": {"code": "version_conflict", "message": "stale"}});
    let h = harness(vec![
        doc("a", 1),
        answer(moved),
        refused,
        answer(doc("d", 3)),
    ]);
    let (status, _) = h
        .send("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = h
        .send(
            "PUT",
            "/docs/d1",
            Some(json!({"title": "b", "version": 1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], 2, "the answer's version stands: {body}");
    let held = h.held("docs", "d1").await;
    assert_eq!(held["version"], 2);
    assert_eq!(
        (held["id"].clone(), held["owner_id"].clone()),
        (json!("d1"), json!("u1"))
    );

    let (status, body) = h
        .send(
            "PUT",
            "/docs/d1",
            Some(json!({"title": "c", "version": 1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "version_conflict", "{body}");
    assert_eq!(h.calls(), 3, "refused before generating; its body written");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("current version"), "{body}");
    let held = h.held("docs", "d1").await;
    assert_eq!(
        (held["title"].clone(), held["version"].clone()),
        (json!("b"), json!(2))
    );

    // The current version goes through.
    assert_eq!(
        h.put("/docs/d1", json!({"title": "d", "version": 2})).await,
        StatusCode::OK
    );
    assert_eq!(h.held("docs", "d1").await["version"], 3);
}

/// What the client sets is the client's: a title the model renamed stays as sent, and
/// the next write with the current version goes through.
#[tokio::test]
async fn a_request_value_is_never_replaced_by_the_answer() {
    let h = harness(vec![
        doc("a", 1),
        answer(doc("a (renamed)", 2)),
        answer(doc("a", 3)),
    ]);
    h.send("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    assert_eq!(
        h.put("/docs/d1", json!({"title": "a", "version": 1})).await,
        StatusCode::OK
    );
    let held = h.held("docs", "d1").await;
    assert_eq!(held["title"], "a", "{held}");
    assert_eq!(held["version"], 2, "{held}");
    assert_eq!(
        h.put("/docs/d1", json!({"title": "a", "version": 2})).await,
        StatusCode::OK
    );
    assert_eq!(h.calls(), 3);
}

/// Members no `409` ties to a refusal are never tokens, however they change: a counter,
/// an integer timestamp, a state the description mentions but enumerates. Sending an
/// old value of any of them reaches the model.
#[tokio::test]
async fn counters_timestamps_and_states_are_never_refused() {
    let step = |n: i64, status: &str| json!({"id": "d1", "title": "t", "version": 1, "status": status, "view_count": n, "updated": 1000 + n});
    let h = harness(vec![
        step(1, "open"),
        answer(step(2, "closed")),
        answer(step(3, "open")),
        answer(step(4, "closed")),
    ]);
    h.send("POST", "/docs", Some(json!({"title": "t"})), &[])
        .await;
    let write = json!({"title": "t", "view_count": 1, "updated": 1001, "status": "closed"});
    assert_eq!(h.put("/docs/d1", write.clone()).await, StatusCode::OK);
    // A cached re-PUT of the same body, old counter and timestamp included.
    assert_eq!(h.put("/docs/d1", write.clone()).await, StatusCode::OK);
    assert_eq!(
        h.put("/docs/d1", json!({"title": "t", "status": "open"}))
            .await,
        StatusCode::OK
    );
    assert_eq!(h.calls(), 4);
}

/// A first write, a write sending no token, and the same record in another session all
/// reach the model; so does a write to an operation declaring neither status.
#[tokio::test]
async fn writes_with_nothing_to_refuse_reach_the_llm() {
    let h = harness(vec![
        doc("a", 1),
        answer(doc("b", 2)),
        answer(doc("c", 3)),
        doc("a", 1),
        answer(doc("b", 2)),
    ]);
    h.send("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    // The record's first write: nothing was seen of how its version moves.
    assert_eq!(
        h.put("/docs/d1", json!({"title": "b", "version": 7})).await,
        StatusCode::OK
    );
    assert_eq!(
        h.put("/docs/d1", json!({"title": "c"})).await,
        StatusCode::OK,
        "no token"
    );
    // Another session holds its own d1, at version 1.
    let other = [("X-Mockingbird-Session", "other")];
    h.send("POST", "/docs", Some(json!({"title": "a"})), &other)
        .await;
    let (status, _) = h
        .send(
            "PUT",
            "/docs/d1",
            Some(json!({"title": "b", "version": 1})),
            &other,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.held_in("other", "docs", "d1").await["version"], 2);
    assert_eq!(h.calls(), 5);

    let h = harness(vec![doc("a", 1), answer(doc("b", 2)), answer(doc("c", 3))]);
    h.send("POST", "/drafts", Some(json!({"title": "a"})), &[])
        .await;
    h.put("/drafts/d1", json!({"title": "b", "version": 1}))
        .await;
    assert_eq!(
        h.put("/drafts/d1", json!({"title": "c", "version": 1}))
            .await,
        StatusCode::OK,
        "no 409 or 412 declared"
    );
    assert_eq!(h.calls(), 3);
}

/// A problem's `title` is no record's: a `409` declaring one ties nothing, so the
/// model never rewrites a title the request left alone, and a rename goes through.
#[tokio::test]
async fn a_problem_409_ties_no_member() {
    let item = |title: &str| json!({"id": "i1", "title": title});
    let h = harness(vec![
        item("a"),
        answer(item("rewritten")),
        answer(item("rewritten again")),
        answer(item("c")),
    ]);
    h.send("POST", "/items", Some(json!({"title": "a"})), &[])
        .await;
    assert_eq!(
        h.put("/items/i1", json!({"notes": "x"})).await,
        StatusCode::OK
    );
    assert_eq!(
        h.put("/items/i1", json!({"notes": "y"})).await,
        StatusCode::OK
    );
    assert_eq!(h.held("items", "i1").await["title"], "a", "untouched");
    assert_eq!(
        h.put("/items/i1", json!({"title": "c"})).await,
        StatusCode::OK
    );
    assert_eq!(h.held("items", "i1").await["title"], "c");
    assert_eq!(h.calls(), 4);
}

/// "A project with this `name` already exists" names a member the write sets: no
/// token, so renames reach the model, whatever it answered before.
#[tokio::test]
async fn a_member_the_write_sets_is_no_token() {
    let project = |name: &str| json!({"id": "p1", "name": name});
    let h = harness(vec![
        project("a"),
        answer(project("a2")),
        answer(project("b")),
        answer(project("c")),
    ]);
    h.send("POST", "/projects", Some(json!({"name": "a"})), &[])
        .await;
    assert_eq!(
        h.put("/projects/p1", json!({"name": "a"})).await,
        StatusCode::OK
    );
    assert_eq!(
        h.held("projects", "p1").await["name"],
        "a",
        "the request's name stands"
    );
    assert_eq!(
        h.put("/projects/p1", json!({"name": "b"})).await,
        StatusCode::OK
    );
    assert_eq!(
        h.put("/projects/p1", json!({"name": "c"})).await,
        StatusCode::OK
    );
    assert_eq!(h.calls(), 4);
}

/// A member another write of the type sets is no token, whichever write's `409` names
/// it: archiving keeps the stored name over the model's, and unarchiving with it goes
/// through.
#[tokio::test]
async fn a_member_any_write_sets_is_no_token() {
    let project =
        |name: &str, archived: bool| json!({"id": "p1", "name": name, "archived": archived});
    let h = harness(vec![
        project("a", false),
        answer(project("made-up", true)),
        answer(project("a", false)),
    ]);
    h.send("POST", "/projects", Some(json!({"name": "a"})), &[])
        .await;
    let (status, _) = h
        .send(
            "PATCH",
            "/projects/p1",
            Some(json!({"archived": true})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.held("projects", "p1").await["name"], "a", "not advanced");
    let (status, body) = h
        .send(
            "PATCH",
            "/projects/p1",
            Some(json!({"archived": false, "name": "a"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.calls(), 3);
}

/// A `readOnly` state with an `enum` is no version, though a `409` names it: the
/// model's new state is not stored over the copy's.
#[tokio::test]
async fn a_read_only_enum_state_is_no_token() {
    let with_state = |state: &str, version: i64| json!({"id": "d1", "title": "t", "version": version, "state": state});
    let h = harness(vec![
        with_state("draft", 1),
        answer(with_state("live", 2)),
        answer(with_state("draft", 3)),
    ]);
    h.send("POST", "/docs", Some(json!({"title": "t"})), &[])
        .await;
    assert_eq!(
        h.put("/docs/d1", json!({"title": "t"})).await,
        StatusCode::OK
    );
    let held = h.held("docs", "d1").await;
    assert_eq!(held["state"], "draft", "{held}");
    assert_eq!(held["version"], 2, "the version advanced: {held}");
    assert_eq!(
        h.put("/docs/d1", json!({"title": "u"})).await,
        StatusCode::OK
    );
    assert_eq!(h.calls(), 3);
}

/// A token a query parameter sets to a new value is the client's: it is not advanced,
/// and the query's value stands.
#[tokio::test]
async fn a_token_the_query_sets_is_the_clients() {
    let h = harness(vec![doc("a", 1), answer(doc("b", 2))]);
    h.send("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    let (status, body) = h
        .send(
            "PUT",
            "/docs/d1?version=5",
            Some(json!({"title": "b"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.held("docs", "d1").await["version"], 5);
}
