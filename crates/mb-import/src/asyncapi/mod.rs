//! AsyncAPI -> MCM compiler. This module holds what the 2.x and 3.0
//! front ends share: `$ref` and trait resolution, payload schema formats,
//! the broker protocol of a channel and the `x-mockingbird-*` extensions.

mod avro;
mod bindings;
mod v2;
mod v3;

use std::collections::{BTreeMap, BTreeSet};

use mb_mcm::{Channel, Direction, Reply, StateEffect, Trigger};
use serde_json::{Map, Value, json};

use crate::openapi3::{EXTERNAL_REF_HINT, check_refs, deref, invalid};
use crate::schema::{collect_refs, normalize_schema};
use crate::{ImportError, triggers::check_triggers, version_string};

pub(crate) use v2::compile_v2;
pub(crate) use v3::compile_v3;

/// Source prefix of component schema references.
pub(crate) const REF_PREFIX: &str = "#/components/schemas/";

const TRIGGERS_EXT: &str = "x-mockingbird-triggers";
const REPLY_EXT: &str = "x-mockingbird-reply";
const STATE_EXT: &str = "x-mockingbird-state";

/// Compilation state shared by the 2.x and 3.0 front ends.
pub(crate) struct Ctx<'a> {
    pub(crate) doc: &'a Value,
    /// JSON Schema definitions: the component schemas, then the named Avro
    /// types.
    pub(crate) defs: BTreeMap<String, Value>,
    /// Channel ids assigned so far.
    pub(crate) ids: BTreeSet<String>,
}

impl<'a> Ctx<'a> {
    /// Starts with `components.schemas` as the definitions.
    pub(crate) fn new(doc: &'a Value) -> Result<Self, ImportError> {
        let mut defs = BTreeMap::new();
        if let Some(schemas) = doc
            .pointer("/components/schemas")
            .and_then(Value::as_object)
        {
            for (name, s) in schemas {
                reject_external(s, &format!("/components/schemas/{}", escape(name)))?;
                defs.insert(name.clone(), normalize_schema(s, REF_PREFIX, false));
            }
        }
        Ok(Self {
            doc,
            defs,
            ids: BTreeSet::new(),
        })
    }

    /// Claims a channel id; a duplicate is invalid at `pointer`.
    pub(crate) fn claim_id(&mut self, id: &str, pointer: String) -> Result<(), ImportError> {
        if self.ids.insert(id.to_owned()) {
            Ok(())
        } else {
            Err(invalid(pointer, format!("duplicate operationId {id:?}")))
        }
    }
}

pub(crate) use crate::openapi3::{escape, slug};

/// `info.title` (required) and `info.version` (`0.0.0` when absent).
pub(crate) fn bundle_info(doc: &Value) -> Result<(String, String), ImportError> {
    let info = doc.get("info");
    let title = info
        .and_then(|i| i.get("title"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("/info/title", "missing required field"))?
        .to_owned();
    let version =
        version_string(info.and_then(|i| i.get("version"))).unwrap_or_else(|| "0.0.0".into());
    Ok((title, version))
}

/// Follows local `$ref`s (through [`deref`]); an external one is invalid
/// at `at`.
pub(crate) fn resolve<'a>(
    doc: &'a Value,
    v: &'a Value,
    at: &str,
) -> Result<&'a Value, ImportError> {
    let mut cur = v;
    for _ in 0..16 {
        let Some(r) = cur.get("$ref").and_then(Value::as_str) else {
            break;
        };
        if !r.starts_with('#') {
            return Err(external_ref(r, at));
        }
        match r.strip_prefix('#').and_then(|p| doc.pointer(p)) {
            Some(next) => cur = next,
            None => break,
        }
    }
    deref(doc, v, at)
}

/// Rejects any `$ref` inside `schema` that is not document-local.
pub(crate) fn reject_external(schema: &Value, at: &str) -> Result<(), ImportError> {
    let mut refs = Vec::new();
    collect_refs(schema, &mut refs);
    match refs.iter().find(|(_, r)| !r.starts_with('#')) {
        Some((_, r)) => Err(external_ref(r, at)),
        None => Ok(()),
    }
}

