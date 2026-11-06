//! Seeded faker answers: the same seed, contract and request give the same
//! body (MB-ENGINE-STUB-9).

use std::sync::Arc;

use futures::StreamExt;
use mb_engine::{
    DEFAULT_FAKER_SEED, Engine, EngineConfig, GenerationContext, OperationCall, SEED_HEADER,
    SchemaValidator, Source, StreamContext,
};
use mb_import::compile;
use mb_llm::{LlmError, Provider, ScriptedProvider};
use mb_mcm::{Bundle, OperationKind};
use serde_json::{Value, json};

const CONTRACT: &str = r##"
openapi: 3.1.0
info: {title: Shop, version: "1"}
paths:
  /orders:
    get:
      operationId: listOrders
      parameters:
        - {name: status, in: query, schema: {type: string}}
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                type: array
                minItems: 3
                items: {$ref: "#/components/schemas/Order"}
  /orders/{orderId}:
    parameters:
      - {name: orderId, in: path, required: true, schema: {type: string}}
    get:
      operationId: getOrder
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Order"}
components:
  schemas:
    Order:
      type: object
      required: [orderId, ref, total, placedAt, placedOn, created_at, lines]
      properties:
        orderId: {type: string}
        ref: {type: string, format: uuid}
        total: {type: number}
        placedAt: {type: string, format: date-time}
        placedOn: {type: string, format: date}
        created_at: {type: integer}
        note: {type: string, pattern: "^[A-Z]{3}-[0-9]{4}$"}
        lines:
          type: array
          items:
            type: object
            required: [sku, qty]
            properties:
              sku: {type: string}
              qty: {type: integer, minimum: 1}
"##;

fn bundle() -> Bundle {
    compile(CONTRACT.as_bytes()).unwrap()
}

fn get_order(id: &str) -> OperationCall {
    let mut call = OperationCall {
        operation_id: "getOrder".into(),
        ..OperationCall::default()
    };
    call.path_params.insert("orderId".into(), json!(id));
    call
}

fn offline() -> Engine {
    Engine::new(None, EngineConfig::default())
}

async fn body(engine: &Engine, bundle: &Bundle, op: &str, call: &OperationCall) -> Value {
    let op = bundle.operation(op).unwrap();
    let response = engine
        .generate(bundle, op, call, GenerationContext::default())
        .await
        .unwrap();
    assert_eq!(response.source, Source::Fallback);
    response.body.unwrap()
}

fn valid(bundle: &Bundle, op: &str, value: &Value) -> bool {
    let op = bundle.operation(op).unwrap();
    let schema = op.success_output().unwrap().schema.as_ref().unwrap();
    SchemaValidator::new(&bundle.with_reachable_defs(schema))
        .unwrap()
        .is_valid(value)
}

#[tokio::test]
async fn offline_answers_repeat_across_engines() {
    let bundle = bundle();
    let call = get_order("o-1");
    // Fresh engines: nothing is cached between them, and their hash maps
    // are keyed differently.
    let first = body(&offline(), &bundle, "getOrder", &call).await;
    let second = body(&offline(), &bundle, "getOrder", &call).await;
    assert_eq!(first, second);
    assert!(valid(&bundle, "getOrder", &first), "{first}");
    assert_eq!(first["orderId"], "o-1", "the path id is still echoed");
    assert_eq!(offline().faker_seed(), Some(DEFAULT_FAKER_SEED));
}

#[tokio::test]
async fn seeded_dates_do_not_follow_the_clock() {
    let bundle = bundle();
    let value = body(&offline(), &bundle, "getOrder", &get_order("o-1")).await;
    // Drawn back from 2026-01-01T00:00:00Z, within 30 days.
    let placed = chrono::DateTime::parse_from_rfc3339(value["placedAt"].as_str().unwrap()).unwrap();
    let anchor = mb_engine::seed::SEEDED_NOW;
    let secs = placed.timestamp();
    assert!((anchor - 30 * 86_400..=anchor).contains(&secs), "{placed}");
    let created = value["created_at"].as_i64().unwrap();
    assert!(
        (anchor - 30 * 86_400..=anchor).contains(&created),
        "{created}"
    );
}

#[tokio::test]
async fn other_requests_and_seeds_give_other_bodies() {
    let bundle = bundle();
    let engine = offline();
    let a = body(&engine, &bundle, "getOrder", &get_order("o-1")).await;
    let b = body(&engine, &bundle, "getOrder", &get_order("o-2")).await;
    assert_ne!(a["ref"], b["ref"], "another path id draws another body");

    let seeded = offline().with_faker_seed(7);
    assert_eq!(seeded.faker_seed(), Some(7));
    let c = body(&seeded, &bundle, "getOrder", &get_order("o-1")).await;
    assert_ne!(a["ref"], c["ref"], "another seed draws another body");
    let d = body(
        &offline().with_faker_seed(7),
        &bundle,
        "getOrder",
        &get_order("o-1"),
    )
    .await;
    assert_eq!(c, d);
}

