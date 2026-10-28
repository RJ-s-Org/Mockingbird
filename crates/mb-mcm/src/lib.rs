//! Mockingbird Contract Model: protocol-neutral description of an API contract.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The shortest interval a [`Trigger::Schedule`] may fire at.
pub const MIN_SCHEDULE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Bundle {
    /// Slug derived from the title, e.g. `orders-api`.
    pub id: String,
    pub title: String,
    /// The contract's `info.version`.
    pub version: String,
    /// Hex sha256 of the source bytes.
    pub source_digest: String,
    pub operations: Vec<Operation>,
    /// JSON Schema 2020-12 definitions.
    pub defs: Map<String, Value>,
    /// The protocol-native source the adapters need on the wire (SDL,
    /// descriptor set or WSDL); absent for HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_payload: Option<ProtocolPayload>,
    /// Event-driven channels (AsyncAPI); absent for request/response contracts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<Channel>,
    /// Outbound events the contract describes (OpenAPI `webhooks` and
    /// `callbacks`); used as webhook payloads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<WebhookDef>,
}

/// An outbound event the contract describes: OpenAPI 3.1 `webhooks` or an
/// operation's `callbacks`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct WebhookDef {
    /// The `webhooks` key, or `<operationId>.<callbackName>`.
    pub name: String,
    /// For a callback, the operation that declares it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// JSON Schema 2020-12 of the request body (refs as in `defs`).
    pub payload_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Protocol-native material carried next to the operations.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolPayload {
    /// A serialized `google.protobuf.FileDescriptorSet` (standard base64 in JSON).
    Grpc {
        #[serde(with = "base64_bytes")]
        file_descriptor_set: Vec<u8>,
    },
    /// GraphQL SDL.
    Graphql { sdl: String },
    /// The WSDL document, with imported schemas inlined.
    Soap { wsdl: String },
}

mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Protocol {
    Http,
    Graphql,
    Grpc,
    Soap,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GraphqlOperationType {
    Query,
    Mutation,
    Subscription,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SoapVersion {
    #[serde(rename = "soap11")]
    Soap11,
    #[serde(rename = "soap12")]
    Soap12,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Binding {
    /// `path` uses `{param}` placeholders.
    Http { method: HttpMethod, path: String },
    /// A root field of the `Query`, `Mutation` or `Subscription` type.
    Graphql {
        operation_type: GraphqlOperationType,
        field: String,
    },
    /// `service` is the short service name; `package` may be empty.
    Grpc {
        package: String,
        service: String,
        method: String,
    },
    /// A WSDL operation reached through `service`/`port` at `address_path`.
    Soap {
        service: String,
        port: String,
        operation: String,
        soap_action: Option<String>,
        version: SoapVersion,
        address_path: String,
    },
}

impl Binding {
    /// The gRPC request path, `/pkg.Service/Method` (`/Service/Method` for
    /// an empty package); `None` for other bindings.
    pub fn grpc_path(&self) -> Option<String> {
        match self {
            Binding::Grpc {
                package,
                service,
                method,
            } if package.is_empty() => Some(format!("/{service}/{method}")),
            Binding::Grpc {
                package,
                service,
                method,
            } => Some(format!("/{package}.{service}/{method}")),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

/// What an operation does. For the streaming kinds, the success
/// [`OutputVariant`]'s schema is the schema of ONE stream item (or of the
/// single response, for [`OperationKind::StreamClient`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Query,
    Command(Effect),
    /// One request, a stream of responses.
    StreamServer,
    /// A stream of requests, one response.
    StreamClient,
    /// Streams in both directions.
    StreamBidi,
}

impl OperationKind {
    /// Whether the response is a stream of items.
    pub fn streams_response(&self) -> bool {
        matches!(
            self,
            OperationKind::StreamServer | OperationKind::StreamBidi
        )
    }

    /// Whether the request is a stream of items.
    pub fn streams_request(&self) -> bool {
        matches!(
            self,
            OperationKind::StreamClient | OperationKind::StreamBidi
        )
    }

    /// `query`, `create`, `update`, `delete`, `other`, `stream-server`,
    /// `stream-client` or `stream-bidi`.
    pub fn label(&self) -> &'static str {
        match self {
            OperationKind::Query => "query",
            OperationKind::Command(Effect::Create) => "create",
            OperationKind::Command(Effect::Update) => "update",
            OperationKind::Command(Effect::Delete) => "delete",
            OperationKind::Command(Effect::Other) => "other",
            OperationKind::StreamServer => "stream-server",
            OperationKind::StreamClient => "stream-client",
            OperationKind::StreamBidi => "stream-bidi",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Create,
    Update,
    Delete,
    Other,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Operation {
    /// `operationId` or a `{method}_{path}` slug.
    pub id: String,
    pub protocol: Protocol,
    pub binding: Binding,
    pub kind: OperationKind,
    pub summary: Option<String>,
    pub input: InputSchemas,
    pub outputs: Vec<OutputVariant>,
    /// e.g. `orders`.
    pub resource_type: Option<String>,
    pub identity: Option<Identity>,
    /// Description text and tags.
    pub semantic_hints: Vec<String>,
    /// Security requirements, any one of which satisfies the operation (OR
    /// across requirements). Empty means the operation is unsecured; a
    /// requirement with no schemes makes authentication optional.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security: Vec<SecurityRequirement>,
}

/// Where an API key is carried.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyLocation {
    Header,
    Query,
    Cookie,
}

/// A credential form an operation accepts.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecurityScheme {
    /// `Authorization: Bearer <token>`.
    Bearer,
    /// `Authorization: Basic <base64(user:password)>`.
    Basic,
    /// A named header, query parameter or cookie.
    ApiKey {
        location: ApiKeyLocation,
        name: String,
    },
    /// An OAuth 2 access token (presented as a bearer token).
    #[serde(rename = "oauth2")]
    OAuth2 { scopes: Vec<String> },
    /// An OpenID Connect token (presented as a bearer token).
    OpenIdConnect { scopes: Vec<String> },
}

/// Schemes that must all be satisfied together (AND).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct SecurityRequirement {
    pub schemes: Vec<SecurityScheme>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct InputSchemas {
    pub path: Option<Value>,
    pub query: Option<Value>,
    pub headers: Option<Value>,
    pub body: Option<Value>,
    /// Whether a request body must be present (OpenAPI 3
    /// `requestBody.required`, Swagger 2 `in: body` `required` or any
    /// required `formData` parameter).
    #[serde(default)]
    pub body_required: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusSelector {
    Code(u16),
    Default,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct OutputVariant {
    pub status: StatusSelector,
    pub content_type: Option<String>,
    pub schema: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Identity {
    pub path_param: Option<String>,
    pub field: String,
    /// Top-level request-body field carrying the target id (non-HTTP protocols).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_field: Option<String>,
}

impl Bundle {
    /// The payload definition of the webhook `op_id` fires as `event_type`:
    /// a callback of the operation, else a webhook named (ASCII
    /// case-insensitively) after the event type or the operation id.
    pub fn webhook_for(&self, op_id: &str, event_type: &str) -> Option<&WebhookDef> {
        self.webhooks
            .iter()
            .find(|w| w.operation.as_deref() == Some(op_id))
            .or_else(|| {
                self.webhooks.iter().find(|w| {
                    w.name.eq_ignore_ascii_case(event_type) || w.name.eq_ignore_ascii_case(op_id)
                })
            })
    }

    /// The first operation's protocol; [`Protocol::Http`] when there are none.
    pub fn protocol(&self) -> Protocol {
        self.operations
            .first()
            .map_or(Protocol::Http, |o| o.protocol)
    }

    pub fn operation(&self, id: &str) -> Option<&Operation> {
        self.operations.iter().find(|o| o.id == id)
    }

    pub fn channel(&self, id: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    /// Full names (`pkg.Service`, or `Service` without a package) of the
    /// gRPC services this bundle's operations bind to, sorted.
    pub fn grpc_services(&self) -> std::collections::BTreeSet<String> {
        self.operations
            .iter()
            .filter_map(|op| match &op.binding {
                Binding::Grpc {
                    package, service, ..
                } if package.is_empty() => Some(service.clone()),
                Binding::Grpc {
                    package, service, ..
                } => Some(format!("{package}.{service}")),
                _ => None,
            })
            .collect()
    }

    /// Clone `schema` with this bundle's definitions attached as `$defs`, so
    /// it can be validated or faked standalone.
    pub fn with_defs(&self, schema: &Value) -> Value {
        attach_defs(schema, self.defs.clone())
    }

    /// Like [`Bundle::with_defs`], but attaches only the definitions
    /// reachable from `schema` through `#/$defs/...` references (the
    /// transitive closure), keeping prompts and validators small.
    pub fn with_reachable_defs(&self, schema: &Value) -> Value {
        let mut reachable = Map::new();
        let mut pending: Vec<String> = Vec::new();
        collect_def_refs(schema, &mut pending);
        while let Some(name) = pending.pop() {
            if reachable.contains_key(&name) {
                continue;
            }
            if let Some(def) = self.defs.get(&name) {
                collect_def_refs(def, &mut pending);
                reachable.insert(name, def.clone());
            }
        }
        attach_defs(schema, reachable)
    }
}

/// A channel's direction from the mock's point of view.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Mockingbird produces messages on the channel.
    Publish,
    /// Mockingbird consumes messages from the channel.
    Subscribe,
}

/// Where a channel's messages travel. Topics, routing keys and paths keep
/// the contract's `{param}` placeholders.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelBinding {
    Kafka {
        topic: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partitions_hint: Option<u32>,
    },
    Amqp {
        /// `""` is the default exchange.
        exchange: String,
        /// `direct`, `topic`, `fanout` or `headers`.
        exchange_type: String,
        routing_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue: Option<String>,
    },
    Mqtt {
        topic: String,
        /// 0..=2.
        qos: u8,
        retain: bool,
    },
    #[serde(rename = "websocket")]
    WebSocket { path: String },
}

impl ChannelBinding {
    /// `kafka`, `amqp`, `mqtt` or `websocket`.
    pub fn protocol(&self) -> &'static str {
        match self {
            ChannelBinding::Kafka { .. } => "kafka",
            ChannelBinding::Amqp { .. } => "amqp",
            ChannelBinding::Mqtt { .. } => "mqtt",
            ChannelBinding::WebSocket { .. } => "websocket",
        }
    }

    /// The Kafka topic, the AMQP routing key (the queue when the routing
    /// key is empty), the MQTT topic or the WebSocket path.
    pub fn address(&self) -> &str {
        match self {
            ChannelBinding::Kafka { topic, .. } | ChannelBinding::Mqtt { topic, .. } => topic,
            ChannelBinding::Amqp {
                routing_key, queue, ..
            } if routing_key.is_empty() => queue.as_deref().unwrap_or(routing_key),
            ChannelBinding::Amqp { routing_key, .. } => routing_key,
            ChannelBinding::WebSocket { path } => path,
        }
    }
}

/// Which resource changes fire a [`Trigger::OnStateChange`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StateChangeKind {
    Upserted,
    Deleted,
    #[default]
    Any,
}

/// When a publish channel produces a message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Every `every` (serialized as whole milliseconds, `every_ms`).
    Schedule {
        #[serde(rename = "every_ms", with = "duration_ms")]
        every: Duration,
    },
    /// A 5- or 6-field cron expression (see [`Trigger::normalized_cron`]).
    Cron { expr: String },
    /// When a resource of `resource_type` changes in the mock's state.
    OnStateChange {
        resource_type: String,
        #[serde(default)]
        kind: StateChangeKind,
    },
    /// Only on demand.
    Manual,
}

impl Trigger {
    /// A 5-field expression gains a leading seconds field (`0 ` + expr);
    /// anything else is returned trimmed.
    pub fn normalized_cron(expr: &str) -> String {
        let trimmed = expr.trim();
        if trimmed.split_whitespace().count() == 5 {
            format!("0 {trimmed}")
        } else {
            trimmed.to_string()
        }
    }
}

mod duration_ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_millis)
    }
}

/// Where a subscribe channel answers each consumed message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Reply {
    /// The id of a [`Direction::Publish`] channel.
    pub channel: String,
    /// A message header naming the reply address, overriding the channel's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address_header: Option<String>,
}

