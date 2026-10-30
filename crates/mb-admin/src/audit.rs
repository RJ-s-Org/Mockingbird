//! The audit log: recording one event per successful mutation, the
//! `/api/v1/workspaces/{ws}/audit` and `/api/v1/audit` endpoints (JSON or a
//! streamed JSONL export) and daily retention.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use mb_auth::{Permission, Principal};
use mb_store::{AuditEvent, AuditFilter, ControlStore, NewAuditEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::access::{CurrentPrincipal, WsAccess};
use crate::problem::{ApiResult, Json, Problem, Query};
use crate::webhook::WebhookSink;

/// Media type of the JSONL export.
pub const JSONL_CONTENT_TYPE: &str = "application/jsonl";

/// How often retention runs.
const RETENTION_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Writes audit events to the store and hands them to the webhook.
#[derive(Clone)]
pub struct Auditor {
    store: Arc<dyn ControlStore>,
    webhook: Option<WebhookSink>,
}

impl std::fmt::Debug for Auditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auditor")
            .field("webhook", &self.webhook.is_some())
            .finish_non_exhaustive()
    }
}

impl Auditor {
    /// Writes to `store` and, when given, hands each stored event to
    /// `webhook`.
    pub fn new(store: Arc<dyn ControlStore>, webhook: Option<WebhookSink>) -> Self {
        Self { store, webhook }
    }

    /// Appends `e`; on success queues it for the webhook. A failed write is
    /// logged and counted in `mb_audit_write_errors_total`; it never fails
    /// the request that caused it.
    pub async fn record(&self, e: NewAuditEvent) {
        match self.store.append_audit(e).await {
            Ok(stored) => {
                if let Some(webhook) = &self.webhook {
                    webhook.send(stored);
                }
            }
            Err(err) => {
                metrics::counter!("mb_audit_write_errors_total").increment(1);
                tracing::warn!(error = %err, "audit event could not be written");
            }
        }
    }

    /// Records `entry` as done by `principal`.
    pub(crate) async fn record_by(&self, principal: &Principal, entry: AuditEntry<'_>) {
        self.record(NewAuditEvent {
            workspace: entry.workspace,
            actor_subject: principal.subject.clone(),
            // The wire string of the principal's kind (`user`, `api_key`,
            // `local`).
            actor_kind: principal.kind.as_str().to_string(),
            action: entry.action.to_string(),
            target: entry.target,
            before: entry.before,
            after: entry.after,
            request_id: Some(entry.request_id),
            at: Utc::now(),
        })
        .await;
    }
}

/// What [`Auditor::record_by`] records: `action` on `target`, in
/// `workspace` (`None`: global), for request `request_id`.
pub(crate) struct AuditEntry<'a> {
    pub(crate) workspace: Option<String>,
    pub(crate) request_id: String,
    pub(crate) action: &'a str,
    pub(crate) target: String,
    pub(crate) before: Option<Value>,
    pub(crate) after: Option<Value>,
}

/// `v` serialised for an event's `before`/`after`.
pub(crate) fn snapshot<T: Serialize>(v: &T) -> Option<Value> {
    serde_json::to_value(v).ok()
}

impl WsAccess {
    /// Records that the caller did `action` on `target` in this workspace.
    /// Call once, after the mutation succeeded.
    pub async fn audit(
        &self,
        auditor: &Auditor,
        action: &str,
        target: String,
        before: Option<Value>,
        after: Option<Value>,
    ) {
        auditor
            .record_by(
                &self.principal,
                AuditEntry {
                    workspace: Some(self.ws.clone()),
                    request_id: self.request_id.clone(),
                    action,
                    target,
                    before,
                    after,
                },
            )
            .await;
    }
}

/// `now` minus `retention_days` days, or `None` when that is not a
/// representable time.
pub fn retention_cutoff(now: DateTime<Utc>, retention_days: u32) -> Option<DateTime<Utc>> {
    chrono::TimeDelta::try_days(i64::from(retention_days)).and_then(|d| now.checked_sub_signed(d))
}

/// Prunes events older than `retention_days` now, then every 24 hours.
///
/// # Panics
///
/// When called outside a Tokio runtime.
pub fn spawn_retention(
    store: Arc<dyn ControlStore>,
    retention_days: u32,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RETENTION_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(cutoff) = retention_cutoff(Utc::now(), retention_days) else {
                tracing::warn!(
                    retention_days,
                    "audit retention out of range; nothing pruned"
                );
                continue;
            };
            match store.prune_audit(cutoff).await {
                Ok(n) => tracing::debug!(pruned = n, retention_days, "audit retention ran"),
                Err(e) => tracing::warn!(error = %e, "audit retention failed"),
            }
        }
    })
}

