//! Paging, filters, scope and order read from the effective world description.
//!
//! A list operation whose description sets `paging`, `filters` (non-empty), `scope` or
//! `order` has a [`ListQuery`]: what the request asks of the list's items, as the
//! description says it. Every path that puts records into a list answer ([`lists`]'s
//! `narrow`, its held-record paths, and the pages [`pages`] and [`runs`] serve) keeps
//! an item through [`keeps`] ([`keeps_answered`] for the model's own items), so each
//! described key replaces only its own heuristic:
//!
//! - `filters` (non-empty) replace the name-matched query filters; unset or empty, the
//!   heuristic filters run as before.
//! - `scope` is checked as well as everything else (a held record also stays in its
//!   route's collection).
//! - `order` sorts the kept items; unset, the order is the model's, then the store's.
//! - `paging` replaces the page parameter, page size and token place detection; unset,
//!   they are found as before.
//! - A soft delete of the list's item type ([`lifecycle::Exclusion`]) leaves a
//!   soft-deleted item out, unless the request sends its `listed_with` parameter. It is
//!   a key of its own: a list whose type has one has a [`ListQuery`] even when its
//!   operation describes nothing else, and every other key then falls back as unset.
//!
//! Filter semantics: `eq` compares the query text with the member by value (`"3"` is
//! `3`, `"true"` is `true`), and an array parameter is any of its values; `contains`
//! keeps an array member holding an item equal to the value (or an object item whose
//! one string member is), or a string member holding it as a case-sensitive
//! substring; `gte`/`lte` compare numbers by value, and RFC 3339 date-times when both
//! sides are one, and otherwise narrow nothing; `flag` keeps the member equal to the
//! boolean; `text` is a case-insensitive substring of the item's string members, or of
//! the described member. A value that is not one of the member's declared type
//! narrows nothing. An item the model answered (a stored record may be overlaid on
//! it) that does not hold the member, or holds `null` there, says nothing about it and
//! is kept ([`keeps_answered`], as the heuristic filters keep one); only a value that
//! disagrees leaves it out. Where the store fills the page from stored records
//! ([`keeps`]), a record that does not hold the member does not match it.
//!
//! Paging: the position is an offset into the matched, ordered items. A page number is
//! 1-based unless its parameter's `minimum` is 0. The size is the request's size
//! parameter, else its declared `default`, else there is no cut. Cursor, next-URL and
//! header-cursor tokens are the [`pages`] tokens.

use std::cmp::Ordering;

use chrono::DateTime;
use mb_engine::budget::PageLimit;
use mb_mcm::world::{FilterKind, PagingStyle};

use super::*;

/// How deep a `text` filter over every string member looks.
const MAX_TEXT_DEPTH: usize = 8;

/// What the description says a list request asks of its items.
#[derive(Debug, Clone, Default)]
pub(super) struct ListQuery {
    /// The described filters; empty when the description gives none.
    pub(super) filters: Vec<Filter>,
    /// The scope's member and the value the request gives its parameter.
    pub(super) scope: Option<(String, String)>,
    /// The member the list is ordered by, and whether largest first.
    pub(super) order: Option<(String, bool)>,
    /// How the list pages.
    pub(super) page: Option<PageAsk>,
    /// Whether the description sets a scope (the request may not carry it).
    scope_described: bool,
    /// The request parameters a described filter, scope or paging names, and the
    /// soft delete's `listed_with` parameter.
    params: Vec<String>,
    /// What leaves a soft-deleted record of the list's item type out, unless the
    /// request asks for deleted ones (then `None`).
    pub(super) lifecycle: Option<lifecycle::Exclusion>,
}

/// A described filter, with the values the request gives it.
#[derive(Debug, Clone)]
pub(super) struct Filter {
    /// The (dotted) member it tests; empty for a `text` filter over every string member.
    pub(super) member: String,
    pub(super) kind: FilterKind,
    /// The request's values, typed as the member declares; empty when the request
    /// gives none (or none of the member's type), which narrows nothing.
    pub(super) values: Vec<Value>,
}

