#![cfg(test)]
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use mb_engine::ndjson::LineSplitter;
use mb_engine::{
    Engine, EngineConfig, EngineError, EngineGuards, OperationCall, ScenarioSpec, SchemaValidator,
    Source, StreamContext, StreamItem, TokenBudget,
};
use mb_import::compile;
use mb_llm::{LlmError, Provider, ResponseFormat, Role, ScriptedProvider, Usage};
use mb_mcm::{Bundle, Operation, OperationKind};
use proptest::prelude::*;
use serde_json::json;

const STREAM_CONTRACT: &str = r#"
openapi: 3.1.0
info: {title: Ticks, version: "1"}
paths:
  /ticks:
    get:
      operationId: ticks
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                type: object
                required: [id, "n"]
                properties: {id: {type: string}, "n": {type: integer}}
"#;

/// A `StreamServer` operation whose item is `{id: string, n: integer}`.
fn stream_bundle() -> Arc<Bundle> {
    let mut bundle = compile(STREAM_CONTRACT.as_bytes()).unwrap();
    bundle.operations[0].kind = OperationKind::StreamServer;
    Arc::new(bundle)
}

fn op(bundle: &Bundle) -> Operation {
    bundle.operation("ticks").unwrap().clone()
}

fn call() -> OperationCall {
    OperationCall {
        operation_id: "ticks".into(),
        ..OperationCall::default()
    }
}

fn item_validator(bundle: &Bundle) -> SchemaValidator {
    let op = op(bundle);
    SchemaValidator::new(&bundle.with_reachable_defs(op.stream_item_schema().unwrap())).unwrap()
}

fn engine_with(provider: &Arc<ScriptedProvider>, config: EngineConfig) -> Arc<Engine> {
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    Arc::new(Engine::new(Some(dyn_provider), config))
}

fn offline() -> Arc<Engine> {
    Arc::new(Engine::new(None, EngineConfig::default()))
}

async fn collect(
    engine: &Arc<Engine>,
    bundle: &Arc<Bundle>,
    ctx: StreamContext,
    max: Option<usize>,
) -> Vec<StreamItem> {
    let op = op(bundle);
    engine
        .generate_stream(Arc::clone(bundle), &op, call(), ctx, max)
        .unwrap()
        .collect()
        .await
}

fn lines(n: usize) -> Vec<Result<String, LlmError>> {
    (0..n)
        .map(|i| Ok(format!("{{\"id\":\"l{i}\",\"n\":{i}}}\n")))
        .collect()
}

fn line_text() -> impl Strategy<Value = String> {
    "[a-z{}\":, 0-9é]{1,12}".prop_filter("non-blank", |s| !s.trim().is_empty())
}

proptest! {
    #[test]
    fn ndjson_split_across_arbitrary_boundaries(
        a in line_text(),
        b in line_text(),
        c in line_text(),
        cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..8),
    ) {
        let text = format!("{a}\n{b}\n{c}");
        let chars: Vec<char> = text.chars().collect();
        let mut points: Vec<usize> = cuts.iter().map(|i| i.index(chars.len() + 1)).collect();
        points.sort_unstable();
        let mut splitter = LineSplitter::new();
        let mut out = Vec::new();
        let mut prev = 0;
        for p in points.into_iter().chain([chars.len()]) {
            let piece: String = chars[prev..p].iter().collect();
            out.extend(splitter.push(&piece));
            prev = p;
        }
        out.extend(splitter.finish());
        prop_assert_eq!(out, vec![a, b, c]);
    }
}

#[test]
fn ndjson_skips_blank_lines_and_strips_cr() {
    let mut s = LineSplitter::new();
    assert_eq!(s.push("a\r\n\n  \nb"), vec!["a".to_string()]);
    assert_eq!(s.push("\n"), vec!["b".to_string()]);
    assert_eq!(s.finish(), None);
}

#[tokio::test]
async fn stream_yields_n_llm_items() {
    let bundle = stream_bundle();
    let provider = Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![
        Ok("{\"id\":\"a\"".into()),
        Ok(",\"n\":1}\n{\"id\":\"b\",\"n\":2}\n".into()),
        Ok("{\"id\":\"c\",\"n\":3}".into()),
    ])]));
    let engine = engine_with(&provider, EngineConfig::default());
    let items = collect(&engine, &bundle, StreamContext::default(), Some(3)).await;
    let got: Vec<(serde_json::Value, Source)> = items
        .iter()
        .map(|i: &StreamItem| (i.value.clone(), i.source))
        .collect();
    assert_eq!(
        got,
        vec![
            (json!({"id": "a", "n": 1}), Source::Llm),
            (json!({"id": "b", "n": 2}), Source::Llm),
            (json!({"id": "c", "n": 3}), Source::Llm),
        ]
    );
    let requests = provider.stream_requests();
    assert_eq!(requests.len(), 1);
    assert!(matches!(requests[0].response_format, ResponseFormat::Text));
    assert!(provider.requests().is_empty());
}

