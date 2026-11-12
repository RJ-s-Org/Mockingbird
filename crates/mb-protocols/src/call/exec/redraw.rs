//! An answer built from a stored record that breaks the answer's schema, repaired in
//! place: the members the validation errors point at are re-drawn from the schema (the
//! faker's), at the narrowest place that can be re-drawn, and everything else the
//! record gave is kept, its identity above all, so that a read of the id the client got
//! agrees with what it was answered. An identity that breaks the schema is never
//! re-drawn: the answer would name a record the session does not hold.
//!
//! Each repair is counted ([`ANSWER_REPAIRS_METRIC`]) and kept on the request record
//! (`answer_repair: {outcome, members}`).

use serde_json::{Map, Value};

/// Answers built from a stored record that broke their schema, by mock, operation and
/// outcome ([`Repair`]'s names): `members_redrawn`, `identity_invalid`,
/// `unrepairable`.
pub(crate) const ANSWER_REPAIRS_METRIC: &str = "mb_answer_repairs_total";

/// Rounds of re-drawing at most: a re-drawn member can expose a failure its old value
/// hid (an `anyOf` decided by a sibling), or still fail where its parent decides.
const MAX_ROUNDS: usize = 4;

/// What became of an answer built from a stored record that broke its schema.
#[derive(Debug, PartialEq)]
pub(super) enum Repair {
    /// Valid with the members at these pointers re-drawn (or, undeclared, removed),
    /// the identity and every other value kept.
    Redrawn(Map<String, Value>, Vec<String>),
    /// The record's identity itself breaks the schema: not repaired into another id.
    IdentityInvalid,
    /// No valid answer within [`MAX_ROUNDS`], or only by re-drawing what holds the
    /// identity.
    Unrepairable,
}

impl Repair {
    /// The outcome as counted and recorded.
    pub(super) fn outcome(&self) -> &'static str {
        match self {
            Repair::Redrawn(..) => "members_redrawn",
            Repair::IdentityInvalid => "identity_invalid",
            Repair::Unrepairable => "unrepairable",
        }
    }
}

/// `answer`, which breaks `op`'s answer schema (the narrowed `response_schema` when
/// given, else `schema`, its success schema), repaired: see the module. `identities` are
/// the JSON pointers of the record's identity in the answer.
pub(super) fn repaired(
    rt: &crate::MockRuntime,
    op: &mb_mcm::Operation,
    (schema, response_schema): (&Value, Option<&Value>),
    answer: Map<String, Value>,
    identities: &[String],
) -> Repair {
    let bundle = &rt.bundle;
    let engine = &rt.engine;
    let mut answer = Value::Object(answer);
    let mut members: Vec<String> = Vec::new();
    for _ in 0..MAX_ROUNDS {
        let Some(mut found) = engine.stored_violations(bundle, op, &answer, response_schema) else {
            return Repair::Unrepairable;
        };
        if found.is_empty() {
            break;
        }
        if found
            .iter()
            .any(|(at, _)| identities.iter().any(|id| within(at, id)))
        {
            return Repair::IdentityInvalid;
        }
        // Shallowest first: a member re-drawn whole needs none of its own fixes.
        found.sort_by_key(|(at, _)| at.matches('/').count());
        let mut round: Vec<String> = Vec::new();
        for (at, keyword) in found {
            if round.iter().any(|done| within(&at, done)) {
                continue;
            }
            if keyword == "additionalProperties" {
                let undeclared = undeclared(bundle, schema, &answer, &at);
                if undeclared.is_empty() {
                    return Repair::Unrepairable;
                }
                for gone in undeclared {
                    remove_at(&mut answer, &gone);
                    round.push(gone);
                }
                continue;
            }
            // A member re-drawn before that still fails is re-drawn with its parent.
            let mut at = match members.iter().find(|m| within(&at, m)) {
                Some(m) => parent(m).to_owned(),
                None => at,
            };
            let value = loop {
                if at.is_empty() || identities.iter().any(|id| within(id, &at)) {
                    return Repair::Unrepairable;
                }
                if let Some(value) = engine.draw_at(bundle, op, schema, &at) {
                    break value;
                }
                at = parent(&at).to_owned();
            };
            if !set_at(&mut answer, &at, value) {
                return Repair::Unrepairable;
            }
            round.retain(|done| !within(done, &at));
            round.push(at);
        }
        for at in round {
            members.retain(|m| !within(m, &at));
            if !members.iter().any(|m| within(&at, m)) {
                members.push(at);
            }
        }
    }
    match engine.stored_violations(bundle, op, &answer, response_schema) {
        Some(found) if found.is_empty() => match answer {
            Value::Object(answer) => Repair::Redrawn(answer, members),
            _ => Repair::Unrepairable,
        },
        _ => Repair::Unrepairable,
    }
}

