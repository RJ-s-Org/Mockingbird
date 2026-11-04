//! More cases where concurrency tokens tied by evidence must refuse nothing a client of
//! the real API would have had accepted: a `readOnly` computed member a full PUT echoes,
//! and an etag the client copies from a list after the mock minted it.
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

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Repro2, version: "1"}
paths:
  /carts:
    post:
      operationId: createCart
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}
  /carts/{cart_id}:
    parameters: [{name: cart_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getCart
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}
    put:
      operationId: replaceCart
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}
        "409": {description: A cart with this name exists., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /notes:
    get:
      operationId: listNotes
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Note"}}}}}
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
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
    patch:
      operationId: updateNote
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
        "412": {description: Precondition failed., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
components:
  schemas:
    Cart:
      type: object
      required: [id, name]
      properties:
        id: {type: string, readOnly: true}
        name: {type: string}
        total: {type: integer, readOnly: true}
    Note:
      type: object
      required: [id, title, etag]
      properties:
        id: {type: string}
        title: {type: string}
        etag: {type: string, readOnly: true}
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

fn answer(body: Value) -> Value {
    json!({"effects": [], "response": body})
}

/// A `readOnly` computed member (`total`) that a full-representation PUT echoes, and the
/// model recomputes, is no version: a later PUT carrying an older copy of it (or an SDK's
/// default 0) reaches the model, as the `409` is about names and the server ignores
/// `readOnly` input.
#[tokio::test]
async fn a_read_only_computed_member_is_never_a_version() {
    let c = |name: &str, total: i64| json!({"id": "c1", "name": name, "total": total});
    let h = harness(vec![c("a", 0), answer(c("b", 5)), answer(c("c", 5))]);
    h.send("POST", "/carts", Some(json!({"name": "a"})), &[])
        .await;
    let (s, _, b) = h
        .send(
            "PUT",
            "/carts/c1",
            Some(json!({"id": "c1", "name": "b", "total": 0})),
            &[],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _, b) = h
        .send(
            "PUT",
            "/carts/c1",
            Some(json!({"id": "c1", "name": "c", "total": 0})),
            &[],
        )
        .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "readOnly computed total refused as stale: {b}"
    );
}

/// After the mock minted an etag, a list the model answers with the old one shows the
/// stored copy, so the etag the client copies from it goes through.
#[tokio::test]
async fn an_etag_copied_from_a_list_after_a_mint_goes_through() {
    let n = |title: &str, etag: &str| json!({"id": "n1", "title": title, "etag": etag});
    let h = harness(vec![
        n("a", "\"e1\""),
        answer(n("b", "\"e1\"")),
        json!([n("b", "\"e1\"")]),
        answer(n("c", "\"e7\"")),
    ]);
    h.send("POST", "/notes", Some(json!({"title": "a"})), &[])
        .await;
    let (s, _, b) = h
        .send(
            "PATCH",
            "/notes/n1",
            Some(json!({"title": "b"})),
            &[("If-Match", "\"e1\"")],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _, list) = h.send("GET", "/notes", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let tag = list[0]["etag"].as_str().unwrap_or_default().to_string();
    assert_eq!(
        tag.as_str(),
        h.held("notes", "n1").await["etag"],
        "the stored copy"
    );
    let (s, _, b) = h
        .send(
            "PATCH",
            "/notes/n1",
            Some(json!({"title": "c"})),
            &[("If-Match", tag.as_str())],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "etag copied from the list refused: {b}");
}

/// A `readOnly` member the client is seen to track (it sent back the value the member
/// moved to) is a token: a later PUT sending a lower one is refused with the `409`.
#[tokio::test]
async fn a_read_only_member_the_client_tracks_is_a_token() {
    let c = |name: &str, total: i64| json!({"id": "c1", "name": name, "total": total});
    let h = harness(vec![c("a", 0), answer(c("b", 5)), answer(c("c", 6))]);
    h.send("POST", "/carts", Some(json!({"name": "a"})), &[])
        .await;
    let put = |name: &str, total: i64| json!({"id": "c1", "name": name, "total": total});
    let (s, _, b) = h.send("PUT", "/carts/c1", Some(put("b", 0)), &[]).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // The client sends back the value the total moved to: tracked.
    let (s, _, b) = h.send("PUT", "/carts/c1", Some(put("c", 5)), &[]).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _, b) = h.send("PUT", "/carts/c1", Some(put("d", 5)), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(h.calls(), 3);
}
