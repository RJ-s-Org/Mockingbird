//! Computed members: a type's `computed` members, and its `derived` ones read as
//! `computed` ([`mb_mcm::world::Expressions`]), evaluated when a record of the type is
//! answered and never stored:
//! - an answer from state ([`filled`], before the held answer is served);
//! - each item of a list from state ([`fill_page`]);
//! - a write's answer ([`filled`], [`filled_new`] for a create);
//! - a generated answer about a held record ([`fill`], after its writes).
//!
//! Each expression sees `record`, the record as it is answered (before its computed
//! members are set), `request` and `now` of the call answering it; `it` is not bound.
//! A value is written as the member declares it ([`exprs::written_back`]); one its
//! schema refuses, `null`, and a failed expression (logged and noted under
//! `expr_failed`) leave the member as it was. An item the answer's schema refuses once
//! filled is put back as it was.
//!
//! A `derived` member reads the record's children through the runtime's own
//! `children(type, where?)` ([`Host::children`]): the held records of the child type
//! that are not soft-deleted, and whose `parent.member` names the record's id, or which
//! this runtime stored under a collection whose `parent.path_param` value is that id
//! ([`Collections::get`]). Ids compare bare: the parent's `pattern` and the type's
//! identity pattern are taken off. A `derived` member is set only on a record created
//! in the session by this runtime ([`Collections::created`]; a rebuild forgets it, and a
//! record with no child held then keeps the model's value), or one with a child held. A
//! sum adds exactly (a [`Decimal`]), and the result goes into an `integer` member only
//! when integral, a `number` member as a number, a `string` member as a decimal at the
//! children's largest scale; it is not set over a listing cut at [`MAX_SCAN`] (it would
//! not be exact), nor over a child whose member is no number. Its failures are logged,
//! not noted.
//!
//! An answer costs one listing per child type its `derived` members read, whatever the
//! page size (at most [`MAX_DERIVED_CHILD_TYPES`]), one per type its expressions'
//! `count`, `exists` and `held` read, and the lookups `held` makes by key.

use std::collections::{BTreeMap, HashMap};

use mb_engine::SchemaValidator;
pub(crate) use mb_mcm::world::MAX_DERIVED_CHILD_TYPES;
use mb_mcm::world::ParentDesc;
use mb_mcm::world::expr::{Decimal, MAX_STEPS, Program};

use super::exprs::{self, Bound, SessionHost};
use super::*;

/// The step budget of a `derived` member, whose expression the runtime writes: its
/// reads are bounded by [`MAX_SCAN`] children, not by the budget.
const DERIVED_STEPS: u64 = 1_000_000;

/// The held children of one type for one session, by the bare id of their parent.
#[derive(Debug, Default)]
pub(super) struct Children {
    by_parent: HashMap<String, Vec<Value>>,
    /// The listing returned [`MAX_SCAN`] records: some may be missing.
    pub(super) cut: bool,
}

impl Children {
    /// The held children of type `of`, whose described parent `parent` is type `t`:
    /// one listing. `None` when the store did not answer.
    async fn of(
        state: &mut StateAccess<'_>,
        session: &SessionKey,
        (t, of): (&str, &str),
        parent: &ParentDesc,
    ) -> Option<Self> {
        let rt = state.rt;
        let listed = state.list(session, of, MAX_SCAN).await;
        if !state.available() {
            return None;
        }
        let gone = lifecycle::exclusion(rt, of);
        // Where `parent.path_param` is in each collection template met.
        let mut positions: HashMap<String, Option<usize>> = HashMap::new();
        let mut children = Self {
            by_parent: HashMap::new(),
            cut: listed.len() >= MAX_SCAN,
        };
        for child in listed {
            if gone.as_ref().is_some_and(|g| g.excludes(&child.data)) {
                continue;
            }
            let mut parents: Vec<String> = Vec::new();
            if let Some(member) = &parent.member
                && let Some(value) = mb_mcm::member_at(&child.data, member).and_then(id_string)
            {
                parents.push(bare(rt, t, parent.pattern.as_deref(), &value));
            }
            if let Some(param) = &parent.path_param
                && let Some(collection) = rt.collections().get(session, &child)
                && let Some(at) = *positions
                    .entry(collection.template.clone())
                    .or_insert_with(|| param_position(rt, &collection.template, param))
                && let Some(value) = path_segments(&collection.path).get(at)
            {
                let value = value.to_string();
                let value = bare(rt, t, parent.pattern.as_deref(), &value);
                if !parents.contains(&value) {
                    parents.push(value);
                }
            }
            for id in parents {
                children
                    .by_parent
                    .entry(id)
                    .or_default()
                    .push(child.data.clone());
            }
        }
        Some(children)
    }

