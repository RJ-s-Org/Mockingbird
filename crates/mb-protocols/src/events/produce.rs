//! Producing one message: generation, key, headers, delivery with retry,
//! and the request record.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use mb_engine::{GeneratedMessage, MessageContext, Source};
use mb_ext::{CallInfo, UsageEvent};
use mb_mcm::{Channel, ChannelBinding};
use mb_state::StoredResource;
use serde_json::{Value, json};

use super::runtime::{ChannelInner, Flow, Origin, PublishError, Published};
use super::{
    BrokerError, HEADER_HOP, HEADER_MOCK, HEADER_SOURCE, MAX_MESSAGE_BYTES, OutMessage,
    param_values, resolve_binding,
};
use crate::{RequestKind, RequestRecord};

/// How long one publish attempt may take.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
/// The pauses before each retry of a failed publish.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

/// `name` lowercased with `-` and `_` removed contains `correlationid`.
fn is_correlation_header(name: &str) -> bool {
    name.to_lowercase()
        .replace(['-', '_'], "")
        .contains("correlationid")
}

/// A JSON value as header or key text: strings as-is, others as JSON.
fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A string or number as text.
fn id_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The value of header `name` (case-insensitive) among `headers`.
pub(crate) fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// `binding` redirected to `address`: the Kafka or MQTT topic, or the
/// AMQP default exchange with `address` as the routing key.
fn redirect(binding: ChannelBinding, address: &str) -> ChannelBinding {
    match binding {
        ChannelBinding::Kafka {
            partitions_hint, ..
        } => ChannelBinding::Kafka {
            topic: address.to_string(),
            partitions_hint,
        },
        ChannelBinding::Mqtt { qos, retain, .. } => ChannelBinding::Mqtt {
            topic: address.to_string(),
            qos,
            retain,
        },
        ChannelBinding::Amqp { .. } => ChannelBinding::Amqp {
            exchange: String::new(),
            exchange_type: "direct".into(),
            routing_key: address.to_string(),
            queue: None,
        },
        ws @ ChannelBinding::WebSocket { .. } => ws,
    }
}

/// One publish to report.
struct PublishReport<'a> {
    channel: &'a Channel,
    session: String,
    address: String,
    status: u16,
    generated: &'a GeneratedMessage,
    started: Instant,
    request: Value,
    error: Option<String>,
}

impl ChannelInner {
    /// Generate one message for the publish channel `channel` and deliver
    /// it, recording the outcome.
    pub(crate) async fn produce(
        self: &Arc<Self>,
        channel: &Channel,
        origin: Origin,
        related: Vec<StoredResource>,
    ) -> Result<Published, PublishError> {
        let started = Instant::now();
        let broker = match &channel.binding {
            ChannelBinding::WebSocket { .. } => None,
            _ => {
                if let Some(reason) = self.inactive_reason(&channel.id) {
                    return Err(PublishError::NoBroker(reason.to_string()));
                }
                match &self.broker {
                    Some(b) => Some(b.clone()),
                    None => return Err(PublishError::NoBroker("no broker configured".into())),
                }
            }
        };
        let session = origin.session();
        let (trigger, context, received, incoming, hop) = match origin {
            Origin::Trigger(trigger) => (trigger, None, None, Vec::new(), 0),
            Origin::Manual(context) => (json!({"kind": "manual"}), context, None, Vec::new(), 0),
            Origin::Reply {
                received,
                headers,
                hop,
            } => (
                json!({"kind": "reply", "hop": hop}),
                None,
                Some(received),
                headers,
                hop.saturating_add(1),
            ),
        };
        let ctx = MessageContext {
            trigger,
            context,
            received,
            related,
        };

        let engine = &self.rt.engine;
        let bundle = &self.rt.bundle;
        let generated = match engine.generate_message(bundle, channel, &ctx).await {
            Ok(generated) => generated,
            Err(e) => {
                tracing::warn!(channel = %channel.id, error = %e, "message generation failed; faking");
                GeneratedMessage {
                    payload: engine.fake(bundle, &channel.message_schema),
                    source: Source::Fallback,
                    usage: Default::default(),
                }
            }
        };

        let target_data = ctx.related.first().map(|r| &r.data);
        let sources: Vec<&Value> = [ctx.context.as_ref(), target_data, ctx.received.as_ref()]
            .into_iter()
            .flatten()
            .collect();
        let params = param_values(engine, bundle, channel, &sources);
        let state_change_id = (ctx.trigger.get("kind").and_then(Value::as_str)
            == Some("on_state_change"))
        .then(|| id_text(ctx.trigger.get("id")))
        .flatten();
        let key = state_change_id
            .or_else(|| id_text(ctx.context.as_ref().and_then(|c| c.get("key"))))
            .or_else(|| {
                let schema = channel.key_schema.as_ref()?;
                Some(text(&engine.fake(bundle, schema)))
            });

        let mut headers = BTreeMap::new();
        if let Some(schema) = &channel.headers_schema
            && let Value::Object(fields) = engine.fake(bundle, schema)
        {
            for (name, value) in fields {
                headers.insert(name, text(&value));
            }
        }
        for (name, value) in &incoming {
            if is_correlation_header(name) {
                headers.insert(name.clone(), value.clone());
            }
        }
        headers.insert(HEADER_SOURCE.into(), generated.source.as_str().into());
        headers.insert(HEADER_MOCK.into(), self.rt.name.clone());
        headers.insert(HEADER_HOP.into(), hop.to_string());

        let mut target = resolve_binding(&channel.binding, &params);
        let redirect_to = self
            .rt
            .bundle
            .channels
            .iter()
            .filter_map(|c| c.reply.as_ref())
            .filter(|r| r.channel == channel.id)
            .filter_map(|r| r.address_header.as_deref())
            .find_map(|name| find_header(&incoming, name));
        if let Some(address) = redirect_to {
            target = redirect(target, address);
        }
        let address = target.address().to_string();
        let request = json!({"trigger": ctx.trigger, "key": key, "headers": headers});

        let payload = serde_json::to_vec(&generated.payload).unwrap_or_default();
        let mut report = PublishReport {
            channel,
            session,
            address: address.clone(),
            status: 200,
            generated: &generated,
            started,
            request,
            error: None,
        };
        if payload.len() > MAX_MESSAGE_BYTES {
            let error = PublishError::TooLarge(payload.len());
            report.status = 413;
            report.error = Some(error.to_string());
            self.report_publish(report).await;
            return Err(error);
        }

        let outcome = match (&broker, &channel.binding) {
            (_, ChannelBinding::WebSocket { path }) => {
                self.hub
                    .send(path, String::from_utf8_lossy(&payload).into_owned());
                Ok(())
            }
            (Some(broker), _) => {
                let msg = OutMessage {
                    key: key.clone().map(String::into_bytes),
                    headers: headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    payload,
                    content_type: channel.content_type.clone(),
                };
                self.deliver(broker, &target, msg).await
            }
            (None, _) => Err(BrokerError::Publish("no broker configured".into())),
        };
        if let Err(e) = outcome {
            let error = PublishError::Broker(e.to_string());
            report.status = 503;
            report.error = Some(error.to_string());
            self.report_publish(report).await;
            return Err(error);
        }
        self.report_publish(report).await;
        Ok(Published {
            channel: channel.id.clone(),
            address,
            key,
            headers,
            payload: generated.payload,
            source: generated.source,
        })
    }

