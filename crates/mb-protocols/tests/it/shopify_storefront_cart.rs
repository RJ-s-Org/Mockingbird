//! The Shopify Storefront example's cart, offline, with its world description:
//! lines of one variant merge, quantity 0 removes a line, and the totals a
//! read answers are computed from the lines; a cart created with lines holds them, totalled.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::{compile, parse_document};
use mb_protocols::{MockRuntime, graphql};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};

const CONTRACT: &str =
    include_str!("../../../../examples/apis/shopify-storefront-graphql/shopify-storefront.graphql");
const WORLD: &str = include_str!(
    "../../../../examples/apis/shopify-storefront-graphql/shopify-storefront.world.yaml"
);

const VARIANT: &str = "gid://shopify/ProductVariant/45001";
const PRODUCT: &str = "gid://shopify/Product/8493028147510";

/// The cart selection the checks read.
const CART: &str = "id totalQuantity \
    cost { subtotalAmount { amount currencyCode } totalAmount { amount currencyCode } } \
    lines(first: 10) { edges { node { id quantity cost { totalAmount { amount } } \
    merchandise { ... on ProductVariant { id title price { amount } product { handle } } } } } }";

/// A product with one variant at 12.99, as a product read leaves them in the session.
fn catalog() -> Vec<StoredResource> {
    let product = json!({"__typename": "Product", "id": PRODUCT, "handle": "fiddle-leaf-fig",
        "title": "Fiddle Leaf Fig"});
    let variant = json!({"__typename": "ProductVariant", "id": VARIANT, "title": "Small",
        "availableForSale": true, "currentlyNotInStock": false, "requiresShipping": true,
        "selectedOptions": [{"__typename": "SelectedOption", "name": "Size", "value": "Small"}],
        "price": {"__typename": "MoneyV2", "amount": "12.99", "currencyCode": "USD"},
        "product": {"__typename": "Product", "id": PRODUCT, "handle": "fiddle-leaf-fig",
            "title": "Fiddle Leaf Fig"}});
    vec![
        StoredResource {
            resource_type: "product".into(),
            id: PRODUCT.into(),
            data: product,
        },
        StoredResource {
            resource_type: "productvariant".into(),
            id: VARIANT.into(),
            data: variant,
        },
    ]
}

