//! World expressions, phase 3: a list's `list.where`, a default item filter over each
//! record as held, applied before paging, scope, filters and order; and its
//! `list.sort`, whose `by` turns the sort parameter as sent into the members to order
//! by, the parameter counting as described.
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness};

const SHOP: &str = r##"
openapi: 3.0.0
info: {title: Shop, version: "1"}
paths:
  /products:
    get:
      operationId: listProducts
      parameters:
        - {name: status, in: query, schema: {type: string}}
        - {name: category, in: query, schema: {type: string}}
        - {name: sort, in: query, schema: {type: string}}
        - {name: order, in: query, style: deepObject, explode: true, schema: {type: object}}
        - {name: from, in: query, schema: {type: string}}
        - {name: rows, in: query, schema: {type: integer}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [products]
                properties:
                  products: {type: array, items: {$ref: "#/components/schemas/Product"}}
                  continuation: {type: string}
    post:
      operationId: createProduct
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Product"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Product"}}}}}
  /products/{productId}:
    get:
      operationId: getProduct
      parameters: [{name: productId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Product"}}}}}
components:
  schemas:
    Product:
      type: object
      required: [id]
      properties:
        id: {type: string}
        name: {type: string}
        price: {type: number}
        status: {type: string}
        category: {type: string}
        rank: {type: integer}
"##;

/// One sort expression for `-price`, `price:desc`, `price:asc`, `price` and lists of
/// them (`category,-price`).
const SORT_BY: &str = "value.split(',').map(p, trim(p).startsWith('-') \
    ? {'member': trim(p).substring(1), 'desc': true} \
    : {'member': trim(p).split(':')[0], 'desc': trim(p).endsWith(':desc')})";

fn product(id: &str, name: &str, price: Value, status: &str, category: &str) -> Value {
    let mut p = json!({"id": id, "name": name, "status": status, "category": category});
    if !price.is_null() {
        p["price"] = price;
    }
    p
}

/// Six products, two archived; `p6` has no price.
fn six() -> Vec<Value> {
    vec![
        product("p1", "Anvil", json!(30), "active", "tools"),
        product("p2", "Bolt", json!(5), "archived", "tools"),
        product("p3", "Chair", json!(80), "active", "home"),
        product("p4", "Drill", json!(120), "active", "tools"),
        product("p5", "Easel", json!(45), "archived", "home"),
        product("p6", "Funnel", Value::Null, "active", "home"),
    ]
}

/// A list of products ordered by name, paged by a cursor, filtered by category and
/// status, holding only the active ones unless a status is asked for, sorted by `sort`.
fn world(list: Value) -> Value {
    json!({"operations": {"listProducts": {
        "kind": "list",
        "type": "products",
        "order": {"member": "name"},
        "filters": [
            {"param": "category", "member": "category", "kind": "eq"},
            {"param": "status", "member": "status", "kind": "eq"}
        ],
        "paging": {"style": "cursor", "request": "from", "response": "continuation", "size": "rows"},
        "list": list
    }}})
}

fn default_list() -> Value {
    json!({
        "where": "has(request.query.status) || it.status == 'active'",
        "sort": {"param": "sort", "by": SORT_BY}
    })
}

async fn shop(list: Value) -> DescribedHarness {
    let h = support::described(SHOP, world(list), Vec::new());
    h.hold("products", &six()).await;
    h
}

async fn get(h: &DescribedHarness, uri: &str) -> Value {
    let (status, _, body) = h.send("GET", uri, None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

fn ids(body: &Value) -> Vec<&str> {
    body["products"]
        .as_array()
        .map(|items| items.iter().filter_map(|i| i["id"].as_str()).collect())
        .unwrap_or_default()
}

/// The request record of the last call.
fn last_request(h: &DescribedHarness) -> Value {
    h.records()
        .last()
        .map(|r| r.request.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn where_filters_before_paging() {
    let h = shop(default_list()).await;
    let first = get(&h, "/products?rows=2").await;
    assert_eq!(ids(&first), vec!["p1", "p3"], "{first}");
    let token = first["continuation"]
        .as_str()
        .expect("a next page")
        .to_string();
    let second = get(&h, &format!("/products?rows=2&from={token}")).await;
    assert_eq!(ids(&second), vec!["p4", "p6"], "{second}");
    assert!(
        second.get("continuation").is_none_or(Value::is_null),
        "the last page: {second}"
    );
    // Asking for a status lifts the default filter.
    let archived = get(&h, "/products?status=archived").await;
    assert_eq!(ids(&archived), vec!["p2", "p5"], "{archived}");
    assert!(h.provider.requests().is_empty());
    assert!(last_request(&h).get("exec_fallback").is_none());
}

#[tokio::test]
async fn where_combines_with_a_described_filter() {
    let h = shop(default_list()).await;
    let tools = get(&h, "/products?category=tools").await;
    assert_eq!(ids(&tools), vec!["p1", "p4"], "{tools}");
    let home = get(&h, "/products?category=home&status=archived").await;
    assert_eq!(ids(&home), vec!["p5"], "{home}");
}

#[tokio::test]
async fn sort_minus_price_price_desc_and_multi_key() {
    let h = shop(default_list()).await;
    // Largest first; a product without a price last.
    let minus = get(&h, "/products?sort=-price").await;
    assert_eq!(ids(&minus), vec!["p4", "p3", "p1", "p6"], "{minus}");
    let desc = get(&h, "/products?sort=price:desc").await;
    assert_eq!(ids(&desc), ids(&minus), "{desc}");
    let asc = get(&h, "/products?sort=price").await;
    assert_eq!(ids(&asc), vec!["p1", "p3", "p4", "p6"], "{asc}");
    // By category, then by price largest first.
    let multi = get(&h, "/products?sort=category,-price").await;
    assert_eq!(ids(&multi), vec!["p3", "p6", "p4", "p1"], "{multi}");
    // Sorted before the page is cut: the second page continues the order.
    let first = get(&h, "/products?sort=-price&rows=3").await;
    assert_eq!(ids(&first), vec!["p4", "p3", "p1"], "{first}");
    let token = first["continuation"]
        .as_str()
        .expect("a next page")
        .to_string();
    let second = get(&h, &format!("/products?sort=-price&rows=3&from={token}")).await;
    assert_eq!(ids(&second), vec!["p6"], "{second}");
    assert!(h.provider.requests().is_empty());
    assert!(last_request(&h).get("expr_failed").is_none());
}

#[tokio::test]
async fn a_bad_sort_value_falls_back_on_the_described_order() {
    let h = shop(default_list()).await;
    let body = get(&h, "/products?sort=-weight").await;
    assert_eq!(ids(&body), vec!["p1", "p3", "p4", "p6"], "{body}");
    let request = last_request(&h);
    let failed = &request["expr_failed"];
    assert_eq!(
        failed[0]["pointer"],
        json!("/operations/listProducts/list/sort/by"),
        "{request}"
    );
    assert!(
        failed[0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("weight")),
        "{failed}"
    );
    // A `by` that fails at runtime falls back too.
    let failing = json!({"sort": {"param": "sort", "by": "value * 2"}});
    let h = shop(failing).await;
    let body = get(&h, "/products?sort=price&status=active").await;
    assert_eq!(ids(&body), vec!["p1", "p3", "p4", "p6"], "{body}");
    assert_eq!(
        last_request(&h)["expr_failed"][0]["pointer"],
        json!("/operations/listProducts/list/sort/by")
    );
}

#[tokio::test]
async fn the_sort_param_counts_as_described() {
    // Without `list.sort`, `sort` is a parameter the description does not read.
    let h = support::described(
        SHOP,
        json!({"operations": {"listProducts": {"kind": "list", "type": "products", "order": {"member": "name"}}}}),
        vec![json!({"products": []})],
    );
    h.hold("products", &six()).await;
    get(&h, "/products?sort=-price").await;
    assert_eq!(last_request(&h)["exec_fallback"], "undescribed_param");
    // With it, the list is answered from state.
    let h = shop(json!({"sort": {"param": "sort", "by": SORT_BY}})).await;
    let body = get(&h, "/products?sort=-price").await;
    assert_eq!(
        ids(&body),
        vec!["p4", "p3", "p5", "p1", "p2", "p6"],
        "{body}"
    );
    assert!(last_request(&h).get("exec_fallback").is_none());
    assert!(h.provider.requests().is_empty());
}

#[tokio::test]
async fn a_bracketed_sort_reads_request_query() {
    // A deep object parameter arrives as an object under its name.
    let by = "[{'member': request.query.order['0'].field, 'desc': request.query.order['0'].dir == 'desc'}]";
    let h = shop(json!({"sort": {"param": "order", "by": by}})).await;
    let body = get(
        &h,
        "/products?order[0][field]=price&order[0][dir]=desc&status=active",
    )
    .await;
    assert_eq!(ids(&body), vec!["p4", "p3", "p1", "p6"], "{body}");
    assert!(last_request(&h).get("exec_fallback").is_none());
    assert!(last_request(&h).get("expr_failed").is_none());
    // `value` is the same object.
    let by = "[{'member': value['0'].field, 'desc': value['0'].dir == 'desc'}]";
    let h = shop(json!({"sort": {"param": "order", "by": by}})).await;
    let body = get(
        &h,
        "/products?order[0][field]=name&order[0][dir]=desc&status=active",
    )
    .await;
    assert_eq!(ids(&body), vec!["p6", "p4", "p3", "p1"], "{body}");
}

#[tokio::test]
async fn a_where_failing_on_one_record_drops_it_and_is_noted_once() {
    let mut records = six();
    records[0]["rank"] = json!(1);
    records[2]["rank"] = json!(2);
    let h = support::described(
        SHOP,
        world(json!({"where": "has(it.rank) ? it.rank / 0 > 1 : !has(it.price) || it.price > 10"})),
        Vec::new(),
    );
    h.hold("products", &records).await;
    let body = get(&h, "/products").await;
    // p2 (5) is left out by the condition, p1 and p3 by their failure.
    assert_eq!(ids(&body), vec!["p4", "p5", "p6"], "{body}");
    let failed = &last_request(&h)["expr_failed"];
    assert_eq!(failed.as_array().map(Vec::len), Some(1), "{failed}");
    assert_eq!(
        failed[0]["pointer"],
        json!("/operations/listProducts/list/where")
    );
}
