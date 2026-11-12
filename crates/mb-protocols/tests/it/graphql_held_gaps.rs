//! A GraphQL read of a held record selecting members it lacks is completed from
//! state, offline: a nullable member is `null`, a non-null one is filled, the stored
//! values are kept, and repeated reads answer alike. With a model, a nullable member
//! a computed object leaves out is `null` from state too.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::{compile, parse_document};
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, graphql};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};

const SDL: &str = r#"
type Money { amount: String! currency: String! }
type Dims { width: Int! height: Int }
type Widget {
  id: ID!
  name: String!
  note: String
  size: Int!
  price: Money!
  dims: Dims!
}
type Query { widget(id: ID!): Widget }
"#;

/// What a read selecting more of the widget than the session holds asks for.
const READ: &str =
    r#"{ widget(id: "W1") { id name note size price { amount currency } dims { width height } } }"#;

async fn serve(store: Arc<dyn StateStore>) -> String {
    serve_with(store, SDL, &mb_mcm::world::World::default(), None).await
}

/// Mock `w` of `sdl` and world `doc` on `store`, with `provider` as its model.
async fn serve_with(
    store: Arc<dyn StateStore>,
    sdl: &str,
    doc: &mb_mcm::world::World,
    provider: Option<Arc<dyn Provider>>,
) -> String {
    let bundle = compile(sdl.as_bytes()).unwrap();
    let base_world = mb_mcm::world::tier0(&bundle);
    let (world, refused) = mb_mcm::world::effective(&bundle, &base_world, doc);
    assert!(refused.is_empty(), "{refused:?}");
    let rt = MockRuntime::new(
        "w",
        Arc::new(bundle),
        Arc::new(Engine::new(provider, EngineConfig::default())),
        store,
    )
    .with_world(&base_world, &world, 1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// `query` in session `s1`: its `data` and its root field's source.
async fn call(base: &str, query: &str) -> (Value, String) {
    let body: Value = reqwest::Client::new()
        .post(format!("{base}/graphql"))
        .header("x-mockingbird-session", "s1")
        .json(&json!({ "query": query }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    let source = body["extensions"]["mockingbird"]["sources"]
        .as_object()
        .and_then(|s| s.values().next())
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    (body["data"].clone(), source)
}

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "w".into(),
        session: "s1".into(),
    }
}

/// Offline: no provider, so nothing here is the model's.
#[tokio::test]
async fn a_selection_a_held_record_lacks_is_completed_from_state() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let key = session();
    // Held without `note` (nullable), `size` and `price` (non-null), and with a `dims`
    // lacking its nullable `height`.
    let held = json!({"__typename": "Widget", "id": "W1", "name": "Lamp",
        "dims": {"__typename": "Dims", "width": 3}});
    store
        .seed(
            &key,
            vec![StoredResource {
                resource_type: "widget".into(),
                id: "W1".into(),
                data: held,
            }],
        )
        .await
        .unwrap();
    let base = serve(Arc::clone(&store)).await;

    let (data, source) = call(&base, READ).await;
    assert_eq!(source, "state", "{data}");
    let widget = &data["widget"];
    // The stored values are kept.
    assert_eq!(widget["id"], "W1", "{data}");
    assert_eq!(widget["name"], "Lamp", "{data}");
    assert_eq!(widget["dims"]["width"], 3, "{data}");
    // A nullable member nothing states is null.
    assert_eq!(widget["note"], Value::Null, "{data}");
    assert_eq!(widget["dims"]["height"], Value::Null, "{data}");
    // A non-null one is filled.
    assert!(widget["size"].is_i64(), "{data}");
    assert!(widget["price"]["amount"].is_string(), "{data}");
    assert!(widget["price"]["currency"].is_string(), "{data}");

    // Read again: the same answer, from state.
    let (again, source) = call(&base, READ).await;
    assert_eq!(source, "state", "{again}");
    assert_eq!(again, data);

    // The completion is stored, what was held unchanged.
    let stored = store.get(&key, "widget", "W1").await.unwrap().unwrap();
    assert_eq!(stored.data["name"], "Lamp", "{:?}", stored.data);
    assert_eq!(stored.data["dims"]["width"], 3, "{:?}", stored.data);
    assert_eq!(stored.data["size"], widget["size"], "{:?}", stored.data);
    assert_eq!(
        stored.data["price"]["amount"], widget["price"]["amount"],
        "{:?}",
        stored.data
    );
}

/// With a model, a nullable member a world-computed object leaves out is `null` from
/// state: the expression states the whole value, and nothing is generated.
#[tokio::test]
async fn a_computed_object_answers_a_nullable_member_it_leaves_out_null() {
    let sdl = r#"
type Cost { total: String! tax: String }
type Order { id: ID! label: String! cost: Cost! }
type Query { order(id: ID!): Order }
"#;
    let world = "types:\n  order:\n    identity: { member: id }\n    computed:\n      cost: \"{'total': record.label + '!'}\"\n";
    let doc: mb_mcm::world::World =
        serde_json::from_value(parse_document(world.as_bytes()).unwrap()).unwrap();
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store
        .seed(
            &session(),
            vec![StoredResource {
                resource_type: "order".into(),
                id: "O1".into(),
                data: json!({"__typename": "Order", "id": "O1", "label": "a"}),
            }],
        )
        .await
        .unwrap();
    // Nothing scripted: any model call fails the read over to a fallback.
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let base = serve_with(
        store,
        sdl,
        &doc,
        Some(provider.clone() as Arc<dyn Provider>),
    )
    .await;
    let (data, source) = call(&base, r#"{ order(id: "O1") { id cost { total tax } } }"#).await;
    assert_eq!(source, "state", "{data}");
    assert_eq!(data["order"]["cost"]["total"], "a!", "{data}");
    assert_eq!(data["order"]["cost"]["tax"], Value::Null, "{data}");
    assert!(provider.requests().is_empty());
}

/// A store holding `record` (a `widget` or a `gadget`, by its `__typename`) in session
/// `s1`.
async fn holding(record: Value) -> Arc<dyn StateStore> {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let t = record["__typename"].as_str().unwrap().to_lowercase();
    let id = record["id"].as_str().unwrap().to_owned();
    store
        .seed(
            &session(),
            vec![StoredResource {
                resource_type: t,
                id,
                data: record,
            }],
        )
        .await
        .unwrap();
    store
}

/// A selection that names a field by another field's name (`note: size`) is not
/// completed: nothing is nulled or filled under the wrong name, nothing is stored, and
/// the read generates as before.
#[tokio::test]
async fn an_aliased_selection_is_not_completed() {
    let held = json!({"__typename": "Widget", "id": "W1", "name": "Lamp",
        "dims": {"__typename": "Dims", "width": 3}});
    let store = holding(held.clone()).await;
    let base = serve(Arc::clone(&store)).await;
    let (data, source) = call(
        &base,
        r#"{ widget(id: "W1") { id note: size size: note } }"#,
    )
    .await;
    assert_ne!(source, "state", "{data}");
    let stored = store
        .get(&session(), "widget", "W1")
        .await
        .unwrap()
        .unwrap();
    assert!(
        stored.data.get("note").is_none_or(|n| !n.is_i64()),
        "{:?}",
        stored.data
    );
    assert!(
        stored.data.get("size").is_none_or(|s| !s.is_null()),
        "{:?}",
        stored.data
    );
}

const GADGETS: &str = r#"
type User { id: ID! login: String! bio: String }
type Part { id: ID! name: String! }
type PartConn { nodes: [Part!]! }
type Meta { tag: String! note: String }
type Gadget {
  id: ID!
  name: String!
  owner: User!
  parts(first: Int): PartConn!
  extras: PartConn
  meta: Meta
  labels: [String!]!
}
type Query { gadget(id: ID!): Gadget }
"#;

fn gadget() -> Value {
    json!({"__typename": "Gadget", "id": "G1", "name": "Box",
        "owner": {"__typename": "User", "id": "U1", "login": "ann"},
        "meta": {"__typename": "Meta", "note": "kept"}})
}

/// Another record the answer embeds (the gadget's `owner`) lacking a selected field is
/// not completed: the read generates over the held record, as before.
#[tokio::test]
async fn an_embedded_record_lacking_a_selected_field_still_generates() {
    let store = holding(gadget()).await;
    let base = serve_with(store, GADGETS, &mb_mcm::world::World::default(), None).await;
    let (data, source) = call(&base, r#"{ gadget(id: "G1") { id owner { login bio } } }"#).await;
    assert_ne!(source, "state", "{data}");
}

/// A missing field holding records (a connection of parts, non-null or nullable) is
/// neither filled nor nulled: its records are found by the read, which generates.
#[tokio::test]
async fn a_missing_field_holding_records_is_neither_nulled_nor_filled() {
    for query in [
        r#"{ gadget(id: "G1") { id parts(first: 2) { nodes { name } } } }"#,
        r#"{ gadget(id: "G1") { id extras { nodes { name } } } }"#,
    ] {
        let store = holding(gadget()).await;
        let base = serve_with(
            Arc::clone(&store),
            GADGETS,
            &mb_mcm::world::World::default(),
            None,
        )
        .await;
        let (data, source) = call(&base, query).await;
        assert_ne!(source, "state", "{query}: {data}");
        let stored = store
            .get(&session(), "gadget", "G1")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            stored.data.get("extras"),
            Some(&Value::Null),
            "{:?}",
            stored.data
        );
    }
}

/// A nullable object below the top whose value lacks a non-null member is completed in
/// place (its stored member kept), and a missing non-null list of values is empty.
#[tokio::test]
async fn a_nullable_object_lacking_a_non_null_member_is_completed_in_place() {
    let store = holding(gadget()).await;
    let base = serve_with(
        Arc::clone(&store),
        GADGETS,
        &mb_mcm::world::World::default(),
        None,
    )
    .await;
    let query = r#"{ gadget(id: "G1") { id name meta { tag note } labels } }"#;
    let (data, source) = call(&base, query).await;
    assert_eq!(source, "state", "{data}");
    let gadget = &data["gadget"];
    assert_eq!(gadget["name"], "Box", "{data}");
    assert_eq!(gadget["meta"]["note"], "kept", "{data}");
    assert!(gadget["meta"]["tag"].is_string(), "{data}");
    assert_eq!(gadget["labels"], json!([]), "{data}");
    let stored = store
        .get(&session(), "gadget", "G1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.data["meta"]["note"], "kept", "{:?}", stored.data);
    assert_eq!(
        stored.data["meta"]["tag"], gadget["meta"]["tag"],
        "{:?}",
        stored.data
    );
    let (again, _) = call(&base, query).await;
    assert_eq!(again, data);
}

/// With a model, a selection asking for top-level fields the record lacks (a nullable
/// and a non-null one) generates over the held record, as before.
#[tokio::test]
async fn with_a_model_missing_top_level_fields_still_generate() {
    let store = holding(json!({"__typename": "Widget", "id": "W1", "name": "Lamp",
        "dims": {"__typename": "Dims", "width": 3}}))
    .await;
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(json!({
        "__typename": "Widget", "id": "W1", "name": "Lamp", "note": "written",
        "size": 4
    })
    .to_string())]));
    let base = serve_with(
        store,
        SDL,
        &mb_mcm::world::World::default(),
        Some(provider.clone() as Arc<dyn Provider>),
    )
    .await;
    let (data, source) = call(&base, r#"{ widget(id: "W1") { id name note size } }"#).await;
    assert_ne!(source, "state", "{data}");
    assert_eq!(provider.requests().len(), 1, "{data}");
    assert_eq!(data["widget"]["name"], "Lamp", "{data}");
}