/// A described paging, with what the request gives it.
#[derive(Debug, Clone)]
pub(super) struct PageAsk {
    pub(super) style: PagingStyle,
    /// The request parameter carrying the cursor, page or offset.
    pub(super) request: Option<String>,
    /// The value the request gives it, when not blank.
    pub(super) position: Option<String>,
    /// The request parameter setting the page size.
    pub(super) size_param: Option<String>,
    /// The page size: the request's, else the size parameter's declared `default`.
    pub(super) size: Option<usize>,
    /// The (dotted) answer member carrying the next cursor or URL.
    pub(super) response: Option<String>,
    /// The answer header carrying the next cursor.
    pub(super) response_header: Option<String>,
    /// For a page number: whether it counts from 0 (its `minimum` is 0).
    pub(super) from_zero: bool,
}

impl PageAsk {
    /// Whether the position is a page number or an offset (not a token).
    pub(super) fn counts(&self) -> bool {
        matches!(self.style, PagingStyle::Page | PagingStyle::Offset)
    }

    /// Whether the description says where a token goes and comes back, or the position
    /// is a page number or an offset: a token paging naming its `request` parameter and
    /// its `response` member or header. One naming only one side (a `next_url` the
    /// client only follows, a `request` with nothing answering it) leaves the other side
    /// to the heuristics ([`super::pages::slot`]).
    pub(super) fn places(&self) -> bool {
        self.counts()
            || (self.request.is_some()
                && (self.response.is_some() || self.response_header.is_some()))
    }

    /// The position's value at the first item.
    pub(super) fn first(&self) -> usize {
        match self.style {
            PagingStyle::Page if !self.from_zero => 1,
            _ => 0,
        }
    }

    /// Whether the request asks for the first page: it carries no position, or a
    /// page number or offset at its first value.
    pub(super) fn asks_first(&self) -> bool {
        match &self.position {
            None => true,
            Some(text) if self.counts() => text.trim().parse::<usize>().ok() == Some(self.first()),
            Some(_) => false,
        }
    }

    /// The offset of the first item the request asks for, for a page of `size`.
    fn start(&self, size: usize) -> Option<usize> {
        let Some(text) = &self.position else {
            return Some(0);
        };
        if !self.counts() {
            return Some(0);
        }
        let at: usize = text.trim().parse().ok()?;
        match self.style {
            PagingStyle::Page => at.checked_sub(self.first())?.checked_mul(size),
            _ => Some(at),
        }
    }
}

impl ListQuery {
    /// Whether the description sets any of `filters` (non-empty), `scope`, `order` or
    /// `paging` for the list.
    pub(super) fn describes_query(&self) -> bool {
        !self.filters.is_empty()
            || self.scope_described
            || self.order.is_some()
            || self.page.is_some()
    }

    /// Whether request parameter `param` is one the description names (a filter's,
    /// the scope's, or the paging's).
    pub(super) fn knows(&self, param: &str) -> bool {
        self.params.iter().any(|p| p == param)
    }

    /// Whether `param` is one of the described paging's parameters.
    pub(super) fn pages_with(&self, param: &str) -> bool {
        self.page.as_ref().is_some_and(|p| {
            p.request.as_deref() == Some(param) || p.size_param.as_deref() == Some(param)
        })
    }
}

/// The text values of query value `raw`: each of an array's, else the one; blank ones
/// aside.
fn texts(raw: &Value) -> Vec<String> {
    let one = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    let all: Vec<String> = match raw {
        Value::Array(values) => values.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    };
    all.into_iter().filter(|t| !t.trim().is_empty()).collect()
}

/// The schema of one item of `op`'s list answer.
fn item_schema(bundle: &mb_mcm::Bundle, op: &Operation) -> Option<Value> {
    let schema = op.success_output()?.schema.as_ref()?;
    let (parts, _) = schema_parts(bundle, Some(schema));
    if let Some(items) = parts.iter().find_map(|p| p.get("items")) {
        return Some(items.clone());
    }
    let members = list_members(bundle, schema);
    let [member] = members.as_slice() else {
        return None;
    };
    let array = property_schema(bundle, schema, member)?;
    let (parts, _) = schema_parts(bundle, Some(&array));
    parts.iter().find_map(|p| p.get("items")).cloned()
}

