//! A resource's identity, where the contract's names do not give it: what a running
//! mock has seen says which member a path goes by. Driven over HTTP with a scripted
//! LLM, asserting what is served, what is stored and how many calls it took.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{CachingStateStore, INTERNAL_TYPE, MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Nothing here says `ref` is what `/things/{id}` goes by.
const THINGS: &str = r##"
openapi: 3.0.0
info: {title: Things, version: "1"}
paths:
  /things:
    get:
      operationId: listThings
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Thing"}}}}}}
    post:
      operationId: createThing
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
  /things/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getThing
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
    patch:
      operationId: patchThing
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
    delete:
      operationId: deleteThing
      responses: {"204": {description: gone}}
  /things/{id}/close:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: closeThing
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"204": {description: closed}}
  /things-by-name:
    get:
      operationId: thingsByName
      parameters: [{name: name, in: query, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Thing"}}}}}}
  /parents/{parent}/kids:
    parameters: [{name: parent, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createKid
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Kid"}}}}}
  /parents/{parent}/kids/{id}:
    parameters:
      - {name: parent, in: path, required: true, schema: {type: string}}
      - {name: id, in: path, required: true, schema: {type: string}}
    get:
      operationId: getKid
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Kid"}}}}}
  /grants:
    post:
      operationId: createGrant
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {code: {type: string}}}}}}}
  /shorts:
    post:
      operationId: createShort
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {type: object, properties: {ref: {type: string}}}}}}}
  /shorts/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getShort
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Short"}}}}}
components:
  schemas:
    Thing:
      type: object
      properties:
        ref: {type: string}
        name: {type: string}
        color: {type: string}
        state: {type: string, enum: [open, closed]}
    Kid:
      type: object
      properties:
        ref: {type: string}
        name: {type: string}
        color: {type: string}
    Short:
      type: object
      properties:
        ref: {type: string}
        color: {type: string}
"##;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<MemoryStore>,
}

fn session(name: &str) -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: name.into(),
    }
}

fn router_on(
    contract: &[u8],
    provider: &Arc<ScriptedProvider>,
    store: &Arc<MemoryStore>,
) -> axum::Router {
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        CachingStateStore::wrap(
            Arc::clone(store) as Arc<dyn StateStore>,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
    );
    http_rest::router(Arc::new(rt))
}

fn harness(contract: &[u8], script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    Harness {
        router: router_on(contract, &provider, &store),
        provider,
        store,
    }
}

impl Harness {
    async fn in_session(
        &self,
        name: &str,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Value) {
        send(&self.router, name, method, uri, body).await
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Value) {
        self.in_session("default", method, uri, body).await
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    /// Every prompt so far, as one text.
    fn prompts(&self) -> String {
        format!("{:?}", self.provider.requests())
    }

    /// What an export of the session lists, as (type, id).
    async fn exported(&self, name: &str) -> Vec<(String, String)> {
        let resources = self.store.export(&session(name)).await.unwrap();
        resources
            .into_iter()
            .map(|r| (r.resource_type, r.id))
            .collect()
    }

    /// The ids of the session's reserved documents.
    async fn internal(&self, name: &str) -> Vec<String> {
        let docs = self
            .store
            .list(&session(name), INTERNAL_TYPE, 100)
            .await
            .unwrap();
        docs.into_iter().map(|r| r.id).collect()
    }
}

async fn send(
    router: &axum::Router,
    name: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-mockingbird-session", name);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let req = req
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let source = resp
        .headers()
        .get("x-mockingbird-source")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        source,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn thing(reference: &str, name: &str) -> Value {
    json!({"ref": reference, "name": name, "color": "red", "state": "open"})
}

const NO_NAME: Option<Value> = None;

/// One user, on every route of the contract that says what it is by `user_id`.
#[tokio::test]
async fn one_auth0_user_on_every_route() {
    let contract = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/apis/auth0-management/auth0-management.openapi.yaml"
    ))
    .unwrap();
    let id = "auth0|0123456789abcdef01234567";
    let path = "/api/v2/users/auth0%7C0123456789abcdef01234567";
    let user = |nickname: &str| {
        json!({
            "user_id": id,
            "email": "ann@example.com",
            "email_verified": false,
            "name": "Ann",
            "nickname": nickname,
            "picture": "https://example.com/a.png",
            "identities": [{
                "connection": "Username-Password-Authentication",
                "user_id": "0123456789abcdef01234567",
                "provider": "auth0",
                "isSocial": false
            }],
            "created_at": "2026-01-01T00:00:00.000Z",
            "updated_at": "2026-01-01T00:00:00.000Z",
            "logins_count": 0
        })
    };
    let h = harness(
        &contract,
        vec![user("ann"), user("annie"), json!([user("annie")])],
    );

