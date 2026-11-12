//! A create whose world computes or sets a member below the record's top (a Shopify
//! cart's `cost.totalAmount.amount`, the sum of its lines): the create is the
//! executor's, answering the nested member where the schema declares it, and a later
//! read answers it too. A nested `set` member is stored.
#![cfg(all(feature = "graphql", feature = "http-rest"))]

use serde_json::{Value, json};

use crate::support;

const SDL: &str = r#"
type MoneyV2 { amount: Float! currencyCode: String! }
type CartCost { totalAmount: MoneyV2! subtotalAmount: MoneyV2! }
type ProductVariant { id: ID! }
type CartLine { quantity: Int! price: Float! merchandise: ProductVariant! }
type CartLineConnection { nodes: [CartLine!]! }
type CartBuyerIdentity { countryCode: String }
type CartSummary { lineCount: Int! }
type Cart { id: ID! lines: CartLineConnection! cost: CartCost! buyerIdentity: CartBuyerIdentity
    summary: CartSummary }
type CartCreatePayload { cart: Cart }
input CartLineInput { merchandiseId: ID! quantity: Int price: Float }
input CartInput { lines: [CartLineInput!] country: String }
type Query { cart(id: ID!): Cart }
type Mutation { cartCreate(input: CartInput): CartCreatePayload }
"#;

const SELECTION: &str = "id lines { nodes { merchandise { id } quantity price } } \
    cost { __typename totalAmount { __typename amount currencyCode } \
    subtotalAmount { amount currencyCode } } \
    buyerIdentity { countryCode } summary { __typename lineCount }";

fn world() -> Value {
    let total = "sum(record.lines.nodes, double(it.quantity) * it.price)";
    json!({
        "version": 1,
        "types": {"cart": {"computed": {
            "cost.totalAmount.amount": total,
            "cost.subtotalAmount.amount": total,
            "summary.lineCount": "size(record.lines.nodes)"
        }}},
        "operations": {
            "Query.cart": {"kind": "read", "type": "cart", "target": {"member": "id"}},
            "Mutation.cartCreate": {"kind": "create", "type": "cart",
                "answer": {"record": "/cart"},
                "set": {
                    "lines": "{'nodes': request.body.input.lines.map(l, {'quantity': l.quantity, 'price': l.price, 'merchandise': {'id': l.merchandiseId}})}",
                    "cost.totalAmount.currencyCode": "'USD'",
                    "cost.subtotalAmount.currencyCode": "'USD'",
                    "buyerIdentity.countryCode": "request.body.input.country"
                }}
        }
    })
}

#[tokio::test]
async fn a_nested_computed_member_of_a_create_is_answered_and_read_back() {
    let h =
        support::graphql_described(mb_import::compile(SDL.as_bytes()).unwrap(), world(), vec![]);
    let body = h
        .query(&format!(
            r#"mutation {{ cartCreate(input: {{country: "CA", lines: [
                {{merchandiseId: "gid://shopify/ProductVariant/42", quantity: 2, price: 1.5}},
                {{merchandiseId: "gid://shopify/ProductVariant/43", quantity: 3, price: 2.0}}]}}) {{
                cart {{ {SELECTION} }} }} }}"#
        ))
        .await;
    assert!(body.get("errors").is_none(), "{body}");
    let records = h.recorded.wait(1).await;
    assert_eq!(
        body["extensions"]["mockingbird"]["sources"]["cartCreate"], "state",
        "{body} {records:?}"
    );
    let cart = body["data"]["cartCreate"]["cart"].clone();
    for money in ["totalAmount", "subtotalAmount"] {
        let money = &cart["cost"][money];
        assert_eq!(money["amount"].as_f64(), Some(9.0), "{body}");
        assert_eq!(money["currencyCode"], "USD", "{body}");
    }
    // The opened objects are of the types their schema declares.
    assert_eq!(cart["cost"]["__typename"], "CartCost", "{body}");
    assert_eq!(
        cart["cost"]["totalAmount"]["__typename"], "MoneyV2",
        "{body}"
    );
    // An object nothing but a computed member writes is opened for it.
    assert_eq!(cart["summary"]["lineCount"], 2, "{body}");
    assert_eq!(cart["summary"]["__typename"], "CartSummary", "{body}");
    assert_eq!(cart["buyerIdentity"]["countryCode"], "CA", "{body}");
    assert!(h.provider.requests().is_empty());

    // The nested `set` members are stored; the computed ones are answered from state.
    let held = h.held("cart").await;
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(
        held[0]["cost"]["totalAmount"]["currencyCode"], "USD",
        "{held:?}"
    );
    assert_eq!(held[0]["buyerIdentity"]["countryCode"], "CA", "{held:?}");
    assert_eq!(
        held[0]["cost"]["totalAmount"]["__typename"], "MoneyV2",
        "{held:?}"
    );
    assert_eq!(held[0]["summary"]["__typename"], "CartSummary", "{held:?}");

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
