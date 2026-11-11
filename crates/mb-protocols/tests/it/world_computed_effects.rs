//! World expressions, phase 2: a type's `computed` members, evaluated when a record of
//! it is answered (a read, a list, a write's answer) and never stored; an operation's
//! `effects`, writes to other records after its own write of its target, skipped when
//! that write did not happen and never cascading; and `seq()` counters kept in the
//! session's state, so a world patch keeps them, a reset clears them and a snapshot
//! carries them.
#![cfg(feature = "http-rest")]

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use mb_state::{MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness, HarnessOptions};

/// Orders holding their `items`, with lines under them, and events about them.
const SHOP: &str = r##"
openapi: 3.0.0
info: {title: Shop, version: "1"}
paths:
  /orders:
    get:
      operationId: listOrders
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [orders]
                properties:
                  orders: {type: array, items: {$ref: "#/components/schemas/OrderSummary"}}
    post:
      operationId: createOrder
      requestBody: {content: {application/json: {schema: {type: object, properties: {customer: {type: string}, items: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{orderId}:
    parameters: [{name: orderId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getOrder
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
    patch:
      operationId: updateOrder
      requestBody: {content: {application/json: {schema: {type: object, properties: {customer: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{orderId}/lines:
    parameters: [{name: orderId, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createLine
      requestBody: {content: {application/json: {schema: {type: object, properties: {sku: {type: string}, quantity: {type: integer}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}
        "422": {description: invalid, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /orders/{orderId}/lines/{lineId}:
    parameters:
      - {name: orderId, in: path, required: true, schema: {type: string}}
      - {name: lineId, in: path, required: true, schema: {type: string}}
    get:
      operationId: getLine
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}}
    delete:
      operationId: deleteLine
      responses: {"204": {description: gone}}
  /events/{eventId}:
    get:
      operationId: getEvent
      parameters: [{name: eventId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Event"}}}}}
components:
  schemas:
    Item:
      type: object
      properties: {price: {type: number}, quantity: {type: integer}}
    Order:
      type: object
      required: [id]
      properties:
        id: {type: string}
        customer: {type: string}
        status: {type: string}
        items: {type: array, items: {$ref: "#/components/schemas/Item"}}
        line_count: {type: integer}
        total: {type: number}
        label: {type: string}
    OrderSummary:
      type: object
      required: [id]
      properties:
        id: {type: string}
        line_count: {type: integer}
        label: {type: string}
    Line:
      type: object
      required: [id]
      properties:
        id: {type: string}
        orderId: {type: string}
        sku: {type: string}
        quantity: {type: integer}
    Event:
      type: object
      required: [id, kind]
      properties:
        id: {type: string}
        order_id: {type: string}
        kind: {type: string}
        number: {type: integer}
    Error:
      type: object
      properties: {code: {type: string}, message: {type: string}}
"##;

/// Orders compute their line count, their total from their items and a label; a new
/// order starts `draft`. A line moves its order from `draft` to `open`, notes an event
/// named for it (numbered by `seq`), and one more for a bulk quantity; a deleted line
/// takes its event with it; a negative quantity is refused. An order's update notes an
/// event too, which a line's effect on the order must not cascade into.
fn world() -> Value {
    json!({
        "types": {
            "lines": {"parent": {"type": "orders", "member": "orderId"}},
            "orders": {"computed": {
                "line_count": "count('lines', it.orderId == record.id)",
                "total": "sum(record.items, it.price * it.quantity)",
                "label": "record.customer + ' #' + record.id"
            }}
        },
        "operations": {
            "createOrder": {"kind": "create", "set": {"status": "'draft'"}},
            "updateOrder": {"effects": [
                {"op": "create", "type": "events", "set": {"order_id": "record.id", "kind": "'updated'"}}
            ]},
            "createLine": {
                "refuse": [{"when": "request.body.quantity < 0", "status": 422, "code": "bad_quantity"}],
                "effects": [
                    {"op": "update", "type": "orders", "id": "record.orderId",
                     "when": "it.status == 'draft'", "set": {"status": "'open'"}},
                    {"op": "create", "type": "events", "set": {
                        "id": "'ev-' + record.id", "order_id": "record.orderId",
                        "kind": "'line_added'", "number": "seq('events')"
                    }},
                    {"op": "create", "type": "events", "when": "record.quantity > 100",
                     "set": {"order_id": "record.orderId", "kind": "'bulk'"}}
                ]
            },
            "deleteLine": {"effects": [{"op": "delete", "type": "events", "id": "'ev-' + record.id"}]}
        }
    })
}

fn shop() -> DescribedHarness {
    support::described(SHOP, world(), Vec::new())
}

/// [`shop`] serving `world` over `store`, as a runtime rebuilt over the same state.
fn shop_over(store: &Arc<dyn StateStore>, world: Value) -> DescribedHarness {
    let store = Arc::clone(store);
    support::harness(SHOP, Vec::new(), &HarnessOptions::default(), |mut rt| {
        rt.state = store;
        support::with_effective(rt, world, false)
    })
}

fn session(h: &DescribedHarness) -> SessionKey {
    SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    }
}

async fn send(h: &DescribedHarness, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (status, _, answer) = h.send(method, uri, body, &[]).await;
    assert!(status.is_success(), "{method} {uri}: {status} {answer}");
    answer
}

/// A new order of `customer` with two items (2 × 2.25 and 3 × 1): its id and answer.
async fn new_order(h: &DescribedHarness, customer: &str) -> (String, Value) {
    let items = json!([{"price": 2.25, "quantity": 2}, {"price": 1, "quantity": 3}]);
    let created = send(
        h,
        "POST",
        "/orders",
        Some(json!({"customer": customer, "items": items})),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    (id, created)
}

/// A new line of `quantity` under order `order`: its id.
async fn new_line(h: &DescribedHarness, order: &str, quantity: i64) -> String {
    let line = send(
        h,
        "POST",
        &format!("/orders/{order}/lines"),
        Some(json!({"sku": "s", "quantity": quantity})),
    )
    .await;
    line["id"].as_str().unwrap().to_string()
}

/// The events stored in the default session.
async fn events(h: &DescribedHarness) -> Vec<Value> {
    h.rt.state
        .list(&session(h), "events", 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.data)
        .collect()
}

fn kinds(events: &[Value]) -> Vec<&str> {
    let mut kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
    kinds.sort_unstable();
    kinds
}

#[tokio::test]
async fn computed_members_render_on_reads_lists_and_write_answers_and_are_never_stored() {
    let h = shop();
    let (id, created) = new_order(&h, "Ada").await;
    // A count of children, a total from line items and a template, on a create's answer.
    assert_eq!(created["line_count"], json!(0), "{created}");
    assert_eq!(created["total"], json!(7.5), "{created}");
    assert_eq!(created["label"], json!(format!("Ada #{id}")), "{created}");
    new_line(&h, &id, 1).await;
    new_line(&h, &id, 2).await;
    // On a read.
    let read = send(&h, "GET", &format!("/orders/{id}"), None).await;
    assert_eq!(read["line_count"], json!(2), "{read}");
    assert_eq!(read["total"], json!(7.5), "{read}");
    // On a list.
    let listed = send(&h, "GET", "/orders", None).await;
    let item = &listed["orders"][0];
    assert_eq!(item["line_count"], json!(2), "{listed}");
    assert_eq!(item["label"], json!(format!("Ada #{id}")), "{listed}");
    // On an update's answer, from the record as the update left it.
    let updated = send(
        &h,
        "PATCH",
        &format!("/orders/{id}"),
        Some(json!({"customer": "Bo"})),
    )
    .await;
    assert_eq!(updated["label"], json!(format!("Bo #{id}")), "{updated}");
    assert_eq!(updated["line_count"], json!(2), "{updated}");
    // Never stored.
    let stored = h.stored("orders", &id).await.unwrap();
    for member in ["line_count", "total", "label"] {
        assert!(stored.get(member).is_none(), "{member}: {stored}");
    }
}

#[tokio::test]
async fn a_failed_computed_expression_leaves_the_member_and_is_on_the_request_record() {
    let mut w = world();
    w["types"]["orders"]["computed"]["total"] = json!("sum(record.items, it.price * 'x')");
    let h = support::described(SHOP, w, Vec::new());
    let (_, created) = new_order(&h, "Ada").await;
    assert!(created.get("total").is_none(), "{created}");
    let records = h.records();
    let failed = &records.last().unwrap().request["expr_failed"];
    assert_eq!(
        failed[0]["pointer"],
        json!("/types/orders/computed/total"),
        "{failed}"
    );
    // On a list (whose items show `label`, not `total`), one failing alike on each
    // item is noted once.
    let mut w = world();
    w["types"]["orders"]["computed"]["label"] = json!("sum(record.items, it.price * 'x')");
    let h = support::described(SHOP, w, Vec::new());
    new_order(&h, "Ada").await;
    new_order(&h, "Bo").await;
    let listed = send(&h, "GET", "/orders", None).await;
    assert_eq!(
        listed["orders"].as_array().map(Vec::len),
        Some(2),
        "{listed}"
    );
    let records = h.records();
    let failed = &records.last().unwrap().request["expr_failed"];
    assert_eq!(failed.as_array().map(Vec::len), Some(1), "{failed}");
    assert_eq!(
        failed[0]["pointer"],
        json!("/types/orders/computed/label"),
        "{failed}"
    );
}

#[tokio::test]
async fn a_create_effect_does_not_store_a_record_a_read_would_not_render() {
    // An event requires its `kind`, which this effect does not set.
    let mut w = world();
    w["operations"]["updateOrder"]["effects"] =
        json!([{"op": "create", "type": "events", "set": {"order_id": "record.id"}}]);
    let h = support::described(SHOP, w, Vec::new());
    let (id, _) = new_order(&h, "Ada").await;
    send(
        &h,
        "PATCH",
        &format!("/orders/{id}"),
        Some(json!({"customer": "Bo"})),
    )
    .await;
    assert!(events(&h).await.is_empty());
    let records = h.records();
    let failed = &records.last().unwrap().request["expr_failed"];
    assert_eq!(
        failed[0]["pointer"],
        json!("/operations/updateOrder/effects/0"),
        "{failed}"
    );
}

#[tokio::test]
async fn effects_create_update_and_delete_other_records_when_their_condition_holds() {
    let h = shop();
    let (order, _) = new_order(&h, "Ada").await;
    let line = new_line(&h, &order, 2).await;
    // Update: the draft order is open (`when` true).
    let read = send(&h, "GET", &format!("/orders/{order}"), None).await;
    assert_eq!(read["status"], json!("open"), "{read}");
    // Create: the line's event, under the id `set` gives it, numbered by `seq`.
    let event = h.stored("events", &format!("ev-{line}")).await.unwrap();
    assert_eq!(event["order_id"], json!(order), "{event}");
    assert_eq!(event["kind"], json!("line_added"), "{event}");
    assert_eq!(event["number"], json!(1), "{event}");
    let served = send(&h, "GET", &format!("/events/ev-{line}"), None).await;
    assert_eq!(served["kind"], json!("line_added"), "{served}");
    // The bulk event's `when` is false for a quantity of 2; true for 150, with an id
    // minted as a create mints one.
    assert_eq!(kinds(&events(&h).await), vec!["line_added"]);
    let bulk = new_line(&h, &order, 150).await;
    let all = events(&h).await;
    assert_eq!(kinds(&all), vec!["bulk", "line_added", "line_added"]);
    let minted = all.iter().find(|e| e["kind"] == "bulk").unwrap();
    assert!(
        minted["id"].as_str().is_some_and(|id| !id.is_empty()),
        "{minted}"
    );
    let second = h.stored("events", &format!("ev-{bulk}")).await.unwrap();
    assert_eq!(second["number"], json!(2), "{second}");
    // The order is not draft any more: its update's `when` is false, and it stays open.
    let read = send(&h, "GET", &format!("/orders/{order}"), None).await;
    assert_eq!(read["status"], json!("open"), "{read}");
    // Delete: the line's event goes with it.
    let (status, _, _) = h
        .send(
            "DELETE",
            &format!("/orders/{order}/lines/{line}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.stored("events", &format!("ev-{line}")).await.is_none());
    assert!(h.stored("events", &format!("ev-{bulk}")).await.is_some());
}

#[tokio::test]
async fn effects_do_not_cascade() {
    let h = shop();
    let (order, _) = new_order(&h, "Ada").await;
    new_line(&h, &order, 1).await;
    // The line's effect updated the order, but not through `updateOrder`: its effects
    // did not run.
    assert_eq!(kinds(&events(&h).await), vec!["line_added"]);
    // A call of `updateOrder` itself runs them.
    send(
        &h,
        "PATCH",
        &format!("/orders/{order}"),
        Some(json!({"customer": "Bo"})),
    )
    .await;
    assert_eq!(kinds(&events(&h).await), vec!["line_added", "updated"]);
}

#[tokio::test]
async fn effects_are_skipped_when_the_target_was_refused_or_not_written() {
    let h = shop();
    let (order, _) = new_order(&h, "Ada").await;
    // Refused: nothing is written, so no effect runs.
    let (status, _, body) = h
        .send(
            "POST",
            &format!("/orders/{order}/lines"),
            Some(json!({"sku": "s", "quantity": -1})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(events(&h).await.is_empty());
    let read = send(&h, "GET", &format!("/orders/{order}"), None).await;
    assert_eq!(read["status"], json!("draft"), "{read}");
    // Generated (a seeded call), whatever write memory keeps of it: the executor wrote
    // no target, so no effect runs.
    let (status, _, body) = h
        .seeded(
            "POST",
            &format!("/orders/{order}/lines"),
            Some(json!({"sku": "s", "quantity": 1})),
            &[],
        )
        .await;
    assert!(status.is_success(), "{body}");
    assert!(events(&h).await.is_empty());
    // An update of an order the session does not hold generates: no effect.
    let (status, _, body) = h
        .send(
            "PATCH",
            "/orders/elsewhere",
            Some(json!({"customer": "C"})),
            &[],
        )
        .await;
    assert!(status.is_success(), "{body}");
    assert!(events(&h).await.is_empty());
}

#[tokio::test]
async fn seq_counters_are_session_state_kept_by_a_world_patch_cleared_by_a_reset_and_carried_by_a_snapshot()
 {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let h = shop_over(&store, world());
    let (order, _) = new_order(&h, "Ada").await;
    let first = new_line(&h, &order, 1).await;
    let number = |e: Option<Value>| e.map(|e| e["number"].clone());
    assert_eq!(
        number(h.stored("events", &format!("ev-{first}")).await),
        Some(json!(1))
    );
    // A patched world, built into a new runtime over the same state, goes on counting.
    let mut patched = world();
    patched["types"]["orders"]["computed"]["label"] = json!("'#' + record.id");
    let h = shop_over(&store, patched);
    let second = new_line(&h, &order, 1).await;
    assert_eq!(
        number(h.stored("events", &format!("ev-{second}")).await),
        Some(json!(2))
    );
    // A snapshot of the session, seeded into another store, carries the counters.
    let exported = store.export(&session(&h)).await.unwrap();
    let other: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    other.seed(&session(&h), exported).await.unwrap();
    let restored = shop_over(&other, world());
    let third = new_line(&restored, &order, 1).await;
    assert_eq!(
        number(restored.stored("events", &format!("ev-{third}")).await),
        Some(json!(3))
    );
    // A reset of the session starts them again.
    store.reset(&session(&h)).await.unwrap();
    let (order, _) = new_order(&h, "Ada").await;
    let fresh = new_line(&h, &order, 1).await;
    assert_eq!(
        number(h.stored("events", &format!("ev-{fresh}")).await),
        Some(json!(1))
    );
}

#[tokio::test]
async fn a_list_item_cut_to_its_summary_computes_from_the_record_as_held() {
    let h = shop();
    let (id, _) = new_order(&h, "Ada").await;
    new_line(&h, &id, 1).await;
    // The summary leaves out `customer`, which `label` reads: it is read from the
    // record as held, not from the item.
    let listed = send(&h, "GET", "/orders", None).await;
    let item = &listed["orders"][0];
    assert!(item.get("customer").is_none(), "{listed}");
    assert_eq!(item["label"], json!(format!("Ada #{id}")), "{listed}");
    assert_eq!(item["line_count"], json!(1), "{listed}");
    let records = h.records();
    let last = &records.last().unwrap().request;
    assert!(last.get("expr_failed").is_none(), "{last}");
}

/// [`world`] where creating and updating an order each add a line to it.
fn world_adding_lines() -> Value {
    let mut w = world();
    let add_line = json!({"op": "create", "type": "lines",
        "set": {"orderId": "record.id", "sku": "'auto'", "quantity": "1"}});
    w["operations"]["createOrder"]["effects"] = json!([add_line.clone()]);
    w["operations"]["updateOrder"]["effects"] = json!([add_line]);
    w
}

#[tokio::test]
async fn a_write_answer_computes_its_members_after_its_effects_wrote() {
    let h = support::described(SHOP, world_adding_lines(), Vec::new());
    let (id, created) = new_order(&h, "Ada").await;
    // The create's effect added a line: its answer counts it, as a read does.
    assert_eq!(created["line_count"], json!(1), "{created}");
    let read = send(&h, "GET", &format!("/orders/{id}"), None).await;
    assert_eq!(read["line_count"], json!(1), "{read}");
    // So does an update's answer, its other members as the update left them.
    let updated = send(
        &h,
        "PATCH",
        &format!("/orders/{id}"),
        Some(json!({"customer": "Bo"})),
    )
    .await;
    assert_eq!(updated["line_count"], json!(2), "{updated}");
    assert_eq!(updated["label"], json!(format!("Bo #{id}")), "{updated}");
    assert_eq!(updated["status"], json!("draft"), "{updated}");
}

#[tokio::test]
async fn a_create_effect_places_its_record_under_the_parent_its_path_names() {
    // Lines name their order only by the path they are created under; the order's
    // line count is a `derived` one, counting by that path.
    let mut w = json!({
        "types": {
            "lines": {"parent": {"type": "orders", "path_param": "orderId"}},
            "orders": {"derived": {"line_count": {"kind": "count", "of": "lines"}}}
        },
        "operations": {"createOrder": {"effects": [
            {"op": "create", "type": "lines", "set": {"sku": "'auto'"}},
            {"op": "create", "type": "lines", "set": {"sku": "'auto'"}}
        ]}}
    });
    let h = support::described(SHOP, w.take(), Vec::new());
    let (id, created) = new_order(&h, "Ada").await;
    assert_eq!(created["line_count"], json!(2), "{created}");
    let read = send(&h, "GET", &format!("/orders/{id}"), None).await;
    assert_eq!(read["line_count"], json!(2), "{read}");
    // Each minted its own id.
    let lines = h.rt.state.list(&session(&h), "lines", 10).await.unwrap();
    assert_eq!(lines.len(), 2);
    assert_ne!(lines[0].id, lines[1].id);
    let line = &lines[0].id;
    let served = send(&h, "GET", &format!("/orders/{id}/lines/{line}"), None).await;
    assert_eq!(served["sku"], json!("auto"), "{served}");
}
