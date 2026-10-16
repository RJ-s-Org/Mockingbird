//! AsyncAPI 2.0–2.6 -> MCM channels.

use mb_mcm::{Bundle, Channel, Direction, Reply};
use serde_json::{Map, Value, json};

use super::{
    Ctx, MessageInput, address_params, apply_traits, binding, bundle_info, channel_protocol,
    check_replies, collect_messages, digest, escape, extensions, finish_defs, message_payload,
    normalize, ref_name, resolve, slug, str_of, tag_names,
};
use crate::ImportError;

/// The 2.x operation keys and the Mockingbird direction of each: the
/// application `subscribe`s to what the mock publishes.
const OPERATIONS: [(&str, Direction); 2] = [
    ("subscribe", Direction::Publish),
    ("publish", Direction::Subscribe),
];

fn direction_label(direction: Direction) -> &'static str {
    match direction {
        Direction::Publish => "publish",
        Direction::Subscribe => "subscribe",
    }
}

/// The operation's messages: one message, `{oneOf: [...]}` or a `$ref` to
/// either.
fn messages(doc: &Value, op: &Value, op_at: &str) -> Result<Vec<MessageInput>, ImportError> {
    let Some(raw) = op.get("message") else {
        return Ok(Vec::new());
    };
    let at = format!("{op_at}/message");
    let resolved = resolve(doc, raw, &at)?;
    let items: Vec<(&Value, Option<String>, String)> =
        match resolved.get("oneOf").and_then(Value::as_array) {
            Some(list) => list
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let item_at = format!("{at}/oneOf/{i}");
                    Ok((resolve(doc, item, &item_at)?, ref_name(item), item_at))
                })
                .collect::<Result<_, ImportError>>()?,
            None => vec![(resolved, ref_name(raw), at)],
        };
    items
        .into_iter()
        .map(|(value, fallback_name, at)| {
            Ok(MessageInput {
                value: apply_traits(doc, value, &at)?,
                fallback_name,
                at,
            })
        })
        .collect()
}

/// Address parameter schemas: the declared ones, plus `{"type":"string"}`
/// for each undeclared `{name}` of the address.
fn parameters(
    doc: &Value,
    item: &Value,
    address: &str,
    at: &str,
) -> Result<Map<String, Value>, ImportError> {
    let mut out = Map::new();
    if let Some(params) = item.get("parameters").and_then(Value::as_object) {
        for (name, p) in params {
            let p_at = format!("{at}/parameters/{}", escape(name));
            let p = resolve(doc, p, &p_at)?;
            let schema = match p.get("schema") {
                Some(s) => normalize(s, &format!("{p_at}/schema"))?,
                None => json!({"type": "string"}),
            };
            out.insert(name.clone(), schema);
        }
    }
    for name in address_params(address) {
        out.entry(name.to_owned())
            .or_insert_with(|| json!({"type": "string"}));
    }
    Ok(out)
}

/// A 2.x message payload, read per the message's `schemaFormat`.
fn read_payload(ctx: &mut Ctx, message: &Value, at: &str) -> Result<Value, ImportError> {
    message_payload(
        ctx,
        message,
        message.get("schemaFormat").and_then(Value::as_str),
        at,
    )
}

pub(crate) fn compile_v2(doc: &Value, bytes: &[u8]) -> Result<Bundle, ImportError> {
    let (title, version) = bundle_info(doc)?;
    let mut ctx = Ctx::new(doc)?;
    let default_content_type = str_of(doc, "defaultContentType").unwrap_or("application/json");
    let mut channels: Vec<Channel> = Vec::new();
    let mut replies: Vec<(usize, String)> = Vec::new();
    let empty = Map::new();
    let items = doc
        .get("channels")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for (address, item) in items {
        let at = format!("/channels/{}", escape(address));
        let item = resolve(doc, item, &at)?;
        if OPERATIONS.iter().all(|(key, _)| item.get(key).is_none()) {
            continue;
        }
        let channel_servers: Option<Vec<String>> =
            item.get("servers").and_then(Value::as_array).map(|s| {
                s.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            });
        let channel_bindings = item.get("bindings");
        let protocol = channel_protocol(doc, channel_bindings, channel_servers.as_deref(), &at)?;
        let params = parameters(doc, item, address, &at)?;
        let channel_ext = extensions(item, &ctx, &at)?;

        for (key, direction) in OPERATIONS {
            let Some(op) = item.get(key) else { continue };
            let op_at = format!("{at}/{key}");
            let op = apply_traits(doc, resolve(doc, op, &op_at)?, &op_at)?;
            let id = match str_of(&op, "operationId") {
                Some(id) => id.to_owned(),
                None => format!("{}-{}", direction_label(direction), slug(address)),
            };
            ctx.claim_id(&id, format!("{op_at}/operationId"))?;

            let mut semantic_hints: Vec<String> =
                [str_of(item, "description"), str_of(&op, "description")]
                    .into_iter()
                    .flatten()
                    .map(str::to_owned)
                    .collect();
            semantic_hints.extend(tag_names(&op));
            let msgs = messages(doc, &op, &op_at)?;
            let set = collect_messages(
                &mut ctx,
                &msgs,
                &id,
                protocol,
                default_content_type,
                read_payload,
            )?;
            semantic_hints.extend(set.hints);

            let ext = extensions(&op, &ctx, &op_at)?.or(channel_ext.clone());
            ext.check(direction)?;
            if let Some((_, reply_at)) = &ext.reply {
                replies.push((channels.len(), reply_at.clone()));
            }
            channels.push(Channel {
                id,
                title: str_of(&op, "summary").unwrap_or(address).to_owned(),
                address: address.clone(),
                binding: binding(
                    protocol,
                    address,
                    channel_bindings,
                    op.get("bindings"),
                    &op_at,
                )?,
                direction,
                message_schema: set.message_schema,
                message_names: set.message_names,
                content_type: set.content_type,
                key_schema: set.key_schema,
                headers_schema: set.headers_schema,
                parameters: params.clone(),
                triggers: ext.triggers.map(|(t, _)| t).unwrap_or_default(),
                reply: ext.reply.map(|(channel, _)| Reply {
                    channel,
                    address_header: None,
                }),
                state_effect: ext.state.map(|(s, _)| s),
                semantic_hints,
            });
        }
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
    })
}
