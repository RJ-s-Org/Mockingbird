//! An update whose request sends no record but says how to change the held one: a
//! patch document, or the instructions its description names. Each applies to the held
//! record, all or nothing; one that cannot (a path it does not reach, a failed `test`,
//! an instruction of a kind the description does not give) leaves the call to
//! generation rather than guess.
//!
//! - A JSON Patch (RFC 6902): a body sent as `application/json-patch+json`, or an array
//!   of `{op, path}` operations.
//! - A SCIM PatchOp (RFC 7644 §3.5.2): `{schemas: [...:PatchOp], Operations: [...]}`, its
//!   `add`, `replace` and `remove` at an attribute path (`name.givenName`) or with
//!   none. A path filtering a multi-valued attribute (`members[value eq "x"]`) or
//!   naming a schema (`urn:...:employeeNumber`) is not applied (`patch_filter`).
//! - The description's `request.instructions` (a semantic patch): the instructions at
//!   its `at`, each kind's `changes` set in order.

use mb_mcm::world::{InstructionChange, InstructionsDesc};
use serde_json::{Map, Value};

use crate::call::OperationCall;

/// A patch the executor cannot apply.
pub(super) const UNAPPLIED: &str = "unapplied_patch";
/// A SCIM path the executor does not resolve (a filter, a schema URN).
const FILTERED: &str = "patch_filter";
/// An instruction of a kind the description does not give.
const UNDESCRIBED: &str = "undescribed_instruction";
/// An instruction the description gives that does not apply to the record.
const UNAPPLIED_INSTRUCTION: &str = "unapplied_instruction";

/// The RFC 6902 operations.
const JSON_PATCH_OPS: [&str; 6] = ["add", "remove", "replace", "move", "copy", "test"];

/// The media type `call` sends its body as, lowercased and without parameters.
pub(super) fn media_type(call: &OperationCall) -> Option<String> {
    call.headers.iter().find_map(|(name, value)| {
        let value = value.as_str()?;
        name.eq_ignore_ascii_case("content-type").then(|| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
    })
}

/// What the request leaves of `held` when its body is a patch document or the
/// instructions `instructions` describes: `None` when it is neither, `Err` with the
/// reason when it does not apply.
pub(super) fn patched(
    call: &OperationCall,
    instructions: Option<&InstructionsDesc>,
    held: &Value,
) -> Option<Result<Value, &'static str>> {
    let body = call.body.as_ref()?;
    if let Some(described) = instructions
        && let Some(Value::Array(list)) = body.pointer(described.at.as_str())
    {
        let mut out = held.clone();
        return Some(instruct(described, body, list, &mut out).map(|()| out));
    }
    let media = media_type(call);
    if media.as_deref() == Some("application/json-patch+json") || is_json_patch(body) {
        let Ok(ops) = serde_json::from_value::<json_patch::Patch>(body.clone()) else {
            return Some(Err(UNAPPLIED));
        };
        let mut out = held.clone();
        return Some(
            json_patch::patch(&mut out, &ops)
                .map(|()| out)
                .map_err(|_| UNAPPLIED),
        );
    }
    let ops = scim_operations(body)?;
    let mut out = held.clone();
    Some(
        ops.iter()
            .try_for_each(|op| scim(&mut out, op))
            .map(|()| out),
    )
}

/// Whether `body` is an RFC 6902 patch: a non-empty array of `{op, path}` objects
/// naming its operations.
fn is_json_patch(body: &Value) -> bool {
    body.as_array().is_some_and(|ops| {
        !ops.is_empty()
            && ops.iter().all(|op| {
                op.get("op")
                    .and_then(Value::as_str)
                    .is_some_and(|name| JSON_PATCH_OPS.contains(&name))
                    && op.get("path").is_some_and(Value::is_string)
            })
    })
}