/// The one declared type of (dotted) `member` of items `item`, `null` aside.
fn member_type(bundle: &mb_mcm::Bundle, item: Option<&Value>, member: &str) -> Option<String> {
    let mut at = item?.clone();
    for step in member.split('.') {
        at = property_schema(bundle, &at, step)?;
    }
    let (parts, _) = schema_parts(bundle, Some(&at));
    parts
        .iter()
        .flat_map(|p| match p.get("type") {
            Some(Value::String(t)) => vec![t.clone()],
            Some(Value::Array(ts)) => ts
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        })
        .find(|t| t != "null")
}

/// `text` as a value of declared type `ty` for a filter of `kind`; `None` when it is
/// not one.
fn typed(ty: Option<&str>, kind: FilterKind, text: &str) -> Option<Value> {
    let text = text.trim();
    if kind == FilterKind::Flag {
        return text.parse::<bool>().ok().map(Value::Bool);
    }
    match ty {
        Some("integer") => text.parse::<i64>().ok().map(Value::from),
        Some("number") => {
            let n = text.parse::<f64>().ok()?;
            serde_json::Number::from_f64(n).map(Value::Number)
        }
        Some("boolean") => text.parse::<bool>().ok().map(Value::Bool),
        _ => Some(Value::String(text.to_string())),
    }
}

/// The value the request gives parameter `param`, in its path or query.
fn request_value(call: &OperationCall, param: &str) -> Option<Value> {
    call.path_params
        .get(param)
        .or_else(|| call.query.get(param))
        .cloned()
}

/// What the effective world describes of list `op`'s query, with `call`'s values;
/// `None` when it describes none of `paging`, `filters`, `scope` or `order` for it and
/// its item type has no soft delete. A query of a soft delete alone sets no other key,
/// so every heuristic runs as without it.
pub(super) fn of(rt: &MockRuntime, op: &Operation, call: &OperationCall) -> Option<ListQuery> {
    let gone = op
        .resource_type
        .as_deref()
        .and_then(|t| lifecycle::exclusion(rt, t));
    let none = mb_mcm::world::OpDesc::default();
    let d = rt.described().op(&op.id).unwrap_or(&none);
    if d.paging.is_none()
        && d.filters.is_empty()
        && d.scope.is_none()
        && d.order.is_none()
        && gone.is_none()
    {
        return None;
    }
    let bundle = rt.bundle();
    let item = item_schema(bundle, op);
    let mut params = Vec::new();
    if let Some((param, _)) = gone.as_ref().and_then(|g| g.unless.as_ref()) {
        params.push(param.clone());
    }
    let lifecycle = gone.filter(|g| !g.lifted_by(&call.query));
    let filters = d
        .filters
        .iter()
        .map(|f| {
            params.push(f.param.clone());
            let member = match (&f.member, f.kind) {
                (Some(m), _) => m.clone(),
                (None, FilterKind::Text) => String::new(),
                (None, _) => f.param.clone(),
            };
            let ty = match f.kind {
                FilterKind::Text | FilterKind::Contains => None,
                _ => member_type(bundle, item.as_ref(), &member),
            };
            let values = request_value(call, &f.param)
                .map(|raw| texts(&raw))
                .unwrap_or_default()
                .iter()
                .filter_map(|t| typed(ty.as_deref(), f.kind, t))
                .collect();
            Filter {
                member,
                kind: f.kind,
                values,
            }
        })
        .collect();
    let scope = d.scope.as_ref().and_then(|s| {
        params.push(s.param.clone());
        let value = request_value(call, &s.param)?;
        let text = texts(&value).into_iter().next()?;
        Some((s.member.clone(), text))
    });
    let page = d.paging.as_ref().map(|p| {
        params.extend(p.request.iter().cloned());
        params.extend(p.size.iter().cloned());
        let bound = |param: &str, at: &str| {
            let schema = op
                .input
                .query
                .as_ref()
                .and_then(|q| q.get("properties"))
                .and_then(|props| props.get(param))?;
            let (parts, _) = schema_parts(bundle, Some(schema));
            parts.iter().find_map(|p| p.get(at).and_then(Value::as_u64))
        };
        let size = p.size.as_deref().and_then(|param| {
            let asked = request_value(call, param)
                .and_then(|raw| texts(&raw).into_iter().next())
                .and_then(|t| t.trim().parse::<u64>().ok())
                .filter(|n| *n > 0);
            asked
                .or_else(|| bound(param, "default"))
                .and_then(|n| usize::try_from(n).ok())
        });
        PageAsk {
            style: p.style,
            position: p
                .request
                .as_deref()
                .and_then(|param| request_value(call, param))
                .and_then(|raw| texts(&raw).into_iter().next()),
            from_zero: p
                .request
                .as_deref()
                .is_some_and(|param| bound(param, "minimum") == Some(0)),
            request: p.request.clone(),
            size_param: p.size.clone(),
            size,
            response: p.response.clone(),
            response_header: p.response_header.clone(),
        }
    });
    Some(ListQuery {
        filters,
        scope,
        order: d.order.as_ref().map(|o| (o.member.clone(), o.descending)),
        page,
        scope_described: d.scope.is_some(),
        params,
        lifecycle,
    })
}

