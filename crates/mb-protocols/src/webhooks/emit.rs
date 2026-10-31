//! The post-write hook: after a 2xx command, builds the event (the
//! contract's payload when it describes one, else the Stripe-shaped
//! envelope) and hands it to the mock's dispatcher, off the response path.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mb_engine::{EffectOutcome, Engine, GeneratedResponse, MessageContext, OperationCall};
use mb_mcm::{Channel, ChannelBinding, Direction, Operation, OperationKind, WebhookDef};
use mb_state::SessionKey;
use serde_json::{Map, json};

use super::deliver::OutboundEvent;
use super::event::{
    envelope, envelope_object, event_type, is_empty_action, new_event_id, read_event_type, singular,
};
use crate::MockRuntime;

/// Sends `op`'s webhook event when the mock has targets for it and the
/// call succeeded; returns at once, the event is built and sent by a
/// spawned task. A contract payload is generated with `engine`, the one
/// the provider policy chose for the call.
pub(crate) fn emit(
    rt: &MockRuntime,
    engine: &Arc<Engine>,
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
    // A read sends only to targets that name it.
    let (event_type, targets) = match event_type(op) {
        Some(t) => {
            let targets = dispatcher.matching(&op.id, &t);
            (t, targets)
        }
        None if op.kind == OperationKind::Query => (read_event_type(op), dispatcher.naming(&op.id)),
        None => return,
    };
    if targets.is_empty() {
        return;
    }
    let dispatcher = dispatcher.clone();
    let bundle = rt.bundle().clone();
    let engine = Arc::clone(engine);
    let def = bundle.webhook_for(&op.id, &event_type).cloned();
    // A bodyless action has nothing to put in the envelope.
    if def.is_none() && is_empty_action(op, generated.body.as_ref()) {
        return;
    }
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

/// Sends `<resource>.updated` for each applied effect of a call to `op`, to
/// the targets matching that event or `op`, with the merged resource as
/// `data.object`. Always the envelope: a contract payload describes what the
/// call itself sends, not a change to another resource.
pub(crate) fn emit_effects(
    rt: &MockRuntime,
    op: &Operation,
    outcomes: &[EffectOutcome],
    session: &SessionKey,
) {
    let Some(dispatcher) = &rt.webhooks else {
        return;
    };
    for outcome in outcomes {
        let Some(merged) = &outcome.merged else {
            continue;
        };
        let event_type = format!("{}.updated", singular(&outcome.effect.resource_type));
        let targets = dispatcher.matching(&op.id, &event_type);
        if targets.is_empty() {
            continue;
        }
        let event_id = new_event_id();
        let payload = envelope(&event_id, &event_type, unix_now(), merged.clone());
        let body = match serde_json::to_vec(&payload) {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(error = %e, "webhook payload did not serialise");
                continue;
            }
        };
        dispatcher.send(
            OutboundEvent {
                event_id,
                event_type,
                operation: op.id.clone(),
                session: session.session.clone(),
                body,
            },
            &targets,
        );
    }
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
