use std::time::Duration;

use futures::StreamExt;
use mb_llm::{
    Capabilities, ChatMessage, ChatRequest, ChatResponse, LlmError, Provider, ResponseFormat, Role,
    ScriptedProvider, Usage,
};

fn request() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage {
            role: Role::User,
            content: "hello".into(),
        }],
        response_format: ResponseFormat::Text,
        temperature: 0.5,
        max_tokens: 64,
        timeout: Duration::from_secs(1),
    }
}

/// Implements only `complete`, so `stream` is the trait default.
struct Once;

#[async_trait::async_trait]
impl Provider for Once {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: false,
            json_mode: false,
        }
    }
    async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
        Ok(ChatResponse {
            content: "whole".into(),
            usage: Usage::default(),
        })
    }
}

#[tokio::test]
async fn scripted_returns_in_order() {
    let p = ScriptedProvider::new(vec![Ok("one".into()), Ok("two".into())]);
    let a = p.complete(request()).await.unwrap();
    let b = p.complete(request()).await.unwrap();
    assert_eq!((a.content.as_str(), b.content.as_str()), ("one", "two"));
    // Every request is recorded, as sent.
    let sent = p.requests();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|r| r.messages[0].content == "hello"));
    // The script is exhausted now.
    assert!(matches!(
        p.complete(request()).await,
        Err(LlmError::Transport(m)) if m == "script exhausted"
    ));
}

#[tokio::test]
async fn default_stream_yields_complete_once() {
    let items: Vec<Result<String, LlmError>> =
        Once.stream(request()).await.unwrap().collect().await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].as_ref().unwrap(), "whole");
}

#[tokio::test(start_paused = true)]
async fn scripted_stream_counts_chunks() {
    let chunks = (0..5).map(|i| Ok(i.to_string())).collect();
    let p = ScriptedProvider::new(vec![])
        .with_streams(vec![Ok(chunks)])
        .with_chunk_delay(Duration::from_millis(10));
    let start = tokio::time::Instant::now();
    let taken: Vec<String> = p
        .stream(request())
        .await
        .unwrap()
        .take(2)
        .map(Result::unwrap)
        .collect()
        .await;
    assert_eq!(taken, vec!["0", "1"]);
    assert_eq!(p.chunks_yielded(), 2);
    assert!(start.elapsed() >= Duration::from_millis(20));
    assert_eq!(p.stream_requests().len(), 1);
    assert!(p.requests().is_empty());

    // The script is exhausted now.
    assert!(matches!(
        p.stream(request()).await,
        Err(LlmError::Transport(m)) if m == "script exhausted"
    ));
}