#[derive(Default)]
struct SpentBudget(AtomicU64);

impl TokenBudget for SpentBudget {
    fn exhausted(&self) -> bool {
        false
    }

    fn spend(&self, tokens: u64) {
        self.0.fetch_add(tokens, Ordering::SeqCst);
    }
}

fn total(usage: Usage) -> u64 {
    u64::from(usage.prompt_tokens) + u64::from(usage.completion_tokens)
}

#[tokio::test]
async fn stream_items_carry_their_usage() {
    let bundle = stream_bundle();
    let provider = Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![
        Ok("{\"id\":\"a\",\"n\":1}\n".into()),
        Ok("{\"id\":\"b\",\"n\":2}\n".into()),
        Ok("{\"id\":\"c\",\"n\":3}".into()),
    ])]));
    let budget = Arc::new(SpentBudget::default());
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    let engine = Arc::new(
        Engine::new(Some(dyn_provider), EngineConfig::default()).with_guards(EngineGuards {
            budget: Some(Arc::clone(&budget) as Arc<dyn TokenBudget>),
            ..EngineGuards::default()
        }),
    );
    let items = collect(&engine, &bundle, StreamContext::default(), Some(3)).await;
    assert_eq!(items.len(), 3);
    // The prompt counts toward the first item; each item its own text.
    assert!(items[0].usage.prompt_tokens > 0, "{:?}", items[0].usage);
    for item in &items {
        assert!(item.usage.completion_tokens > 0, "{:?}", item.usage);
    }
    assert!(items[1..].iter().all(|i| i.usage.prompt_tokens == 0));
    // Every byte read was attributed: the items sum to the budget charge.
    let attributed: u64 = items.iter().map(|i| total(i.usage)).sum();
    assert_eq!(attributed, budget.0.load(Ordering::SeqCst));
}

#[tokio::test]
async fn faker_stream_items_cost_nothing() {
    let bundle = stream_bundle();
    let items = collect(&offline(), &bundle, StreamContext::default(), Some(3)).await;
    assert!(items.iter().all(|i| i.usage == Usage::default()));
}

#[tokio::test]
async fn fences_and_trailing_commas_are_tolerated() {
    let bundle = stream_bundle();
    let provider = Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![Ok(
        "```json\n{\"id\":\"a\",\"n\":1},\n\n{\"id\":\"b\",\"n\":2}\n```\n".into(),
    )])]));
    let engine = engine_with(&provider, EngineConfig::default());
    let items = collect(&engine, &bundle, StreamContext::default(), Some(2)).await;
    assert_eq!(items[0].value, json!({"id": "a", "n": 1}));
    assert_eq!(items[1].value, json!({"id": "b", "n": 2}));
    assert!(items.iter().all(|i| i.source == Source::Llm));
}

#[tokio::test]
async fn invalid_line_is_repaired() {
    let bundle = stream_bundle();
    let provider = Arc::new(
        ScriptedProvider::new(vec![Ok(json!({"id": "fixed", "n": 7}).to_string())]).with_streams(
            vec![Ok(vec![Ok(
                "{\"id\":\"a\"}\n{\"id\":\"b\",\"n\":2}\n".into()
            )])],
        ),
    );
    let engine = engine_with(&provider, EngineConfig::default());
    let items = collect(&engine, &bundle, StreamContext::default(), Some(2)).await;
    assert_eq!(items[0].value, json!({"id": "fixed", "n": 7}));
    assert_eq!(items[0].source, Source::Repaired);
    assert_eq!(items[1].value, json!({"id": "b", "n": 2}));
    assert_eq!(items[1].source, Source::Llm);

    let repairs = provider.requests();
    assert_eq!(repairs.len(), 1);
    let stream_system = &provider.stream_requests()[0].messages[0].content;
    assert_eq!(repairs[0].messages[0].role, Role::System);
    assert_eq!(&repairs[0].messages[0].content, stream_system);
    let user = &repairs[0].messages[1].content;
    assert!(user.starts_with("Fix this stream item. Errors: "), "{user}");
    assert!(user.contains("<item>{\"id\":\"a\"}</item>"), "{user}");
    assert!(
        user.ends_with("\nReturn only the corrected JSON."),
        "{user}"
    );
    assert!(matches!(
        repairs[0].response_format,
        ResponseFormat::JsonSchema { .. }
    ));
}

