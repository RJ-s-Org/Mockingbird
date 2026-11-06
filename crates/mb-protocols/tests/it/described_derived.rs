//! Derived counts and sums.
//!
//! A member the description derives from a type's children (`count`, or `sum` of a
//! child member, optionally where children hold given values) is computed from the
//! session's stored children when a record of the type is answered: from state, in a
//! list from state, or generated about a held record. It is set only when the record
//! was created in the session or a child under it is held, and never stored.
#![cfg(test)]
#![cfg(feature = "http-rest")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use mb_state::{
    MemoryStore, ResponseCache, SessionInfo, SessionKey, StateChanged, StateError, StateStore,
    StoredResource,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::support::{self, DescribedHarness, HarnessOptions};

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Orders, version: "1"}
paths:
  /orders:
    get:
      operationId: listOrders
      parameters:
        - {name: page, in: query, schema: {type: integer, minimum: 1}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [orders]
                properties:
                  orders: {type: array, items: {$ref: "#/components/schemas/Order"}}
    post:
      operationId: createOrder
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{orderId}:
    get:
      operationId: getOrder
      parameters: [{name: orderId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{orderId}/lines:
    parameters: [{name: orderId, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createLine
      requestBody: {content: {application/json: {schema: {type: object, properties: {orderId: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}}
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
components:
  schemas:
    Order:
      type: object
      required: [id]
      properties:
        id: {type: string}
        line_count: {type: integer}
        balance: {type: number}
        total: {type: string, pattern: '^-?[0-9]+(\.[0-9]+)?$'}
    Line:
      type: object
      required: [id]
      properties:
        id: {type: string}
        orderId: {type: string}
        amount: {type: number}
        price: {type: string}
        status: {type: string, enum: [posted, pending]}
"##;

/// Lines are an order's children by the `orderId` path parameter; the order counts
/// them, sums the posted ones' `amount` and all their `price`s.
fn world() -> Value {
    json!({"types": {
        "lines": {"parent": {"type": "orders", "path_param": "orderId"}},
        "orders": {"derived": {
            "line_count": {"kind": "count", "of": "lines"},
            "balance": {"kind": "sum", "of": "lines", "member": "amount", "where": {"status": "posted"}},
            "total": {"kind": "sum", "of": "lines", "member": "price"}
        }}
    }})
}

/// [`world`], with lines naming their order by their `orderId` member instead.
fn world_by_member() -> Value {
    let mut w = world();
    w["types"]["lines"]["parent"] = json!({"type": "orders", "member": "orderId"});
    w
}

fn order(id: &str) -> Value {
    json!({"id": id, "line_count": 7, "balance": 99.5, "total": "42.00"})
}

fn line(id: &str, amount: f64, price: &str, status: &str) -> Value {
    json!({"id": id, "orderId": "o1", "amount": amount, "price": price, "status": status})
}

async fn send(h: &DescribedHarness, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (status, _, answer) = h.send(method, uri, body, &[]).await;
    assert!(status.is_success(), "{method} {uri}: {status} {answer}");
    answer
}

async fn get(h: &DescribedHarness, uri: &str) -> (String, Value) {
    let (status, headers, body) = h.send("GET", uri, None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    (source(&headers), body)
}

fn source(headers: &HeaderMap) -> String {
    headers
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn a_count_follows_children_created_and_deleted() {
    let h = support::described(
        CONTRACT,
        world(),
        vec![
            order("o1"),
            line("l1", 1.0, "1.00", "posted"),
            line("l2", 2.0, "2.00", "posted"),
        ],
    );
    // A record created in the session with no child yet counts none.
    let created = send(&h, "POST", "/orders", Some(json!({}))).await;
    assert_eq!(created["line_count"], json!(0), "{created}");
    send(&h, "POST", "/orders/o1/lines", Some(json!({}))).await;
    send(&h, "POST", "/orders/o1/lines", Some(json!({}))).await;
    let (source, body) = get(&h, "/orders/o1").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(body["line_count"], json!(2), "{body}");
    send(&h, "DELETE", "/orders/o1/lines/l1", None).await;
    let (_, body) = get(&h, "/orders/o1").await;
    assert_eq!(body["line_count"], json!(1), "{body}");
    // Computed on serve, never stored.
    let stored = h.stored("orders", "o1").await.unwrap();
    assert_eq!(stored["line_count"], json!(7), "{stored}");
}

#[tokio::test]
async fn a_sum_filters_by_state() {
    let h = support::described(
        CONTRACT,
        world_by_member(),
        vec![
            order("o1"),
            line("l1", 10.5, "1.00", "posted"),
            line("l2", 4.0, "1.00", "pending"),
            line("l3", 2.25, "1.00", "posted"),
        ],
    );
    send(&h, "POST", "/orders", Some(json!({}))).await;
    for _ in 0..3 {
        send(
            &h,
            "POST",
            "/orders/o1/lines",
            Some(json!({"orderId": "o1"})),
        )
        .await;
    }
    let (_, body) = get(&h, "/orders/o1").await;
    assert_eq!(body["balance"], json!(12.75), "{body}");
    assert_eq!(body["line_count"], json!(3), "{body}");
}

#[tokio::test]
async fn a_sum_into_a_string_member_keeps_the_children_scale() {
    let h = support::described(
        CONTRACT,
        world(),
        vec![
            order("o1"),
            line("l1", 1.0, "12.50", "posted"),
            line("l2", 1.0, "1.25", "posted"),
        ],
    );
    send(&h, "POST", "/orders", Some(json!({}))).await;
    send(&h, "POST", "/orders/o1/lines", Some(json!({}))).await;
    send(&h, "POST", "/orders/o1/lines", Some(json!({}))).await;
    let (_, body) = get(&h, "/orders/o1").await;
    assert_eq!(body["total"], json!("13.75"), "{body}");
}

#[tokio::test]
async fn a_generated_parent_with_no_held_children_keeps_the_models_value() {
    let h = support::described(
        CONTRACT,
        world(),
        vec![order("o9"), json!({"orders": [order("o5")]})],
    );
    // Not held: generated, and nothing under it is held.
    let (_, body) = get(&h, "/orders/o9").await;
    assert_eq!(body["line_count"], json!(7), "{body}");
    // Held by a list's seeding, not created in the session, with no child held.
    get(&h, "/orders").await;
    let (source, body) = get(&h, "/orders/o5").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(body["line_count"], json!(7), "{body}");
    assert_eq!(body["total"], json!("42.00"), "{body}");
}

/// A state store counting the listings of `lines`.
struct Counting {
    inner: Arc<dyn StateStore>,
    lines: AtomicUsize,
}

#[async_trait::async_trait]
impl StateStore for Counting {
    async fn get(
        &self,
        s: &SessionKey,
        t: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        self.inner.get(s, t, id).await
    }
    async fn list(
        &self,
        s: &SessionKey,
        t: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        if t == "lines" {
            self.lines.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.list(s, t, limit).await
    }
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        self.inner.upsert(s, r).await
    }
    async fn insert_seeded(&self, s: &SessionKey, r: StoredResource) -> Result<bool, StateError> {
        self.inner.insert_seeded(s, r).await
    }
    async fn insert_seeded_all(
        &self,
        s: &SessionKey,
        r: Vec<StoredResource>,
    ) -> Result<Vec<bool>, StateError> {
        self.inner.insert_seeded_all(s, r).await
    }
    async fn delete(&self, s: &SessionKey, t: &str, id: &str) -> Result<bool, StateError> {
        self.inner.delete(s, t, id).await
    }
    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.reset(s).await
    }
    async fn reset_mock(&self, workspace: &str, mock: &str) -> Result<(), StateError> {
        self.inner.reset_mock(workspace, mock).await
    }
    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.touch(s).await
    }
    async fn touch_live(&self, s: &SessionKey) -> Result<bool, StateError> {
        self.inner.touch_live(s).await
    }
    async fn sessions(&self, workspace: &str, mock: &str) -> Result<Vec<SessionInfo>, StateError> {
        self.inner.sessions(workspace, mock).await
    }
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        self.inner.export(s).await
    }
    async fn seed(&self, s: &SessionKey, resources: Vec<StoredResource>) -> Result<(), StateError> {
        self.inner.seed(s, resources).await
    }
    async fn ping(&self) -> Result<(), StateError> {
        self.inner.ping().await
    }
    fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.inner.subscribe()
    }
    fn response_cache(&self) -> Option<&ResponseCache> {
        self.inner.response_cache()
    }
}

#[tokio::test]
async fn a_page_of_parents_lists_the_children_once() {
    let ten: Vec<Value> = (1..=10).map(|i| order(&format!("o{i}"))).collect();
    let counting = Arc::new(Counting {
        inner: Arc::new(MemoryStore::new(Duration::from_secs(3600))),
        lines: AtomicUsize::new(0),
    });
    let store = Arc::clone(&counting);
    let mut w = world();
    w["operations"] = json!({"listOrders": {"paging": {"style": "page", "request": "page"}}});
    let h = support::harness(
        CONTRACT,
        vec![json!({"orders": ten}), line("l1", 1.0, "1.00", "posted")],
        &HarnessOptions::default(),
        |mut rt| {
            rt.state = store as Arc<dyn StateStore>;
            support::with_effective(rt, w, false)
        },
    );
    let (first, _) = get(&h, "/orders").await;
    assert_ne!(first, "state");
    send(&h, "POST", "/orders/o3/lines", Some(json!({}))).await;
    let before = counting.lines.load(Ordering::SeqCst);
    let (second, body) = get(&h, "/orders").await;
    assert_eq!(second, "state", "{body}");
    assert_eq!(counting.lines.load(Ordering::SeqCst) - before, 1);
    let orders = body["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 10, "{body}");
    // A seeded order with a held child counts it; one with none keeps the model's.
    assert_eq!(orders[2]["line_count"], json!(1), "{body}");
    assert_eq!(orders[0]["line_count"], json!(7), "{body}");
}

#[tokio::test]
async fn a_children_listing_cut_at_max_scan_sets_nothing() {
    let h = support::described(CONTRACT, world_by_member(), vec![order("o1")]);
    send(&h, "POST", "/orders", Some(json!({}))).await;
    let session = SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    };
    // `MAX_SCAN` (500) children: the listing is cut, so no count is exact.
    for i in 0..500 {
        let line = StoredResource {
            resource_type: "lines".into(),
            id: format!("l{i}"),
            data: line(&format!("l{i}"), 1.0, "1.00", "posted"),
        };
        h.rt.state.upsert(&session, line).await.unwrap();
    }
    let (_, body) = get(&h, "/orders/o1").await;
    assert_eq!(body["line_count"], json!(7), "{body}");
    assert_eq!(body["total"], json!("42.00"), "{body}");
}