    /// Publish `msg` on `broker`, each attempt bounded by 10 s, retrying
    /// after 200, 400 and 800 ms. Every attempt's outcome is reported.
    async fn deliver(
        &self,
        broker: &str,
        target: &ChannelBinding,
        msg: OutMessage,
    ) -> Result<(), BrokerError> {
        let brokers = &self.deps.brokers;
        let mut retries = RETRY_DELAYS.iter();
        loop {
            let result = match brokers.client(broker).await {
                Ok(client) => {
                    match tokio::time::timeout(PUBLISH_TIMEOUT, client.publish(target, msg.clone()))
                        .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(BrokerError::Publish(format!(
                            "timed out after {}s",
                            PUBLISH_TIMEOUT.as_secs()
                        ))),
                    }
                }
                Err(e) => Err(e),
            };
            brokers.report(broker, result.as_ref().map(|_| ()));
            match result {
                Ok(()) => return Ok(()),
                Err(e) => match retries.next() {
                    Some(delay) => {
                        tracing::debug!(%broker, error = %e, "publish failed; retrying");
                        tokio::time::sleep(*delay).await;
                    }
                    None => return Err(e),
                },
            }
        }
    }

    /// Record a publish: usage, the request record, activity and metrics.
    async fn report_publish(&self, r: PublishReport<'_>) {
        let rt = &self.rt;
        let latency = r.started.elapsed();
        let empty = HeaderMap::new();
        let call_info = CallInfo {
            mock: &rt.name,
            operation_id: &r.channel.id,
            headers: &empty,
        };
        if let Ok(tenancy) = rt.ext.tenants.resolve(&call_info).await {
            rt.ext.usage.record(&UsageEvent {
                tenancy,
                mock: rt.name.clone(),
                operation_id: r.channel.id.clone(),
                status: r.status,
                source: r.generated.source.as_str().to_string(),
                latency,
                prompt_tokens: r.generated.usage.prompt_tokens,
                completion_tokens: r.generated.usage.completion_tokens,
            });
        }
        if let Some(recorder) = &rt.recorder {
            recorder.record(RequestRecord {
                mock: rt.name.clone(),
                operation_id: Some(r.channel.id.clone()),
                session: r.session,
                method: "PUBLISH".into(),
                path: r.address,
                status: r.status,
                source: Some(r.generated.source.as_str().to_string()),
                latency,
                prompt_tokens: r.generated.usage.prompt_tokens,
                completion_tokens: r.generated.usage.completion_tokens,
                request: r.request,
                request_body_bytes: None,
                // An oversized payload is not kept.
                response: (r.status != 413).then(|| r.generated.payload.clone()),
                kind: RequestKind::Event,
            });
        }
        self.note(
            &r.channel.id,
            Flow::Publish,
            r.error.as_deref().map_or(Ok(()), Err),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlation_header_names() {
        assert!(is_correlation_header("correlation_id"));
        assert!(is_correlation_header("X-Correlation-Id"));
        assert!(is_correlation_header("correlationId"));
        assert!(!is_correlation_header("correlation"));
        assert!(!is_correlation_header("x-request-id"));
    }

    #[test]
    fn redirect_targets() {
        let amqp = ChannelBinding::Amqp {
            exchange: "orders".into(),
            exchange_type: "topic".into(),
            routing_key: "orders.cancelled".into(),
            queue: Some("q".into()),
        };
        assert_eq!(
            redirect(amqp, "reply-to"),
            ChannelBinding::Amqp {
                exchange: String::new(),
                exchange_type: "direct".into(),
                routing_key: "reply-to".into(),
                queue: None,
            }
        );
        let kafka = ChannelBinding::Kafka {
            topic: "a".into(),
            partitions_hint: Some(2),
        };
        assert_eq!(redirect(kafka, "b").address(), "b");
        let mqtt = ChannelBinding::Mqtt {
            topic: "a".into(),
            qos: 1,
            retain: false,
        };
        assert_eq!(redirect(mqtt, "b/c").address(), "b/c");
    }
}