    let create = json!({"connection": "Username-Password-Authentication", "email": "ann@example.com", "name": "Ann"});
    let (status, _, created) = h.call("POST", "/api/v2/users", Some(create)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["user_id"], id);
    assert_eq!(
        h.exported("default").await,
        [("users".to_owned(), id.to_owned())]
    );

    let (status, source, read) = h.call("GET", path, None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read["nickname"], "ann");
    assert_eq!(h.calls(), 1);

    let (status, _, patched) = h
        .call("PATCH", path, Some(json!({"nickname": "annie"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    let (_, source, read) = h.call("GET", path, None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["nickname"], "annie");
    assert_eq!(h.calls(), 2);

    // The lookup is shown the stored user, of the type it reads.
    let (status, _, found) = h
        .call(
            "GET",
            "/api/v2/users-by-email?email=ann%40example.com",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found[0]["user_id"], id);
    assert_eq!(h.calls(), 3);
    let lookup = format!("{:?}", h.provider.requests()[2]);
    assert!(lookup.contains("annie"), "{lookup}");
    assert_eq!(
        h.exported("default").await,
        [("users".to_owned(), id.to_owned())]
    );

    let (status, _, _) = h.call("DELETE", path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, gone) = h.call("GET", path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    assert!(h.exported("default").await.is_empty());
}

/// Create, then every route: the first path value teaches the member.
#[tokio::test]
async fn a_path_value_teaches_the_member_and_everything_then_acts_on_one_record() {
    let h = harness(
        THINGS.as_bytes(),
        vec![
            thing("r-1", "one"),
            thing("r-1", "renamed"),
            thing("r-2", "two"),
        ],
    );
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    // Held, under a key no one is shown.
    assert!(h.exported("default").await.is_empty());

    let (status, source, read) = h.call("GET", "/things/r-1", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read, thing("r-1", "one"));
    assert_eq!(h.calls(), 1);
    assert_eq!(
        h.exported("default").await,
        [("things".to_owned(), "r-1".to_owned())]
    );

    let (status, _, patched) = h
        .call("PATCH", "/things/r-1", Some(json!({"name": "renamed"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    let (_, source, read) = h.call("GET", "/things/r-1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["name"], "renamed");
    assert_eq!(
        h.exported("default").await,
        [("things".to_owned(), "r-1".to_owned())]
    );

    // Known now: the next create goes under it at once.
    h.call("POST", "/things", Some(json!({"name": "two"})))
        .await;
    let mut held = h.exported("default").await;
    held.sort();
    assert_eq!(
        held,
        [
            ("things".to_owned(), "r-1".to_owned()),
            ("things".to_owned(), "r-2".to_owned())
        ]
    );

    let (status, _, _) = h.call("DELETE", "/things/r-1", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, gone) = h.call("GET", "/things/r-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    let (_, source, kept) = h.call("GET", "/things/r-2", None).await;
    assert_eq!(source, "state", "{kept}");
}

/// A delete is the first path value: it still finds what it names.
#[tokio::test]
async fn a_delete_can_teach_the_member() {
    let h = harness(THINGS.as_bytes(), vec![thing("r-1", "one")]);
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    let (status, _, _) = h.call("DELETE", "/things/r-1", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.exported("default").await.is_empty());
    let (status, _, gone) = h.call("GET", "/things/r-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    assert!(
        !h.internal("default")
            .await
            .iter()
            .any(|id| id.starts_with("#pending"))
    );
}

/// The key a record is held under reaches no prompt, answer or export.
#[tokio::test]
async fn the_held_key_is_shown_to_no_one() {
    let h = harness(
        THINGS.as_bytes(),
        vec![
            thing("r-1", "one"),
            json!([thing("r-1", "one")]),
            json!([thing("r-1", "one")]),
        ],
    );
    let (_, _, created) = h
        .call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    let (_, _, listed) = h.call("GET", "/things", None).await;
    let (_, _, found) = h.call("GET", "/things-by-name?name=one", None).await;
    let held = h.internal("default").await;
    let key = held
        .iter()
        .find(|id| id.starts_with("#pending"))
        .unwrap()
        .clone();
    for shown in [
        created.to_string(),
        listed.to_string(),
        found.to_string(),
        h.prompts(),
    ] {
        assert!(!shown.contains(&key), "{shown}");
        assert!(!shown.contains("#pending"), "{shown}");
    }
    assert!(h.exported("default").await.is_empty());
    // But a lookup and a list are shown the record.
    for request in &h.provider.requests()[1..] {
        assert!(format!("{request:?}").contains("r-1"), "{request:?}");
    }
}

/// Two records holding the value say nothing of which member is meant.
#[tokio::test]
async fn a_value_two_records_hold_teaches_nothing() {
    let h = harness(
        THINGS.as_bytes(),
        vec![
            thing("r-1", "one"),
            thing("r-2", "two"),
            thing("x", "generated"),
        ],
    );
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    h.call("POST", "/things", Some(json!({"name": "two"})))
        .await;
    let (status, source, _) = h.call("GET", "/things/red", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(source, "state");
    assert_eq!(h.calls(), 3);
    assert!(
        !h.internal("default")
            .await
            .iter()
            .any(|id| id.starts_with("#identity"))
    );

    let (_, source, read) = h.call("GET", "/things/r-2", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["name"], "two");
    let mut held = h.exported("default").await;
    held.sort();
    assert_eq!(
        held,
        [
            ("things".to_owned(), "r-1".to_owned()),
            ("things".to_owned(), "r-2".to_owned())
        ]
    );
}

/// One record holding the value in two members does not say which.
#[tokio::test]
async fn a_value_in_two_members_teaches_nothing() {
    let h = harness(
        THINGS.as_bytes(),
        vec![
            json!({"ref": "same", "name": "n", "color": "same", "state": "open"}),
            thing("x", "generated"),
        ],
    );
    h.call("POST", "/things", Some(json!({"name": "n"}))).await;
    let (_, source, _) = h.call("GET", "/things/same", None).await;
    assert_ne!(source, "state");
    assert!(h.exported("default").await.is_empty());
}

/// What the client itself said is no sign of a key: `name` came from the request.
#[tokio::test]
async fn a_value_the_request_said_teaches_nothing() {
    let h = harness(
        THINGS.as_bytes(),
        vec![thing("r-1", "one"), thing("x", "generated")],
    );
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    let (status, source, _) = h.call("GET", "/things/one", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(source, "state");
    assert!(h.exported("default").await.is_empty());

    let (_, source, read) = h.call("GET", "/things/r-1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(
        h.exported("default").await,
        [("things".to_owned(), "r-1".to_owned())]
    );
}

/// A member with a fixed set of values cannot be a key, whatever a path says.
#[tokio::test]
async fn an_enumerated_member_teaches_nothing() {
    let h = harness(
        THINGS.as_bytes(),
        vec![thing("r-1", "one"), thing("x", "generated")],
    );
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    let (_, source, _) = h.call("GET", "/things/open", None).await;
    assert_ne!(source, "state");
    assert!(h.exported("default").await.is_empty());
}

/// A server-set member that happened to match goes when the real key shows: the
/// record moves, and the member cannot come back.
#[tokio::test]
async fn a_member_taken_by_mistake_gives_way_to_the_real_one() {
    let h = harness(
        THINGS.as_bytes(),
        vec![thing("r-1", "one"), thing("r-2", "two")],
    );
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    let (_, source, _) = h.call("GET", "/things/red", None).await;
    assert_eq!(source, "state");
    assert_eq!(
        h.exported("default").await,
        [("things".to_owned(), "red".to_owned())]
    );

    let (_, source, read) = h.call("GET", "/things/r-1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["ref"], "r-1");
    assert_eq!(
        h.exported("default").await,
        [("things".to_owned(), "r-1".to_owned())]
    );

    h.call("POST", "/things", Some(json!({"name": "two"})))
        .await;
    let (status, source, _) = h.call("GET", "/things/red", None).await;
    assert_ne!((status, source.as_str()), (StatusCode::OK, "state"));
    let mut held = h.exported("default").await;
    held.sort();
    assert_eq!(held[1], ("things".to_owned(), "r-2".to_owned()));
}

/// A create's answer that did not show its request: what was remembered for the held
/// record goes with it, so the first read catches up.
#[tokio::test]
async fn rekeying_carries_the_memory() {
    let h = harness(
        THINGS.as_bytes(),
        vec![
            json!({"ref": "r-1"}),
            json!({"ref": "r-1", "color": "blue"}),
        ],
    );
    h.call("POST", "/shorts", Some(json!({"color": "blue"})))
        .await;
    let before = h.internal("default").await;
    assert!(
        before.iter().any(|id| id.starts_with("shorts/#pending")),
        "{before:?}"
    );

    let (status, _, read) = h.call("GET", "/shorts/r-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["color"], "blue", "{read}");
    assert_eq!(h.calls(), 2);
    let told = format!("{:?}", h.provider.requests()[1]);
    assert!(told.contains("blue"), "{told}");
    let after = h.internal("default").await;
    assert!(!after.iter().any(|id| id.contains("#pending")), "{after:?}");
    let (_, source, read) = h.call("GET", "/shorts/r-1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["color"], "blue");
    assert_eq!(h.calls(), 2);
}

/// An action on the value before anything was learned is told to the read after.
#[tokio::test]
async fn rekeying_carries_what_a_value_was_told_before_it_was_a_record() {
    let h = harness(
        THINGS.as_bytes(),
        vec![
            thing("r-1", "one"),
            json!({"ref": "r-1", "name": "one", "color": "red", "state": "closed"}),
        ],
    );
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    let (status, _, _) = h
        .call("POST", "/things/r-1/close", Some(json!({"reason": "done"})))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, read) = h.call("GET", "/things/r-1", None).await;
    assert_eq!(read["state"], "closed", "{read}");
    assert_eq!(h.calls(), 2);
    assert!(format!("{:?}", h.provider.requests()[1]).contains("done"));
    let after = h.internal("default").await;
    assert!(
        !after.iter().any(|id| id.starts_with("things/#")),
        "{after:?}"
    );
}

/// A marker for the value goes when a record comes to hold it, as a create's does.
#[tokio::test]
async fn rekeying_ends_a_delete_marker_and_a_delete_after_sets_one() {
    let h = harness(THINGS.as_bytes(), vec![thing("r-1", "one")]);
    let (status, _, _) = h.call("DELETE", "/things/r-1", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;

    let (status, source, read) = h.call("GET", "/things/r-1", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );

    let (status, _, _) = h.call("DELETE", "/things/r-1", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, gone) = h.call("GET", "/things/r-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
}

/// One session's learning is no other's, and a reset ends it.
#[tokio::test]
async fn sessions_learn_apart_and_a_reset_forgets() {
    let h = harness(
        THINGS.as_bytes(),
        vec![
            thing("r-1", "a"),
            json!({"ref": "other", "name": "b", "color": "r-1", "state": "open"}),
            thing("x", "generated"),
            thing("r-3", "c"),
        ],
    );
    h.in_session("a", "POST", "/things", Some(json!({"name": "a"})))
        .await;
    h.in_session("b", "POST", "/things", Some(json!({"name": "b"})))
        .await;
    let (_, source, _) = h.in_session("a", "GET", "/things/r-1", None).await;
    assert_eq!(source, "state");
    assert_eq!(
        h.exported("a").await,
        [("things".to_owned(), "r-1".to_owned())]
    );
    // Session b's record is still held, and learns its own member from its own value.
    assert!(h.exported("b").await.is_empty());
    let (_, source, read) = h.in_session("b", "GET", "/things/other", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(
        h.exported("b").await,
        [("things".to_owned(), "other".to_owned())]
    );

    // Session c has nothing, whatever a and b learned.
    let (_, source, _) = h.in_session("c", "GET", "/things/r-1", None).await;
    assert_ne!(source, "state");

    h.store.reset(&session("a")).await.unwrap();
    assert!(h.internal("a").await.is_empty());
    h.in_session("a", "POST", "/things", Some(json!({"name": "c"})))
        .await;
    assert!(
        h.exported("a").await.is_empty(),
        "the member is to be learned anew"
    );
}

/// What one node learned, another knows: it lives in the shared store.
#[tokio::test]
async fn another_node_knows_what_one_learned() {
    let h = harness(
        THINGS.as_bytes(),
        vec![thing("r-1", "one"), thing("r-2", "two")],
    );
    let other = router_on(THINGS.as_bytes(), &h.provider, &h.store);
    // Held on this node, found on the other.
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    let (_, source, read) = send(&other, "default", "GET", "/things/r-1", None).await;
    assert_eq!(source, "state", "{read}");
    // Learned there, known here: this create goes straight under it.
    h.call("POST", "/things", Some(json!({"name": "two"})))
        .await;
    let mut held = h.exported("default").await;
    held.sort();
    assert_eq!(held[1], ("things".to_owned(), "r-2".to_owned()));
}

/// With no route by path nothing could find it again, so nothing is kept.
#[tokio::test]
async fn an_answer_no_route_reads_is_not_held() {
    let h = harness(THINGS.as_bytes(), vec![json!({"code": "c-1"})]);
    let (status, _, _) = h.call("POST", "/grants", NO_NAME).await;
    assert_eq!(status, StatusCode::OK);
    assert!(h.internal("default").await.is_empty());
    assert!(h.exported("default").await.is_empty());
}

/// A value held only under another parent is not this path's.
#[tokio::test]
async fn a_value_under_another_parent_teaches_nothing() {
    let h = harness(
        THINGS.as_bytes(),
        vec![
            thing("r-1", "kid"),
            thing("x", "generated"),
            thing("r-1", "other"),
        ],
    );
    h.call("POST", "/parents/p1/kids", Some(json!({"name": "kid"})))
        .await;
    let (_, source, _) = h.call("GET", "/parents/p2/kids/r-1", None).await;
    assert_ne!(source, "state");
    assert!(h.exported("default").await.is_empty());
    let (_, source, read) = h.call("GET", "/parents/p1/kids/r-1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["name"], "kid");
}

/// An envelope or an array names no member to learn: such a type is as it was, with
/// the create's own member stored as a list would see it.
#[tokio::test]
async fn an_enveloped_or_array_answer_is_left_as_it_was() {
    let contract = r##"
openapi: 3.0.0
info: {title: Wrapped, version: "1"}
paths:
  /tickets:
    get:
      operationId: listTickets
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {tickets: {type: array, items: {$ref: "#/components/schemas/Ticket"}}}}}}}}
    post:
      operationId: createTicket
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Wrapped"}}}}}
  /tickets/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTicket
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Wrapped"}}}}}
  /bounces/{email}:
    parameters: [{name: email, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBounce
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {type: object, properties: {email: {type: string}}}}}}}}
components:
  schemas:
    Ticket: {type: object, properties: {id: {type: string}, subject: {type: string}}}
    Wrapped: {type: object, properties: {ticket: {$ref: "#/components/schemas/Ticket"}}}
"##;
    let wrapped = json!({"ticket": {"id": "t-1", "subject": "s"}});
    let h = harness(contract.as_bytes(), vec![wrapped.clone(), wrapped]);
    h.call("POST", "/tickets", Some(json!({"subject": "s"})))
        .await;
    assert_eq!(
        h.exported("default").await,
        [("tickets".to_owned(), "t-1".to_owned())]
    );
    let (_, source, _) = h.call("GET", "/tickets/t-1", None).await;
    assert_eq!(source, "llm");
    let internal = h.internal("default").await;
    assert!(
        !internal.iter().any(|id| id.starts_with('#')),
        "{internal:?}"
    );
}

/// With consistency off nothing is held or learned.
#[tokio::test]
async fn consistency_off_holds_and_learns_nothing() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        Ok(thing("r-1", "one").to_string()),
        Ok(thing("r-1", "generated").to_string()),
    ]));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(compile(THINGS.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store) as Arc<dyn StateStore>,
    );
    rt.consistency = false;
    let router = http_rest::router(Arc::new(rt));
    send(
        &router,
        "default",
        "POST",
        "/things",
        Some(json!({"name": "one"})),
    )
    .await;
    let (_, source, read) = send(&router, "default", "GET", "/things/r-1", None).await;
    assert_eq!(source, "llm", "{read}");
    let internal = store
        .list(&session("default"), INTERNAL_TYPE, 10)
        .await
        .unwrap();
    assert!(internal.is_empty(), "{internal:?}");
}

/// A contract whose `id` already works: same calls, same store, nothing held or learned.
#[tokio::test]
async fn a_contract_with_an_id_is_as_it_was() {
    let contract = THINGS.replace("ref: {type: string}", "id: {type: string}");
    let widget = |name: &str| json!({"id": "w-1", "name": name, "color": "red", "state": "open"});
    let h = harness(
        contract.as_bytes(),
        vec![widget("one"), widget("two"), json!([widget("two")])],
    );
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    assert_eq!(
        h.exported("default").await,
        [("things".to_owned(), "w-1".to_owned())]
    );
    let (_, source, _) = h.call("GET", "/things/w-1", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 1);
    h.call("PATCH", "/things/w-1", Some(json!({"name": "two"})))
        .await;
    let (_, source, read) = h.call("GET", "/things/w-1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["name"], "two");
    assert_eq!(h.calls(), 2);
    h.call("GET", "/things", None).await;
    assert_eq!(h.calls(), 3);
    h.call("DELETE", "/things/w-1", None).await;
    let (status, _, _) = h.call("GET", "/things/w-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let internal = h.internal("default").await;
    assert!(
        !internal
            .iter()
            .any(|id| id.starts_with("#identity") || id.contains("#pending")),
        "{internal:?}"
    );
}
