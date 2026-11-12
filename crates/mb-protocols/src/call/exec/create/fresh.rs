//! What a created record starts with that its request leaves out and nothing else
//! decides: the state its state-like members start in ([`start_states`]) and the ids
//! of its id members ([`mint_ids`]).
//!
//! - A member is state-like when the world moves it: its type's lifecycle member, and
//!   each top-level member an update, replace or action of the type sets (its
//!   `transition`, `changes` or `set`), or an `update` effect on the type does; and,
//!   where no rule names it, a top-level member the contract names as a state
//!   (`state`, `status`, `phase`, `stage`, `*_state`, `*Status`, ...), which no rule
//!   moves, so it skips the next step. A contract `default` is set before all this. One
//!   with an `enum` that may not be null starts, where the lifecycle's `initial` did
//!   not set it ([`super::initial`]), in the one value of its enum no such rule moves
//!   it to (the state the transitions leave from: `OPEN` where a close sets `CLOSED`),
//!   else in the value the create's answer shows of it in the contract's examples,
//!   else in the model's pick ([`mb_engine::FillContext::states`]); not in its first
//!   listed value, which fill gives an enum nothing speaks of, unless no model answers.
//! - A member named as an id (`id`, `*_id`, `*Id`, `*ID`, `uuid`) or formatted as a
//!   `uuid`, a string or integer that may not be null, gets an id: when it names a
//!   reference (`<x>Id`, `<x>_id`) the request does not declare, the id of a record of
//!   the type it names the session holds; else one minted as the executor mints record
//!   ids: as declared (a pattern, a `uuid` format), else in the form the session's
//!   records of the type show of the member, else in the form the contract's examples
//!   show (their prefix and length, or with no prefix to share their characters and
//!   length: [`crate::call::header_ids::member_example_shape`]), else as letters and
//!   digits ([`crate::call::header_ids::draw_member_id`]); never a faker word. One of no
//!   declared form the session's records show no form of and the persona names
//!   (`node_id values use GitHub's prefixes`) is left to fill, which asks the model
//!   for it in the persona's shape ([`mb_engine::persona_names`]).

use mb_engine::consistency::property_schema;
use mb_mcm::Operation;
use mb_mcm::world::{ChangeDesc, EffectOp, OpKind};
use serde_json::{Map, Value};
use tracing::Instrument;

use super::{fits, scalar_types, unsent_reference, whole_parts, write_only};
use crate::call::header_ids::{self, MintSeed};
use crate::call::{CallPlan, StateAccess, id_string, reference_prefix, relations, state_span};

