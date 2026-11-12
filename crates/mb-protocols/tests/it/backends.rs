//! What a mock keeps for itself under the state store's reserved type, on each backend:
//! the write memory, the deleted markers and the values a mock minted. The in-memory
//! store always runs, and Redis and Postgres when `MB_TEST_REDIS_URL` or
//! `MB_TEST_POSTGRES_URL` is set, as `mb-state`'s conformance suite gates them (see
//! `scripts/test-redis.sh` and `scripts/test-postgres.sh`). Each run has its own
//! workspace, so runs can share one server.
#![cfg(test)]
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{CachingStateStore, INTERNAL_TYPE, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

const LONG_TTL: Duration = Duration::from_secs(3600);

const ITEMS: &str = r##"
openapi: 3.0.0
info: {title: Items, version: "1"}
paths:
  /me:
    get:
      operationId: getMe
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Me"}}}}
  /items:
    post:
      operationId: createItem
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Created"}}}}
  /items/{item_id}:
    parameters: [{name: item_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getItem
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "404": {description: gone, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
    delete:
      operationId: deleteItem
      responses:
        "204": {description: gone}
  /links:
    post:
      operationId: createLink
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {item_id: {type: string}, secret: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Link"}}}}
  /links/inspect:
    post:
      operationId: inspectLink
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {link_token: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Inspection"}}}}
components:
  schemas:
    Me:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Created:
      type: object
      required: [id]
      properties: {id: {type: string}}
    Item:
      type: object
      required: [id, title]
      properties: {id: {type: string}, title: {type: string}}
    Link:
      type: object
      required: [link_token, item_id]
      properties: {link_token: {type: string}, item_id: {type: string}}
    Inspection:
      type: object
      required: [valid]
      properties: {valid: {type: boolean}}
    Error:
      type: object
      required: [code, message]
      properties: {code: {type: integer}, message: {type: string}}
"##;

/// A workspace no other run uses.
fn workspace() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("ws-{}-{nanos}", std::process::id())
}

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    state: Arc<dyn StateStore>,
    session: SessionKey,
}

fn harness(store: Arc<dyn StateStore>, script: Vec<Value>) -> Harness {
    harness_in(store, script, &workspace())
}

/// [`harness`] in `ws`: another node of the same mock, over its own store client.
fn harness_in(store: Arc<dyn StateStore>, script: Vec<Value>, ws: &str) -> Harness {
    harness_of(ITEMS, store, script, ws)
}

/// [`harness_in`] serving `contract`.
fn harness_of(contract: &str, store: Arc<dyn StateStore>, script: Vec<Value>, ws: &str) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let state = CachingStateStore::wrap(store, LONG_TTL, mb_state::DEFAULT_CACHE_ENTRIES);
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&state),
    );
    rt.workspace = ws.to_string();
    let session = SessionKey {
        workspace: rt.workspace.clone(),
        mock: "m".into(),
        session: "default".into(),
    };
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        state,
        session,
    }
}

impl Harness {
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, String) {
        self.call_with(method, uri, body, &[]).await
    }

    /// [`Self::call`] of a create, seeded: a seeded create takes the generated path,
    /// which stores the record the model answered under the id it chose, as these
    /// tests expect.
    async fn seeded(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, String) {
        self.call_with(method, uri, body, &[crate::support::SEED])
            .await
    }

    async fn call_with(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let mut req = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let source = resp
            .headers()
            .get("x-mockingbird-source")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        (status, source)
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    fn prompt(&self, n: usize) -> String {
        let requests = self.provider.requests();
        requests[n].messages.last().unwrap().content.clone()
    }

    /// The write memory's documents, without the marker that the session owns a type
    /// (`unknown_ids`).
    async fn kept(&self) -> Vec<mb_state::StoredResource> {
        let mut kept = self
            .state
            .list(&self.session, INTERNAL_TYPE, 100)
            .await
            .unwrap();
        kept.retain(|doc| !doc.id.starts_with("#owns/"));
        kept
    }
}

