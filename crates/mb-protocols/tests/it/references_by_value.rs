//! A request that refers to a stored record by a value no member rule knows: an id
//! inside a URI (`spotify:track:<id>`), a short id in a nested object named after its
//! type (`transition: {id: "21"}`), or a unique member value such as an email. The
//! records the mock has stored are indexed by their identities and unique member values,
//! and a request's values are matched against that index. A match must be unique and
//! not trivial, and is shown as related context only. Driven over HTTP with a scripted
//! LLM, asserting the prompts it was sent and what the store holds.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{
    MemoryStore, SessionInfo, SessionKey, StateChanged, StateError, StateStore, StoredResource,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower::ServiceExt;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: References, version: "1"}
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
    put:
      operationId: updateContact
      parameters: [{name: contact_id, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}
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
  /tickets/{ticket_id}:
    get:
      operationId: getTicket
      parameters: [{name: ticket_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}
  /tracks:
    post:
      operationId: createTrack
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Track"}}}}
  /tracks/{track_id}:
    get:
      operationId: getTrack
      parameters: [{name: track_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Track"}}}}
  /transitions:
    post:
      operationId: createTransition
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Transition"}}}}
  /transitions/{transition_id}:
    get:
      operationId: getTransition
      parameters: [{name: transition_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Transition"}}}}
  /playlists/{playlist_id}/items:
    post:
      operationId: addItems
      parameters: [{name: playlist_id, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}
  /moves:
    post:
      operationId: move
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}
components:
  schemas:
    Contact:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        email: {type: string}
        state: {type: string, enum: [on-hold, active-1]}
        joined: {type: string, format: date}
        api_key: {type: string}
        timezone: {type: string}
        status: {type: string}
    Ticket:
      type: object
      required: [id, subject]
      properties:
        id: {type: string}
        subject: {type: string}
        email: {type: string}
        owner: {type: string}
        requester_id: {type: string}
        requester:
          type: object
          properties:
            id: {type: string}
            name: {type: string}
            email: {type: string}
    Track:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
    Transition:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
    Ack:
      type: object
      required: [ok]
      properties:
        ok: {type: boolean}
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

/// A store other nodes may write to as well: every call goes to a [`MemoryStore`], but
/// it does not say that only this process writes to it.
struct Shared(Arc<MemoryStore>);

#[async_trait::async_trait]
impl StateStore for Shared {
    async fn get(
        &self,
        s: &SessionKey,
        t: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        self.0.get(s, t, id).await
    }
    async fn list(
        &self,
        s: &SessionKey,
        t: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        self.0.list(s, t, limit).await
    }
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        self.0.upsert(s, r).await
    }
    async fn delete(&self, s: &SessionKey, t: &str, id: &str) -> Result<bool, StateError> {
        self.0.delete(s, t, id).await
    }
    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        self.0.reset(s).await
    }
    async fn reset_mock(&self, w: &str, m: &str) -> Result<(), StateError> {
        self.0.reset_mock(w, m).await
    }
    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.0.touch(s).await
    }
    async fn touch_live(&self, s: &SessionKey) -> Result<bool, StateError> {
        self.0.touch_live(s).await
    }
    async fn sessions(&self, w: &str, m: &str) -> Result<Vec<SessionInfo>, StateError> {
        self.0.sessions(w, m).await
    }
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        self.0.export(s).await
    }
    async fn seed(&self, s: &SessionKey, r: Vec<StoredResource>) -> Result<(), StateError> {
        self.0.seed(s, r).await
    }
    async fn ping(&self) -> Result<(), StateError> {
        self.0.ping().await
    }
    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.0.subscribe()
    }
}

/// The in-memory store, as only this process writes to it, that counts the reads of
/// each record and can be made to fail them.
struct Flaky {
    inner: Arc<MemoryStore>,
    /// Reads of (type, id) so far.
    reads: std::sync::Mutex<std::collections::HashMap<(String, String), usize>>,
    /// The record whose next read fails.
    fail: std::sync::Mutex<Option<(String, String)>>,
}

impl Flaky {
    fn reads_of(&self, t: &str, id: &str) -> usize {
        let reads = self.reads.lock().unwrap();
        reads
            .get(&(t.to_string(), id.to_string()))
            .copied()
            .unwrap_or(0)
    }

    fn fail_next_read_of(&self, t: &str, id: &str) {
        *self.fail.lock().unwrap() = Some((t.to_string(), id.to_string()));
    }
}

#[async_trait::async_trait]
impl StateStore for Flaky {
    async fn get(
        &self,
        s: &SessionKey,
        t: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        let key = (t.to_string(), id.to_string());
        *self.reads.lock().unwrap().entry(key.clone()).or_default() += 1;
        let failing = {
            let mut fail = self.fail.lock().unwrap();
            let hit = fail.as_ref() == Some(&key);
            if hit {
                *fail = None;
            }
            hit
        };
        if failing {
            return Err(StateError::Backend("injected".into()));
        }
        self.inner.get(s, t, id).await
    }
    async fn list(
        &self,
        s: &SessionKey,
        t: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        self.inner.list(s, t, limit).await
    }
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        self.inner.upsert(s, r).await
    }
    async fn delete(&self, s: &SessionKey, t: &str, id: &str) -> Result<bool, StateError> {
        self.inner.delete(s, t, id).await
    }
    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.reset(s).await
    }
    async fn reset_mock(&self, w: &str, m: &str) -> Result<(), StateError> {
        self.inner.reset_mock(w, m).await
    }
    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.touch(s).await
    }
    async fn touch_live(&self, s: &SessionKey) -> Result<bool, StateError> {
        self.inner.touch_live(s).await
    }
    async fn sessions(&self, w: &str, m: &str) -> Result<Vec<SessionInfo>, StateError> {
        self.inner.sessions(w, m).await
    }
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        self.inner.export(s).await
    }
    async fn seed(&self, s: &SessionKey, r: Vec<StoredResource>) -> Result<(), StateError> {
        self.inner.seed(s, r).await
    }
    async fn ping(&self) -> Result<(), StateError> {
        self.inner.ping().await
    }
    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.inner.subscribe()
    }
    fn local_only(&self) -> bool {
        true
    }
}