/// The page size list `op`'s request `call` asks for, with its parameter: the
/// described paging's when the description sets one (none without a size), else as
/// [`mb_engine::budget::requested_page`] finds it.
pub(super) fn requested_page(
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
) -> Option<(String, PageLimit)> {
    if let Some(page) = of(rt, op, call).and_then(|q| q.page) {
        let size = u64::try_from(page.size?).ok()?;
        return Some((page.size_param?, PageLimit::from(size)));
    }
    let bundle = rt.bundle();
    let schema = op.success_output()?.schema.as_ref()?;
    let doc = std::cell::OnceCell::new();
    mb_engine::budget::requested_page(
        op.input.query.as_ref(),
        || doc.get_or_init(|| bundle.with_reachable_defs(schema)),
        call,
    )
    .map(|(name, limit)| (name.to_string(), limit))
}

/// Whether `a` and `b` are the same value: text, numbers and booleans compared by
/// value, so `"3"` is `3` and `"true"` is `true`.
fn same(a: &Value, b: &Value) -> bool {
    let number = |v: &Value| match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match (a, b) {
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Bool(x), Value::String(y)) | (Value::String(y), Value::Bool(x)) => {
            y.trim() == x.to_string()
        }
        (Value::Number(_), _) | (_, Value::Number(_)) => {
            matches!((number(a), number(b)), (Some(x), Some(y)) if x == y)
        }
        _ => false,
    }
}

/// How `held` compares with `wanted`: as numbers when both are, as RFC 3339 date-times
/// when both parse as one; `None` otherwise.
fn compare(held: &Value, wanted: &Value) -> Option<Ordering> {
    let number = |v: &Value| match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    };
    if let (Some(a), Some(b)) = (number(held), number(wanted)) {
        return a.partial_cmp(&b);
    }
    let time = |v: &Value| DateTime::parse_from_rfc3339(v.as_str()?.trim()).ok();
    match (time(held), time(wanted)) {
        (Some(a), Some(b)) => Some(a.cmp(&b)),
        _ => None,
    }
}

/// Whether some string in `value`, at most [`MAX_TEXT_DEPTH`] deep, holds `wanted`
/// (lower case), ignoring case.
fn holds_text(value: &Value, wanted: &str, depth: usize) -> bool {
    if depth > MAX_TEXT_DEPTH {
        return false;
    }
    match value {
        Value::String(s) => s.to_lowercase().contains(wanted),
        Value::Array(items) => items.iter().any(|v| holds_text(v, wanted, depth + 1)),
        Value::Object(members) => members.values().any(|v| holds_text(v, wanted, depth + 1)),
        _ => false,
    }
}

/// Whether array element `e` is `wanted`: equal by value, or an object whose one
/// string member is.
fn element_is(e: &Value, wanted: &Value) -> bool {
    same(e, wanted)
        || e.as_object().is_some_and(|members| {
            let mut texts = members.values().filter_map(Value::as_str);
            match (texts.next(), texts.next()) {
                (Some(one), None) => Some(one) == wanted.as_str(),
                _ => false,
            }
        })
}

