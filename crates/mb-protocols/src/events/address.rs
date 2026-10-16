//! Channel addresses: `{param}` placeholders, their resolution and
//! matching.

use std::collections::BTreeMap;

use mb_engine::Engine;
use mb_mcm::{Bundle, Channel, ChannelBinding};
use serde_json::Value;

/// One piece of an address template.
#[derive(Debug, PartialEq)]
enum Piece<'a> {
    Literal(&'a str),
    Param(&'a str),
}

/// Split `template` into literal text and `{name}` placeholders. A `{`
/// without a closing `}`, or `{}`, is literal text.
fn pieces(template: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let Some(len) = rest[open + 1..].find('}') else {
            break;
        };
        let name = &rest[open + 1..open + 1 + len];
        if name.is_empty() || name.contains('{') {
            // Not a placeholder: keep the brace as literal text.
            out.push(Piece::Literal(&rest[..open + 1]));
            rest = &rest[open + 1..];
            continue;
        }
        if open > 0 {
            out.push(Piece::Literal(&rest[..open]));
        }
        out.push(Piece::Param(name));
        rest = &rest[open + 2 + len..];
    }
    if !rest.is_empty() {
        out.push(Piece::Literal(rest));
    }
    out
}

/// The placeholder names of `template`, in order of first appearance:
/// `"{a}.x.{b}"` → `["a", "b"]`.
pub fn param_names(template: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for piece in pieces(template) {
        if let Piece::Param(name) = piece
            && !names.iter().any(|n| n == name)
        {
            names.push(name.to_string());
        }
    }
    names
}

fn is_param_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Make `value` safe as one address segment: characters outside
/// `[A-Za-z0-9_-]` become `_`, and `""` becomes `"_"`.
pub fn sanitize_param(value: &str) -> String {
    if value.is_empty() {
        return "_".into();
    }
    value
        .chars()
        .map(|c| if is_param_char(c) { c } else { '_' })
        .collect()
}

/// `template` with each `{p}` that `params` names replaced by its value;
/// the others are left as `{p}`.
fn substitute(template: &str, params: &BTreeMap<String, String>) -> String {
    pieces(template)
        .into_iter()
        .map(|piece| match piece {
            Piece::Literal(text) => text.to_string(),
            Piece::Param(name) => params
                .get(name)
                .cloned()
                .unwrap_or_else(|| format!("{{{name}}}")),
        })
        .collect()
}

/// `binding` with its parameters substituted in the Kafka topic, the AMQP
/// routing key and queue, the MQTT topic and the WebSocket path.
pub fn resolve_binding(
    binding: &ChannelBinding,
    params: &BTreeMap<String, String>,
) -> ChannelBinding {
    match binding {
        ChannelBinding::Kafka {
            topic,
            partitions_hint,
        } => ChannelBinding::Kafka {
            topic: substitute(topic, params),
            partitions_hint: *partitions_hint,
        },
        ChannelBinding::Amqp {
            exchange,
            exchange_type,
            routing_key,
            queue,
        } => ChannelBinding::Amqp {
            exchange: exchange.clone(),
            exchange_type: exchange_type.clone(),
            routing_key: substitute(routing_key, params),
            queue: queue.as_deref().map(|q| substitute(q, params)),
        },
        ChannelBinding::Mqtt { topic, qos, retain } => ChannelBinding::Mqtt {
            topic: substitute(topic, params),
            qos: *qos,
            retain: *retain,
        },
        ChannelBinding::WebSocket { path } => ChannelBinding::WebSocket {
            path: substitute(path, params),
        },
    }
}

/// Whether `actual` is an instance of `template`: literal text matches
/// exactly and each `{p}` matches one or more of `[A-Za-z0-9_-]`.
pub fn address_matches(template: &str, actual: &str) -> bool {
    matches_from(&pieces(template), actual)
}

