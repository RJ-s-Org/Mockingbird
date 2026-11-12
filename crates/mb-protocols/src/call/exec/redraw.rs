//! An answer built from a stored record that breaks the answer's schema, repaired in
//! place: the members the validation errors point at are re-drawn from the schema (the
//! faker's), at the narrowest place that can be re-drawn (inside the branch of a union
//! the answer is of), and everything else the answer holds is kept, the record's
//! identity above all, so that a read of the id the client got agrees with what it was
//! answered. A member that names the record (its identity, a key a read or path takes,
//! an alternate key, its parent's reference, a compound key) is never re-drawn: the
//! answer would name a record the session does not hold.
//!
//! A re-drawn member is drawn from the call's seed (the request's, else the mock's,
//! else [`mb_engine::DEFAULT_FAKER_SEED`]) mixed with the record's type and id and the
//! member's place in the record, so every answer of one record agrees and two records
//! differ.
//!
//! Each repair is counted ([`ANSWER_REPAIRS_METRIC`]) and kept on the request record
//! (`answer_repair: {outcome, members}`).

use serde_json::{Map, Value};

/// Answers built from a stored record that broke their schema, by mock, operation and
/// outcome: [`MEMBERS_REDRAWN`], [`IDENTITY_INVALID`], [`UNREPAIRABLE`].
pub(crate) const ANSWER_REPAIRS_METRIC: &str = "mb_answer_repairs_total";

/// The outcome of a repair that re-drew members and kept the identity.
pub(super) const MEMBERS_REDRAWN: &str = "members_redrawn";
/// The outcome of an answer whose stored identity breaks its schema.
pub(super) const IDENTITY_INVALID: &str = "identity_invalid";
/// The outcome of an answer no repair made valid.
pub(super) const UNREPAIRABLE: &str = "unrepairable";

/// Rounds of re-drawing at most: a re-drawn member can expose a failure its old value
/// hid (an `anyOf` decided by a sibling), or still fail where its parent decides.
const MAX_ROUNDS: usize = 4;

/// Unions a violation is followed into at most.
const MAX_UNION_DEPTH: usize = 4;

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
            Repair::Redrawn(..) => MEMBERS_REDRAWN,
            Repair::IdentityInvalid => IDENTITY_INVALID,
            Repair::Unrepairable => UNREPAIRABLE,
        }
    }
}

/// The record an answer is built from: its type, its id, and the JSON pointer of the
/// place in the answer that holds its members (`""` at the top, `/data` in an
/// envelope).
pub(super) struct RecordAt<'a> {
    pub(super) resource_type: &'a str,
    pub(super) id: &'a str,
    pub(super) at: &'a str,
}

/// The call's answer schema: the narrowed `response` when given, else `schema`, the
/// success schema, which members are drawn from.
pub(super) struct Schemas<'a> {
    pub(super) schema: &'a Value,
    pub(super) response: Option<&'a Value>,
}

/// The members that name a record of type `t` in the answers of `op`: its identity
/// member and the field `op` keys it by, the keys its reads and paths take (`number`,
/// `key`, see [`super::super::effects::identity_members`]), the described alternate
/// keys and changed identity member, the alternate keys `op`'s path parameter offers,
/// the member naming its parent, and the members of a compound unique key.
pub(super) fn naming_members(
    rt: &crate::MockRuntime,
    op: &mb_mcm::Operation,
    t: &str,
) -> Vec<String> {
    let mut names: Vec<String> = vec![super::records::identity_member(rt, op, t).to_owned()];
    if let Some(identity) = &op.identity {
        names.push(identity.field.clone());
        names.extend(identity.path_field.iter().cloned());
    }
    names.extend(
        super::super::effects::identity_members(rt, t)
            .into_iter()
            .map(str::to_owned),
    );
    names.extend(super::records::alternates(rt, t));
    names.extend(super::super::lookup::alternate_members(rt, op, t));
    let described = rt.described();
    names.extend(described.changed_identity_member(t).map(str::to_owned));
    if let Some(desc) = described.type_desc(t) {
        names.extend(desc.parent.as_ref().and_then(|p| p.member.clone()));
        for unique in desc.unique.iter().filter(|u| u.members.len() > 1) {
            names.extend(unique.members.iter().cloned());
        }
    }
    let mut seen = Vec::new();
    names.retain(|name| {
        let new = !seen.contains(name);
        seen.push(name.clone());
        new
    });
    names
}

/// The JSON pointers in `answer` of `members` (dotted ones nested, or as one member
/// so named) under each of `places`: those the answer holds a value at.
pub(super) fn pointers_of(members: &[String], places: &[&str], answer: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for place in places {
        for member in members {
            let nested: Vec<String> = member.split('.').map(escape).collect();
            for at in [
                format!("{place}/{}", escape(member)),
                format!("{place}/{}", nested.join("/")),
            ] {
                if answer.pointer(&at).is_some() && !out.contains(&at) {
                    out.push(at);
                }
            }
        }
    }
    out
}

