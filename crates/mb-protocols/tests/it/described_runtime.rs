//! The effective world compiled into the runtime: a tier-0 world answers as no
//! world, a described identity changes what is stored, and every recorded answer of
//! an operation carries what the description leaves open about it (`world_gap`).
#![cfg(test)]
#![cfg(feature = "http-rest")]

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use mb_protocols::world_gaps::GapCode;
use mb_protocols::{ChaosRuntime, ChaosSpec, RateLimiter};
use serde_json::{Value, json};

use crate::support::{self, HarnessOptions};

const NO_ID_CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Widgets, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
        "400": {description: bad, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
        "429": {description: slow, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /widgets/{widgetRef}:
    get:
      operationId: getWidget
      parameters: [{name: widgetRef, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}}
    delete:
      operationId: deleteWidget
      parameters: [{name: widgetRef, in: path, required: true, schema: {type: string}}]
      responses:
        "204": {description: gone}
        default: {description: error, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
components:
  schemas:
    Widget:
      type: object
      required: [label]
      properties: {label: {type: string}, color: {type: string}}
    Problem:
      type: object
      properties: {message: {type: string}}
"##;

/// Things with an `id`: every kind of operation on one type, its identity and targets
/// found by tier 0.
const THINGS: &str = r##"
openapi: 3.0.0
info: {title: Things, version: "1"}
paths:
  /things:
    get:
      operationId: listThings
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Thing"}}}}}}
    post:
      operationId: createThing
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/ThingInput"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
  /things/{thingId}:
    parameters: [{name: thingId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getThing
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
    patch:
      operationId: updateThing
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/ThingInput"}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}
    delete:
      operationId: deleteThing
      responses: {"204": {description: gone}}
components:
  schemas:
    Thing:
      type: object
      required: [id]
      properties: {id: {type: string}, name: {type: string}}
    ThingInput:
      type: object
      properties: {name: {type: string}, other: {type: string}}
"##;

fn thing(id: &str, name: &str) -> Value {
    json!({"id": id, "name": name})
}

fn gaps_of(record: &mb_protocols::RequestRecord) -> Option<&Value> {
    record.request.get("world_gap")
}

fn with_gaps(gaps: &[(&str, &[GapCode])]) -> HashMap<String, Vec<GapCode>> {
    gaps.iter()
        .map(|(op, codes)| ((*op).to_owned(), codes.to_vec()))
        .collect()
}

/// Every call but a list, which the state-first executor answers from the session's
/// records when the world describes its type's identity (`tests/exec.rs`).
#[tokio::test]
async fn tier0_world_answers_as_no_world() {
    let script = || {
        vec![
            thing("t1", "a"),
            thing("t1", "c"),
            thing("t1", "d"),
            thing("t1", "e"),
        ]
    };
    let plain = support::harness(THINGS, script(), &HarnessOptions::default(), |rt| rt);
    let tier0 = support::harness(THINGS, script(), &HarnessOptions::default(), |rt| {
        let base = mb_mcm::world::tier0(rt.bundle());
        rt.with_world(&base, &base, 0)
    });
    assert_eq!(tier0.rt.world_revision(), 0);
    assert!(tier0.rt.world().is_some());
    assert!(plain.rt.world().is_none());
    assert_eq!(**tier0.rt.bundle(), **plain.rt.bundle());
    let calls: [(&str, &str, Option<Value>); 6] = [
        ("POST", "/things", Some(json!({"name": "a"}))),
        ("GET", "/things/t1", None),
        ("PATCH", "/things/t1", Some(json!({"name": "c"}))),
        ("GET", "/things/t1", None),
        ("DELETE", "/things/t1", None),
        ("GET", "/things/t1", None),
    ];
    for (method, uri, body) in calls {
        // The create is seeded ([`support::SEED`]): it stores the scripted `t1`.
        let seed: &[(&str, &str)] = match method {
            "POST" => &[support::SEED],
            _ => &[],
        };
        let (a_status, mut a_headers, a_body) = plain.send(method, uri, body.clone(), seed).await;
        let (b_status, mut b_headers, b_body) = tier0.send(method, uri, body, seed).await;
        a_headers.remove("date");
        b_headers.remove("date");
        assert_eq!(a_status, b_status, "{method} {uri}");
        assert_eq!(a_headers, b_headers, "{method} {uri}");
        assert_eq!(a_body, b_body, "{method} {uri}");
        for id in ["t1", "t2"] {
            assert_eq!(
                plain.stored("things", id).await,
                tier0.stored("things", id).await,
                "{method} {uri}: stored {id}"
            );
        }
    }
    assert_eq!(
        plain.provider.requests().len(),
        tier0.provider.requests().len()
    );
}

#[tokio::test]
async fn world_gap_marks_an_operation_whose_type_has_no_identity() {
    let h = support::described(
        NO_ID_CONTRACT,
        json!({}),
        vec![json!({"label": "a", "color": "red"})],
    );
    let (status, _, _) = h
        .send("POST", "/widgets", Some(json!({"label": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(gaps_of(&h.records()[0]), Some(&json!(["no_identity"])));
    // Only the writing operations of the type are marked: the read carries no
    // `no_identity`.
    h.send("GET", "/widgets/a", None, &[]).await;
    let read = &h.records()[1];
    assert_eq!(read.operation_id.as_deref(), Some("getWidget"));
    assert!(
        !gaps_of(read)
            .and_then(Value::as_array)
            .is_some_and(|g| g.contains(&json!("no_identity"))),
        "{:?}",
        read.request
    );
}

#[tokio::test]
async fn a_described_identity_clears_the_gap() {
    let h = support::described(
        NO_ID_CONTRACT,
        json!({
            "types": {"widgets": {"identity": {"member": "label"}}},
            "operations": {"getWidget": {"kind": "read", "target": {"path_param": "widgetRef"}}}
        }),
        vec![json!({"label": "a", "color": "red"})],
    );
    assert_eq!(h.rt.world_revision(), 1);
    let (status, _, _) = h
        .seeded("POST", "/widgets", Some(json!({"label": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        gaps_of(&h.records()[0]).is_none(),
        "{:?}",
        h.records()[0].request
    );
    // The described identity also changes what is stored, not only the log mark:
    // `createWidget` had no identity; the description gave it one by `label`.
    assert_eq!(
        h.stored("widgets", "a").await.map(|r| r["label"].clone()),
        Some(json!("a"))
    );
    let (status, headers, body) = h.send("GET", "/widgets/a", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-mockingbird-source"], "state");
    assert_eq!(body["label"], "a");
    assert_eq!(h.provider.requests().len(), 1);
}

#[test]
#[should_panic(expected = "refused entries")]
fn a_refused_entry_in_a_test_world_fails_the_harness() {
    support::described(
        NO_ID_CONTRACT,
        json!({"types": {"widgets": {"identity": {"member": "nope"}}}}),
        Vec::new(),
    );
}

#[tokio::test]
async fn a_refused_entry_is_allowed_where_asked_and_falls_back_to_tier0() {
    let h = support::described_allowing_refusals(
        NO_ID_CONTRACT,
        json!({"types": {"widgets": {"identity": {"member": "nope"}}}}),
        vec![json!({"label": "a"})],
    );
    h.send("POST", "/widgets", Some(json!({"label": "a"})), &[])
        .await;
    assert_eq!(gaps_of(&h.records()[0]), Some(&json!(["no_identity"])));
}

// The HTTP tests below also pass through `CallLog::answered`, which seeds the same
// static gaps again, so they do not prove the `Prepared::Answered` arm of
// `generate_narrowed` seeds; `a_graphql_field_prepare_answers_in_a_batch_is_marked`
// covers that arm with a single-field GraphQL answer from the cache.
#[tokio::test]
async fn an_answer_prepare_refuses_is_marked() {
    let h = support::described_with_gaps_and(
        THINGS,
        json!({}),
        Vec::new(),
        with_gaps(&[("getThing", &[GapCode::NoRecord])]),
        &HarnessOptions {
            unknown_ids_not_found: true,
            ..HarnessOptions::default()
        },
    );
    let (status, _, _) = h.send("GET", "/things/nope", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(h.provider.requests().is_empty());
    assert_eq!(gaps_of(&h.records()[0]), Some(&json!(["no_record"])));
}

#[tokio::test]
async fn an_answer_from_state_is_marked() {
    let h = support::described_with_gaps(
        THINGS,
        json!({}),
        vec![thing("t1", "a")],
        with_gaps(&[("getThing", &[GapCode::NoTarget])]),
    );
    h.seeded("POST", "/things", Some(json!({"name": "a"})), &[])
        .await;
    let (status, headers, _) = h.send("GET", "/things/t1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-mockingbird-source"], "state");
    assert_eq!(gaps_of(&h.records()[1]), Some(&json!(["no_target"])));
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn an_http_record_keeps_world_gap_next_to_effects() {
    let h = support::described_with_gaps(
        THINGS,
        json!({}),
        vec![
            thing("t1", "a"),
            thing("t2", "b"),
            json!({
                "response": thing("t1", "c"),
                "effects": [{"type": "things", "id": "t2", "set": {"name": "linked"}}]
            }),
        ],
        with_gaps(&[("updateThing", &[GapCode::NoRecord])]),
    );
    h.seeded("POST", "/things", Some(json!({"name": "a"})), &[])
        .await;
    h.seeded("POST", "/things", Some(json!({"name": "b"})), &[])
        .await;
    let (status, _, _) = h
        .send(
            "PATCH",
            "/things/t1",
            Some(json!({"name": "c", "other": "t2"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let request = &h.records()[2].request;
    assert!(request["effects"].is_array(), "{request}");
    assert_eq!(request["world_gap"], json!(["no_record"]), "{request}");
}

#[tokio::test]
async fn answers_that_never_reach_prepare_carry_the_static_gaps() {
    let h = support::harness(
        NO_ID_CONTRACT,
        Vec::new(),
        &HarnessOptions::default(),
        |mut rt| {
            rt.limits.rate = Some(Arc::new(RateLimiter::new(0.01, 1)));
            rt.chaos = Some(Arc::new(ChaosRuntime::new(
                ChaosSpec {
                    error_rate: 1.0,
                    ..ChaosSpec::default()
                },
                Some(7),
            )));
            support::with_effective(rt, json!({}), false)
        },
    );
    // A chaos-injected error in the shape of the `default` response.
    let (status, _, _) = h.send("DELETE", "/widgets/a", None, &[]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // An invalid create answered with the declared 400 (validation runs before chaos).
    let (status, _, _) = h
        .send("POST", "/widgets", Some(json!({"label": 5})), &[])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // The declared rate-limit answer.
    let (status, _, _) = h
        .send("POST", "/widgets", Some(json!({"label": "a"})), &[])
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    // A problem+json refusal: the rate limit of an operation that declares no 429.
    let (status, _, _) = h.send("GET", "/widgets/a", None, &[]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let records = h.records();
    assert_eq!(records.len(), 4);
    for r in &records[..3] {
        assert!(
            gaps_of(r)
                .and_then(Value::as_array)
                .is_some_and(|g| g.contains(&json!("no_identity"))),
            "{} {}: {}",
            r.method,
            r.path,
            r.request
        );
    }
    assert!(gaps_of(&records[3]).is_none(), "{}", records[3].request);
    assert!(h.provider.requests().is_empty());
}

#[cfg(feature = "graphql")]
const THINGS_SDL: &str = r#"
type Thing { id: ID! name: String }
type Query { thing(id: ID!): Thing  things: [Thing] }
"#;

#[cfg(feature = "graphql")]
async fn graphql_post(router: &axum::Router, query: &str) -> Value {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(json!({"query": query}).to_string()))
        .unwrap();
    let (status, _, body) = support::send_to(router, req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("errors").is_none(), "{body}");
    body
}

/// A root field `prepare` answers in a batch (here from the response cache) carries
/// its operation's static gaps, as the same field sent alone does, and the request's
/// one record holds every field's. Sent alone again, the field is answered from the
/// cache by `prepare` outside a batch and is marked the same way.
#[cfg(feature = "graphql")]
#[tokio::test]
async fn a_graphql_field_prepare_answers_in_a_batch_is_marked() {
    let h = support::described_with_gaps_and(
        THINGS_SDL,
        json!({}),
        vec![
            json!({"__typename": "Thing", "id": "t1", "name": "a"}),
            json!({"things": [{"__typename": "Thing", "id": "t2", "name": "b"}]}),
        ],
        with_gaps(&[
            ("Query.thing", &[GapCode::NoTarget]),
            ("Query.things", &[GapCode::NoRecord]),
        ]),
        &HarnessOptions {
            cached: true,
            ..HarnessOptions::default()
        },
    );
    let router = mb_protocols::graphql::try_router(Arc::clone(&h.rt)).unwrap();
    graphql_post(&router, r#"{ thing(id: "t1") { id name } }"#).await;
    graphql_post(
        &router,
        r#"{ thing(id: "t1") { id name } things { id name } }"#,
    )
    .await;
    let records = h.records();
    assert_eq!(records.len(), 2);
    assert_eq!(gaps_of(&records[0]), Some(&json!(["no_target"])));
    // One record for the two fields, with each field's gaps once.
    assert_eq!(
        gaps_of(&records[1]),
        Some(&json!(["no_target", "no_record"])),
        "{}",
        records[1].request
    );
    // The batched `thing` was not generated again.
    assert_eq!(h.provider.requests().len(), 2);
    // The same field alone again: a single root field is not batched, so the cache
    // answer comes from `prepare` through `generate_narrowed`, the only place that
    // seeds it on this path.
    graphql_post(&router, r#"{ thing(id: "t1") { id name } }"#).await;
    let records = h.records();
    assert_eq!(records.len(), 3);
    assert_eq!(gaps_of(&records[2]), Some(&json!(["no_target"])));
    assert_eq!(h.provider.requests().len(), 2);
}

#[cfg(feature = "soap")]
fn envelope11(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Header/><soap:Body>{body}</soap:Body></soap:Envelope>"#
    )
}

/// A SOAP operation's declared rate-limit fault and its generated forced-500 fault
/// detail are recorded with the operation's static gaps.
#[cfg(feature = "soap")]
#[tokio::test]
async fn a_soap_rate_limited_answer_carries_the_static_gaps() {
    let path = format!(
        "{}/../../fixtures/wsdl/calculator.wsdl",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut bundle = mb_import::compile(&std::fs::read(path).unwrap()).unwrap();
    let op_id = {
        let op = bundle
            .operations
            .iter_mut()
            .find(|o| o.id.ends_with("Divide"))
            .unwrap();
        let mut limited = op
            .outputs
            .iter()
            .find(|o| o.status == mb_mcm::StatusSelector::Code(500))
            .unwrap()
            .clone();
        limited.status = mb_mcm::StatusSelector::Code(429);
        op.outputs.push(limited);
        op.id.clone()
    };
    let fault = json!({"message": "cannot divide by zero", "dividend": 4.5});
    let h = support::harness_of(bundle, vec![fault], &HarnessOptions::default(), |mut rt| {
        rt.limits.rate = Some(Arc::new(RateLimiter::new(0.01, 1)));
        support::with_effective(rt, json!({}), false)
            .with_test_gaps(with_gaps(&[(op_id.as_str(), &[GapCode::NoTarget])]))
    });
    let router = mb_protocols::soap::try_router(Arc::clone(&h.rt)).unwrap();
    let divide = || {
        axum::http::Request::builder()
            .method("POST")
            .uri("/calculator.asmx")
            .header("content-type", "text/xml")
            .header("SOAPAction", "\"http://tempuri.org/Divide\"")
            .header("X-Mockingbird-Status", "500")
            .body(axum::body::Body::from(envelope11(
                r#"<Divide xmlns="http://tempuri.org/"><dividend>4.5</dividend><divisor>0</divisor></Divide>"#,
            )))
            .unwrap()
    };
    let (status, _, body) = support::send_to(&router, divide()).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    let (status, _, body) = support::send_to(&router, divide()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    let records = h.records();
    assert_eq!(records.len(), 2);
    for r in &records {
        assert_eq!(
            gaps_of(r),
            Some(&json!(["no_target"])),
            "{} {}",
            r.status,
            r.request
        );
    }
}
