//! A runtime built anew over a session's state (a restart, a reloaded world) counts
//! its seeded mints from zero while the session keeps what it minted before: a world's
//! `mint()` identity the session holds is drawn again, so a create never names (and
//! overwrites) a held record.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use mb_state::{MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};

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
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let first = carts_over(&store);
    let mut held = Vec::new();
    // Every cart is created alike, as an app's `cartCreate` is.
    for _ in 0..3 {
        held.push(new_cart(&first, "cart").await);
    }
    let before = carts(&first).await;
    assert_eq!(before.len(), 3, "{before:?}");
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
}

#[tokio::test]
async fn when_every_redraw_is_held_the_executor_s_id_stays() {
    // More carts than a create redraws: each of its draws after a reload is held.
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let first = carts_over(&store);
    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(new_cart(&first, "cart").await);
    }
    let before = carts(&first).await;
    let reloaded = carts_over(&store);
    let (status, _, body) = reloaded
        .send("POST", "/carts", Some(json!({"note": "cart"})), &[SEED])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let next = body["id"].as_str().unwrap().to_string();
    assert!(!held.contains(&next), "{next} names a held cart: {held:?}");
    let after = carts(&reloaded).await;
    assert_eq!(after.len(), 9, "{after:?}");
    for cart in &before {
        assert!(after.contains(cart), "{cart} changed: {after:?}");
    }
}