/// `answer`, which breaks `op`'s answer schema, repaired: see the module. `identities`
/// are the JSON pointers of what names the record in the answer, and `seed` the
/// call's.
pub(super) fn repaired(
    rt: &crate::MockRuntime,
    op: &mb_mcm::Operation,
    (schemas, record): (&Schemas<'_>, &RecordAt<'_>),
    (answer, identities): (Map<String, Value>, &[String]),
    seed: u64,
) -> Repair {
    let bundle = &rt.bundle;
    let engine = &rt.engine;
    let schema = schemas.schema;
    let mut answer = Value::Object(answer);
    let mut members: Vec<String> = Vec::new();
    for _ in 0..MAX_ROUNDS {
        let Some(found) = engine.stored_violations(bundle, op, &answer, schemas.response) else {
            return Repair::Unrepairable;
        };
        if found.is_empty() {
            break;
        }
        let mut found: Vec<(String, String)> = found
            .into_iter()
            .flat_map(|(at, keyword)| narrowed(rt, schema, &answer, at, keyword, 0))
            .collect();
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
            if keyword == "additionalProperties" || keyword == "unevaluatedProperties" {
                let undeclared = undeclared(rt, schema, &answer, &at);
                if undeclared.is_empty()
                    || undeclared
                        .iter()
                        .any(|gone| identities.iter().any(|id| within(id, gone)))
                {
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
                let place = relative(&at, record.at);
                let labels = [record.resource_type, record.id, place];
                if let Some(value) = engine.draw_at(bundle, (schema, &answer), &at, (seed, &labels))
                {
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
    match engine.stored_violations(bundle, op, &answer, schemas.response) {
        Some(found) if found.is_empty() => match answer {
            Value::Object(answer) => Repair::Redrawn(answer, members),
            _ => Repair::Unrepairable,
        },
        _ => Repair::Unrepairable,
    }
}

/// Violation `(at, keyword)` of `answer`, followed into the branch of a union
/// (`oneOf`, `anyOf`) the value at `at` is of ([`mb_engine::Engine::union_branch`]):
/// that branch's own violations of it, so that a repair re-draws what breaks the branch
/// rather than the whole value. As it is when the branch has none (it matches several
/// branches, or none is told).
pub(super) fn narrowed(
    rt: &crate::MockRuntime,
    schema: &Value,
    answer: &Value,
    at: String,
    keyword: String,
    depth: usize,
) -> Vec<(String, String)> {
    let (bundle, engine) = (&rt.bundle, &rt.engine);
    if depth < MAX_UNION_DEPTH
        && (keyword == "oneOf" || keyword == "anyOf")
        && let Some(value) = answer.pointer(&at)
        && let Some(union) = engine.schema_at(bundle, schema, &at, answer)
        && let Some(branch) = engine.union_branch(bundle, &union, value)
        && let Some(inner) = engine.schema_violations(bundle, &branch, value)
        && !inner.is_empty()
    {
        return inner
            .into_iter()
            .flat_map(|(p, k)| narrowed(rt, schema, answer, format!("{at}{p}"), k, depth + 1))
            .collect();
    }
    vec![(at, keyword)]
}

/// `at`'s place in the record held at `record` of the answer (`at` itself when the
/// record is at the top, or `at` is not in it).
fn relative<'a>(at: &'a str, record: &str) -> &'a str {
    match at.strip_prefix(record) {
        Some(rest) if !record.is_empty() && rest.starts_with('/') => rest,
        _ => at,
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
fn escape(step: &str) -> String {
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

/// The members an `additionalProperties` (or `unevaluatedProperties`) violation at `at`
/// is about that `schema` declares nowhere: `at` itself when it names one such member,
/// else those of the object at `at`.
fn undeclared(rt: &crate::MockRuntime, schema: &Value, answer: &Value, at: &str) -> Vec<String> {
    let declared = |member: &str| {
        rt.engine
            .schema_at(&rt.bundle, schema, member, answer)
            .is_some()
    };
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
    fn a_member_is_placed_relative_to_its_record() {
        assert_eq!(relative("/data/code", "/data"), "/code");
        assert_eq!(relative("/code", ""), "/code");
        assert_eq!(relative("/datum/code", "/data"), "/datum/code");
        assert_eq!(relative("/status", "/data"), "/status");
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

    #[test]
    fn naming_members_are_found_at_the_top_and_in_the_envelope() {
        let answer = json!({"id": "w1", "data": {"id": "w1", "meta": {"key": "k"}}});
        let members = vec!["id".to_owned(), "meta.key".to_owned(), "gone".to_owned()];
        assert_eq!(
            pointers_of(&members, &["", "/data"], &answer),
            vec!["/id", "/data/id", "/data/meta/key"]
        );
    }
}
