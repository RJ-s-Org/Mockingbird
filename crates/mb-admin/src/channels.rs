//! `/api/v1/workspaces/{ws}/mocks/{mock}/channels…`,
//! `/api/v1/workspaces/{ws}/mocks/{mock}/event-settings` (with their P2
//! aliases under `/api/v1/mocks/…` for workspace `default`) and
//! `/api/v1/brokers`: inspect and configure a mock's event channels,
//! publish on demand, pick the mock's broker and list the brokers.
//!
//! Reading needs the viewer role in the workspace; changing settings and
//! publishing need editor. `channel.update`, `channel.publish` and
//! `event_settings.update` are audited. The broker list is global and
//! read-only; any authenticated caller may read it.
//!
//! Runtime actions (publishing, activity counters, the broker list) go
//! through a [`ChannelControl`] passed as an `axum::Extension<SharedChannelControl>`;
//! without one, publishing answers 503 and activity and brokers are empty.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Router};
use chrono::{DateTime, Utc};
use mb_auth::Permission;
use mb_mcm::{Bundle, Channel, Direction, Trigger};
use mb_store::{ChannelSettings, Mock, MockEventSettings, ResourceSource};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::{OpenApi, ToSchema};

use crate::access::{CurrentPrincipal, WsAccess};
use crate::audit::snapshot;
use crate::mocks::MockPath;
use crate::problem::{ApiResult, Json, Path, Problem};
use crate::{AdminState, ensure_api_managed};

/// Path of one channel.
#[derive(Deserialize, Debug)]
pub struct ChannelPath {
    pub mock: String,
    pub channel: String,
}

/// Most messages one manual publish may generate.
const MAX_PUBLISH_COUNT: u32 = 100;

/// A channel's runtime counters.
#[derive(Serialize, ToSchema, Clone, Debug, PartialEq)]
pub struct ChannelActivityView {
    pub published: u64,
    pub received: u64,
    pub last_published_at: Option<DateTime<Utc>>,
    pub last_received_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// A message produced by a manual publish.
#[derive(Serialize, ToSchema, Clone, Debug, PartialEq)]
pub struct PublishedMessage {
    /// The resolved address it was published to.
    pub address: String,
    pub key: Option<String>,
    pub headers: BTreeMap<String, String>,
    #[schema(value_type = Object)]
    pub payload: Value,
    /// `llm`, `patched`, `repaired` or `fallback`.
    pub source: String,
}

/// A configured broker connection.
#[derive(Serialize, ToSchema, Clone, Debug, PartialEq)]
pub struct BrokerView {
    pub name: String,
    /// `kafka`, `amqp`, `mqtt` or `memory`.
    pub kind: String,
    /// The connection URL, without credentials.
    pub url: String,
    pub status: String,
}

/// Why a [`ChannelControl`] action failed.
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelControlError {
    /// The mock has no running channel runtime (503).
    NotRunning(String),
    /// No such channel (404).
    Unknown(String),
    /// The action does not apply to the channel, e.g. publishing on a
    /// channel the mock consumes (409).
    Conflict(String),
    /// Generating or publishing failed (502).
    Failed(String),
    /// A publish failed with `error` after `published` messages had gone
    /// out (they stay on the broker); answered as `error` is.
    Partial {
        published: Vec<PublishedMessage>,
        error: Box<ChannelControlError>,
    },
}

impl ChannelControlError {
    /// The problem detail.
    fn detail(&self) -> &str {
        match self {
            Self::NotRunning(d) | Self::Unknown(d) | Self::Conflict(d) | Self::Failed(d) => d,
            Self::Partial { error, .. } => error.detail(),
        }
    }
}

/// The problem detail the Admin API answers with.
impl std::fmt::Display for ChannelControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.detail())
    }
}

/// [`Partial`](ChannelControlError::Partial)'s source is the error that
/// stopped the publish.
impl std::error::Error for ChannelControlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Partial { error, .. } => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// The event runtime, as seen by the Admin API.
#[async_trait]
pub trait ChannelControl: Send + Sync {
    /// Generates and publishes `count` messages on `channel` of mock
    /// `mock` in workspace `ws`.
    async fn publish(
        &self,
        ws: &str,
        mock: &str,
        channel: &str,
        count: usize,
        context: Option<Value>,
    ) -> Result<Vec<PublishedMessage>, ChannelControlError>;
    /// Channel id → counters for the running channels of mock `mock` in
    /// workspace `ws`.
    fn activity(&self, ws: &str, mock: &str) -> BTreeMap<String, ChannelActivityView>;
    /// The configured brokers.
    fn brokers(&self) -> Vec<BrokerView>;
}

