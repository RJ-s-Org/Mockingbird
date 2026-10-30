#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use mb_llm::{
    Capabilities, ChatMessage, ChatRequest, ChatResponse, FallbackProvider, LlmError, Provider,
    ResponseFormat, Role, ScriptedProvider,
};

fn request() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage {
            role: Role::User,
            content: "hello".into(),
        }],
        response_format: ResponseFormat::JsonObject,
        temperature: 0.5,
        max_tokens: 64,
        timeout: Duration::from_secs(1),
    }
}

#[tokio::test]
async fn fallback_uses_secondary_on_error() {
    let primary = Arc::new(ScriptedProvider::new(vec![Err(LlmError::Http {
        status: 500,
        body: "boom".into(),
    })]));
    let secondary = Arc::new(ScriptedProvider::new(vec![Ok("from secondary".into())]));
    let fallback = FallbackProvider::new(primary.clone(), secondary.clone());

    let response = fallback.complete(request()).await.unwrap();

    assert_eq!(response.content, "from secondary");
    assert_eq!(primary.requests().len(), 1);
    let forwarded = secondary.requests();
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].messages[0].content, "hello");
    assert_eq!(forwarded[0].max_tokens, 64);
}

#[tokio::test]
async fn fallback_does_not_call_secondary_on_success() {
    let primary = Arc::new(ScriptedProvider::new(vec![Ok("from primary".into())]));
    let secondary = Arc::new(ScriptedProvider::new(vec![Ok("from secondary".into())]));
    let fallback = FallbackProvider::new(primary.clone(), secondary.clone());

    let response = fallback.complete(request()).await.unwrap();

    assert_eq!(response.content, "from primary");
    assert_eq!(primary.requests().len(), 1);
    assert!(secondary.requests().is_empty());
}

#[tokio::test]
async fn fallback_returns_secondary_error_when_both_fail() {
    let primary = Arc::new(ScriptedProvider::new(vec![Err(LlmError::Timeout)]));
    let secondary = Arc::new(ScriptedProvider::new(vec![Err(LlmError::Http {
        status: 503,
        body: "down".into(),
    })]));
    let fallback = FallbackProvider::new(primary, secondary);

    let err = fallback.complete(request()).await.unwrap_err();
    assert!(matches!(err, LlmError::Http { status: 503, .. }), "{err}");
}

struct Caps(Capabilities);

#[async_trait::async_trait]
impl Provider for Caps {
    fn capabilities(&self) -> Capabilities {
        self.0
    }
    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
        Err(LlmError::Transport("unused".into()))
    }
}

#[test]
fn fallback_capabilities_are_the_intersection() {
    let primary: Arc<dyn Provider> = Arc::new(Caps(Capabilities {
        json_schema: true,
        json_mode: true,
    }));
    let secondary: Arc<dyn Provider> = Arc::new(Caps(Capabilities {
        json_schema: false,
        json_mode: true,
    }));
    let caps = FallbackProvider::new(primary, secondary).capabilities();
    assert_eq!(
        caps,
        Capabilities {
            json_schema: false,
            json_mode: true,
        }
    );
}

#[tokio::test]
async fn fallback_rewrites_request_per_hop_and_sums_timeouts() {
    use mb_llm::HopSettings;

    let primary = Arc::new(ScriptedProvider::new(vec![Err(LlmError::Timeout)]));
    let secondary = Arc::new(ScriptedProvider::new(vec![Ok("from secondary".into())]));
    let p = HopSettings {
        timeout: Duration::from_millis(300),
        temperature: 0.1,
        max_tokens: 10,
    };
    let s = HopSettings {
        timeout: Duration::from_secs(2),
        temperature: 0.9,
        max_tokens: 20,
    };
    let fallback = FallbackProvider::new(primary.clone(), secondary.clone())
        .with_primary_settings(p)
        .with_secondary_settings(s);

    assert_eq!(
        fallback.total_timeout(Duration::from_secs(1)),
        Duration::from_millis(2300)
    );
    // Without settings each hop gets the request timeout.
    let plain = FallbackProvider::new(primary.clone(), secondary.clone());
    assert_eq!(
        plain.total_timeout(Duration::from_secs(1)),
        Duration::from_secs(2)
    );

    let response = fallback.complete(request()).await.unwrap();
    assert_eq!(response.content, "from secondary");
    let sent = primary.requests();
    assert_eq!(sent[0].timeout, Duration::from_millis(300));
    assert_eq!(sent[0].max_tokens, 10);
    let sent = secondary.requests();
    assert_eq!(sent[0].timeout, Duration::from_secs(2));
    assert_eq!(sent[0].max_tokens, 20);
    assert!((sent[0].temperature - 0.9).abs() < f32::EPSILON);
}

