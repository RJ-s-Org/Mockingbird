use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use mb_engine::pii::{Scrubber, ScrubberConfig};
use mb_engine::{
    Engine, EngineConfig, EngineGuards, GenerationContext, GenerationLimiter, MessageContext,
    OperationCall, SchemaValidator, Source, StreamContext, TokenBudget,
};
use mb_import::compile;
use mb_llm::{
    Capabilities, ChatRequest, ChatResponse, LlmError, Provider, ScriptedProvider, Usage,
};
use mb_mcm::{Bundle, Channel, ChannelBinding, Direction, OperationKind};
use serde_json::{Map, json};

const CONTACT_CONTRACT: &str = r#"
openapi: 3.1.0
info: {title: Contacts, version: "1"}
paths:
  /contact:
    get:
      operationId: getContact
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                type: object
                required: [email, ssn, note]
                properties:
                  email: {type: string, format: email}
                  ssn: {type: string, pattern: "^\\d{3}-\\d{2}-\\d{4}$"}
                  note: {type: string}
"#;

fn bundle() -> Bundle {
    compile(CONTACT_CONTRACT.as_bytes()).unwrap()
}

fn stream_bundle() -> Arc<Bundle> {
    let mut bundle = bundle();
    bundle.operations[0].kind = OperationKind::StreamServer;
    Arc::new(bundle)
}

fn message_bundle() -> Bundle {
    let schema = json!({
        "type": "object",
        "required": ["email"],
        "properties": {"email": {"type": "string"}}
    });
    let channel = Channel {
        id: "contacts".into(),
        title: "Contacts".into(),
        address: "contacts".into(),
        binding: ChannelBinding::Kafka {
            topic: "contacts".into(),
            partitions_hint: None,
        },
        direction: Direction::Publish,
        message_schema: schema,
        message_names: Vec::new(),
        content_type: "application/json".into(),
        key_schema: None,
        headers_schema: None,
        parameters: Map::new(),
        triggers: Vec::new(),
        reply: None,
        state_effect: None,
        semantic_hints: Vec::new(),
    };
    Bundle {
        id: "contacts".into(),
        title: "Contacts".into(),
        version: "1".into(),
        description: None,
        source_digest: "d1".into(),
        operations: Vec::new(),
        defs: Map::new(),
        protocol_payload: None,
        channels: vec![channel],
        webhooks: Vec::new(),
        api_key_names: None,
        inferred: false,
    }
}

fn call() -> OperationCall {
    OperationCall {
        operation_id: "getContact".into(),
        ..OperationCall::default()
    }
}

fn contact(email: &str) -> String {
    json!({"email": email, "ssn": "123-45-6789", "note": "hello"}).to_string()
}

fn validator(bundle: &Bundle) -> SchemaValidator {
    let op = bundle.operation("getContact").unwrap();
    let schema = op.success_output().unwrap().schema.as_ref().unwrap();
    SchemaValidator::new(&bundle.with_reachable_defs(schema)).unwrap()
}

fn engine(provider: Arc<dyn Provider>, timeout: Duration, guards: EngineGuards) -> Arc<Engine> {
    let config = EngineConfig {
        timeout,
        ..EngineConfig::default()
    };
    Arc::new(Engine::new(Some(provider), config).with_guards(guards))
}

fn scripted_engine(
    script: Vec<Result<String, LlmError>>,
    guards: EngineGuards,
) -> (Arc<ScriptedProvider>, Arc<Engine>) {
    let provider = Arc::new(ScriptedProvider::new(script));
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    (
        provider,
        engine(dyn_provider, Duration::from_secs(3), guards),
    )
}

async fn generate(engine: &Engine, bundle: &Bundle) -> mb_engine::GeneratedResponse {
    let op = bundle.operation("getContact").unwrap();
    engine
        .generate(bundle, op, &call(), GenerationContext::default())
        .await
        .unwrap()
}

/// Answers every `complete` after `delay`.
struct SlowProvider {
    delay: Duration,
    body: String,
}

#[async_trait]
impl Provider for SlowProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
        tokio::time::sleep(self.delay).await;
        Ok(ChatResponse {
            content: self.body.clone(),
            usage: Usage::default(),
        })
    }
}

#[derive(Default)]
struct TestBudget {
    exhausted: AtomicBool,
    spent: AtomicU64,
}