/// How a consumed message changes the mock's state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct StateEffect {
    pub resource_type: String,
    /// The message field carrying the resource id.
    pub id_field: String,
    #[serde(default)]
    pub delete: bool,
}

/// An event-driven channel.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct Channel {
    pub id: String,
    pub title: String,
    /// As in the contract, with `{param}` placeholders kept.
    pub address: String,
    pub binding: ChannelBinding,
    pub direction: Direction,
    /// JSON Schema 2020-12; a `oneOf` over the messages when several.
    pub message_schema: Value,
    pub message_names: Vec<String>,
    /// Usually `application/json`.
    pub content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers_schema: Option<Value>,
    /// Address parameter name → schema.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub parameters: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<Trigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<Reply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_effect: Option<StateEffect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub semantic_hints: Vec<String>,
}

impl Channel {
    /// Publish channels: their triggers, or `[Manual]` when none are
    /// declared. Subscribe channels never fire triggers.
    pub fn effective_triggers(&self) -> Vec<Trigger> {
        match self.direction {
            Direction::Publish if self.triggers.is_empty() => vec![Trigger::Manual],
            Direction::Publish => self.triggers.clone(),
            Direction::Subscribe => Vec::new(),
        }
    }
}

fn attach_defs(schema: &Value, defs: Map<String, Value>) -> Value {
    let mut out = schema.clone();
    match out.as_object_mut() {
        Some(obj) => {
            obj.insert("$defs".into(), Value::Object(defs));
            out
        }
        // Boolean schemas (`true`/`false`) cannot carry `$defs`; wrap them.
        None => {
            let mut obj = Map::new();
            obj.insert("allOf".into(), Value::Array(vec![out]));
            obj.insert("$defs".into(), Value::Object(defs));
            Value::Object(obj)
        }
    }
}

