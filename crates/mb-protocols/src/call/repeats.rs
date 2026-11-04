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
//!   repeat of that member ([`mb_engine::guards::refusal_tied_to`]): a `409` whose
//!   response description names the member, or a sentence of the operation's or the
//!   API's description that gives the status beside the member's name;
//! - a stored record of the type the create makes holds the very value the request
//!   gives that member: read back now, in the collection the create runs in
//!   ([`super::in_create_collection`]), and agreeing with the references the request
//!   sets, as [`super::value_index::same_values`] required to list it.
//!
//! Anything weaker (a member no refusal names, another collection, a value that
//! differs, a record gone since) keeps the model's answer: the refusal stays an offer.
//! So does an answer that is the very record holding the value, with its identity, as
//! stored ([`answers_the_held`]): an API answering a repeat with what it holds is right.

use mb_engine::{GeneratedResponse, OperationCall};
use mb_mcm::Operation;
use mb_state::{SessionKey, StoredResource};
use serde_json::Value;

use super::{MockRuntime, StateAccess, in_create_collection};

/// What a create's success comes to, by the rule of the module's documentation.
pub(super) enum Enforced {
    /// Refused with `code`: it repeats the value of `member` the record `held` holds.
    Refuse {
        code: u16,
        member: String,
        held: String,
    },
    /// The answer is the record that holds the value, with this id, as stored: it
    /// stands, and stores nothing.
    Held(String),
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
        let Some(code) = mb_engine::guards::refusal_tied_to(rt.bundle(), op, member) else {
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
            return Some(Enforced::Held(held.id));
        }
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
    let (Some(Value::Object(answer)), Value::Object(stored)) = (answer, &held.data) else {
        return false;
    };
    let same_id = answer.get(field).is_some_and(|id| match id {
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