impl TokenBudget for TestBudget {
    fn exhausted(&self) -> bool {
        self.exhausted.load(Ordering::SeqCst)
    }

    fn spend(&self, tokens: u64) {
        self.spent.fetch_add(tokens, Ordering::SeqCst);
    }
}

fn scrubber() -> Arc<Scrubber> {
    Arc::new(Scrubber::new(&ScrubberConfig::default()).unwrap())
}

// Paused clock: the provider delay and the queue timeout are tokio timers,
// so the 200 ms wait is virtual and the elapsed time is exact.
#[tokio::test(start_paused = true)]
async fn saturated_limiter_falls_back_within_timeout() {
    let bundle = Arc::new(bundle());
    let limiter = GenerationLimiter::new_shared(1);
    let guards = EngineGuards {
        limiter: Some(Arc::clone(&limiter)),
        ..EngineGuards::default()
    };
    let slow: Arc<dyn Provider> = Arc::new(SlowProvider {
        delay: Duration::from_secs(2),
        body: contact("a@example.com"),
    });
    let holder = engine(Arc::clone(&slow), Duration::from_secs(5), guards.clone());
    let waiter = engine(slow, Duration::from_millis(200), guards);

    let held = {
        let bundle = Arc::clone(&bundle);
        tokio::spawn(async move { generate(&holder, &bundle).await })
    };
    // Bounded: the holder takes its permit within a few scheduler turns.
    for _ in 0..1_000 {
        if limiter.in_use() > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(limiter.in_use(), 1, "the holder never took its permit");

    let started = tokio::time::Instant::now();
    let response = generate(&waiter, &bundle).await;
    let elapsed = started.elapsed();
    assert_eq!(response.source, Source::Fallback);
    assert!(
        elapsed >= Duration::from_millis(200) && elapsed < Duration::from_millis(300),
        "took {elapsed:?}, expected the 200 ms queue timeout"
    );
    assert!(response.body.is_some());
    // The waiter gave up on the queue; the holder still has its permit.
    assert_eq!(limiter.in_use(), 1);
    assert!(!held.is_finished());

    held.abort();
    let _ = held.await;
    assert_eq!(limiter.in_use(), 0);
}

#[tokio::test]
async fn limiter_releases_permits() {
    let bundle = Arc::new(bundle());
    let limiter = GenerationLimiter::new_shared(2);
    assert_eq!(limiter.max(), 2);
    let guards = EngineGuards {
        limiter: Some(Arc::clone(&limiter)),
        ..EngineGuards::default()
    };

    // Success.
    let (_, ok) = scripted_engine(vec![Ok(contact("a@example.com"))], guards.clone());
    assert_eq!(generate(&ok, &bundle).await.source, Source::Llm);
    assert_eq!(limiter.in_use(), 0);

    // Error.
    let (_, failing) = scripted_engine(
        vec![Err(LlmError::Transport("down".into()))],
        guards.clone(),
    );
    assert_eq!(generate(&failing, &bundle).await.source, Source::Fallback);
    assert_eq!(limiter.in_use(), 0);

    // Stream dropped while its LLM request is open.
    let line = format!("{}\n", contact("a@example.com"));
    let provider = Arc::new(
        ScriptedProvider::new(Vec::new())
            .with_streams(vec![Ok(vec![Ok(line.clone()), Ok(line.clone()), Ok(line)])])
            .with_chunk_delay(Duration::from_millis(10)),
    );
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    let streaming = engine(dyn_provider, Duration::from_secs(3), guards);
    let bundle = stream_bundle();
    let op = bundle.operation("getContact").unwrap().clone();
    let mut stream = streaming
        .generate_stream(
            Arc::clone(&bundle),
            &op,
            call(),
            StreamContext::default(),
            Some(3),
        )
        .unwrap();
    let first = stream.next().await.unwrap();
    assert_eq!(first.source, Source::Llm);
    assert_eq!(limiter.in_use(), 1, "the open stream holds its permit");
    drop(stream);
    assert_eq!(limiter.in_use(), 0);
}

#[tokio::test]
async fn exhausted_budget_skips_llm() {
    let bundle = bundle();
    let budget = Arc::new(TestBudget::default());
    budget.exhausted.store(true, Ordering::SeqCst);
    let guards = EngineGuards {
        budget: Some(budget.clone()),
        ..EngineGuards::default()
    };
    let (provider, engine) = scripted_engine(vec![Ok(contact("a@example.com"))], guards);
    let response = generate(&engine, &bundle).await;
    assert_eq!(response.source, Source::Fallback);
    assert_eq!(provider.requests().len(), 0);

    let bundle = stream_bundle();
    let op = bundle.operation("getContact").unwrap().clone();
    let items: Vec<_> = engine
        .generate_stream(
            Arc::clone(&bundle),
            &op,
            call(),
            StreamContext::default(),
            Some(2),
        )
        .unwrap()
        .collect()
        .await;
    assert!(items.iter().all(|i| i.source == Source::Fallback));
    assert_eq!(provider.stream_requests().len(), 0);

    let bundle = message_bundle();
    let message = engine
        .generate_message(&bundle, &bundle.channels[0], &MessageContext::default())
        .await
        .unwrap();
    assert_eq!(message.source, Source::Fallback);
    assert_eq!(provider.requests().len(), 0);
}

#[tokio::test]
async fn budget_receives_spent_tokens() {
    let bundle = bundle();
    let budget = Arc::new(TestBudget::default());
    let guards = EngineGuards {
        budget: Some(budget.clone()),
        ..EngineGuards::default()
    };
    let usage = |p, c| Usage {
        prompt_tokens: p,
        completion_tokens: c,
        reasoning_tokens: 0,
    };
    let provider = Arc::new(ScriptedProvider::with_usage(vec![
        Ok(("not json".into(), usage(100, 7))),
        Ok((contact("a@example.com"), usage(120, 30))),
    ]));
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    let engine = engine(dyn_provider, Duration::from_secs(3), guards);
    let response = generate(&engine, &bundle).await;
    assert_eq!(response.source, Source::Repaired);
    assert_eq!(budget.spent.load(Ordering::SeqCst), 257);
}

#[tokio::test]
async fn scrubber_runs_before_validation() {
    let bundle = bundle();
    let guards = EngineGuards {
        scrubber: Some(scrubber()),
        ..EngineGuards::default()
    };
    let body =
        json!({"email": "a@example.com", "ssn": "900-12-3456", "note": "mail jane@corp.com"});
    let (_, engine) = scripted_engine(vec![Ok(body.to_string())], guards);
    let response = generate(&engine, &bundle).await;
    assert_eq!(response.source, Source::Llm);
    let note = response.body.as_ref().unwrap()["note"].as_str().unwrap();
    assert!(!note.contains("jane@corp.com"), "{note}");
    assert!(
        note.starts_with("mail user") && note.ends_with("@example.com"),
        "{note}"
    );
}

#[tokio::test]
async fn scrubbed_output_still_validates() {
    let bundle = bundle();
    let guards = EngineGuards {
        scrubber: Some(scrubber()),
        ..EngineGuards::default()
    };
    let (_, engine) = scripted_engine(vec![Ok(contact("jane@corp.com"))], guards);
    let response = generate(&engine, &bundle).await;
    assert_eq!(response.source, Source::Llm);
    let body = response.body.unwrap();
    assert!(body["email"].as_str().unwrap().ends_with("@example.com"));
    assert_ne!(body["ssn"], json!("123-45-6789"));
    assert!(body["ssn"].as_str().unwrap().starts_with('9'));
    assert!(validator(&bundle).validate(&body).is_ok());
}

#[tokio::test]
async fn scrubber_applies_to_stream_items_and_messages() {
    let guards = EngineGuards {
        scrubber: Some(scrubber()),
        ..EngineGuards::default()
    };
    let line = format!("{}\n", contact("jane@corp.com"));
    let provider = Arc::new(
        ScriptedProvider::new(vec![Ok(json!({"email": "bob@corp.com"}).to_string())])
            .with_streams(vec![Ok(vec![Ok(line.clone()), Ok(line)])]),
    );
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    let engine = engine(dyn_provider, Duration::from_secs(3), guards);

    let bundle = stream_bundle();
    let op = bundle.operation("getContact").unwrap().clone();
    let items: Vec<_> = engine
        .generate_stream(
            Arc::clone(&bundle),
            &op,
            call(),
            StreamContext::default(),
            Some(2),
        )
        .unwrap()
        .collect()
        .await;
    assert_eq!(items.len(), 2);
    for item in &items {
        assert_eq!(item.source, Source::Llm);
        let email = item.value["email"].as_str().unwrap();
        assert!(email.ends_with("@example.com"), "{email}");
        assert!(item.value["ssn"].as_str().unwrap().starts_with('9'));
    }

    let bundle = message_bundle();
    let message = engine
        .generate_message(&bundle, &bundle.channels[0], &MessageContext::default())
        .await
        .unwrap();
    assert_eq!(message.source, Source::Llm);
    let email = message.payload["email"].as_str().unwrap();
    assert!(email.ends_with("@example.com"), "{email}");
}

#[tokio::test]
async fn oversized_llm_body_falls_back_without_repair() {
    let bundle = bundle();
    let guards = EngineGuards {
        max_response_bytes: Some(16),
        ..EngineGuards::default()
    };
    let (provider, engine) = scripted_engine(
        vec![Ok(contact("a@example.com")), Ok(contact("a@example.com"))],
        guards,
    );
    let response = generate(&engine, &bundle).await;
    assert_eq!(response.source, Source::Fallback);
    assert_eq!(provider.requests().len(), 1);
    assert!(response.body.is_some());
}

/// Streams report no usage, so the engine charges an estimate of
/// `ceil((prompt bytes + streamed bytes) / 4)` once the provider stream
/// closes, including when the consumer drops the stream early.
#[tokio::test]
async fn stream_spends_estimated_tokens() {
    let bundle = stream_bundle();
    let op = bundle.operation("getContact").unwrap().clone();
    let line = format!("{}\n", contact("a@example.com"));
    let open = |chunks: Vec<String>| {
        let budget = Arc::new(TestBudget::default());
        let guards = EngineGuards {
            budget: Some(budget.clone()),
            ..EngineGuards::default()
        };
        let provider = Arc::new(
            ScriptedProvider::new(Vec::new())
                .with_streams(vec![Ok(chunks.into_iter().map(Ok).collect())]),
        );
        let dyn_provider: Arc<dyn Provider> = provider.clone();
        let engine = engine(dyn_provider, Duration::from_secs(3), guards);
        (budget, provider, engine)
    };
    let prompt_bytes = |provider: &ScriptedProvider| -> usize {
        provider.stream_requests()[0]
            .messages
            .iter()
            .map(|m| m.content.len())
            .sum()
    };

    // Fully consumed.
    let (budget, provider, engine) = open(vec![line.clone(), line.clone()]);
    let items: Vec<_> = engine
        .generate_stream(
            Arc::clone(&bundle),
            &op,
            call(),
            StreamContext::default(),
            Some(2),
        )
        .unwrap()
        .collect()
        .await;
    assert!(items.iter().all(|i| i.source == Source::Llm));
    let expected = (prompt_bytes(&provider) + 2 * line.len()).div_ceil(4) as u64;
    assert_eq!(budget.spent.load(Ordering::SeqCst), expected);

    // Dropped after the first item: only what was received is charged.
    let (budget, provider, engine) = open(vec![line.clone(), line.clone(), line.clone()]);
    let mut stream = engine
        .generate_stream(
            Arc::clone(&bundle),
            &op,
            call(),
            StreamContext::default(),
            Some(3),
        )
        .unwrap();
    assert_eq!(stream.next().await.unwrap().source, Source::Llm);
    assert_eq!(budget.spent.load(Ordering::SeqCst), 0, "charged on close");
    drop(stream);
    let expected = (prompt_bytes(&provider) + line.len()).div_ceil(4) as u64;
    assert_eq!(budget.spent.load(Ordering::SeqCst), expected);
}

/// A seeded engine with the scrubber, answering every call with `answers`.
fn seeded_scrubbing_engine(answers: Vec<String>) -> Arc<Engine> {
    let guards = EngineGuards {
        scrubber: Some(scrubber()),
        ..EngineGuards::default()
    };
    let provider: Arc<dyn Provider> =
        Arc::new(ScriptedProvider::new(answers.into_iter().map(Ok).collect()));
    let config = EngineConfig {
        timeout: Duration::from_secs(3),
        ..EngineConfig::default()
    };
    Arc::new(
        Engine::new(Some(provider), config)
            .with_guards(guards)
            .with_faker_seed(7),
    )
}

/// The scrubbed `email` and `ssn` of the answer to `call`.
async fn scrubbed_pair(engine: &Engine, bundle: &Bundle, call: &OperationCall) -> (String, String) {
    let op = bundle.operation("getContact").unwrap();
    let body = engine
        .generate(bundle, op, call, GenerationContext::default())
        .await
        .unwrap()
        .body
        .unwrap();
    let field = |k: &str| body[k].as_str().unwrap().to_owned();
    (field("email"), field("ssn"))
}

fn call_with(query: serde_json::Value, seed: Option<&str>) -> OperationCall {
    let mut call = call();
    call.query = query.as_object().cloned().unwrap_or_default();
    if let Some(seed) = seed {
        call.headers
            .insert("X-Mockingbird-Seed".into(), json!(seed));
    }
    call
}

#[tokio::test]
async fn seeded_answer_scrubbing_follows_the_call_not_the_original() {
    let bundle = bundle();
    let answer =
        |email: &str, ssn: &str| json!({"email": email, "ssn": ssn, "note": "hello"}).to_string();
    let engine = seeded_scrubbing_engine(vec![
        answer("jane@corp.com", "123-45-6789"),
        answer("jane@corp.com", "123-45-6789"),
        answer("bob@else.org", "234-56-7890"),
        answer("jane@corp.com", "123-45-6789"),
    ]);
    let first = call_with(json!({"page": 1}), None);
    let a = scrubbed_pair(&engine, &bundle, &first).await;
    assert!(
        a.0.ends_with("@example.com") && a.1.starts_with('9'),
        "{a:?}"
    );
    assert_eq!(
        a,
        scrubbed_pair(&engine, &bundle, &first).await,
        "same call, same seed"
    );
    assert_eq!(
        a,
        scrubbed_pair(&engine, &bundle, &first).await,
        "another original in the same call and place gets the same replacement"
    );
    let second = call_with(json!({"page": 2}), None);
    let b = scrubbed_pair(&engine, &bundle, &second).await;
    assert_ne!(a.0, b.0, "the same original in another call is not linked");
    assert_ne!(a.1, b.1);
}

#[tokio::test]
async fn seeded_answer_scrubbing_ignores_the_requests_pii() {
    let bundle = bundle();
    let answer = contact("jane@corp.com");
    let engine = seeded_scrubbing_engine(vec![answer.clone(), answer]);
    let a = scrubbed_pair(
        &engine,
        &bundle,
        &call_with(json!({"q": "jane@corp.com"}), None),
    )
    .await;
    let b = scrubbed_pair(
        &engine,
        &bundle,
        &call_with(json!({"q": "bob@else.org"}), None),
    )
    .await;
    assert_eq!(a, b, "the request's PII does not seed the replacements");
}

#[tokio::test]
async fn seeded_answer_scrubbing_honours_the_seed_header() {
    let bundle = bundle();
    let answer = contact("jane@corp.com");
    let engine = seeded_scrubbing_engine(vec![answer.clone(), answer.clone(), answer]);
    let one = call_with(json!({}), Some("11"));
    let a = scrubbed_pair(&engine, &bundle, &one).await;
    assert_eq!(a, scrubbed_pair(&engine, &bundle, &one).await);
    let b = scrubbed_pair(&engine, &bundle, &call_with(json!({}), Some("12"))).await;
    assert_ne!(a, b, "X-Mockingbird-Seed seeds the replacements");
}

#[test]
fn request_scrubbing_stays_random_with_a_seed() {
    let engine = seeded_scrubbing_engine(Vec::new());
    let runs: std::collections::HashSet<String> = (0..4)
        .map(|_| {
            let mut body = json!({"email": "jane@corp.com"});
            engine.scrub(&mut body);
            body["email"].as_str().unwrap().to_owned()
        })
        .collect();
    assert!(runs.len() > 1, "{runs:?}");
}

const ARCHIVE_CONTRACT: &str = r#"
openapi: 3.1.0
info: {title: Contacts, version: "1"}
paths:
  /contacts/{id}/archive:
    post:
      operationId: archiveContact
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "204":
          description: archived
"#;

/// The `email` and `ssn` the effect of a seeded engine's bodyless answer to
/// archiving `id` sets, with the LLM listing the same originals each time.
async fn bodyless_effect_pair(engine: &Engine, id: &str) -> (String, String) {
    let mut bundle = compile(ARCHIVE_CONTRACT.as_bytes()).unwrap();
    bundle.operations[0].kind = OperationKind::Command(mb_mcm::Effect::Update);
    let op = bundle.operation("archiveContact").unwrap();
    let related = [mb_state::StoredResource {
        resource_type: "contacts".into(),
        id: "c-9".into(),
        data: json!({"id": "c-9", "email": "old@corp.com"}),
    }];
    let mut call = OperationCall {
        operation_id: "archiveContact".into(),
        ..OperationCall::default()
    };
    call.path_params.insert("id".into(), json!(id));
    let out = engine
        .generate(
            &bundle,
            op,
            &call,
            GenerationContext {
                related: &related,
                ..GenerationContext::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.status, 204);
    assert_eq!(out.effects.len(), 1, "{:?}", out.effects);
    let set = &out.effects[0].set;
    let field = |k: &str| set[k].as_str().unwrap().to_owned();
    (field("email"), field("ssn"))
}

#[tokio::test]
async fn seeded_bodyless_effects_scrub_alike_for_the_same_call() {
    let effect = json!({
        "effects": [{"type": "contacts", "id": "c-9", "set": {"email": "jane@corp.com", "ssn": "123-45-6789"}}],
        "response": null
    })
    .to_string();
    let engine = seeded_scrubbing_engine(vec![effect.clone(); 3]);
    let a = bodyless_effect_pair(&engine, "c-1").await;
    assert!(
        a.0.ends_with("@example.com") && a.0 != "jane@corp.com",
        "{a:?}"
    );
    assert_eq!(
        a,
        bodyless_effect_pair(&engine, "c-1").await,
        "same seed and call, same replacements"
    );
    let b = bodyless_effect_pair(&engine, "c-2").await;
    assert_ne!(a, b, "another call replaces otherwise");
}

/// A provider chain whose hops add up to two minutes and which never answers in
/// time.
struct HangingChain;

#[async_trait]
impl Provider for HangingChain {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    fn total_timeout(&self, _request_timeout: Duration) -> Duration {
        Duration::from_secs(120)
    }

    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
        tokio::time::sleep(Duration::from_secs(600)).await;
        Err(LlmError::Timeout)
    }
}

/// A served answer waits at most the generation timeout, even when a fallback
/// chain's hops add up to more, and the faker answers then.
#[tokio::test(start_paused = true)]
async fn a_chain_waits_at_most_the_generation_timeout() {
    let bundle = bundle();
    let config = EngineConfig {
        timeout: Duration::from_secs(60),
        generation_timeout: Some(Duration::from_secs(20)),
        ..EngineConfig::default()
    };
    let engine = Engine::new(Some(Arc::new(HangingChain)), config);
    let started = tokio::time::Instant::now();
    let response = generate(&engine, &bundle).await;
    let elapsed = started.elapsed();
    assert_eq!(response.source, Source::Fallback);
    assert!(
        elapsed >= Duration::from_secs(20) && elapsed < Duration::from_secs(21),
        "took {elapsed:?}, expected the 20 s generation timeout"
    );
}

/// Answers each call from its script entry: after its delay, with its body.
struct PacedProvider {
    script: Vec<(Duration, String)>,
    calls: AtomicU64,
}

#[async_trait]
impl Provider for PacedProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let index = usize::try_from(n).unwrap_or(usize::MAX);
        let Some((delay, body)) = self.script.get(index).cloned() else {
            return Err(LlmError::Transport("script exhausted".into()));
        };
        tokio::time::sleep(delay).await;
        if body == "ERROR" {
            return Err(LlmError::Http {
                status: 400,
                body: "bad request".into(),
            });
        }
        Ok(ChatResponse {
            content: body,
            usage: Usage::default(),
        })
    }
}

