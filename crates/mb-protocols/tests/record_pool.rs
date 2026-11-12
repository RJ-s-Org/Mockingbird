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

/// Widgets with a name filter on the list, a create and a delete.
fn writable() -> (String, Value) {
    let contract = WIDGETS
        .replace(
            "      operationId: listWidgets\n",
            "      operationId: listWidgets\n      parameters: [{name: name, in: query, schema: {type: string}}]\n",
        )
        .replace(
            "  /widgets/{id}:\n    get:",
            "  /widgets/{id}:\n    delete:\n      operationId: deleteWidget\n      parameters: [{name: id, in: path, required: true, schema: {type: string}}]\n      responses:\n        \"204\": {description: gone}\n    get:",
        )
        .replace(
            "  /widgets:\n    get:",
            "  /widgets:\n    post:\n      operationId: createWidget\n      requestBody: {content: {application/json: {schema: {$ref: \"#/components/schemas/Widget\"}}}}\n      responses:\n        \"201\": {description: ok, content: {application/json: {schema: {$ref: \"#/components/schemas/Widget\"}}}}\n    get:",
        );
    let mut w = world();
    w["operations"]["listWidgets"]["filters"] =
        json!([{"param": "name", "member": "name", "kind": "eq"}]);
    w["operations"]["createWidget"] =
        json!({"kind": "create", "type": "widgets", "target": {"member": "id"}});
    w["operations"]["deleteWidget"] =
        json!({"kind": "delete", "type": "widgets", "target": {"path_param": "id"}});
    (contract, w)
}

