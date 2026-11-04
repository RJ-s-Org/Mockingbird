//! Concurrency tokens tied by evidence refuse nothing a client of the real API would
//! have had accepted, and rewrite no data on a guess: a constant header, an entity tag
//! holding a comma, an `If-Match` equal to a data member by chance, a required member
//! beside others, and an enveloped answer.
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
info: {title: Repro, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
  /widgets/{widget_id}:
    parameters: [{name: widget_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getWidget
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
    patch:
      operationId: updateWidget
      parameters: [{name: X-Api-Version, in: header, schema: {type: integer}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
        "409": {description: Conflict., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
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
  /books:
    post:
      operationId: createBook
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Book"}}}}
  /books/{book_id}:
    parameters: [{name: book_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBook
      responses:
        "200": {description: ok, headers: {ETag: {schema: {type: string}}}, content: {application/json: {schema: {$ref: "#/components/schemas/Book"}}}}
    patch:
      operationId: updateBook
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Book"}}}}
        "412": {description: Precondition failed., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /tasks:
    post:
      operationId: createTask
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
  /tasks/{task_id}:
    parameters: [{name: task_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTask
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
    put:
      operationId: replaceTask
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [title, position]
              properties: {title: {type: string}, position: {type: integer}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
        "409": {description: Conflict., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /docs:
    post:
      operationId: createDoc
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/DocEnvelope"}}}}
  /docs/{doc_id}:
    parameters: [{name: doc_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getDoc
      responses:
        "200": {description: ok, headers: {ETag: {schema: {type: string}}}, content: {application/json: {schema: {$ref: "#/components/schemas/DocEnvelope"}}}}
    patch:
      operationId: updateDoc
      parameters: [{name: If-Match, in: header, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "200": {description: ok, headers: {ETag: {schema: {type: string}}}, content: {application/json: {schema: {$ref: "#/components/schemas/DocEnvelope"}}}}
        "412": {description: Precondition failed., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
components:
  schemas:
    Widget:
      type: object
      required: [id, name, version]
      properties:
        id: {type: string}
        name: {type: string}
        version: {type: integer}
    Note:
      type: object
      required: [id, title, etag]
      properties:
        id: {type: string}
        title: {type: string}
        etag: {type: string}
    Book:
      type: object
      required: [id, title, pages]
      properties:
        id: {type: string}
        title: {type: string}
        pages: {type: integer}
    Task:
      type: object
      required: [id, title, position]
      properties:
        id: {type: string}
        title: {type: string}
        position: {type: integer}
    Doc:
      type: object
      required: [id, title, etag]
      properties:
        id: {type: string}
        title: {type: string}
        etag: {type: string}
    DocEnvelope:
      type: object
      required: [data]
      properties: {data: {$ref: "#/components/schemas/Doc"}}
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

/// A constant API-version header whose value equals the record's `version` by chance
/// (both 1 for a fresh record) stays the same while `version` moves: it never pairs, and
/// later writes carrying it reach the model.
#[tokio::test]
async fn a_constant_api_version_header_never_pairs_with_the_version() {
    let w = |name: &str, v: i64| json!({"id": "w1", "name": name, "version": v});
    let h = harness(vec![
        w("a", 1),
        answer(w("b", 2)),
        answer(w("c", 2)),
        answer(w("d", 2)),
    ]);
    h.send("POST", "/widgets", Some(json!({"name": "a"})), &[])
        .await;
    let hdr = [("X-Api-Version", "1")];
    let (s, _, b) = h
        .send("PATCH", "/widgets/w1", Some(json!({"name": "b"})), &hdr)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _, b) = h
        .send("PATCH", "/widgets/w1", Some(json!({"name": "c"})), &hdr)
        .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "same constant API version refused as stale: {b}"
    );
    assert_eq!(h.calls(), 3);
}

/// An entity tag holding a comma (a legal `etagc`, RFC 9110) is one tag: the client's
/// current etag matches and goes through.
#[tokio::test]
async fn a_quoted_etag_holding_a_comma_is_one_tag() {
    let n = |title: &str, etag: &str| json!({"id": "n1", "title": title, "etag": etag});
    let h = harness(vec![
        n("a", "\"e1\""),
        answer(n("b", "\"x,y\"")),
        answer(n("c", "\"z\"")),
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
    assert_eq!(b["etag"], "\"x,y\"");
    let (s, _, b) = h
        .send(
            "PATCH",
            "/notes/n1",
            Some(json!({"title": "c"})),
            &[("If-Match", "\"x,y\"")],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "current etag refused: {b}");
}

/// `If-Match` holding a value that merely equals an integer data member (`pages` is 3,
/// the ETag the client sends is "3"): no pairing, and `pages` keeps what the model
/// left.
#[tokio::test]
async fn if_match_never_pairs_with_an_integer_data_member() {
    let bk = |title: &str, pages: i64| json!({"id": "b1", "title": title, "pages": pages});
    let h = harness(vec![bk("a", 3), answer(bk("b", 3))]);
    h.send("POST", "/books", Some(json!({"title": "a"})), &[])
        .await;
    let (s, _, b) = h
        .send(
            "PATCH",
            "/books/b1",
            Some(json!({"title": "b"})),
            &[("If-Match", "\"3\"")],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        h.held("books", "b1").await["pages"],
        3,
        "data member rewritten on a guess"
    );
}

/// A required PUT member that no create sets (`position`), declared beside other
/// members of the record, is no token: a deliberate move to a new position reaches the
/// model.
#[tokio::test]
async fn a_required_member_beside_others_is_never_a_token() {
    let t = |title: &str, p: i64| json!({"id": "t1", "title": title, "position": p});
    let h = harness(vec![t("a", 0), answer(t("b", 1)), answer(t("c", 3))]);
    h.send("POST", "/tasks", Some(json!({"title": "a"})), &[])
        .await;
    let (s, _, b) = h
        .send(
            "PUT",
            "/tasks/t1",
            Some(json!({"title": "b", "position": 0})),
            &[],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _, b) = h
        .send(
            "PUT",
            "/tasks/t1",
            Some(json!({"title": "c", "position": 3})),
            &[],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "a deliberate new position refused: {b}");
}

/// An enveloped answer (`{data: {...}}`) whose etag the model kept shows the minted
/// one, so the client's next write with what it was shown goes through.
#[tokio::test]
async fn an_enveloped_answer_shows_the_minted_etag() {
    let d = |title: &str, etag: &str| json!({"data": {"id": "d1", "title": title, "etag": etag}});
    let h = harness(vec![
        d("a", "\"e1\""),
        answer(d("b", "\"e1\"")),
        answer(d("c", "\"e9\"")),
    ]);
    h.send("POST", "/docs", Some(json!({"title": "a"})), &[])
        .await;
    let (s, hd, b) = h
        .send(
            "PATCH",
            "/docs/d1",
            Some(json!({"title": "b"})),
            &[("If-Match", "\"e1\"")],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        hd.get("etag").and_then(|v| v.to_str().ok()),
        h.held("docs", "d1").await["etag"].as_str(),
        "the ETag header serves the stored etag"
    );
    let shown = b["data"]["etag"].as_str().unwrap_or_default().to_string();
    let (s, _, b) = h
        .send(
            "PATCH",
            "/docs/d1",
            Some(json!({"title": "c"})),
            &[("If-Match", shown.as_str())],
        )
        .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "the etag the client was shown refused: {b}"
    );
}
