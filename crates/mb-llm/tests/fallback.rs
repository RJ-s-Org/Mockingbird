#![allow(clippy::unwrap_used)]

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