/// Where the items a list keeps come from, which decides what an item that does not
/// hold a filtered member says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The store fills the page from stored records: a record lacking the member does
    /// not match.
    Stored,
    /// The model answered the items (a stored record may be overlaid on one): an item
    /// lacking the member, or holding `null` there, is unknown, and is kept.
    Answered,
}

/// Whether `item` holds what `filter` asks, its items coming from `source`.
fn holds(filter: &Filter, item: &Value, source: Source) -> bool {
    if filter.values.is_empty() {
        return true;
    }
    let absent = source == Source::Answered;
    if filter.kind == FilterKind::Text {
        return filter.values.iter().any(|v| {
            let Some(wanted) = v.as_str().map(str::to_lowercase) else {
                return true;
            };
            match filter.member.as_str() {
                // Naming no member, the filter says nothing a model's item can be
                // checked against (a query with qualifiers, `language:rust`): kept, as
                // an item lacking a filtered member is.
                "" if absent => true,
                "" => holds_text(item, &wanted, 0),
                member => match mb_mcm::member_at(item, member).filter(|h| !h.is_null()) {
                    None => absent,
                    Some(held) => held
                        .as_str()
                        .is_some_and(|s| s.to_lowercase().contains(&wanted)),
                },
            }
        });
    }
    let Some(held) = mb_mcm::member_at(item, &filter.member).filter(|h| !h.is_null()) else {
        return absent;
    };
    filter.values.iter().any(|wanted| match filter.kind {
        FilterKind::Eq | FilterKind::Flag => same(held, wanted),
        FilterKind::Contains => match held {
            Value::Array(elements) => elements.iter().any(|e| element_is(e, wanted)),
            Value::String(s) => wanted.as_str().is_some_and(|w| s.contains(w)),
            _ => false,
        },
        FilterKind::Gte => compare(held, wanted).is_none_or(|o| o != Ordering::Less),
        FilterKind::Lte => compare(held, wanted).is_none_or(|o| o != Ordering::Greater),
        FilterKind::Text => true,
    })
}

/// Whether `item` holds the described scope, when the request gives one.
fn in_scope(q: &ListQuery, item: &Value) -> bool {
    q.scope.as_ref().is_none_or(|(member, value)| {
        mb_mcm::member_at(item, member)
            .filter(|h| !h.is_null())
            .is_some_and(|held| same(held, &Value::String(value.clone())))
    })
}

/// Whether `item` is soft-deleted and the request does not ask for deleted records.
fn soft_deleted(q: &ListQuery, item: &Value) -> bool {
    q.lifecycle.as_ref().is_some_and(|gone| gone.excludes(item))
}

/// Whether `item`, from `source`, matches the described keys of `q`: its filters (when
/// non-empty), its scope, and its soft delete.
fn matches_from(q: &ListQuery, item: &Value, source: Source) -> bool {
    q.filters.iter().all(|f| holds(f, item, source)) && in_scope(q, item) && !soft_deleted(q, item)
}

/// [`keeps`] for `item` from `source`.
fn keeps_from(
    q: Option<&ListQuery>,
    item: &Value,
    heuristic: impl Fn(&Value) -> bool,
    source: Source,
) -> bool {
    match q {
        None => heuristic(item),
        Some(q) if q.filters.is_empty() => {
            heuristic(item) && in_scope(q, item) && !soft_deleted(q, item)
        }
        Some(q) => matches_from(q, item, source),
    }
}

/// Whether a list the store fills keeps stored `item`: with described filters, by them
/// (a record lacking a filtered member does not match); else by `heuristic`, the path's
/// own filter test; and by the described scope, when set. A soft-deleted item is left
/// out either way, unless the request asks for deleted ones.
pub(super) fn keeps(
    q: Option<&ListQuery>,
    item: &Value,
    heuristic: impl Fn(&Value) -> bool,
) -> bool {
    keeps_from(q, item, heuristic, Source::Stored)
}

