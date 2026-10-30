//! Delivers audit events to an HTTP webhook, signed with HMAC-SHA256.
//!
//! Events are queued (capacity [`QUEUE_CAPACITY`]) and posted by one
//! background task, so delivery never blocks a request. When the queue is
//! full the event is dropped and `mb_audit_webhook_dropped_total` is
//! incremented. First attempts are made one at a time in queue order. A
//! failed delivery (transport error or non-2xx answer) is retried up to
//! [`MAX_RETRIES`] times with exponential backoff in a task of its own, so
//! a failing event does not hold up the ones behind it (a retried event can
//! therefore arrive after later ones); at most [`MAX_RETRYING`] events are
//! retried at once, and further first attempts wait while that many are.
//! After the last failure `mb_audit_webhook_failed_total` is incremented.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use mb_store::AuditEvent;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Semaphore, mpsc};

use crate::audit::AuditEventView;

/// Events waiting for delivery.
pub const QUEUE_CAPACITY: usize = 1024;
/// Retries after the first failed attempt.
pub const MAX_RETRIES: u32 = 3;
/// Backoff before the first retry; it doubles for each further retry.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Events whose retries may be pending at once.
pub const MAX_RETRYING: usize = 64;
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
    ///
    /// # Errors
    ///
    /// An [`mb_llm::LlmError`] for an invalid CA bundle or proxy in `opts`,
    /// or when the client cannot be built.
    pub fn client(opts: &mb_llm::HttpOptions) -> Result<reqwest::Client, mb_llm::LlmError> {
        mb_llm::http_client(opts, None)
    }

    /// Starts delivering to `url` with the default 1 s, 2 s, 4 s backoff.
    ///
    /// # Panics
    ///
    /// When called outside a Tokio runtime.
    pub fn spawn(url: String, secret: Option<String>, http: reqwest::Client) -> Self {
        Self::spawn_with(url, secret, http, BACKOFF_BASE)
    }

    /// Like [`WebhookSink::spawn`], with the backoff before the first retry
    /// set to `backoff_base` (doubling for each further retry).
    ///
    /// # Panics
    ///
    /// When called outside a Tokio runtime.
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
            let delivery = Arc::new(delivery);
            let retrying = Arc::new(Semaphore::new(MAX_RETRYING));
            while let Some(event) = rx.recv().await {
                let Some(payload) = delivery.payload(event) else {
                    continue;
                };
                if delivery.attempt(&payload, 0).await {
                    continue;
                }
                // The semaphore is never closed.
                let Ok(permit) = retrying.clone().acquire_owned().await else {
                    continue;
                };
                let delivery = delivery.clone();
                tokio::spawn(async move {
                    delivery.retry(&payload).await;
                    drop(permit);
                });
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
    mb_protocols::webhooks::sign::mockingbird_signature(secret, body)
}

struct Delivery {
    url: String,
    secret: Option<String>,
    http: reqwest::Client,
    backoff_base: Duration,
}

/// One event's signed request body.
struct Payload {
    id: i64,
    body: Bytes,
    signature: Option<String>,
}

impl Delivery {
    /// `event` serialised and signed; `None` (counted as failed) when it
    /// cannot be serialised.
    fn payload(&self, event: AuditEvent) -> Option<Payload> {
        let id = event.id;
        let body = match serde_json::to_vec(&AuditEventView::from(event)) {
            Ok(body) => Bytes::from(body),
            Err(e) => {
                metrics::counter!("mb_audit_webhook_failed_total").increment(1);
                tracing::warn!(id, error = %e, "audit event could not be serialised for the webhook");
                return None;
            }
        };
        let signature = self.secret.as_ref().map(|s| signature(s.as_bytes(), &body));
        Some(Payload {
            id,
            body,
            signature,
        })
    }

    /// Posts `p` once; whether the webhook accepted it.
    async fn attempt(&self, p: &Payload, attempt: u32) -> bool {
        let id = p.id;
        let mut req = self
            .http
            .post(&self.url)
            .timeout(ATTEMPT_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(p.body.clone());
        if let Some(sig) = &p.signature {
            req = req.header(SIGNATURE_HEADER, sig);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => true,
            Ok(resp) => {
                tracing::debug!(id, attempt, status = %resp.status(), "audit webhook rejected the event");
                false
            }
            Err(e) => {
                tracing::debug!(id, attempt, error = %e, "audit webhook delivery failed");
                false
            }
        }
    }

    /// Retries `p` after a failed first attempt, with exponential backoff.
    async fn retry(&self, p: &Payload) {
        let id = p.id;
        let mut backoff = self.backoff_base;
        for attempt in 1..=MAX_RETRIES {
            tokio::time::sleep(backoff).await;
            backoff = backoff.saturating_mul(2);
            if self.attempt(p, attempt).await {
                return;
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
