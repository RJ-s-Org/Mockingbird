//! Plain root list fields (`[T]`) answered from the session's records, and the faker's
//! list items never seeded as records.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::{compile, parse_document};
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, UnknownIds, graphql};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};

const SDL: &str = r#"
type Query {
  widgets(first: Int, after: String): WidgetConnection!
  widgetsByColor(color: String, sortKey: WidgetSortKeys, reverse: Boolean, limit: Int): [Widget!]!
  related(widgetId: ID!): [Widget!]
  relatedWidgets(widgetId: ID!, first: Int): WidgetConnection!
  allWidgets: [Widget]
  widget(id: ID!): Widget
}
enum WidgetSortKeys { TITLE PRICE ID }
type Widget { id: ID! title: String! color: String price: Float relatedTo: ID similar: [Widget!] }
type WidgetEdge { cursor: String! node: Widget! }
type PageInfo { hasNextPage: Boolean! endCursor: String }
type WidgetConnection { edges: [WidgetEdge!]! nodes: [Widget!]! pageInfo: PageInfo! }
"#;

const WORLD: &str = r"
version: 1
types:
  widget:
    identity: { member: id }
";

/// The `related` list described as the widgets whose `relatedTo` is the argument.
const SCOPED_WORLD: &str = r"
version: 1
types:
  widget:
    identity: { member: id }
operations:
  Query.related:
    kind: list
    type: widget
    scope: { param: widgetId, member: relatedTo }
  Query.relatedWidgets:
    kind: list
    type: widget
    scope: { param: widgetId, member: relatedTo }
";

fn widget(id: &str, title: &str, color: &str, price: f64, related: &str) -> StoredResource {
    StoredResource {
        resource_type: "widget".into(),
        id: id.into(),
        data: json!({"__typename": "Widget", "id": id, "title": title, "color": color,
            "price": price, "relatedTo": related}),
    }
}

fn widgets() -> Vec<StoredResource> {
    vec![
        widget("w1", "Banana", "red", 12.5, "w9"),
        widget("w2", "apple", "blue", 3.0, "w1"),
        widget("w3", "Cherry", "red", 100.0, "w1"),
    ]
}

fn key() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "s1".into(),
    }
}

/// A mock of `contract` with `world`, session `s1` seeded with `seeded`, answered by
/// `provider` (none: offline).
async fn serve(
    contract: &str,
    world: &str,
    seeded: Vec<StoredResource>,
    provider: Option<Arc<ScriptedProvider>>,
) -> String {
    serve_ids(contract, world, seeded, provider, UnknownIds::Inferred).await
}

