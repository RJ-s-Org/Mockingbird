//! AsyncAPI 3.0 -> MCM channels: one channel per operation, plus a
//! synthetic publish channel for each `reply` of a received operation.

use mb_mcm::{Bundle, Channel, Direction, Reply};
use serde_json::{Map, Value, json};

use super::{
    Ctx, MessageInput, MessageSet, address_params, apply_traits_under, binding, bundle_info,
    channel_protocol, check_replies, collect_messages, digest, escape, extensions, finish_defs,
    invalid, payload_schema, ref_name, resolve, slug, str_of, tag_names,
};
use crate::ImportError;

const CHANNELS_PREFIX: &str = "#/channels/";
const HEADER_LOCATION: &str = "$message.header#/";

/// A channel an operation refers to.
struct ChannelRef<'a> {
    value: &'a Value,
    /// The pointer of the channel definition.
    at: String,
    /// `address`, else the channel key when it is absent or null.
    address: String,
}

/// Resolves an operation's (or reply's) `channel`, which must be a `$ref`
/// to `#/channels/<name>`. `at` points at the `channel` field.
fn channel_ref<'a>(
    doc: &'a Value,
    raw: Option<&'a Value>,
    at: &str,
) -> Result<ChannelRef<'a>, ImportError> {
    let pointer = raw
        .and_then(|r| r.get("$ref"))
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix(CHANNELS_PREFIX))
        .ok_or_else(|| invalid(at, "channel must be a $ref to #/channels/<name>"))?;
    let name = pointer.replace("~1", "/").replace("~0", "~");
    let raw_channel = doc
        .pointer(&format!("/channels/{pointer}"))
        .ok_or_else(|| invalid(at, format!("unknown channel {name:?}")))?;
    let channel_at = format!("/channels/{pointer}");
    let value = resolve(doc, raw_channel, &channel_at)?;
    let address = str_of(value, "address").unwrap_or(&name).to_owned();
    Ok(ChannelRef {
        value,
        at: channel_at,
        address,
    })
}

/// The server names a channel's `servers` `$ref`s point at.
fn server_names(channel: &Value) -> Option<Vec<String>> {
    channel
        .get("servers")
        .and_then(Value::as_array)
        .map(|s| s.iter().filter_map(ref_name).collect())
}

/// The channel's broker protocol, from its bindings or servers.
fn protocol_of(doc: &Value, channel: &ChannelRef) -> Result<&'static str, ImportError> {
    channel_protocol(
        doc,
        channel.value.get("bindings"),
        server_names(channel.value).as_deref(),
        &channel.at,
    )
}

/// Address parameter schemas: `{"type":"string"}` with the declared
/// `enum`/`default`, plus `{"type":"string"}` for each undeclared `{name}`
/// of the address.
fn parameters(
    doc: &Value,
    channel: &Value,
    address: &str,
    at: &str,
) -> Result<Map<String, Value>, ImportError> {
    let mut out = Map::new();
    if let Some(params) = channel.get("parameters").and_then(Value::as_object) {
        for (name, p) in params {
            let p = resolve(doc, p, &format!("{at}/parameters/{}", escape(name)))?;
            let mut schema = json!({"type": "string"});
            for key in ["enum", "default"] {
                if let Some(v) = p.get(key) {
                    schema[key] = v.clone();
                }
            }
            out.insert(name.clone(), schema);
        }
    }
    for name in address_params(address) {
        out.entry(name.to_owned())
            .or_insert_with(|| json!({"type": "string"}));
    }
    Ok(out)
}

/// A message, its traits merged under it.
fn message(
    doc: &Value,
    raw: &Value,
    fallback_name: Option<String>,
    at: String,
) -> Result<MessageInput, ImportError> {
    let value = apply_traits_under(doc, resolve(doc, raw, &at)?, &at)?;
    Ok(MessageInput {
        value,
        fallback_name,
        at,
    })
}

/// The messages a list of `$ref`s (an operation's or reply's `messages`)
/// points at. `at` points at the list.
fn listed_messages(
    doc: &Value,
    list: &[Value],
    at: &str,
) -> Result<Vec<MessageInput>, ImportError> {
    list.iter()
        .enumerate()
        .map(|(i, raw)| message(doc, raw, ref_name(raw), format!("{at}/{i}")))
        .collect()
}

