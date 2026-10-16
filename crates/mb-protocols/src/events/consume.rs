//! Consuming messages: the per-channel subscription loop and the handling
//! of each received message (validation, recording, state, replies).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mb_mcm::Channel;
use mb_state::{SessionKey, StoredResource};
use serde_json::{Map, Value, json};

use super::produce::find_header;
use super::runtime::{ChannelInner, DEFAULT_SESSION, Flow, Origin, PublishError, WORKSPACE};
use super::{BrokerError, HEADER_HOP, InMessage, MAX_HOPS, MAX_MESSAGE_BYTES};
use crate::ws::Outbound;
use crate::{RequestKind, RequestRecord};

/// How long subscribing may take.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);
/// The first pause before resubscribing.
const BACKOFF_START: Duration = Duration::from_secs(1);
/// The longest pause before resubscribing.
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Bytes of an unparsable payload kept in its record.
const RAW_PREVIEW_BYTES: usize = 1024;
const SESSION_HEADER: &str = "x-mockingbird-session";
const MAX_SESSION_LEN: usize = 128;

/// Consume `channel` from `broker` until the process shuts down,
/// resubscribing with backoff after errors.
pub(crate) async fn run_consumer(inner: Arc<ChannelInner>, channel: Channel, broker: String) {
    let stopped = inner.rt.shutdown.stopped();
    tokio::pin!(stopped);
    let group = format!("mockingbird-{}", inner.rt.name);
    let brokers = Arc::clone(&inner.deps.brokers);
    let mut backoff = BACKOFF_START;
    loop {
        let subscribed = match brokers.client(&broker).await {
            Ok(client) => {
                match tokio::time::timeout(
                    SUBSCRIBE_TIMEOUT,
                    client.subscribe(&channel.binding, &group),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(BrokerError::Subscribe(format!(
                        "timed out after {}s",
                        SUBSCRIBE_TIMEOUT.as_secs()
                    ))),
                }
            }
            Err(e) => Err(e),
        };
        let error = match subscribed {
            Ok(mut stream) => {
                brokers.report(&broker, Ok(()));
                inner.set_consumer_error(&channel.id, None);
                backoff = BACKOFF_START;
                loop {
                    tokio::select! {
                        _ = &mut stopped => return,
                        next = stream.next() => match next {
                            Some(Ok(msg)) => {
                                inner.receive(&channel, msg).await;
                            }
                            Some(Err(e)) => break e,
                            None => break BrokerError::Subscribe("subscription ended".into()),
                        },
                    }
                }
            }
            Err(e) => e,
        };
        tracing::warn!(mock = %inner.rt.name, channel = %channel.id, error = %error, "consumer failed; resubscribing");
        brokers.report(&broker, Err(&error));
        inner.set_consumer_error(&channel.id, Some(error.to_string()));
        tokio::select! {
            _ = &mut stopped => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// 1-128 characters of `[A-Za-z0-9._-]`.
fn is_session_id(s: &str) -> bool {
    (1..=MAX_SESSION_LEN).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A string or number field as an id.
fn id_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl ChannelInner {
    /// Handle one message received on `channel`: validate and record it,
    /// then for a valid one apply the state effect and send the reply.
    /// Returns the payload when it is valid. Never panics on input.
    pub(crate) async fn receive(
        self: &Arc<Self>,
        channel: &Channel,
        msg: InMessage,
    ) -> Option<Value> {
        self.receive_from(channel, msg, None).await
    }

    /// [`Self::receive`] for a message from a WebSocket client: a reply on
    /// a WebSocket channel goes to `socket` only, not to every subscriber.
    pub(crate) async fn receive_from(
        self: &Arc<Self>,
        channel: &Channel,
        msg: InMessage,
        socket: Option<&tokio::sync::mpsc::Sender<Outbound>>,
    ) -> Option<Value> {
        let started = Instant::now();
        let bytes = msg.payload.len();
        let headers: Map<String, Value> = msg
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let key = msg
            .key
            .as_deref()
            .map(|k| String::from_utf8_lossy(k).into_owned());
        let session = find_header(&msg.headers, SESSION_HEADER)
            .map(str::trim)
            .filter(|s| is_session_id(s))
            .unwrap_or(DEFAULT_SESSION)
            .to_string();

        let (status, body, mut response, valid) = if bytes > MAX_MESSAGE_BYTES {
            (413, json!({"truncated": true, "bytes": bytes}), None, None)
        } else {
            match serde_json::from_slice::<Value>(&msg.payload) {
                Err(_) => {
                    let preview = &msg.payload[..bytes.min(RAW_PREVIEW_BYTES)];
                    let raw = String::from_utf8_lossy(preview).into_owned();
                    (400, json!({"raw": raw}), None, None)
                }
                Ok(payload) => {
                    match self
                        .rt
                        .engine
                        .validate_message(&self.rt.bundle, channel, &payload)
                    {
                        Err(errors) => (
                            422,
                            payload,
                            Some(json!({"valid": false, "errors": errors})),
                            None,
                        ),
                        Ok(()) => (
                            200,
                            payload.clone(),
                            Some(json!({"valid": true})),
                            Some(payload),
                        ),
                    }
                }
            }
        };

        if let (Some(payload), Some(Value::Object(response))) = (&valid, response.as_mut()) {
            if let Some(outcome) = self.apply_state_effect(channel, &session, payload).await {
                response.insert("state".into(), Value::String(outcome));
            }
            if let Some(outcome) = self.reply(channel, payload, &msg.headers, socket).await {
                response.insert("reply".into(), Value::String(outcome));
            }
        }

        if let Some(recorder) = &self.rt.recorder {
            recorder.record(RequestRecord {
                mock: self.rt.name.clone(),
                operation_id: Some(channel.id.clone()),
                session,
                method: "RECEIVE".into(),
                path: msg.address.clone(),
                status,
                source: None,
                latency: started.elapsed(),
                prompt_tokens: 0,
                completion_tokens: 0,
                request: json!({"key": key, "headers": headers, "body": body}),
                request_body_bytes: Some(bytes),
                response,
                kind: RequestKind::Event,
            });
        }
        let outcome = match status {
            200 => Ok(()),
            413 => Err(format!(
                "received message is {bytes} bytes, over the 1 MiB limit"
            )),
            400 => Err("received message is not JSON".to_string()),
            _ => Err("received message does not match the schema".to_string()),
        };
        self.note(
            &channel.id,
            Flow::Receive,
            outcome.as_ref().map(|_| ()).map_err(String::as_str),
        );
        valid
    }

    /// Upsert or delete the resource a valid message names. `Some` carries
    /// a note for the record when nothing was applied.
    async fn apply_state_effect(
        &self,
        channel: &Channel,
        session: &str,
        payload: &Value,
    ) -> Option<String> {
        let effect = channel.state_effect.as_ref()?;
        let Some(id) = id_text(payload.get(&effect.id_field)) else {
            return Some(format!("skipped: no {}", effect.id_field));
        };
        let key = SessionKey {
            workspace: WORKSPACE.into(),
            mock: self.rt.name.clone(),
            session: session.to_string(),
        };
        let result = if effect.delete {
            self.rt
                .state
                .delete(&key, &effect.resource_type, &id)
                .await
                .map(|_| ())
        } else {
            self.rt
                .state
                .upsert(
                    &key,
                    StoredResource {
                        resource_type: effect.resource_type.clone(),
                        id,
                        data: payload.clone(),
                    },
                )
                .await
        };
        match result {
            Ok(()) => None,
            Err(e) => {
                tracing::warn!(channel = %channel.id, error = %e, "state effect failed");
                Some(format!("error: {e}"))
            }
        }
    }

    /// Send the reply to a valid message, unless the hop limit is reached.
    /// `Some` carries a note for the record when no reply was sent.
    async fn reply(
        self: &Arc<Self>,
        channel: &Channel,
        payload: &Value,
        headers: &[(String, String)],
        socket: Option<&tokio::sync::mpsc::Sender<Outbound>>,
    ) -> Option<String> {
        let reply = channel.reply.as_ref()?;
        let hop = find_header(headers, HEADER_HOP)
            .and_then(|h| h.trim().parse::<u32>().ok())
            .unwrap_or(0);
        if hop >= MAX_HOPS {
            return Some("skipped: hop limit".into());
        }
        let Some(target) = self.rt.bundle.channel(&reply.channel) else {
            return Some(format!(
                "error: {}",
                PublishError::UnknownChannel(reply.channel.clone())
            ));
        };
        if !self.enabled(target) {
            return Some(format!(
                "error: {}",
                PublishError::Disabled(reply.channel.clone())
            ));
        }
        let origin = Origin::Reply {
            received: payload.clone(),
            headers: headers.to_vec(),
            hop,
        };
        match self.produce_to(target, origin, Vec::new(), socket).await {
            Ok(_) => None,
            Err(e) => Some(format!("error: {e}")),
        }
    }
}
