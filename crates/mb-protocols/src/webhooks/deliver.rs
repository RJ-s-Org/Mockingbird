//! Sends webhook events: one spawned delivery per (event, target), retried
//! [`MAX_RETRIES`] times with exponential backoff, re-signed per attempt,
//! bounded to [`DispatchSettings::max_pending`] in flight per mock.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::log::{DeliveryRecord, DeliveryStatus, SharedDeliveryLog, redacted_url};
use super::sign::{Signing, signature_header};

/// Retries after the first failed attempt.
pub const MAX_RETRIES: u32 = 3;

const USER_AGENT: &str = concat!("Mockingbird-Webhooks/", env!("CARGO_PKG_VERSION"));

/// A resolved webhook secret; its `Debug` never shows the value.
#[derive(Clone)]
pub struct SecretBytes(Arc<[u8]>);

impl SecretBytes {
    pub fn new(value: String) -> Self {
        Self(Arc::from(value.into_bytes()))
    }

    fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A webhook target with its secret reference resolved. Its `Debug` shows
/// the URL without userinfo, query or fragment ([`redacted_url`]) and never
/// the secret.
#[derive(Clone)]
pub struct ResolvedTarget {
    pub url: String,
    /// Operation ids, event types or `"*"`.
    pub events: Vec<String>,
    pub signing: Signing,
    /// The secret, or why its reference could not be resolved (naming the
    /// reference, never a value). A target with an `Err` is never sent to.
    pub secret: Result<Option<SecretBytes>, String>,
    /// Wait before the first attempt.
    pub delay: Duration,
}

impl fmt::Debug for ResolvedTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedTarget")
            .field("url", &redacted_url(&self.url))
            .field("events", &self.events)
            .field("signing", &self.signing)
            .field("secret", &self.secret)
            .field("delay", &self.delay)
            .finish()
    }
}

/// Retry, concurrency and timeout settings of a [`WebhookDispatcher`].
#[derive(Clone, Debug)]
pub struct DispatchSettings {
    /// The wait before the first retry; doubled for each later one.
    pub backoff_base: Duration,
    /// Deliveries in flight per mock; beyond it new ones are dropped.
    pub max_pending: usize,
    pub attempt_timeout: Duration,
}

impl Default for DispatchSettings {
    fn default() -> Self {
        Self {
            backoff_base: Duration::from_secs(1),
            max_pending: 256,
            attempt_timeout: Duration::from_secs(10),
        }
    }
}

impl DispatchSettings {
    /// The wait before retry `retry` (1-based): `backoff_base * 2^(retry-1)`,
    /// saturating.
    pub fn retry_delay(&self, retry: u32) -> Duration {
        let factor = 1u32
            .checked_shl(retry.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.backoff_base.saturating_mul(factor)
    }
}

/// The wait before retry `retry` (1-based): `backoff_base * 2^(retry-1)`.
#[deprecated(since = "0.1.1", note = "use `DispatchSettings::retry_delay`")]
pub fn retry_delay(settings: &DispatchSettings, retry: u32) -> Duration {
    settings.retry_delay(retry)
}

/// One event to send: the serialised body and what the log records.
#[derive(Clone, Debug)]
pub struct OutboundEvent {
    pub event_id: String,
    pub event_type: String,
    pub operation: String,
    pub session: String,
    pub body: Vec<u8>,
}

/// Sends one mock's webhook events to its targets.
pub struct WebhookDispatcher {
    workspace: String,
    mock: String,
    targets: Vec<ResolvedTarget>,
    http: reqwest::Client,
    log: SharedDeliveryLog,
    settings: DispatchSettings,
    pending: Arc<Semaphore>,
}

impl fmt::Debug for WebhookDispatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookDispatcher")
            .field("workspace", &self.workspace)
            .field("mock", &self.mock)
            .field("targets", &self.targets)
            .finish_non_exhaustive()
    }
}

impl WebhookDispatcher {
    pub fn new(
        workspace: String,
        mock: String,
        targets: Vec<ResolvedTarget>,
        http: reqwest::Client,
        log: SharedDeliveryLog,
        settings: DispatchSettings,
    ) -> Self {
        let pending = Arc::new(Semaphore::new(settings.max_pending));
        Self {
            workspace,
            mock,
            targets,
            http,
            log,
            settings,
            pending,
        }
    }