/// The `Operations` of a SCIM PatchOp body: one whose `schemas` names the PatchOp
/// message and whose `Operations` each name their `op`.
fn scim_operations(body: &Value) -> Option<&Vec<Value>> {
    let patch_op = body
        .get("schemas")
        .and_then(Value::as_array)
        .is_some_and(|s| {
            s.iter()
                .filter_map(Value::as_str)
                .any(|s| s.to_ascii_lowercase().ends_with(":patchop"))
        });
    let ops = body.get("Operations").and_then(Value::as_array)?;
    (patch_op
        && ops
            .iter()
            .all(|op| op.get("op").is_some_and(Value::is_string)))
    .then_some(ops)
}

/// The key of `map` that names attribute `name`, as SCIM attribute names are matched
/// without case; `name` itself when none does.
fn attribute(map: &Map<String, Value>, name: &str) -> String {
    map.keys()
        .find(|key| key.eq_ignore_ascii_case(name))
        .cloned()
        .unwrap_or_else(|| name.to_owned())
}

/// The steps of a SCIM attribute path (`name.givenName`); `Err` for one the executor
/// does not resolve.
fn scim_steps(path: &str) -> Result<Vec<&str>, &'static str> {
    if path.contains(['[', ']', ' ', '"']) || path.to_ascii_lowercase().starts_with("urn:") {
        return Err(FILTERED);
    }
    let steps: Vec<&str> = path.split('.').collect();
    match steps.iter().any(|s| s.is_empty()) {
        true => Err(UNAPPLIED),
        false => Ok(steps),
    }
}

/// Applies one SCIM PatchOp operation to `record`.
fn scim(record: &mut Value, op: &Value) -> Result<(), &'static str> {
    let name = op
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let value = op.get("value");
    let path = op
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    let add = match name.as_str() {
        "add" => true,
        "replace" => false,
        "remove" => {
            let steps = scim_steps(path.ok_or(UNAPPLIED)?)?;
            return scim_remove(record, &steps, value);
        }
        _ => return Err(UNAPPLIED),
    };
    let value = value.ok_or(UNAPPLIED)?;
    match path {
        Some(path) => scim_set(record, &scim_steps(path)?, value, add),
        // With no path, the value's members are the attributes to set.
        None => {
            let Value::Object(members) = value else {
                return Err(UNAPPLIED);
            };
            for (key, value) in members {
                // An extension's attributes are the members of its schema's key.
                let steps = match key.to_ascii_lowercase().starts_with("urn:") {
                    true => vec![key.as_str()],
                    false => scim_steps(key)?,
                };
                scim_set(record, &steps, value, add)?;
            }
            Ok(())
        }
    }
}

/// Sets the attribute at `steps` of `record` to `value` (an `add`, with `add`): an
/// `add` to a multi-valued attribute adds each value it does not hold, and an `add` or
/// `replace` of a complex attribute sets the sub-attributes it sends and keeps the
/// rest. The steps before the last are complex attributes, added where missing.
fn scim_set(
    record: &mut Value,
    steps: &[&str],
    value: &Value,
    add: bool,
) -> Result<(), &'static str> {
    let Value::Object(map) = record else {
        return Err(UNAPPLIED);
    };
    let Some((first, rest)) = steps.split_first() else {
        return Err(UNAPPLIED);
    };
    let key = attribute(map, first);
    if !rest.is_empty() {
        let below = map.entry(key).or_insert_with(|| Value::Object(Map::new()));
        return scim_set(below, rest, value, add);
    }
    match (map.get_mut(&key), value) {
        (Some(Value::Array(held)), _) if add => {
            let sent = match value {
                Value::Array(items) => items.as_slice(),
                one => std::slice::from_ref(one),
            };
            for item in sent {
                if !held.contains(item) {
                    held.push(item.clone());
                }
            }
            Ok(())
        }
        (Some(below @ Value::Object(_)), Value::Object(members)) => members
            .iter()
            .try_for_each(|(name, value)| scim_set(below, &[name], value, add)),
        _ => {
            map.insert(key, value.clone());
            Ok(())
        }
    }
}

