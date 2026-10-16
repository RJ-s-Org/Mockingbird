//! AsyncAPI 2.0–2.6 -> MCM channels.

use mb_mcm::{Bundle, Channel, Direction, Reply};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{
    Ctx, address_params, apply_traits, binding, bundle_info, channel_protocol, check_replies,
    escape, extensions, finish_defs, headers_schema, message_payload, normalize, one_of, ref_name,
    resolve, slug,
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

/// One message of an operation, with its traits merged.
struct Message {
    value: Value,
    ref_name: Option<String>,
    at: String,
}

/// The operation's messages: one message, `{oneOf: [...]}` or a `$ref` to
/// either.
fn messages(doc: &Value, op: &Value, op_at: &str) -> Result<Vec<Message>, ImportError> {
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
        .map(|(value, ref_name, at)| {
            Ok(Message {
                value: apply_traits(doc, value, &at)?,
                ref_name,
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

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
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

            let mut message_schemas = Vec::new();
            let mut message_names = Vec::new();
            let mut headers = Vec::new();
            let mut key_schema = None;
            let mut semantic_hints: Vec<String> =
                [str_of(item, "description"), str_of(&op, "description")]
                    .into_iter()
                    .flatten()
                    .map(str::to_owned)
                    .collect();
            if let Some(tags) = op.get("tags").and_then(Value::as_array) {
                semantic_hints.extend(
                    tags.iter()
                        .filter_map(|t| str_of(t, "name"))
                        .map(str::to_owned),
                );
            }
            let msgs = messages(doc, &op, &op_at)?;
            for (n, m) in msgs.iter().enumerate() {
                let v = &m.value;
                message_schemas.push(message_payload(
                    &mut ctx,
                    v,
                    v.get("schemaFormat").and_then(Value::as_str),
                    &m.at,
                )?);
                message_names.push(
                    str_of(v, "name")
                        .or_else(|| str_of(v, "messageId"))
                        .map(str::to_owned)
                        .or_else(|| m.ref_name.clone())
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
                semantic_hints.extend(
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
                message_schema: one_of(message_schemas),
                message_names,
                content_type,
                key_schema,
                headers_schema: headers_schema(headers),
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

    let digest = Sha256::digest(bytes);
    Ok(Bundle {
        id: slug(&title),
        title,
        version,
        source_digest: digest.iter().map(|b| format!("{b:02x}")).collect(),
        operations: Vec::new(),
        defs,
        protocol_payload: None,
        channels,
    })
}
