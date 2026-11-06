//! A create answered as a success although it repeats a value the contract says is
//! unique: refused from state, whatever the LLM answered.
//!
//! A create shown a stored record that holds a value of its request under the same
//! member ([`super::value_index::same_values`]) is offered the declared refusal, and
//! the model decides. Models often ignore the offer and answer `201`, so a second
//! product takes a taken SKU. When the contract's evidence is strong and structural the
//! refusal is enforced, and the success is replaced by the declared error:
//!
//! - the operation is a create, and the contract ties a declared refusal status to a
//!   repeat of that member ([`mb_engine::guards::refusal_tied_to`]): a declared `409`
//!   whose own response description names the member in code position and nothing
//!   else there. A `409` naming a member is taken for that member's uniqueness
//!   conflict; a `400`, `412` or `422` is validation as often as uniqueness, and a
//!   sentence of the operation's or the API's description proves nothing, so those
//!   stay offers;
//! - a stored record of the type the create makes holds the very value the request
//!   gives that member: read back now, in the collection the create runs in
//!   ([`super::in_create_collection`]), and agreeing with the references the request
//!   sets, as [`super::value_index::same_values`] required to list it.
//!
//! Anything weaker (a member no refusal names, another collection, a value that
//! differs, a record gone since) keeps the model's answer: the refusal stays an offer.
//! An answer that is the very record holding the value, with its identity, is never
//! refused and stores nothing: as answered when it agrees with the record as stored
//! ([`answers_the_held`], an API answering a repeat with what it holds), else the stored
//! record unchanged (it names the holder's identity and value, so it is no new record).
//!
//! **A described unique set** (`types.<t>.unique` in the world description) is checked
//! first, before generating, by [`described_conflict`]: a create, or an update, whose
//! values of the set's members equal a stored record's (another record, for an update),
//! within the same value of the set's `scope` member, is refused with the set's
//! `conflict` status when the operation declares a response for it, else the first
//! refusal the operation declares; a set the operation can refuse with neither is
//! skipped, and the next set is checked. The refusal's body names the set's members
//! and the request's values of them. A type the description gives no unique set keeps
//! the heuristics above.

use mb_engine::{GeneratedResponse, OperationCall};
use mb_mcm::{Effect, Operation, OperationKind};
use mb_state::{SessionKey, StoredResource};
use serde_json::Value;

use super::{
    CallPlan, MAX_SCAN, MockRuntime, StateAccess, collection_of, in_create_collection, plain,
};

/// A described unique set a write repeats: the status it is refused with, and the
/// set's members with the values the write gives them.
#[derive(Debug)]
pub(super) struct Conflict {
    /// The declared status of the refusal.
    pub code: u16,
    /// Each member of the set (a dotted path), with the value the write gives it.
    pub taken: Vec<(String, Value)>,
}

/// The status `op` refuses a repeat of `set` with: the set's `conflict` when `op`
/// declares a response for it, else the first refusal `op` declares; `None` when it
/// declares neither.
fn set_status(op: &Operation, set: &mb_mcm::world::UniqueDesc) -> Option<u16> {
    set.conflict
        .filter(|code| mb_engine::guards::declares_code(op, *code))
        .or_else(|| mb_engine::guards::refusal_codes(op).first().copied())
}

