//! A list's world expressions at runtime ([`mb_mcm::world::ListWhere`] and
//! [`mb_mcm::world::ListSort`]), evaluated by the executor's list from state:
//!
//! - `list.where` keeps a record only when it gives `true`, with `it` bound to the
//!   record as stored (not the item cut to the list's schema) and `request` the call's.
//!   It runs before paging, scope, filters and order, and AND-combines with them. A
//!   failure (or a value that is no boolean) is noted once per call under
//!   `expr_failed` and leaves that record out; `null` leaves it out quietly.
//! - `list.sort` applies when the request sends its parameter, in the query (a deep
//!   object parameter, `sort[0][field]=price`, as the object its keys make) or as a
//!   member of the body: `by`, with `value` the parameter as sent, gives
//!   `[{member, desc?}]` (or one such map), and the items are ordered by those members in turn, as `order`
//!   orders them: numbers by value, date-times by time, text by text, an item not
//!   holding the member (or holding `null`) last either way, and items that compare
//!   equal kept in their order. A `by` that fails, gives no members, or gives one the
//!   items' schema does not declare (and no item holds) is noted under `expr_failed`,
//!   and the list keeps its described order. The parameter counts as described: the
//!   list never falls back as `undescribed_param` on it.

use mb_engine::OperationCall;
use mb_engine::consistency::property_schema;
use mb_mcm::Operation;
use mb_mcm::world::{ListSort, ListWhere};
use serde_json::Value;

use super::exprs::{self, Bound, SessionHost};
use super::{CallPlan, StateAccess};
use crate::MockRuntime;

/// The sort parameter list `op` describes, if any.
pub(super) fn sort_param<'r>(rt: &'r MockRuntime, op: &Operation) -> Option<&'r str> {
    rt.described()
        .expressions(&op.id)
        .and_then(|o| o.list_sort.as_ref())
        .map(|s| s.param.as_str())
}

/// Whether request parameter `name` is sort parameter `param`, or a key bracketed
/// below it (`sort[0][field]`).
pub(super) fn is_sort_param(param: &str, name: &str) -> bool {
    name == param
        || name
            .strip_prefix(param)
            .is_some_and(|rest| rest.starts_with('['))
}

/// A list's `where`, ready to test each record.
pub(super) struct Where<'r> {
    entry: &'r ListWhere,
    host: SessionHost<'r>,
    bound: Bound,
    reads_held: bool,
}

impl<'r> Where<'r> {
    /// The `where` of `plan`'s operation, with the session records it reads, when it
    /// has one.
    pub(super) async fn of(
        state: &mut StateAccess<'_>,
        rt: &'r MockRuntime,
        (plan, call): (&'r CallPlan<'_>, &'r OperationCall),
    ) -> Option<Where<'r>> {
        let ops = rt.described().expressions(&plan.op.id)?;
        let entry = ops.list_where.as_ref()?;
        let host = exprs::host(state, rt, (plan, call), ops).await;
        Some(Where {
            entry,
            host,
            bound: Bound::new(rt, call, None),
            reads_held: ops.reads_held,
        })
    }

    /// Whether `record` (as stored) is listed.
    pub(super) async fn keeps(
        &mut self,
        state: &mut StateAccess<'_>,
        plan: &CallPlan<'_>,
        record: &Value,
    ) -> bool {
        let rt = state.rt;
        self.bound.it = Some(record.clone());
        if self.reads_held {
            exprs::resolve_held(state, &mut self.host, &[&self.entry.program], &self.bound).await;
        }
        let at = (&self.entry.program, self.entry.pointer.as_str());
        let kept = match exprs::eval(plan, rt, at, &self.bound, &mut self.host) {
            Some(Value::Bool(kept)) => kept,
            Some(Value::Null) | None => false,
            Some(other) => {
                let error = format!("`where` gave {other}, not a bool");
                exprs::failed(plan, rt, &self.entry.pointer, &error);
                false
            }
        };
        self.bound.it = None;
        kept
    }
}