/// Every message of a channel, in key order.
fn channel_messages(doc: &Value, channel: &ChannelRef) -> Result<Vec<MessageInput>, ImportError> {
    let Some(map) = channel.value.get("messages").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    keys.into_iter()
        .map(|key| {
            let at = format!("{}/messages/{}", channel.at, escape(key));
            message(doc, &map[key.as_str()], Some(key.clone()), at)
        })
        .collect()
}

/// A 3.0 payload: a schema, or a multi-format schema object
/// (`{schemaFormat, schema}`) read per its format.
fn read_payload(ctx: &mut Ctx, message: &Value, at: &str) -> Result<Value, ImportError> {
    let payload_at = format!("{at}/payload");
    let Some(payload) = message.get("payload") else {
        return payload_schema(ctx, None, None, at, &payload_at);
    };
    let resolved = resolve(ctx.doc, payload, &payload_at)?;
    match (
        resolved.get("schemaFormat").and_then(Value::as_str),
        resolved.get("schema"),
    ) {
        (Some(format), Some(schema)) => payload_schema(
            ctx,
            Some(schema),
            Some(format),
            at,
            &format!("{payload_at}/schema"),
        ),
        _ => payload_schema(ctx, Some(payload), None, at, &payload_at),
    }
}

/// The header named by a reply address `location` of the form
/// `$message.header#/<name>` (its last pointer segment).
fn address_header(reply: &Value, doc: &Value, at: &str) -> Result<Option<String>, ImportError> {
    let Some(address) = reply.get("address") else {
        return Ok(None);
    };
    let address = resolve(doc, address, &format!("{at}/address"))?;
    Ok(address
        .get("location")
        .and_then(Value::as_str)
        .and_then(|l| l.strip_prefix(HEADER_LOCATION))
        .and_then(|p| p.rsplit('/').next())
        .filter(|s| !s.is_empty())
        .map(|s| s.replace("~1", "/").replace("~0", "~")))
}

fn new_channel(
    id: String,
    title: String,
    address: String,
    binding: mb_mcm::ChannelBinding,
    direction: Direction,
    set: MessageSet,
    parameters: Map<String, Value>,
) -> Channel {
    Channel {
        id,
        title,
        address,
        binding,
        direction,
        message_schema: set.message_schema,
        message_names: set.message_names,
        content_type: set.content_type,
        key_schema: set.key_schema,
        headers_schema: set.headers_schema,
        parameters,
        triggers: Vec::new(),
        reply: None,
        state_effect: None,
        semantic_hints: set.hints,
    }
}