/// Push the definition name of every `#/$defs/<name>[/...]` reference in
/// `value` (JSON-pointer unescaped).
fn collect_def_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(obj) => {
            for (k, v) in obj {
                if k == "$ref"
                    && let Some(r) = v.as_str()
                    && let Some(rest) = r.strip_prefix("#/$defs/")
                {
                    let name = rest.split('/').next().unwrap_or(rest);
                    out.push(name.replace("~1", "/").replace("~0", "~"));
                } else {
                    collect_def_refs(v, out);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_def_refs(v, out);
            }
        }
        _ => {}
    }
}

impl Operation {
    /// The schema of one response stream item (the single response, for
    /// client streaming): the success output's schema, for streaming kinds.
    pub fn stream_item_schema(&self) -> Option<&Value> {
        match self.kind {
            OperationKind::StreamServer
            | OperationKind::StreamClient
            | OperationKind::StreamBidi => self.success_output().and_then(|o| o.schema.as_ref()),
            _ => None,
        }
    }

    /// A human-readable call target: `GET /orders/{id}`, `query user`,
    /// `/pkg.Service/Method` or `Service/Port Operation`.
    pub fn target(&self) -> String {
        match &self.binding {
            Binding::Http { method, path } => format!("{} {path}", method_name(*method)),
            Binding::Graphql {
                operation_type,
                field,
            } => {
                let t = match operation_type {
                    GraphqlOperationType::Query => "query",
                    GraphqlOperationType::Mutation => "mutation",
                    GraphqlOperationType::Subscription => "subscription",
                };
                format!("{t} {field}")
            }
            Binding::Grpc { .. } => self.binding.grpc_path().unwrap_or_default(),
            Binding::Soap {
                service,
                port,
                operation,
                ..
            } => format!("{service}/{port} {operation}"),
        }
    }

    /// Lowest `Code` in 200..=299, else `Default`.
    pub fn success_output(&self) -> Option<&OutputVariant> {
        self.outputs
            .iter()
            .filter_map(|o| match o.status {
                StatusSelector::Code(c) if (200..300).contains(&c) => Some((c, o)),
                _ => None,
            })
            .min_by_key(|(c, _)| *c)
            .map(|(_, o)| o)
            .or_else(|| self.default_output())
    }

    /// Exact `Code` match, else `Default`.
    pub fn output_for_status(&self, code: u16) -> Option<&OutputVariant> {
        self.outputs
            .iter()
            .find(|o| o.status == StatusSelector::Code(code))
            .or_else(|| self.default_output())
    }

    /// First exact match from `preferred`, else any 4xx `Code`.
    pub fn error_output(&self, preferred: &[u16]) -> Option<&OutputVariant> {
        preferred
            .iter()
            .find_map(|c| {
                self.outputs
                    .iter()
                    .find(|o| o.status == StatusSelector::Code(*c))
            })
            .or_else(|| {
                self.outputs.iter().find(
                    |o| matches!(o.status, StatusSelector::Code(c) if (400..500).contains(&c)),
                )
            })
    }

    fn default_output(&self) -> Option<&OutputVariant> {
        self.outputs
            .iter()
            .find(|o| o.status == StatusSelector::Default)
    }
}

fn method_name(m: HttpMethod) -> &'static str {
    match m {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Head => "HEAD",
        HttpMethod::Options => "OPTIONS",
    }
}