/// One member a sort orders by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SortKey {
    /// The (dotted) member.
    pub(super) member: String,
    /// Largest first.
    pub(super) desc: bool,
}

/// The members `plan`'s list orders its items by, from its sort parameter, when the
/// request sends it and `by` names members `item_schema` declares (or one of `items`,
/// each an item beside its record, holds); `None` when the list keeps its described
/// order.
pub(super) async fn sort_keys(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    item_schema: &Value,
    items: &[(Value, Value)],
) -> Option<Vec<SortKey>> {
    let rt = state.rt;
    let ops = rt.described().expressions(&plan.op.id)?;
    let sort = ops.list_sort.as_ref()?;
    let value = sent(call, &sort.param)?;
    let mut host = exprs::host(state, rt, (plan, call), ops).await;
    let mut bound = Bound::new(rt, call, None);
    bound.value = Some(value);
    if ops.reads_held {
        exprs::resolve_held(state, &mut host, &[&sort.by], &bound).await;
    }
    let given = exprs::eval(plan, rt, (&sort.by, &sort.pointer), &bound, &mut host)?;
    match keys_of(rt, sort, &given, item_schema, items) {
        Ok(keys) => Some(keys),
        Err(error) => {
            exprs::failed(plan, rt, &sort.pointer, &error);
            None
        }
    }
}

/// The value the request sends sort parameter `param`: in the query (a deep object
/// parameter as the object its bracketed keys make), else as a member of the body.
fn sent(call: &OperationCall, param: &str) -> Option<Value> {
    call.query
        .get(param)
        .or_else(|| call.body.as_ref().and_then(|b| b.get(param)))
        .cloned()
}

/// The sort keys `given` (what `by` gave) states, or why it states none: a list of
/// `{member, desc?}` (or one such map), each member a text the items' schema declares
/// or an item holds.
fn keys_of(
    rt: &MockRuntime,
    sort: &ListSort,
    given: &Value,
    item_schema: &Value,
    items: &[(Value, Value)],
) -> Result<Vec<SortKey>, String> {
    let entries = match given {
        Value::Array(entries) => entries.as_slice(),
        Value::Object(_) => std::slice::from_ref(given),
        other => return Err(format!("`by` gave {other}, not a list of {{member, desc}}")),
    };
    let mut keys = Vec::new();
    for entry in entries {
        let member = match entry.get("member") {
            Some(Value::String(m)) if !m.trim().is_empty() => m.trim().to_string(),
            _ => return Err(format!("`by` gave {entry}, which names no member")),
        };
        let desc = match entry.get("desc") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(d)) => *d,
            Some(other) => return Err(format!("`desc` of `{member}` is {other}, not a bool")),
        };
        let declared = property_schema(rt.bundle(), item_schema, &member).is_some()
            || items
                .iter()
                .any(|(item, _)| mb_mcm::member_at(item, &member).is_some());
        if !declared {
            return Err(format!(
                "`{member}` (from parameter `{}`) is not a member of the list's items",
                sort.param
            ));
        }
        keys.push(SortKey { member, desc });
    }
    if keys.is_empty() {
        return Err("`by` names no member".to_string());
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use mb_engine::OperationCall;
    use serde_json::json;

    use super::*;

    #[test]
    fn a_sort_parameter_counts_with_its_bracketed_keys() {
        assert!(is_sort_param("sort", "sort"));
        assert!(is_sort_param("sort", "sort[0][field]"));
        assert!(!is_sort_param("sort", "sorted"));
        assert!(!is_sort_param("sort", "order"));
    }

    #[test]
    fn the_sort_parameter_is_read_from_the_query_then_the_body() {
        let mut call = OperationCall {
            body: Some(json!({"sort": "name", "other": 1})),
            ..OperationCall::default()
        };
        assert_eq!(sent(&call, "sort"), Some(json!("name")));
        call.query.insert("sort".into(), json!("-price"));
        assert_eq!(sent(&call, "sort"), Some(json!("-price")));
        assert_eq!(sent(&OperationCall::default(), "sort"), None);
    }
}
