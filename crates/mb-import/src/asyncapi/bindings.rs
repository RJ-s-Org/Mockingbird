//! AsyncAPI kafka/amqp/mqtt/ws bindings -> [`ChannelBinding`]. Server URLs
//! are never used for connections.

use mb_mcm::ChannelBinding;
use serde_json::Value;

use crate::ImportError;
use crate::openapi3::invalid;

/// The binding keys Mockingbird maps.
const PROTOCOLS: [&str; 4] = ["kafka", "amqp", "mqtt", "ws"];

/// The single broker binding key of a channel's `bindings`, if it has
/// exactly one.
pub(crate) fn binding_protocol(channel_bindings: Option<&Value>) -> Option<&'static str> {
    let bindings = channel_bindings?.as_object()?;
    let mut present = PROTOCOLS.iter().filter(|p| bindings.contains_key(**p));
    match (present.next(), present.next()) {
        (Some(p), None) => Some(p),
        _ => None,
    }
}

fn str_at<'a>(v: Option<&'a Value>, pointer: &str) -> Option<&'a str> {
    v?.pointer(pointer)?.as_str()
}

/// The [`ChannelBinding`] of a channel at `address` for `protocol`
/// (`kafka`, `amqp`, `mqtt` or `ws`). `at` points at the operation.
pub(crate) fn binding(
    protocol: &str,
    address: &str,
    channel_bindings: Option<&Value>,
    op_bindings: Option<&Value>,
    at: &str,
) -> Result<ChannelBinding, ImportError> {
    match protocol {
        "kafka" => Ok(ChannelBinding::Kafka {
            topic: str_at(channel_bindings, "/kafka/topic")
                .unwrap_or(address)
                .to_owned(),
            partitions_hint: channel_bindings
                .and_then(|b| b.pointer("/kafka/partitions"))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok()),
        }),
        "amqp" => {
            let is = str_at(channel_bindings, "/amqp/is").unwrap_or("routingKey");
            match is {
                "routingKey" => Ok(ChannelBinding::Amqp {
                    exchange: str_at(channel_bindings, "/amqp/exchange/name")
                        .unwrap_or_default()
                        .to_owned(),
                    exchange_type: str_at(channel_bindings, "/amqp/exchange/type")
                        .unwrap_or("topic")
                        .to_owned(),
                    routing_key: address.to_owned(),
                    queue: None,
                }),
                "queue" => Ok(ChannelBinding::Amqp {
                    exchange: String::new(),
                    exchange_type: "direct".into(),
                    routing_key: String::new(),
                    queue: Some(
                        str_at(channel_bindings, "/amqp/queue/name")
                            .unwrap_or(address)
                            .to_owned(),
                    ),
                }),
                other => Err(invalid(
                    at,
                    format!("amqp binding `is` must be routingKey or queue, not {other:?}"),
                )),
            }
        }
        "mqtt" => {
            let qos = match op_bindings.and_then(|b| b.pointer("/mqtt/qos")) {
                None => 0,
                Some(v) => v
                    .as_u64()
                    .filter(|q| *q <= 2)
                    .and_then(|q| u8::try_from(q).ok())
                    .ok_or_else(|| {
                        invalid(format!("{at}/bindings/mqtt/qos"), "qos must be 0, 1 or 2")
                    })?,
            };
            let retain = op_bindings
                .and_then(|b| b.pointer("/mqtt/retain"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(ChannelBinding::Mqtt {
                topic: address.to_owned(),
                qos,
                retain,
            })
        }
        "ws" => Ok(ChannelBinding::WebSocket {
            path: if address.starts_with('/') {
                address.to_owned()
            } else {
                format!("/{address}")
            },
        }),
        other => Err(invalid(
            at,
            format!("unsupported broker protocol {other:?}"),
        )),
    }
}