/// How handlers receive the control: `axum::Extension<SharedChannelControl>`.
pub type SharedChannelControl = Arc<dyn ChannelControl>;

type Control = Option<Extension<SharedChannelControl>>;

/// A channel of the active contract with its effective settings.
#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct ChannelView {
    pub id: String,
    pub title: String,
    /// `publish` (the mock produces) or `subscribe` (the mock consumes).
    pub direction: String,
    /// `kafka`, `amqp`, `mqtt` or `websocket`.
    pub protocol: String,
    /// As in the contract, with `{param}` placeholders kept.
    pub address: String,
    pub message_names: Vec<String>,
    pub content_type: String,
    pub enabled: bool,
    /// The effective triggers, as MCM JSON (e.g. `"manual"`,
    /// `{"cron": {"expr": "*/5 * * * *"}}`, `{"schedule": {"every_ms": 1000}}`).
    #[schema(value_type = Vec<Object>)]
    pub triggers: Vec<Value>,
    /// Where `enabled` and `triggers` come from: `contract`, `file` or `api`.
    pub settings_source: String,
    /// The id of the channel replies are published on.
    pub reply: Option<String>,
    /// `null` without a running event runtime.
    pub activity: Option<ChannelActivityView>,
}

/// A channel with its schemas.
#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct ChannelDetail {
    #[serde(flatten)]
    pub channel: ChannelView,
    /// JSON Schema 2020-12 with the definitions it references as `$defs`.
    #[schema(value_type = Object)]
    pub message_schema: Value,
    #[schema(value_type = Option<Object>)]
    pub key_schema: Option<Value>,
    #[schema(value_type = Option<Object>)]
    pub headers_schema: Option<Value>,
    /// Address parameter name → schema.
    #[schema(value_type = Object)]
    pub parameters: Value,
}

/// A channel's settings. Absent `triggers` use the contract's.
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct PutChannel {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// MCM triggers; only channels the mock publishes may have any.
    #[serde(default)]
    #[schema(value_type = Option<Vec<Object>>)]
    pub triggers: Option<Vec<Value>>,
}

fn default_true() -> bool {
    true
}

/// A manual publish.
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    /// 1–100, default 1.
    #[serde(default)]
    pub count: Option<u32>,
    /// A JSON object handed to the message prompt.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub context: Option<Value>,
}

/// The published messages, in order.
#[derive(Serialize, ToSchema, Debug)]
pub struct PublishResult {
    pub messages: Vec<PublishedMessage>,
}

/// A mock's event settings.
#[derive(Serialize, ToSchema, Debug)]
pub struct EventSettingsView {
    /// `null` picks a broker automatically.
    pub broker: Option<String>,
    /// `file` or `api`.
    pub source: String,
}

/// A mock's event settings.
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct PutEventSettings {
    /// A configured broker's name; `null` picks automatically.
    pub broker: Option<String>,
}

#[derive(OpenApi)]
#[openapi(
    paths(list, get_one, put, publish, get_event_settings, put_event_settings, brokers),
    components(schemas(
        ChannelActivityView,
        PublishedMessage,
        BrokerView,
        ChannelView,
        ChannelDetail,
        PutChannel,
        PublishRequest,
        PublishResult,
        EventSettingsView,
        PutEventSettings,
    )),
    tags((name = "channels"), (name = "brokers"))
)]
#[derive(Debug)]
pub struct ChannelsDoc;

/// The channel and event-settings routes of a workspace (relative to
/// `/api/v1/workspaces/{ws}`, and to `/api/v1` for the `default` aliases).
/// [`brokers`] is mounted at `/api/v1/brokers` by the router.
pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/mocks/{mock}/channels", get(list))
        .route("/mocks/{mock}/channels/{channel}", get(get_one).put(put))
        .route("/mocks/{mock}/channels/{channel}/publish", post(publish))
        .route(
            "/mocks/{mock}/event-settings",
            get(get_event_settings).put(put_event_settings),
        )
}

fn source_name(source: &ResourceSource) -> &'static str {
    match source {
        ResourceSource::File => "file",
        ResourceSource::Api => "api",
    }
}

fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Publish => "publish",
        Direction::Subscribe => "subscribe",
    }
}