/// Removes the attribute at `steps` of `record`, or, given a `value`, the values of
/// that multi-valued attribute that match one of it (equal, or holding every member
/// it sends). Removing what is not there changes nothing.
fn scim_remove(
    record: &mut Value,
    steps: &[&str],
    value: Option<&Value>,
) -> Result<(), &'static str> {
    let Value::Object(map) = record else {
        return Err(UNAPPLIED);
    };
    let Some((first, rest)) = steps.split_first() else {
        return Err(UNAPPLIED);
    };
    let key = attribute(map, first);
    if !rest.is_empty() {
        return match map.get_mut(&key) {
            Some(below) => scim_remove(below, rest, value),
            None => Ok(()),
        };
    }
    let Some(sent) = value else {
        map.remove(&key);
        return Ok(());
    };
    let held = match map.get_mut(&key) {
        Some(Value::Array(held)) => held,
        None | Some(Value::Null) => return Ok(()),
        Some(_) => return Err(UNAPPLIED),
    };
    let sent = match sent {
        Value::Array(items) => items.as_slice(),
        one => std::slice::from_ref(one),
    };
    held.retain(|item| !sent.iter().any(|s| matches(item, s)));
    Ok(())
}

/// Whether held value `item` is the one `sent` names: equal to it, or an object
/// holding every member `sent` sends.
fn matches(item: &Value, sent: &Value) -> bool {
    match (item, sent) {
        (Value::Object(held), Value::Object(members)) => {
            !members.is_empty()
                && members
                    .iter()
                    .all(|(name, value)| held.get(name) == Some(value))
        }
        _ => item == sent,
    }
}

/// Applies `list`, the instructions of request `body`, to `record` as `described`
/// says, in order.
fn instruct(
    described: &InstructionsDesc,
    body: &Value,
    list: &[Value],
    record: &mut Value,
) -> Result<(), &'static str> {
    for instruction in list {
        let kind = mb_mcm::member_at(instruction, &described.kind)
            .and_then(Value::as_str)
            .ok_or(UNDESCRIBED)?;
        let kind = described.kinds.get(kind).ok_or(UNDESCRIBED)?;
        for (member, change) in &kind.changes {
            let member = filled(member, instruction, body).ok_or(UNAPPLIED_INSTRUCTION)?;
            let read = |pointer: &mb_mcm::world::Pointer, from: &Value| {
                from.pointer(pointer.as_str())
                    .cloned()
                    .ok_or(UNAPPLIED_INSTRUCTION)
            };
            let set = match change {
                InstructionChange::Value(value) => value.clone(),
                InstructionChange::FromInstruction(p) => read(p, instruction)?,
                InstructionChange::FromRequest(p) => read(p, body)?,
                InstructionChange::Append(p) | InstructionChange::Remove(p) => {
                    let sent = read(p, instruction)?;
                    let sent = match sent {
                        Value::Array(items) => items,
                        one => vec![one],
                    };
                    let mut held = match mb_mcm::member_at(record, &member) {
                        Some(Value::Array(held)) => held.clone(),
                        None | Some(Value::Null) => Vec::new(),
                        Some(_) => return Err(UNAPPLIED_INSTRUCTION),
                    };
                    match change {
                        InstructionChange::Append(_) => {
                            for item in sent {
                                if !held.contains(&item) {
                                    held.push(item);
                                }
                            }
                        }
                        _ => held.retain(|item| !sent.contains(item)),
                    }
                    Value::Array(held)
                }
            };
            if !mb_mcm::set_member_at(record, &member, set) {
                return Err(UNAPPLIED_INSTRUCTION);
            }
        }
    }
    Ok(())
}

/// `member` with each braced step (`{environmentKey}`) the value of that member of
/// `instruction`, else of `body`: a string, or a number as written. `None` when
/// neither holds one.
fn filled(member: &str, instruction: &Value, body: &Value) -> Option<String> {
    if !member.contains('{') {
        return Some(member.to_owned());
    }
    let steps: Option<Vec<String>> = member
        .split('.')
        .map(|step| {
            let Some(name) = step.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
                return Some(step.to_owned());
            };
            let value = mb_mcm::member_at(instruction, name)
                .filter(|v| !v.is_null())
                .or_else(|| mb_mcm::member_at(body, name))?;
            match value {
                Value::String(s) if !s.is_empty() && !s.contains('.') => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            }
        })
        .collect();
    steps.map(|s| s.join("."))
}