fn hedging_engine(script: Vec<(Duration, String)>) -> (Arc<PacedProvider>, Engine) {
    hedging_engine_with(script, Duration::from_secs(2), EngineGuards::default())
}

fn hedging_engine_with(
    script: Vec<(Duration, String)>,
    hedge_after: Duration,
    guards: EngineGuards,
) -> (Arc<PacedProvider>, Engine) {
    let provider = Arc::new(PacedProvider {
        script,
        calls: AtomicU64::new(0),
    });
    let config = EngineConfig {
        timeout: Duration::from_secs(60),
        hedge_after: Some(hedge_after),
        ..EngineConfig::default()
    };
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    (
        provider,
        Engine::new(Some(dyn_provider), config).with_guards(guards),
    )
}

/// A call still unanswered after `hedge_after` is sent once more, and the first
/// answer that passes is served.
#[tokio::test(start_paused = true)]
async fn a_slow_call_is_hedged_and_the_faster_answer_served() {
    let bundle = bundle();
    let (provider, engine) = hedging_engine(vec![
        (Duration::from_secs(30), contact("slow@example.com")),
        (Duration::from_secs(1), contact("fast@example.com")),
    ]);
    let started = tokio::time::Instant::now();
    let response = generate(&engine, &bundle).await;
    let elapsed = started.elapsed();
    assert_eq!(response.source, Source::Llm);
    assert_eq!(
        response.body.as_ref().and_then(|b| b["email"].as_str()),
        Some("fast@example.com")
    );
    assert!(
        elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(4),
        "took {elapsed:?}: 2 s before the hedge, 1 s for its answer"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}

/// An answer that does not pass its check sends the hedge at once, and the
/// hedge's valid answer is served without a repair round.
#[tokio::test(start_paused = true)]
async fn an_invalid_answer_is_hedged_at_once() {
    let bundle = bundle();
    let (provider, engine) = hedging_engine(vec![
        (Duration::from_millis(100), "{\"nope\": true".to_string()),
        (Duration::from_millis(100), contact("second@example.com")),
    ]);
    let started = tokio::time::Instant::now();
    let response = generate(&engine, &bundle).await;
    assert_eq!(
        response.source,
        Source::Llm,
        "{:?}",
        response.fallback_reason
    );
    assert_eq!(
        response.body.as_ref().and_then(|b| b["email"].as_str()),
        Some("second@example.com")
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}

/// When neither answer passes, the first goes on to the repair round.
#[tokio::test(start_paused = true)]
async fn two_invalid_answers_go_on_to_the_repair() {
    let bundle = bundle();
    let (provider, engine) = hedging_engine(vec![
        (Duration::from_millis(100), "{\"nope\": true".to_string()),
        (Duration::from_millis(100), "{\"nope\": 2".to_string()),
        (Duration::from_millis(100), contact("repaired@example.com")),
    ]);
    let response = generate(&engine, &bundle).await;
    assert_eq!(
        response.source,
        Source::Repaired,
        "{:?}",
        response.fallback_reason
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
}

/// A call that errors is not hedged.
#[tokio::test(start_paused = true)]
async fn an_error_is_not_hedged() {
    let bundle = bundle();
    let (provider, engine) = hedging_engine(vec![
        (Duration::from_millis(100), "ERROR".to_string()),
        (Duration::from_millis(100), contact("unused@example.com")),
    ]);
    let response = generate(&engine, &bundle).await;
    assert_eq!(response.source, Source::Fallback);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

/// `hedge_after: 0s` never hedges, and neither does a full limiter: the hedge
/// never waits for a slot.
#[tokio::test(start_paused = true)]
async fn no_hedge_when_off_or_without_a_free_slot() {
    let bundle = bundle();
    let slow = || {
        vec![
            (Duration::from_secs(5), contact("only@example.com")),
            (Duration::from_millis(1), contact("hedge@example.com")),
        ]
    };
    let (provider, engine) = hedging_engine_with(slow(), Duration::ZERO, EngineGuards::default());
    let response = generate(&engine, &bundle).await;
    assert_eq!(
        response.body.as_ref().and_then(|b| b["email"].as_str()),
        Some("only@example.com")
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

    let guards = EngineGuards {
        limiter: Some(GenerationLimiter::new_shared(1)),
        ..EngineGuards::default()
    };
    let (provider, engine) = hedging_engine_with(slow(), Duration::from_secs(2), guards);
    let response = generate(&engine, &bundle).await;
    assert_eq!(
        response.body.as_ref().and_then(|b| b["email"].as_str()),
        Some("only@example.com")
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}