#[tokio::test]
async fn a_declared_filter_runs_over_the_pool_and_may_match_nothing() {
    let (contract, w) = writable();
    let batch = json!({"records": (1..=5).map(widget).collect::<Vec<_>>()});
    let h = support::described(&contract, w, vec![batch]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    let calls = h.provider.requests().len();
    let (_, headers, one) = h.send("GET", "/widgets?name=Widget%202", None, &[]).await;
    assert_eq!(headers["x-mockingbird-source"], "state", "{one}");
    assert_eq!(one, json!([widget(2)]));
    let (status, headers, none) = h.send("GET", "/widgets?name=Nobody", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-mockingbird-source"], "state", "{none}");
    assert_eq!(none, json!([]));
    assert_eq!(h.provider.requests().len(), calls);
}

#[tokio::test]
async fn a_param_the_world_does_not_describe_falls_back_as_before() {
    let batch = json!({"records": (1..=5).map(widget).collect::<Vec<_>>()});
    let h = support::described(WIDGETS, world(), vec![batch, json!([widget(8)])]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    let (_, headers, body) = h.send("GET", "/widgets?color=red", None, &[]).await;
    assert_ne!(headers["x-mockingbird-source"], "state", "{body}");
    assert_eq!(pool_note(&h), None);
}

#[tokio::test]
async fn the_pool_never_hides_a_created_or_deleted_record() {
    let (contract, w) = writable();
    let batch = json!({"records": (1..=5).map(widget).collect::<Vec<_>>()});
    let h = support::described(&contract, w, vec![batch, widget(9)]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    let (_, _, first) = h.send("GET", "/widgets", None, &[]).await;
    assert_eq!(first.as_array().unwrap().len(), 5);
    let (status, _, made) = h.send("POST", "/widgets", Some(widget(9)), &[]).await;
    assert!(status.is_success(), "{made}");
    let (status, _, _) = h.send("DELETE", "/widgets/w2", None, &[]).await;
    assert!(status.is_success());
    let (_, headers, listed) = h.send("GET", "/widgets", None, &[]).await;
    assert_eq!(headers["x-mockingbird-source"], "state", "{listed}");
    let mut ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["w1", "w3", "w4", "w5", "w9"]);
}

#[tokio::test]
async fn a_session_that_wrote_the_type_first_lists_its_own_records() {
    let (contract, w) = writable();
    let batch = json!({"records": (1..=5).map(widget).collect::<Vec<_>>()});
    let h = support::described(&contract, w, vec![batch, widget(9)]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    h.send("POST", "/widgets", Some(widget(9)), &[]).await;
    let (_, _, listed) = h.send("GET", "/widgets", None, &[]).await;
    assert_eq!(listed, json!([widget(9)]));
    assert_eq!(pool_note(&h), None);
}

#[tokio::test]
async fn a_list_whose_items_are_another_type_never_shapes_the_pool() {
    // `/widget-kinds` lists kinds (ids from an enum) but is typed as widgets. The pool's
    // widgets are written from the read by id's schema, so no record takes a kind's id;
    // none reads as a kind either, so the type is left unpooled instead of corrupted.
    let contract = WIDGETS.replace(
        "  /widgets/{id}:",
        "  /widget-kinds:\n    get:\n      operationId: listKinds\n      responses:\n        \"200\": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: \"#/components/schemas/Kind\"}}}}}\n  /widgets/{id}:",
    ) + "    Kind:\n      type: object\n      required: [id, name]\n      properties:\n        id: {type: string, enum: [k1, k2, k3]}\n        name: {type: string}\n";
    let mut w = world();
    w["operations"]["listKinds"] = json!({"kind": "list", "type": "widgets"});
    let batch = json!({"records": (1..=5).map(widget).collect::<Vec<_>>()});
    let h = support::described(&contract, w, vec![batch, widget(9)]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    assert_eq!(h.rt.pool.sizes().get("widgets").copied().unwrap_or(0), 0);
    // The model was asked for widgets alone: the kinds' id enum never reached it.
    let asked = format!("{:?}", h.provider.requests());
    assert!(
        !asked.contains("k2"),
        "the pool call carried the kinds' enum"
    );
}

#[tokio::test]
async fn a_type_whose_lists_show_a_summary_of_its_record_is_still_pooled() {
    // The read by id answers the whole widget; the list shows a summary of it.
    let contract = WIDGETS
        .replace(
            "schema: {type: array, items: {$ref: \"#/components/schemas/Widget\"}}",
            "schema: {type: array, items: {$ref: \"#/components/schemas/WidgetSummary\"}}",
        )
        .replace(
            "      required: [id, name]\n      properties:\n        id: {type: string}\n        name: {type: string}\n",
            "      required: [id, name, description]\n      properties:\n        id: {type: string}\n        name: {type: string}\n        description: {type: string}\n    WidgetSummary:\n      type: object\n      additionalProperties: false\n      required: [id, name]\n      properties:\n        id: {type: string}\n        name: {type: string}\n",
        );
    let full = |i: usize| json!({"id": format!("w{i}"), "name": format!("Widget {i}"), "description": format!("The {i}th")});
    let batch = json!({"records": (1..=5).map(full).collect::<Vec<_>>()});
    let h = support::described(&contract, world(), vec![batch]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    assert_eq!(h.rt.pool.sizes().get("widgets"), Some(&5));
    let (_, headers, listed) = h.send("GET", "/widgets", None, &[]).await;
    assert_eq!(headers["x-mockingbird-source"], "state", "{listed}");
    assert_eq!(listed.as_array().unwrap().len(), 5, "{listed}");
    assert!(
        listed[0].get("description").is_none(),
        "a summary: {listed}"
    );
}

#[tokio::test]
async fn a_create_takes_what_it_did_not_send_from_a_pool_record_with_no_model_call() {
    let (contract, mut w) = writable();
    w["operations"]["createWidget"] = json!({"kind": "create", "type": "widgets"});
    let contract = contract.replace(
        "      required: [id, name]\n      properties:\n        id: {type: string}\n        name: {type: string}\n",
        "      required: [id, name, color, url]\n      properties:\n        id: {type: string}\n        name: {type: string}\n        color: {type: string}\n        url: {type: string}\n",
   )
    .replace(
        "requestBody: {content: {application/json: {schema: {$ref: \"#/components/schemas/Widget\"}}}}",
        "requestBody: {content: {application/json: {schema: {type: object, required: [name], properties: {name: {type: string}}}}}}",
    );
    let colored = |i: usize| {
        json!({"id": format!("wdg{i}"), "name": format!("Widget {i}"), "color": format!("color-{i}"),
               "url": format!("https://example.test/widgets/wdg{i}")})
    };
    let batch = json!({"records": (1..=5).map(colored).collect::<Vec<_>>()});
    let h = support::described(&contract, w, vec![batch]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    assert_eq!(h.rt.pool.sizes().get("widgets"), Some(&5));
    let calls = h.provider.requests().len();
    let (status, _, made) = h
        .send("POST", "/widgets", Some(json!({"name": "Brand new"})), &[])
        .await;
    assert!(status.is_success(), "{made}");
    assert_eq!(h.provider.requests().len(), calls, "no model call: {made}");
    assert_eq!(made["name"], "Brand new", "the request wins: {made}");
    let id = made["id"].as_str().unwrap();
    assert!(
        !id.starts_with('w') || !(1..=5).any(|i| id == format!("w{i}")),
        "a new id: {made}"
    );
    let color = made["color"].as_str().unwrap();
    assert!(color.starts_with("color-"), "from a pool record: {made}");
    assert_eq!(
        made["url"],
        format!("https://example.test/widgets/{id}"),
        "{made}"
    );
    assert_eq!(pool_note(&h), Some(json!(1)));
    // Stored as answered.
    let (_, _, read) = h.send("GET", &format!("/widgets/{id}"), None, &[]).await;
    assert_eq!(read, made);
}

#[tokio::test]
async fn a_create_leaves_its_state_nulls_and_partly_sent_members_out_of_the_pool() {
    let (contract, mut w) = writable();
    let contract = contract
        .replace(
            "      required: [id, name]\n      properties:\n        id: {type: string}\n        name: {type: string}\n",
            "      required: [id, name, color, status, dims]\n      properties:\n        id: {type: string}\n        name: {type: string}\n        color: {type: string}\n        status: {type: string, enum: [active, archived]}\n        meta: {type: object, nullable: true, properties: {note: {type: string}}}\n        dims: {type: object, required: [w, h], properties: {w: {type: integer}, h: {type: integer}}}\n",
        )
        .replace(
            "requestBody: {content: {application/json: {schema: {$ref: \"#/components/schemas/Widget\"}}}}",
            "requestBody: {content: {application/json: {schema: {type: object, required: [name], properties: {name: {type: string}, dims: {type: object, properties: {w: {type: integer}}}}}}}}",
        );
    w["operations"]["createWidget"] = json!({"kind": "create", "type": "widgets"});
    w["types"]["widgets"]["lifecycle"] =
        json!({"member": "status", "states": ["active", "archived"]});
    let pooled = |i: usize| {
        json!({"id": format!("wdg{i}"), "name": format!("Widget {i}"), "color": format!("color-{i}"),
               "status": "archived", "meta": {"note": "old"}, "dims": {"w": 9, "h": 9}})
    };
    let batch = json!({"records": (1..=5).map(pooled).collect::<Vec<_>>()});
    // The model's fill: the rest of the dims the request sent part of.
    let fill = json!({"dims": {"h": 2}});
    let h = support::described(&contract, w, vec![batch, fill]);
    mb_protocols::fill_record_pool(Arc::downgrade(&h.rt), 60_000).await;
    assert_eq!(h.rt.pool.sizes().get("widgets"), Some(&5));
    let calls = h.provider.requests().len();
    let (status, _, made) = h
        .send(
            "POST",
            "/widgets",
            Some(json!({"name": "New", "dims": {"w": 1}})),
            &[],
        )
        .await;
    assert!(status.is_success(), "{made}");
    assert!(
        made["color"].as_str().unwrap().starts_with("color-"),
        "from the pool: {made}"
    );
    assert!(
        made.get("meta").is_none_or(Value::is_null),
        "fill's null, not the pool's: {made}"
    );
    assert_eq!(made["dims"]["w"], 1, "the request's: {made}");
    // One fill call, for the dims the request sent part of and the state nothing says
    // a record starts in (the lifecycle names no `initial`, no rule moves it, no
    // example shows it); never for what the pool record gave, nor the pool's state.
    let requests = h.provider.requests();
    assert_eq!(requests.len(), calls + 1, "{made}");
    let asked = format!("{:?}", requests.last().unwrap().response_format);
    assert!(
        asked.contains("dims") && asked.contains("status"),
        "{asked}"
    );
    assert!(!asked.contains("color"), "{asked}");
    // The model wrote no state: its first value, as a last resort.
    assert_eq!(made["status"], "active", "{made}");
}
