//! Lists paged by the `Range` request header, as Heroku pages its lists: the cursor of
//! a [`pages::HeaderSlot`].
//!
//! The request's `Range` header reads `<field> [<start>]..[<end>][; max=<n>][, order=<asc|desc>]`,
//! its parameters after the bounds separated by `;` or `,` in any order (`id ..; max=10;`,
//! `name a..m; order=desc,max=10;`). `<field>` names a member the list's items declare.
//! `<start>` and `<end>` are values of it, both included, in the order's direction: with
//! `order=desc` the start is the greatest. A `]` before `<start>` leaves the start value
//! out, which is how a `Next-Range` carries on after the last item served. Either bound
//! may be left out. A value compares as a number when both sides are numbers, else as
//! text. A header that reads otherwise (no `..`, an unknown parameter, `max=0`), or a
//! field the items do not declare, leaves the list as it was.
//!
//! The page is made of what the session holds of the route's collection
//! ([`lists::collection_records`]), first, and of the items the answer gave that the
//! session does not hold under the same identity. Nothing is made up: an item holding no
//! text or number under the field, or one outside the bounds, is left out (when no item
//! holds the field, which an optional one may be, the list is left as answered); the rest are
//! sorted by the field (a stable sort, so equal values keep the order above), and the
//! first `max` of them served, at most [`MAX_RANGE`] (all of them, up to that, without a
//! `max`). When more remain, the answer is the `206` and carries a `Next-Range` header
//! ([`next_range`]): `<field> ]<last value>..[<end>]; [order=desc,]max=<n>;`, which the
//! client sends back as its next `Range`. Otherwise it is the other 2xx the read
//! declares with the same schema (`200`). A page that would not validate is left as the
//! answer gave it.
//!
//! An LLM and `consistency: on` only, as [`deleted::applies`] says, for a plain call: a
//! scenario's or a forced status's answer is left as it is. A list the state-first
//! executor answers (`exec/list.rs`) is ordered and cut alike by [`serve`], from its own
//! items, which are the session's records already, with or without an LLM.

use std::cmp::Ordering;

use super::*;

/// The most items one page serves.
const MAX_RANGE: usize = 1000;

/// What a `Range` header asks for.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Range {
    /// The member the items are ordered and bounded by.
    field: String,
    /// The first value served, if bounded.
    start: Option<String>,
    /// Whether the start value itself is left out (`]start..`).
    exclusive: bool,
    /// The last value served, if bounded.
    end: Option<String>,
    /// The most items served.
    max: Option<usize>,
    /// Whether the order is descending.
    desc: bool,
}

/// `text` as a `Range` header (see the module's documentation); `None` when it reads
/// otherwise.
pub(super) fn parse(text: &str) -> Option<Range> {
    let text = text.trim();
    let (head, params) = text.split_once(';').unwrap_or((text, ""));
    let (field, bounds) = head.trim().split_once(char::is_whitespace)?;
    let field = field.trim();
    if field.is_empty() || field.contains("..") {
        return None;
    }
    let (start, end) = bounds.trim().split_once("..")?;
    let start = start.trim();
    let (exclusive, start) = match start.strip_prefix(']') {
        Some(rest) => (true, rest.trim()),
        None => (false, start),
    };
    let given = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let (start, end) = (given(start), given(end.trim()));
    let (mut max, mut desc) = (None, None);
    for part in params.split([';', ',']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, value) = part.split_once('=')?;
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "max" if max.is_none() => {
                let n: usize = value.parse().ok().filter(|n| *n > 0)?;
                max = Some(n);
            }
            "order" if desc.is_none() => {
                desc = Some(match value.to_ascii_lowercase().as_str() {
                    "asc" => false,
                    "desc" => true,
                    _ => return None,
                });
            }
            _ => return None,
        }
    }
    Some(Range {
        field: field.to_string(),
        exclusive: exclusive && start.is_some(),
        start,
        end,
        max,
        desc: desc.unwrap_or(false),
    })
}

/// The slot of `op` and what `call`'s header asks of it, when it has both.
fn requested(op: &Operation, call: &OperationCall) -> Option<(pages::HeaderSlot, Range)> {
    let slot = pages::header_slot(op)?;
    let text = call
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(&slot.header))
        .and_then(|(_, value)| value.as_str())?;
    let range = parse(text)?;
    Some((slot, range))
}

/// A text or a number as text.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Two values of the field, as numbers when both are, else as text.
fn compare(a: &Value, b: &Value) -> Ordering {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => scalar_text(a).cmp(&scalar_text(b)),
    }
}

