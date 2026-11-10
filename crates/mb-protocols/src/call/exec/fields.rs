//! Sparse fieldsets: a read's record, or each of a list's items, answered with the
//! members the described `fields` parameter names (`fields=id,name`, `$select=name`).
//!
//! The parameter's value is split on the separator (`,` by default), each name trimmed,
//! `/` read as `.` (a dotted member: `category.name`); the members the description
//! answers `always` are added. A request not sending it answers the `default` members,
//! or the whole record when the description gives none. A value naming everything
//! (`*`) or written in a grammar of its own (`items(id,name)`) answers the whole record.
//! A named member the record does not hold is left out. The projection is laid over an
//! answer that already rendered whole, so the members it drops are the client's choice,
//! not a schema violation of the engine's.

use mb_mcm::Operation;
use serde_json::{Map, Value};

use crate::MockRuntime;
use crate::call::OperationCall;

/// The query parameter the description names the fields by, for `op`.
pub(super) fn param<'a>(rt: &'a MockRuntime, op: &Operation) -> Option<&'a str> {
    rt.described()
        .op(&op.id)
        .and_then(|d| d.fields.as_ref())
        .map(|f| f.param.as_str())
}

/// The members `call` asks `op` to answer; `None` for the whole record.
pub(super) fn named(rt: &MockRuntime, op: &Operation, call: &OperationCall) -> Option<Vec<String>> {
    let desc = rt.described().op(&op.id)?.fields.as_ref()?;
    let separator = desc.separator.as_deref().unwrap_or(",");
    let sent = match call.query.get(&desc.param) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(values)) => Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(separator),
        ),
        Some(other) => Some(other.to_string()),
        None => None,
    };
    let mut names: Vec<String> = match sent.filter(|text| !text.trim().is_empty()) {
        Some(text) => {
            let names: Vec<String> = text
                .split(separator)
                .map(|name| name.trim().replace('/', "."))
                .filter(|name| !name.is_empty())
                .collect();
            if names
                .iter()
                .any(|name| name == "*" || name.contains(['(', ')', '*', ' ']))
            {
                return None;
            }
            names
        }
        None => desc.default.clone()?,
    };
    for member in desc.always.iter().rev() {
        if !names.contains(member) {
            names.insert(0, member.clone());
        }
    }
    Some(names)
}

/// `record` with only the members `names` names, each where the record holds it.
pub(super) fn project(record: &Value, names: &[String]) -> Value {
    let Value::Object(_) = record else {
        return record.clone();
    };
    let mut out = Value::Object(Map::new());
    for name in names {
        if let Some(value) = mb_mcm::member_at(record, name) {
            mb_mcm::set_member_at(&mut out, name, value.clone());
        }
    }
    out
}

/// The record `body`, `op`'s answer, holds narrowed to what `call` asks for: where the
/// operation's identity says the answer holds it, else the body itself.
pub(super) fn narrow_read(
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
    body: Option<&mut Value>,
) {
    let (Some(names), Some(body)) = (named(rt, op, call), body) else {
        return;
    };
    let record = match op.identity.as_ref() {
        Some(identity) => identity.record_in_mut(body),
        None => Some(body),
    };
    if let Some(record) = record {
        *record = project(record, &names);
    }
}

/// Each item of `items`, a list answer's, narrowed to what `call` asks `op` for.
pub(super) fn narrow_items(
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
    items: Option<&mut Vec<Value>>,
) {
    let (Some(names), Some(items)) = (named(rt, op, call), items) else {
        return;
    };
    for item in items {
        *item = project(item, &names);
    }
}