    pub(super) fn under(&self, id: &str) -> &[Value] {
        self.by_parent.get(id).map_or(&[], Vec::as_slice)
    }
}

/// The segment of collection `template` (`/orders/{}/lines`, its parameters unnamed)
/// that path parameter `param` fills, as a route of the contract beginning with that
/// collection names it.
fn param_position(rt: &MockRuntime, template: &str, param: &str) -> Option<usize> {
    let wanted = path_segments(template);
    rt.bundle().operations.iter().find_map(|op| {
        let Binding::Http { path, .. } = &op.binding else {
            return None;
        };
        let segments = path_segments(path);
        let prefix = segments.get(..wanted.len())?;
        let same = prefix
            .iter()
            .zip(&wanted)
            .all(|(segment, want)| match segment_param(segment) {
                Some(_) => *want == "{}",
                None => segment == want,
            });
        if !same {
            return None;
        }
        prefix
            .iter()
            .position(|segment| segment_param(segment) == Some(param))
    })
}

/// `value`, an id of type `t`, bare: the parent's `pattern` taken off, else `t`'s
/// identity pattern, else as it is.
fn bare(rt: &MockRuntime, t: &str, pattern: Option<&str>, value: &str) -> String {
    pattern
        .and_then(|p| records::bare_id(p, value))
        .or_else(|| records::bare_id(records::pattern(rt, t)?, value))
        .unwrap_or_else(|| value.to_string())
}

/// The member type `t`'s records are named by.
fn identity_member<'a>(rt: &'a MockRuntime, t: &str) -> &'a str {
    rt.described()
        .type_desc(t)
        .and_then(|d| d.identity.as_ref())
        .and_then(|i| i.member.as_deref())
        .unwrap_or("id")
}

/// The types of `world` whose derived members are over more than
/// [`MAX_DERIVED_CHILD_TYPES`] child types: those past it are not set. Logged once per
/// runtime, when it is built.
pub(crate) fn log_skipped_child_types(world: &mb_mcm::world::World) {
    for (t, desc) in &world.types {
        let mut all: Vec<&str> = desc
            .derived
            .values()
            .map(|d| match d {
                mb_mcm::world::DerivedDesc::Count { of, .. }
                | mb_mcm::world::DerivedDesc::Sum { of, .. } => of.as_str(),
            })
            .collect();
        all.sort_unstable();
        all.dedup();
        if all.len() > MAX_DERIVED_CHILD_TYPES {
            tracing::info!(
                r#type = %t,
                child_types = all.len(),
                max = MAX_DERIVED_CHILD_TYPES,
                "derived members over child types past the cap are not set"
            );
        }
    }
}

/// Whether the description computes any member of type `t`.
pub(super) fn describes(rt: &MockRuntime, t: &str) -> bool {
    rt.world().is_some() && rt.described().computed(t).is_some()
}

/// Whether `child` holds each of `wanted`'s values: equal, or the same text.
pub(super) fn holds(child: &Value, wanted: Option<&BTreeMap<String, Value>>) -> bool {
    wanted.into_iter().flatten().all(|(member, want)| {
        let Some(held) = mb_mcm::member_at(child, member) else {
            return false;
        };
        held == want
            || match (held, want) {
                (Value::String(text), other) | (other, Value::String(text)) => {
                    !other.is_string()
                        && serde_json::from_str::<Value>(text.trim()).is_ok_and(|v| v == *other)
                }
                _ => false,
            }
    })
}

/// How one computed member of a type is written: its schema's declared types, and a
/// validator of its whole schema.
struct Target {
    typed: Value,
    validator: SchemaValidator,
}

impl Target {
    /// `member` as record schema `record` declares it; `None` when it does not.
    fn of(rt: &MockRuntime, record: &Value, member: &str) -> Option<Self> {
        let bundle = rt.bundle();
        let schema = property_schema(bundle, record, member)?;
        let (parts, _) = schema_parts(bundle, Some(&schema));
        let mut types: Vec<Value> = Vec::new();
        for part in parts {
            let declared = match part.get("type") {
                Some(Value::String(t)) => vec![Value::String(t.clone())],
                Some(Value::Array(ts)) => ts.clone(),
                _ => Vec::new(),
            };
            for t in declared {
                if !types.contains(&t) {
                    types.push(t);
                }
            }
        }
        let validator = SchemaValidator::for_output(&bundle.with_reachable_defs(&schema)).ok()?;
        Some(Self {
            typed: serde_json::json!({ "type": types }),
            validator,
        })
    }

