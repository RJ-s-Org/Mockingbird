//! A runtime built anew over a session's state (a restart, a reloaded world) counts
//! its seeded mints from zero while the session keeps what it minted before: it never
//! mints an id the session holds again, whether the world's `mint()` draws it or the
//! executor does, and two creates at once are minted two ids.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_state::{MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::support::{self, DescribedHarness, HarnessOptions};

const CARTS: &str = r##"
openapi: 3.0.0
info: {title: Carts, version: "1"}
paths:
  /carts:
    post:
      operationId: createCart
      requestBody: {content: {application/json: {schema: {type: object, properties: {note: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}}
  /carts/{cartId}:
    get:
      operationId: getCart
      parameters: [{name: cartId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}}
components:
  schemas:
    Cart:
      type: object
      required: [id]
      properties:
        id: {type: string}
        note: {type: string}
"##;

/// Carts are named by the world: a prefix and a `mint()`ed key.
fn world() -> Value {
    json!({"operations": {"createCart": {
        "kind": "create",
        "set": {"id": "'gid://shop/Cart/' + mint('[A-Za-z0-9]{8}')"}
    }}})
}

/// Every call is seeded alike, so a runtime built anew draws as the first one drew.
const SEED: (&str, &str) = (mb_engine::seed::SEED_HEADER, "42");

/// A runtime of [`CARTS`] serving [`world`] over `store`: built anew, as on a reload.
fn carts_over(store: &Arc<dyn StateStore>) -> DescribedHarness {
    let store = Arc::clone(store);
    support::harness(CARTS, Vec::new(), &HarnessOptions::default(), |mut rt| {
        rt.state = store;
        support::with_effective(rt, world(), false)
    })
}

fn memory() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new(Duration::from_secs(3600)))
}

async fn new_cart(h: &DescribedHarness, note: &str) -> String {
    let (status, _, body) = h
        .send("POST", "/carts", Some(json!({"note": note})), &[SEED])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("gid://shop/Cart/"), "{id}");
    id
}

/// The carts the default session holds, as stored.
async fn carts(h: &DescribedHarness) -> Vec<Value> {
    let session = SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    };
    h.rt.state
        .list(&session, "carts", 100)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.data)
        .collect()
}

#[tokio::test]
async fn a_world_mint_after_a_reload_never_names_a_held_cart() {
    // Every cart is created alike, as an app's `cartCreate` is.
    let store = memory();
    let first = carts_over(&store);
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(new_cart(&first, "cart").await);
    }
    let before = carts(&first).await;
    assert_eq!(before.len(), 3);
    // The same call in a runtime built over the same state.
    let reloaded = carts_over(&store);
    let next = new_cart(&reloaded, "cart").await;
    assert!(!held.contains(&next), "{next} names a held cart: {held:?}");
    // The held carts are as they were, the new one beside them.
    let after = carts(&reloaded).await;
    assert_eq!(after.len(), 4, "{after:?}");
    for cart in &before {
        assert!(after.contains(cart), "{cart} changed: {after:?}");
    }
    // Built anew again, it still draws apart from both runtimes'.
    held.push(next);
    let again = carts_over(&store);
    let last = new_cart(&again, "cart").await;
    assert!(!held.contains(&last), "{last} names a held cart: {held:?}");
}

#[tokio::test]
async fn a_fresh_session_mints_as_it_always_did() {
    // Generation 0 draws as before: the same calls over two empty stores mint alike.
    let a = carts_over(&memory());
    let b = carts_over(&memory());
    assert_eq!(new_cart(&a, "x").await, new_cart(&b, "x").await);
}

#[tokio::test]
async fn concurrent_world_mints_differ_within_a_runtime_and_across_reloaded_ones() {
    let store = memory();
    let first = carts_over(&store);
    let mut held = vec![new_cart(&first, "c").await, new_cart(&first, "c").await];
    // Two runtimes built anew at once, each creating two carts at once.
    let (one, two) = (carts_over(&store), carts_over(&store));
    let (w, x, y, z) = tokio::join!(
        new_cart(&one, "c"),
        new_cart(&one, "c"),
        new_cart(&two, "c"),
        new_cart(&two, "c"),
    );
    held.extend([w, x, y, z]);
    let mut unique = held.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), held.len(), "{held:?}");
}

