//! The post-write hook: after a 2xx command, builds the event (the
//! contract's payload when it describes one, else the Stripe-shaped
//! envelope) and hands it to the mock's dispatcher, off the response path.

use std::time::{SystemTime, UNIX_EPOCH};

use mb_engine::{GeneratedResponse, MessageContext, OperationCall};
use mb_mcm::{Channel, ChannelBinding, Direction, Operation, WebhookDef};
use mb_state::SessionKey;
use serde_json::{Map, json};

use super::deliver::OutboundEvent;
use super::event::{envelope, envelope_object, event_type, new_event_id};
use crate::MockRuntime;

/// Sends `op`'s webhook event when the mock has targets for it and the
/// call succeeded; returns at once, the event is built and sent by a
/// spawned task.
pub(crate) fn emit(
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
    generated: &GeneratedResponse,
    target_id: Option<&str>,
    session: &SessionKey,
) {
    let Some(dispatcher) = &rt.webhooks else {
        return;
    };
    if !(200..300).contains(&generated.status) {
        return;
    }
    let Some(event_type) = event_type(op) else {
        return;
    };
    let targets = dispatcher.matching(&op.id, &event_type);
    if targets.is_empty() {
        return;
    }
    let dispatcher = dispatcher.clone();
    let bundle = rt.bundle().clone();
    let engine = rt.engine.clone();
    let def = bundle.webhook_for(&op.id, &event_type).cloned();
    let event_id = new_event_id();
    let object = envelope_object(op, generated.body.as_ref(), target_id);
    let request = call.body.clone();
    let response = generated.body.clone();
    let operation = op.id.clone();
    let session = session.session.clone();
    tokio::spawn(async move {
        let fallback = || envelope(&event_id, &event_type, unix_now(), object.clone());
        let payload = match def {
            Some(def) => {
                let ctx = MessageContext {
                    trigger: json!({"kind": "webhook", "operation": operation, "type": event_type}),
                    context: Some(json!({"request": request, "response": response})),
                    received: None,
                    related: vec![],
                };
                match engine
                    .generate_message(&bundle, &webhook_channel(&def), &ctx)
                    .await
                {
                    Ok(m) => m.payload,
                    Err(e) => {
                        tracing::warn!(webhook = %def.name, error = %e, "webhook payload generation failed; sending the envelope");
                        fallback()
                    }
                }
            }
            None => fallback(),
        };
        let body = match serde_json::to_vec(&payload) {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(error = %e, "webhook payload did not serialise");
                return;
            }
        };
        dispatcher.send(
            OutboundEvent {
                event_id,
                event_type,
                operation,
                session,
                body,
            },
            &targets,
        );
    });
}

/// A publish channel standing in for a contract webhook, so the engine
/// generates its payload like any event message.
fn webhook_channel(def: &WebhookDef) -> Channel {
    let id = format!("webhook:{}", def.name);
    Channel {
        title: def.summary.clone().unwrap_or_else(|| def.name.clone()),
        address: id.clone(),
        binding: ChannelBinding::WebSocket { path: id.clone() },
        direction: Direction::Publish,
        message_schema: def.payload_schema.clone(),
        message_names: vec![def.name.clone()],
        content_type: "application/json".into(),
        key_schema: None,
        headers_schema: None,
        parameters: Map::new(),
        triggers: Vec::new(),
        reply: None,
        state_effect: None,
        semantic_hints: def.summary.iter().cloned().collect(),
        id,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
