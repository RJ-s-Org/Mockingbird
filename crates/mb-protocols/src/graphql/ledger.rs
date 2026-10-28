//! Keeps a generated answer's money consistent: objects the request names
//! (a cart line's variant) take the prices the session stored for them,
//! then line and cart totals are recomputed from unit prices and
//! quantities (Shopify's `CartLine.cost`, `Cart.cost`, `totalQuantity`).

use mb_state::StoredResource;
use serde_json::{Map, Value};

/// Overlays each nested object of `body` whose `id` is a stored resource's
/// with that resource's scalar and value-object members (those without an
/// `id`), where the answer has them; then [`totals`]. The answer and its
/// top-level objects (a payload's `cart`) are what the call changes, so
/// they keep what was generated.
pub(crate) fn reconcile(body: &mut Value, stored: &[StoredResource]) {
    overlay(body, stored, 0);
    totals(body, 0);
}

/// Bounds the walks (recursive answers).
const MAX_DEPTH: usize = 12;

/// The depth of the first objects [`overlay`] may change: below the answer
/// (0) and its members (1).
const NESTED: usize = 2;

fn overlay(value: &mut Value, stored: &[StoredResource], depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::Object(obj) => {
            let hit = obj
                .get("id")
                .and_then(Value::as_str)
                .filter(|_| depth >= NESTED)
                .and_then(|id| stored.iter().find(|r| r.id == id));
            if let Some(Value::Object(truth)) = hit.map(|r| &r.data) {
                for (k, v) in obj.iter_mut() {
                    if k == "__typename" || k == "id" {
                        continue;
                    }
                    if let Some(t) = truth.get(k) {
                        merge_values(v, t);
                    }
                }
            }
            for v in obj.values_mut() {
                overlay(v, stored, depth + 1);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| overlay(v, stored, depth + 1)),
        _ => {}
    }
}

/// `answer` takes `truth`'s scalars; objects without an `id` merge member
/// by member; lists and identified objects are left alone.
fn merge_values(answer: &mut Value, truth: &Value) {
    match (answer, truth) {
        (Value::Object(a), Value::Object(t)) if !t.contains_key("id") => {
            for (k, v) in a.iter_mut() {
                if k != "__typename"
                    && let Some(tv) = t.get(k)
                {
                    merge_values(v, tv);
                }
            }
        }
        (a @ (Value::String(_) | Value::Number(_) | Value::Bool(_)), t)
            if matches!(t, Value::String(_) | Value::Number(_) | Value::Bool(_)) =>
        {
            *a = t.clone();
        }
        _ => {}
    }
}

/// An amount as written (`"32.00"`, `32`), as a number.
fn amount(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// `x` in the form `like` was written: a two-decimal string, or a number.
fn written_like(x: f64, like: &Value) -> Value {
    let cents = (x * 100.0).round() / 100.0;
    match like {
        Value::String(_) => Value::String(format!("{cents:.2}")),
        _ => serde_json::Number::from_f64(cents).map_or(Value::Null, Value::Number),
    }
}

/// Sets `obj.<money>.amount`, when the answer has it, to `x`.
fn set_amount(obj: &mut Map<String, Value>, money: &str, x: f64) {
    if let Some(slot) = obj
        .get_mut(money)
        .and_then(Value::as_object_mut)
        .and_then(|m| m.get_mut("amount"))
    {
        *slot = written_like(x, slot);
    }
}

/// The line objects of a `lines` member: a list, or a connection's
/// `edges[].node` or `nodes`.
fn lines_mut(lines: &mut Value) -> Vec<&mut Map<String, Value>> {
    match lines {
        Value::Array(items) => items.iter_mut().filter_map(Value::as_object_mut).collect(),
        Value::Object(conn) => {
            if conn.get("edges").is_some_and(Value::is_array) {
                conn.get_mut("edges")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.get_mut("node")?.as_object_mut())
                    .collect()
            } else {
                conn.get_mut("nodes")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_object_mut)
                    .collect()
            }
        }
        _ => Vec::new(),
    }
}

/// The unit price of a line: its merchandise's `price.amount`, else its
/// cost's `amountPerQuantity.amount`.
fn unit_price(line: &Map<String, Value>) -> Option<f64> {
    line.get("merchandise")
        .and_then(|m| m.get("price"))
        .and_then(|p| p.get("amount"))
        .and_then(amount)
        .or_else(|| {
            line.get("cost")
                .and_then(|c| c.get("amountPerQuantity"))
                .and_then(|p| p.get("amount"))
                .and_then(amount)
        })
}

/// Recomputes, in every object with `lines`, each priced line's
/// `cost.totalAmount`/`subtotalAmount`, and when every line is priced the
/// object's `cost.subtotalAmount`, `cost.totalAmount` (plus any
/// `totalTaxAmount`) and `totalQuantity`. Only members the answer has are
/// set.
fn totals(value: &mut Value, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::Object(obj) => {
            for v in obj.values_mut() {
                totals(v, depth + 1);
            }
            let Some(lines) = obj.get_mut("lines") else {
                return;
            };
            let mut sum = 0.0;
            let mut quantity = 0;
            let mut priced = true;
            let mut any = false;
            for line in lines_mut(lines) {
                any = true;
                let qty = line.get("quantity").and_then(Value::as_i64);
                let (Some(qty), Some(unit)) = (qty, unit_price(line)) else {
                    priced = false;
                    continue;
                };
                let total = unit * qty as f64;
                if let Some(Value::Object(cost)) = line.get_mut("cost") {
                    set_amount(cost, "totalAmount", total);
                    set_amount(cost, "subtotalAmount", total);
                }
                sum += total;
                quantity += qty;
            }
            if !any || !priced {
                return;
            }
            if let Some(Value::Object(cost)) = obj.get_mut("cost") {
                let tax = cost
                    .get("totalTaxAmount")
                    .and_then(|t| t.get("amount"))
                    .and_then(amount)
                    .unwrap_or(0.0);
                set_amount(cost, "subtotalAmount", sum);
                set_amount(cost, "totalAmount", sum + tax);
            }
            if let Some(slot) = obj.get_mut("totalQuantity")
                && slot.is_number()
            {
                *slot = Value::from(quantity);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| totals(v, depth + 1)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn totals_of_a_list_cart_with_tax() {
        let mut cart = json!({"totalQuantity": 9, "lines": [
            {"quantity": 2, "cost": {"amountPerQuantity": {"amount": "8.50"}, "totalAmount": {"amount": "1"}}},
            {"quantity": 1, "merchandise": {"price": {"amount": 10}}}
        ], "cost": {"subtotalAmount": {"amount": "0"}, "totalTaxAmount": {"amount": "2.25"}, "totalAmount": {"amount": "0"}}});
        totals(&mut cart, 0);
        assert_eq!(cart["lines"][0]["cost"]["totalAmount"]["amount"], "17.00");
        assert_eq!(cart["cost"]["subtotalAmount"]["amount"], "27.00");
        assert_eq!(cart["cost"]["totalAmount"]["amount"], "29.25");
        assert_eq!(cart["totalQuantity"], 3);
    }

    #[test]
    fn an_unpriced_line_leaves_the_cart_totals() {
        let mut cart = json!({"totalQuantity": 9, "lines": [{"quantity": 2}], "cost": {"totalAmount": {"amount": "5.00"}}});
        let before = cart.clone();
        totals(&mut cart, 0);
        assert_eq!(cart, before);
    }
}
