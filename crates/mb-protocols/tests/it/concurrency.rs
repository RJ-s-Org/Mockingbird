//! Optimistic concurrency the contract declares: a body member a write's `409`/`412`
//! names, an `If-Match` against the `ETag` the mock served, and a JSON Patch `test`. A
//! stale one is refused before any LLM call and changes nothing; nothing else is ever
//! refused, and nothing else of a request is rewritten. Driven over HTTP with a scripted
//! LLM, counting the calls it was sent.
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

/// Docs whose `PUT` names `version` in its `409`, whose `PATCH` and `DELETE` take
/// `If-Match` and answer `ETag`; notes patched by JSON Patch; tasks taking `If-Match`
/// with no `ETag` served; drafts whose `PUT` declares neither a `409` nor a `412`.
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
        "201":
          description: created
          headers: {ETag: {schema: {type: string}}}
          content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}
  /docs/{doc_id}:
    parameters: [{name: doc_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getDoc
      responses:
        "200":
          description: ok
          headers: {ETag: {schema: {type: string}}}
          content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}
    put:
      operationId: replaceDoc
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/DocWrite"}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
        "409": {description: "The `version` sent is stale: `error.code` is `version_conflict`. A doc whose `status` is closed conflicts too.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
    patch:
      operationId: updateDoc
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "200":
          description: ok
          headers: {ETag: {schema: {type: string}}}
          content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}
        "412": {description: Answers `precondition_failed`., content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
    delete:
      operationId: deleteDoc
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      responses:
        "204": {description: deleted}
        "412": {description: stale, content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
  /notes:
    post:
      operationId: createNote
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {text: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
  /notes/{note_id}:
    parameters: [{name: note_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getNote
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
    patch:
      operationId: patchNote
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: array
              items:
                type: object
                required: [op, path]
                properties:
                  op: {type: string, enum: [add, remove, replace, move, copy, test]}
                  path: {type: string}
                  value: {}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
        "409": {description: "The `rev` tested is not the note's.", content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
  /tasks:
    post:
      operationId: createTask
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {priority: {type: integer}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
  /tasks/{task_id}:
    parameters: [{name: task_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTask
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
    patch:
      operationId: updateTask
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {priority: {type: integer}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
        "412": {description: "Fails unless `priority` matches.", content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
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
    Note:
      type: object
      required: [id, text, rev]
      properties:
        id: {type: string}
        text: {type: string}
        rev: {type: string}
    Task:
      type: object
      required: [id, priority]
      properties:
        id: {type: string}
        priority: {type: integer}
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

fn etag(headers: &HeaderMap) -> String {
    headers
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// The `version` the `409` names advances with the model's answer, and a `PUT` sending
/// another one is refused with the declared `409`, its code the one the description
/// names, before any LLM call. The model's answer moves neither the owner nor the id.
#[tokio::test]
async fn a_stale_version_the_409_names_is_refused_before_the_llm() {
    let mut moved = doc("b", 2);
    moved["owner_id"] = json!("someone_else");
    let h = harness(vec![doc("a", 1), answer(moved), answer(doc("d", 3))]);
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
    assert_eq!(h.calls(), 2, "refused without an LLM call");
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

/// `If-Match` is compared with the `ETag` the mock served for the record: stable across
/// reads, new after a write. An old one is refused with the declared `412`, for a
/// `PATCH` and a `DELETE`; `*` and no header are not.
#[tokio::test]
async fn if_match_is_compared_with_the_served_etag() {
    let h = harness(vec![
        doc("a", 1),
        answer(doc("b", 1)),
        answer(doc("c", 1)),
        answer(doc("d", 1)),
    ]);
    let (_, headers, _) = h
        .full("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    let first = etag(&headers);
    assert!(!first.is_empty());
    let (_, headers, _) = h.full("GET", "/docs/d1", None, &[]).await;
    assert_eq!(etag(&headers), first, "a read serves the current tag");

    let (status, headers, _) = h
        .full(
            "PATCH",
            "/docs/d1",
            Some(json!({"title": "b"})),
            &[("If-Match", &first)],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let second = etag(&headers);
    assert_ne!(second, first);
    let (_, headers, _) = h.full("GET", "/docs/d1", None, &[]).await;
    assert_eq!(etag(&headers), second);

    let weak = format!("W/{first}");
    let (status, body) = h
        .send(
            "PATCH",
            "/docs/d1",
            Some(json!({"title": "x"})),
            &[("If-Match", &weak)],
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");
    assert_eq!(body["code"], "precondition_failed", "{body}");
    let (status, _) = h
        .send("DELETE", "/docs/d1", None, &[("If-Match", &first)])
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(h.calls(), 2);
    assert_eq!(h.held("docs", "d1").await["title"], "b");

    let (status, _) = h
        .send(
            "PATCH",
            "/docs/d1",
            Some(json!({"title": "c"})),
            &[("If-Match", "*")],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .send("PATCH", "/docs/d1", Some(json!({"title": "d"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.calls(), 4);
}

/// With no `ETag` served for the record, `If-Match` decides nothing, and a member whose
/// value it happens to hold is no token.
#[tokio::test]
async fn an_entity_tag_is_never_compared_with_a_member() {
    let task = |p: i64| json!({"id": "t1", "priority": p});
    let h = harness(vec![task(1), answer(task(2)), answer(task(5))]);
    h.send("POST", "/tasks", Some(json!({"priority": 1})), &[])
        .await;
    let (status, _) = h
        .send(
            "PATCH",
            "/tasks/t1",
            Some(json!({"priority": 2})),
            &[("If-Match", "\"1\"")],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .send(
            "PATCH",
            "/tasks/t1",
            Some(json!({"priority": 5})),
            &[("If-Match", "\"1\"")],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.held("tasks", "t1").await["priority"], 5);
    assert_eq!(h.calls(), 3);
}

/// A JSON Patch applies to the stored note, and a `test` failing against it is refused
/// with the declared `409`, from the first write on.
#[tokio::test]
async fn a_failed_json_patch_test_is_refused() {
    let note = |text: &str, rev: &str| json!({"id": "n1", "text": text, "rev": rev});
    let h = harness(vec![
        note("t", "1-a"),
        answer(note("u", "2-b")),
        answer(note("v", "3-c")),
    ]);
    h.send("POST", "/notes", Some(json!({"text": "t"})), &[])
        .await;
    let stale = json!([{"op": "test", "path": "/rev", "value": "0-z"}]);
    let (status, body) = h.send("PATCH", "/notes/n1", Some(stale), &[]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(h.calls(), 1);

    let patch = json!([
        {"op": "test", "path": "/rev", "value": "1-a"},
        {"op": "replace", "path": "/text", "value": "u"},
    ]);
    let (status, body) = h.send("PATCH", "/notes/n1", Some(patch.clone()), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let held = h.held("notes", "n1").await;
    assert_eq!(held["text"], "u", "the patch applied: {held}");
    assert_eq!(
        held["rev"], "2-b",
        "the `rev` the 409 names advanced: {held}"
    );

    // The same patch again tests a revision no longer held.
    let (status, _) = h.send("PATCH", "/notes/n1", Some(patch), &[]).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // A test of what an earlier patch set passes.
    let follow = json!([
        {"op": "test", "path": "/text", "value": "u"},
        {"op": "replace", "path": "/text", "value": "v"},
    ]);
    let (status, _) = h.send("PATCH", "/notes/n1", Some(follow), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.calls(), 3);
}