/// [`keeps`] for an item the model answered (a stored record may be overlaid on it):
/// an item that does not hold a described filter's member, or holds `null` there, is
/// unknown and is kept; only a value that disagrees leaves it out.
pub(super) fn keeps_answered(
    q: Option<&ListQuery>,
    item: &Value,
    heuristic: impl Fn(&Value) -> bool,
) -> bool {
    keeps_from(q, item, heuristic, Source::Answered)
}

/// How `a` and `b` order under `member`, both holding it: numbers by value, date-times
/// by time, text by text.
fn order_of(member: &str, a: &Value, b: &Value) -> Ordering {
    match (mb_mcm::member_at(a, member), mb_mcm::member_at(b, member)) {
        (Some(x), Some(y)) => compare(x, y).unwrap_or_else(|| match (x.as_str(), y.as_str()) {
            (Some(s), Some(t)) => s.cmp(t),
            _ => Ordering::Equal,
        }),
        _ => Ordering::Equal,
    }
}

/// Sorts `items` by the described order, when one is set; items not holding the
/// member go last either way.
pub(super) fn sort(q: &ListQuery, items: &mut [Value]) {
    let Some((member, descending)) = &q.order else {
        return;
    };
    let held = |v: &Value| mb_mcm::member_at(v, member).is_some_and(|h| !h.is_null());
    items.sort_by(|a, b| match (held(a), held(b)) {
        (true, true) if *descending => order_of(member, b, a),
        (true, true) => order_of(member, a, b),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => Ordering::Equal,
    });
}