/// Things answer their id only in a header, minted by the executor in a short pattern.
const THINGS: &str = r##"
openapi: 3.0.0
info: {title: Things, version: "1"}
paths:
  /things:
    post:
      operationId: createThing
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}
      responses:
        "201":
          description: created
          headers:
            Thing-Id: {schema: {type: string}}
  /things/{thingId}:
    get:
      operationId: getThing
      parameters: [{name: thingId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}
components:
  schemas:
    Thing:
      type: object
      properties:
        id: {type: string, pattern: "^[a-z]{3}$"}
        name: {type: string}
"##;

/// A runtime of [`THINGS`] over `store`, its faker seeded: built anew, as on a restart.
fn things_over(store: &Arc<dyn StateStore>) -> axum::Router {
    let rt = mb_protocols::MockRuntime::new(
        "things",
        Arc::new(mb_import::compile(THINGS.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default()).with_faker_seed(7)),
        Arc::clone(store),
    );
    mb_protocols::http_rest::router(Arc::new(rt))
}

async fn new_thing(router: &axum::Router, name: &str) -> String {
    let req = Request::builder()
        .method("POST")
        .uri("/things")
        .header("content-type", "application/json")
        .body(Body::from(json!({"name": name}).to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = resp
        .headers()
        .get("thing-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(!id.is_empty(), "{name} was minted no id");
    id
}

async fn thing_name(router: &axum::Router, id: &str) -> Value {
    let req = Request::builder()
        .uri(format!("/things/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null)["name"].clone()
}

#[tokio::test]
async fn an_executor_mint_after_a_restart_never_names_a_held_record() {
    // More than the draws a mint tries against what the session holds.
    let store = memory();
    let first = things_over(&store);
    let mut held = Vec::new();
    for _ in 0..12 {
        held.push(new_thing(&first, "t").await);
    }
    let restarted = things_over(&store);
    let next = new_thing(&restarted, "t").await;
    assert!(!held.contains(&next), "{next} names a held thing: {held:?}");
    for id in &held {
        assert_eq!(thing_name(&restarted, id).await, json!("t"), "{id}");
    }
}

#[tokio::test]
async fn concurrent_executor_mints_differ() {
    let store = memory();
    let first = things_over(&store);
    let mut held = vec![new_thing(&first, "b").await, new_thing(&first, "b").await];
    let (one, two) = (things_over(&store), things_over(&store));
    let (w, x, y, z) = tokio::join!(
        new_thing(&one, "b"),
        new_thing(&one, "b"),
        new_thing(&two, "b"),
        new_thing(&two, "b"),
    );
    held.extend([w, x, y, z]);
    let mut unique = held.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), held.len(), "{held:?}");
}

#[tokio::test]
async fn a_world_mint_after_a_restart_over_a_sqlite_file_never_names_a_held_cart() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("state.db").display());
    let mut held = Vec::new();
    {
        let store = mb_state::connect(&url, Duration::from_secs(3600))
            .await
            .unwrap();
        let first = carts_over(&store);
        for _ in 0..3 {
            held.push(new_cart(&first, "cart").await);
        }
    }
    // A new process: a new client of the same file, a new runtime.
    let store = mb_state::connect(&url, Duration::from_secs(3600))
        .await
        .unwrap();
    let restarted = carts_over(&store);
    let next = new_cart(&restarted, "cart").await;
    assert!(!held.contains(&next), "{next} names a held cart: {held:?}");
    assert_eq!(carts(&restarted).await.len(), 4);
}

fn default_session(h: &DescribedHarness) -> SessionKey {
    SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    }
}

#[tokio::test]
async fn a_world_mint_in_a_session_restored_from_a_snapshot_never_names_a_held_cart() {
    // A snapshot carries the records, not the runtime's own documents.
    let store = memory();
    let first = carts_over(&store);
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(new_cart(&first, "cart").await);
    }
    let exported = store.export(&default_session(&first)).await.unwrap();
    let other = memory();
    other
        .seed(&default_session(&first), exported)
        .await
        .unwrap();
    let restored = carts_over(&other);
    let next = new_cart(&restored, "cart").await;
    assert!(!held.contains(&next), "{next} names a held cart: {held:?}");
    assert_eq!(carts(&restored).await.len(), 4);
}

#[tokio::test]
async fn a_runtime_still_serving_after_a_reset_draws_apart_from_a_new_one() {
    let store = memory();
    let old = carts_over(&store);
    new_cart(&old, "cart").await;
    store.reset(&default_session(&old)).await.unwrap();
    // A new runtime takes the reset session first; the old one goes on serving it.
    let new = carts_over(&store);
    let mut ids = vec![
        new_cart(&new, "cart").await,
        new_cart(&new, "cart").await,
        new_cart(&old, "cart").await,
        new_cart(&new, "cart").await,
        new_cart(&old, "cart").await,
    ];
    assert_eq!(carts(&new).await.len(), ids.len());
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 5, "{ids:?}");
}
