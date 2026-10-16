//! The request log writer: a [`RequestRecorder`] that queues each record
//! and, off the request path, redacts it, appends it to the control store
//! and fans it out to live subscribers (the console's request tail).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::Utc;
use mb_protocols::{RequestRecord, RequestRecorder};
use mb_store::{ControlChange, ControlStore, DEFAULT_WORKSPACE, NewRequestLogEntry};
use serde_json::Value;
use tokio::sync::{Notify, broadcast, mpsc};
use uuid::Uuid;

/// Records waiting to be written; beyond this, new records are dropped.
pub const QUEUE_CAPACITY: usize = 4096;

/// Capacity of the [`StoreRecorder::live`] broadcast channel.
pub const LIVE_CAPACITY: usize = 1024;

/// Replacement for the value of a redacted field.
pub const REDACTED: &str = "[redacted]";

/// Largest recorded request or response body, as serialized JSON (or, for
/// a request body, as raw bytes); a larger one is replaced by a marker
/// (see [`truncate_body`]).
pub const MAX_RECORDED_BODY_BYTES: usize = 64 * 1024;

/// Bytes of a truncated body kept as its preview.
pub const PREVIEW_BYTES: usize = 1024;

/// Writes [`RequestRecord`]s to the request log through a bounded queue
/// drained by one background task, so [`RequestRecorder::record`] never
/// waits on the store and does no heavy work: the writer redacts records
/// and truncates their bodies before storing and broadcasting them.
pub struct StoreRecorder {
    queue: mpsc::Sender<RequestRecord>,
    /// Tells the writer to drain the queue and stop.
    stop: Arc<Notify>,
    /// The writer task, taken by [`StoreRecorder::shutdown`].
    writer: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    settings: Arc<ArcSwap<Settings>>,
    live: broadcast::Sender<RequestRecord>,
    dropped: AtomicU64,
}

impl StoreRecorder {
    /// Starts the writer task; it must be called inside a Tokio runtime.
    /// The task stops once the recorder is dropped and the queue drained.
    ///
    /// `retention` is the number of entries kept per mock; `redact_fields`
    /// are JSON keys whose values are replaced with `"[redacted]"`.
    pub fn new(
        store: Arc<dyn ControlStore>,
        retention: u32,
        redact_fields: Vec<String>,
    ) -> Arc<Self> {
        let (queue, rx) = mpsc::channel(QUEUE_CAPACITY);
        let (live, _) = broadcast::channel(LIVE_CAPACITY);
        let settings = Arc::new(ArcSwap::from_pointee(Settings {
            retention,
            redact_fields,
        }));
        let writer = Writer {
            changes: store.subscribe(),
            store,
            ids: HashMap::new(),
            settings: Arc::clone(&settings),
            live: live.clone(),
        };
        let stop = Arc::new(Notify::new());
        let handle = tokio::spawn(writer.run(rx, Arc::clone(&stop)));
        Arc::new(Self {
            queue,
            stop,
            writer: std::sync::Mutex::new(Some(handle)),
            settings,
            live,
            dropped: AtomicU64::new(0),
        })
    }

    /// Replaces the retention and the redacted fields; records written from
    /// now on use the new values.
    pub fn set_settings(&self, retention: u32, redact_fields: Vec<String>) {
        self.settings.store(Arc::new(Settings {
            retention,
            redact_fields,
        }));
    }

    /// Redacted, truncated records as they are written.
    pub fn live(&self) -> broadcast::Receiver<RequestRecord> {
        self.live.subscribe()
    }

    /// Stops accepting records and waits, at most `timeout`, for the writer
    /// to write the ones already queued. Later records are dropped. Call it
    /// on graceful shutdown so the tail of the request log is not lost.
    pub async fn shutdown(&self, timeout: Duration) {
        let handle = self.writer.lock().ok().and_then(|mut w| w.take());
        let Some(handle) = handle else {
            return;
        };
        self.stop.notify_one();
        if tokio::time::timeout(timeout, handle).await.is_err() {
            tracing::warn!(
                "request log writer did not drain within {}s; remaining entries are lost",
                timeout.as_secs()
            );
        }
    }

    /// Records dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl RequestRecorder for StoreRecorder {
    /// Queues the record for the writer. The only work done here is a
    /// cheap bound: a request body whose raw size is known to exceed
    /// [`MAX_RECORDED_BODY_BYTES`] is replaced by `{"truncated": true,
    /// "bytes": N}` (no preview: it could not be redacted unparsed).
    fn record(&self, mut r: RequestRecord) {
        if let Some(bytes) = r
            .request_body_bytes
            .filter(|n| *n > MAX_RECORDED_BODY_BYTES)
            && let Some(body) = r.request.get_mut("body")
        {
            *body = serde_json::json!({"truncated": true, "bytes": bytes});
        }
        // `Closed` means the writer task is gone (its runtime shut down).
        if let Err(mpsc::error::TrySendError::Full(r) | mpsc::error::TrySendError::Closed(r)) =
            self.queue.try_send(r)
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("mb_request_log_dropped_total", "mock" => r.mock).increment(1);
        }
    }
}