#[tokio::test]
async fn the_seed_header_overrides_the_engine_seed() {
    let bundle = bundle();
    let mut call = get_order("o-1");
    call.headers.insert(SEED_HEADER.into(), json!("7"));
    let by_header = body(&offline(), &bundle, "getOrder", &call).await;
    let by_engine = body(
        &offline().with_faker_seed(7),
        &bundle,
        "getOrder",
        &get_order("o-1"),
    )
    .await;
    assert_eq!(
        by_header, by_engine,
        "headers other than the seed do not count"
    );
    let default = body(&offline(), &bundle, "getOrder", &get_order("o-1")).await;
    assert_ne!(by_header["ref"], default["ref"]);
}

#[tokio::test]
async fn query_order_does_not_change_the_body() {
    let bundle = bundle();
    let mut a = OperationCall {
        operation_id: "listOrders".into(),
        ..OperationCall::default()
    };
    let mut b = a.clone();
    a.query.insert("status".into(), json!("open"));
    a.query.insert("page".into(), json!("2"));
    b.query.insert("page".into(), json!("2"));
    b.query.insert("status".into(), json!("open"));
    let first = body(&offline(), &bundle, "listOrders", &a).await;
    let second = body(&offline(), &bundle, "listOrders", &b).await;
    assert_eq!(first, second);
    assert!(valid(&bundle, "listOrders", &first), "{first}");
}

#[tokio::test]
async fn unseeded_fallbacks_stay_random() {
    let bundle = bundle();
    let failing = || {
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(vec![
            Err(LlmError::Timeout),
            Err(LlmError::Timeout),
        ]));
        Engine::new(Some(provider), EngineConfig::default())
    };
    assert_eq!(failing().faker_seed(), None);
    let refs: std::collections::HashSet<String> =
        futures::future::join_all((0..4).map(|_| async {
            let engine = failing();
            body(&engine, &bundle, "getOrder", &get_order("o-1")).await["ref"].to_string()
        }))
        .await
        .into_iter()
        .collect();
    assert!(refs.len() > 1, "{refs:?}");

    // A seed makes an LLM engine's fallbacks repeat too.
    let a = body(
        &failing().with_faker_seed(3),
        &bundle,
        "getOrder",
        &get_order("o-1"),
    )
    .await;
    let b = body(
        &failing().with_faker_seed(3),
        &bundle,
        "getOrder",
        &get_order("o-1"),
    )
    .await;
    assert_eq!(a, b);
}

#[tokio::test]
async fn fallback_and_fresh_identity_repeat() {
    let bundle = bundle();
    let op = bundle.operation("getOrder").unwrap();
    let variant = op.success_output().unwrap();
    let call = get_order("o-1");
    let ctx = GenerationContext::default();
    let a = offline().fallback(&bundle, op, variant, 200, &call, &ctx);
    let b = offline().fallback(&bundle, op, variant, 200, &call, &ctx);
    assert_eq!(a.body, b.body);

    let mut op = op.clone();
    op.identity = Some(mb_mcm::Identity {
        path_param: None,
        field: "ref".into(),
        input_field: None,
        path_field: None,
        record: None,
    });
    let body = a.body.unwrap();
    let fresh = offline().fresh_identity(&bundle, &op, &body).unwrap();
    assert_ne!(fresh["ref"], body["ref"]);
    assert_eq!(Some(fresh), offline().fresh_identity(&bundle, &op, &body));
}

#[tokio::test]
async fn offline_streams_repeat_item_for_item() {
    let mut bundle = bundle();
    for op in &mut bundle.operations {
        if op.id == "getOrder" {
            op.kind = OperationKind::StreamServer;
        }
    }
    let bundle = Arc::new(bundle);
    let op = bundle.operation("getOrder").unwrap().clone();
    let items = |engine: Arc<Engine>| {
        let bundle = Arc::clone(&bundle);
        let op = op.clone();
        async move {
            engine
                .generate_stream(
                    bundle,
                    &op,
                    get_order("o-1"),
                    StreamContext::default(),
                    Some(3),
                )
                .unwrap()
                .map(|item| item.value)
                .collect::<Vec<_>>()
                .await
        }
    };
    let first = items(Arc::new(offline())).await;
    let second = items(Arc::new(offline())).await;
    assert_eq!(first.len(), 3);
    assert_eq!(first, second);
    assert_ne!(
        first[0]["ref"], first[1]["ref"],
        "items differ from each other"
    );
}

/// The Shopify example contract, compiled.
fn shopify() -> Bundle {
    let path = format!(
        "{}/../../examples/apis/shopify-admin-graphql/shopify-admin.graphql",
        env!("CARGO_MANIFEST_DIR")
    );
    compile(&std::fs::read(path).unwrap()).unwrap()
}