/// A value of the field against a bound, as numbers when both are, else as text.
fn against(value: &Value, bound: &str) -> Ordering {
    if let (Some(x), Ok(y)) = (value.as_f64(), bound.trim().parse::<f64>()) {
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    scalar_text(value).as_deref().unwrap_or_default().cmp(bound)
}

impl Range {
    /// `order` in the range's direction.
    fn directed(&self, order: Ordering) -> Ordering {
        if self.desc { order.reverse() } else { order }
    }

    /// Whether `value` is within the range's bounds.
    fn holds(&self, value: &Value) -> bool {
        let from_start = |start: &str| {
            let order = self.directed(against(value, start));
            match self.exclusive {
                true => order == Ordering::Greater,
                false => order != Ordering::Less,
            }
        };
        let to_end = |end: &str| self.directed(against(value, end)) != Ordering::Greater;
        self.start.as_deref().is_none_or(from_start) && self.end.as_deref().is_none_or(to_end)
    }

    /// The `Range` of the page after one whose last value is `last` and which served
    /// `served` items: `None` for a value it could not be read back from.
    #[cfg_attr(not(feature = "http-rest"), allow(dead_code))]
    fn next(&self, last: &str, served: usize) -> Option<String> {
        if last.contains(';') || last.contains("..") || last.chars().any(char::is_control) {
            return None;
        }
        let end = self.end.as_deref().unwrap_or_default();
        let max = self.max.unwrap_or(served);
        let order = if self.desc { "order=desc," } else { "" };
        Some(format!("{} ]{last}..{end}; {order}max={max};", self.field))
    }
}

/// Orders and cuts the list answer `generated` to what `call`'s `Range` header asks
/// (see the module's documentation), its status the slot's `206` when more remain, else
/// its full one. The items' type `t` when their schema names no type of its own.
pub(super) async fn apply(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: Option<&str>,
    generated: &mut GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
    if !deleted::applies(rt, plan) || !plain(plan) {
        return;
    }
    let Some((slot, range)) = requested(op, call) else {
        return;
    };
    if generated.status != slot.full && generated.status != slot.partial {
        return;
    }
    let Some(body) = generated.body.as_ref() else {
        return;
    };
    let Some(items) = list_items(body).cloned() else {
        return;
    };
    let bundle = rt.bundle();
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let mut held = None;
    let Some((_, item_parts)) = lists::list_parts(bundle, schema, body, &mut held) else {
        return;
    };
    if !lists::declared_names(&item_parts).contains(&range.field.as_str()) {
        return;
    }
    let field = item_identity(bundle, op, body);
    let identity = |item: &Value| mb_mcm::member_at(item, &field).and_then(id_string);
    let mut pool = lists::collection_records(state, (plan, call), t, body)
        .await
        .unwrap_or_default();
    let mut known: Vec<String> = pool.iter().filter_map(identity).collect();
    for item in items {
        if let Some(id) = identity(&item) {
            if known.contains(&id) {
                continue;
            }
            known.push(id);
        }
        pool.push(item);
    }
    cut(bundle, plan, (&slot, &range), pool, generated);
}

/// Orders and cuts the executor's list answer `served` to what `call`'s `Range` header
/// asks, from its own items, which are the session's records the list keeps (see the
/// module's documentation): its status the slot's `206` when more remain. A call with
/// no such header, or an answer whose items do not declare the field, is left as it is.
pub(super) fn serve(
    bundle: &mb_mcm::Bundle,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    served: &mut GeneratedResponse,
) {
    let Some((slot, range)) = requested(plan.op, call) else {
        return;
    };
    if served.status != slot.full && served.status != slot.partial {
        return;
    }
    let Some(pool) = served.body.as_ref().and_then(list_items).cloned() else {
        return;
    };
    let Some(schema) = plan.op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let mut held = None;
    let declared = served
        .body
        .as_ref()
        .and_then(|body| lists::list_parts(bundle, schema, body, &mut held))
        .is_some_and(|(_, item_parts)| {
            lists::declared_names(&item_parts).contains(&range.field.as_str())
        });
    if declared {
        cut(bundle, plan, (&slot, &range), pool, served);
    }
}

/// `generated` as the page of `pool` that `range` asks for: ordered by its field and
/// bounded, at most its `max`, with the slot's `206` when more remain, else its full
/// status. Left as it is when no item holds the field, or the page would not validate.
fn cut(
    bundle: &mb_mcm::Bundle,
    plan: &CallPlan<'_>,
    (slot, range): (&pages::HeaderSlot, &Range),
    mut pool: Vec<Value>,
    generated: &mut GeneratedResponse,
) {
    let op = plan.op;
    let Some(body) = generated.body.as_ref() else {
        return;
    };
    let key = range.field.as_str();
    let ranged = |item: &Value| item.get(key).and_then(scalar_text).is_some();
    if !pool.iter().any(ranged) {
        tracing::debug!(operation = %op.id, "no item holds the Range field; left as answered");
        return;
    }
    pool.retain(|item| ranged(item) && range.holds(&item[key]));
    pool.sort_by(|a, b| range.directed(compare(&a[key], &b[key])));
    let max = range.max.unwrap_or(MAX_RANGE).min(MAX_RANGE);
    let more = pool.len() > max;
    pool.truncate(max);
    let Some(page) = pages::with_items(body, pool) else {
        return;
    };
    if plan.engine.serve_stored(bundle, op, &page, None).is_none() {
        tracing::debug!(operation = %op.id, "a Range page would not validate; left as answered");
        return;
    }
    generated.body = Some(page);
    generated.status = if more { slot.partial } else { slot.full };
}

/// The `Next-Range` header of a `206` page of a list paged by `call`'s `Range` header,
/// by its declared name: the range after the page's last item (see the module's
/// documentation). `None` for any other answer, a scenario or a forced status.
#[cfg(feature = "http-rest")]
pub(super) fn next_range(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    answer: &GeneratedResponse,
) -> Option<(String, String)> {
    if !rt.consistency || !plain(plan) {
        return None;
    }
    let (slot, range) = requested(plan.op, call)?;
    if answer.status != slot.partial {
        return None;
    }
    let items = list_items(answer.body.as_ref()?)?;
    let last = scalar_text(items.last()?.get(&range.field)?)?;
    let next = range.next(&last, items.len())?;
    Some((slot.next, next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn range(text: &str) -> Range {
        parse(text).unwrap_or_else(|| panic!("{text} should parse"))
    }

    #[test]
    fn a_range_reads_both_documented_forms() {
        assert_eq!(
            range("id ..; max=10;"),
            Range {
                field: "id".into(),
                start: None,
                exclusive: false,
                end: None,
                max: Some(10),
                desc: false,
            }
        );
        let desc = range("name ..; order=desc,max=10;");
        assert_eq!((desc.max, desc.desc), (Some(10), true));
        let other = range("name a..m; max=2, order=asc");
        assert_eq!(
            (other.start.as_deref(), other.end.as_deref(), other.max),
            (Some("a"), Some("m"), Some(2))
        );
        let next = range("name ]beta..gamma; max=1;");
        assert_eq!(
            (next.start.as_deref(), next.exclusive, next.end.as_deref()),
            (Some("beta"), true, Some("gamma"))
        );
        assert_eq!(range("version ..").max, None);
    }

    #[test]
    fn a_range_that_reads_otherwise_is_none() {
        for text in [
            "",
            "..",
            "name",
            "name a-m",
            "name ..; max=0",
            "name ..; max=ten",
            "name ..; order=sideways",
            "name ..; page=2",
            "name ..; max",
            "bytes=0-499",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn bounds_go_in_the_order_s_direction() {
        let asc = range("name b..c");
        assert!(!asc.holds(&json!("a")));
        assert!(asc.holds(&json!("b")) && asc.holds(&json!("c")));
        assert!(!asc.holds(&json!("d")));
        let after = range("name ]b..");
        assert!(!after.holds(&json!("b")) && after.holds(&json!("c")));
        let desc = range("name ]c..; order=desc");
        assert!(desc.holds(&json!("b")) && !desc.holds(&json!("c")) && !desc.holds(&json!("d")));
        let numbers = range("version 2..10");
        assert!(
            numbers.holds(&json!(9)) && numbers.holds(&json!(10)) && !numbers.holds(&json!(11))
        );
        assert_eq!(compare(&json!(9), &json!(10)), Ordering::Less);
    }

    #[test]
    fn a_next_range_reads_back_as_the_following_page() {
        let first = range("name a..m; max=1;");
        let next = first.next("beta", 1).unwrap();
        assert_eq!(next, "name ]beta..m; max=1;");
        assert_eq!(parse(&next).unwrap().start.as_deref(), Some("beta"));
        let desc = range("name ..; order=desc,max=10;")
            .next("gamma", 10)
            .unwrap();
        assert_eq!(desc, "name ]gamma..; order=desc,max=10;");
        assert!(parse(&desc).unwrap().desc);
        assert_eq!(first.next("a;b", 1), None);
        assert_eq!(first.next("a..b", 1), None);
    }
}