async fn collect(stream: mb_llm::TextStream) -> Vec<Result<String, LlmError>> {
    use futures::StreamExt;
    stream.collect().await
}

#[tokio::test]
async fn fallback_stream_uses_secondary_when_primary_fails_first() {
    use mb_llm::HopSettings;

    // Primary: stream() itself fails.
    let primary =
        Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Err(LlmError::Timeout)]));
    let secondary = Arc::new(
        ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![Ok("x".into()), Ok("y".into())])]),
    );
    let p = HopSettings {
        timeout: Duration::from_millis(300),
        temperature: 0.1,
        max_tokens: 10,
    };
    let fallback =
        FallbackProvider::new(primary.clone(), secondary.clone()).with_primary_settings(p);
    let items = collect(fallback.stream(request()).await.unwrap()).await;
    let items: Vec<String> = items.into_iter().map(Result::unwrap).collect();
    assert_eq!(items, vec!["x", "y"]);
    assert_eq!(primary.stream_requests()[0].max_tokens, 10);
    assert_eq!(secondary.stream_requests()[0].max_tokens, 64);

    // Primary: the first item is an error.
    let primary = Arc::new(
        ScriptedProvider::new(vec![])
            .with_streams(vec![Ok(vec![Err(LlmError::Decode("bad".into()))])]),
    );
    let secondary =
        Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![Ok("z".into())])]));
    let fallback = FallbackProvider::new(primary.clone(), secondary.clone());
    let items = collect(fallback.stream(request()).await.unwrap()).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].as_ref().unwrap(), "z");
    assert_eq!(secondary.stream_requests().len(), 1);
}

/// The usage a metered stream reports is that of the hop that served it.
#[tokio::test]
async fn fallback_metered_stream_reports_the_serving_hops_usage() {
    use futures::StreamExt;

    let usage = mb_llm::Usage {
        prompt_tokens: 40,
        completion_tokens: 4,
        reasoning_tokens: 0,
    };
    let primary =
        Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Err(LlmError::Timeout)]));
    let secondary = Arc::new(
        ScriptedProvider::new(vec![])
            .with_streams(vec![Ok(vec![Ok("z".into())])])
            .with_stream_usage(usage),
    );
    let fallback = FallbackProvider::new(primary, secondary);
    let metered = fallback.stream_metered(request()).await.unwrap();
    let items: Vec<_> = metered.text.collect().await;
    assert_eq!(items.len(), 1);
    assert_eq!(metered.usage.get(), Some(usage));
}

#[tokio::test]
async fn fallback_stream_does_not_switch_after_first_delta() {
    let primary = Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![
        Ok("a".into()),
        Err(LlmError::Transport("cut".into())),
    ])]));
    let secondary =
        Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![Ok("z".into())])]));
    let fallback = FallbackProvider::new(primary.clone(), secondary.clone());
    let items = collect(fallback.stream(request()).await.unwrap()).await;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].as_ref().unwrap(), "a");
    assert!(matches!(items[1], Err(LlmError::Transport(_))));
    assert!(secondary.stream_requests().is_empty());
}