/// [`serve`] with `unknown_ids`.
async fn serve_ids(
    contract: &str,
    world: &str,
    seeded: Vec<StoredResource>,
    provider: Option<Arc<ScriptedProvider>>,
    unknown_ids: UnknownIds,
) -> String {
    let bundle = compile(contract.as_bytes()).unwrap();
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    store.seed(&key(), seeded).await.unwrap();
    let doc: mb_mcm::world::World =
        serde_json::from_value(parse_document(world.as_bytes()).unwrap()).unwrap();
    let base = mb_mcm::world::tier0(&bundle);
    let (effective, refused) = mb_mcm::world::effective(&bundle, &base, &doc);
    assert!(refused.is_empty(), "{refused:?}");
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(bundle),
        Arc::new(Engine::new(
            provider.map(|p| p as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store),
    )
    .with_world(&base, &effective, 1);
    rt.unknown_ids = unknown_ids;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

async fn gql(base: &str, path: &str, query: &str) -> Value {
    reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header("x-mockingbird-session", "s1")
        .json(&json!({ "query": query }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn source(answer: &Value) -> String {
    answer["extensions"]["mockingbird"]["sources"]
        .as_object()
        .and_then(|s| s.values().next())
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn ids(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_plain_root_list_answers_the_held_records_sorted_and_filtered() {
    let base = serve(SDL, WORLD, widgets(), None).await;
    let all = gql(&base, "/graphql", "{ widgetsByColor { id title } }").await;
    assert!(all.get("errors").is_none(), "{all}");
    assert_eq!(source(&all), "state", "{all}");
    assert_eq!(
        ids(&all["data"]["widgetsByColor"]),
        ["w1", "w2", "w3"],
        "{all}"
    );
    assert_eq!(all["data"]["widgetsByColor"][2]["title"], "Cherry", "{all}");
    let red = gql(
        &base,
        "/graphql",
        r#"{ widgetsByColor(color: "red", sortKey: PRICE, reverse: true) { id } }"#,
    )
    .await;
    assert_eq!(source(&red), "state", "{red}");
    assert_eq!(ids(&red["data"]["widgetsByColor"]), ["w3", "w1"], "{red}");
    let titled = gql(
        &base,
        "/graphql",
        "{ widgetsByColor(sortKey: TITLE, limit: 2) { id } }",
    )
    .await;
    assert_eq!(
        ids(&titled["data"]["widgetsByColor"]),
        ["w2", "w1"],
        "{titled}"
    );
}

#[tokio::test]
async fn an_undescribed_argument_generates_and_the_world_can_scope_it() {
    // `widgetId` names a related record, no member: generated, as before.
    let base = serve(SDL, WORLD, widgets(), None).await;
    let related = gql(&base, "/graphql", r#"{ related(widgetId: "w1") { id } }"#).await;
    assert!(related.get("errors").is_none(), "{related}");
    assert_ne!(source(&related), "state", "{related}");

    // Described as a scope on `relatedTo`, it lists the held records it names.
    let base = serve(SDL, SCOPED_WORLD, widgets(), None).await;
    let related = gql(
        &base,
        "/graphql",
        r#"{ related(widgetId: "w1") { id title } }"#,
    )
    .await;
    assert_eq!(source(&related), "state", "{related}");
    assert_eq!(ids(&related["data"]["related"]), ["w2", "w3"], "{related}");
    // The same scope on a connection.
    let conn = gql(
        &base,
        "/graphql",
        r#"{ relatedWidgets(widgetId: "w1", first: 1) { nodes { id } pageInfo { hasNextPage } } }"#,
    )
    .await;
    assert_eq!(source(&conn), "state", "{conn}");
    assert_eq!(
        ids(&conn["data"]["relatedWidgets"]["nodes"]),
        ["w2"],
        "{conn}"
    );
    assert_eq!(
        conn["data"]["relatedWidgets"]["pageInfo"]["hasNextPage"], true,
        "{conn}"
    );
}

#[tokio::test]
async fn a_nullable_plain_list_answers_from_state() {
    let base = serve(SDL, WORLD, widgets(), None).await;
    let all = gql(&base, "/graphql", "{ allWidgets { id color } }").await;
    assert!(all.get("errors").is_none(), "{all}");
    assert_eq!(source(&all), "state", "{all}");
    assert_eq!(ids(&all["data"]["allWidgets"]), ["w1", "w2", "w3"], "{all}");
}

#[tokio::test]
async fn offline_a_generated_list_leaves_no_records_a_later_list_shows() {
    let base = serve(SDL, WORLD, widgets(), None).await;
    let related = gql(
        &base,
        "/graphql",
        r#"{ related(widgetId: "w1") { id title } }"#,
    )
    .await;
    assert_ne!(source(&related), "state", "{related}");
    assert!(!ids(&related["data"]["related"]).is_empty(), "{related}");
    let listed = gql(&base, "/graphql", "{ widgets(first: 50) { nodes { id } } }").await;
    assert_eq!(source(&listed), "state", "{listed}");
    assert_eq!(
        ids(&listed["data"]["widgets"]["nodes"]),
        ["w1", "w2", "w3"],
        "{listed}"
    );
}

/// A lookup the faker answers offline: the record it answers is stored, and its list
/// of a type the session holds other records of seeds nothing.
#[tokio::test]
async fn offline_a_lookups_invented_list_items_seed_nothing() {
    let base = serve(SDL, WORLD, widgets(), None).await;
    let read = gql(
        &base,
        "/graphql",
        r#"{ widget(id: "wx") { id title similar { id title } } }"#,
    )
    .await;
    assert!(read.get("errors").is_none(), "{read}");
    let similar = ids(&read["data"]["widget"]["similar"]);
    assert!(!similar.is_empty(), "{read}");
    let listed = gql(&base, "/graphql", "{ widgets(first: 50) { nodes { id } } }").await;
    let held = ids(&listed["data"]["widgets"]["nodes"]);
    assert_eq!(held, ["w1", "w2", "w3", "wx"], "{listed}");
}

/// A provider refusing the key (`401`): its faker answers are no model's, and their list
/// items join no held records (the unusable-provider reason is unit-tested in
/// `call::graphql_reads`).
#[tokio::test]
async fn a_refusing_providers_fallback_lists_seed_nothing() {
    let refused = || {
        Err(mb_llm::LlmError::Http {
            status: 401,
            body: "invalid key".into(),
        })
    };
    let provider = Arc::new(ScriptedProvider::new(vec![refused(), refused(), refused()]));
    let base = serve(SDL, WORLD, widgets(), Some(provider)).await;
    let mut reasons = Vec::new();
    for _ in 0..2 {
        let related = gql(&base, "/graphql", r#"{ related(widgetId: "w1") { id } }"#).await;
        assert!(related.get("errors").is_none(), "{related}");
        assert_eq!(source(&related), "fallback", "{related}");
        reasons.push(related["extensions"]["mockingbird"]["fallbackReasons"]["related"].clone());
    }
    let listed = gql(&base, "/graphql", "{ widgets(first: 50) { nodes { id } } }").await;
    assert_eq!(
        ids(&listed["data"]["widgets"]["nodes"]),
        ["w1", "w2", "w3"],
        "{reasons:?} {listed}"
    );
}

/// From an empty session offline, a catalog list is seeded with what it showed, so a
/// read of a listed item by id or by handle agrees with it, under `unknown_ids: 404`
/// too.
#[tokio::test]
async fn offline_a_listed_item_reads_back_from_an_empty_session() {
    for unknown_ids in [UnknownIds::Inferred, UnknownIds::NotFound] {
        let base = serve_ids(STOREFRONT, STOREFRONT_WORLD, vec![], None, unknown_ids).await;
        let listed = gql(
            &base,
            "/graphql",
            "{ products(first: 3) { nodes { id handle title } } }",
        )
        .await;
        assert!(listed.get("errors").is_none(), "{listed}");
        let shown = &listed["data"]["products"]["nodes"][0];
        let (id, handle) = (
            shown["id"].as_str().unwrap(),
            shown["handle"].as_str().unwrap(),
        );
        let by_id = gql(
            &base,
            "/graphql",
            &format!(r#"{{ product(id: "{id}") {{ id handle title }} }}"#),
        )
        .await;
        assert!(by_id.get("errors").is_none(), "{unknown_ids:?}: {by_id}");
        assert_eq!(by_id["data"]["product"]["handle"], handle, "{by_id}");
        assert_eq!(by_id["data"]["product"]["title"], shown["title"], "{by_id}");
        let by_handle = gql(
            &base,
            "/graphql",
            &format!(r#"{{ product(handle: "{handle}") {{ id title }} }}"#),
        )
        .await;
        assert!(
            by_handle.get("errors").is_none(),
            "{unknown_ids:?}: {by_handle}"
        );
        assert_eq!(by_handle["data"]["product"]["id"], id, "{by_handle}");
    }
}

#[tokio::test]
async fn a_models_list_items_are_still_stored() {
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(json!({
        "related": [
            {"__typename": "Widget", "id": "m1", "title": "Model one"},
            {"__typename": "Widget", "id": "m2", "title": "Model two"}
        ]
    })
    .to_string())]));
    let base = serve(SDL, WORLD, vec![], Some(provider)).await;
    let related = gql(
        &base,
        "/graphql",
        r#"{ related(widgetId: "w1") { id title } }"#,
    )
    .await;
    assert_eq!(ids(&related["data"]["related"]), ["m1", "m2"], "{related}");
    let listed = gql(
        &base,
        "/graphql",
        "{ widgets(first: 50) { nodes { id title } } }",
    )
    .await;
    assert_eq!(source(&listed), "state", "{listed}");
    let mut held = ids(&listed["data"]["widgets"]["nodes"]);
    held.sort();
    assert_eq!(held, ["m1", "m2"], "{listed}");
}

const STOREFRONT: &str =
    include_str!("../../../../examples/apis/shopify-storefront-graphql/shopify-storefront.graphql");
const STOREFRONT_WORLD: &str = include_str!(
    "../../../../examples/apis/shopify-storefront-graphql/shopify-storefront.world.yaml"
);

fn product(n: u32, handle: &str) -> StoredResource {
    let id = format!("gid://shopify/Product/{n}");
    StoredResource {
        resource_type: "product".into(),
        id: id.clone(),
        data: json!({"__typename": "Product", "id": id, "handle": handle, "title": handle}),
    }
}

#[tokio::test]
async fn storefront_recommendations_no_longer_pollute_the_catalog() {
    let seeded = || vec![product(1, "fig"), product(2, "pothos")];
    let recommend =
        r#"{ productRecommendations(productId: "gid://shopify/Product/1") { id handle } }"#;
    let catalog = "{ products(first: 100) { nodes { handle } } }";

    // As the example describes it: generated (its `productId` names a related product),
    // and its invented products are not seeded into the catalog.
    let base = serve(STOREFRONT, STOREFRONT_WORLD, seeded(), None).await;
    let recommended = gql(&base, "/graphql", recommend).await;
    assert!(recommended.get("errors").is_none(), "{recommended}");
    assert_ne!(source(&recommended), "state", "{recommended}");
    let listed = gql(&base, "/graphql", catalog).await;
    assert_eq!(source(&listed), "state", "{listed}");
    let handles: Vec<&str> = listed["data"]["products"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["handle"].as_str().unwrap())
        .collect();
    assert_eq!(handles, ["fig", "pothos"], "{listed}");

    // A world that says the recommendations are the held products (reading past what
    // names the product and the intent) answers them from state.
    let world = format!(
        "{STOREFRONT_WORLD}  Query.productRecommendations:\n    kind: list\n    type: product\n    \
         ignores: [productId, productHandle, intent]\n"
    );
    let base = serve(STOREFRONT, &world, seeded(), None).await;
    let recommended = gql(&base, "/graphql", recommend).await;
    assert!(recommended.get("errors").is_none(), "{recommended}");
    assert_eq!(source(&recommended), "state", "{recommended}");
    assert_eq!(
        ids(&recommended["data"]["productRecommendations"]),
        ["gid://shopify/Product/1", "gid://shopify/Product/2"],
        "{recommended}"
    );
}