/// One audit event.
#[derive(Serialize, ToSchema, Debug, Clone, PartialEq)]
pub struct AuditEventView {
    pub id: i64,
    /// `null` for events outside any workspace (e.g. logins).
    pub workspace: Option<String>,
    pub actor_subject: String,
    /// `user`, `api_key` or `local`.
    pub actor_kind: String,
    /// e.g. `mock.create`.
    pub action: String,
    /// e.g. `mock:orders`.
    pub target: String,
    /// The resource's API view before the change.
    #[schema(value_type = Option<Object>)]
    pub before: Option<Value>,
    /// The resource's API view after the change.
    #[schema(value_type = Option<Object>)]
    pub after: Option<Value>,
    pub request_id: Option<String>,
    pub at: DateTime<Utc>,
}

impl From<AuditEvent> for AuditEventView {
    fn from(e: AuditEvent) -> Self {
        Self {
            id: e.id,
            workspace: e.workspace,
            actor_subject: e.actor_subject,
            actor_kind: e.actor_kind,
            action: e.action,
            target: e.target,
            before: e.before,
            after: e.after,
            request_id: e.request_id,
            at: e.at,
        }
    }
}

/// Response format of an audit listing.
#[derive(Deserialize, ToSchema, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditFormat {
    /// A JSON array of at most `limit` events.
    #[default]
    Json,
    /// Every matching event, one JSON object per line (`limit` is ignored).
    Jsonl,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct AuditQuery {
    /// Only this action, e.g. `mock.create`.
    pub action: Option<String>,
    /// Only events by this subject.
    pub actor: Option<String>,
    /// 1–1000; default 100. Ignored by `format=jsonl`.
    pub limit: Option<u32>,
    /// Only events with a smaller id (keyset pagination).
    pub before: Option<i64>,
    /// `json` (default) or `jsonl`.
    #[param(value_type = Option<AuditFormat>)]
    pub format: Option<AuditFormat>,
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/audit",
    tag = "audit",
    params(("ws" = String, Path, description = "Workspace name"), AuditQuery),
    responses(
        (status = 200, description = "Newest first; with `format=jsonl` every matching event as `application/jsonl`", body = Vec<AuditEventView>),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 403, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Response> {
    access.require(Permission::Read)?;
    respond(&st, Some(access.ws), q).await
}

#[utoipa::path(
    get,
    path = "/api/v1/audit",
    tag = "audit",
    params(AuditQuery),
    responses(
        (status = 200, description = "Every workspace's events (and those outside any workspace), newest first; with `format=jsonl` every matching event as `application/jsonl`", body = Vec<AuditEventView>),
        (status = 400, body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a sysadmin", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_global(
    State(st): State<AdminState>,
    CurrentPrincipal(principal): CurrentPrincipal,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Response> {
    if !principal.sysadmin {
        return Err(Problem::forbidden(
            "only a sysadmin can read the global audit log",
        ));
    }
    respond(&st, None, q).await
}

async fn respond(st: &AdminState, workspace: Option<String>, q: AuditQuery) -> ApiResult<Response> {
    let mut filter = AuditFilter {
        workspace,
        action: q.action,
        actor: q.actor,
        limit: q.limit.unwrap_or(AuditFilter::DEFAULT_LIMIT),
        before_id: q.before,
    };
    match q.format.unwrap_or_default() {
        AuditFormat::Json => {
            let events = st.store.list_audit(&filter).await?;
            let views: Vec<AuditEventView> = events.into_iter().map(Into::into).collect();
            Ok(Json(views).into_response())
        }
        AuditFormat::Jsonl => {
            filter.limit = AuditFilter::MAX_LIMIT;
            // The first page is read before answering, so a store failure
            // is still a problem response rather than a truncated stream.
            let first = st.store.list_audit(&filter).await?;
            let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
            tokio::spawn(stream_jsonl(st.store.clone(), filter, first, tx));
            Ok((
                StatusCode::OK,
                [(header::CONTENT_TYPE, JSONL_CONTENT_TYPE)],
                Body::from_stream(ReceiverStream::new(rx)),
            )
                .into_response())
        }
    }
}

/// Sends `page` and every later page of `filter` as JSONL chunks. A store
/// error ends the stream with an error, so the client sees a truncated body.
async fn stream_jsonl(
    store: Arc<dyn ControlStore>,
    mut filter: AuditFilter,
    mut page: Vec<AuditEvent>,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    let page_size = filter.effective_limit() as usize;
    loop {
        let full = page.len() >= page_size;
        let last_id = page.last().map(|e| e.id);
        let mut chunk = Vec::new();
        for e in page {
            if serde_json::to_writer(&mut chunk, &AuditEventView::from(e)).is_err() {
                continue;
            }
            chunk.push(b'\n');
        }
        if !chunk.is_empty() && tx.send(Ok(Bytes::from(chunk))).await.is_err() {
            return; // The client went away.
        }
        let (true, Some(last_id)) = (full, last_id) else {
            return;
        };
        filter.before_id = Some(last_id);
        page = match store.list_audit(&filter).await {
            Ok(page) => page,
            Err(e) => {
                tracing::warn!(error = %e, "audit export failed mid-stream");
                let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                return;
            }
        };
    }
}