pub(crate) fn compile_v3(doc: &Value, bytes: &[u8]) -> Result<Bundle, ImportError> {
    let (title, version) = bundle_info(doc)?;
    let mut ctx = Ctx::new(doc)?;
    let default_content_type = str_of(doc, "defaultContentType").unwrap_or("application/json");
    let mut channels: Vec<Channel> = Vec::new();
    let mut replies: Vec<(usize, String)> = Vec::new();
    let empty = Map::new();
    let operations = doc
        .get("operations")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for (id, op) in operations {
        let op_at = format!("/operations/{}", escape(id));
        let op = apply_traits_under(doc, resolve(doc, op, &op_at)?, &op_at)?;
        let direction = match op.get("action").and_then(Value::as_str) {
            Some("send") => Direction::Publish,
            Some("receive") => Direction::Subscribe,
            other => {
                return Err(invalid(
                    format!("{op_at}/action"),
                    format!("action must be send or receive, not {other:?}"),
                ));
            }
        };
        let channel = channel_ref(doc, op.get("channel"), &format!("{op_at}/channel"))?;
        ctx.claim_id(id, op_at.clone())?;
        let protocol = protocol_of(doc, &channel)?;
        let params = parameters(doc, channel.value, &channel.address, &channel.at)?;
        let msgs = match op.get("messages").and_then(Value::as_array) {
            Some(list) => listed_messages(doc, list, &format!("{op_at}/messages"))?,
            None => channel_messages(doc, &channel)?,
        };
        let mut set = collect_messages(
            &mut ctx,
            &msgs,
            id,
            protocol,
            default_content_type,
            read_payload,
        )?;
        let mut semantic_hints: Vec<String> = [
            str_of(channel.value, "description"),
            str_of(&op, "description"),
        ]
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect();
        semantic_hints.extend(tag_names(&op));
        semantic_hints.append(&mut set.hints);
        set.hints = semantic_hints;

        let ext = extensions(&op, &ctx, &op_at)?.or(extensions(channel.value, &ctx, &channel.at)?);
        ext.check(direction)?;
        let title = str_of(&op, "title")
            .or_else(|| str_of(&op, "summary"))
            .unwrap_or(&channel.address)
            .to_owned();
        let mut c = new_channel(
            id.clone(),
            title,
            channel.address.clone(),
            binding(
                protocol,
                &channel.address,
                channel.value.get("bindings"),
                op.get("bindings"),
                &op_at,
            )?,
            direction,
            set,
            params,
        );
        c.triggers = ext.triggers.map(|(t, _)| t).unwrap_or_default();
        c.state_effect = ext.state.map(|(s, _)| s);

        // `operation.reply` of a received operation; it takes precedence
        // over `x-mockingbird-reply`.
        let reply_channel = match op.get("reply") {
            Some(raw) if direction == Direction::Subscribe => {
                let r_at = format!("{op_at}/reply");
                let reply = resolve(doc, raw, &r_at)?;
                let reply_id = format!("{id}.reply");
                ctx.claim_id(&reply_id, r_at.clone())?;
                c.reply = Some(Reply {
                    channel: reply_id.clone(),
                    address_header: address_header(reply, doc, &r_at)?,
                });
                Some(reply_channel(
                    &mut ctx,
                    reply,
                    &channel,
                    reply_id,
                    &r_at,
                    default_content_type,
                )?)
            }
            _ => {
                if let Some((target, at)) = ext.reply {
                    replies.push((channels.len(), at));
                    c.reply = Some(Reply {
                        channel: target,
                        address_header: None,
                    });
                }
                None
            }
        };
        channels.push(c);
        channels.extend(reply_channel);
    }
    check_replies(&channels, &replies)?;
    // Sorting after the reply check keeps the recorded indexes valid.
    channels.sort_by(|a, b| a.id.cmp(&b.id));
    let defs = finish_defs(&channels, ctx.defs)?;

    Ok(Bundle {
        id: slug(&title),
        title,
        version,
        source_digest: digest(bytes),
        operations: Vec::new(),
        defs,
        protocol_payload: None,
        channels,
        webhooks: Vec::new(),
    })
}

/// The synthetic publish channel `reply_id` for a reply object: the reply
/// channel's messages, else the reply's `messages`; its binding comes from
/// the reply channel, the address falling back to the request channel's.
fn reply_channel(
    ctx: &mut Ctx,
    reply: &Value,
    request: &ChannelRef,
    reply_id: String,
    at: &str,
    default_content_type: &str,
) -> Result<Channel, ImportError> {
    let doc = ctx.doc;
    let target = match reply.get("channel") {
        Some(raw) => Some(channel_ref(doc, Some(raw), &format!("{at}/channel"))?),
        None => None,
    };
    let source = target.as_ref().unwrap_or(request);
    let address = target
        .as_ref()
        .and_then(|t| str_of(t.value, "address"))
        .unwrap_or(&request.address)
        .to_owned();
    let protocol = protocol_of(doc, source)?;
    let mut msgs = match &target {
        Some(t) => channel_messages(doc, t)?,
        None => Vec::new(),
    };
    if msgs.is_empty()
        && let Some(list) = reply.get("messages").and_then(Value::as_array)
    {
        msgs = listed_messages(doc, list, &format!("{at}/messages"))?;
    }
    let mut set = collect_messages(
        ctx,
        &msgs,
        &reply_id,
        protocol,
        default_content_type,
        read_payload,
    )?;
    let params = parameters(doc, source.value, &address, &source.at)?;
    let mut semantic_hints: Vec<String> = str_of(source.value, "description")
        .map(str::to_owned)
        .into_iter()
        .collect();
    semantic_hints.append(&mut set.hints);
    set.hints = semantic_hints;
    Ok(new_channel(
        reply_id,
        address.clone(),
        address.clone(),
        binding(protocol, &address, source.value.get("bindings"), None, at)?,
        Direction::Publish,
        set,
        params,
    ))
}