fn trigger_json(triggers: &[Trigger]) -> ApiResult<Vec<Value>> {
    triggers
        .iter()
        .map(|t| {
            serde_json::to_value(t).map_err(|e| {
                Problem::new(StatusCode::INTERNAL_SERVER_ERROR, format!("trigger: {e}"))
            })
        })
        .collect()
}

fn view(
    channel: &Channel,
    settings: Option<&ChannelSettings>,
    activity: Option<ChannelActivityView>,
) -> ApiResult<ChannelView> {
    let triggers = match settings.and_then(|s| s.triggers.as_ref()) {
        Some(t) => trigger_json(t)?,
        None => trigger_json(&channel.effective_triggers())?,
    };
    Ok(ChannelView {
        id: channel.id.clone(),
        title: channel.title.clone(),
        direction: direction_name(channel.direction).into(),
        protocol: channel.binding.protocol().into(),
        address: channel.address.clone(),
        message_names: channel.message_names.clone(),
        content_type: channel.content_type.clone(),
        enabled: settings.is_none_or(|s| s.enabled),
        triggers,
        settings_source: settings
            .map_or("contract", |s| source_name(&s.source))
            .into(),
        reply: channel.reply.as_ref().map(|r| r.channel.clone()),
        activity,
    })
}

/// The mock's active bundle, or `None`.
async fn active_bundle(st: &AdminState, m: &Mock) -> ApiResult<Option<Bundle>> {
    Ok(st.store.active_bundle(m.id).await?.map(|c| c.bundle))
}

/// The active bundle's channel `id`, or a 404.
async fn find_channel(st: &AdminState, m: &Mock, id: &str) -> ApiResult<Channel> {
    let missing = || {
        Problem::not_found(format!(
            "the active contract of mock {:?} has no channel {id:?}",
            m.name
        ))
    };
    let bundle = active_bundle(st, m).await?.ok_or_else(missing)?;
    bundle.channel(id).cloned().ok_or_else(missing)
}

async fn settings_row(st: &AdminState, m: &Mock, id: &str) -> ApiResult<Option<ChannelSettings>> {
    Ok(st
        .store
        .list_channel_settings(m.id)
        .await?
        .into_iter()
        .find(|s| s.channel_id == id))
}

fn activity_of(control: &Control, ws: &str, mock: &str, id: &str) -> Option<ChannelActivityView> {
    control
        .as_ref()
        .and_then(|Extension(c)| c.activity(ws, mock).remove(id))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/channels",
    tag = "channels",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    responses(
        (status = 200, description = "The active contract's channels, in contract order; empty without an active contract", body = Vec<ChannelView>),
        (status = 403, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
    control: Control,
    Path(MockPath { mock }): Path<MockPath>,
) -> ApiResult<Json<Vec<ChannelView>>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let Some(bundle) = active_bundle(&st, &m).await? else {
        return Ok(Json(Vec::new()));
    };
    let settings: BTreeMap<String, ChannelSettings> = st
        .store
        .list_channel_settings(m.id)
        .await?
        .into_iter()
        .map(|s| (s.channel_id.clone(), s))
        .collect();
    let mut activity = control
        .as_ref()
        .map(|Extension(c)| c.activity(&access.ws, &mock))
        .unwrap_or_default();
    let views = bundle
        .channels
        .iter()
        .map(|c| view(c, settings.get(&c.id), activity.remove(&c.id)))
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(views))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/channels/{channel}",
    tag = "channels",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("channel" = String, Path, description = "Channel id"),
    ),
    responses(
        (status = 200, body = ChannelDetail),
        (status = 403, body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such mock, no active contract, or no such channel", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_one(
    State(st): State<AdminState>,
    access: WsAccess,
    control: Control,
    Path(ChannelPath { mock, channel: id }): Path<ChannelPath>,
) -> ApiResult<Json<ChannelDetail>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let bundle = active_bundle(&st, &m).await?;
    let channel = bundle
        .as_ref()
        .and_then(|b| b.channel(&id))
        .ok_or_else(|| {
            Problem::not_found(format!(
                "the active contract of mock {mock:?} has no channel {id:?}"
            ))
        })?;
    let row = settings_row(&st, &m, &id).await?;
    let activity = activity_of(&control, &access.ws, &mock, &id);
    let base = view(channel, row.as_ref(), activity)?;
    let with_defs = |schema: &Value| {
        bundle
            .as_ref()
            .map_or_else(|| schema.clone(), |b| b.with_reachable_defs(schema))
    };
    Ok(Json(ChannelDetail {
        message_schema: with_defs(&channel.message_schema),
        key_schema: channel.key_schema.as_ref().map(with_defs),
        headers_schema: channel.headers_schema.as_ref().map(with_defs),
        parameters: Value::Object(
            channel
                .parameters
                .iter()
                .map(|(k, v)| (k.clone(), with_defs(v)))
                .collect::<Map<_, _>>(),
        ),
        channel: base,
    }))
}