    /// A `derived` member's exact result (a count, or a sum's decimal text) as the
    /// member declares it; `None` when it cannot be written so, or its schema refuses
    /// it.
    fn decimal(&self, value: &Value) -> Option<Value> {
        Decimal::parse(value)?
            .to_value(&self.typed)
            .filter(|v| self.validator.is_valid(v))
    }

    /// A computed value as type `t`'s `member` declares it ([`exprs::written_back`]);
    /// `None` when its schema refuses it.
    fn written(&self, rt: &MockRuntime, (t, member): (&str, &str), value: Value) -> Option<Value> {
        Some(exprs::written_back(rt, t, member, value)).filter(|v| self.validator.is_valid(v))
    }
}

/// Fills the computed members of type `t` on each of `items`, records of it, for the
/// call `plan` answers: one children listing per child type for the whole page.
/// Answers, per item, the members it set.
///
/// `items_schema` is the schema each item answers to (a list's item schema): only
/// the members it declares are set, each written as it types it, and an item it
/// refuses once filled is put back as it was. With none, the members are typed by
/// type `t`'s read schema and the caller checks the answer they end up in.
pub(super) async fn fill_page(
    state: &mut StateAccess<'_>,
    at: (&CallPlan<'_>, &OperationCall),
    t: &str,
    items: &mut [Value],
    items_schema: Option<&Value>,
) -> Vec<Vec<String>> {
    fill_items(state, at, (t, false), items, items_schema).await
}

/// [`fill_page`], each item taken for one this runtime created when `new` (a create
/// being answered, not stored yet).
async fn fill_items(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    (t, new): (&str, bool),
    items: &mut [Value],
    items_schema: Option<&Value>,
) -> Vec<Vec<String>> {
    let mut set: Vec<Vec<String>> = vec![Vec::new(); items.len()];
    let rt = state.rt;
    if items.is_empty() || !describes(rt, t) {
        return set;
    }
    let Some(entries) = rt.described().computed(t) else {
        return set;
    };
    let Some(record_schema) = items_schema.or_else(|| write_memory::read_schema(rt, t)) else {
        return set;
    };
    let session = &plan.session;
    let mut groups: HashMap<String, Children> = HashMap::new();
    for of in &entries.children {
        let Some(parent) = rt
            .described()
            .type_desc(of)
            .and_then(|d| d.parent.as_ref())
            .filter(|p| p.r#type == t)
        else {
            continue;
        };
        if let Some(children) = Children::of(state, session, (t, of), parent).await {
            groups.insert(of.clone(), children);
        }
    }
    let only_derived = entries.computed.iter().all(|e| e.derived);
    if only_derived && groups.is_empty() {
        return set;
    }
    // Each filled item still answers to `items_schema`, when one is given.
    let item_check = match items_schema {
        Some(schema) => {
            match SchemaValidator::for_output(&rt.bundle().with_reachable_defs(schema)) {
                Ok(validator) => Some(validator),
                Err(_) => return set,
            }
        }
        None => None,
    };
    let targets: HashMap<&str, Target> = entries
        .computed
        .iter()
        .filter_map(|e| Some((e.member.as_str(), Target::of(rt, record_schema, &e.member)?)))
        .collect();
    if targets.is_empty() {
        return set;
    }
    let field = identity_member(rt, t);
    let mut host = SessionHost::load(state, (rt, session, call), &entries.types).await;
    host.children = groups;
    let mut bound = Bound::new(rt, call, None);
    let held_reading: Vec<&Program> = match entries.reads_held {
        true => entries.computed.iter().map(|e| &e.program).collect(),
        false => Vec::new(),
    };
    for (item, set) in items.iter_mut().zip(set.iter_mut()) {
        if !item.is_object() {
            continue;
        }
        let id = mb_mcm::member_at(item, field)
            .and_then(id_string)
            .map(|value| (bare(rt, t, None, &value), value));
        // A derived member is set on a record created in the session, or with a
        // child held.
        let derived_set = id.as_ref().is_some_and(|(id, value)| {
            let created = new
                || [value.as_str(), id.as_str()]
                    .into_iter()
                    .chain(records::in_pattern(rt, t, id).as_deref())
                    .any(|key| rt.collections().created(session, t, key));
            created || host.children.values().any(|g| !g.under(id).is_empty())
        });
        if only_derived && !derived_set {
            continue;
        }
        host.parent = id.map(|(id, _)| id);
        bound.record = item.clone();
        if !held_reading.is_empty() {
            exprs::resolve_held(state, &mut host, &held_reading, &bound).await;
        }
        let before = item.clone();
        for entry in &entries.computed {
            if entry.derived && !derived_set {
                continue;
            }
            let Some(target) = targets.get(entry.member.as_str()) else {
                continue;
            };
            let budget = match entry.derived {
                true => DERIVED_STEPS,
                false => MAX_STEPS,
            };
            let value = match entry
                .program
                .eval_with_budget(&bound.bindings(), &mut host, budget)
            {
                Ok(Value::Null) => continue,
                Ok(value) => value,
                Err(e) if entry.derived => {
                    tracing::info!(
                        mock = %rt.name,
                        r#type = %t,
                        member = %entry.member,
                        error = %e,
                        "a derived member has no exact result: not set"
                    );
                    continue;
                }
                Err(e) => {
                    exprs::failed(plan, rt, &entry.pointer, &e.to_string());
                    continue;
                }
            };
            let written = match entry.derived {
                true => target.decimal(&value),
                false => target.written(rt, (t, &entry.member), value),
            };
            let Some(result) = written else {
                tracing::info!(
                    mock = %rt.name,
                    r#type = %t,
                    member = %entry.member,
                    "a computed member's result does not fit its schema: not set"
                );
                continue;
            };
            if mb_mcm::set_member_at(item, &entry.member, result) {
                set.push(entry.member.clone());
            }
        }
        if !set.is_empty()
            && let Some(validator) = &item_check
            && !validator.is_valid(item)
        {
            tracing::info!(
                mock = %rt.name,
                r#type = %t,
                "an item with its computed members does not fit its schema: not set"
            );
            *item = before;
            set.clear();
        }
    }
    set
}

/// Fills the computed members of type `t` on `record`, one record of it, typed by `t`'s
/// read schema (the caller checks the answer it ends up in); answers the members it
/// set.
pub(super) async fn fill(
    state: &mut StateAccess<'_>,
    at: (&CallPlan<'_>, &OperationCall),
    t: &str,
    record: &mut Value,
) -> Vec<String> {
    fill_page(state, at, t, std::slice::from_mut(record), None)
        .await
        .pop()
        .unwrap_or_default()
}

/// `stored`, a held record, with its computed members filled; `None` when none is set.
pub(super) async fn filled(
    state: &mut StateAccess<'_>,
    at: (&CallPlan<'_>, &OperationCall),
    stored: &StoredResource,
) -> Option<StoredResource> {
    filled_as(state, at, stored, false).await
}

/// [`filled`] for a record a create is about to store: it counts as created in the
/// session (its derived members are those of a record with no children yet, but
/// those state holds under its id).
pub(super) async fn filled_new(
    state: &mut StateAccess<'_>,
    at: (&CallPlan<'_>, &OperationCall),
    stored: &StoredResource,
) -> Option<StoredResource> {
    filled_as(state, at, stored, true).await
}

async fn filled_as(
    state: &mut StateAccess<'_>,
    at: (&CallPlan<'_>, &OperationCall),
    stored: &StoredResource,
    new: bool,
) -> Option<StoredResource> {
    if !describes(state.rt, &stored.resource_type) {
        return None;
    }
    let mut data = stored.data.clone();
    let t = stored.resource_type.as_str();
    let set = fill_items(state, at, (t, new), std::slice::from_mut(&mut data), None)
        .await
        .pop()
        .unwrap_or_default();
    (!set.is_empty()).then(|| StoredResource {
        data,
        ..stored.clone()
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_where_compares_values_and_their_text() {
        let wanted: BTreeMap<String, Value> = [("status".to_string(), json!("posted"))]
            .into_iter()
            .collect();
        assert!(holds(&json!({"status": "posted"}), Some(&wanted)));
        assert!(!holds(&json!({"status": "pending"}), Some(&wanted)));
        assert!(!holds(&json!({}), Some(&wanted)));
        let flag: BTreeMap<String, Value> = [("ok".to_string(), json!(true))].into_iter().collect();
        assert!(holds(&json!({"ok": "true"}), Some(&flag)));
        assert!(holds(&json!({"anything": 1}), None));
    }
}
