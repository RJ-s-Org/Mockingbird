#![cfg(test)]
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
        source_digest: "d1".into(),
        operations: Vec::new(),
        defs: Map::new(),
        protocol_payload: None,
        channels: vec![channel],
        webhooks: Vec::new(),
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

#[tokio::test]
async fn saturated_limiter_falls_back_within_timeout() {
    let bundle = Arc::new(bundle());
    let limiter = GenerationLimiter::new(1);
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
    while limiter.in_use() == 0 {
        tokio::task::yield_now().await;
    }

    let started = Instant::now();
    let response = generate(&waiter, &bundle).await;
    let elapsed = started.elapsed();
    assert_eq!(response.source, Source::Fallback);
    assert!(
        elapsed < Duration::from_millis(300),
        "took {elapsed:?}, expected under 300 ms"
    );
    assert!(response.body.is_some());

    held.abort();
    let _ = held.await;
    assert_eq!(limiter.in_use(), 0);
}

#[tokio::test]
async fn limiter_releases_permits() {
    let bundle = Arc::new(bundle());
    let limiter = GenerationLimiter::new(2);
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
