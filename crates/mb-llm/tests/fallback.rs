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
