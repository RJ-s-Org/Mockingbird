//! A create whose input sends a member in another shape than the record holds it
//! (Shopify's `cartCreate(input: {lines: [CartLineInput]})` beside the cart's
//! connection of lines): when the world's `set` builds that member from the input,
//! the create is the executor's, stored and answered; without one it is generated
//! knowing the request (`reshaped_member`).
#![cfg(all(feature = "graphql", feature = "http-rest"))]

use serde_json::{Value, json};

use crate::support;

const SDL: &str = r#"
type ProductVariant { id: ID! }
type CartLine { quantity: Int! merchandise: ProductVariant! }
type CartLineConnection { nodes: [CartLine!]! }
type CartCost { totalAmount: Int! }
type Cart { id: ID! totalQuantity: Int! lines: CartLineConnection! cost: CartCost! }
type CartCreatePayload { cart: Cart }
input CartLineInput { merchandiseId: ID! quantity: Int }
input CartInput { lines: [CartLineInput!] }
type Query { cart(id: ID!): Cart }
type Mutation { cartCreate(input: CartInput): CartCreatePayload }
"#;

const SELECTION: &str =
    "id totalQuantity lines { nodes { merchandise { id } quantity } } cost { totalAmount }";

/// The cart's operations, the create's `set` being `set`.
fn world(set: Value) -> Value {
    json!({
        "version": 1,
        "types": {"cart": {"computed": {"totalQuantity": "sum(record.lines.nodes, it.quantity)"}}},
        "operations": {
            "Query.cart": {"kind": "read", "type": "cart", "target": {"member": "id"}},
            "Mutation.cartCreate": {"kind": "create", "type": "cart",
                "answer": {"record": "/cart"}, "set": set}
        }
    })
}

fn create() -> String {
    format!(
        r#"mutation {{ cartCreate(input: {{lines: [
            {{merchandiseId: "gid://shopify/ProductVariant/42", quantity: 2}},
            {{merchandiseId: "gid://shopify/ProductVariant/43", quantity: 3}}]}}) {{
            cart {{ {SELECTION} }} }} }}"#
    )
}

#[tokio::test]
async fn a_create_set_building_a_reshaped_member_from_the_input_is_stored_and_answered() {
    let set = json!({
        "lines": "{'nodes': request.body.input.lines.map(l, {'quantity': l.quantity, 'merchandise': {'id': l.merchandiseId}})}"
    });
    let h = support::graphql_described(
        mb_import::compile(SDL.as_bytes()).unwrap(),
        world(set),
        vec![],
    );
    let body = h.query(&create()).await;
    assert!(body.get("errors").is_none(), "{body}");
    let records = h.recorded.wait(1).await;
    assert_eq!(
        body["extensions"]["mockingbird"]["sources"]["cartCreate"], "state",
        "{body} {records:?}"
    );
    let cart = body["data"]["cartCreate"]["cart"].clone();
    let lines = json!([
        {"merchandise": {"id": "gid://shopify/ProductVariant/42"}, "quantity": 2},
        {"merchandise": {"id": "gid://shopify/ProductVariant/43"}, "quantity": 3}
    ]);
    assert_eq!(cart["lines"]["nodes"], lines, "{body}");
    assert_eq!(cart["totalQuantity"], 5, "{body}");
    assert!(h.provider.requests().is_empty());

    let held = h.held("cart").await;
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["lines"]["nodes"][1]["quantity"], 3, "{held:?}");

    let id = cart["id"].as_str().unwrap_or_else(|| panic!("{body}"));
    let read = h
        .query(&format!(r#"{{ cart(id: "{id}") {{ {SELECTION} }} }}"#))
        .await;
    assert_eq!(
        read["extensions"]["mockingbird"]["sources"]["cart"], "state",
        "{read}"
    );
    assert_eq!(read["data"]["cart"], cart, "{read}");
}

#[tokio::test]
async fn a_reshaped_member_no_set_builds_is_generated() {
    let generated = json!({"__typename": "CartCreatePayload", "cart": {"__typename": "Cart",
        "id": "gid://shopify/Cart/c1", "lines": {"__typename": "CartLineConnection", "nodes": []},
        "cost": {"__typename": "CartCost", "totalAmount": 0}}});
    let h = support::graphql_described(
        mb_import::compile(SDL.as_bytes()).unwrap(),
        world(json!({})),
        vec![generated.clone(), generated],
    );
    let body = h.query(&create()).await;
    assert!(body.get("errors").is_none(), "{body}");
    let records = h.recorded.wait(1).await;
    let fallbacks: Vec<&Value> = records
        .iter()
        .filter_map(|r| r.request.get("exec_fallback"))
        .collect();
    assert_eq!(fallbacks, vec![&json!("reshaped_member")], "{records:?}");
}

/// The input wrapping the cart (`input: {cart: {lines}}`), the create's record
/// described at `/input`: a `set` reads the lines by their whole body path.
const WRAPPED: &str = r#"
type ProductVariant { id: ID! }
type CartLine { quantity: Int! merchandise: ProductVariant! }
type CartLineConnection { nodes: [CartLine!]! }
type Cart { id: ID! lines: CartLineConnection! }
type CartCreatePayload { cart: Cart }
input CartLineInput { merchandiseId: ID! quantity: Int }
input CartInput { lines: [CartLineInput!] }
input CartCreateInput { cart: CartInput }
type Query { cart(id: ID!): Cart }
type Mutation { cartCreate(input: CartCreateInput): CartCreatePayload }
"#;

#[tokio::test]
async fn a_set_reads_a_reshaped_member_under_the_described_request_record() {
    let world = json!({
        "version": 1,
        "operations": {
            "Query.cart": {"kind": "read", "type": "cart", "target": {"member": "id"}},
            "Mutation.cartCreate": {"kind": "create", "type": "cart",
                "request": {"record": "/input"},
                "answer": {"record": "/cart"},
                "set": {"lines": "{'nodes': request.body.input.cart.lines.map(l, {'quantity': l.quantity, 'merchandise': {'id': l.merchandiseId}})}"}}
        }
    });
    let h = support::graphql_described(
        mb_import::compile(WRAPPED.as_bytes()).unwrap(),
        world,
        vec![],
    );
    let body = h
        .query(
            r#"mutation { cartCreate(input: {cart: {lines: [
                {merchandiseId: "gid://shopify/ProductVariant/42", quantity: 2}]}}) {
                cart { id lines { nodes { merchandise { id } quantity } } } } }"#,
        )
        .await;
    assert!(body.get("errors").is_none(), "{body}");
    assert_eq!(
        body["extensions"]["mockingbird"]["sources"]["cartCreate"], "state",
        "{body}"
    );
    assert_eq!(
        body["data"]["cartCreate"]["cart"]["lines"]["nodes"],
        json!([{"merchandise": {"id": "gid://shopify/ProductVariant/42"}, "quantity": 2}]),
        "{body}"
    );
}
