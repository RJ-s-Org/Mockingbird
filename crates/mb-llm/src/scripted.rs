use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use crate::{
    Capabilities, ChatRequest, ChatResponse, LlmError, MeteredStream, Provider, StreamUsage,
    TextStream, Usage,
};

/// One scripted stream: either `stream()` fails, or it yields these items.
type StreamScript = Result<Vec<Result<String, LlmError>>, LlmError>;

/// Test provider that replays a fixed script and records requests.
pub struct ScriptedProvider {
    script: Mutex<VecDeque<Result<ChatResponse, LlmError>>>,
    requests: Mutex<Vec<ChatRequest>>,
    streams: Option<Mutex<VecDeque<StreamScript>>>,
    stream_requests: Mutex<Vec<ChatRequest>>,
    chunk_delay: Duration,
    chunks_yielded: Arc<AtomicUsize>,
    stream_usage: Option<Usage>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl ScriptedProvider {
    /// Replays `script` reporting zero usage for every call.
    pub fn new(script: Vec<Result<String, LlmError>>) -> Self {
        Self::with_usage(
            script
                .into_iter()
                .map(|r| r.map(|content| (content, Usage::default())))
                .collect(),
        )
    }

    /// Replays `script`, reporting the given usage for each successful call.
    pub fn with_usage(script: Vec<Result<(String, Usage), LlmError>>) -> Self {
        let script = script
            .into_iter()
            .map(|r| r.map(|(content, usage)| ChatResponse { content, usage }))
            .collect();
        Self {
            script: Mutex::new(script),
            requests: Mutex::new(Vec::new()),
            streams: None,
            stream_requests: Mutex::new(Vec::new()),
            chunk_delay: Duration::ZERO,
            chunks_yielded: Arc::new(AtomicUsize::new(0)),
            stream_usage: None,
        }
    }

    /// Each [`Provider::stream`] call pops one entry: an `Err` fails the
    /// call, an `Ok` yields its items in order. Without this, `stream` falls
    /// back to the trait default (one delta from the `complete` script).
    pub fn with_streams(mut self, streams: Vec<StreamScript>) -> Self {
        self.streams = Some(Mutex::new(streams.into()));
        self
    }

    /// Every [`Provider::stream_metered`] stream reports `usage` once its
    /// items are exhausted (without this, streams report none).
    pub fn with_stream_usage(mut self, usage: Usage) -> Self {
        self.stream_usage = Some(usage);
        self
    }

    /// Sleeps `delay` before each streamed chunk.
    pub fn with_chunk_delay(mut self, delay: Duration) -> Self {
        self.chunk_delay = delay;
        self
    }

    /// Chunks handed to stream consumers so far.
    pub fn chunks_yielded(&self) -> usize {
        self.chunks_yielded.load(Ordering::SeqCst)
    }

    /// The `complete` requests received, in order.
    pub fn requests(&self) -> Vec<ChatRequest> {
        locked(&self.requests).clone()
    }

    /// The `stream` requests received, in order.
    pub fn stream_requests(&self) -> Vec<ChatRequest> {
        locked(&self.stream_requests).clone()
    }

    fn pop_complete(&self) -> Result<ChatResponse, LlmError> {
        locked(&self.script)
            .pop_front()
            .unwrap_or_else(|| Err(LlmError::Transport("script exhausted".into())))
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            json_schema: true,
            json_mode: true,
        }
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        locked(&self.requests).push(req);
        self.pop_complete()
    }

    async fn stream(&self, req: ChatRequest) -> Result<TextStream, LlmError> {
        self.stream_metered(req).await.map(|m| m.text)
    }

    async fn stream_metered(&self, req: ChatRequest) -> Result<MeteredStream, LlmError> {
        locked(&self.stream_requests).push(req);
        let chunks = match &self.streams {
            Some(streams) => locked(streams)
                .pop_front()
                .unwrap_or_else(|| Err(LlmError::Transport("script exhausted".into())))?,
            // The trait default's behaviour, without recording a `complete`
            // request: one delta carrying the next `complete` script entry.
            None => vec![Ok(self.pop_complete()?.content)],
        };
        let delay = self.chunk_delay;
        let yielded = Arc::clone(&self.chunks_yielded);
        let usage = StreamUsage::new();
        let (reported, at_end) = (usage.clone(), self.stream_usage);
        let text = futures::stream::iter(chunks)
            .then(move |chunk| {
                let yielded = Arc::clone(&yielded);
                async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    yielded.fetch_add(1, Ordering::SeqCst);
                    chunk
                }
            })
            // Reports the usage when the items run out, and yields nothing.
            .chain(
                futures::stream::once(async move {
                    if let Some(u) = at_end {
                        reported.set(u);
                    }
                    None
                })
                .filter_map(std::future::ready),
            );
        Ok(MeteredStream {
            text: Box::pin(text),
            usage,
        })
    }
}