#[tokio::test]
async fn fallback_stream_uses_secondary_when_primary_ends_without_content() {
    let primary = Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![])]));
    let secondary =
        Arc::new(ScriptedProvider::new(vec![]).with_streams(vec![Ok(vec![Ok("z".into())])]));
    let fallback = FallbackProvider::new(primary.clone(), secondary.clone());
    let items = collect(fallback.stream(request()).await.unwrap()).await;
    let items: Vec<String> = items.into_iter().map(Result::unwrap).collect();
    assert_eq!(items, vec!["z"]);
    assert_eq!(secondary.stream_requests().len(), 1);
}

/// An OpenAI-compatible client of the fake server `server`.
fn openai(server: &wiremock::MockServer) -> Arc<dyn Provider> {
    Arc::new(
        mb_llm::OpenAiCompatible::new(mb_llm::OpenAiConfig {
            base_url: format!("{}/", server.uri()),
            api_key: Some("k".into()),
            model: "m".into(),
            output_mode: mb_llm::OutputMode::JsonObject,
            headers: Vec::new(),
            extra_body: serde_json::Map::new(),
            http: mb_llm::HttpOptions::default(),
        })
        .unwrap(),
    )
}

/// A fake OpenAI-compatible server answering every request with `response`.
async fn fake_openai(response: wiremock::ResponseTemplate) -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

fn sse(body: &'static str) -> wiremock::ResponseTemplate {
    wiremock::ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

const SECONDARY_SSE: &str = concat!(
    r#"data: {"choices":[{"delta":{"content":"from secondary"}}]}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn fallback_stream_fails_over_on_openai_error_status() {
    let primary = fake_openai(wiremock::ResponseTemplate::new(503).set_body_string("down")).await;
    let secondary = fake_openai(sse(SECONDARY_SSE)).await;
    let fallback = FallbackProvider::new(openai(&primary), openai(&secondary));
    let items = collect(fallback.stream(request()).await.unwrap()).await;
    let items: Vec<String> = items.into_iter().map(Result::unwrap).collect();
    assert_eq!(items, vec!["from secondary"]);
}

#[tokio::test]
async fn fallback_stream_fails_over_on_openai_error_event() {
    let primary = fake_openai(sse(concat!(
        r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#,
        "\n\n",
        r#"data: {"error":{"code":429,"message":"rate limited"}}"#,
        "\n\n",
        "data: [DONE]\n\n",
    )))
    .await;
    let secondary = fake_openai(sse(SECONDARY_SSE)).await;
    let fallback = FallbackProvider::new(openai(&primary), openai(&secondary));
    let items = collect(fallback.stream(request()).await.unwrap()).await;
    let items: Vec<String> = items.into_iter().map(Result::unwrap).collect();
    assert_eq!(items, vec!["from secondary"]);
    assert_eq!(primary.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn fallback_stream_fails_over_on_openai_empty_stream() {
    let primary = fake_openai(sse("data: [DONE]\n\n")).await;
    let secondary = fake_openai(sse(SECONDARY_SSE)).await;
    let fallback = FallbackProvider::new(openai(&primary), openai(&secondary));
    let items = collect(fallback.stream(request()).await.unwrap()).await;
    let items: Vec<String> = items.into_iter().map(Result::unwrap).collect();
    assert_eq!(items, vec!["from secondary"]);
}

/// The primary's tokens on a truncated reply are not lost when the
/// secondary answers.
#[tokio::test]
async fn fallback_keeps_the_primarys_usage_on_truncation() {
    let spent = mb_llm::Usage {
        prompt_tokens: 10,
        completion_tokens: 64,
        reasoning_tokens: 0,
    };
    let primary = Arc::new(ScriptedProvider::new(vec![Err(LlmError::Truncated {
        message: "stopped at max_tokens (64)".into(),
        partial: "{\"a\"".into(),
        usage: spent,
    })]));
    let secondary = Arc::new(ScriptedProvider::new(vec![Ok("{}".into())]));
    let fallback = FallbackProvider::new(primary, secondary);
    let response = fallback.complete(request()).await.unwrap();
    assert_eq!(response.usage, spent);
}
