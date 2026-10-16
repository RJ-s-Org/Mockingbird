//! Delivers audit events to an HTTP webhook, signed with HMAC-SHA256.
//!
//! Events are queued (capacity [`QUEUE_CAPACITY`]) and posted by one
//! background task, so delivery never blocks a request. When the queue is
//! full the event is dropped and `mb_audit_webhook_dropped_total` is
//! incremented. A failed delivery (transport error or non-2xx answer) is
//! retried up to [`MAX_RETRIES`] times with exponential backoff; after the
//! last failure `mb_audit_webhook_failed_total` is incremented.

use std::time::Duration;

use hmac::{Hmac, Mac};
use mb_store::AuditEvent;
use sha2::Sha256;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::audit::AuditEventView;

/// Events waiting for delivery.
pub const QUEUE_CAPACITY: usize = 1024;
/// Retries after the first failed attempt.
pub const MAX_RETRIES: u32 = 3;
/// Backoff before the first retry; it doubles for each further retry.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Timeout of each delivery attempt.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
/// Header carrying `sha256=<hex HMAC-SHA256(secret, body)>`.
pub const SIGNATURE_HEADER: &str = "x-mockingbird-signature";

/// A handle to the webhook delivery task.
#[derive(Clone, Debug)]
pub struct WebhookSink {
    tx: mpsc::Sender<AuditEvent>,
}

impl WebhookSink {
    /// The HTTP client for [`WebhookSink::spawn`], with the outbound CA
    /// roots and proxy of `opts`.
    pub fn client(opts: &mb_llm::HttpOptions) -> Result<reqwest::Client, mb_llm::LlmError> {
        mb_llm::http_client(opts, None)
    }

    /// Starts delivering to `url` with the default 1 s, 2 s, 4 s backoff.
    /// Must be called inside a Tokio runtime.
    pub fn spawn(url: String, secret: Option<String>, http: reqwest::Client) -> Self {
        Self::spawn_with(url, secret, http, BACKOFF_BASE)
    }

    /// Like [`WebhookSink::spawn`], with the backoff before the first retry
    /// set to `backoff_base` (doubling for each further retry).
    pub fn spawn_with(
        url: String,
        secret: Option<String>,
        http: reqwest::Client,
        backoff_base: Duration,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<AuditEvent>(QUEUE_CAPACITY);
        let delivery = Delivery {
            url,
            secret,
            http,
            backoff_base,
        };
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                delivery.deliver(event).await;
            }
        });
        WebhookSink { tx }
    }

    /// Queues `e` for delivery without waiting; drops it when the queue is
    /// full (or the delivery task has stopped).
    pub fn send(&self, e: AuditEvent) {
        match self.tx.try_send(e) {
            Ok(()) => {}
            Err(TrySendError::Full(e)) => {
                metrics::counter!("mb_audit_webhook_dropped_total").increment(1);
                tracing::warn!(id = e.id, "audit webhook queue is full; event dropped");
            }
            Err(TrySendError::Closed(e)) => {
                metrics::counter!("mb_audit_webhook_dropped_total").increment(1);
                tracing::warn!(id = e.id, "audit webhook task has stopped; event dropped");
            }
        }
    }
}

/// `sha256=<lowercase hex HMAC-SHA256(secret, body)>`.
pub fn signature(secret: &[u8], body: &[u8]) -> String {
    use std::fmt::Write;
    // HMAC accepts keys of any length, so this never fails.
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret) {
        Ok(mac) => mac,
        Err(_) => return String::new(),
    };
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256=");
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

struct Delivery {
    url: String,
    secret: Option<String>,
    http: reqwest::Client,
    backoff_base: Duration,
}

impl Delivery {
    async fn deliver(&self, event: AuditEvent) {
        let id = event.id;
        let body = match serde_json::to_vec(&AuditEventView::from(event)) {
            Ok(body) => body,
            Err(e) => {
                metrics::counter!("mb_audit_webhook_failed_total").increment(1);
                tracing::warn!(id, error = %e, "audit event could not be serialised for the webhook");
                return;
            }
        };
        let sig = self.secret.as_ref().map(|s| signature(s.as_bytes(), &body));
        let mut backoff = self.backoff_base;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
            }
            let mut req = self
                .http
                .post(&self.url)
                .timeout(ATTEMPT_TIMEOUT)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone());
            if let Some(sig) = &sig {
                req = req.header(SIGNATURE_HEADER, sig);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => return,
                Ok(resp) => {
                    tracing::debug!(id, attempt, status = %resp.status(), "audit webhook rejected the event");
                }
                Err(e) => {
                    tracing::debug!(id, attempt, error = %e, "audit webhook delivery failed");
                }
            }
        }
        metrics::counter!("mb_audit_webhook_failed_total").increment(1);
        tracing::warn!(
            id,
            retries = MAX_RETRIES,
            "audit webhook delivery failed; giving up"
        );
    }
}
