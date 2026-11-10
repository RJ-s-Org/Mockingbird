//! Message-queue actions: an action whose description gives it a `queue` puts a message
//! on its held target or takes messages off it, first in first out, with no LLM call.
//!
//! - `effect: append` puts the request's record (its body, or at `request.record`) last
//!   on the target's messages, and answers as the operation answers a write of the
//!   target ([`render`]: an acknowledgement's booleans `true`, a publish's `routed`).
//! - `effect: take` answers the first messages, oldest first: as many as the request
//!   body's `count` member asks (one when absent or not a positive number), at most as
//!   many as are held, as an array (at `answer.records` when described, else the
//!   answer itself). It removes them, unless the request body's `requeue.at` member
//!   holds the `requeue` value (or one of its values): then they stay at the head, and
//!   a later take answers them with the `redelivered` member `true` (`false` on a
//!   message never taken before).
//!
//! The messages are kept per session, beside the target, in one document under the
//! store's reserved type ([`mb_state::INTERNAL_TYPE`]); a target holds at most
//! [`MAX_MESSAGES`], of [`MAX_BYTES`] at most, and they go when the target is deleted
//! ([`forget`]). Either falls back, and the action is generated, on an unheld target
//! (`not_held`), an append past either cap (`over_cap`), or an answer that does not
//! render (`render_failed`); nothing is kept then.

use mb_mcm::world::{OpKind, QueueDesc, QueueEffect};
use mb_state::{INTERNAL_TYPE, StoredResource};
use serde_json::{Map, Value, json};
use tracing::Instrument;

use super::{Fallback, Held, Ran, answered, fell_back, render};
use crate::call::{CallPlan, OperationCall, StateAccess, lifecycle, records, replays, state_span};
use mb_state::SessionKey;

/// Messages a target holds, at most.
pub(super) const MAX_MESSAGES: usize = 1000;

/// Bytes a target's messages take, as stored JSON, at most (1 MiB).
pub(super) const MAX_BYTES: usize = 1 << 20;

/// The prefix of a target's messages document.
const PREFIX: &str = "#queue/";

/// Executes the call (see the module's doc).
pub(super) async fn run(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: Held<'_>,
    response_schema: Option<&Value>,
    queue: &QueueDesc,
) -> Ran {
    let rt = state.rt;
    let kind = OpKind::Action;
    let (Some(stored), Some(id)) = (held.target, held.target_id) else {
        fell_back(rt, plan, kind, Fallback::NotHeld.as_str());
        return Ran::Generate(None);
    };
    let key = key(&stored.resource_type, id);
    let mut messages = read(state, plan, &key).await;
    let answer = match queue.effect {
        QueueEffect::Append => {
            let body = call.body.clone().unwrap_or(Value::Object(Map::new()));
            let record = records::request_record(rt, plan.op, &body).clone();
            messages.push(json!({"record": record, "taken": false}));
            let bytes = serde_json::to_vec(&messages).map_or(usize::MAX, |b| b.len());
            if messages.len() > MAX_MESSAGES || bytes > MAX_BYTES {
                fell_back(rt, plan, kind, Fallback::OverCap.as_str());
                return Ran::Generate(None);
            }
            render(rt, plan, call, (id, &stored.data), response_schema)
        }
        QueueEffect::Take => {
            let taken = messages.len().min(count(queue, call));
            let requeued = requeues(queue, call);
            let answered: Vec<Value> = messages[..taken].iter().map(|m| shown(queue, m)).collect();
            if requeued {
                for message in &mut messages[..taken] {
                    message["taken"] = Value::Bool(true);
                }
            } else {
                messages.drain(..taken);
            }
            take_answer(rt, plan, answered, response_schema)
        }
    };
    let Some(answer) = answer else {
        fell_back(rt, plan, kind, Fallback::RenderFailed.as_str());
        return Ran::Generate(None);
    };
    async {
        let doc = StoredResource {
            resource_type: INTERNAL_TYPE.to_string(),
            id: key,
            data: json!({"messages": messages}),
        };
        state.upsert(&plan.session, doc).await;
        replays::keep(state, plan, call, &answer).await;
        if let Some(cache) = rt.state.response_cache() {
            cache.clear_session(&plan.session);
        }
        state.touch(&plan.session).await;
    }
    .instrument(state_span())
    .await;
    answered(rt, plan, kind);
    Ran::Answered(Box::new(answer))
}

