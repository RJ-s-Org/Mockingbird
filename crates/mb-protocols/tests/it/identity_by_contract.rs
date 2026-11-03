//! A resource's identity where only the contract's names give it, and a lookup that
//! reads as its items' type. Driven over HTTP with a scripted LLM, asserting what is
//! served, what is stored and how many calls it took.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, UnknownIds, http_rest};
use mb_state::{CachingStateStore, MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<MemoryStore>,
}

fn harness_with(
    contract: &[u8],
    script: Vec<Value>,
    change: impl FnOnce(&mut MockRuntime),
) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        CachingStateStore::wrap(
            Arc::clone(&store) as Arc<dyn StateStore>,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
    );
    change(&mut rt);
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

fn harness(contract: &[u8], script: Vec<Value>) -> Harness {
    harness_with(contract, script, |_| {})
}

impl Harness {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Value) {
        let mut req = Request::builder().method(method).uri(uri);
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
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            source,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    /// What an export of the session lists, as (type, id).
    async fn exported(&self) -> Vec<(String, String)> {
        let session = SessionKey {
            workspace: "default".into(),
            mock: "m".into(),
            session: "default".into(),
        };
        let resources = self.store.export(&session).await.unwrap();
        resources
            .into_iter()
            .map(|r| (r.resource_type, r.id))
            .collect()
    }
}

/// `/{resource}` and `/{resource}/{param}` over `{member, name}`.
fn by_path(resource: &str, param: &str, member: &str) -> String {
    format!(
        r##"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /{resource}:
    post:
      operationId: create
      requestBody: {{content: {{application/json: {{schema: {{type: object}}}}}}}}
      responses: {{"201": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/R"}}}}}}}}}}
  /{resource}/{{{param}}}:
    parameters: [{{name: {param}, in: path, required: true, schema: {{type: string}}}}]
    get:
      operationId: read
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/R"}}}}}}}}}}
    patch:
      operationId: patch
      requestBody: {{content: {{application/json: {{schema: {{type: object}}}}}}}}
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/R"}}}}}}}}}}
    delete:
      operationId: remove
      responses: {{"204": {{description: gone}}}}
components:
  schemas:
    R:
      type: object
      properties:
        {member}: {{type: string}}
        name: {{type: string}}
"##
    )
}

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
    assert_eq!(h.exported().await, [("users".to_owned(), id.to_owned())]);

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
    assert_eq!(h.exported().await, [("users".to_owned(), id.to_owned())]);

    let (status, _, _) = h.call("DELETE", path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, gone) = h.call("GET", path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    assert!(h.exported().await.is_empty());
}

/// `user_id` is not what `{handle}` names, so no answer has it rewritten to the path's
/// value, and the type has no identity.
#[tokio::test]
async fn a_handle_does_not_rewrite_the_type_named_member() {
    let contract = by_path("users", "handle", "user_id");
    let user = |name: &str| json!({"user_id": "u_123", "name": name});
    let h = harness(contract.as_bytes(), vec![user("Ann"), user("Annie")]);
    let (status, _, read) = h.call("GET", "/users/alice", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["user_id"], "u_123");
    let (_, _, patched) = h
        .call("PATCH", "/users/alice", Some(json!({"name": "Annie"})))
        .await;
    assert_eq!(patched["user_id"], "u_123", "{patched}");
    let stored = h.exported().await;
    assert!(stored.iter().all(|(_, id)| id == "alice"), "{stored:?}");
}

/// A plural the rule reads as the budget does: one record on every route.
#[tokio::test]
async fn a_category_is_found_by_category_id() {
    let contract = by_path("categories", "id", "category_id");
    let h = harness(
        contract.as_bytes(),
        vec![json!({"category_id": "c-1", "name": "one"})],
    );
    h.call("POST", "/categories", Some(json!({"name": "one"})))
        .await;
    assert_eq!(
        h.exported().await,
        [("categories".to_owned(), "c-1".to_owned())]
    );
    let (_, source, read) = h.call("GET", "/categories/c-1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(h.calls(), 1);
}

/// With an identity, `unknown_ids: 404` can tell a missing record: main generated.
#[tokio::test]
async fn the_type_name_rule_lets_unknown_ids_answer_404() {
    let contract = by_path("accounts", "id", "account_id");
    let h = harness_with(contract.as_bytes(), vec![], |rt| {
        rt.unknown_ids = UnknownIds::NotFound;
    });
    let (status, _, body) = h.call("GET", "/accounts/7", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(h.calls(), 0);
}

/// A search listing another shape beside a type's items stays its own type: what it
/// lists does not go under that type, so a read of it holds nothing foreign.
#[tokio::test]
async fn no_foreign_member_reaches_a_read_through_a_search() {
    let contract = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /search:
    get:
      operationId: search
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {users: {type: array, items: {$ref: "#/components/schemas/Hit"}}, tickets: {type: array, items: {$ref: "#/components/schemas/Ticket"}}}}}}}}
  /tickets/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTicket
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}}
components:
  schemas:
    Ticket: {type: object, properties: {id: {type: string}, subject: {type: string}}}
    Hit: {type: object, properties: {id: {type: string}, label: {type: string}}}
"##;
    let h = harness(
        contract.as_bytes(),
        vec![
            json!({"users": [{"id": "7", "label": "a hit"}], "tickets": []}),
            json!({"id": "7", "subject": "a ticket"}),
        ],
    );
    let (status, _, found) = h.call("GET", "/search", None).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    let stored = h.exported().await;
    assert!(!stored.iter().any(|(t, _)| t == "tickets"), "{stored:?}");
    let (_, source, read) = h.call("GET", "/tickets/7", None).await;
    assert_eq!(source, "llm", "{read}");
    assert_eq!(read, json!({"id": "7", "subject": "a ticket"}));
}

/// A contract whose `id` already works: same calls, same store.
#[tokio::test]
async fn a_contract_with_an_id_is_as_it_was() {
    let contract = by_path("things", "id", "id");
    let thing = |name: &str| json!({"id": "w-1", "name": name});
    let h = harness(contract.as_bytes(), vec![thing("one"), thing("two")]);
    h.call("POST", "/things", Some(json!({"name": "one"})))
        .await;
    assert_eq!(
        h.exported().await,
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
    h.call("DELETE", "/things/w-1", None).await;
    let (status, _, _) = h.call("GET", "/things/w-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(h.calls(), 2);
}
