//! Root connections answered from the session's records, and lookups by a key other
//! than `id` of records the session does not hold, offline.
#![cfg(feature = "graphql")]

use std::sync::Arc;
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::{compile, parse_document};
use mb_protocols::{MockRuntime, UnknownIds, graphql};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};

const SDL: &str = r#"
type Query {
  widgets(first: Int, after: String, last: Int, before: String, reverse: Boolean = false,
          sortKey: WidgetSortKeys = ID, query: String, color: String): WidgetConnection!
  widget(id: ID, slug: String): Widget
}
enum WidgetSortKeys { TITLE PRICE CREATED_AT ID RELEVANCE }
type Money { amount: String! currencyCode: String! }
type Widget {
  id: ID!
  slug: String!
  title: String!
  color: String
  createdAt: String!
  price: Money!
}
type WidgetEdge { cursor: String! node: Widget! }
type PageInfo { hasNextPage: Boolean! hasPreviousPage: Boolean! startCursor: String endCursor: String }
type WidgetConnection { edges: [WidgetEdge!]! nodes: [Widget!]! pageInfo: PageInfo! totalCount: Int! }
"#;

const WORLD: &str = r"
version: 1
types:
  widget:
    identity: { member: id, alternates: [slug] }
operations:
  Query.widget:
    target: { member: slug }
";

fn widget(id: &str, slug: &str, title: &str, price: &str, created: &str) -> StoredResource {
    StoredResource {
        resource_type: "widget".into(),
        id: id.into(),
        data: json!({"__typename": "Widget", "id": id, "slug": slug, "title": title,
            "color": "red", "createdAt": created,
            "price": {"__typename": "Money", "amount": price, "currencyCode": "USD"}}),
    }
}

/// Held in this order: w1, w2, w3.
fn widgets() -> Vec<StoredResource> {
    vec![
        widget("w1", "banana", "Banana", "12.5", "2024-03-01T00:00:00Z"),
        widget("w2", "apple", "apple", "3", "2024-01-01T00:00:00Z"),
        widget("w3", "cherry", "Cherry", "100.0", "2024-02-01T00:00:00Z"),
    ]
}

fn key() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "s1".into(),
    }
}

/// A mock of `contract` with `world`, its session `s1` seeded with `seeded`, offline.
async fn serve(
    contract: &str,
    world: Option<&str>,
    seeded: Vec<StoredResource>,
    unknown_ids: UnknownIds,
) -> String {
    serve_with(contract, world, seeded, |rt| rt.unknown_ids = unknown_ids).await
}