/// What a create or an update `plan` of `call` is refused with, before any
/// generation, for repeating a stored record's values of a unique set its type's world
/// description declares (see the module's documentation). `target` is the write's
/// target id and the record held there, when there is one; that record is never a
/// conflict of its own update, but is one of a create (the record it would make). At most [`MAX_SCAN`] records of the type are compared.
/// `None` when nothing conflicts, the type describes no unique set the operation can
/// refuse, or the call has a scenario or a forced status.
pub(super) async fn described_conflict(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: (Option<&str>, Option<&StoredResource>),
) -> Option<Conflict> {
    let rt: &MockRuntime = state.rt;
    let op = plan.op;
    let creates = op.kind == OperationKind::Command(Effect::Create);
    let updates = op.kind == OperationKind::Command(Effect::Update);
    if !(creates || updates) || !rt.consistency || !plain(plan) {
        return None;
    }
    let t = op.resource_type.as_deref()?;
    let desc = rt.described().type_desc(t)?;
    if desc.unique.is_empty() {
        return None;
    }
    let body = call.body.as_ref()?;
    let request = op
        .identity
        .as_ref()
        .and_then(|i| i.record_in(body))
        .unwrap_or(body);
    let (target_id, held) = target;
    let held = held.filter(|_| updates);
    // A scope member a create's body leaves out is its parent's, from the path.
    let parent_param = desc
        .parent
        .as_ref()
        .and_then(|p| Some((p.member.as_deref()?, p.path_param.as_deref()?)));
    let in_collection = collection_of(op, call).is_some();
    // A soft-deleted record's values are free again.
    let gone = super::lifecycle::exclusion(rt, t);
    let mut listed: Option<Vec<StoredResource>> = None;
    for set in &desc.unique {
        // A set this operation declares no refusal for is not its to refuse.
        let Some(code) = set_status(op, set) else {
            continue;
        };
        let sent = |m: &str| mb_mcm::member_at(request, m).filter(|v| !v.is_null());
        // An update that sets none of the set's members, nor its scope, changes none of it.
        if updates
            && !set
                .members
                .iter()
                .chain(set.scope.iter())
                .any(|m| sent(m).is_some())
        {
            continue;
        }
        let after = |m: &str| {
            sent(m)
                .cloned()
                .or_else(|| held.and_then(|h| mb_mcm::member_at(&h.data, m)).cloned())
        };
        let Some(values) = set
            .members
            .iter()
            .map(|m| after(m).map(|v| (m.as_str(), v)))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let scope = set.scope.as_deref().map(|scope| {
            let from_path = parent_param
                .filter(|(member, _)| *member == scope)
                .and_then(|(_, param)| call.path_params.get(param))
                .cloned();
            // Only a create whose scope the path gives (a parent's parameter naming the
            // scope member, or a collection under a parent path) can tell, from where a
            // record was placed, whether one holding no scope value shares it.
            let path_scoped = creates && (from_path.is_some() || in_collection);
            (scope, after(scope).or(from_path), path_scoped)
        });
        let records = match &listed {
            Some(records) => records,
            None => listed.insert(state.list(&plan.session, t, MAX_SCAN).await),
        };
        let taken = records.iter().find(|r| {
            // A create's target is the record it would make: one already held there
            // repeats it.
            (creates || Some(r.id.as_str()) != target_id)
                && gone.as_ref().is_none_or(|g| !g.excludes(&r.data))
                && held.is_none_or(|h| h.id != r.id)
                && values
                    .iter()
                    .all(|(m, v)| same_scalar(mb_mcm::member_at(&r.data, m), v))
                && scope.as_ref().is_none_or(|(member, asked, path_scoped)| {
                    let kept = mb_mcm::member_at(&r.data, member).filter(|v| !v.is_null());
                    match (asked, kept) {
                        (Some(asked), Some(kept)) => same_scalar(Some(kept), asked),
                        // Where a record is that holds no scope value, only the
                        // collection a create runs in tells, when the path scopes it.
                        (_, None) if *path_scoped => {
                            in_create_collection(rt, &plan.session, op, call, r)
                        }
                        // No scope value is the same scope only as no scope value; a
                        // value against none is another scope.
                        (None, None) => true,
                        _ => false,
                    }
                })
        });
        let Some(taken) = taken else {
            continue;
        };
        tracing::info!(
            mock = %rt.name,
            operation = %op.id,
            held = %super::log_capped(&taken.id),
            status = code,
            "write refused before generation: it repeats a described unique set"
        );
        return Some(Conflict {
            code,
            taken: values
                .into_iter()
                .map(|(m, v)| (m.to_string(), v))
                .collect(),
        });
    }
    None
}

/// What a create's success comes to, by the rule of the module's documentation.
pub(super) enum Enforced {
    /// Refused with `code`: it repeats the value of `member` the record `held` holds.
    Refuse {
        code: u16,
        member: String,
        held: String,
    },
    /// The answer is the record `id` that holds the value: it stores nothing, and is
    /// served as answered when it agrees with the record as stored, else as `stored`,
    /// the record unchanged (an answer under the holder's identity and value that
    /// changes other members is no new record, and no update either).
    Held { id: String, stored: Option<Value> },
}

/// What the success `answer`, the body of a create `call` as the model gave it,
/// deserves (see the module's documentation), from `same_values` (the records the prompt
/// listed, each with its type and member): `None` when no listed record makes the
/// evidence strong.
pub(super) async fn enforced(
    state: &mut StateAccess<'_>,
    (session, op, t): (&SessionKey, &Operation, &str),
    (call, answer): (&OperationCall, Option<&Value>),
    same_values: &[(String, String, String)],
) -> Option<Enforced> {
    let rt: &MockRuntime = state.rt;
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let Some(Value::Object(body)) = call.body.as_ref() else {
        return None;
    };
    for (listed_type, id, member) in same_values {
        if listed_type != t {
            continue;
        }
        let Some(asked) = body.get(member).filter(|v| v.is_string() || v.is_number()) else {
            continue;
        };
        let Some(held) = state.get(session, t, id).await else {
            continue;
        };
        if !same_scalar(held.data.get(member), asked)
            || !in_create_collection(rt, session, op, call, &held)
        {
            continue;
        }
        if answers_the_held(answer, &held, field) {
            return Some(Enforced::Held {
                id: held.id,
                stored: None,
            });
        }
        if names_the_held(answer, &held, (field, member)) {
            return Some(Enforced::Held {
                id: held.id,
                stored: Some(held.data),
            });
        }
        let Some(code) = mb_engine::guards::refusal_tied_to(op, member) else {
            continue;
        };
        return Some(Enforced::Refuse {
            code,
            member: member.clone(),
            held: held.id,
        });
    }
    None
}

