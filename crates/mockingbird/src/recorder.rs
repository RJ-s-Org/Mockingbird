//! The request log writer: a [`RequestRecorder`] that redacts each record
//! and appends it to the control store off the request path, and fans it
//! out to live subscribers (the console's request tail).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use chrono::Utc;
use mb_protocols::{RequestRecord, RequestRecorder};
use mb_store::{ControlChange, ControlStore, DEFAULT_WORKSPACE, NewRequestLogEntry};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

/// Records waiting to be written; beyond this, new records are dropped.
pub const QUEUE_CAPACITY: usize = 4096;

/// Capacity of the [`StoreRecorder::live`] broadcast channel.
pub const LIVE_CAPACITY: usize = 1024;

/// Replacement for the value of a redacted field.
pub const REDACTED: &str = "[redacted]";

/// Largest recorded request or response body, as serialized JSON; a larger
/// one is replaced by [`truncate_body`].
pub const MAX_RECORDED_BODY_BYTES: usize = 64 * 1024;

/// Bytes of a truncated body kept as its preview.
pub const PREVIEW_BYTES: usize = 1024;

/// Writes [`RequestRecord`]s to the request log through a bounded queue
/// drained by one background task, so [`RequestRecorder::record`] never
/// waits on the store. Records are redacted and their bodies truncated
/// before they are queued.
pub struct StoreRecorder {
    queue: mpsc::Sender<RequestRecord>,
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
        tokio::spawn(writer.run(rx));
        Arc::new(Self {
            queue,
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

    /// Redacted records as they are written.
    pub fn live(&self) -> broadcast::Receiver<RequestRecord> {
        self.live.subscribe()
    }

    /// Records dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl RequestRecorder for StoreRecorder {
    /// Redacts and truncates the record, then queues it for the writer.
    fn record(&self, mut r: RequestRecord) {
        {
            let settings = self.settings.load();
            redact(&mut r.request, &settings.redact_fields);
            if let Some(response) = &mut r.response {
                redact(response, &settings.redact_fields);
            }
        }
        if let Some(body) = r.request.get_mut("body") {
            truncate_body(body);
        }
        if let Some(response) = &mut r.response {
            truncate_body(response);
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
    async fn run(mut self, mut rx: mpsc::Receiver<RequestRecord>) {
        let mut changes_open = true;
        loop {
            tokio::select! {
                biased;
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

    /// Writes a record that [`StoreRecorder::record`] already redacted and
    /// truncated.
    async fn write(&mut self, r: RequestRecord) {
        let settings = self.settings.load_full();
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
/// (arrays included), that equals one of `fields` ignoring ASCII case.
pub fn redact(value: &mut Value, fields: &[String]) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if fields.iter().any(|f| f.eq_ignore_ascii_case(key)) {
                    *v = Value::String(REDACTED.to_string());
                } else {
                    redact(v, fields);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                redact(v, fields);
            }
        }
        _ => {}
    }
}