/// A harness over the in-memory store behind the response cache, as a single node
/// serves it.
fn harness(script: Vec<Value>) -> Harness {
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let cached = mb_state::CachingStateStore::wrap(
        Arc::clone(&store) as Arc<dyn StateStore>,
        Duration::from_secs(3600),
        mb_state::DEFAULT_CACHE_ENTRIES,
    );
    harness_on(cached, store, script)
}

/// A harness over a [`Flaky`] store, whose breaker lets every call try the store.
fn flaky(script: Vec<Value>) -> (Harness, Arc<Flaky>) {
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let flaky = Arc::new(Flaky {
        inner: Arc::clone(&store),
        reads: std::sync::Mutex::default(),
        fail: std::sync::Mutex::default(),
    });
    let state = Arc::clone(&flaky) as Arc<dyn StateStore>;
    let h = harness_with(state, store, script, |rt| {
        rt.state_health = Arc::new(mb_protocols::StateHealth::new(Duration::ZERO));
    });
    (h, flaky)
}

/// A harness whose runtime goes through `state`, which keeps its records in `store`.
fn harness_on(state: Arc<dyn StateStore>, store: Arc<MemoryStore>, script: Vec<Value>) -> Harness {
    harness_with(state, store, script, |_| {})
}

fn harness_with(
    state: Arc<dyn StateStore>,
    store: Arc<MemoryStore>,
    script: Vec<Value>,
    change: impl FnOnce(&mut MockRuntime),
) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        state,
    );
    change(&mut rt);
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

impl Harness {
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call_in(None, method, uri, body).await
    }

    async fn call_in(
        &self,
        session: Option<&str>,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        if let Some(session) = session {
            req = req.header("x-mockingbird-session", session);
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

    /// The user message of the `n`th LLM call.
    fn prompt(&self, n: usize) -> String {
        let requests = self.provider.requests();
        requests[n].messages.last().unwrap().content.clone()
    }

    async fn held(&self, resource_type: &str, id: &str) -> Value {
        self.store
            .get(&session(), resource_type, id)
            .await
            .unwrap()
            .unwrap()
            .data
    }
}

/// The text between a block's tags, or `None` without one.
fn block<'a>(prompt: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = prompt.find(&open)? + open.len();
    let end = prompt.find(&format!("</{tag}>"))?;
    Some(&prompt[start..end])
}