/// Every seeded offline answer of every Shopify operation validates, so
/// none is flagged with `FAKER_INVALID_REASON`. `InventoryItem.variant` and
/// `ProductVariant.inventoryItem` require each other (no finite instance):
/// the faker leaves lists of variants empty instead of recursing to its
/// hard depth limit (`Query.products` used to fail on every seed).
#[tokio::test]
async fn seeded_shopify_answers_validate_offline() {
    let bundle = shopify();
    let engine = offline();
    let mut invalid = Vec::new();
    for op in &bundle.operations {
        for seed in 0..10u64 {
            let mut call = OperationCall {
                operation_id: op.id.clone(),
                ..OperationCall::default()
            };
            call.headers
                .insert(SEED_HEADER.into(), json!(seed.to_string()));
            let response = engine
                .generate(&bundle, op, &call, GenerationContext::default())
                .await
                .unwrap();
            let reason = response.fallback_reason.unwrap_or_default();
            if reason.contains(mb_engine::FAKER_INVALID_REASON) {
                invalid.push(format!("{} (seed {seed})", op.id));
            }
        }
    }
    assert!(invalid.is_empty(), "{invalid:?}");
}

/// An LLM answer for `getOrder` whose `note` breaks its pattern: the engine
/// repairs it locally with a faker value.
const BAD_NOTE: &str = r#"{"orderId": "o-1", "ref": "6f1c2a52-4f0e-4b8e-9d8a-0c7d2d1f5a10", "total": 12.5, "placedAt": "2026-01-01T00:00:00Z", "placedOn": "2026-01-01", "created_at": 1767225600, "note": "not a note", "lines": [{"sku": "a-1", "qty": 1}]}"#;

/// The `note` the engine repaired `BAD_NOTE` to for `call`, on a fresh
/// engine with `seed`.
async fn repaired_note(seed: Option<u64>, call: &OperationCall) -> String {
    let bundle = bundle();
    let provider: Arc<dyn Provider> =
        Arc::new(ScriptedProvider::new(vec![Ok(BAD_NOTE.to_string())]));
    let engine = Engine::new(Some(provider), EngineConfig::default());
    let engine = match seed {
        Some(seed) => engine.with_faker_seed(seed),
        None => engine,
    };
    let op = bundle.operation("getOrder").unwrap();
    let response = engine
        .generate(&bundle, op, call, GenerationContext::default())
        .await
        .unwrap();
    assert_eq!(response.source, Source::Patched);
    let body = response.body.unwrap();
    assert!(valid(&bundle, "getOrder", &body), "{body}");
    let note = body["note"].as_str().unwrap().to_string();
    assert_ne!(note, "not a note");
    note
}

#[tokio::test]
async fn seeded_repairs_of_an_llm_answer_repeat() {
    let first = repaired_note(Some(7), &get_order("o-1")).await;
    let second = repaired_note(Some(7), &get_order("o-1")).await;
    assert_eq!(first, second, "the same seed and call repair alike");

    let other_call = repaired_note(Some(7), &get_order("o-2")).await;
    assert_ne!(first, other_call, "another call repairs otherwise");
    let other_seed = repaired_note(Some(8), &get_order("o-1")).await;
    assert_ne!(first, other_seed, "another seed repairs otherwise");

    let mut by_header = get_order("o-1");
    by_header.headers.insert(SEED_HEADER.into(), json!("7"));
    assert_eq!(repaired_note(None, &by_header).await, first);
}

#[tokio::test]
async fn unseeded_repairs_stay_random() {
    let call = get_order("o-1");
    let notes: std::collections::HashSet<String> =
        futures::future::join_all((0..4).map(|_| repaired_note(None, &call)))
            .await
            .into_iter()
            .collect();
    assert!(notes.len() > 1, "{notes:?}");
}

const HEADERS: &str = r##"
openapi: 3.0.3
info: {title: t, version: "1"}
paths:
  /mail/send:
    post:
      operationId: send
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {to: {type: string}}}
      responses:
        "202":
          description: ok
          headers:
            X-Message-Id: {schema: {type: string}}
            X-RateLimit-Limit: {schema: {type: integer}}
            X-RateLimit-Remaining: {schema: {type: integer}}
            X-Note: {schema: {type: string, pattern: "^[a-z]{12}$"}}
"##;

fn send(to: &str) -> OperationCall {
    OperationCall {
        operation_id: "send".into(),
        body: Some(json!({ "to": to })),
        ..OperationCall::default()
    }
}

#[test]
fn seeded_declared_headers_repeat_per_call() {
    let bundle = compile(HEADERS.as_bytes()).unwrap();
    let op = bundle.operation("send").unwrap();
    let headers =
        |engine: &Engine, call: &OperationCall| engine.declared_headers(&bundle, op, 202, call);
    let seeded = || offline().with_faker_seed(7);

    let first = headers(&seeded(), &send("a@x.io"));
    assert_eq!(first.len(), 4);
    assert_eq!(first, headers(&seeded(), &send("a@x.io")));
    let other = headers(&seeded(), &send("b@x.io"));
    let id = |h: &[(String, String)]| {
        h.iter()
            .find(|(n, _)| n == "X-Message-Id")
            .unwrap()
            .1
            .clone()
    };
    assert_ne!(id(&first), id(&other), "another call draws another id");

    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(Vec::new()));
    let unseeded = Engine::new(Some(provider), EngineConfig::default());
    let ids: std::collections::HashSet<String> = (0..4)
        .map(|_| id(&headers(&unseeded, &send("a@x.io"))))
        .collect();
    assert!(ids.len() > 1, "{ids:?}");
}