fn matches_from(pieces: &[Piece<'_>], actual: &str) -> bool {
    let Some((first, rest)) = pieces.split_first() else {
        return actual.is_empty();
    };
    match first {
        Piece::Literal(text) => actual
            .strip_prefix(text)
            .is_some_and(|tail| matches_from(rest, tail)),
        Piece::Param(_) => {
            // The longest run of parameter characters, then backtrack.
            let run = actual
                .char_indices()
                .find(|(_, c)| !is_param_char(*c))
                .map_or(actual.len(), |(i, _)| i);
            (1..=run)
                .rev()
                .any(|len| matches_from(rest, &actual[len..]))
        }
    }
}

/// A value for each parameter of `channel`'s address: the first source
/// object with a string or number field of that name, else a faker value
/// for the parameter's schema, else `"1"`; every value sanitized.
pub fn param_values(
    engine: &Engine,
    bundle: &Bundle,
    channel: &Channel,
    sources: &[&Value],
) -> BTreeMap<String, String> {
    param_names(channel.binding.address())
        .into_iter()
        .map(|name| {
            let value = sources
                .iter()
                .find_map(|source| match source.get(&name)? {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .or_else(|| {
                    let schema = channel.parameters.get(&name)?;
                    Some(match engine.fake(bundle, schema) {
                        Value::String(s) => s,
                        other => other.to_string(),
                    })
                })
                .unwrap_or_else(|| "1".into());
            let value = sanitize_param(&value);
            (name, value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn param_names_and_resolve() {
        assert_eq!(param_names("{a}.x.{b}"), vec!["a", "b"]);
        assert_eq!(param_names("{a}/{a}/{b}"), vec!["a", "b"]);
        assert!(param_names("plain/topic").is_empty());
        assert!(param_names("odd{").is_empty());

        let p = params(&[("id", "42"), ("zone", "north")]);
        let kafka = ChannelBinding::Kafka {
            topic: "lights.{zone}.{id}.on".into(),
            partitions_hint: Some(3),
        };
        assert_eq!(
            resolve_binding(&kafka, &p),
            ChannelBinding::Kafka {
                topic: "lights.north.42.on".into(),
                partitions_hint: Some(3),
            }
        );
        let amqp = ChannelBinding::Amqp {
            exchange: "ex.{id}".into(),
            exchange_type: "topic".into(),
            routing_key: "orders.{id}".into(),
            queue: Some("q.{missing}.{id}".into()),
        };
        assert_eq!(
            resolve_binding(&amqp, &p),
            ChannelBinding::Amqp {
                exchange: "ex.{id}".into(),
                exchange_type: "topic".into(),
                routing_key: "orders.42".into(),
                queue: Some("q.{missing}.42".into()),
            }
        );
        let mqtt = ChannelBinding::Mqtt {
            topic: "s/{id}/t".into(),
            qos: 2,
            retain: true,
        };
        assert_eq!(
            resolve_binding(&mqtt, &p),
            ChannelBinding::Mqtt {
                topic: "s/42/t".into(),
                qos: 2,
                retain: true,
            }
        );
        let ws = ChannelBinding::WebSocket {
            path: "/rooms/{id}".into(),
        };
        assert_eq!(
            resolve_binding(&ws, &p),
            ChannelBinding::WebSocket {
                path: "/rooms/42".into()
            }
        );
    }

    #[test]
    fn sanitize_param_rules() {
        assert_eq!(sanitize_param("a/b+c#"), "a_b_c_");
        assert_eq!(sanitize_param(""), "_");
        assert_eq!(sanitize_param("Ok_id-9"), "Ok_id-9");
        assert_eq!(sanitize_param("lamp 7"), "lamp_7");
        assert_eq!(sanitize_param("é"), "_");
    }

    #[test]
    fn address_matches_segments() {
        assert!(address_matches("s/{id}/t", "s/42/t"));
        assert!(!address_matches("s/{id}/t", "s/4/2/t"));
        assert!(!address_matches("s/{id}/t", "s/42/t/x"));
        assert!(!address_matches("s/{id}/t", "s//t"));
        assert!(address_matches("plain", "plain"));
        assert!(!address_matches("plain", "plainer"));
        assert!(address_matches("a.{x}-{y}.b", "a.one-two.b"));
        assert!(address_matches("{x}", "a_b-c"));
        assert!(!address_matches("{x}", "a.b"));
    }
}
