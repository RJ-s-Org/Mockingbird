#![cfg(test)]
//! A provider whose key is refused is marked unusable and not called again;
//! one that rate limits is backed off for its `Retry-After`.
use std::sync::Arc;
use std::time::{Duration, Instant};

use mb_llm::*;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn req() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage {
            role: Role::User,
            content: "hi".into(),
        }],
        response_format: ResponseFormat::JsonObject,
        temperature: 0.2,
        max_tokens: 100,
        max_answer_bytes: None,
        timeout: Duration::from_secs(10),
        hedge: 0,
    }
}

/// A client of `server` going through a fresh health; streamed or plain.
fn guarded(server: &MockServer, streamed: bool) -> (OpenAiCompatible, Arc<ProviderHealth>) {
    let health = ProviderHealth::new("default/p");
    let client = OpenAiCompatible::new(OpenAiConfig {
        base_url: server.uri(),
        api_key: Some("test-key".into()),
        model: "m".into(),
        output_mode: OutputMode::JsonObject,
        headers: Vec::new(),
        extra_body: serde_json::Map::new(),
        http: HttpOptions::default(),
    })
    .unwrap()
    .with_repetition_guard(streamed)
    .with_health(Arc::clone(&health));
    (client, health)
}

async fn answering(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

async fn calls(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{"message": {"content": "{\"a\":1}"}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    }))
}

#[tokio::test]
async fn a_402_marks_the_provider_and_the_next_call_does_not_reach_it() {
    for streamed in [false, true] {
        let server = answering(
            ResponseTemplate::new(402)
                .set_body_json(json!({"error": {"message": "Insufficient credits", "code": 402}})),
        )
        .await;
        let (client, health) = guarded(&server, streamed);
        // Not retried: one request.
        let first = client.complete(req()).await.unwrap_err();
        assert!(matches!(first, LlmError::Unavailable(_)), "{first}");
        assert!(first.to_string().starts_with(UNUSABLE_REASON), "{first}");
        assert!(!first.is_retryable());
        assert_eq!(calls(&server).await, 1);
        for _ in 0..3 {
            assert!(matches!(
                client.complete(req()).await,
                Err(LlmError::Unavailable(_))
            ));
        }
        assert!(client.stream(req()).await.is_err());
        assert_eq!(calls(&server).await, 1, "a marked provider is not called");
        assert!(client.blocked().is_some());
        let s = health.snapshot();
        assert_eq!((s.state, s.status, s.fallbacks), ("unusable", Some(402), 4));
    }
}

#[tokio::test]
async fn a_401_marks_the_provider() {
    let server = answering(ResponseTemplate::new(401).set_body_string("bad key")).await;
    let (client, health) = guarded(&server, false);
    assert!(client.complete(req()).await.is_err());
    assert!(client.complete(req()).await.is_err());
    assert_eq!(calls(&server).await, 1);
    assert_eq!(health.snapshot().state, "unusable");
}

#[tokio::test]
async fn a_403_counts_only_when_it_says_the_key_is_spent() {
    let limit = answering(ResponseTemplate::new(403).set_body_json(json!({"error": {
        "message": "Key limit exceeded (total limit). Manage it using https://openrouter.ai/settings/keys",
        "code": 403
    }})))
    .await;
    let (client, health) = guarded(&limit, true);
    assert!(matches!(
        client.complete(req()).await,
        Err(LlmError::Unavailable(_))
    ));
    assert!(client.complete(req()).await.is_err());
    assert_eq!(calls(&limit).await, 1);
    assert_eq!(health.snapshot().state, "unusable");

    let other = answering(ResponseTemplate::new(403).set_body_json(json!({"error": {
        "code": "unsupported_country_region_territory",
        "message": "Country, region, or territory not supported"
    }})))
    .await;
    let (client, health) = guarded(&other, true);
    for _ in 0..2 {
        assert!(matches!(
            client.complete(req()).await,
            Err(LlmError::Http { status: 403, .. })
        ));
    }
    assert_eq!(calls(&other).await, 2, "an ordinary 403 is called again");
    let s = health.snapshot();
    assert_eq!((s.state, s.fallbacks), ("ok", 0));
}

#[tokio::test]
async fn a_429_backs_off_for_its_retry_after_then_calls_again() {
    // `Retry-After: 0` ends at once: the call is an ordinary error.
    let now = answering(ResponseTemplate::new(429).insert_header("retry-after", "0")).await;
    let (client, health) = guarded(&now, false);
    assert!(matches!(
        client.complete(req()).await,
        Err(LlmError::Http { status: 429, .. })
    ));
    assert_eq!(health.snapshot().state, "ok");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok())
        .mount(&server)
        .await;
    let (client, health) = guarded(&server, false);
    // Not retried while backing off (a 429 is otherwise retried once).
    let first = client.complete(req()).await.unwrap_err();
    assert!(first.to_string().starts_with(BACKING_OFF_REASON), "{first}");
    assert_eq!(calls(&server).await, 1);
    let started = Instant::now();
    assert!(matches!(
        client.complete(req()).await,
        Err(LlmError::Unavailable(_))
    ));
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(calls(&server).await, 1);
    let s = health.snapshot();
    assert_eq!(
        (s.state, s.status, s.fallbacks),
        ("backing_off", Some(429), 1)
    );
    assert!(s.until_unix.is_some());

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let answer = client.complete(req()).await.unwrap();
    assert_eq!(answer.content, "{\"a\":1}");
    assert_eq!(calls(&server).await, 2);
    assert_eq!(health.snapshot().state, "ok");
}

/// Inside a `Retry-After: 60` window a call returns at once, without waiting
/// out the window or contacting the provider.
#[tokio::test]
async fn a_call_inside_a_long_backoff_returns_at_once() {
    let server = answering(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "60")
            .set_body_string("rate limited"),
    )
    .await;
    let (client, _) = guarded(&server, true);
    let started = Instant::now();
    assert!(client.complete(req()).await.is_err());
    for _ in 0..5 {
        assert!(matches!(
            client.complete(req()).await,
            Err(LlmError::Unavailable(_))
        ));
        assert!(client.skip().is_some());
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(calls(&server).await, 1);
}

/// A fallback chain is blocked only when every hop is; a blocked primary
/// passes the call to the secondary without contacting it.
#[tokio::test]
async fn a_marked_primary_falls_through_to_the_secondary() {
    let spent = answering(ResponseTemplate::new(402)).await;
    let good = answering(ok()).await;
    let (primary, _) = guarded(&spent, false);
    let (secondary, _) = guarded(&good, false);
    let chain = FallbackProvider::new(Arc::new(primary), Arc::new(secondary));
    for _ in 0..3 {
        assert_eq!(chain.complete(req()).await.unwrap().content, "{\"a\":1}");
    }
    assert_eq!(calls(&spent).await, 1);
    assert_eq!(calls(&good).await, 3);
    assert!(chain.blocked().is_none());
    assert!(chain.skip().is_none());
}