    /// Targets whose `events` list `"*"`, `operation` or `event_type`.
    pub fn matching(&self, operation: &str, event_type: &str) -> Vec<usize> {
        self.targets
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.events
                    .iter()
                    .any(|e| e == "*" || e == operation || e == event_type)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The targets whose `events` name `operation` itself.
    pub fn naming(&self, operation: &str) -> Vec<usize> {
        self.targets
            .iter()
            .enumerate()
            .filter(|(_, t)| t.events.iter().any(|e| e == operation))
            .map(|(i, _)| i)
            .collect()
    }

    /// Records one `Pending` delivery per listed target and spawns it;
    /// never awaits the network.
    ///
    /// # Panics
    ///
    /// When called outside a Tokio runtime with at least one target to send
    /// to.
    pub fn send(self: &Arc<Self>, event: OutboundEvent, targets: &[usize]) {
        let event = Arc::new(event);
        for &index in targets {
            let Some(target) = self.targets.get(index) else {
                continue;
            };
            let mut record = DeliveryRecord {
                id: delivery_id(),
                event_id: event.event_id.clone(),
                event_type: event.event_type.clone(),
                operation: event.operation.clone(),
                session: event.session.clone(),
                url: redacted_url(&target.url),
                status: DeliveryStatus::Pending,
                attempts: 0,
                response_status: None,
                error: None,
                created_at: SystemTime::now(),
                finished_at: None,
            };
            let Ok(permit) = self.pending.clone().try_acquire_owned() else {
                record.status = DeliveryStatus::Dropped;
                record.error = Some(format!(
                    "more than {} deliveries pending",
                    self.settings.max_pending
                ));
                record.finished_at = Some(SystemTime::now());
                metrics::counter!("mb_webhook_dropped_total").increment(1);
                tracing::warn!(mock = %self.mock, event = %event.event_id, "webhook delivery dropped: too many pending");
                self.log.push(&self.workspace, &self.mock, record);
                continue;
            };
            let id = record.id.clone();
            self.log.push(&self.workspace, &self.mock, record);
            let this = self.clone();
            let event = event.clone();
            tokio::spawn(async move { this.deliver(index, id, event, permit).await });
        }
    }

    async fn deliver(
        &self,
        index: usize,
        id: String,
        event: Arc<OutboundEvent>,
        _permit: OwnedSemaphorePermit,
    ) {
        let Some(target) = self.targets.get(index) else {
            return;
        };
        if !target.delay.is_zero() {
            tokio::time::sleep(target.delay).await;
        }
        let secret = match &target.secret {
            Ok(secret) => secret.as_ref(),
            Err(reason) => {
                self.finish(&id, DeliveryStatus::Failed, Some(reason.clone()));
                return;
            }
        };
        if target.signing != Signing::None && secret.is_none() {
            self.finish(
                &id,
                DeliveryStatus::Failed,
                Some("signing needs a secret".into()),
            );
            return;
        }
        let mut last_error = None;
        for attempt in 1..=1 + MAX_RETRIES {
            if attempt > 1 {
                tokio::time::sleep(self.settings.retry_delay(attempt - 1)).await;
            }
            let outcome = self.attempt(target, secret, &event).await;
            let status = outcome.as_ref().ok().copied();
            self.log.update(&self.workspace, &self.mock, &id, |r| {
                r.attempts = attempt;
                r.response_status = status.or(r.response_status);
            });
            match outcome {
                Ok(code) if (200..300).contains(&code) => {
                    self.finish(&id, DeliveryStatus::Delivered, None);
                    return;
                }
                Ok(code) => last_error = Some(format!("receiver answered {code}")),
                Err(e) => last_error = Some(e),
            }
        }
        self.finish(&id, DeliveryStatus::Failed, last_error);
    }

    /// One signed POST: the response status, or a transport error without
    /// the URL.
    async fn attempt(
        &self,
        target: &ResolvedTarget,
        secret: Option<&SecretBytes>,
        event: &OutboundEvent,
    ) -> Result<u16, String> {
        let mut req = self
            .http
            .post(&target.url)
            .timeout(self.settings.attempt_timeout)
            .header("content-type", "application/json")
            .header("user-agent", USER_AGENT)
            .header("x-mockingbird-event-id", &event.event_id)
            .header("x-mockingbird-event-type", &event.event_type);
        if let Some(secret) = secret
            && let Some((name, value)) =
                signature_header(target.signing, secret.expose(), unix_now(), &event.body)
        {
            req = req.header(name, value);
        }
        match req.body(event.body.clone()).send().await {
            Ok(resp) => Ok(resp.status().as_u16()),
            Err(e) => Err(e.without_url().to_string()),
        }
    }

    fn finish(&self, id: &str, status: DeliveryStatus, error: Option<String>) {
        let counter = match status {
            DeliveryStatus::Delivered => "mb_webhook_delivered_total",
            _ => "mb_webhook_failed_total",
        };
        metrics::counter!(counter).increment(1);
        if status == DeliveryStatus::Failed {
            tracing::warn!(mock = %self.mock, delivery = %id, error = ?error, "webhook delivery failed");
        }
        self.log.update(&self.workspace, &self.mock, id, |r| {
            r.status = status;
            r.error = error;
            r.finished_at = Some(SystemTime::now());
        });
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `whd_` + 16 random alphanumerics.
fn delivery_id() -> String {
    let suffix: String = rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();
    format!("whd_{suffix}")
}
