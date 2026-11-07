//! Optimistic concurrency tied by evidence, not by wording: a token the client sends in
//! a request header (`If-Match`, or another header holding a member of the record), or a
//! body member the write requires and no create sets, under a write declaring a `409` or
//! a `412` whose description names nothing. A stale one is refused before generating
//! (the model writes only its error body, once);
//! a successful update advances it. Driven over HTTP with a scripted LLM.
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

/// Notes guarded by `If-Match` against their `etag`, served in an `ETag` header too;
/// ledgers guarded by a `revision` their `{revision, actions}` update requires in its
/// body (and their delete in its query); boards guarded by a `revision` sent in a header of their own.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Shelf, version: "1"}
paths:
  /notes:
    post:
      operationId: createNote
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
  /notes/{note_id}:
    parameters: [{name: note_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getNote
      responses:
        "200":
          description: ok
          headers: {ETag: {schema: {type: string}}}
          content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}
    patch:
      operationId: updateNote
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "200":
          description: ok
          headers: {ETag: {schema: {type: string}}}
          content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}
        "412": {description: Precondition failed., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
    delete:
      operationId: deleteNote
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      responses:
        "204": {description: deleted}
        "412": {description: Precondition failed., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /ledgers:
    post:
      operationId: createLedger
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Ledger"}}}}
  /ledgers/{ledger_id}:
    parameters: [{name: ledger_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getLedger
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ledger"}}}}
    put:
      operationId: updateLedger
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [revision]
              properties:
                revision: {type: integer}
                actions: {type: array, items: {type: object, properties: {action: {type: string}, name: {type: string}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ledger"}}}}
        "409": {description: "Conflict (`StaleWrite`).", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
    delete:
      operationId: deleteLedger
      parameters: [{name: revision, in: query, required: true, schema: {type: integer}}]
      responses:
        "204": {description: deleted}
        "409": {description: "Conflict (`StaleWrite`).", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /boards:
    post:
      operationId: createBoard
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}
  /boards/{board_id}:
    parameters: [{name: board_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBoard
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}
    put:
      operationId: updateBoard
      parameters: [{name: X-Board-Revision, in: header, schema: {type: integer}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}
        "409": {description: Conflict., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
components:
  schemas:
    Note:
      type: object
      required: [id, title, etag]
      properties:
        id: {type: string}
        title: {type: string}
        etag: {type: string}
    Ledger:
      type: object
      required: [id, name, revision]
      properties:
        id: {type: string}
        name: {type: string}
        revision: {type: integer}
    Board:
      type: object
      required: [id, name, revision]
      properties:
        id: {type: string}
        name: {type: string}
        revision: {type: integer}
    Error:
      type: object
      required: [code]
      properties: {code: {type: string}, message: {type: string}}
"##;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<dyn StateStore>,
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

    /// [`Self::send`] of a create, seeded: a seeded create takes the generated path,
    /// which stores the record the model answered under the id it chose, as these
    /// tests expect.
    async fn seeded(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Value) {
        let mut headers = headers.to_vec();
        headers.push(crate::support::SEED);
        self.send(method, uri, body, &headers).await
    }

    async fn patch_note(
        &self,
        if_match: Option<&str>,
        title: &str,
    ) -> (StatusCode, HeaderMap, Value) {
        let headers: Vec<(&str, &str)> = if_match.map(|v| ("If-Match", v)).into_iter().collect();
        self.send(
            "PATCH",
            "/notes/n1",
            Some(json!({"title": title})),
            &headers,
        )
        .await
    }

    async fn put_board(&self, name: &str, revision: &str) -> (StatusCode, HeaderMap, Value) {
        self.send(
            "PUT",
            "/boards/b1",
            Some(json!({"name": name})),
            &[("X-Board-Revision", revision)],
        )
        .await
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    async fn held(&self, t: &str, id: &str) -> Value {
        let session = SessionKey {
            workspace: "default".into(),
            mock: "m".into(),
            session: "default".into(),
        };
        self.store
            .get(&session, t, id)
            .await
            .unwrap()
            .map_or(Value::Null, |r| r.data)
    }
}

fn note(title: &str, etag: &str) -> Value {
    json!({"id": "n1", "title": title, "etag": etag})
}

/// An update's answer, in the effects envelope its prompt asks for.
fn answer(body: Value) -> Value {
    json!({"effects": [], "response": body})
}

/// The error body the model writes, once per operation and status, for a stale token.
fn refused() -> Value {
    json!({"code": "stale_token", "message": "stale"})
}

/// `If-Match` holding the note's `etag`: the update's new etag is the model's, not the
/// stored copy's, and is served in the declared `ETag` header. An etag no longer current
/// is refused with the declared `412` before generating (the model writes its body once),
/// and nothing changes; a weak
/// form of the current one goes through.
#[tokio::test]
async fn a_stale_if_match_is_refused_and_the_etag_advances() {
    let h = harness(vec![
        note("a", "\"e1\""),
        answer(note("b", "\"e2\"")),
        refused(),
        refused(),
        answer(note("c", "\"e3\"")),
    ]);
    let (status, _, _) = h
        .seeded("POST", "/notes", Some(json!({"title": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, headers, body) = h.patch_note(Some("\"e1\""), "b").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["etag"], "\"e2\"", "the answer's etag stands: {body}");
    assert_eq!(h.held("notes", "n1").await["etag"], "\"e2\"");
    assert_eq!(
        headers.get("etag").and_then(|v| v.to_str().ok()),
        Some("\"e2\""),
        "the ETag header serves the token"
    );
    let (_, headers, _) = h.send("GET", "/notes/n1", None, &[]).await;
    assert_eq!(
        headers.get("etag").and_then(|v| v.to_str().ok()),
        Some("\"e2\"")
    );

    let (status, _, body) = h.patch_note(Some("\"e1\""), "stale").await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");
    assert_eq!(h.calls(), 3, "refused before generating; its body written");
    assert_eq!(body["code"], "stale_token", "{body}");
    let held = h.held("notes", "n1").await;
    assert_eq!(
        (held["title"].clone(), held["etag"].clone()),
        (json!("b"), json!("\"e2\""))
    );

    // A delete with the stale etag is refused too, and the note stays.
    let (status, _, _) = h
        .send("DELETE", "/notes/n1", None, &[("If-Match", "\"e1\"")])
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(h.calls(), 4, "the delete's own body");
    assert_eq!(h.held("notes", "n1").await["title"], "b");

    // A weak entity tag compares by its opaque value.
    let (status, _, body) = h.patch_note(Some("W/\"e2\""), "c").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.held("notes", "n1").await["etag"], "\"e3\"");
}

/// The model left the etag as it was: a successful update still gives the note a new
/// one, minted in the same format, served and stored, and the old one is stale.
#[tokio::test]
async fn an_unchanged_etag_is_minted_anew() {
    let h = harness(vec![
        note("a", "\"7f3a\""),
        answer(note("b", "\"7f3a\"")),
        refused(),
    ]);
    h.seeded("POST", "/notes", Some(json!({"title": "a"})), &[])
        .await;
    let (status, headers, body) = h.patch_note(Some("\"7f3a\""), "b").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let minted = body["etag"].as_str().unwrap().to_string();
    assert_ne!(minted, "\"7f3a\"", "{body}");
    assert!(
        minted.len() == "\"7f3a\"".len() && minted.starts_with('"') && minted.ends_with('"'),
        "same format: {minted}"
    );
    assert_eq!(h.held("notes", "n1").await["etag"], minted.as_str());
    assert_eq!(
        headers.get("etag").and_then(|v| v.to_str().ok()),
        Some(minted.as_str())
    );
    let (status, _, _) = h.patch_note(Some("\"7f3a\""), "c").await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(h.calls(), 3, "the refusal's body only");
}

/// No `If-Match`, `If-Match: *`, and the record's first write with an etag it never
/// held are all answered (by the state-first executor, from the store); a write to a
/// record nothing stores reaches the model.
#[tokio::test]
async fn no_token_a_wildcard_or_nothing_stored_is_never_refused() {
    let h = harness(vec![
        note("a", "\"e1\""),
        answer(note("b", "\"e2\"")),
        answer(note("c", "\"e3\"")),
        answer(note("d", "\"e4\"")),
        answer(note("e", "\"e5\"")),
        answer(json!({"id": "n9", "title": "x", "etag": "\"z\""})),
    ]);
    h.seeded("POST", "/notes", Some(json!({"title": "a"})), &[])
        .await;
    // First write, an etag the note never held: nothing seen of how it moves.
    assert_eq!(h.patch_note(Some("\"other\""), "b").await.0, StatusCode::OK);
    // Now an etag is tied and confirmed.
    assert_eq!(h.patch_note(Some("\"e2\""), "c").await.0, StatusCode::OK);
    assert_eq!(h.patch_note(None, "d").await.0, StatusCode::OK, "no token");
    assert_eq!(
        h.patch_note(Some("*"), "e").await.0,
        StatusCode::OK,
        "wildcard"
    );
    assert_eq!(h.calls(), 1, "each answered from the store");
    let (status, _, _) = h
        .send(
            "PATCH",
            "/notes/n9",
            Some(json!({"title": "x"})),
            &[("If-Match", "\"e1\"")],
        )
        .await;
    assert_ne!(status, StatusCode::PRECONDITION_FAILED, "nothing stored");
    assert!(h.calls() > 1, "it reached the model");
}

fn ledger(name: &str, revision: i64) -> Value {
    json!({"id": "l1", "name": name, "revision": revision})
}

fn rename(revision: i64, name: &str) -> Value {
    json!({"revision": revision, "actions": [{"action": "rename", "name": name}]})
}

/// A `revision` that is the only member of the record its update declares, required,
/// set by no create, echoed and seen to advance: a token though the `409` names nothing.
/// A lower one in the body or in the delete's query is refused; the current one, or a
/// higher one, reaches the model.
#[tokio::test]
async fn a_body_token_is_tied_by_evidence() {
    let h = harness(vec![
        ledger("a", 1),
        answer(ledger("b", 2)),
        refused(),
        refused(),
        answer(ledger("c", 3)),
        answer(ledger("d", 4)),
    ]);
    h.seeded("POST", "/ledgers", Some(json!({"name": "a"})), &[])
        .await;
    let (status, _, body) = h
        .send("PUT", "/ledgers/l1", Some(rename(1, "b")), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        h.held("ledgers", "l1").await["revision"],
        2,
        "the answer's revision stands"
    );
    let before = h.held("ledgers", "l1").await;

    let (status, _, body) = h
        .send("PUT", "/ledgers/l1", Some(rename(1, "stale")), &[])
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _, _) = h.send("DELETE", "/ledgers/l1?revision=1", None, &[]).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(h.calls(), 4, "each refusal's body, written once");
    assert_eq!(h.held("ledgers", "l1").await, before, "nothing changed");

    let (status, _, body) = h
        .send("PUT", "/ledgers/l1", Some(rename(2, "c")), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.held("ledgers", "l1").await["revision"], 3);
    // A higher one is no stale read: the model answers it.
    let (status, _, body) = h
        .send("PUT", "/ledgers/l1", Some(rename(9, "d")), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.calls(), 6);
}

/// A header of the contract's own that the client fills with the board's `revision`
/// pairs with it only once the client is seen to track it: the header followed the
/// revision when it moved. It is never minted; a stale one is then refused with the
/// declared `409`.
#[tokio::test]
async fn a_version_header_pairs_once_the_client_tracks_it() {
    let board = |name: &str, revision: i64| json!({"id": "b1", "name": name, "revision": revision});
    let h = harness(vec![
        board("a", 4),
        answer(board("b", 5)),
        answer(board("c", 5)),
        answer(board("d", 6)),
        refused(),
    ]);
    h.seeded("POST", "/boards", Some(json!({"name": "a"})), &[])
        .await;
    // A candidate: the header holds the revision, which the model moves.
    assert_eq!(h.put_board("b", "4").await.0, StatusCode::OK);
    assert_eq!(h.held("boards", "b1").await["revision"], 5);
    // The header followed it: tracked. The model leaves it, and nothing mints it.
    assert_eq!(h.put_board("c", "5").await.0, StatusCode::OK);
    assert_eq!(h.held("boards", "b1").await["revision"], 5, "not minted");
    assert_eq!(h.put_board("d", "5").await.0, StatusCode::OK);
    assert_eq!(h.held("boards", "b1").await["revision"], 6);
    assert_eq!(h.put_board("stale", "5").await.0, StatusCode::CONFLICT);
    assert_eq!(h.calls(), 5, "the refusal's body only");
}
