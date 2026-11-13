//! A create whose request builds child items (`line_items[0][price]=…`) or a reference
//! array (`discounts[0][coupon]=…`) stores them in the record: its reads, its lists and
//! a sub-list of the items under it answer the items the request sent, never invented
//! ones (Stripe trial: a Checkout Session created with two prices read back with others).
//! A dotted expansion through a list embed (`line_items.data.price.product`) reaches the
//! held records the items name.
#![cfg(feature = "http-rest")]

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::Value;

use crate::support::{self, HarnessOptions};

/// Shaped as Stripe's contract: a session's `line_items` is a list object of items it
/// returns only when expanded, listed under the session too, with no read of its own;
/// an item's `price` is the price object; a price's `product` a choice of an id and the
/// product. An invoice's `discounts` is an array of a choice of an id and a discount.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Payments, version: "1"}
paths:
  /v1/products:
    post:
      operationId: PostProducts
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema: {type: object, required: [name], properties: {name: {type: string}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/product"}}}}}
  /v1/products/{id}:
    get:
      operationId: GetProductsId
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/product"}}}}}
  /v1/prices:
    post:
      operationId: PostPrices
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema:
              type: object
              required: [product, unit_amount]
              properties: {product: {type: string}, unit_amount: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/price"}}}}}
  /v1/prices/{price}:
    get:
      operationId: GetPricesPrice
      parameters:
        - {name: price, in: path, required: true, schema: {type: string}}
        - $ref: "#/components/parameters/expand"
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/price"}}}}}
  /v1/checkout/sessions:
    get:
      operationId: GetCheckoutSessions
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/session_list"}}}}}
    post:
      operationId: PostCheckoutSessions
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema:
              type: object
              properties:
                mode: {type: string, enum: [payment, setup, subscription]}
                line_items:
                  type: array
                  items:
                    type: object
                    properties: {price: {type: string}, quantity: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/checkout.session"}}}}}
  /v1/checkout/sessions/{session}:
    get:
      operationId: GetCheckoutSessionsSession
      parameters:
        - {name: session, in: path, required: true, schema: {type: string}}
        - $ref: "#/components/parameters/expand"
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/checkout.session"}}}}}
  /v1/checkout/sessions/{session}/line_items:
    get:
      operationId: GetCheckoutSessionsSessionLineItems
      parameters:
        - {name: session, in: path, required: true, schema: {type: string}}
        - $ref: "#/components/parameters/expand"
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/item_list"}}}}}
  /v1/invoices:
    post:
      operationId: PostInvoices
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema:
              type: object
              properties:
                customer: {type: string}
                discounts:
                  type: array
                  items: {type: object, properties: {coupon: {type: string}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/invoice"}}}}}
  /v1/invoices/{invoice}:
    get:
      operationId: GetInvoicesInvoice
      parameters:
        - {name: invoice, in: path, required: true, schema: {type: string}}
        - $ref: "#/components/parameters/expand"
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/invoice"}}}}}
  /v1/subscriptions:
    post:
      operationId: PostSubscriptions
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema:
              type: object
              properties:
                customer: {type: string}
                items:
                  type: array
                  items:
                    type: object
                    properties: {price: {type: string}, quantity: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/subscription"}}}}}
  /v1/subscriptions/{subscription}:
    get:
      operationId: GetSubscriptionsSubscription
      parameters: [{name: subscription, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/subscription"}}}}}
  /v1/subscription_items:
    get:
      operationId: GetSubscriptionItems
      parameters: [{name: subscription, in: query, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [object, data, has_more, url]
                properties:
                  object: {type: string, enum: [list]}
                  data: {type: array, items: {$ref: "#/components/schemas/subscription_item"}}
                  has_more: {type: boolean}
                  url: {type: string}
  /v1/subscription_items/{item}:
    get:
      operationId: GetSubscriptionItemsItem
      parameters: [{name: item, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/subscription_item"}}}}}
components:
  parameters:
    expand:
      name: expand
      in: query
      style: deepObject
      explode: true
      schema: {type: array, items: {type: string}}
  schemas:
    product:
      type: object
      required: [id, object, name]
      properties:
        id: {type: string}
        object: {type: string, enum: [product]}
        name: {type: string}
    price:
      type: object
      required: [id, object, product, unit_amount]
      properties:
        id: {type: string}
        object: {type: string, enum: [price]}
        unit_amount: {type: integer}
        product:
          anyOf:
            - {type: string}
            - $ref: "#/components/schemas/product"
    item:
      type: object
      required: [id, object, price, quantity]
      properties:
        id: {type: string}
        object: {type: string, enum: [item]}
        amount_total: {type: integer}
        price:
          nullable: true
          anyOf:
            - $ref: "#/components/schemas/price"
        quantity: {type: integer, nullable: true}
    item_list:
      type: object
      required: [data, has_more, object, url]
      properties:
        data: {type: array, items: {$ref: "#/components/schemas/item"}}
        has_more: {type: boolean}
        object: {type: string, enum: [list]}
        url: {type: string}
    checkout.session:
      type: object
      required: [id, object, mode]
      x-expandableFields: [line_items]
      properties:
        id: {type: string}
        object: {type: string, enum: [checkout.session]}
        mode: {type: string, enum: [payment, setup, subscription]}
        line_items:
          type: object
          required: [data, has_more, object, url]
          properties:
            data: {type: array, items: {$ref: "#/components/schemas/item"}}
            has_more: {type: boolean}
            object: {type: string, enum: [list]}
            url: {type: string}
    session_list:
      type: object
      properties:
        object: {type: string, enum: [list]}
        data: {type: array, items: {$ref: "#/components/schemas/checkout.session"}}
        has_more: {type: boolean}
        url: {type: string}
    subscription_item:
      type: object
      required: [id, object, price, quantity, subscription]
      properties:
        id: {type: string}
        object: {type: string, enum: [subscription_item]}
        price: {$ref: "#/components/schemas/price"}
        quantity: {type: integer}
        subscription: {type: string}
    subscription:
      type: object
      required: [id, object, items]
      properties:
        id: {type: string}
        object: {type: string, enum: [subscription]}
        customer: {type: string}
        items:
          type: object
          required: [object, data, has_more, url]
          properties:
            object: {type: string, enum: [list]}
            data: {type: array, items: {$ref: "#/components/schemas/subscription_item"}}
            has_more: {type: boolean}
            url: {type: string}
    coupon:
      type: object
      required: [id, object]
      properties:
        id: {type: string}
        object: {type: string, enum: [coupon]}
        percent_off: {type: number}
    discount:
      type: object
      required: [id, object, coupon]
      properties:
        id: {type: string}
        object: {type: string, enum: [discount]}
        coupon:
          anyOf:
            - {type: string}
            - $ref: "#/components/schemas/coupon"
    invoice:
      type: object
      required: [id, object, discounts]
      properties:
        id: {type: string}
        object: {type: string, enum: [invoice]}
        customer: {type: string, nullable: true}
        discounts:
          type: array
          items:
            anyOf:
              - {type: string}
              - $ref: "#/components/schemas/discount"
"##;

fn offline() -> support::DescribedHarness {
    let options = HarnessOptions {
        offline: true,
        ..HarnessOptions::default()
    };
    support::harness(CONTRACT, Vec::new(), &options, |rt| rt)
}

async fn post_form(h: &support::DescribedHarness, uri: &str, form: &str) -> Value {
    let router = mb_protocols::http_rest::router(Arc::clone(&h.rt));
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(form.to_owned()))
        .unwrap();
    let (status, _, body) = support::send_to(&router, req).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

async fn get(h: &support::DescribedHarness, path: &str) -> Value {
    let (status, _, body) = h.send("GET", path, None, &[]).await;
    assert!(status.is_success(), "{path}: {status} {body}");
    body
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

/// A session created with two prices answers them, with their quantities, on its read
/// (expanded), its sub-list of items, and through `line_items.data.price.product` to
/// the held product.
#[tokio::test]
async fn a_created_session_keeps_its_line_items() {
    let h = offline();
    let fig = post_form(&h, "/v1/products", "name=Fiddle+leaf+fig").await;
    let fern = post_form(&h, "/v1/products", "name=Boston+fern").await;
    let p1 = post_form(
        &h,
        "/v1/prices",
        &format!("product={}&unit_amount=4500", id(&fig)),
    )
    .await;
    let p2 = post_form(
        &h,
        "/v1/prices",
        &format!("product={}&unit_amount=1200", id(&fern)),
    )
    .await;
    let form = format!(
        "mode=payment&line_items[0][price]={}&line_items[0][quantity]=2\
         &line_items[1][price]={}&line_items[1][quantity]=1",
        id(&p1),
        id(&p2)
    );
    let session = post_form(&h, "/v1/checkout/sessions", &form).await;
    let sid = id(&session);

    let plain = get(&h, &format!("/v1/checkout/sessions/{sid}")).await;
    assert!(plain.get("line_items").is_none(), "{plain}");

    let expanded = get(
        &h,
        &format!("/v1/checkout/sessions/{sid}?expand[]=line_items"),
    )
    .await;
    let items = expanded["line_items"]["data"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{expanded}");
    assert_eq!(items[0]["price"]["id"], p1["id"], "{expanded}");
    assert_eq!(items[0]["quantity"], 2, "{expanded}");
    assert_eq!(items[1]["price"]["id"], p2["id"], "{expanded}");
    assert_eq!(items[1]["quantity"], 1, "{expanded}");
    assert_eq!(expanded["line_items"]["has_more"], false, "{expanded}");

    let listed = get(&h, &format!("/v1/checkout/sessions/{sid}/line_items")).await;
    let listed = listed["data"].as_array().unwrap();
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert_eq!(listed[0]["price"]["id"], p1["id"], "{listed:?}");
    assert_eq!(listed[1]["quantity"], 1, "{listed:?}");

    let deep = get(
        &h,
        &format!("/v1/checkout/sessions/{sid}?expand[]=line_items.data.price.product"),
    )
    .await;
    let items = deep["line_items"]["data"].as_array().unwrap();
    assert_eq!(items[0]["price"]["product"], fig, "{deep}");
    assert_eq!(items[1]["price"]["product"], fern, "{deep}");
    // Expansion is a view: the held price keeps its product's id.
    let price = get(&h, &format!("/v1/prices/{}", id(&p1))).await;
    assert_eq!(price["product"], fig["id"], "{price}");
}

/// An invoice created with two coupons answers two discounts naming them, on every read.
#[tokio::test]
async fn invoice_discounts_round_trip() {
    let h = offline();
    let invoice = post_form(
        &h,
        "/v1/invoices",
        "customer=cus_1&discounts[0][coupon]=co_1&discounts[1][coupon]=co_2",
    )
    .await;
    let read = get(
        &h,
        &format!("/v1/invoices/{}?expand[]=discounts", id(&invoice)),
    )
    .await;
    let discounts = read["discounts"].as_array().unwrap();
    assert_eq!(discounts.len(), 2, "{read}");
    assert_eq!(discounts[0]["coupon"], "co_1", "{read}");
    assert_eq!(discounts[1]["coupon"], "co_2", "{read}");
    // Each is completed as the discount the choice holds besides an id.
    for discount in discounts {
        assert_eq!(discount["object"], "discount", "{read}");
        assert!(discount["id"].is_string(), "{read}");
    }
    let again = get(
        &h,
        &format!("/v1/invoices/{}?expand[]=discounts", id(&invoice)),
    )
    .await;
    assert_eq!(again, read);
}

/// Items that are records of their own (a subscription's items, each read by id and
/// listed by the subscription it names) get ids of their own, name the subscription,
/// and are read back by id and in the subscription's list.
#[tokio::test]
async fn created_child_records_are_read_on_their_own() {
    let h = offline();
    let fig = post_form(&h, "/v1/products", "name=Fiddle+leaf+fig").await;
    let price = post_form(
        &h,
        "/v1/prices",
        &format!("product={}&unit_amount=4500", id(&fig)),
    )
    .await;
    let form = format!(
        "customer=cus_1&items[0][price]={p}&items[0][quantity]=3\
         &items[1][price]={p}&items[1][quantity]=1",
        p = id(&price)
    );
    let sub = post_form(&h, "/v1/subscriptions", &form).await;
    let sid = id(&sub);
    let read = get(&h, &format!("/v1/subscriptions/{sid}")).await;
    let items = read["items"]["data"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{read}");
    assert_ne!(items[0]["id"], items[1]["id"], "{read}");
    assert_eq!(read["items"]["has_more"], false, "{read}");
    for (item, quantity) in items.iter().zip([3, 1]) {
        assert_eq!(item["subscription"], sid.as_str(), "{read}");
        assert_eq!(item["quantity"], quantity, "{read}");
        assert_eq!(item["price"]["id"], price["id"], "{read}");
        let own = get(&h, &format!("/v1/subscription_items/{}", id(item))).await;
        assert_eq!(own["subscription"], sid.as_str(), "{own}");
        assert_eq!(own["quantity"], quantity, "{own}");
    }
    let listed = get(&h, &format!("/v1/subscription_items?subscription={sid}")).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 2, "{listed}");
}
