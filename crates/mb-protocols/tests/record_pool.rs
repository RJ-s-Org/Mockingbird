//! Record pools: records the model wrote ahead of any call answer a read of an id the
//! session does not hold, and a first list of a type, from state with no model call.
#![cfg(test)]
#![cfg(feature = "http-rest")]

#[path = "it/support.rs"]
#[allow(dead_code)]
mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};

const WIDGETS: &str = r##"
openapi: 3.0.3
info: {title: Widgets, version: "1"}
paths:
  /widgets:
    get:
      operationId: listWidgets
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Widget"}}}}}
  /widgets/{id}:
    get:
      operationId: getWidget
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
components:
  schemas:
    Widget:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
"##;

fn world() -> Value {
    json!({"types": {"widgets": {"identity": {"member": "id"}}},
           "operations": {"listWidgets": {"kind": "list", "type": "widgets"},
                          "getWidget": {"kind": "read", "type": "widgets", "target": {"path_param": "id"}}}})
}

fn widget(i: usize) -> Value {
    json!({"id": format!("w{i}"), "name": format!("Widget {i}")})
}

/// A harness whose model answers the pool's one call with five widgets, then nothing.
async fn pooled() -> support::DescribedHarness {
    let batch = json!({"records": (1..=5).map(widget).collect::<Vec<_>>()});
    let h = support::described(WIDGETS, world(), vec![batch]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    assert_eq!(h.rt.pool.sizes().get("widgets"), Some(&5));
    h
}

fn pool_note(h: &support::DescribedHarness) -> Option<Value> {
    let records = h.recorded.0.lock().unwrap();
    records.last().and_then(|r| r.request.get("pool").cloned())
}

#[tokio::test]
async fn a_read_of_an_id_the_session_does_not_hold_is_a_pool_record_under_that_id() {
    let h = pooled().await;
    let calls = h.provider.requests().len();
    let (status, headers, body) = h.send("GET", "/widgets/abc", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-mockingbird-source"], "state");
    assert_eq!(body["id"], "abc");
    assert!(
        body["name"].as_str().unwrap().starts_with("Widget "),
        "{body}"
    );
    assert_eq!(pool_note(&h), Some(json!(1)));
    assert_eq!(
        h.provider.requests().len(),
        calls,
        "no model call on the request path"
    );
    // Stored in the session: the next read answers the same record.
    let (_, _, again) = h.send("GET", "/widgets/abc", None, &[]).await;
    assert_eq!(again, body);
    // Another session reading that id gets the same record.
    let (_, _, other) = h
        .send(
            "GET",
            "/widgets/abc",
            None,
            &[("x-mockingbird-session", "s2")],
        )
        .await;
    assert_eq!(other, body);
}

#[tokio::test]
async fn a_first_list_of_a_type_is_the_pool() {
    let h = pooled().await;
    let calls = h.provider.requests().len();
    let (status, headers, body) = h.send("GET", "/widgets", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-mockingbird-source"], "state");
    let mut ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["w1", "w2", "w3", "w4", "w5"]);
    assert_eq!(pool_note(&h), Some(json!(5)));
    assert_eq!(h.provider.requests().len(), calls);
    // A listed record reads back as listed.
    let (_, _, one) = h.send("GET", "/widgets/w3", None, &[]).await;
    assert_eq!(one, widget(3));
}

#[tokio::test]
async fn without_a_pool_a_read_is_generated_as_before() {
    let h = support::described(WIDGETS, world(), vec![widget(9)]);
    let (status, headers, body) = h.send("GET", "/widgets/w9", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(headers["x-mockingbird-source"], "state");
    assert_eq!(pool_note(&h), None);
}

#[tokio::test]
async fn a_record_repeating_an_id_is_left_out_of_the_pool() {
    let batch = json!({"records": [widget(1), widget(2), widget(1)]});
    let h = support::described(WIDGETS, world(), vec![batch]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    assert_eq!(h.rt.pool.sizes().get("widgets"), Some(&2));
}

#[tokio::test]
async fn a_cap_too_small_for_a_call_writes_nothing() {
    let h = support::described(WIDGETS, world(), vec![json!({"records": [widget(1)]})]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), mb_protocols::MIN_POOL_TOKENS - 1).await;
    assert!(h.rt.pool.sizes().is_empty());
    assert!(h.provider.requests().is_empty());
}

#[tokio::test]
async fn every_list_of_a_pooled_type_answers_the_same_records() {
    // A second list with no path parameters: without a pool it is `unscoped_list`.
    let contract = WIDGETS.replace(
        "  /widgets/{id}:",
        "  /featured-widgets:\n    get:\n      operationId: listFeatured\n      responses:\n        \"200\": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: \"#/components/schemas/Widget\"}}}}}\n  /widgets/{id}:",
    );
    let mut w = world();
    w["operations"]["listFeatured"] = json!({"kind": "list", "type": "widgets"});
    let batch = json!({"records": (1..=5).map(widget).collect::<Vec<_>>()});
    let h = support::described(&contract, w, vec![batch]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    let calls = h.provider.requests().len();
    let (_, first_headers, first) = h.send("GET", "/featured-widgets", None, &[]).await;
    let (_, second_headers, second) = h.send("GET", "/widgets", None, &[]).await;
    assert_eq!(first_headers["x-mockingbird-source"], "state", "{first}");
    assert_eq!(second_headers["x-mockingbird-source"], "state", "{second}");
    assert_eq!(first.as_array().unwrap().len(), 5);
    assert_eq!(first, second);
    assert_eq!(h.provider.requests().len(), calls);
}

#[tokio::test]
async fn a_pool_holds_no_more_records_than_a_page_of_its_lists_admits() {
    let contract = WIDGETS.replace(
        "schema: {type: array, items: {$ref: \"#/components/schemas/Widget\"}}",
        "schema: {type: array, maxItems: 3, items: {$ref: \"#/components/schemas/Widget\"}}",
    );
    let batch = json!({"records": (1..=3).map(widget).collect::<Vec<_>>()});
    let h = support::described(&contract, world(), vec![batch]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    assert_eq!(h.rt.pool.sizes().get("widgets"), Some(&3));
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 1, "one call asks for the three");
    let (_, headers, body) = h.send("GET", "/widgets", None, &[]).await;
    assert_eq!(headers["x-mockingbird-source"], "state", "{body}");
    assert_eq!(body.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn a_pool_record_that_cannot_take_the_asked_id_is_not_served() {
    // The type's ids follow a pattern: a pool record under an id breaking it would not
    // read as the type, so the read is generated as before.
    let contract = WIDGETS.replace(
        "        id: {type: string}\n        name",
        "        id: {type: string, pattern: \"^w[0-9]+$\"}\n        name",
    );
    let batch = json!({"records": (1..=5).map(widget).collect::<Vec<_>>()});
    let h = support::described(&contract, world(), vec![batch, widget(9)]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    assert_eq!(h.rt.pool.sizes().get("widgets"), Some(&5));
    let (_, headers, body) = h.send("GET", "/widgets/abc", None, &[]).await;
    assert_ne!(headers["x-mockingbird-source"], "state", "{body}");
    assert_eq!(pool_note(&h), None);
    let (_, headers, body) = h.send("GET", "/widgets/w77", None, &[]).await;
    assert_eq!(headers["x-mockingbird-source"], "state", "{body}");
    assert_eq!(body["id"], "w77");
}

#[tokio::test]
async fn a_word_is_not_a_pool_record_of_a_type_with_numeric_ids() {
    let contract = WIDGETS.replace("        id: {type: string}", "        id: {type: integer}");
    let batch = json!({"records": (1..=5).map(|i| json!({"id": i, "name": format!("Widget {i}")})).collect::<Vec<_>>()});
    let h = support::described(
        &contract,
        world(),
        vec![batch, json!({"id": 9, "name": "Nine"})],
    );
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    assert_eq!(h.rt.pool.sizes().get("widgets"), Some(&5));
    let (_, headers, body) = h.send("GET", "/widgets/abc", None, &[]).await;
    assert_ne!(headers["x-mockingbird-source"], "state", "{body}");
    let (_, headers, body) = h.send("GET", "/widgets/42", None, &[]).await;
    assert_eq!(headers["x-mockingbird-source"], "state", "{body}");
    assert_eq!(body["id"], 42);
}

#[tokio::test]
async fn a_reset_session_takes_the_pool_again_on_its_first_list() {
    let h = pooled().await;
    let (_, _, first) = h.send("GET", "/widgets", None, &[]).await;
    assert_eq!(first.as_array().unwrap().len(), 5);
    let session = mb_state::SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    };
    h.rt.state.reset(&session).await.unwrap();
    let (_, headers, again) = h.send("GET", "/widgets", None, &[]).await;
    assert_eq!(headers["x-mockingbird-source"], "state", "{again}");
    assert_eq!(again.as_array().unwrap().len(), 5, "{again}");
    assert_eq!(pool_note(&h), Some(json!(5)));
}
