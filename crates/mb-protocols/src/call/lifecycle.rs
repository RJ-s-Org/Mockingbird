//! Lifecycles, soft deletes and `not_found` read from the effective world description.
//!
//! A type's `lifecycle` names its status member and, optionally:
//!
//! - `initial`: the state a created record starts in, when the create's request does
//!   not name the member ([`initial`]);
//! - `soft_delete`: the state a delete moves a record to instead of removing it
//!   ([`soft_delete`]). The record stays stored, readable in that state, and leaves no
//!   deleted marker ([`deleted`]). A list leaves it out ([`Exclusion`], applied by
//!   [`list_query::keeps`] on every list path) unless the request sends the
//!   `listed_with` parameter at its value; a unique set no longer counts it
//!   ([`repeats::described_conflict`]).
//!
//! An operation's `not_found` is the status a read of a missing record answers
//! ([`not_found`]), instead of the one the engine would pick.
//!
//! Each state is written as its member declares it ([`Described::state_value`]): a
//! boolean member takes `"true"` as `true`. A state the member's schema refuses is not
//! written: the record stays as it was, and a delete removes it as today. Tier 0 emits
//! no lifecycle and no `not_found`, so a runtime without a world answers alike.
//!
//! [`Described::state_value`]: super::described::Described::state_value

use mb_mcm::Operation;
use serde_json::Value;

use super::entities::entity_fits;
use crate::MockRuntime;

/// What makes a record of a type soft-deleted, for a list or a unique check to leave it
/// out: its `member` holds `state`. `unless` is the list parameter and value that ask
/// for deleted records too.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Exclusion {
    pub(super) member: String,
    pub(super) state: Value,
    pub(super) unless: Option<(String, Value)>,
}

impl Exclusion {
    /// Whether `record` is soft-deleted.
    pub(super) fn excludes(&self, record: &Value) -> bool {
        mb_mcm::member_at(record, &self.member).is_some_and(|held| *held == self.state)
    }

    /// Whether the request's query `query` sends the `unless` parameter at its value
    /// (`"true"` is `true`; an array sends it when any of its values does).
    pub(super) fn lifted_by(&self, query: &serde_json::Map<String, Value>) -> bool {
        let Some((param, wanted)) = &self.unless else {
            return false;
        };
        let Some(sent) = query.get(param) else {
            return false;
        };
        let same = |v: &Value| match (v, wanted) {
            (Value::String(text), Value::String(w)) => text == w,
            (Value::String(text), other) => {
                serde_json::from_str::<Value>(text.trim()).is_ok_and(|v| v == *other)
            }
            (v, w) => v == w,
        };
        match sent {
            Value::Array(values) => values.iter().any(same),
            one => same(one),
        }
    }
}

/// The soft-delete exclusion of type `t`, when its effective lifecycle has a soft
/// delete whose state its member takes.
pub(super) fn exclusion(rt: &MockRuntime, t: &str) -> Option<Exclusion> {
    let described = rt.described();
    let lifecycle = described.type_desc(t)?.lifecycle.as_ref()?;
    let soft = lifecycle.soft_delete.as_ref()?;
    let state = described.state_value(t, &soft.state)?;
    Some(Exclusion {
        member: lifecycle.member.clone(),
        state,
        unless: soft
            .listed_with
            .as_ref()
            .map(|lw| (lw.param.clone(), lw.value.clone())),
    })
}

/// Sets the described `initial` state of type `t` on `record`, a created record, when
/// `request` does not name the lifecycle member. Nothing is set when the record is not
/// an object, or setting it leaves a record the type's schema refuses.
pub(super) fn initial(rt: &MockRuntime, t: &str, record: &mut Value, request: Option<&Value>) {
    let described = rt.described();
    let Some(lifecycle) = described.type_desc(t).and_then(|d| d.lifecycle.as_ref()) else {
        return;
    };
    let Some(state) = lifecycle.initial.as_deref() else {
        return;
    };
    if request.is_some_and(|r| mb_mcm::member_at(r, &lifecycle.member).is_some()) {
        return;
    }
    let Some(value) = described.state_value(t, state) else {
        return;
    };
    set_state(rt, t, record, &lifecycle.member, value);
}

/// Moves `record`, a held record of type `t`, to the described soft-delete state;
/// answers whether it did. `false` when `t` has no soft delete, or the record cannot
/// take the state, so the delete removes it as today.
pub(super) fn soft_delete(rt: &MockRuntime, t: &str, record: &mut Value) -> bool {
    let Some(gone) = exclusion(rt, t) else {
        return false;
    };
    set_state(rt, t, record, &gone.member, gone.state)
}

/// Sets `member` of `record` to `value`, unless that leaves a record type `t`'s schema
/// refuses; answers whether it set it.
fn set_state(rt: &MockRuntime, t: &str, record: &mut Value, member: &str, value: Value) -> bool {
    if !record.is_object() {
        return false;
    }
    let before = record.clone();
    if !mb_mcm::set_member_at(record, member, value) {
        return false;
    }
    if !entity_fits(rt, t, record) {
        tracing::info!(
            mock = %rt.name,
            r#type = %t,
            member = %member,
            "a described lifecycle state leaves a record its schema refuses: not set"
        );
        *record = before;
        return false;
    }
    true
}

/// The status the description says a read by `op` of a missing record answers.
pub(super) fn not_found(rt: &MockRuntime, op: &Operation) -> Option<u16> {
    rt.described().op(&op.id)?.not_found
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_exclusion_matches_its_state_and_is_lifted_by_its_parameter() {
        let gone = Exclusion {
            member: "state".into(),
            state: json!("GONE"),
            unless: Some(("showDeleted".into(), json!(true))),
        };
        assert!(gone.excludes(&json!({"state": "GONE"})));
        assert!(!gone.excludes(&json!({"state": "LIVE"})));
        assert!(!gone.excludes(&json!({})));
        let query = |v: Value| {
            let mut q = serde_json::Map::new();
            q.insert("showDeleted".into(), v);
            q
        };
        assert!(gone.lifted_by(&query(json!("true"))));
        assert!(gone.lifted_by(&query(json!(true))));
        assert!(gone.lifted_by(&query(json!(["false", "true"]))));
        assert!(!gone.lifted_by(&query(json!("false"))));
        assert!(!gone.lifted_by(&serde_json::Map::new()));
        let no_param = Exclusion {
            unless: None,
            ..gone
        };
        assert!(!no_param.lifted_by(&query(json!("true"))));
    }
}