/// Whether `prompt`'s related resources hold `text`.
fn related_holds(prompt: &str, text: &str) -> bool {
    block(prompt, "related_resources").is_some_and(|r| r.contains(text))
}

const TRACK: &str = "4uLU6hMCjMI75M1A2tKUQC";

fn contact(id: &str, email: &str) -> Value {
    json!({"id": id, "name": "Jane Roe", "email": email})
}

/// Spotify's `uris`: the id a stored track was created under, inside a URI, shows
/// that track to the add, so the model does not invent one.
#[tokio::test]
async fn an_id_inside_a_uri_shows_its_record() {
    let h = harness(vec![
        json!({"id": TRACK, "name": "Song of the Mock"}),
        json!({"effects": [], "response": {"ok": true}}),
    ]);
    h.call("POST", "/tracks", Some(json!({"name": "Song of the Mock"})))
        .await;
    let uri = format!("spotify:track:{TRACK}");
    let (status, _) = h
        .call(
            "POST",
            "/playlists/pl-000001/items",
            Some(json!({"uris": [uri], "position": 0})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let prompt = h.prompt(1);
    assert!(related_holds(&prompt, "Song of the Mock"), "{prompt}");
    let related = block(&prompt, "related_resources").unwrap();
    assert_eq!(related.matches(TRACK).count(), 1, "shown once: {prompt}");
}

/// Jira's `transition: {id: "21"}`: a short id under an identity member of an object
/// named after a stored type shows that record.
#[tokio::test]
async fn a_short_id_in_an_object_named_after_its_type_shows_its_record() {
    let h = harness(vec![
        json!({"id": "21", "name": "Done"}),
        json!({"effects": [], "response": {"ok": true}}),
    ]);
    h.call("POST", "/transitions", Some(json!({"name": "Done"})))
        .await;
    h.call("POST", "/moves", Some(json!({"transition": {"id": "21"}})))
        .await;
    let prompt = h.prompt(1);
    assert!(related_holds(&prompt, r#""name":"Done""#), "{prompt}");
}

/// Freshdesk: a ticket created with its requester's email is shown that contact, so the
/// model does not invent a `requester_id`.
#[tokio::test]
async fn a_unique_member_value_shows_its_record() {
    let h = harness(vec![
        contact("ct-000101", "jane.roe@example.com"),
        json!({"effects": [], "response": {"id": "tk-000001", "subject": "Help", "requester_id": "ct-000101"}}),
    ]);
    h.call(
        "POST",
        "/contacts",
        Some(json!({"name": "Jane Roe", "email": "jane.roe@example.com"})),
    )
    .await;
    h.call(
        "POST",
        "/tickets",
        Some(json!({"subject": "Help", "email": "jane.roe@example.com"})),
    )
    .await;
    let prompt = h.prompt(1);
    assert!(related_holds(&prompt, "ct-000101"), "{prompt}");
}

/// A plain string a record holds (a name with no digit) is kept only for the creates of
/// its own type to see what they repeat: under the same member of another type's
/// request it is no reference.
#[tokio::test]
async fn a_plain_member_value_is_no_reference() {
    let h = harness(vec![
        json!({"id": "ct-000101", "name": "jane-roe-contact", "email": "jane.roe@example.com"}),
        json!({"id": "tk-000001", "subject": "Help"}),
    ]);
    h.call(
        "POST",
        "/contacts",
        Some(json!({"name": "jane-roe-contact", "email": "jane.roe@example.com"})),
    )
    .await;
    h.call(
        "POST",
        "/tickets",
        Some(json!({"subject": "Help", "name": "jane-roe-contact"})),
    )
    .await;
    let prompt = h.prompt(1);
    assert!(block(&prompt, "related_resources").is_none(), "{prompt}");
}

/// A value two stored records hold shows neither: which one the request means is a guess.
#[tokio::test]
async fn an_ambiguous_value_shows_nothing() {
    let h = harness(vec![
        contact("ct-000101", "shared@example.com"),
        contact("ct-000102", "shared@example.com"),
        json!({"id": "tk-000001", "subject": "Help"}),
    ]);
    for _ in 0..2 {
        h.call(
            "POST",
            "/contacts",
            Some(json!({"name": "Jane Roe", "email": "shared@example.com"})),
        )
        .await;
    }
    h.call(
        "POST",
        "/tickets",
        Some(json!({"subject": "Help", "email": "shared@example.com"})),
    )
    .await;
    let prompt = h.prompt(2);
    assert!(!prompt.contains("ct-00010"), "{prompt}");
}

/// Short and trivial values match nothing: a short id outside an object named after its
/// type, a short token of a longer string, a word, an enum value the schema declares, a
/// date, and what a credential member holds.
#[tokio::test]
async fn short_and_trivial_values_show_nothing() {
    let h = harness(vec![
        json!({"id": "21", "name": "Done"}),
        json!({
            "id": "ct-000101", "name": "Jane Roe", "email": "jane.roe@example.com",
            "state": "on-hold", "joined": "2026-11-03", "api_key": "sk-test-4711abcdef",
        }),
        json!({"id": "tk-000001", "subject": "Help"}),
    ]);
    h.call("POST", "/transitions", Some(json!({"name": "Done"})))
        .await;
    h.call("POST", "/contacts", Some(json!({"name": "Jane Roe"})))
        .await;
    h.call(
        "POST",
        "/tickets",
        Some(json!({
            "subject": "Help",
            "count": "21",
            "ref": "pr-21",
            "list": {"id": "21"},
            "kind": "general",
            "status": "on-hold",
            "since": "2026-11-03",
            "secret": "sk-test-4711abcdef",
        })),
    )
    .await;
    let prompt = h.prompt(2);
    assert!(block(&prompt, "related_resources").is_none(), "{prompt}");
}

/// A record matched by value is context only: the create's answer, which embeds a changed
/// copy of it, does not rewrite it, and its next read costs no generation and answers it
/// as stored.
#[tokio::test]
async fn a_value_matched_record_is_not_rewritten_by_the_answer() {
    let h = harness(vec![
        contact("ct-000101", "jane.roe@example.com"),
        json!({"effects": [], "response": {
            "id": "tk-000001", "subject": "Help", "requester_id": "ct-000101",
            "requester": {"id": "ct-000101", "name": "Someone Else", "email": "jane.roe@example.com"},
        }}),
    ]);
    h.call(
        "POST",
        "/contacts",
        Some(json!({"name": "Jane Roe", "email": "jane.roe@example.com"})),
    )
    .await;
    let stored = h.held("contacts", "ct-000101").await;
    h.call(
        "POST",
        "/tickets",
        Some(json!({"subject": "Help", "email": "jane.roe@example.com"})),
    )
    .await;
    assert!(related_holds(&h.prompt(1), "ct-000101"), "{}", h.prompt(1));
    assert_eq!(h.held("contacts", "ct-000101").await, stored);
    let (status, body) = h.call("GET", "/contacts/ct-000101", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Jane Roe");
    assert_eq!(h.calls(), 2, "the read is answered from the store");
}

/// A ticket's envelope answer, as a create shown related records gets.
fn ticket(id: &str, extra: Value) -> Value {
    let mut answer = json!({"id": id, "subject": "Help"});
    if let (Value::Object(answer), Value::Object(extra)) = (&mut answer, extra) {
        answer.extend(extra);
    }
    json!({"effects": [], "response": answer})
}

async fn create_contact(h: &Harness, session: Option<&str>) {
    h.call_in(
        session,
        "POST",
        "/contacts",
        Some(json!({"name": "Jane Roe", "email": "jane.roe@example.com"})),
    )
    .await;
}

async fn create_ticket(h: &Harness, session: Option<&str>, body: Value) {
    h.call_in(session, "POST", "/tickets", Some(body)).await;
}

/// A common value (a time zone) a contact holds matches only a request member of the
/// same name: under another name it is no reference to the contact.
#[tokio::test]
async fn a_common_value_under_another_name_shows_nothing() {
    let h = harness(vec![
        json!({
            "id": "ct-000101", "name": "Jane Roe", "email": "jane.roe@example.com",
            "timezone": "America/New_York",
        }),
        json!({"id": "tk-000001", "subject": "Help"}),
    ]);
    create_contact(&h, None).await;
    create_ticket(
        &h,
        None,
        json!({"subject": "Help", "zone": "America/New_York", "contact": "jane.roe@example.com"}),
    )
    .await;
    let prompt = h.prompt(1);
    assert!(block(&prompt, "related_resources").is_none(), "{prompt}");
}

/// A member value matches under a request member whose words name the stored member.
#[tokio::test]
async fn a_member_value_under_a_name_naming_its_member_shows_its_record() {
    let h = harness(vec![
        contact("ct-000101", "jane.roe@example.com"),
        ticket("tk-000001", json!({})),
    ]);
    create_contact(&h, None).await;
    create_ticket(
        &h,
        None,
        json!({"subject": "Help", "requester_email": "jane.roe@example.com"}),
    )
    .await;
    assert!(related_holds(&h.prompt(1), "ct-000101"), "{}", h.prompt(1));
}

/// The ticket the first create stored holds the email too, but it is a sibling of the
/// second, not what its email names: the contact is still shown.
#[tokio::test]
async fn a_second_create_still_shows_the_record_its_value_names() {
    let h = harness(vec![
        contact("ct-000101", "jane.roe@example.com"),
        ticket("tk-000001", json!({"email": "jane.roe@example.com"})),
        ticket("tk-000002", json!({"email": "jane.roe@example.com"})),
    ]);
    create_contact(&h, None).await;
    for _ in 0..2 {
        create_ticket(
            &h,
            None,
            json!({"subject": "Help", "email": "jane.roe@example.com"}),
        )
        .await;
    }
    let prompt = h.prompt(2);
    assert!(related_holds(&prompt, "ct-000101"), "{prompt}");
    assert!(!related_holds(&prompt, "tk-000001"), "{prompt}");
}

/// A value the id lookup already resolves to a record names that record: another that
/// only holds it in a member (a ticket's `owner`) is not shown beside it.
#[tokio::test]
async fn a_sibling_sharing_a_held_id_is_not_shown() {
    let h = harness(vec![
        json!({"id": "tk-000001", "subject": "Help", "owner": "ct-000101"}),
        json!({"effects": [], "response": {"ok": true}}),
    ]);
    // Stored by the Admin API, say: not through this runtime.
    h.store
        .upsert(
            &session(),
            StoredResource {
                resource_type: "contacts".into(),
                id: "ct-000101".into(),
                data: contact("ct-000101", "jane.roe@example.com"),
            },
        )
        .await
        .unwrap();
    create_ticket(&h, None, json!({"subject": "Help"})).await;
    h.call("POST", "/moves", Some(json!({"owner": "ct-000101"})))
        .await;
    let prompt = h.prompt(1);
    assert!(related_holds(&prompt, "Jane Roe"), "{prompt}");
    assert!(!related_holds(&prompt, "tk-000001"), "{prompt}");
}

/// With a store other nodes write to, a member value the index finds is checked against
/// the store: unique there, it matches; once another node stored a contact with the same
/// email, it names neither. An id is unique by construction and matches as it is.
#[tokio::test]
async fn a_shared_store_matches_member_values_the_store_shows_unique() {
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let shared = Arc::new(Shared(Arc::clone(&store))) as Arc<dyn StateStore>;
    let h = harness_on(
        shared,
        Arc::clone(&store),
        vec![
            contact("ct-000101", "jane.roe@example.com"),
            json!({"id": "tk-000001", "subject": "Help"}),
            json!({"id": "tk-000002", "subject": "Help"}),
            json!({"id": TRACK, "name": "Song of the Mock"}),
            json!({"effects": [], "response": {"ok": true}}),
        ],
    );
    let asks = json!({"subject": "Help", "email": "jane.roe@example.com"});
    create_contact(&h, None).await;
    create_ticket(&h, None, asks.clone()).await;
    assert!(related_holds(&h.prompt(1), "ct-000101"), "{}", h.prompt(1));
    // Another node stores a contact with the same email: this one cannot see it written.
    let twin = json!({"id": "ct-000202", "name": "J. Roe", "email": "jane.roe@example.com"});
    store
        .upsert(
            &session(),
            StoredResource {
                resource_type: "contacts".into(),
                id: "ct-000202".into(),
                data: twin,
            },
        )
        .await
        .unwrap();
    create_ticket(&h, None, asks).await;
    assert!(
        block(&h.prompt(2), "related_resources").is_none_or(|r| !r.contains("ct-000")),
        "{}",
        h.prompt(2)
    );
    h.call("POST", "/tracks", Some(json!({"name": "Song of the Mock"})))
        .await;
    h.call(
        "POST",
        "/playlists/pl-000001/items",
        Some(json!({"uris": [format!("spotify:track:{TRACK}")]})),
    )
    .await;
    assert!(
        related_holds(&h.prompt(4), "Song of the Mock"),
        "{}",
        h.prompt(4)
    );
}

/// A reset session's records are gone: they neither match nor make a value ambiguous.
#[tokio::test]
async fn a_reset_session_leaves_nothing_behind() {
    let h = harness(vec![
        contact("ct-000101", "jane.roe@example.com"),
        contact("ct-000202", "jane.roe@example.com"),
        ticket("tk-000001", json!({})),
    ]);
    create_contact(&h, None).await;
    h.store.reset(&session()).await.unwrap();
    create_contact(&h, None).await;
    create_ticket(
        &h,
        None,
        json!({"subject": "Help", "email": "jane.roe@example.com"}),
    )
    .await;
    let prompt = h.prompt(2);
    assert!(related_holds(&prompt, "ct-000202"), "{prompt}");
    assert!(!prompt.contains("ct-000101"), "{prompt}");
}

/// A record deleted where this runtime did not see it (the Admin API) no longer makes
/// its email ambiguous.
#[tokio::test]
async fn a_deleted_record_no_longer_counts() {
    let h = harness(vec![
        contact("ct-000101", "jane.roe@example.com"),
        contact("ct-000202", "jane.roe@example.com"),
        ticket("tk-000001", json!({})),
    ]);
    create_contact(&h, None).await;
    create_contact(&h, None).await;
    h.store
        .delete(&session(), "contacts", "ct-000101")
        .await
        .unwrap();
    create_ticket(
        &h,
        None,
        json!({"subject": "Help", "email": "jane.roe@example.com"}),
    )
    .await;
    let prompt = h.prompt(2);
    assert!(related_holds(&prompt, "ct-000202"), "{prompt}");
}

/// An updated record is indexed as it now stands: its old email names nothing, its new
/// one names it.
#[tokio::test]
async fn an_updated_record_matches_its_new_value_only() {
    // The update is the state-first executor's: no answer of the model's.
    let h = harness(vec![
        contact("ct-000101", "old.mail@example.com"),
        json!({"id": "tk-000001", "subject": "Help"}),
        ticket("tk-000002", json!({})),
    ]);
    h.call(
        "POST",
        "/contacts",
        Some(json!({"name": "Jane Roe", "email": "old.mail@example.com"})),
    )
    .await;
    h.call(
        "PUT",
        "/contacts/ct-000101",
        Some(contact("ct-000101", "new.mail@example.com")),
    )
    .await;
    assert_eq!(
        h.held("contacts", "ct-000101").await["email"],
        "new.mail@example.com"
    );
    let first = h.calls();
    create_ticket(
        &h,
        None,
        json!({"subject": "Help", "email": "old.mail@example.com"}),
    )
    .await;
    let prompt = h.prompt(first);
    assert!(!prompt.contains("ct-000101"), "{prompt}");
    create_ticket(
        &h,
        None,
        json!({"subject": "Help", "email": "new.mail@example.com"}),
    )
    .await;
    let prompt = h.prompt(first + 1);
    assert!(related_holds(&prompt, "ct-000101"), "{prompt}");
}

/// One session's records never match in another.
#[tokio::test]
async fn sessions_do_not_see_each_others_values() {
    let h = harness(vec![
        contact("ct-000101", "jane.roe@example.com"),
        json!({"id": "tk-000001", "subject": "Help"}),
        ticket("tk-000002", json!({})),
    ]);
    create_contact(&h, Some("a")).await;
    let body = json!({"subject": "Help", "email": "jane.roe@example.com"});
    create_ticket(&h, Some("b"), body.clone()).await;
    assert!(!h.prompt(1).contains("ct-000101"), "{}", h.prompt(1));
    create_ticket(&h, Some("a"), body).await;
    assert!(related_holds(&h.prompt(2), "ct-000101"), "{}", h.prompt(2));
}

/// A common value under the very member it is stored in still names nothing: a time zone
/// or a status word is no identifier, however unique it happens to be.
#[tokio::test]
async fn common_values_under_the_same_name_show_nothing() {
    for (member, value) in [("timezone", "America/New_York"), ("status", "in_progress")] {
        let mut stored = contact("ct-000101", "jane.roe@example.com");
        stored[member] = json!(value);
        let h = harness(vec![stored, json!({"id": "tk-000001", "subject": "Help"})]);
        create_contact(&h, None).await;
        let mut body = json!({"subject": "Help"});
        body[member] = json!(value);
        create_ticket(&h, None, body).await;
        let prompt = h.prompt(1);
        assert!(
            block(&prompt, "related_resources").is_none(),
            "{member}: {prompt}"
        );
    }
}

/// A read the store failed says nothing of the record: the index keeps it, and the next
/// request finds it.
#[tokio::test]
async fn a_failed_read_back_forgets_nothing() {
    let (h, store) = flaky(vec![
        contact("ct-000101", "jane.roe@example.com"),
        json!({"id": "tk-000001", "subject": "Help"}),
        ticket("tk-000002", json!({})),
    ]);
    create_contact(&h, None).await;
    store.fail_next_read_of("contacts", "ct-000101");
    let body = json!({"subject": "Help", "email": "jane.roe@example.com"});
    create_ticket(&h, None, body.clone()).await;
    assert!(!h.prompt(1).contains("ct-000101"), "{}", h.prompt(1));
    create_ticket(&h, None, body).await;
    assert!(related_holds(&h.prompt(2), "ct-000101"), "{}", h.prompt(2));
}

/// A record the id lookup already showed costs the index no read of its own.
#[tokio::test]
async fn a_record_in_hand_is_not_read_back() {
    let (h, store) = flaky(vec![
        contact("ct-000101", "jane.roe@example.com"),
        json!({"effects": [], "response": {"ok": true}}),
    ]);
    create_contact(&h, None).await;
    let before = store.reads_of("contacts", "ct-000101");
    h.call("POST", "/moves", Some(json!({"subject": "ct-000101"})))
        .await;
    assert!(related_holds(&h.prompt(1), "Jane Roe"), "{}", h.prompt(1));
    assert_eq!(store.reads_of("contacts", "ct-000101") - before, 1);
}

/// A delete the store reports, made where this runtime did not see it, is dropped from
/// the index at once: even past the holders a request reads back, the one record left
/// is shown.
#[tokio::test]
async fn a_reported_delete_is_dropped_from_the_index() {
    let mut script: Vec<Value> = (1..=5)
        .map(|n| contact(&format!("ct-00010{n}"), "jane.roe@example.com"))
        .collect();
    script.push(ticket("tk-000001", json!({})));
    let h = harness(script);
    for _ in 1..=5 {
        create_contact(&h, None).await;
    }
    for n in 1..=4 {
        h.store
            .delete(&session(), "contacts", &format!("ct-00010{n}"))
            .await
            .unwrap();
    }
    // The feed is delivered on its own task.
    tokio::time::sleep(Duration::from_millis(50)).await;
    create_ticket(
        &h,
        None,
        json!({"subject": "Help", "email": "jane.roe@example.com"}),
    )
    .await;
    let prompt = h.prompt(5);
    assert!(related_holds(&prompt, "ct-000105"), "{prompt}");
}