/// The error of an external `$ref` left after bundling, at `at`.
fn external_ref(r: &str, at: &str) -> ImportError {
    invalid(
        at,
        format!("external $ref {r} cannot be resolved: {EXTERNAL_REF_HINT}"),
    )
}

/// Normalizes an AsyncAPI (JSON Schema) schema.
pub(crate) fn normalize(schema: &Value, at: &str) -> Result<Value, ImportError> {
    reject_external(schema, at)?;
    Ok(normalize_schema(schema, REF_PREFIX, false))
}

/// The last segment of `v`'s `$ref`, if it is a reference.
pub(crate) fn ref_name(v: &Value) -> Option<String> {
    v.get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.rsplit('/').next())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// JSON Merge Patch (RFC 7396): `patch`'s values win.
pub(crate) fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(obj) = target {
        for (k, v) in patch {
            if v.is_null() {
                obj.remove(k);
            } else {
                merge_patch(obj.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    }
}

/// `item` with its `traits` merged in declared order (2.x semantics: the
/// trait's values win).
pub(crate) fn apply_traits(doc: &Value, item: &Value, at: &str) -> Result<Value, ImportError> {
    let mut out = item.clone();
    if let Value::Object(obj) = &mut out {
        obj.remove("traits");
    }
    if let Some(traits) = item.get("traits").and_then(Value::as_array) {
        for (i, t) in traits.iter().enumerate() {
            let t = resolve(doc, t, &format!("{at}/traits/{i}"))?;
            merge_patch(&mut out, t);
        }
    }
    Ok(out)
}

/// `item` with its `traits` merged in declared order under it (3.0
/// semantics: the item's own values win).
pub(crate) fn apply_traits_under(
    doc: &Value,
    item: &Value,
    at: &str,
) -> Result<Value, ImportError> {
    let Some(traits) = item.get("traits").and_then(Value::as_array) else {
        return Ok(item.clone());
    };
    let mut own = item.clone();
    if let Value::Object(obj) = &mut own {
        obj.remove("traits");
    }
    let mut out = Value::Object(Map::new());
    for (i, t) in traits.iter().enumerate() {
        let t = resolve(doc, t, &format!("{at}/traits/{i}"))?;
        merge_patch(&mut out, t);
    }
    merge_patch(&mut out, &own);
    Ok(out)
}

/// A non-empty string field of `v`.
pub(crate) fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// How a message payload is written.
enum PayloadFormat {
    JsonSchema,
    Avro,
}

fn payload_format(schema_format: Option<&str>) -> Option<PayloadFormat> {
    let Some(f) = schema_format else {
        return Some(PayloadFormat::JsonSchema);
    };
    let lower = f.to_ascii_lowercase();
    let mut parts = lower.split(';').map(str::trim);
    let base = parts.next().unwrap_or_default();
    let version = parts
        .filter_map(|p| p.strip_prefix("version="))
        .map(|v| v.trim_matches('"'))
        .next();
    match base {
        "application/vnd.aai.asyncapi"
        | "application/vnd.aai.asyncapi+json"
        | "application/vnd.aai.asyncapi+yaml" => Some(PayloadFormat::JsonSchema),
        "application/schema+json" | "application/schema+yaml" if version == Some("draft-07") => {
            Some(PayloadFormat::JsonSchema)
        }
        "application/vnd.apache.avro"
        | "application/vnd.apache.avro+json"
        | "application/vnd.apache.avro+yaml"
            if version.is_some_and(|v| v.starts_with("1.")) =>
        {
            Some(PayloadFormat::Avro)
        }
        _ => None,
    }
}

/// The JSON Schema of `message`'s payload (`{}` when it has none), read
/// according to `schema_format`. `at` points at the message.
pub(crate) fn message_payload(
    ctx: &mut Ctx,
    message: &Value,
    schema_format: Option<&str>,
    at: &str,
) -> Result<Value, ImportError> {
    payload_schema(
        ctx,
        message.get("payload"),
        schema_format,
        at,
        &format!("{at}/payload"),
    )
}

/// The JSON Schema of a payload (`{}` when there is none) written in
/// `schema_format`. `at` points at the message, `payload_at` at the schema.
pub(crate) fn payload_schema(
    ctx: &mut Ctx,
    payload: Option<&Value>,
    schema_format: Option<&str>,
    at: &str,
    payload_at: &str,
) -> Result<Value, ImportError> {
    let format = payload_format(schema_format).ok_or_else(|| {
        ImportError::Unsupported(format!(
            "schemaFormat {} at {at}",
            schema_format.unwrap_or_default()
        ))
    })?;
    let Some(payload) = payload else {
        return Ok(json!({}));
    };
    match format {
        PayloadFormat::JsonSchema => normalize(payload, payload_at),
        PayloadFormat::Avro => {
            let payload = resolve(ctx.doc, payload, payload_at)?;
            avro_payload(ctx, payload, payload_at)
        }
    }
}

/// One message of an operation, with its traits merged.
pub(crate) struct MessageInput {
    pub(crate) value: Value,
    /// The name used when the message has neither `name` nor `messageId`.
    pub(crate) fallback_name: Option<String>,
    pub(crate) at: String,
}

/// What a channel takes from its messages.
pub(crate) struct MessageSet {
    pub(crate) message_schema: Value,
    pub(crate) message_names: Vec<String>,
    pub(crate) content_type: String,
    pub(crate) key_schema: Option<Value>,
    pub(crate) headers_schema: Option<Value>,
    /// The messages' summaries and descriptions.
    pub(crate) hints: Vec<String>,
}

/// Reads a message's payload schema; `at` points at the message.
pub(crate) type PayloadReader = fn(&mut Ctx, &Value, &str) -> Result<Value, ImportError>;

/// Collects the schemas, names, content type, Kafka key and headers of the
/// messages of channel `id`.
pub(crate) fn collect_messages(
    ctx: &mut Ctx,
    msgs: &[MessageInput],
    id: &str,
    protocol: &str,
    default_content_type: &str,
    read_payload: PayloadReader,
) -> Result<MessageSet, ImportError> {
    let mut schemas = Vec::new();
    let mut names = Vec::new();
    let mut headers = Vec::new();
    let mut key_schema = None;
    let mut hints = Vec::new();
    for (n, m) in msgs.iter().enumerate() {
        let v = &m.value;
        schemas.push(read_payload(ctx, v, &m.at)?);
        names.push(
            str_of(v, "name")
                .or_else(|| str_of(v, "messageId"))
                .map(str::to_owned)
                .or_else(|| m.fallback_name.clone())
                .unwrap_or_else(|| format!("{id}-message-{}", n + 1)),
        );
        headers.push(
            v.get("headers")
                .map(|h| normalize(h, &format!("{}/headers", m.at)))
                .transpose()?,
        );
        if protocol == "kafka"
            && key_schema.is_none()
            && let Some(k) = v.pointer("/bindings/kafka/key")
        {
            key_schema = Some(normalize(k, &format!("{}/bindings/kafka/key", m.at))?);
        }
        hints.extend(
            [str_of(v, "summary"), str_of(v, "description")]
                .into_iter()
                .flatten()
                .map(str::to_owned),
        );
    }
    let content_type = msgs
        .first()
        .and_then(|m| str_of(&m.value, "contentType"))
        .unwrap_or(default_content_type)
        .to_owned();
    Ok(MessageSet {
        message_schema: one_of(schemas),
        message_names: names,
        content_type,
        key_schema,
        headers_schema: headers_schema(headers),
        hints,
    })
}

/// `tags[].name` of an operation.
pub(crate) fn tag_names(op: &Value) -> Vec<String> {
    op.get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|t| str_of(t, "name"))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Hex sha256 of `bytes`.
pub(crate) fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Converts an Avro payload. Avro names resolve only against the Avro
/// types defined so far, and a named type may not take the name of a
/// component schema.
fn avro_payload(ctx: &mut Ctx, payload: &Value, at: &str) -> Result<Value, ImportError> {
    let components: BTreeSet<&str> = ctx
        .doc
        .pointer("/components/schemas")
        .and_then(Value::as_object)
        .map(|s| s.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let mut avro_defs: BTreeMap<String, Value> = ctx
        .defs
        .iter()
        .filter(|(name, _)| !components.contains(name.as_str()))
        .map(|(name, s)| (name.clone(), s.clone()))
        .collect();
    let schema = avro::avro_to_json_schema(payload, &mut avro_defs, at)?;
    if let Some(name) = avro_defs.keys().find(|n| components.contains(n.as_str())) {
        return Err(invalid(
            at,
            format!("Avro type {name:?} collides with the component schema of the same name"),
        ));
    }
    ctx.defs.extend(avro_defs);
    Ok(schema)
}

const NO_PROTOCOL: &str = "cannot determine the broker protocol: add channel bindings or servers";

/// The broker protocol (`kafka`, `amqp`, `mqtt` or `ws`) of a channel
/// without bindings: that of its first listed server, else of the
/// document's first server in name order. `at` points at the channel.
pub(crate) fn server_protocol(
    doc: &Value,
    channel_servers: Option<&[String]>,
    at: &str,
) -> Result<&'static str, ImportError> {
    let servers = doc.get("servers").and_then(Value::as_object);
    let name = channel_servers
        .and_then(|s| s.first())
        .map(String::as_str)
        .or_else(|| servers.and_then(|s| s.keys().min()).map(String::as_str));
    let server = name
        .and_then(|n| servers.and_then(|s| s.get(n)))
        .ok_or_else(|| invalid(at, NO_PROTOCOL))?;
    let server = resolve(doc, server, at)?;
    let protocol = server
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match protocol.as_str() {
        "kafka" | "kafka-secure" => Ok("kafka"),
        "amqp" | "amqps" => Ok("amqp"),
        "mqtt" | "mqtts" | "secure-mqtt" => Ok("mqtt"),
        "ws" | "wss" => Ok("ws"),
        _ => Err(invalid(at, NO_PROTOCOL)),
    }
}

/// A channel's protocol: its single broker binding key, else
/// [`server_protocol`].
pub(crate) fn channel_protocol(
    doc: &Value,
    channel_bindings: Option<&Value>,
    channel_servers: Option<&[String]>,
    at: &str,
) -> Result<&'static str, ImportError> {
    match bindings::binding_protocol(channel_bindings) {
        Some(p) => Ok(p),
        None => server_protocol(doc, channel_servers, at),
    }
}

pub(crate) use bindings::binding;

/// The `x-mockingbird-*` extensions of one item, each with the pointer it
/// was read from.
#[derive(Clone, Default)]
pub(crate) struct Extensions {
    pub(crate) triggers: Option<(Vec<Trigger>, String)>,
    pub(crate) reply: Option<(String, String)>,
    pub(crate) state: Option<(StateEffect, String)>,
}

impl Extensions {
    /// Each extension of `self`, else of `fallback`.
    pub(crate) fn or(self, fallback: Extensions) -> Extensions {
        Extensions {
            triggers: self.triggers.or(fallback.triggers),
            reply: self.reply.or(fallback.reply),
            state: self.state.or(fallback.state),
        }
    }

    /// Checks the triggers and the state effect against `direction`.
    pub(crate) fn check(&self, direction: Direction) -> Result<(), ImportError> {
        if let Some((triggers, at)) = &self.triggers {
            check_triggers(triggers, direction).map_err(|e| invalid(at, e.to_string()))?;
        }
        if let Some((_, at)) = &self.state
            && direction != Direction::Subscribe
        {
            return Err(invalid(
                at,
                "x-mockingbird-state applies only to channels this mock subscribes to",
            ));
        }
        Ok(())
    }
}

fn deserialize_ext<T: serde::de::DeserializeOwned>(
    item: &Value,
    ctx: &Ctx,
    key: &str,
    at: &str,
) -> Result<Option<(T, String)>, ImportError> {
    let Some(raw) = item.get(key) else {
        return Ok(None);
    };
    let pointer = format!("{at}/{key}");
    let raw = resolve(ctx.doc, raw, &pointer)?;
    let value = T::deserialize(raw).map_err(|e| invalid(pointer.clone(), e.to_string()))?;
    Ok(Some((value, pointer)))
}

/// Reads `item`'s `x-mockingbird-triggers` (a JSON array of triggers),
/// `x-mockingbird-reply` (a channel id) and `x-mockingbird-state`.
pub(crate) fn extensions(item: &Value, ctx: &Ctx, at: &str) -> Result<Extensions, ImportError> {
    Ok(Extensions {
        triggers: deserialize_ext(item, ctx, TRIGGERS_EXT, at)?,
        reply: deserialize_ext(item, ctx, REPLY_EXT, at)?,
        state: deserialize_ext(item, ctx, STATE_EXT, at)?,
    })
}

/// `{"oneOf": [...]}` over several schemas; the schema itself when there
/// is one; `{}` when there are none.
pub(crate) fn one_of(mut schemas: Vec<Value>) -> Value {
    match schemas.len() {
        0 => json!({}),
        1 => schemas.remove(0),
        _ => json!({ "oneOf": schemas }),
    }
}

/// The headers schema of a channel: the first message's when all messages
/// agree, else a `oneOf` over the distinct declared ones.
pub(crate) fn headers_schema(headers: Vec<Option<Value>>) -> Option<Value> {
    let first = headers.first().cloned().flatten();
    if headers.iter().all(|h| *h == headers[0]) {
        return first;
    }
    let mut distinct: Vec<Value> = Vec::new();
    for h in headers.into_iter().flatten() {
        if !distinct.contains(&h) {
            distinct.push(h);
        }
    }
    Some(one_of(distinct))
}

/// The `{name}` placeholders of an address, in order.
pub(crate) fn address_params(address: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = address;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        out.push(&after[..end]);
        rest = &after[end + 1..];
    }
    out
}