async fn serve() -> std::net::SocketAddr {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let doc: mb_mcm::world::World =
        serde_json::from_value(parse_document(WORLD.as_bytes()).unwrap()).unwrap();
    let base_world = mb_mcm::world::tier0(&bundle);
    let (world, refused) = mb_mcm::world::effective(&bundle, &base_world, &doc);
    assert!(refused.is_empty(), "{refused:?}");
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let key = SessionKey {
        workspace: "default".into(),
        mock: "shop".into(),
        session: "s1".into(),
    };
    store.seed(&key, catalog()).await.unwrap();
    // Offline: no provider, so nothing here is the model's.
    let rt = MockRuntime::new(
        "shop",
        Arc::new(bundle),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&store),
    )
    .with_world(&base_world, &world, 1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

/// `query` with `variables` in session `s1`: its `data` and its root field's source.
async fn call(addr: std::net::SocketAddr, query: &str, variables: Value) -> (Value, String) {
    let body: Value = reqwest::Client::new()
        .post(format!("http://{addr}/graphql"))
        .header("content-type", "application/json")
        .header("x-mockingbird-session", "s1")
        .body(json!({ "query": query, "variables": variables }).to_string())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    let sources = &body["extensions"]["mockingbird"]["sources"];
    let source = sources
        .as_object()
        .and_then(|s| s.values().next())
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    (body["data"].clone(), source)
}

fn amount(money: &Value) -> &str {
    money["amount"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn the_cart_merges_removes_and_totals_from_its_lines() {
    let addr = serve().await;

    let (data, source) = call(
        addr,
        &format!("mutation {{ cartCreate {{ cart {{ {CART} }} userErrors {{ message }} }} }}"),
        json!({}),
    )
    .await;
    assert_eq!(source, "state", "{data}");
    let cart = &data["cartCreate"]["cart"];
    let id = cart["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("gid://shopify/Cart/"), "{id}");
    assert_eq!(cart["totalQuantity"], 0, "{data}");
    assert_eq!(data["cartCreate"]["userErrors"], json!([]), "{data}");

    // The same variant, added twice: one line, its quantities summed.
    let add = format!(
        "mutation($c: ID!, $l: [CartLineInput!]!) {{ cartLinesAdd(cartId: $c, lines: $l) {{ cart {{ {CART} }} }} }}"
    );
    call(
        addr,
        &add,
        json!({"c": id, "l": [{"merchandiseId": VARIANT, "quantity": 2}]}),
    )
    .await;
    let (data, source) = call(
        addr,
        &add,
        json!({"c": id, "l": [{"merchandiseId": VARIANT, "quantity": 1}]}),
    )
    .await;
    assert_eq!(source, "state", "{data}");
    let cart = &data["cartLinesAdd"]["cart"];
    let edges = cart["lines"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{cart}");
    let line = &edges[0]["node"];
    assert_eq!(line["quantity"], 3, "{cart}");
    assert_eq!(line["merchandise"]["id"], VARIANT, "{cart}");
    assert_eq!(line["merchandise"]["title"], "Small", "{cart}");
    assert_eq!(
        line["merchandise"]["product"]["handle"], "fiddle-leaf-fig",
        "{cart}"
    );
    assert_eq!(amount(&line["cost"]["totalAmount"]), "38.97", "{cart}");
    assert_eq!(cart["totalQuantity"], 3, "{cart}");
    assert_eq!(amount(&cart["cost"]["subtotalAmount"]), "38.97", "{cart}");
    assert_eq!(amount(&cart["cost"]["totalAmount"]), "38.97", "{cart}");
    assert_eq!(cart["cost"]["totalAmount"]["currencyCode"], "USD", "{cart}");
    let line_id = line["id"].as_str().unwrap().to_string();

    // Quantity 0 removes the line.
    let update = format!(
        "mutation($c: ID!, $l: [CartLineUpdateInput!]!) {{ cartLinesUpdate(cartId: $c, lines: $l) {{ cart {{ {CART} }} }} }}"
    );
    let (data, _) = call(
        addr,
        &update,
        json!({"c": id, "l": [{"id": line_id, "quantity": 0}]}),
    )
    .await;
    assert_eq!(
        data["cartLinesUpdate"]["cart"]["lines"]["edges"],
        json!([]),
        "{data}"
    );

    // A read of the cart answers what the mutations left, totals included.
    let (data, source) = call(
        addr,
        &format!("query($id: ID!) {{ cart(id: $id) {{ {CART} }} }}"),
        json!({"id": id}),
    )
    .await;
    assert_eq!(source, "state", "{data}");
    let cart = &data["cart"];
    assert_eq!(cart["id"], id.as_str(), "{data}");
    assert_eq!(cart["lines"]["edges"], json!([]), "{data}");
    assert_eq!(cart["totalQuantity"], 0, "{data}");
    assert_eq!(amount(&cart["cost"]["subtotalAmount"]), "0.0", "{data}");
    assert_eq!(amount(&cart["cost"]["totalAmount"]), "0.0", "{data}");
}

/// A cart created with lines holds them, totalled, from state.
#[tokio::test]
async fn a_cart_created_with_lines_holds_them() {
    let addr = serve().await;
    let (data, source) = call(
        addr,
        &format!(
            "mutation($i: CartInput) {{ cartCreate(input: $i) {{ cart {{ {CART} }} userErrors {{ message }} }} }}"
        ),
        json!({"i": {"lines": [{"merchandiseId": VARIANT, "quantity": 2}]}}),
    )
    .await;
    assert_eq!(source, "state", "{data}");
    assert_eq!(data["cartCreate"]["userErrors"], json!([]), "{data}");
    let cart = &data["cartCreate"]["cart"];
    assert_eq!(cart["totalQuantity"], 2, "{data}");
    let line = &cart["lines"]["edges"][0]["node"];
    assert_eq!(line["merchandise"]["id"], VARIANT, "{data}");
    assert_eq!(line["merchandise"]["title"], "Small", "{data}");
    assert_eq!(amount(&cart["cost"]["totalAmount"]), "25.98", "{data}");
    let id = cart["id"].as_str().unwrap().to_string();
    let (data, source) = call(
        addr,
        &format!("query($id: ID!) {{ cart(id: $id) {{ {CART} }} }}"),
        json!({"id": id}),
    )
    .await;
    assert_eq!(source, "state", "{data}");
    let edges = data["cart"]["lines"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{data}");
    assert_eq!(edges[0]["node"]["quantity"], 2, "{data}");
    assert_eq!(
        amount(&data["cart"]["cost"]["totalAmount"]),
        "25.98",
        "{data}"
    );
}
