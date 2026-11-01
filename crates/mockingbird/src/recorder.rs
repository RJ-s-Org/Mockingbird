//! The request log writer: a [`RequestRecorder`] that queues each record
//! and, off the request path, redacts it, appends it to the control store
//! and fans it out to live subscribers (the console's request tail).
//!
//! With HA ([`StoreRecorder::with_bus`]), every written entry is announced
//! on the bus by id, and entries other nodes announce are read back from
//! the shared store and fanned out too, so a tail on any node sees every
//! node's traffic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::Utc;
use mb_protocols::{RequestKind, RequestRecord, RequestRecorder};
use mb_state::{Bus, BusEvent};
use mb_store::{ControlChange, ControlStore, NewRequestLogEntry, RequestLogEntry};
use serde_json::Value;
use tokio::sync::{Notify, broadcast, mpsc};
use uuid::Uuid;

use crate::ha::Ha;

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
    store: Arc<dyn ControlStore>,
    /// Ids of written entries to announce on the bus; set by
    /// [`StoreRecorder::with_bus`].
    announce: Arc<OnceLock<mpsc::Sender<i64>>>,
    /// The bus announcer and receiver started by [`StoreRecorder::with_bus`].
    bus_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for StoreRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreRecorder")
            .field(
                "dropped",
                &self.dropped.load(std::sync::atomic::Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl StoreRecorder {
    /// Starts the writer task; it must be called inside a Tokio runtime.
    /// The task stops once the recorder is dropped and the queue drained.
    ///
    /// `retention` is the number of entries kept per mock; `redact_fields`
    /// are JSON keys whose values are replaced with `"[redacted]"`.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    pub fn new(
        store: Arc<dyn ControlStore>,
        retention: u32,
        redact_fields: Vec<String>,
    ) -> Arc<Self> {
        let (queue, rx) = mpsc::channel(QUEUE_CAPACITY);
        let (live, _) = broadcast::channel(LIVE_CAPACITY);
        let settings = Arc::new(ArcSwap::from_pointee(Settings::new(
            retention,
            redact_fields,
        )));
        let announce = Arc::new(OnceLock::new());
        let writer = Writer {
            changes: store.subscribe(),
            store: store.clone(),
            ids: HashMap::new(),
            settings: Arc::clone(&settings),
            live: live.clone(),
            announce: Arc::clone(&announce),
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
            store,
            announce,
            bus_tasks: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Shares the request log across nodes over `ha`'s bus: every entry
    /// this recorder writes is announced as [`BusEvent::RequestLogged`]
    /// (not on a single node's local bus), and entries other nodes
    /// announce are read from the store and sent to [`StoreRecorder::live`]
    /// subscribers. Only the first call has an effect.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    pub fn with_bus(self: Arc<Self>, ha: Arc<Ha>) {
        let bus = Arc::clone(&ha.bus);
        if bus.is_local() {
            return;
        }
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        if self.announce.set(tx).is_err() {
            tracing::warn!("request log already shared over a bus");
            return;
        }
        let remote = Remote {
            store: Arc::clone(&self.store),
            live: self.live.clone(),
            mocks: HashMap::new(),
            listed: None,
        };
        let tasks = vec![
            tokio::spawn(announce(rx, Arc::clone(&bus))),
            tokio::spawn(remote.run(bus.subscribe())),
        ];
        if let Ok(mut slot) = self.bus_tasks.lock() {
            slot.extend(tasks);
        }
    }

    /// Replaces the retention and the redacted fields; records written from
    /// now on use the new values.
    pub fn set_settings(&self, retention: u32, redact_fields: Vec<String>) {
        self.settings
            .store(Arc::new(Settings::new(retention, redact_fields)));
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
    /// The redaction the writer applies to a record's bodies, with the same
    /// live settings.
    fn redact(&self, value: &mut Value) {
        let settings = self.settings.load();
        redact_kept(value, &settings.redact_fields, &mut String::new());
    }

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

impl Drop for StoreRecorder {
    fn drop(&mut self) {
        if let Ok(tasks) = self.bus_tasks.lock() {
            for task in tasks.iter() {
                task.abort();
            }
        }
    }
}

/// Announces the ids of written entries on the bus, off the writer's path
/// (a slow bus never holds up the request log).
async fn announce(mut ids: mpsc::Receiver<i64>, bus: Arc<dyn Bus>) {
    while let Some(id) = ids.recv().await {
        if let Err(error) = bus.publish(BusEvent::RequestLogged { id }).await {
            tracing::warn!(%error, id, "cannot announce a request log entry on the bus");
        }
    }
}

/// Fans entries written by other nodes out to the live subscribers.
struct Remote {
    store: Arc<dyn ControlStore>,
    live: broadcast::Sender<RequestRecord>,
    /// mock id → (workspace, name), filled from `list_all_mocks` on a miss.
    mocks: HashMap<Uuid, (String, String)>,
    /// When `mocks` was last listed: misses list again at most once per
    /// [`RELIST_INTERVAL`].
    listed: Option<Instant>,
}

/// How often an entry of an unknown mock (deleted, or created since the
/// last listing) may make [`Remote`] list the mocks again.
const RELIST_INTERVAL: Duration = Duration::from_secs(1);

impl Remote {
    async fn run(mut self, mut events: broadcast::Receiver<BusEvent>) {
        loop {
            match events.recv().await {
                Ok(BusEvent::RequestLogged { id }) => self.relay(id).await,
                Ok(_) => {}
                // The live tail is best effort: skipped entries are still
                // in the request log.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(skipped = n, "remote request log announcements lagged");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    async fn relay(&mut self, id: i64) {
        // Nobody tailing: skip the reads.
        if self.live.receiver_count() == 0 {
            return;
        }
        let entry = match self.store.get_request_log(id).await {
            Ok(Some(entry)) => entry,
            // Pruned already, or not visible yet.
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, id, "cannot read an announced request log entry");
                return;
            }
        };
        let Some((workspace, mock)) = self.mock(entry.mock_id).await else {
            return;
        };
        let _ = self.live.send(from_entry(&entry, &workspace, &mock));
    }

    /// `(workspace, name)` of mock `id`. An unknown id lists the mocks
    /// again, at most once per [`RELIST_INTERVAL`] (entries of a deleted
    /// mock would otherwise list them once each).
    async fn mock(&mut self, id: Uuid) -> Option<(String, String)> {
        let stale = self.listed.is_none_or(|at| at.elapsed() >= RELIST_INTERVAL);
        if !self.mocks.contains_key(&id) && stale {
            self.listed = Some(Instant::now());
            match self.store.list_all_mocks().await {
                Ok(mocks) => {
                    self.mocks = mocks
                        .into_iter()
                        .map(|m| (m.id, (m.workspace, m.name)))
                        .collect();
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot list mocks for the request log");
                    return None;
                }
            }
        }
        self.mocks.get(&id).cloned()
    }
}

/// The live-feed record of a stored request-log entry of mock `mock` in
/// `workspace` (already redacted and truncated when it was written).
pub fn from_entry(e: &RequestLogEntry, workspace: &str, mock: &str) -> RequestRecord {
    RequestRecord {
        workspace: workspace.to_owned(),
        mock: mock.to_owned(),
        operation_id: e.operation_id.clone(),
        session: e.session.clone(),
        method: e.method.clone(),
        path: e.path.clone(),
        status: e.status,
        source: e.source.clone(),
        fallback_reason: e.fallback_reason.clone(),
        latency: Duration::from_millis(e.latency_ms),
        prompt_tokens: e.prompt_tokens,
        completion_tokens: e.completion_tokens,
        request: e.request.clone(),
        request_body_bytes: None,
        response: e.response.clone(),
        kind: match e.kind {
            mb_store::RequestKind::Http => RequestKind::Http,
            mb_store::RequestKind::Graphql => RequestKind::Graphql,
            mb_store::RequestKind::Grpc => RequestKind::Grpc,
            mb_store::RequestKind::Soap => RequestKind::Soap,
            mb_store::RequestKind::Event => RequestKind::Event,
        },
    }
}

/// Request-log settings that a config reload may change.
struct Settings {
    retention: u32,
    /// The redacted fields, normalized (see [`normalize_fields`]).
    redact_fields: Vec<String>,
}

impl Settings {
    fn new(retention: u32, redact_fields: Vec<String>) -> Self {
        Self {
            retention,
            redact_fields: normalize_fields(&redact_fields),
        }
    }
}

/// The background half of a [`StoreRecorder`].
struct Writer {
    store: Arc<dyn ControlStore>,
    changes: broadcast::Receiver<ControlChange>,
    /// `(workspace, mock name)` → id. An entry is evicted when the mock
    /// changes, so a mock deleted and recreated under the same name is
    /// looked up afresh.
    ids: HashMap<(String, String), Uuid>,
    settings: Arc<ArcSwap<Settings>>,
    live: broadcast::Sender<RequestRecord>,
    announce: Arc<OnceLock<mpsc::Sender<i64>>>,
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
                    Ok(ControlChange::Mock { workspace, name }) => {
                        self.ids.remove(&(workspace, name));
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
        let mut scratch = String::new();
        redact_normalized(&mut r.request, &settings.redact_fields, &mut scratch);
        if let Some(response) = &mut r.response {
            redact_normalized(response, &settings.redact_fields, &mut scratch);
        }
        if let Some(body) = r.request.get_mut("body") {
            truncate_body(body);
        }
        if let Some(response) = &mut r.response {
            truncate_body(response);
        }
        send_live(&self.live, &r);

        let Some(mock_id) = self.mock_id(&r.workspace, &r.mock).await else {
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
            fallback_reason: r.fallback_reason,
            latency_ms: u64::try_from(r.latency.as_millis()).unwrap_or(u64::MAX),
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            request: r.request,
            response: r.response,
            at: Utc::now(),
            kind: mb_store::RequestKind::parse(r.kind.as_str()).unwrap_or_default(),
        };
        match self
            .store
            .append_request_log(entry, settings.retention)
            .await
        {
            Ok(id) => {
                // A full queue drops the announcement: the entry is stored.
                if let Some(announce) = self.announce.get() {
                    let _ = announce.try_send(id);
                }
            }
            Err(error) => {
                // The cached id may belong to a mock deleted since.
                self.ids.remove(&(r.workspace.clone(), r.mock.clone()));
                tracing::warn!(workspace = %r.workspace, mock = %r.mock, %error, "cannot write request log entry");
                metrics::counter!("mb_request_log_errors_total", "mock" => r.mock).increment(1);
            }
        }
    }

    async fn mock_id(&mut self, workspace: &str, name: &str) -> Option<Uuid> {
        let key = (workspace.to_string(), name.to_string());
        if let Some(id) = self.ids.get(&key) {
            return Some(*id);
        }
        match self.store.get_mock(workspace, name).await {
            Ok(Some(mock)) => {
                self.ids.insert(key, mock.id);
                Some(mock.id)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%workspace, mock = %name, %error, "cannot resolve mock for request log");
                None
            }
        }
    }
}

/// Sends `r` to the live tail; nothing (not even a clone) when nobody
/// tails. A subscriber joining meanwhile misses this record, which a
/// best-effort tail allows.
fn send_live(live: &broadcast::Sender<RequestRecord>, r: &RequestRecord) {
    if live.receiver_count() > 0 {
        // Every receiver gone since is not an error either.
        let _ = live.send(r.clone());
    }
}

/// Replaces a body whose serialized JSON exceeds
/// [`MAX_RECORDED_BODY_BYTES`] with `{"truncated": true, "bytes": N,
/// "preview": "<first PREVIEW_BYTES of the JSON>"}`. Run it after
/// [`redact`], so the preview holds no redacted values.
pub fn truncate_body(body: &mut Value) {
    // One pass that counts the serialized bytes and keeps only the first
    // PREVIEW_BYTES: the whole JSON text is never held in memory.
    let mut out = Preview::default();
    if serde_json::to_writer(&mut out, body).is_err() || out.len <= MAX_RECORDED_BODY_BYTES {
        return;
    }
    // The cut may split a character; keep the valid prefix.
    let preview = match std::str::from_utf8(&out.head) {
        Ok(text) => text,
        Err(e) => std::str::from_utf8(&out.head[..e.valid_up_to()]).unwrap_or_default(),
    };
    *body = serde_json::json!({
        "truncated": true,
        "bytes": out.len,
        "preview": preview,
    });
}

/// A writer that counts every byte and keeps the first [`PREVIEW_BYTES`].
#[derive(Default)]
struct Preview {
    len: usize,
    head: Vec<u8>,
}

impl std::io::Write for Preview {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let room = PREVIEW_BYTES.saturating_sub(self.head.len());
        self.head.extend_from_slice(&buf[..room.min(buf.len())]);
        self.len += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Replaces with `"[redacted]"` the value of every object key, at any depth
/// (arrays included), that contains one of `fields` once both are
/// normalized (ASCII-lowercased, with `_`, `-` and `.` removed). So `token`
/// redacts `access_token` and `Refresh-Token`, and `apiKey` redacts
/// `api_key` and `x-api-key`; it also redacts `tokenizer_version`, erring
/// on the side of hiding a value.
pub fn redact(value: &mut Value, fields: &[String]) {
    redact_normalized(value, &normalize_fields(fields), &mut String::new());
}

/// `fields` normalized (see [`normalize_key_into`]), empty ones dropped.
fn normalize_fields(fields: &[String]) -> Vec<String> {
    fields
        .iter()
        .map(|f| {
            let mut key = String::new();
            normalize_key_into(f, &mut key);
            key
        })
        .filter(|f| !f.is_empty())
        .collect()
}

/// [`redact`] with `fields` already normalized; `scratch` holds each
/// normalized key in turn, so no key allocates.
fn redact_normalized(value: &mut Value, fields: &[String], scratch: &mut String) {
    if fields.is_empty() {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                normalize_key_into(key, scratch);
                if fields.iter().any(|f| scratch.contains(f.as_str())) {
                    *v = Value::String(REDACTED.to_string());
                } else {
                    redact_normalized(v, fields, scratch);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                redact_normalized(v, fields, scratch);
            }
        }
        _ => {}
    }
}

/// [`redact_normalized`] for what is kept and shown again rather than logged,
/// where hiding a count costs an answer: a number under a plural `...tokens` key
/// (`max_tokens`) stays.
fn redact_kept(value: &mut Value, fields: &[String], scratch: &mut String) {
    if fields.is_empty() {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                normalize_key_into(key, scratch);
                if fields.iter().any(|f| scratch.contains(f.as_str())) {
                    if !(v.is_number() && scratch.ends_with("tokens")) {
                        *v = Value::String(REDACTED.to_string());
                    }
                } else {
                    redact_kept(v, fields, scratch);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                redact_kept(v, fields, scratch);
            }
        }
        _ => {}
    }
}

/// Replaces `out` with `key` ASCII-lowercased without `_`, `-` and `.`.
fn normalize_key_into(key: &str, out: &mut String) {
    out.clear();
    out.extend(
        key.chars()
            .filter(|c| !matches!(c, '_' | '-' | '.'))
            .map(|c| c.to_ascii_lowercase()),
    );
}

#[cfg(test)]
mod tests {
    use mb_store::{NewMock, ResourceSource, SqliteControlStore};
    use serde_json::json;

    use super::*;

    /// What is kept beyond the log is redacted as the log is, with its live
    /// settings.
    #[tokio::test]
    async fn redact_uses_the_live_settings() {
        let store: Arc<dyn ControlStore> = Arc::new(
            SqliteControlStore::connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        let recorder = StoreRecorder::new(store, 10, vec!["token".to_string()]);
        let mut body = json!({"access_token": "a", "nested": {"note": "b"}});
        recorder.redact(&mut body);
        assert_eq!(
            body,
            json!({"access_token": REDACTED, "nested": {"note": "b"}})
        );

        let mut count = json!({"max_tokens": 100, "tokens": "a"});
        recorder.redact(&mut count);
        assert_eq!(count, json!({"max_tokens": 100, "tokens": REDACTED}));

        recorder.set_settings(10, vec!["note".to_string()]);
        let mut body = json!({"access_token": "a", "nested": {"note": "b"}});
        recorder.redact(&mut body);
        assert_eq!(
            body,
            json!({"access_token": "a", "nested": {"note": REDACTED}})
        );
    }

    /// A JSON body of `items` objects (about 120 bytes each), some keys
    /// redacted.
    fn body(items: usize) -> Value {
        let items: Vec<Value> = (0..items)
            .map(|i| {
                json!({
                    "id": i,
                    "user_name": format!("user-{i}"),
                    "access_token": "secret-secret",
                    "address": {"street": "Main St", "Zip-Code": "12345"},
                })
            })
            .collect();
        json!({"data": items})
    }

    fn record(request: Value, response: Value) -> RequestRecord {
        RequestRecord {
            workspace: "default".into(),
            mock: "m".into(),
            operation_id: Some("op".into()),
            session: "s".into(),
            method: "POST".into(),
            path: "/x".into(),
            status: 200,
            source: Some("faker".into()),
            fallback_reason: None,
            latency: Duration::from_millis(3),
            prompt_tokens: 0,
            completion_tokens: 0,
            request: json!({"body": request}),
            request_body_bytes: None,
            response: Some(response),
            kind: RequestKind::Http,
        }
    }

    #[test]
    fn truncated_preview_matches_the_whole_text_cut_at_a_char_boundary() {
        // `"`, 1022 `a`, then two-byte characters: byte 1024 splits one.
        let text = format!("{}{}", "a".repeat(1022), "é".repeat(40_000));
        let mut body = Value::String(text.clone());
        let whole = serde_json::to_string(&body).unwrap();
        truncate_body(&mut body);
        assert_eq!(body["truncated"], true);
        assert_eq!(body["bytes"], whole.len());
        assert_eq!(body["preview"], whole[..1023]);

        let mut small = json!({"a": "é".repeat(100)});
        let before = small.clone();
        truncate_body(&mut small);
        assert_eq!(small, before);
    }

    #[test]
    fn redaction_normalizes_keys_and_fields() {
        let mut v = json!({
            "Access-Token": "x",
            "nested": [{"x.api_key": "y", "keep": {"Password": "z"}}],
            "user": "u",
        });
        redact(
            &mut v,
            &[
                "token".into(),
                "API-Key".into(),
                "pass_word".into(),
                "_".into(),
            ],
        );
        assert_eq!(
            v,
            json!({
                "Access-Token": REDACTED,
                "nested": [{"x.api_key": REDACTED, "keep": {"Password": REDACTED}}],
                "user": "u",
            })
        );
    }

    #[tokio::test]
    async fn unknown_mock_ids_relist_at_most_once_per_interval() {
        let store = sqlite_with_mocks(1).await;
        let (live, _rx) = broadcast::channel(LIVE_CAPACITY);
        let mut remote = Remote {
            store: Arc::clone(&store),
            live,
            mocks: HashMap::new(),
            listed: None,
        };
        let known = store.list_all_mocks().await.unwrap()[0].id;
        assert_eq!(
            remote.mock(known).await,
            Some(("default".into(), "m0".into()))
        );
        let first = remote.listed;
        assert!(remote.mock(Uuid::new_v4()).await.is_none());
        assert_eq!(remote.listed, first, "no relisting within the interval");
        remote.listed = first.map(|at| at - RELIST_INTERVAL);
        assert!(remote.mock(Uuid::new_v4()).await.is_none());
        assert_ne!(remote.listed, first.map(|at| at - RELIST_INTERVAL));
    }

    /// MOCKINGBIRD-PERF-4 benchmark: redacting and truncating a response
    /// body of about `mb` MB, as the writer does for every record.
    #[test]
    #[ignore = "benchmark"]
    fn bench_redact_and_truncate() {
        let fields: Vec<String> = ["token", "password", "apiKey", "secret"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        for mb in [1usize, 10] {
            let original = body(mb * 8_700);
            let size = serde_json::to_vec(&original).map(|v| v.len()).unwrap_or(0);
            let mut best = Duration::MAX;
            for _ in 0..3 {
                let mut v = original.clone();
                let start = Instant::now();
                redact(&mut v, &fields);
                truncate_body(&mut v);
                best = best.min(start.elapsed());
                assert_eq!(v["truncated"], true);
            }
            println!("PERF-4 redact+truncate of a {size}-byte body: {best:?} (best of 3)");
        }
    }

    /// MOCKINGBIRD-PERF-3 benchmark: writing records with 64 KiB bodies
    /// to the live tail while nobody tails.
    #[test]
    #[ignore = "benchmark"]
    fn bench_live_send_without_subscribers() {
        let (live, rx) = broadcast::channel::<RequestRecord>(LIVE_CAPACITY);
        drop(rx);
        let r = record(body(500), body(500));
        let n = 2_000;
        let start = Instant::now();
        for _ in 0..n {
            send_live(&live, &r);
        }
        println!(
            "PERF-3 live send of a 2x60 KiB record, no subscriber: {:?} per record",
            start.elapsed() / n
        );
    }

    async fn sqlite_with_mocks(m: usize) -> Arc<dyn ControlStore> {
        let store = SqliteControlStore::connect("sqlite::memory:")
            .await
            .unwrap();
        for i in 0..m {
            store
                .create_mock(
                    mb_store::DEFAULT_WORKSPACE,
                    NewMock {
                        name: format!("m{i}"),
                        route_prefix: Some(format!("/m{i}")),
                        persona: None,
                        llm_profile: None,
                        header_overrides: true,
                        source: ResourceSource::Api,
                        chaos: None,
                        limits: Default::default(),
                        data_plane_auth: Default::default(),
                        hosts: vec![],
                        behavior: Default::default(),
                        webhooks: Vec::new(),
                    },
                )
                .await
                .unwrap();
        }
        Arc::new(store)
    }

    /// MOCKINGBIRD-PERF-14 benchmark: resolving `n` announced entries of a
    /// mock id the relay does not know, with 200 mocks stored.
    #[tokio::test]
    #[ignore = "benchmark"]
    async fn bench_relay_unknown_mock() {
        let store = sqlite_with_mocks(200).await;
        let (live, _rx) = broadcast::channel(LIVE_CAPACITY);
        let mut remote = Remote {
            store,
            live,
            mocks: HashMap::new(),
            listed: None,
        };
        let unknown = Uuid::new_v4();
        let n = 200u32;
        let start = Instant::now();
        for _ in 0..n {
            assert!(remote.mock(unknown).await.is_none());
        }
        println!(
            "PERF-14 {n} lookups of an unknown mock id (200 mocks): {:?}",
            start.elapsed()
        );
    }
}