#[utoipa::path(
    put,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/channels/{channel}",
    tag = "channels",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("channel" = String, Path, description = "Channel id"),
    ),
    request_body = PutChannel,
    responses(
        (status = 200, body = ChannelView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 415, body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid triggers", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn put(
    State(st): State<AdminState>,
    access: WsAccess,
    control: Control,
    Path(ChannelPath { mock, channel: id }): Path<ChannelPath>,
    Json(body): Json<PutChannel>,
) -> ApiResult<Json<ChannelView>> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    let channel = find_channel(&st, &m, &id).await?;
    let existing = settings_row(&st, &m, &id).await?;
    if let Some(existing) = &existing {
        ensure_api_managed(&existing.source, format_args!("channel {id:?}"))?;
    }
    let before = existing
        .as_ref()
        .map(|e| view(&channel, Some(e), None))
        .transpose()?;
    let triggers = match body.triggers {
        None => None,
        Some(values) => {
            let triggers = values
                .into_iter()
                .enumerate()
                .map(|(i, v)| {
                    serde_json::from_value::<Trigger>(v)
                        .map_err(|e| Problem::unprocessable(format!("triggers[{i}]: {e}")))
                })
                .collect::<ApiResult<Vec<_>>>()?;
            mb_import::check_triggers(&triggers, channel.direction)
                .map_err(|e| Problem::unprocessable(e.to_string()))?;
            Some(triggers)
        }
    };
    let row = ChannelSettings {
        mock_id: m.id,
        channel_id: id.clone(),
        enabled: body.enabled,
        triggers,
        source: ResourceSource::Api,
        updated_at: Utc::now(),
    };
    st.store.put_channel_settings(&row).await?;
    let after = view(&channel, Some(&row), None)?;
    access
        .audit(
            &st.audit,
            "channel.update",
            format!("channel:{mock}/{id}"),
            before.as_ref().and_then(snapshot),
            snapshot(&after),
        )
        .await;
    let activity = activity_of(&control, &access.ws, &mock, &id);
    Ok(Json(ChannelView { activity, ..after }))
}

