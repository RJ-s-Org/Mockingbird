//! Optimistic concurrency: a write sending a token its record held before (a `version`
//! in the body, an `If-Match` entity tag, a JSON Patch `test`) is refused with the
//! operation's declared `409` or `412` before any LLM call, and changes nothing. A token
//! is a member the mock saw a write echo and every write since change. Driven over
//! HTTP with a scripted LLM, counting the calls it was sent.
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

/// Docs updated by a `PUT` sending their `version`, by a `PATCH` and a `DELETE` under
/// `If-Match`, and by a JSON Patch testing their `rev`; drafts whose `PUT` declares
/// neither a `409` nor a `412`.
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
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}, version: {type: integer}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
        "409": {description: "The version is stale: `error.code` is `version_conflict`.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
    patch:
      operationId: updateDoc
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
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
        "409": {description: conflict, content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
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
        content: {application/json: {schema: {type: object, properties: {title: {type: string}, version: {type: integer}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Doc"}}}}
        "422": {description: invalid, content: {application/json: {schema: {$ref: "#/components/schemas/Coded"}}}}
components:
  schemas:
    Doc:
      type: object
      required: [id, title, version, etag]
      properties:
        id: {type: string}
        title: {type: string}
        version: {type: integer}
        etag: {type: string, readOnly: true}
        updated_at: {type: string, format: date-time}
    Note:
      type: object
      required: [id, text, rev]
      properties:
        id: {type: string}
        text: {type: string}
        rev: {type: string}
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

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "default".into(),
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
    async fn send(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
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
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    async fn held(&self, t: &str, id: &str) -> Value {
        self.store
            .get(&session(), t, id)
            .await
            .unwrap()
            .map_or(Value::Null, |r| r.data)
    }
}

fn doc(title: &str, version: i64, etag: &str) -> Value {
    json!({"id": "d1", "title": title, "version": version, "etag": etag})
}

/// An update's answer, in the effects envelope its prompt asks for.
fn answer(body: Value) -> Value {
    json!({"effects": [], "response": body})
}

/// A `PUT` echoing the `version` it read gets it advanced; one sending that old
/// version again is refused with the declared `409` before the LLM, its code the one
/// the description names, and the record keeps what the first write stored.
#[tokio::test]
async fn a_stale_version_in_the_body_is_refused_before_the_llm() {
    let h = harness(vec![
        doc("a", 1, "e1"),
        answer(doc("b", 2, "e2")),
        answer(doc("d", 3, "e3")),
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
    assert_eq!(h.held("docs", "d1").await["version"], 2);

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

    // The current version goes through, and advances again.
    let (status, body) = h
        .send(
            "PUT",
            "/docs/d1",
            Some(json!({"title": "d", "version": 2})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], 3, "{body}");
    assert_eq!(h.calls(), 3);
}

/// A `PATCH` under the etag it read advances it; one under that old etag is refused
/// with the declared `412`, and a `DELETE` under it too, while `If-Match: *` and a
/// write sending no etag are left to the LLM.
#[tokio::test]
async fn a_stale_if_match_is_refused_with_the_declared_412() {
    let h = harness(vec![
        doc("a", 1, "e1"),
        answer(doc("b", 1, "e2")),
        answer(doc("c", 1, "e3")),
        answer(doc("d", 1, "e4")),
    ]);
    h.send("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    let (status, body) = h
        .send(
            "PATCH",
            "/docs/d1",
            Some(json!({"title": "b"})),
            &[("If-Match", "\"e1\"")],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        h.held("docs", "d1").await["etag"],
        "e2",
        "a readOnly etag advanced"
    );

    let (status, body) = h
        .send(
            "PATCH",
            "/docs/d1",
            Some(json!({"title": "x"})),
            &[("If-Match", "W/\"e1\"")],
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");
    assert_eq!(body["code"], "precondition_failed", "{body}");
    let (status, _) = h
        .send("DELETE", "/docs/d1", None, &[("If-Match", "\"e1\"")])
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
    assert_eq!(h.held("docs", "d1").await["etag"], "e4");
}

/// A JSON Patch whose `test` of `/rev` names the revision it read advances it; the
/// same patch again is refused with the declared `409`.
#[tokio::test]
async fn a_json_patch_testing_a_stale_rev_is_refused() {
    let note = |rev: &str| json!({"id": "n1", "text": "t", "rev": rev});
    let h = harness(vec![note("1-a"), answer(note("2-b"))]);
    h.send("POST", "/notes", Some(json!({"text": "t"})), &[])
        .await;
    let patch = |rev: &str| {
        json!([
            {"op": "test", "path": "/rev", "value": rev},
            {"op": "replace", "path": "/text", "value": "u"},
        ])
    };
    let (status, body) = h.send("PATCH", "/notes/n1", Some(patch("1-a")), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.held("notes", "n1").await["rev"], "2-b");
    let (status, body) = h.send("PATCH", "/notes/n1", Some(patch("1-a")), &[]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(h.calls(), 2);
    assert_eq!(h.held("notes", "n1").await["rev"], "2-b");
}

/// No token: the first write (nothing seen yet), a write sending none, a value the
/// record never held, and an operation declaring neither status all reach the LLM.
#[tokio::test]
async fn writes_with_no_stale_token_reach_the_llm() {
    let h = harness(vec![
        doc("a", 1, "e1"),
        answer(doc("b", 2, "e2")),
        answer(doc("c", 3, "e3")),
        answer(doc("d", 4, "e4")),
    ]);
    h.send("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    // The record's first write, sending a version it never held.
    let (status, _) = h
        .send(
            "PUT",
            "/docs/d1",
            Some(json!({"title": "b", "version": 7})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .send("PUT", "/docs/d1", Some(json!({"title": "c"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "no token sent");
    let (status, _) = h
        .send(
            "PUT",
            "/docs/d1",
            Some(json!({"title": "d", "version": 99})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "a version never held");
    assert_eq!(h.calls(), 4);

    let draft = |title: &str, version: i64| json!({"id": "d1", "title": title, "version": version, "etag": "x"});
    let h = harness(vec![
        draft("a", 1),
        answer(draft("b", 2)),
        answer(draft("c", 3)),
    ]);
    h.send("POST", "/drafts", Some(json!({"title": "a"})), &[])
        .await;
    h.send(
        "PUT",
        "/drafts/d1",
        Some(json!({"title": "b", "version": 1})),
        &[],
    )
    .await;
    let (status, _) = h
        .send(
            "PUT",
            "/drafts/d1",
            Some(json!({"title": "c", "version": 1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "no 409 or 412 declared");
    assert_eq!(h.calls(), 3);
}

/// A member one write left as it was is no token: an old value of it later is not
/// refused.
#[tokio::test]
async fn a_member_a_write_left_unchanged_is_no_token() {
    let h = harness(vec![
        doc("a", 1, "e1"),
        answer(doc("b", 2, "e2")),
        answer(doc("c", 2, "e3")),
        answer(doc("d", 2, "e4")),
    ]);
    h.send("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    h.send(
        "PUT",
        "/docs/d1",
        Some(json!({"title": "b", "version": 1})),
        &[],
    )
    .await;
    // Its answer keeps the version: it changed on one write, not on every one.
    let (status, _) = h
        .send("PUT", "/docs/d1", Some(json!({"title": "c"})), &[])
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .send(
            "PUT",
            "/docs/d1",
            Some(json!({"title": "d", "version": 1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.calls(), 4);
}