/// [`serve`], the runtime set up by `configure`.
async fn serve_with(
    contract: &str,
    world: Option<&str>,
    seeded: Vec<StoredResource>,
    configure: impl FnOnce(&mut MockRuntime),
) -> String {
    let bundle = compile(contract.as_bytes()).unwrap();
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    // Written in this order.
    store.seed(&key(), seeded).await.unwrap();
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(bundle.clone()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::clone(&store),
    );
    if let Some(world) = world {
        let doc: mb_mcm::world::World =
            serde_json::from_value(parse_document(world.as_bytes()).unwrap()).unwrap();
        let base = mb_mcm::world::tier0(&bundle);
        let (effective, refused) = mb_mcm::world::effective(&bundle, &base, &doc);
        assert!(refused.is_empty(), "{refused:?}");
        rt = rt.with_world(&base, &effective, 1);
    }
    configure(&mut rt);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// The whole answer to `query` in session `s1`.
async fn gql(base: &str, path: &str, query: &str) -> Value {
    gql_with(base, path, query, &[]).await
}

/// [`gql`] with `headers`.
async fn gql_with(base: &str, path: &str, query: &str, headers: &[(&str, &str)]) -> Value {
    let mut request = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header("x-mockingbird-session", "s1");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request
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

fn ids(nodes: &Value) -> Vec<String> {
    nodes
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_root_connection_answers_the_held_records_paged_and_sorted() {
    let base = serve(SDL, Some(WORLD), widgets(), UnknownIds::Inferred).await;
    let all = gql(
        &base,
        "/graphql",
        "{ widgets(first: 10) { edges { cursor node { id title price { amount } } } \
         nodes { id slug } pageInfo { hasNextPage hasPreviousPage startCursor endCursor } totalCount } }",
    )
    .await;
    assert!(all.get("errors").is_none(), "{all}");
    assert_eq!(source(&all), "state", "{all}");
    let conn = &all["data"]["widgets"];
    assert_eq!(ids(&conn["nodes"]), ["w1", "w2", "w3"], "{all}");
    let edges = conn["edges"].as_array().unwrap();
    assert_eq!(edges[0]["cursor"], "w1", "{all}");
    assert_eq!(edges[0]["node"]["title"], "Banana", "{all}");
    assert_eq!(edges[2]["node"]["price"]["amount"], "100.0", "{all}");
    assert_eq!(conn["nodes"][1]["slug"], "apple", "{all}");
    assert_eq!(
        conn["pageInfo"],
        json!({"hasNextPage": false, "hasPreviousPage": false, "startCursor": "w1", "endCursor": "w3"}),
        "{all}"
    );
    assert_eq!(conn["totalCount"], 3, "{all}");

    // first/after pages through them.
    let page = |q: &'static str| {
        let base = base.clone();
        async move { gql(&base, "/graphql", q).await }
    };
    let one =
        page("{ widgets(first: 2) { nodes { id } pageInfo { hasNextPage endCursor } } }").await;
    assert_eq!(ids(&one["data"]["widgets"]["nodes"]), ["w1", "w2"], "{one}");
    assert_eq!(
        one["data"]["widgets"]["pageInfo"]["hasNextPage"], true,
        "{one}"
    );
    let two = page(
        r#"{ widgets(first: 2, after: "w2") { nodes { id } pageInfo { hasNextPage hasPreviousPage } } }"#,
    )
    .await;
    assert_eq!(ids(&two["data"]["widgets"]["nodes"]), ["w3"], "{two}");
    assert_eq!(
        two["data"]["widgets"]["pageInfo"],
        json!({"hasNextPage": false, "hasPreviousPage": true}),
        "{two}"
    );
    let last = page(r#"{ widgets(last: 1, before: "w3") { nodes { id } } }"#).await;
    assert_eq!(ids(&last["data"]["widgets"]["nodes"]), ["w2"], "{last}");

    // A sort key ordering by a mapped member: text without case, numbers as numbers,
    // the price below an object member.
    let by_title = page("{ widgets(first: 5, sortKey: TITLE) { nodes { id } } }").await;
    assert_eq!(
        ids(&by_title["data"]["widgets"]["nodes"]),
        ["w2", "w1", "w3"],
        "{by_title}"
    );
    let by_price =
        page("{ widgets(first: 5, sortKey: PRICE, reverse: true) { nodes { id } } }").await;
    assert_eq!(
        ids(&by_price["data"]["widgets"]["nodes"]),
        ["w3", "w1", "w2"],
        "{by_price}"
    );
    let by_created = page("{ widgets(first: 5, sortKey: CREATED_AT) { nodes { id } } }").await;
    assert_eq!(
        ids(&by_created["data"]["widgets"]["nodes"]),
        ["w2", "w3", "w1"],
        "{by_created}"
    );
    // An unmapped key, and the identity, keep the held order.
    for key in ["RELEVANCE", "ID"] {
        let q = format!("{{ widgets(first: 5, sortKey: {key}) {{ nodes {{ id }} }} }}");
        let kept = gql(&base, "/graphql", &q).await;
        assert_eq!(
            ids(&kept["data"]["widgets"]["nodes"]),
            ["w1", "w2", "w3"],
            "{kept}"
        );
        assert_eq!(source(&kept), "state", "{kept}");
    }
    // An argument naming a member filters by it.
    let red = page(r#"{ widgets(first: 5, color: "red") { nodes { id } } }"#).await;
    assert_eq!(ids(&red["data"]["widgets"]["nodes"]).len(), 3, "{red}");
    let blue = page(r#"{ widgets(first: 5, color: "blue") { nodes { id } } }"#).await;
    assert_eq!(ids(&blue["data"]["widgets"]["nodes"]).len(), 0, "{blue}");
    assert_eq!(source(&blue), "state", "{blue}");
}

#[tokio::test]
async fn an_undescribed_query_or_an_empty_session_generates() {
    // Free text the world does not describe: generated, as a REST list's undescribed
    // parameter is.
    let base = serve(SDL, Some(WORLD), widgets(), UnknownIds::Inferred).await;
    let searched = gql(
        &base,
        "/graphql",
        r#"{ widgets(first: 2, query: "ban") { nodes { id } } }"#,
    )
    .await;
    assert!(searched.get("errors").is_none(), "{searched}");
    assert_ne!(source(&searched), "state", "{searched}");
    // A blank one asks nothing.
    let blank = gql(
        &base,
        "/graphql",
        r#"{ widgets(first: 2, query: "") { nodes { id } } }"#,
    )
    .await;
    assert_eq!(source(&blank), "state", "{blank}");
    // A cursor no record has.
    let unknown = gql(
        &base,
        "/graphql",
        r#"{ widgets(first: 2, after: "nope") { nodes { id } } }"#,
    )
    .await;
    assert_ne!(source(&unknown), "state", "{unknown}");

    // Described as a text filter, the query reads the records.
    let described = format!(
        "{WORLD}  Query.widgets:\n    filters: [{{param: query, member: title, kind: text}}]\n"
    );
    let base = serve(SDL, Some(&described), widgets(), UnknownIds::Inferred).await;
    let found = gql(
        &base,
        "/graphql",
        r#"{ widgets(first: 5, query: "AN") { nodes { id } } }"#,
    )
    .await;
    assert_eq!(source(&found), "state", "{found}");
    assert_eq!(ids(&found["data"]["widgets"]["nodes"]), ["w1"], "{found}");

    // No records of the type: generated, as before.
    let base = serve(SDL, Some(WORLD), vec![], UnknownIds::Inferred).await;
    let fresh = gql(&base, "/graphql", "{ widgets(first: 2) { nodes { id } } }").await;
    assert!(fresh.get("errors").is_none(), "{fresh}");
    assert_ne!(source(&fresh), "state", "{fresh}");
}

#[tokio::test]
async fn a_lookup_by_an_alternate_key_finds_the_held_record_or_answers_not_found() {
    let base = serve(SDL, Some(WORLD), widgets(), UnknownIds::NotFound).await;
    let held = gql(
        &base,
        "/graphql",
        r#"{ widget(slug: "cherry") { id title } }"#,
    )
    .await;
    assert_eq!(held["data"]["widget"]["id"], "w3", "{held}");
    assert_eq!(held["data"]["widget"]["title"], "Cherry", "{held}");

    let missing = gql(
        &base,
        "/graphql",
        r#"{ widget(slug: "no-such") { id title } }"#,
    )
    .await;
    assert_eq!(missing["data"]["widget"], Value::Null, "{missing}");
    let error = &missing["errors"][0];
    assert_eq!(error["extensions"]["code"], "NOT_FOUND", "{missing}");
    assert_eq!(error["path"], json!(["widget"]), "{missing}");
    // A lookup by the identity itself keeps its answer (generated here).
    let by_id = gql(&base, "/graphql", r#"{ widget(id: "w9") { id } }"#).await;
    assert!(by_id.get("errors").is_none(), "{by_id}");
    assert_eq!(by_id["data"]["widget"]["id"], "w9", "{by_id}");

    // Left `inferred`, a type no operation creates is reference data: generated, as
    // before (and so is a creatable one before the session writes one).
    let base = serve(SDL, Some(WORLD), widgets(), UnknownIds::Inferred).await;
    let made_up = gql(
        &base,
        "/graphql",
        r#"{ widget(slug: "no-such") { id slug } }"#,
    )
    .await;
    assert!(made_up.get("errors").is_none(), "{made_up}");
    assert_eq!(made_up["data"]["widget"]["slug"], "no-such", "{made_up}");
}

const GITHUB: &str = r"
type Query { user(login: String!): User }
type User { id: ID! login: String! name: String }
";

#[tokio::test]
async fn an_unknown_login_is_not_found_under_404() {
    let octo = StoredResource {
        resource_type: "user".into(),
        id: "U_1".into(),
        data: json!({"__typename": "User", "id": "U_1", "login": "octocat", "name": "Mona"}),
    };
    let base = serve(GITHUB, None, vec![octo], UnknownIds::NotFound).await;
    let missing = gql(
        &base,
        "/graphql",
        r#"{ user(login: "nobody-here") { id login } }"#,
    )
    .await;
    assert_eq!(missing["data"]["user"], Value::Null, "{missing}");
    assert_eq!(
        missing["errors"][0]["extensions"]["code"], "NOT_FOUND",
        "{missing}"
    );
    // GitHub's form: the code is the top-level `type` too, which clients test for.
    let error = &missing["errors"][0];
    assert_eq!(error["type"], "NOT_FOUND", "{missing}");
    assert_eq!(
        error["message"], "Could not resolve to a User with the login of 'nobody-here'.",
        "{missing}"
    );
    assert_eq!(error["path"], json!(["user"]), "{missing}");
    let held = gql(
        &base,
        "/graphql",
        r#"{ user(login: "octocat") { id name } }"#,
    )
    .await;
    assert!(held.get("errors").is_none(), "{held}");
    assert_eq!(held["data"]["user"]["id"], "U_1", "{held}");
}

const STOREFRONT: &str =
    include_str!("../../../../examples/apis/shopify-storefront-graphql/shopify-storefront.graphql");
const STOREFRONT_WORLD: &str = include_str!(
    "../../../../examples/apis/shopify-storefront-graphql/shopify-storefront.world.yaml"
);

fn product(n: u32, handle: &str, title: &str, price: &str) -> StoredResource {
    let id = format!("gid://shopify/Product/{n}");
    StoredResource {
        resource_type: "product".into(),
        id: id.clone(),
        data: json!({"__typename": "Product", "id": id, "handle": handle, "title": title,
            "priceRange": {"minVariantPrice": {"amount": price, "currencyCode": "USD"},
                           "maxVariantPrice": {"amount": price, "currencyCode": "USD"}}}),
    }
}

#[tokio::test]
async fn the_storefront_lists_and_collects_its_seeded_products() {
    let fig = product(1, "fiddle-leaf-fig", "Fiddle Leaf Fig", "34.0");
    let pothos = product(2, "pothos", "Pothos", "12.0");
    let collection = StoredResource {
        resource_type: "collection".into(),
        id: "gid://shopify/Collection/7".into(),
        data: json!({"__typename": "Collection", "id": "gid://shopify/Collection/7",
            "handle": "indoor-plants", "title": "Indoor Plants",
            "products": {"edges": [{"node": {"id": "gid://shopify/Product/2"}},
                {"node": {"id": "gid://shopify/Product/404", "title": "Never read"}}]}}),
    };
    let base = serve(
        STOREFRONT,
        Some(STOREFRONT_WORLD),
        vec![fig, pothos, collection],
        UnknownIds::NotFound,
    )
    .await;
    let path = "/graphql";
    let listed = gql(
        &base,
        path,
        "{ products(first: 100) { edges { node { id handle title \
         priceRange { minVariantPrice { amount } } } } pageInfo { hasNextPage } } }",
    )
    .await;
    assert!(listed.get("errors").is_none(), "{listed}");
    assert_eq!(source(&listed), "state", "{listed}");
    let edges = listed["data"]["products"]["edges"].as_array().unwrap();
    let handles: Vec<&str> = edges
        .iter()
        .map(|e| e["node"]["handle"].as_str().unwrap())
        .collect();
    assert_eq!(handles, ["fiddle-leaf-fig", "pothos"], "{listed}");
    let cheapest = gql(
        &base,
        path,
        "{ products(first: 1, sortKey: PRICE) { nodes { handle } } }",
    )
    .await;
    assert_eq!(
        cheapest["data"]["products"]["nodes"][0]["handle"], "pothos",
        "{cheapest}"
    );

    let collected = gql(
        &base,
        path,
        r#"{ collection(handle: "indoor-plants") { id title
             products(first: 10) { edges { node { id handle title } } } } }"#,
    )
    .await;
    assert!(collected.get("errors").is_none(), "{collected}");
    let products = &collected["data"]["collection"]["products"]["edges"];
    assert_eq!(products.as_array().unwrap().len(), 1, "{collected}");
    assert_eq!(products[0]["node"]["handle"], "pothos", "{collected}");
    assert_eq!(products[0]["node"]["title"], "Pothos", "{collected}");
    // The item the session holds no record of was not shown by the generated answer:
    // it is left out, and nothing is drawn for it or stored.
    let again = gql(&base, path, "{ products(first: 100) { nodes { id } } }").await;
    assert_eq!(
        again["data"]["products"]["nodes"].as_array().unwrap().len(),
        2,
        "{again}"
    );

    let missing = gql(
        &base,
        path,
        r#"{ product(handle: "no-such") { id title } }"#,
    )
    .await;
    assert_eq!(missing["data"]["product"], Value::Null, "{missing}");
    assert_eq!(
        missing["errors"][0]["extensions"]["code"], "NOT_FOUND",
        "{missing}"
    );
}

/// Widgets with a status, a soft delete, and a create.
const STATUSED: &str = r#"
enum WidgetStatus { ACTIVE ARCHIVED }
type Query {
  widgets(first: Int, after: String, showArchived: Boolean): WidgetConnection!
  widget(id: ID, slug: String): Widget
}
type Mutation {
  createWidget(slug: String!, title: String!): Widget
  deleteWidget(id: ID!): Widget
}
type Widget { id: ID! slug: String! title: String! status: WidgetStatus! }
type WidgetEdge { cursor: String! node: Widget! }
type PageInfo { hasNextPage: Boolean! hasPreviousPage: Boolean! startCursor: String endCursor: String }
type WidgetConnection { edges: [WidgetEdge!]! nodes: [Widget!]! pageInfo: PageInfo! }
"#;

const STATUSED_WORLD: &str = r"
version: 1
types:
  widget:
    identity: { member: id, alternates: [slug] }
    lifecycle:
      member: status
      states: [ACTIVE, ARCHIVED]
      soft_delete: { state: ARCHIVED, listed_with: { param: showArchived, value: true } }
operations:
  Query.widget:
    target: { member: slug }
  Query.widgets:
    kind: list
    type: widget
";

fn statused(id: &str, slug: &str, status: &str) -> StoredResource {
    StoredResource {
        resource_type: "widget".into(),
        id: id.into(),
        data: json!({"__typename": "Widget", "id": id, "slug": slug, "title": slug,
            "status": status}),
    }
}

#[tokio::test]
async fn a_soft_deleted_record_is_left_out_unless_asked_for() {
    let base = serve(
        STATUSED,
        Some(STATUSED_WORLD),
        vec![statused("a", "a", "ACTIVE"), statused("b", "b", "ARCHIVED")],
        UnknownIds::Inferred,
    )
    .await;
    let listed = gql(&base, "/graphql", "{ widgets(first: 5) { nodes { id } } }").await;
    assert_eq!(source(&listed), "state", "{listed}");
    assert_eq!(ids(&listed["data"]["widgets"]["nodes"]), ["a"], "{listed}");
    let all = gql(
        &base,
        "/graphql",
        "{ widgets(first: 5, showArchived: true) { nodes { id } } }",
    )
    .await;
    assert_eq!(ids(&all["data"]["widgets"]["nodes"]), ["a", "b"], "{all}");
}

#[tokio::test]
async fn a_null_sort_value_goes_last_under_reverse_too() {
    let mut held = widgets();
    held.push(StoredResource {
        resource_type: "widget".into(),
        id: "w4".into(),
        data: json!({"__typename": "Widget", "id": "w4", "slug": "plum", "title": "Plum",
            "createdAt": "2024-04-01T00:00:00Z", "price": null}),
    });
    let base = serve(SDL, Some(WORLD), held, UnknownIds::Inferred).await;
    for (reverse, want) in [
        (false, ["w2", "w1", "w3", "w4"]),
        (true, ["w3", "w1", "w2", "w4"]),
    ] {
        let q = format!(
            "{{ widgets(first: 5, sortKey: PRICE, reverse: {reverse}) {{ nodes {{ id }} }} }}"
        );
        let sorted = gql(&base, "/graphql", &q).await;
        assert_eq!(ids(&sorted["data"]["widgets"]["nodes"]), want, "{sorted}");
    }
    // `first` then `last`, as Relay applies them.
    let both = gql(
        &base,
        "/graphql",
        "{ widgets(first: 3, last: 1) { nodes { id } } }",
    )
    .await;
    assert_eq!(ids(&both["data"]["widgets"]["nodes"]), ["w3"], "{both}");
}

#[tokio::test]
async fn a_forced_status_or_a_scenario_wins_over_state() {
    let base = serve_with(SDL, Some(WORLD), widgets(), |rt| {
        rt.unknown_ids = UnknownIds::NotFound;
        rt.scenarios.insert(
            "quiet".into(),
            mb_protocols::ScenarioRuntime {
                spec: mb_engine::ScenarioSpec {
                    name: "quiet".into(),
                    ..mb_engine::ScenarioSpec::default()
                },
                ..mb_protocols::ScenarioRuntime::default()
            },
        );
    })
    .await;
    let unknown = r#"{ widget(slug: "no-such") { id slug } }"#;
    let list = "{ widgets(first: 5) { nodes { id } } }";
    for headers in [
        [("x-mockingbird-status", "200")],
        [("x-mockingbird-scenario", "quiet")],
    ] {
        let found = gql_with(&base, "/graphql", unknown, &headers).await;
        assert!(found.get("errors").is_none(), "{headers:?}: {found}");
        assert_eq!(found["data"]["widget"]["slug"], "no-such", "{found}");
        let listed = gql_with(&base, "/graphql", list, &headers).await;
        assert_ne!(source(&listed), "state", "{headers:?}: {listed}");
    }
    // Without either, state answers (the slug read above was stored by its answer).
    let missing = gql(&base, "/graphql", r#"{ widget(slug: "other") { id } }"#).await;
    assert_eq!(
        missing["errors"][0]["extensions"]["code"], "NOT_FOUND",
        "{missing}"
    );
}

#[tokio::test]
async fn inferred_answers_not_found_once_the_session_writes_the_type() {
    let base = serve(STATUSED, Some(STATUSED_WORLD), vec![], UnknownIds::Inferred).await;
    let unknown = r#"{ widget(slug: "no-such") { id slug } }"#;
    // Before any write: generated, as before.
    let before = gql(&base, "/graphql", unknown).await;
    assert!(before.get("errors").is_none(), "{before}");
    assert_eq!(before["data"]["widget"]["slug"], "no-such", "{before}");
    let created = gql(
        &base,
        "/graphql",
        r#"mutation { createWidget(slug: "made", title: "Made") { id slug } }"#,
    )
    .await;
    assert!(created.get("errors").is_none(), "{created}");
    // The session writes widgets now: a slug it never saw is missing.
    let after = gql(
        &base,
        "/graphql",
        r#"{ widget(slug: "never-seen") { id slug } }"#,
    )
    .await;
    assert_eq!(after["data"]["widget"], Value::Null, "{after}");
    assert_eq!(
        after["errors"][0]["extensions"]["code"], "NOT_FOUND",
        "{after}"
    );
    // The one it created is found.
    let made = gql(
        &base,
        "/graphql",
        r#"{ widget(slug: "made") { slug title } }"#,
    )
    .await;
    assert!(made.get("errors").is_none(), "{made}");
    assert_eq!(made["data"]["widget"]["title"], "Made", "{made}");
}

const GITHUB_REPOS: &str = r"
type Query {
  user(login: String!): User
  users(first: Int): [User!]!
  repository(owner: String!, name: String!): Repository
  repositoryById(id: ID!, name: String): Repository
}
type User { id: ID! login: String! name: String repository(name: String!): Repository }
type Repository { id: ID! name: String! stargazerCount: Int! }
";

fn github_records() -> Vec<StoredResource> {
    vec![
        StoredResource {
            resource_type: "user".into(),
            id: "U_1".into(),
            data: json!({"__typename": "User", "id": "U_1", "login": "octocat", "name": "Mona"}),
        },
        StoredResource {
            resource_type: "repository".into(),
            id: "R_1".into(),
            data: json!({"__typename": "Repository", "id": "R_1", "name": "hello",
                "stargazerCount": 3}),
        },
    ]
}

#[tokio::test]
async fn an_unknown_nested_lookup_is_null_and_not_found_at_its_path() {
    let base = serve(GITHUB_REPOS, None, github_records(), UnknownIds::NotFound).await;
    let missing = gql(
        &base,
        "/graphql",
        r#"{ user(login: "octocat") { login repo: repository(name: "nope") { name } } }"#,
    )
    .await;
    assert_eq!(missing["data"]["user"]["login"], "octocat", "{missing}");
    assert_eq!(missing["data"]["user"]["repo"], Value::Null, "{missing}");
    assert_eq!(
        missing["errors"][0]["extensions"]["code"], "NOT_FOUND",
        "{missing}"
    );
    assert_eq!(
        missing["errors"][0]["path"],
        json!(["user", "repo"]),
        "{missing}"
    );
    assert_eq!(
        missing["errors"].as_array().map(Vec::len),
        Some(1),
        "{missing}"
    );

    let held = gql(
        &base,
        "/graphql",
        r#"{ user(login: "octocat") { repository(name: "hello") { name } } }"#,
    )
    .await;
    assert!(held.get("errors").is_none(), "{held}");
    assert_eq!(
        held["data"]["user"]["repository"]["name"], "hello",
        "{held}"
    );

    // The root lookup by owner and name, too.
    let root = gql(
        &base,
        "/graphql",
        r#"{ repository(owner: "octocat", name: "nope") { name } }"#,
    )
    .await;
    assert_eq!(root["data"]["repository"], Value::Null, "{root}");
    assert_eq!(
        root["errors"][0]["extensions"]["code"], "NOT_FOUND",
        "{root}"
    );
}

#[tokio::test]
async fn a_nested_lookup_generates_unless_unknown_ids_answers_404() {
    let base = serve(GITHUB_REPOS, None, github_records(), UnknownIds::Generate).await;
    let answer = gql(
        &base,
        "/graphql",
        r#"{ user(login: "octocat") { repository(name: "nope") { name } } }"#,
    )
    .await;
    assert!(answer.get("errors").is_none(), "{answer}");
    assert_eq!(
        answer["data"]["user"]["repository"]["name"], "nope",
        "{answer}"
    );
}

#[tokio::test]
async fn a_nested_lookup_in_a_list_is_not_found_at_each_items_path() {
    let base = serve(GITHUB_REPOS, None, github_records(), UnknownIds::NotFound).await;
    let answer = gql(
        &base,
        "/graphql",
        r#"{ users(first: 2) { login repository(name: "nope") { name } } }"#,
    )
    .await;
    let users = answer["data"]["users"].as_array().expect("users");
    let errors = answer["errors"].as_array().expect("errors");
    assert_eq!(errors.len(), users.len(), "{answer}");
    for (i, user) in users.iter().enumerate() {
        assert_eq!(user["repository"], Value::Null, "{answer}");
        assert_eq!(
            errors[i]["path"],
            json!(["users", i, "repository"]),
            "{answer}"
        );
    }
}

#[tokio::test]
async fn a_root_lookup_by_id_keeps_its_answer_beside_a_member_argument() {
    let base = serve(GITHUB_REPOS, None, github_records(), UnknownIds::NotFound).await;
    let answer = gql(
        &base,
        "/graphql",
        r#"{ repositoryById(id: "R_9", name: "other") { name } }"#,
    )
    .await;
    assert_ne!(
        answer["errors"][0]["message"]
            .as_str()
            .map(|m| m.contains("name of")),
        Some(true),
        "{answer}"
    );
}

#[test]
fn the_lookups_by_a_key_other_than_the_identity_are_listed() {
    let bundle = compile(GITHUB_REPOS.as_bytes()).unwrap();
    let world = mb_mcm::world::tier0(&bundle);
    let applied = mb_mcm::world::apply(&bundle, &world, &world);
    let types = mb_protocols::unknown_id_types(&applied, &world, &Default::default(), true);
    let lookups: Vec<(&str, &str, Vec<&str>)> = types
        .lookups
        .iter()
        .map(|(op, (t, by))| {
            (
                op.as_str(),
                t.as_str(),
                by.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    assert_eq!(
        lookups,
        vec![
            ("Query.repository", "repository", vec!["name"]),
            ("Query.repositoryById", "repository", vec!["name"]),
            ("Query.user", "user", vec!["login"]),
        ]
    );
    // With consistency off no lookup is answered from state, so none is listed.
    let off = mb_protocols::unknown_id_types(&applied, &world, &Default::default(), false);
    assert!(off.lookups.is_empty());
}

#[tokio::test]
async fn a_batch_entrys_errors_carry_their_type() {
    let base = serve(GITHUB, None, vec![], UnknownIds::NotFound).await;
    let answer: Value = reqwest::Client::new()
        .post(format!("{base}/graphql"))
        .header("x-mockingbird-session", "s1")
        .json(&json!([
            {"query": r#"{ user(login: "nobody-here") { id } }"#},
            {"query": "{ nope }"},
        ]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = answer.as_array().expect("a batch answer");
    assert_eq!(entries[0]["data"]["user"], Value::Null, "{answer}");
    assert_eq!(entries[0]["errors"][0]["type"], "NOT_FOUND", "{answer}");
    // A validation error has no `extensions.code`, so no `type`.
    let invalid = &entries[1]["errors"][0];
    assert!(invalid["message"].is_string(), "{answer}");
    assert!(invalid["extensions"].get("code").is_none(), "{answer}");
    assert!(invalid.get("type").is_none(), "{answer}");
}

#[tokio::test]
async fn under_null_an_unknown_lookup_answers_null_without_an_error() {
    let fig = product(1, "fiddle-leaf-fig", "Fiddle Leaf Fig", "34.0");
    let base = serve(
        STOREFRONT,
        Some(STOREFRONT_WORLD),
        vec![fig],
        UnknownIds::Null,
    )
    .await;
    let missing = gql(
        &base,
        "/graphql",
        r#"{ product(handle: "no-such") { id title } }"#,
    )
    .await;
    assert_eq!(missing["data"]["product"], Value::Null, "{missing}");
    assert!(missing.get("errors").is_none(), "{missing}");
    let held = gql(
        &base,
        "/graphql",
        r#"{ product(handle: "fiddle-leaf-fig") { id title } }"#,
    )
    .await;
    assert!(held.get("errors").is_none(), "{held}");
    assert_eq!(
        held["data"]["product"]["title"], "Fiddle Leaf Fig",
        "{held}"
    );

    // Below the root too.
    let base = serve(GITHUB_REPOS, None, github_records(), UnknownIds::Null).await;
    let nested = gql(
        &base,
        "/graphql",
        r#"{ user(login: "octocat") { login repository(name: "nope") { name } } }"#,
    )
    .await;
    assert!(nested.get("errors").is_none(), "{nested}");
    assert_eq!(nested["data"]["user"]["login"], "octocat", "{nested}");
    assert_eq!(
        nested["data"]["user"]["repository"],
        Value::Null,
        "{nested}"
    );
}

#[tokio::test]
async fn under_null_a_non_null_lookup_keeps_its_error() {
    const STRICT: &str = r"
type Query { user(login: String!): User! }
type User { id: ID! login: String! }
";
    let octo = StoredResource {
        resource_type: "user".into(),
        id: "U_1".into(),
        data: json!({"__typename": "User", "id": "U_1", "login": "octocat"}),
    };
    let base = serve(STRICT, None, vec![octo], UnknownIds::Null).await;
    let missing = gql(
        &base,
        "/graphql",
        r#"{ user(login: "nobody-here") { id } }"#,
    )
    .await;
    assert_eq!(missing["data"], Value::Null, "{missing}");
    assert_eq!(missing["errors"][0]["type"], "NOT_FOUND", "{missing}");
}

const GITHUB_OWNED: &str = r"
type Query {
  user(login: String!): User
  repository(owner: String!, name: String!, locale: String): Repository
}
type User { id: ID! login: String! repository(name: String!): Repository }
type Owner { id: ID login: String }
type Repository { id: ID! name: String! nameWithOwner: String owner: Owner }
";

fn owned_records() -> Vec<StoredResource> {
    let repo = |id: &str, owner: &str, name: &str| StoredResource {
        resource_type: "repository".into(),
        id: id.into(),
        data: json!({"__typename": "Repository", "id": id, "name": name,
            "nameWithOwner": format!("{owner}/{name}"), "owner": {"login": owner}}),
    };
    vec![
        StoredResource {
            resource_type: "user".into(),
            id: "U_1".into(),
            data: json!({"__typename": "User", "id": "U_1", "login": "octocat"}),
        },
        repo("R_1", "vercel", "next.js"),
        repo("R_2", "octocat", "hello-world"),
        // Linked to its owner by id alone, and one showing no owner.
        StoredResource {
            resource_type: "repository".into(),
            id: "R_3".into(),
            data: json!({"__typename": "Repository", "id": "R_3", "name": "by-id", "owner": {"id": "U_1"}}),
        },
        StoredResource {
            resource_type: "repository".into(),
            id: "R_4".into(),
            data: json!({"__typename": "Repository", "id": "R_4", "name": "anyones"}),
        },
    ]
}

#[tokio::test]
async fn a_lookup_finds_only_a_record_under_the_parent_it_names() {
    let base = serve(GITHUB_OWNED, None, owned_records(), UnknownIds::NotFound).await;
    let q = |query: &'static str| {
        let base = base.clone();
        async move { gql(&base, "/graphql", query).await }
    };
    // Nested: octocat holds no next.js, though vercel's is held.
    let other = q(r#"{ user(login: "octocat") { repository(name: "next.js") { name } } }"#).await;
    assert_eq!(other["data"]["user"]["repository"], Value::Null, "{other}");
    assert_eq!(
        other["errors"][0]["path"],
        json!(["user", "repository"]),
        "{other}"
    );
    let own = q(r#"{ user(login: "octocat") { repository(name: "hello-world") { name } } }"#).await;
    assert!(own.get("errors").is_none(), "{own}");
    assert_eq!(
        own["data"]["user"]["repository"]["name"], "hello-world",
        "{own}"
    );
    // Root, the parent named by an argument that is no scalar member.
    let other = q(r#"{ repository(owner: "octocat", name: "next.js") { name } }"#).await;
    assert_eq!(other["data"]["repository"], Value::Null, "{other}");
    assert_eq!(
        other["errors"][0]["extensions"]["code"], "NOT_FOUND",
        "{other}"
    );
    let theirs = q(r#"{ repository(owner: "vercel", name: "next.js") { name } }"#).await;
    assert!(theirs.get("errors").is_none(), "{theirs}");
    // Logins compare without case.
    let cased = q(r#"{ repository(owner: "Vercel", name: "next.js") { name } }"#).await;
    assert!(cased.get("errors").is_none(), "{cased}");
    let cased =
        q(r#"{ user(login: "OctoCat") { repository(name: "hello-world") { name } } }"#).await;
    assert!(cased.get("errors").is_none(), "{cased}");
    // The parent's held record names it by id, though the query selects no `id`.
    let by_id = q(r#"{ user(login: "octocat") { repository(name: "by-id") { name } } }"#).await;
    assert!(by_id.get("errors").is_none(), "{by_id}");
    // A record showing no owner counts under any parent.
    let anyones = q(r#"{ user(login: "octocat") { repository(name: "anyones") { name } } }"#).await;
    assert!(anyones.get("errors").is_none(), "{anyones}");
    // An argument that is no member names no parent.
    let locale =
        q(r#"{ repository(owner: "vercel", name: "next.js", locale: "en") { name } }"#).await;
    assert!(locale.get("errors").is_none(), "{locale}");
}

const GITHUB_GISTS: &str = r"
type Query {
  user(login: String!): User
  viewer: User!
}
type User { id: ID! login: String! name: String bio: String gist(name: String!): Gist }
type Gist { id: ID! name: String! description: String owner: User }
";

#[tokio::test]
async fn a_gist_read_leaves_its_seeded_owner_as_seeded() {
    let seeded = vec![
        StoredResource {
            resource_type: "user".into(),
            id: "U_1".into(),
            data: json!({"__typename": "User", "id": "U_1", "login": "octocat",
                "name": "The Octocat", "bio": "Seeded bio"}),
        },
        StoredResource {
            resource_type: "gist".into(),
            id: "G_1".into(),
            data: json!({"__typename": "Gist", "id": "G_1", "name": "g1",
                "description": "Hello", "owner": {"__typename": "User", "id": "U_1", "login": "octocat"}}),
        },
    ];
    for mode in [UnknownIds::Inferred, UnknownIds::NotFound] {
        let base = serve(GITHUB_GISTS, None, seeded.clone(), mode).await;
        let gist = gql(
            &base,
            "/graphql",
            r#"{ viewer { gist(name: "g1") { description owner { login name } } } }"#,
        )
        .await;
        assert_eq!(
            gist["data"]["viewer"]["gist"]["owner"]["name"], "The Octocat",
            "{gist}"
        );
        // With the owner's id selected too.
        let gist = gql(
            &base,
            "/graphql",
            r#"{ viewer { gist(name: "g1") { owner { id login name } } } }"#,
        )
        .await;
        assert_eq!(
            gist["data"]["viewer"]["gist"]["owner"]["id"], "U_1",
            "{gist}"
        );
        assert_eq!(
            gist["data"]["viewer"]["gist"]["owner"]["name"], "The Octocat",
            "{gist}"
        );
        let user = gql(
            &base,
            "/graphql",
            r#"{ user(login: "octocat") { id name bio } }"#,
        )
        .await;
        assert_eq!(
            user["data"]["user"]["name"], "The Octocat",
            "{mode:?} {gist} {user}"
        );
        assert_eq!(
            user["data"]["user"]["bio"], "Seeded bio",
            "{mode:?} {gist} {user}"
        );
    }
}