#[tokio::test]
async fn unrepairable_line_falls_back_to_faker() {
    let bundle = stream_bundle();
    let provider = Arc::new(
        ScriptedProvider::new(vec![Ok("still not it".into())])
            .with_streams(vec![Ok(vec![Ok("nonsense\n".into())])]),
    );
    let engine = engine_with(&provider, EngineConfig::default());
    let items = collect(&engine, &bundle, StreamContext::default(), Some(1)).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].source, Source::Fallback);
    assert!(item_validator(&bundle).validate(&items[0].value).is_ok());
}

#[tokio::test]
async fn provider_error_mid_stream_fills_with_faker() {
    let bundle = stream_bundle();
    let provider = Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![
        Ok("{\"id\":\"a\",\"n\":1}\n".into()),
        Err(LlmError::Transport("boom".into())),
    ])]));
    let engine = engine_with(&provider, EngineConfig::default());
    let items = collect(&engine, &bundle, StreamContext::default(), Some(4)).await;
    let sources: Vec<Source> = items.iter().map(|i| i.source).collect();
    assert_eq!(
        sources,
        vec![
            Source::Llm,
            Source::Fallback,
            Source::Fallback,
            Source::Fallback
        ]
    );
    let v = item_validator(&bundle);
    assert!(items.iter().all(|i| v.validate(&i.value).is_ok()));
}

#[tokio::test]
async fn offline_engine_yields_faker_items() {
    let bundle = stream_bundle();
    let items = collect(&offline(), &bundle, StreamContext::default(), None).await;
    assert_eq!(items.len(), 5);
    let v = item_validator(&bundle);
    for item in &items {
        assert_eq!(item.source, Source::Fallback);
        assert!(v.validate(&item.value).is_ok(), "{}", item.value);
    }
}

#[tokio::test]
async fn max_items_caps_requested_count() {
    let bundle = stream_bundle();
    let items = collect(&offline(), &bundle, StreamContext::default(), Some(500)).await;
    assert_eq!(items.len(), 50);
    let items = collect(&offline(), &bundle, StreamContext::default(), Some(0)).await;
    assert_eq!(items.len(), 1);
}

#[tokio::test]
async fn non_stream_operation_has_no_output() {
    let bundle = Arc::new(compile(STREAM_CONTRACT.as_bytes()).unwrap());
    let op = op(&bundle);
    let result = offline().generate_stream(
        Arc::clone(&bundle),
        &op,
        call(),
        StreamContext::default(),
        None,
    );
    assert!(matches!(result, Err(EngineError::NoOutput)));
}

#[tokio::test]
async fn stream_prompt_includes_request_stream_and_count() {
    let bundle = stream_bundle();
    let provider = Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(lines(3))]));
    let engine = engine_with(&provider, EngineConfig::default());
    let ctx = StreamContext {
        request_stream: (0..25).map(|i| json!({"seq": i, "x": "<b>"})).collect(),
        ..StreamContext::default()
    };
    collect(&engine, &bundle, ctx, Some(3)).await;
    let request = &provider.stream_requests()[0];
    let system = &request.messages[0].content;
    assert!(system.contains("Operation: ticks"), "{system}");
    assert!(system.contains("Stream item JSON Schema:\n"), "{system}");
    assert!(!system.contains("Response status"), "{system}");
    let user = &request.messages[1].content;
    assert!(user.contains("<request>"), "{user}");
    assert!(user.contains("<request_stream>\n"), "{user}");
    assert!(!user.contains("{\"seq\":4,"), "{user}");
    assert!(user.contains("{\"seq\":5,"), "{user}");
    assert!(user.contains("{\"seq\":24,"), "{user}");
    assert!(!user.contains("<b>"), "{user}");
    assert!(user.ends_with("Generate 3 items."), "{user}");

    // Without request items there is no block, and the system message is
    // identical for every call.
    let provider = Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(lines(2))]));
    let engine = engine_with(&provider, EngineConfig::default());
    collect(&engine, &bundle, StreamContext::default(), Some(2)).await;
    let request2 = &provider.stream_requests()[0];
    assert_eq!(&request2.messages[0].content, system);
    assert!(!request2.messages[1].content.contains("<request_stream>"));
    assert!(request2.messages[1].content.ends_with("Generate 2 items."));
}