/// Whether JSON pointer `at` is `outer` or below it.
fn within(at: &str, outer: &str) -> bool {
    at == outer
        || at
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The pointer of what holds `at` (`""` for a top member).
fn parent(at: &str) -> &str {
    at.rfind('/').map_or("", |i| &at[..i])
}

/// The last step of `at`, unescaped.
fn last_step(at: &str) -> String {
    at.rsplit('/')
        .next()
        .unwrap_or_default()
        .replace("~1", "/")
        .replace("~0", "~")
}

/// One step of a JSON pointer, escaped.
pub(super) fn escape(step: &str) -> String {
    step.replace('~', "~0").replace('/', "~1")
}

/// Sets the value at `at` of `answer`, in an object (adding the member) or over an
/// array's item; whether it could.
fn set_at(answer: &mut Value, at: &str, value: Value) -> bool {
    let step = last_step(at);
    match answer.pointer_mut(parent(at)) {
        Some(Value::Object(members)) => {
            members.insert(step, value);
            true
        }
        Some(Value::Array(items)) => {
            match step.parse::<usize>().ok().and_then(|i| items.get_mut(i)) {
                Some(item) => {
                    *item = value;
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}

/// Removes the member at `at` of `answer`.
fn remove_at(answer: &mut Value, at: &str) {
    let step = last_step(at);
    if let Some(Value::Object(members)) = answer.pointer_mut(parent(at)) {
        members.remove(&step);
    }
}

/// The members an `additionalProperties` violation at `at` is about that `schema`
/// declares nowhere: `at` itself when it names one such member, else those of the
/// object at `at`.
fn undeclared(bundle: &mb_mcm::Bundle, schema: &Value, answer: &Value, at: &str) -> Vec<String> {
    let declared = |at: &str| mb_engine::consistency::schema_at(bundle, schema, at).is_some();
    if !at.is_empty() && !declared(at) && answer.pointer(at).is_some() {
        return vec![at.to_owned()];
    }
    match answer.pointer(at) {
        Some(Value::Object(members)) => members
            .keys()
            .map(|name| format!("{at}/{}", escape(name)))
            .filter(|member| !declared(member))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_pointer_is_within_itself_and_its_ancestors_only() {
        assert!(within("/a/b", "/a"));
        assert!(within("/a", "/a"));
        assert!(!within("/ab", "/a"));
        assert!(!within("/a", "/a/b"));
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/a"), "");
    }

    #[test]
    fn a_value_is_set_in_an_object_or_over_an_item() {
        let mut v = json!({"a": {"x~y": 1}, "l": [1, 2]});
        assert!(set_at(&mut v, "/a/x~0y", json!(2)));
        assert!(set_at(&mut v, "/l/1", json!(3)));
        assert!(!set_at(&mut v, "/l/5", json!(3)));
        assert!(!set_at(&mut v, "/nope/x", json!(3)));
        assert_eq!(v, json!({"a": {"x~y": 2}, "l": [1, 3]}));
        remove_at(&mut v, "/a/x~0y");
        assert_eq!(v, json!({"a": {}, "l": [1, 3]}));
    }
}