/// The page of `items` (matched and ordered) the request asks for, and the position
/// of the next one (an offset) when more follow. Without a described page size, every
/// item, and no next.
pub(super) fn page<'a>(q: &ListQuery, items: &'a [Value]) -> (&'a [Value], Option<String>) {
    let Some(page) = &q.page else {
        return (items, None);
    };
    let Some(size) = page.size.filter(|s| *s > 0) else {
        return (items, None);
    };
    let Some(start) = page.start(size) else {
        return (&[], None);
    };
    let start = start.min(items.len());
    let end = start.saturating_add(size).min(items.len());
    let next = (end < items.len()).then(|| end.to_string());
    (&items[start..end], next)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Whether stored `item` matches the described keys of `q`.
    fn matches(q: &ListQuery, item: &Value) -> bool {
        matches_from(q, item, Source::Stored)
    }

    fn query(filters: Vec<Filter>) -> ListQuery {
        ListQuery {
            filters,
            ..ListQuery::default()
        }
    }

    fn filter(member: &str, kind: FilterKind, values: Vec<Value>) -> Filter {
        Filter {
            member: member.to_string(),
            kind,
            values,
        }
    }

    #[test]
    fn eq_compares_by_value_and_reads_nested_members() {
        let q = query(vec![filter("a.b", FilterKind::Eq, vec![json!("3")])]);
        assert!(matches(&q, &json!({"a": {"b": 3}})));
        assert!(!matches(&q, &json!({"a": {"b": 4}})));
        assert!(!matches(&q, &json!({"a": {}})));
        let q = query(vec![filter("on", FilterKind::Eq, vec![json!("true")])]);
        assert!(matches(&q, &json!({"on": true})));
    }

    #[test]
    fn contains_reads_arrays_of_objects_and_substrings() {
        let q = query(vec![filter(
            "tags",
            FilterKind::Contains,
            vec![json!("urgent")],
        )]);
        assert!(matches(&q, &json!({"tags": [{"tag": "urgent"}]})));
        assert!(matches(&q, &json!({"tags": ["urgent"]})));
        assert!(!matches(&q, &json!({"tags": [{"tag": "later"}]})));
        let q = query(vec![filter(
            "name",
            FilterKind::Contains,
            vec![json!("Ann")],
        )]);
        assert!(matches(&q, &json!({"name": "Joanna Ann"})));
        assert!(!matches(&q, &json!({"name": "ann"})));
    }

    #[test]
    fn bounds_compare_numbers_and_date_times_else_narrow_nothing() {
        let q = query(vec![filter("n", FilterKind::Gte, vec![json!(5)])]);
        assert!(matches(&q, &json!({"n": 5})));
        assert!(!matches(&q, &json!({"n": 4})));
        let q = query(vec![filter(
            "at",
            FilterKind::Lte,
            vec![json!("2024-01-02T00:00:00Z")],
        )]);
        assert!(matches(&q, &json!({"at": "2024-01-01T00:00:00Z"})));
        assert!(!matches(&q, &json!({"at": "2024-01-03T00:00:00Z"})));
        assert!(matches(&q, &json!({"at": "soon"})));
    }

    #[test]
    fn a_mistyped_value_narrows_nothing() {
        assert_eq!(typed(Some("integer"), FilterKind::Gte, "abc"), None);
        let q = query(vec![filter("n", FilterKind::Gte, Vec::new())]);
        assert!(matches(&q, &json!({"n": 1})));
    }

    #[test]
    fn text_is_a_case_insensitive_substring() {
        let q = query(vec![filter("", FilterKind::Text, vec![json!("WALK")])]);
        assert!(matches(&q, &json!({"title": "Walk the dog"})));
        assert!(!matches(&q, &json!({"title": "Feed the cat"})));
    }

    #[test]
    fn an_answered_item_lacking_the_member_is_kept_and_a_stored_one_is_not() {
        let q = query(vec![filter(
            "project.id",
            FilterKind::Eq,
            vec![json!("p1")],
        )]);
        let heuristic = |_: &Value| false;
        // Over a model's answer, an absent or null member says nothing.
        assert!(keeps_answered(Some(&q), &json!({"id": "t1"}), heuristic));
        assert!(keeps_answered(
            Some(&q),
            &json!({"id": "t1", "project": null}),
            heuristic
        ));
        assert!(keeps_answered(
            Some(&q),
            &json!({"id": "t1", "project": {"id": null}}),
            heuristic
        ));
        assert!(keeps_answered(
            Some(&q),
            &json!({"project": {"id": "p1"}}),
            heuristic
        ));
        assert!(!keeps_answered(
            Some(&q),
            &json!({"project": {"id": "p2"}}),
            heuristic
        ));
        // A described text filter over a named member too.
        let t = query(vec![filter("title", FilterKind::Text, vec![json!("walk")])]);
        assert!(keeps_answered(Some(&t), &json!({"id": "t1"}), heuristic));
        assert!(!keeps_answered(
            Some(&t),
            &json!({"title": "Feed the cat"}),
            heuristic
        ));
        // From the store, a record lacking the member does not match.
        assert!(!keeps(Some(&q), &json!({"id": "t1"}), heuristic));
        assert!(!keeps(Some(&t), &json!({"id": "t1"}), heuristic));
    }

    #[test]
    fn keeps_runs_the_heuristic_only_without_described_filters() {
        let none = ListQuery::default();
        assert!(!keeps(Some(&none), &json!({}), |_| false));
        let q = query(vec![filter("x", FilterKind::Eq, vec![json!("1")])]);
        assert!(keeps(Some(&q), &json!({"x": "1"}), |_| false));
        assert!(!keeps(None, &json!({"x": "1"}), |_| false));
    }

    #[test]
    fn sort_then_page() {
        let q = ListQuery {
            order: Some(("at".into(), true)),
            page: Some(PageAsk {
                style: PagingStyle::Page,
                request: Some("p".into()),
                position: Some("2".into()),
                size_param: Some("n".into()),
                size: Some(2),
                response: None,
                response_header: None,
                from_zero: false,
            }),
            ..ListQuery::default()
        };
        let mut items = vec![
            json!({"at": 1}),
            json!({}),
            json!({"at": 3}),
            json!({"at": 2}),
        ];
        sort(&q, &mut items);
        assert_eq!(
            items,
            [
                json!({"at": 3}),
                json!({"at": 2}),
                json!({"at": 1}),
                json!({})
            ]
        );
        let (cut, next) = page(&q, &items);
        assert_eq!(cut, [json!({"at": 1}), json!({})]);
        assert_eq!(next, None);
    }
}