fn control_problem(err: ChannelControlError) -> Problem {
    match err {
        ChannelControlError::Unknown(d) => Problem::not_found(d),
        ChannelControlError::Conflict(d) => Problem::conflict(d),
        ChannelControlError::NotRunning(d) => Problem::new(StatusCode::SERVICE_UNAVAILABLE, d),
        ChannelControlError::Failed(d) => {
            let title = if d.starts_with("generated message") {
                "Generated message too large"
            } else {
                "Broker publish failed"
            };
            Problem::titled(StatusCode::BAD_GATEWAY, title, d)
        }
        ChannelControlError::Partial { error, .. } => control_problem(*error),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/channels/{channel}/publish",
    tag = "channels",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("channel" = String, Path, description = "Channel id"),
    ),
    request_body = PublishRequest,
    responses(
        (status = 200, body = PublishResult),
        (status = 403, body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such mock or channel", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The channel is not one the mock publishes", body = Problem, content_type = "application/problem+json"),
        (status = 415, body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "count outside 1-100, or context not an object", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "Broker publish failed, or the generated message is too large", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "No event runtime, or the mock's channels are not running", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn publish(
    State(st): State<AdminState>,
    access: WsAccess,
    control: Control,
    Path(ChannelPath { mock, channel: id }): Path<ChannelPath>,
    Json(body): Json<PublishRequest>,
) -> ApiResult<Json<PublishResult>> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    let count = body.count.unwrap_or(1);
    if !(1..=MAX_PUBLISH_COUNT).contains(&count) {
        return Err(Problem::unprocessable(format!(
            "count must be between 1 and {MAX_PUBLISH_COUNT}"
        )));
    }
    if body.context.as_ref().is_some_and(|c| !c.is_object()) {
        return Err(Problem::unprocessable("context must be a JSON object"));
    }
    let Some(Extension(control)) = control else {
        return Err(Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Event runtime not available",
        ));
    };
    // The context is caller-supplied prompt input: only its top-level keys
    // are audited, never its values.
    let context_keys: Option<Vec<String>> = body
        .context
        .as_ref()
        .and_then(Value::as_object)
        .map(|c| c.keys().cloned().collect());
    let result = control
        .publish(&access.ws, &m.name, &id, count as usize, body.context)
        .await;
    let (messages, error) = match result {
        Ok(messages) => (messages, None),
        Err(ChannelControlError::Partial { published, error }) => (published, Some(*error)),
        Err(e) => return Err(control_problem(e)),
    };
    // Every publish that sent something is audited, a partial one too.
    if !messages.is_empty() {
        let mut after = serde_json::json!({
            "count": count,
            "published": messages.len(),
            "addresses": messages.iter().map(|m| m.address.as_str()).collect::<Vec<_>>(),
        });
        if let Some(keys) = &context_keys {
            after["context_keys"] = serde_json::json!(keys);
        }
        if let Some(e) = &error {
            after["error"] = Value::String(e.detail().to_string());
        }
        access
            .audit(
                &st.audit,
                "channel.publish",
                format!("channel:{mock}/{id}"),
                None,
                Some(after),
            )
            .await;
    }
    match error {
        None => Ok(Json(PublishResult { messages })),
        Some(e) => {
            let sent = messages.len();
            let detail = format!(
                "{} ({sent} of {count} messages were published before the failure)",
                e.detail()
            );
            Err(control_problem(match e {
                ChannelControlError::Unknown(_) => ChannelControlError::Unknown(detail),
                ChannelControlError::Conflict(_) => ChannelControlError::Conflict(detail),
                ChannelControlError::NotRunning(_) => ChannelControlError::NotRunning(detail),
                ChannelControlError::Failed(_) | ChannelControlError::Partial { .. } => {
                    ChannelControlError::Failed(detail)
                }
            }))
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/event-settings",
    tag = "channels",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    responses(
        (status = 200, body = EventSettingsView),
        (status = 403, body = Problem, content_type = "application/problem+json"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_event_settings(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
) -> ApiResult<Json<EventSettingsView>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let view = match st.store.get_event_settings(m.id).await? {
        Some(s) => EventSettingsView {
            broker: s.broker,
            source: source_name(&s.source).into(),
        },
        None => EventSettingsView {
            broker: None,
            source: "api".into(),
        },
    };
    Ok(Json(view))
}

#[utoipa::path(
    put,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/event-settings",
    tag = "channels",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    request_body = PutEventSettings,
    responses(
        (status = 200, body = EventSettingsView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 415, body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Unknown broker", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn put_event_settings(
    State(st): State<AdminState>,
    access: WsAccess,
    control: Control,
    Path(MockPath { mock }): Path<MockPath>,
    Json(body): Json<PutEventSettings>,
) -> ApiResult<Json<EventSettingsView>> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    ensure_api_managed(&m.source, format_args!("mock {mock:?}"))?;
    let existing = st.store.get_event_settings(m.id).await?;
    if let Some(existing) = &existing {
        ensure_api_managed(
            &existing.source,
            format_args!("event settings of mock {mock:?}"),
        )?;
    }
    if let (Some(name), Some(Extension(control))) = (&body.broker, &control)
        && !control.brokers().iter().any(|b| &b.name == name)
    {
        return Err(Problem::unprocessable(format!(
            "no broker named {name:?} is configured"
        )));
    }
    let row = MockEventSettings {
        mock_id: m.id,
        broker: body.broker,
        source: ResourceSource::Api,
        updated_at: Utc::now(),
    };
    st.store.put_event_settings(&row).await?;
    let view = EventSettingsView {
        broker: row.broker,
        source: "api".into(),
    };
    let before = existing.map(|e| EventSettingsView {
        broker: e.broker,
        source: source_name(&e.source).into(),
    });
    access
        .audit(
            &st.audit,
            "event_settings.update",
            format!("mock:{mock}"),
            before.as_ref().and_then(snapshot),
            snapshot(&view),
        )
        .await;
    Ok(Json(view))
}

#[utoipa::path(
    get,
    path = "/api/v1/brokers",
    tag = "brokers",
    responses(
        (status = 200, description = "The configured brokers, sorted by name", body = Vec<BrokerView>),
        (status = 401, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn brokers(_caller: CurrentPrincipal, control: Control) -> Json<Vec<BrokerView>> {
    let mut list = control.map(|Extension(c)| c.brokers()).unwrap_or_default();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    Json(list)
}