#[tokio::test(start_paused = true)]
async fn idle_timeout_falls_back() {
    let bundle = stream_bundle();
    // Lines 1 s apart stay within the 3 s timeouts; a first line after 4 s
    // misses the first-item deadline, so every item comes from the faker.
    let provider = Arc::new(
        ScriptedProvider::new(vec![])
            .with_streams(vec![Ok(lines(3))])
            .with_chunk_delay(Duration::from_secs(1)),
    );
    let provider_slow_after_first = Arc::new(
        ScriptedProvider::new(vec![])
            .with_streams(vec![Ok(lines(3))])
            .with_chunk_delay(Duration::from_secs(4)),
    );
    let engine = engine_with(&provider, EngineConfig::default());
    let items = collect(&engine, &bundle, StreamContext::default(), Some(3)).await;
    assert!(items.iter().all(|i| i.source == Source::Llm), "{items:?}");

    let engine = engine_with(&provider_slow_after_first, EngineConfig::default());
    let start = tokio::time::Instant::now();
    let items = collect(&engine, &bundle, StreamContext::default(), Some(3)).await;
    assert!(
        items.iter().all(|i| i.source == Source::Fallback),
        "{items:?}"
    );
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_secs(3), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
}

#[tokio::test(start_paused = true)]
async fn item_interval_paces_items() {
    let bundle = stream_bundle();
    let ctx = StreamContext {
        item_interval: Some(Duration::from_millis(100)),
        ..StreamContext::default()
    };
    let start = tokio::time::Instant::now();
    let items = collect(&offline(), &bundle, ctx, Some(3)).await;
    assert_eq!(items.len(), 3);
    assert!(start.elapsed() >= Duration::from_millis(200));
    assert!(start.elapsed() < Duration::from_millis(300));
}

#[tokio::test]
async fn scenario_set_patches_items() {
    let bundle = stream_bundle();
    let ctx = StreamContext {
        scenario: Some(ScenarioSpec {
            name: "fixed".into(),
            instructions: None,
            set: Some(json!({"n": 42})),
            status: None,
        }),
        ..StreamContext::default()
    };
    let items = collect(&offline(), &bundle, ctx, Some(3)).await;
    assert!(items.iter().all(|i| i.value["n"] == 42), "{items:?}");

    let ctx = StreamContext {
        scenario: Some(ScenarioSpec {
            name: "invalid".into(),
            instructions: None,
            set: Some(json!({"n": "not a number"})),
            status: None,
        }),
        ..StreamContext::default()
    };
    let items = collect(&offline(), &bundle, ctx, Some(3)).await;
    assert!(items.iter().all(|i| i.value["n"].is_i64()), "{items:?}");
}

#[tokio::test]
async fn dropping_stream_stops_provider() {
    let bundle = stream_bundle();
    let provider = Arc::new(
        ScriptedProvider::new(vec![])
            .with_streams(vec![Ok(lines(50))])
            .with_chunk_delay(Duration::from_millis(20)),
    );
    let engine = engine_with(&provider, EngineConfig::default());
    let op = op(&bundle);
    let mut stream = engine
        .generate_stream(
            Arc::clone(&bundle),
            &op,
            call(),
            StreamContext::default(),
            Some(50),
        )
        .unwrap();
    let first = stream.next().await.unwrap();
    assert_eq!(first.source, Source::Llm);
    drop(stream);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        provider.chunks_yielded() <= 2,
        "{}",
        provider.chunks_yielded()
    );
}

#[test]
fn conform_item_checks_item_schema() {
    let bundle = stream_bundle();
    let op = op(&bundle);
    let engine = offline();
    assert!(engine.conform_item(&bundle, &op, &json!({"id": "a", "n": 1})));
    assert!(!engine.conform_item(&bundle, &op, &json!({"id": "a"})));
    let unary = compile(STREAM_CONTRACT.as_bytes()).unwrap();
    let unary_op = unary.operation("ticks").unwrap();
    assert!(!engine.conform_item(&unary, unary_op, &json!({"id": "a", "n": 1})));
    assert_eq!(engine.config().stream.default_items, 5);
    assert_eq!(engine.config().stream.max_items, 50);
    assert_eq!(engine.config().stream.item_interval, Duration::ZERO);
}

#[test]
fn source_worst_orders_fallback_repaired_llm() {
    assert_eq!(Source::Llm.worst(Source::Llm), Source::Llm);
    assert_eq!(Source::Llm.worst(Source::Repaired), Source::Repaired);
    assert_eq!(Source::Repaired.worst(Source::Llm), Source::Repaired);
    assert_eq!(Source::Fallback.worst(Source::Repaired), Source::Fallback);
    assert_eq!(Source::Llm.worst(Source::Fallback), Source::Fallback);
}