/// A create answering only its id tells the read after it what it asked, once, and
/// the memory stays out of what the session shows.
async fn write_memory(store: Arc<dyn StateStore>) {
    let h = harness(
        store,
        vec![
            json!({"id": "i1"}),
            json!({"id": "i1", "title": "A remembered title"}),
        ],
    );
    let (status, _) = h
        .seeded(
            "POST",
            "/items",
            Some(json!({"title": "A remembered title"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let kept = h.kept().await;
    assert_eq!(kept.len(), 1, "{kept:?}");
    let exported = h.state.export(&h.session).await.unwrap();
    assert!(exported.iter().all(|r| r.resource_type != INTERNAL_TYPE));
    let sessions = h.state.sessions(&h.session.workspace, "m").await.unwrap();
    assert_eq!(sessions[0].resources, 1, "{sessions:?}");

    let (status, source) = h.call("GET", "/items/i1", None).await;
    assert_eq!((status, source.as_str()), (StatusCode::OK, "llm"));
    assert!(
        h.prompt(1).contains("A remembered title"),
        "{}",
        h.prompt(1)
    );
    assert!(h.kept().await.is_empty(), "{:?}", h.kept().await);
    let (_, source) = h.call("GET", "/items/i1", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 2);

    h.state.reset(&h.session).await.unwrap();
    assert!(h.kept().await.is_empty());
}

/// A deleted record is a 404 that generates no record (one call writes its body's
/// code, kept for the operation), its marker stays out of what the session shows, and
/// a reset forgets it.
async fn deleted_markers(store: Arc<dyn StateStore>) {
    let h = harness(
        store,
        vec![
            json!({"id": "i1"}),
            json!({"code": 1101, "message": "gone"}),
            json!({"id": "i1", "title": "Back"}),
        ],
    );
    h.seeded("POST", "/items", Some(json!({"title": "A title"})))
        .await;
    let (status, _) = h.call("DELETE", "/items/i1", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for _ in 0..2 {
        let (status, source) = h.call("GET", "/items/i1", None).await;
        assert_eq!((status, source.as_str()), (StatusCode::NOT_FOUND, "state"));
    }
    assert_eq!(h.calls(), 2);
    assert!(
        h.kept().await.iter().any(|d| d.id.starts_with("#deleted")),
        "{:?}",
        h.kept().await
    );
    assert!(h.state.export(&h.session).await.unwrap().is_empty());

    h.state.reset(&h.session).await.unwrap();
    let (status, source) = h.call("GET", "/items/i1", None).await;
    assert_eq!((status, source.as_str()), (StatusCode::OK, "llm"));
}

/// A value a mock minted shows, when a later request carries it, the answer that
/// minted it, without the value or the client's secret; only a hash of it is kept.
async fn minted_values(store: Arc<dyn StateStore>) {
    let token = "link-sandbox-5f2b9c1e";
    let h = harness(
        store,
        vec![
            json!({"link_token": token, "item_id": "item-4711"}),
            json!({"valid": true}),
        ],
    );
    h.call(
        "POST",
        "/links",
        Some(json!({"item_id": "item-4711", "secret": "client-secret-77"})),
    )
    .await;
    h.call("POST", "/links/inspect", Some(json!({"link_token": token})))
        .await;
    let prompt = h.prompt(1);
    let origins = prompt
        .split("<earlier_origins>")
        .nth(1)
        .and_then(|rest| rest.split("</earlier_origins>").next())
        .unwrap_or_default();
    assert!(origins.contains("item-4711"), "{prompt}");
    assert!(!origins.contains(token), "{prompt}");
    assert!(!prompt.contains("client-secret-77"), "{prompt}");
    let kept = serde_json::to_string(&h.kept().await).unwrap();
    assert!(
        !kept.contains(token) && !kept.contains("client-secret-77"),
        "{kept}"
    );
    assert!(h.state.export(&h.session).await.unwrap().is_empty());

    h.state.reset(&h.session).await.unwrap();
    assert!(h.kept().await.is_empty());
}

/// A caller's answer read before the session held anything is kept in the shared store,
/// so the first write on another node, through its own store client, is told of it. The
/// read lists no session on either.
async fn answers_reach_another_node(one: Arc<dyn StateStore>, other: Arc<dyn StateStore>) {
    let ws = workspace();
    let a = harness_in(one, vec![json!({"id": "user-4711", "name": "Ann"})], &ws);
    let b = harness_in(other, vec![json!({"id": "i1", "title": "A title"})], &ws);
    let (status, _) = a.call("GET", "/me", None).await;
    assert_eq!(status, StatusCode::OK);
    for h in [&a, &b] {
        let sessions = h.state.sessions(&ws, "m").await.unwrap();
        assert!(sessions.is_empty(), "{sessions:?}");
        assert_eq!(h.state.session_count(&ws, "m").await.unwrap(), 0);
    }
    b.seeded("POST", "/items", Some(json!({"title": "A title"})))
        .await;
    let prompt = b.prompt(0);
    assert!(prompt.contains("user-4711"), "{prompt}");
    let sessions = a.state.sessions(&ws, "m").await.unwrap();
    assert_eq!(sessions.len(), 1, "the write lists it: {sessions:?}");
    a.state.reset(&a.session).await.unwrap();
    assert!(a.kept().await.is_empty());
}

const CONTACTS: &str = r##"
openapi: 3.0.0
info: {title: Contacts, version: "1"}
paths:
  /contacts:
    post:
      operationId: createContact
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}
  /contacts/{contact_id}:
    get:
      operationId: getContact
      parameters: [{name: contact_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}
  /tickets:
    post:
      operationId: createTicket
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}
components:
  schemas:
    Contact:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}, email: {type: string}}
    Ticket:
      type: object
      required: [id, subject]
      properties: {id: {type: string}, subject: {type: string}}
"##;

/// `harness`'s create of `body` at `uri` in session `session`, seeded: a seeded create
/// takes the generated path, which stores the record the model answered under the id it
/// chose, as these tests expect.
async fn post_in(h: &Harness, session: &str, uri: &str, body: Value) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-mockingbird-session", session)
        .header(crate::support::SEED.0, crate::support::SEED.1)
        .body(Body::from(body.to_string()))
        .unwrap();
    h.router.clone().oneshot(req).await.unwrap().status()
}

/// The records a prompt shows as related, empty when it shows none.
fn related_of(prompt: &str) -> &str {
    let Some(start) = prompt.find("<related_resources>") else {
        return "";
    };
    let end = prompt[start..]
        .find("</related_resources>")
        .map_or(prompt.len(), |e| start + e);
    &prompt[start..end]
}

/// A ticket created with a contact's email is shown that contact, on any store: the
/// value index holds member values whatever the backend. Neither another session nor
/// another workspace sees it, and on a store another node writes to, a second contact
/// that node stored with the same email makes the value name neither.
async fn member_values_name_their_record(one: Arc<dyn StateStore>, other: Arc<dyn StateStore>) {
    let ws = workspace();
    let shared = !one.local_only();
    let ticket = |id: &str| json!({"effects": [], "response": {"id": id, "subject": "Help"}});
    let a = harness_of(
        CONTACTS,
        Arc::clone(&one),
        vec![
            json!({"id": "ct-000101", "name": "Jane Roe", "email": "jane.roe@example.com"}),
            ticket("tk-000001"),
            ticket("tk-000002"),
            ticket("tk-000003"),
        ],
        &ws,
    );
    let asks = json!({"subject": "Help", "email": "jane.roe@example.com"});
    let contact = json!({"name": "Jane Roe", "email": "jane.roe@example.com"});
    assert_eq!(
        post_in(&a, "s1", "/contacts", contact).await,
        StatusCode::CREATED
    );
    assert_eq!(
        post_in(&a, "s1", "/tickets", asks.clone()).await,
        StatusCode::CREATED
    );
    assert!(
        related_of(&a.prompt(1)).contains("ct-000101"),
        "{}",
        a.prompt(1)
    );
    assert_eq!(
        post_in(&a, "s2", "/tickets", asks.clone()).await,
        StatusCode::CREATED
    );
    assert!(
        !related_of(&a.prompt(2)).contains("ct-000101"),
        "{}",
        a.prompt(2)
    );
    let elsewhere = harness_of(
        CONTACTS,
        Arc::clone(&one),
        vec![ticket("tk-000009")],
        &workspace(),
    );
    post_in(&elsewhere, "s1", "/tickets", asks.clone()).await;
    assert!(!related_of(&elsewhere.prompt(0)).contains("ct-000101"));
    if shared {
        let theirs = SessionKey {
            workspace: ws.clone(),
            mock: "m".into(),
            session: "s1".into(),
        };
        let twin = json!({"id": "ct-000202", "name": "J. Roe", "email": "jane.roe@example.com"});
        other
            .upsert(
                &theirs,
                mb_state::StoredResource {
                    resource_type: "contacts".into(),
                    id: "ct-000202".into(),
                    data: twin,
                },
            )
            .await
            .unwrap();
        post_in(&a, "s1", "/tickets", asks).await;
        let prompt = a.prompt(3);
        let related = related_of(&prompt);
        assert!(!related.contains("ct-000101"), "{}", a.prompt(3));
        assert!(!related.contains("ct-000202"), "{}", a.prompt(3));
    }
}

/// Each check on a fresh client of `url`; with `shared`, two clients of one store stand
/// for two nodes (an in-memory store cannot be reached twice).
async fn run_all(url: &str, shared: bool) {
    for check in 0..3 {
        let store = mb_state::connect(url, LONG_TTL).await.expect("connect");
        match check {
            0 => write_memory(store).await,
            1 => deleted_markers(store).await,
            _ => minted_values(store).await,
        }
    }
    let one = mb_state::connect(url, LONG_TTL).await.expect("connect");
    let other = if shared {
        Arc::clone(&one)
    } else {
        mb_state::connect(url, LONG_TTL).await.expect("connect")
    };
    answers_reach_another_node(Arc::clone(&one), Arc::clone(&other)).await;
    member_values_name_their_record(one, other).await;
}

#[tokio::test]
async fn memory_keeps_what_a_mock_keeps_for_itself() {
    run_all("memory", true).await;
}

#[tokio::test]
async fn postgres_keeps_what_a_mock_keeps_for_itself() {
    let Ok(url) = std::env::var("MB_TEST_POSTGRES_URL") else {
        println!("skipped: MB_TEST_POSTGRES_URL not set");
        return;
    };
    run_all(&url, false).await;
}

#[tokio::test]
async fn redis_keeps_what_a_mock_keeps_for_itself() {
    let Ok(url) = std::env::var("MB_TEST_REDIS_URL") else {
        println!("skipped: MB_TEST_REDIS_URL not set");
        return;
    };
    run_all(&url, false).await;
}

#[tokio::test]
async fn sqlite_keeps_what_a_mock_keeps_for_itself() {
    run_all("sqlite::memory:", true).await;
}

/// A SQLite file, which two processes may open: two clients of it stand for two nodes.
#[tokio::test]
async fn a_sqlite_file_keeps_what_a_mock_keeps_for_itself() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("state.db").display());
    run_all(&url, false).await;
}