/// The state-like members of type `t`'s records: the top-level members the world
/// moves (see the module docs), each with the values its rules move it to where they
/// are literals.
pub(super) fn state_members(rt: &crate::MockRuntime, t: &str) -> Vec<(String, Vec<Value>)> {
    let described = rt.described();
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    let mut add = |member: &str, to: Option<Value>| {
        if member.contains('.') {
            return;
        }
        let at = match out.iter().position(|(m, _)| m == member) {
            Some(at) => at,
            None => {
                out.push((member.to_string(), Vec::new()));
                out.len() - 1
            }
        };
        if let (Some(to), Some((_, targets))) = (to, out.get_mut(at))
            && !targets.contains(&to)
        {
            targets.push(to);
        }
    };
    if let Some(lifecycle) = described.type_desc(t).and_then(|d| d.lifecycle.as_ref()) {
        add(&lifecycle.member, None);
        if let Some(gone) = &lifecycle.soft_delete {
            add(&lifecycle.member, Some(Value::String(gone.state.clone())));
        }
    }
    let Some(world) = described.world() else {
        return out;
    };
    let bundle = rt.bundle();
    for (id, desc) in &world.operations {
        let of_t = desc
            .r#type
            .as_deref()
            .or_else(|| bundle.operation(id)?.resource_type.as_deref())
            == Some(t);
        let kind = desc
            .kind
            .or_else(|| bundle.operation(id).and_then(OpKind::of));
        let moves = matches!(
            kind,
            Some(OpKind::Update | OpKind::Replace | OpKind::Action)
        );
        if of_t && moves {
            for (member, to) in desc.transition.iter().flatten() {
                add(member, Some(to.clone()));
            }
            for (member, change) in &desc.changes {
                let to = match change {
                    ChangeDesc::Value(v) => Some(v.clone()),
                    ChangeDesc::FromRequest(_) => None,
                };
                add(member, to);
            }
            for (member, expr) in &desc.set {
                add(member, literal(expr));
            }
        }
        for effect in desc
            .effects
            .iter()
            .filter(|e| e.op == EffectOp::Update && e.r#type == t)
        {
            for (member, expr) in &effect.set {
                add(member, literal(expr));
            }
        }
    }
    out
}

/// The value expression `expr` is when it is a literal: a quoted string, a boolean or
/// a number.
fn literal(expr: &str) -> Option<Value> {
    let expr = expr.trim();
    for quote in ['\'', '"'] {
        if let Some(inner) = expr
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
            && !inner.contains(quote)
            && !inner.contains('\\')
        {
            return Some(Value::String(inner.to_string()));
        }
    }
    match expr {
        "true" => Some(Value::Bool(true)),
        "false" => Some(Value::Bool(false)),
        _ => serde_json::from_str::<serde_json::Number>(expr)
            .ok()
            .map(Value::Number),
    }
}

/// The state-like members of a created record ([`start_states`]).
#[derive(Debug, Default)]
pub(super) struct Starts {
    /// The state-like members with an enum the record declares: a record pool's values
    /// of them are none of a create's.
    pub(super) members: Vec<String>,
    /// Those nothing gave a start: the model's to pick.
    pub(super) asked: Vec<String>,
}

/// Sets on `known`, a record of type `t` that `op` creates, the start of each
/// state-like member ([`state_members`]) with an enum that `members` (the record's
/// members with their schemas) declares, that may not be null (a nullable one starts
/// null, as fill leaves it) and is no client's alone (`writeOnly`), and that nothing
/// set (`known` lacks it, `skip` holds none of it): the one value no rule moves it to,
/// else the one the create's answer shows in the contract's examples. Those it found no
/// start for are the model's to pick.
pub(super) fn start_states(
    rt: &crate::MockRuntime,
    (op, t): (&Operation, &str),
    members: &[(String, Value)],
    known: &mut Map<String, Value>,
    skip: &[String],
) -> Starts {
    let bundle = rt.bundle();
    let mut starts = Starts::default();
    let mut states = state_members(rt, t);
    // Where no world rule names it, a member the contract names as a state is one too.
    for (name, _) in members {
        if state_named(name) && !states.iter().any(|(m, _)| m == name) {
            states.push((name.clone(), Vec::new()));
        }
    }
    for (member, targets) in states {
        let Some((_, schema)) = members.iter().find(|(m, _)| *m == member) else {
            continue;
        };
        let values: Vec<Value> = whole_parts(bundle, schema)
            .iter()
            .filter_map(|p| p.get("enum").and_then(Value::as_array))
            .flatten()
            .filter(|v| !v.is_null() && fits(rt, schema, v))
            .cloned()
            .collect();
        if values.is_empty() || fits(rt, schema, &Value::Null) || write_only(bundle, schema) {
            continue;
        }
        starts.members.push(member.clone());
        if known.contains_key(&member) || skip.contains(&member) {
            continue;
        }
        let squeezed = |v: &Value| {
            id_string(v).map(|text| {
                text.chars()
                    .filter(char::is_ascii_alphanumeric)
                    .collect::<String>()
                    .to_ascii_lowercase()
            })
        };
        let moved_to: Vec<Option<String>> = targets.iter().map(squeezed).collect();
        let unmoved: Vec<&Value> = values
            .iter()
            .filter(|v| !moved_to.contains(&squeezed(v)))
            .collect();
        let start = match unmoved.as_slice() {
            [only] if !targets.is_empty() => Some((*only).clone()),
            _ => shown_on_create(rt, op, &member)
                .into_iter()
                .find(|v| values.contains(v)),
        };
        match start {
            Some(value) => {
                known.insert(member, value);
            }
            None => starts.asked.push(member),
        }
    }
    starts
}

/// Whether member `name` is named as a state: `state`, `status`, `phase`, `stage`, or
/// ending in `_state`, `_status`, `State` or `Status`.
fn state_named(name: &str) -> bool {
    ["state", "status", "phase", "stage"].contains(&name)
        || ["_state", "_status", "State", "Status"]
            .iter()
            .any(|suffix| name.len() > suffix.len() && name.ends_with(suffix))
}

/// The values the contract's examples show of member `member` of the record `op`'s
/// answer holds: the member's own examples, then the record's, then the whole
/// answer's at the record.
fn shown_on_create(rt: &crate::MockRuntime, op: &Operation, member: &str) -> Vec<Value> {
    let bundle = rt.bundle();
    let Some(answer) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return Vec::new();
    };
    let identity = op.identity.as_ref().filter(|i| i.record.is_some());
    let record = match identity {
        Some(identity) => crate::call::bundle_index::record_schema_of(bundle, identity, answer),
        None => Some(answer.clone()),
    };
    let examples = |schema: &Value| -> Vec<Value> {
        let mut parts = vec![schema];
        parts.extend(whole_parts(bundle, schema));
        parts
            .into_iter()
            .flat_map(|p| {
                p.get("example").into_iter().chain(
                    p.get("examples")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                )
            })
            .cloned()
            .collect()
    };
    let mut out = Vec::new();
    if let Some(record) = &record {
        if let Some(schema) = property_schema(bundle, record, member) {
            out.extend(examples(&schema));
        }
        out.extend(
            examples(record)
                .iter()
                .filter_map(|e| e.get(member).cloned()),
        );
    }
    if let Some(identity) = identity {
        out.extend(
            examples(answer)
                .iter()
                .filter_map(|e| identity.record_in(e)?.get(member).cloned()),
        );
    }
    out
}