/// Whether `answer`, a create's `2xx` body, is the stored record `held` it repeats,
/// answered as it is stored (an "already exists, here it is" answer): it holds `held`'s
/// id under the identity field `field`, at least one other member `held` holds, and
/// every member it shares with `held` agrees (members of its own, such as an
/// `existing: true`, may stand beside them). Such an answer is right, and is never
/// replaced by a refusal.
pub(super) fn answers_the_held(answer: Option<&Value>, held: &StoredResource, field: &str) -> bool {
    let whole = answer.and_then(|a| mb_mcm::member_at(a, field));
    let (Some(Value::Object(answer)), Value::Object(stored)) = (answer, &held.data) else {
        return false;
    };
    let same_id = whole.is_some_and(|id| match id {
        Value::String(s) => *s == held.id,
        Value::Number(n) => n.to_string() == held.id,
        _ => false,
    });
    let mut shared = 0;
    for (name, value) in answer {
        let Some(kept) = stored.get(name) else {
            continue;
        };
        if !agrees(value, kept) {
            return false;
        }
        if name != field {
            shared += 1;
        }
    }
    same_id && shared > 0
}

/// Whether `answer` names the stored record `held`: its id under the identity field
/// `field`, and `held`'s value of `member`, whatever else it says.
fn names_the_held(
    answer: Option<&Value>,
    held: &StoredResource,
    (field, member): (&str, &str),
) -> bool {
    let whole = answer.and_then(|a| mb_mcm::member_at(a, field));
    let Some(Value::Object(answer)) = answer else {
        return false;
    };
    let same_id = whole.is_some_and(|id| match id {
        Value::String(s) => *s == held.id,
        Value::Number(n) => n.to_string() == held.id,
        _ => false,
    });
    same_id
        && answer
            .get(member)
            .is_some_and(|v| same_scalar(held.data.get(member), v))
}

/// Whether `answered` agrees with `kept`: equal scalars (numbers by value), objects
/// agreeing on the members both hold, arrays item by item.
fn agrees(answered: &Value, kept: &Value) -> bool {
    match (answered, kept) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Object(a), Value::Object(b)) => a
            .iter()
            .all(|(k, v)| b.get(k).is_none_or(|kept| agrees(v, kept))),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| agrees(x, y))
        }
        _ => answered == kept,
    }
}

/// Whether `held` is the scalar `asked` (numbers by value).
fn same_scalar(held: Option<&Value>, asked: &Value) -> bool {
    match (held, asked) {
        (Some(Value::Number(a)), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Some(held), asked) => held == asked,
        (None, _) => false,
    }
}

/// `generated`, a create's success, replaced by `refusal`, its declared error: every
/// effect it listed is dropped and said so in its outcomes, as a refused call changes
/// nothing.
pub(super) fn replace(
    rt: &MockRuntime,
    op: &Operation,
    (member, id): (&str, &str),
    generated: &mut GeneratedResponse,
    mut refusal: GeneratedResponse,
) {
    const REASON: &str = "the create repeats a unique value a stored record holds: it is refused";
    let dropped = std::mem::take(&mut generated.effects);
    refusal.dropped_effects = generated.dropped_effects + dropped.len();
    refusal.effect_outcomes = std::mem::take(&mut generated.effect_outcomes);
    refusal
        .effect_outcomes
        .extend(dropped.into_iter().map(|effect| mb_engine::EffectOutcome {
            effect,
            rejected: Some(REASON.to_string()),
            merged: None,
            applied: Vec::new(),
        }));
    tracing::info!(
        mock = %rt.name,
        operation = %op.id,
        member = %super::log_capped(member),
        id = %super::log_capped(id),
        status = refusal.status,
        "create refused: it repeats a unique value a stored record holds"
    );
    *generated = refusal;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_compare_numbers_by_value() {
        assert!(same_scalar(Some(&Value::from(3)), &Value::from(3.0)));
        assert!(same_scalar(Some(&Value::from("a")), &Value::from("a")));
        assert!(!same_scalar(Some(&Value::from("a")), &Value::from("b")));
        assert!(!same_scalar(None, &Value::from("a")));
    }
}