/// Checks each reply target is a publish channel of the bundle.
/// `replies` pairs a channel index with the pointer of its reply extension.
pub(crate) fn check_replies(
    channels: &[Channel],
    replies: &[(usize, String)],
) -> Result<(), ImportError> {
    for (index, at) in replies {
        let Some(Reply {
            channel: target, ..
        }) = channels.get(*index).and_then(|c| c.reply.as_ref())
        else {
            continue;
        };
        let ok = channels
            .iter()
            .any(|c| c.id == *target && c.direction == Direction::Publish);
        if !ok {
            return Err(invalid(
                at,
                format!("reply channel {target:?} is not a channel this mock publishes"),
            ));
        }
    }
    Ok(())
}

/// Checks every `#/$defs/...` reference of the channels and the
/// definitions resolves; returns the definitions as a map.
pub(crate) fn finish_defs(
    channels: &[Channel],
    defs: BTreeMap<String, Value>,
) -> Result<Map<String, Value>, ImportError> {
    let defs: Map<String, Value> = defs.into_iter().collect();
    for c in channels {
        let at = format!("channel {}", c.id);
        for s in [
            Some(&c.message_schema),
            c.key_schema.as_ref(),
            c.headers_schema.as_ref(),
        ]
        .into_iter()
        .flatten()
        .chain(c.parameters.values())
        {
            check_refs(s, &defs, &at, "")?;
        }
    }
    for (name, s) in &defs {
        check_refs(
            s,
            &defs,
            &format!("schema {name}"),
            &format!("/components/schemas/{}", escape(name)),
        )?;
    }
    Ok(defs)
}