/// The document holding the messages of record `id` of type `t`.
fn key(t: &str, id: &str) -> String {
    format!("{PREFIX}{t}/{id}")
}

/// Forgets the messages record `id` of type `t` held: it was deleted, so a record made
/// again under its id starts with none.
pub(in crate::call) async fn forget(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    id: &str,
) {
    if state.rt.world().is_none() {
        return;
    }
    state.delete(session, INTERNAL_TYPE, &key(t, id)).await;
}

/// The messages the document `key` holds, oldest first.
async fn read(state: &mut StateAccess<'_>, plan: &CallPlan<'_>, key: &str) -> Vec<Value> {
    state
        .get(&plan.session, INTERNAL_TYPE, key)
        .instrument(state_span())
        .await
        .and_then(|doc| doc.data.get("messages")?.as_array().cloned())
        .unwrap_or_default()
}

/// How many messages `call` asks to take: the request body's `count` member as a
/// positive whole number (or its text), else one.
fn count(queue: &QueueDesc, call: &OperationCall) -> usize {
    queue
        .count
        .as_ref()
        .and_then(|at| call.body.as_ref()?.pointer(at.as_str()))
        .and_then(|n| match n {
            Value::Number(n) => n.as_u64(),
            Value::String(text) => text.trim().parse().ok(),
            _ => None,
        })
        .filter(|n| *n > 0)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(1)
}

/// Whether `call` asks the messages it takes to stay on the queue: its body's
/// `requeue.at` member holds the value, or one of the values, `requeue` gives.
fn requeues(queue: &QueueDesc, call: &OperationCall) -> bool {
    let Some(requeue) = &queue.requeue else {
        return false;
    };
    let Some(sent) = call
        .body
        .as_ref()
        .and_then(|b| b.pointer(requeue.at.as_str()))
    else {
        return false;
    };
    match &requeue.value {
        Value::Array(values) => values.iter().any(|v| lifecycle::sends_value(sent, v)),
        one => lifecycle::sends_value(sent, one),
    }
}

/// A held message as a take answers it: its record, with the `redelivered` member
/// saying whether it was taken before.
fn shown(queue: &QueueDesc, message: &Value) -> Value {
    let mut record = message.get("record").cloned().unwrap_or(Value::Null);
    if let (Some(member), Value::Object(_)) = (&queue.redelivered, &record) {
        let before = message
            .get("taken")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        mb_mcm::set_member_at(&mut record, member, Value::Bool(before));
    }
    record
}

/// A take's answer: the messages as an array, at the described `answer.records`
/// pointer, else the answer itself; `None` when it does not suit the schema.
fn take_answer(
    rt: &crate::MockRuntime,
    plan: &CallPlan<'_>,
    messages: Vec<Value>,
    response_schema: Option<&Value>,
) -> Option<mb_engine::GeneratedResponse> {
    let op = plan.op;
    let at = rt
        .described()
        .op(&op.id)
        .and_then(|d| d.answer.as_ref())
        .and_then(|a| a.records.as_ref())
        .map(|p| p.tokens())
        .unwrap_or_default();
    let body = at
        .iter()
        .rev()
        .fold(Value::Array(messages), |inner, token| {
            let mut envelope = Map::new();
            envelope.insert(token.clone(), inner);
            Value::Object(envelope)
        });
    rt.engine
        .serve_stored(rt.bundle(), op, &body, response_schema)
}