/// Sets on `known`, a record of type `t` keyed by `field`, an id for each id member
/// (see the module docs) of `members` nothing set: neither `known` nor `skip` holds
/// it, and it is no reference the create's request (of schema `request`) could have
/// sent ([`unsent_reference`]), which fill leaves absent.
pub(super) async fn mint_ids(
    state: &mut StateAccess<'_>,
    (plan, seed): (&CallPlan<'_>, MintSeed<'_>),
    (t, field): (&str, &str),
    (members, request): (&[(String, Value)], Option<&Value>),
    known: &mut Map<String, Value>,
    skip: &[String],
) {
    let rt = state.rt;
    let bundle = rt.bundle();
    let persona = plan
        .engine
        .config()
        .persona
        .as_deref()
        .unwrap_or_default()
        .to_lowercase();
    for (name, member) in members {
        if name == field || known.contains_key(name) || skip.contains(name) {
            continue;
        }
        let parts = whole_parts(bundle, member);
        let uuid = parts
            .iter()
            .any(|p| p.get("format").and_then(Value::as_str) == Some("uuid"));
        let scalar = parts.iter().any(|p| {
            scalar_types(p)
                .iter()
                .any(|ty| matches!(*ty, "string" | "integer"))
        }) && !parts
            .iter()
            .any(|p| p.get("enum").is_some() || p.get("const").is_some());
        if !(header_ids::id_named(name) || uuid)
            || !scalar
            || fits(rt, member, &Value::Null)
            || write_only(bundle, member)
            || unsent_reference(rt, request, name, member)
        {
            continue;
        }
        let prefix = reference_prefix(name)
            .or_else(|| name.strip_suffix("ID"))
            .filter(|p| !p.is_empty());
        let held = match prefix {
            Some(prefix) => {
                referenced(state, plan, t, prefix, member)
                    .instrument(state_span())
                    .await
            }
            None => None,
        };
        let id = match held {
            Some(id) => Some(id),
            None => {
                // The shape the session's records of the type show (two at least);
                // else, where the persona speaks of the member (`node_id values use
                // GitHub's prefixes`), the model writes it in the persona's shape; else
                // the contract's examples' shape, else one held record's.
                let declared = header_ids::formed_member(rt, member);
                let shared = match declared {
                    true => None,
                    false => {
                        header_ids::shared_session_id_shape(state, &plan.session, t, name)
                            .instrument(state_span())
                            .await
                    }
                };
                if shared.is_none() && !declared && mb_engine::persona_names(&persona, name) {
                    continue;
                }
                let shape = match shared.or_else(|| header_ids::member_example_shape(rt, t, name)) {
                    Some(shape) => Some(shape),
                    None if declared => None,
                    None => {
                        header_ids::session_id_shape(state, &plan.session, t, name)
                            .instrument(state_span())
                            .await
                    }
                };
                header_ids::draw_member_id(rt, (seed, t, name), member, shape.as_ref())
            }
        };
        if let Some(id) = id {
            known.insert(name.clone(), id);
        }
    }
}

/// The id of a record the session holds of a type `<prefix>` names
/// ([`relations::reference_types`]), but `t`, as member schema `member` holds it: its
/// own `id` member (a node id beside a `number` key), else the key it is held under.
async fn referenced(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    t: &str,
    prefix: &str,
    member: &Value,
) -> Option<Value> {
    let rt = state.rt;
    for ty in relations::reference_types(Some(rt.index()), &prefix.to_ascii_lowercase()) {
        if ty == t {
            continue;
        }
        for record in state.list(&plan.session, &ty, REFERENCE_SCAN).await {
            let own = record
                .data
                .get("id")
                .filter(|v| id_string(v).is_some())
                .cloned();
            let text = Value::String(record.id.clone());
            let number = record.id.parse::<i64>().ok().map(Value::from);
            if let Some(id) = [own, Some(text), number]
                .into_iter()
                .flatten()
                .find(|v| fits(rt, member, v))
            {
                return Some(id);
            }
        }
    }
    None
}

/// The held records of a referenced type looked at for one that fits.
const REFERENCE_SCAN: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_are_quoted_strings_booleans_and_numbers() {
        assert_eq!(literal("'CLOSED'"), Some(Value::from("CLOSED")));
        assert_eq!(literal(" \"open\" "), Some(Value::from("open")));
        assert_eq!(literal("true"), Some(Value::Bool(true)));
        assert_eq!(literal("3"), Some(Value::from(3)));
        assert_eq!(literal("request.state"), None);
        assert_eq!(literal("'a' + 'b'"), None);
    }

    #[test]
    fn state_names_are_states_and_their_suffixes() {
        for name in [
            "state",
            "status",
            "phase",
            "stage",
            "merge_state",
            "reviewStatus",
        ] {
            assert!(state_named(name), "{name}");
        }
        for name in ["statement", "states", "priority", "State", "estate"] {
            assert!(!state_named(name), "{name}");
        }
    }
}