/// Request-log settings that a config reload may change.
struct Settings {
    retention: u32,
    redact_fields: Vec<String>,
}

/// The background half of a [`StoreRecorder`].
struct Writer {
    store: Arc<dyn ControlStore>,
    changes: broadcast::Receiver<ControlChange>,
    /// Mock name → id. An entry is evicted when the mock changes, so a mock
    /// deleted and recreated under the same name is looked up afresh.
    ids: HashMap<String, Uuid>,
    settings: Arc<ArcSwap<Settings>>,
    live: broadcast::Sender<RequestRecord>,
}

impl Writer {
    async fn run(mut self, mut rx: mpsc::Receiver<RequestRecord>, stop: Arc<Notify>) {
        let mut changes_open = true;
        loop {
            tokio::select! {
                biased;
                () = stop.notified() => {
                    // Refuse new records, then write the queued ones.
                    rx.close();
                    while let Some(record) = rx.recv().await {
                        self.write(record).await;
                    }
                    break;
                }
                change = self.changes.recv(), if changes_open => match change {
                    Ok(ControlChange::Mock { name, .. }) => {
                        self.ids.remove(&name);
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => self.ids.clear(),
                    Err(broadcast::error::RecvError::Closed) => changes_open = false,
                },
                record = rx.recv() => match record {
                    Some(record) => self.write(record).await,
                    None => break,
                },
            }
        }
    }

    /// Redacts and truncates a record, then broadcasts and stores it.
    async fn write(&mut self, mut r: RequestRecord) {
        let settings = self.settings.load_full();
        redact(&mut r.request, &settings.redact_fields);
        if let Some(response) = &mut r.response {
            redact(response, &settings.redact_fields);
        }
        if let Some(body) = r.request.get_mut("body") {
            truncate_body(body);
        }
        if let Some(response) = &mut r.response {
            truncate_body(response);
        }
        // No receivers is not an error: nobody is tailing.
        let _ = self.live.send(r.clone());

        let Some(mock_id) = self.mock_id(&r.mock).await else {
            return;
        };
        let entry = NewRequestLogEntry {
            mock_id,
            operation_id: r.operation_id,
            session: r.session,
            method: r.method,
            path: r.path,
            status: r.status,
            source: r.source,
            latency_ms: u64::try_from(r.latency.as_millis()).unwrap_or(u64::MAX),
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            request: r.request,
            response: r.response,
            at: Utc::now(),
            kind: mb_store::RequestKind::parse(r.kind.as_str()).unwrap_or_default(),
        };
        if let Err(error) = self
            .store
            .append_request_log(entry, settings.retention)
            .await
        {
            // The cached id may belong to a mock deleted since.
            self.ids.remove(&r.mock);
            tracing::warn!(mock = %r.mock, %error, "cannot write request log entry");
            metrics::counter!("mb_request_log_errors_total", "mock" => r.mock).increment(1);
        }
    }

    async fn mock_id(&mut self, name: &str) -> Option<Uuid> {
        if let Some(id) = self.ids.get(name) {
            return Some(*id);
        }
        match self.store.get_mock(DEFAULT_WORKSPACE, name).await {
            Ok(Some(mock)) => {
                self.ids.insert(name.to_string(), mock.id);
                Some(mock.id)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(mock = %name, %error, "cannot resolve mock for request log");
                None
            }
        }
    }
}

/// Replaces a body whose serialized JSON exceeds
/// [`MAX_RECORDED_BODY_BYTES`] with `{"truncated": true, "bytes": N,
/// "preview": "<first PREVIEW_BYTES of the JSON>"}`. Run it after
/// [`redact`], so the preview holds no redacted values.
pub fn truncate_body(body: &mut Value) {
    let Ok(text) = serde_json::to_string(body) else {
        return;
    };
    if text.len() <= MAX_RECORDED_BODY_BYTES {
        return;
    }
    let mut end = PREVIEW_BYTES.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    *body = serde_json::json!({
        "truncated": true,
        "bytes": text.len(),
        "preview": &text[..end],
    });
}

/// Replaces with `"[redacted]"` the value of every object key, at any depth
/// (arrays included), that contains one of `fields` once both are
/// normalized (ASCII-lowercased, with `_`, `-` and `.` removed). So `token`
/// redacts `access_token` and `Refresh-Token`, and `apiKey` redacts
/// `api_key` and `x-api-key`; it also redacts `tokenizer_version`, erring
/// on the side of hiding a value.
pub fn redact(value: &mut Value, fields: &[String]) {
    let fields: Vec<String> = fields
        .iter()
        .map(|f| normalize_key(f))
        .filter(|f| !f.is_empty())
        .collect();
    redact_normalized(value, &fields);
}

fn redact_normalized(value: &mut Value, fields: &[String]) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                let key = normalize_key(key);
                if fields.iter().any(|f| key.contains(f.as_str())) {
                    *v = Value::String(REDACTED.to_string());
                } else {
                    redact_normalized(v, fields);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                redact_normalized(v, fields);
            }
        }
        _ => {}
    }
}

/// `key` ASCII-lowercased without `_`, `-` and `.`.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| !matches!(c, '_' | '-' | '.'))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}
