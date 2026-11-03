//! What a list read's query says its answer holds, applied to the generated answer.
//!
//! A query parameter that names a member of the list's items (`To` for `to`,
//! `generations` for `generation`: by name exactly, then ignoring case, then by
//! [`mb_mcm::stem`]) asks for the items whose member equals its value. The model is
//! told so and does not always listen, so after generation an item whose member holds
//! another value goes. Only where the contract is clear: the parameter and the member
//! declare the same one type, text, an integer or a boolean (a real number, such as a
//! coordinate, is matched within a margin if at all), the member is no `const`, the
//! parameter is no bare `id` (which names what the route is about), the value is one the
//! member's `enum` and `format` allow, and a comma-separated value is a set only when
//! every part is in that `enum`. A parameter with an `enum` of its own needs one on the
//! member too, as its words (`all`) may be no item's. So `state=all` beside an `enum`
//! without `all`, or beside plain text, a search's `q` (no member is named so), a
//! `labels` text beside a `labels` array and a range (`DateSent<`) change nothing. Text
//! the contract says nothing more of (no `enum`, `format` or `pattern`) is often
//! searched by part, so it keeps an item whose value contains the requested one,
//! ignoring case: `name=Ann` keeps `Anna`. A value holding a digit (an id, a phone
//! number, a code) matches whole, ignoring case, as `u1` is not `u10`. Values are
//! trimmed, and one with a leading space and a digit also matches with a `+`, as an
//! unencoded `+` decodes to a space. An
//! item that does not hold the member, or holds something of another shape, stays. The
//! list's main array is then cut to the page size a parameter asks for, by the signal
//! [`mb_engine::budget::requested_limit`] goes by.
//!
//! Each step is kept only when the answer still validates and keeps its `minItems`, and
//! a count beside the list that is clearly its own loses what the filter removed (as
//! [`deleted`] does for a deleted id); a page size leaves it, as a total counts every
//! page. Nothing is added or changed, and nothing reads the store.
//!
//! An LLM and `consistency: on` only, as [`deleted::applies`] says: offline answers, and
//! those with consistency off, are as they were.

use std::collections::BTreeSet;

use mb_engine::SchemaValidator;
use mb_engine::budget::requested_page;
use mb_engine::consistency::{list_items_mut, list_member};

use super::*;

/// A member of a list's items that a query parameter names: what the contract
/// declares of it.
pub(super) struct Member<'a> {
    pub(super) name: &'a str,
    parts: Vec<&'a Value>,
}

impl<'a> Member<'a> {
    fn types(&self) -> BTreeSet<&'a str> {
        types_of(&self.parts)
    }

    /// Whether the member holds text only.
    pub(super) fn is_text(&self) -> bool {
        self.types().into_iter().eq(["string"])
    }

    fn enumerated(&self) -> Option<&'a Vec<Value>> {
        self.parts
            .iter()
            .find_map(|p| p.get("enum").and_then(Value::as_array))
    }

    fn constant(&self) -> Option<&'a Value> {
        self.parts.iter().find_map(|p| p.get("const"))
    }

    fn format(&self) -> Option<&'a str> {
        self.parts
            .iter()
            .find_map(|p| p.get("format").and_then(Value::as_str))
    }

    /// Whether the member is text the contract says nothing more of: no `enum`,
    /// `format` or `pattern`. Such a filter is often a search by part (`name=Ann`).
    fn is_free_text(&self) -> bool {
        self.is_text()
            && self.enumerated().is_none()
            && self.format().is_none()
            && !self.parts.iter().any(|p| p.get("pattern").is_some())
    }
}

/// The types `parts` declare, `null` aside.
fn types_of<'a>(parts: &[&'a Value]) -> BTreeSet<&'a str> {
    let mut types = BTreeSet::new();
    for part in parts {
        match part.get("type") {
            Some(Value::String(t)) => {
                types.insert(t.as_str());
            }
            Some(Value::Array(list)) => types.extend(list.iter().filter_map(Value::as_str)),
            _ => {}
        }
    }
    types.remove("null");
    types
}

/// The member of the items `item_parts` describe that query parameter `param` names:
/// one with its very name, else one with it in another case, else one sharing its stem.
/// Two found the same way name neither.
pub(super) fn named_member<'a>(
    bundle: &'a mb_mcm::Bundle,
    item_parts: &[&'a Value],
    param: &str,
) -> Option<Member<'a>> {
    let mut names: Vec<&'a str> = Vec::new();
    for part in item_parts {
        for name in part
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|p| p.keys())
        {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
    }
    let stem = mb_mcm::stem(param);
    let ways: [&dyn Fn(&str) -> bool; 3] = [
        &|name| name == param,
        &|name| name.eq_ignore_ascii_case(param),
        &|name| mb_mcm::stem(name) == stem,
    ];
    let name = ways.iter().find_map(|way| {
        let mut found = names.iter().filter(|name| way(name));
        let first = found.next()?;
        Some(found.next().is_none().then_some(*first))
    })??;
    let schema = item_parts
        .iter()
        .find_map(|p| p.get("properties")?.get(name))?;
    let (parts, _) = schema_parts(bundle, Some(schema));
    Some(Member { name, parts })
}

/// The member query parameter `param`, declared by `query`, names among the items
/// `item_parts` describe, when both declare the one same type.
pub(super) fn filtered_member<'a>(
    bundle: &'a mb_mcm::Bundle,
    query: Option<&'a Value>,
    item_parts: &[&'a Value],
    param: &str,
) -> Option<Member<'a>> {
    // A bare `id` names what the route is about (OpenWeatherMap's city), not an item.
    if mb_mcm::is_generic_id(param) {
        return None;
    }
    let declared = query?.get("properties")?.get(param)?;
    let member = named_member(bundle, item_parts, param)?;
    let (param_parts, _) = schema_parts(bundle, Some(declared));
    let types = member.types();
    // A real number (a coordinate) is matched within a margin, if at all, and a
    // constant member (Jira's `expand`) is no filter.
    let single = types.len() == 1 && !types.contains("number");
    // A parameter's own `enum` beside a member with none may hold words of its own
    // (`state=all`) that no item holds.
    let own_words = param_parts.iter().any(|p| p.get("enum").is_some());
    let fits = single
        && types == types_of(&param_parts)
        && member.constant().is_none()
        && !(own_words && member.enumerated().is_none());
    fits.then_some(member)
}

/// `raw` as a value of `member`'s one type, when it is one its `enum` and `format`
/// allow.
fn typed(member: &Member<'_>, raw: &str) -> Option<Value> {
    let value = match member.types().first().copied()? {
        "string" => Value::String(raw.to_string()),
        "integer" => Value::from(raw.trim().parse::<i64>().ok()?),
        "boolean" => Value::Bool(raw.trim().parse::<bool>().ok()?),
        _ => return None,
    };
    if let Some(allowed) = member.enumerated()
        && !allowed.contains(&value)
    {
        return None;
    }
    // A format no validator checks says nothing against the value.
    if let Some(format) = member.format() {
        let schema = serde_json::json!({"format": format});
        let holds = SchemaValidator::for_output(&schema)
            .map(|v| v.is_valid_output(&value))
            .unwrap_or(true);
        if !holds {
            return None;
        }
    }
    Some(value)
}

/// The values query value `raw` asks `member` to hold: one, or a comma-separated set
/// whose every part is in the member's `enum`.
fn wanted(member: &Member<'_>, raw: &Value) -> Option<Vec<Value>> {
    let raw = match raw {
        Value::String(text) => text.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if value.contains(',') && member.enumerated().is_some() {
        let parts: Option<Vec<Value>> = value.split(',').map(|p| typed(member, p.trim())).collect();
        if let Some(parts) = parts {
            return Some(parts);
        }
    }
    let mut values = vec![typed(member, value)?];
    // An unencoded `+` decodes to a space: `to=+1555` arrives as ` 1555`.
    if raw.starts_with(' ')
        && holds_a_digit(value)
        && let Some(plus) = typed(member, &format!("+{value}"))
    {
        values.push(plus);
    }
    Some(values)
}

/// Whether `text` holds a digit: an id, a phone number or a code, matched whole.
fn holds_a_digit(text: &str) -> bool {
    text.chars().any(|c| c.is_ascii_digit())
}

/// Whether `held` is one of `wanted`: text ignoring case, and holding it when `loose`,
/// numbers by value.
fn is_wanted(held: &Value, wanted: &[Value], loose: bool) -> bool {
    wanted.iter().any(|w| match (held, w) {
        (Value::String(a), Value::String(b)) if loose => {
            a.to_lowercase().contains(&b.to_lowercase())
        }
        (Value::String(a), Value::String(b)) => a.to_lowercase() == b.to_lowercase(),
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (a, b) => a == b,
    })
}

/// A filter: the member, the values it may hold, and whether holding one as part of
/// its text is enough (see [`Member::is_free_text`]).
struct Filter<'a> {
    member: &'a str,
    values: Vec<Value>,
    loose: bool,
}

/// Whether `item` holds another value than the filter's under its member: one of the
/// same shape, so a missing member, `null` or another shape says nothing.
fn contradicts(item: &Value, filter: &Filter<'_>) -> bool {
    let (member, wanted) = (filter.member, &filter.values);
    let Some(held) = item.get(member) else {
        return false;
    };
    let comparable = wanted.iter().any(|w| {
        matches!(
            (held, w),
            (Value::String(_), Value::String(_))
                | (Value::Bool(_), Value::Bool(_))
                | (Value::Number(_), Value::Number(_))
        )
    });
    comparable && !is_wanted(held, wanted, filter.loose)
}

/// The schema parts of `op`'s list answer `body`'s main array, and of its items.
fn list_parts<'a>(
    bundle: &'a mb_mcm::Bundle,
    schema: &'a Value,
    body: &Value,
    held: &'a mut Option<Value>,
) -> Option<(Vec<&'a Value>, Vec<&'a Value>)> {
    let array: &'a Value = match body {
        Value::Array(_) => schema,
        Value::Object(_) => {
            *held = property_schema(bundle, schema, list_member(body)?);
            held.as_ref()?
        }
        _ => return None,
    };
    let (array_parts, _) = schema_parts(bundle, Some(array));
    let items = array_parts.iter().find_map(|p| p.get("items"))?;
    let (item_parts, _) = schema_parts(bundle, Some(items));
    Some((array_parts, item_parts))
}

/// The most the parts of an array declare of `keyword` (`minItems`, `maxItems`).
fn declared_count(parts: &[&Value], keyword: &str) -> Option<u64> {
    parts
        .iter()
        .filter_map(|p| p.get(keyword).and_then(Value::as_u64))
        .max()
}

/// Applies a list read's equality filters and page size to its generated answer (see
/// the module's doc).
pub(super) fn narrow(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    body: &mut Value,
) {
    if call.query.is_empty() {
        return;
    }
    let bundle = rt.bundle();
    let op = plan.op;
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let mut held = None;
    let Some((array_parts, item_parts)) = list_parts(bundle, schema, body, &mut held) else {
        return;
    };
    let fewest = declared_count(&array_parts, "minItems").unwrap_or(0);
    let query = op.input.query.as_ref();
    let doc = std::cell::OnceCell::new();
    let page = requested_page(
        query,
        || doc.get_or_init(|| bundle.with_reachable_defs(schema)),
        call,
    );
    let mut filters: Vec<Filter<'_>> = Vec::new();
    for (param, raw) in &call.query {
        if page.is_some_and(|(name, _)| name == param) {
            continue;
        }
        if let Some(member) = filtered_member(bundle, query, &item_parts, param)
            && let Some(values) = wanted(&member, raw)
        {
            // `name=Ann` keeps `Anna`; `owner_id=u1` does not keep `u10`.
            let loose = member.is_free_text()
                && !values.iter().any(|v| v.as_str().is_some_and(holds_a_digit));
            filters.push(Filter {
                member: member.name,
                values,
                loose,
            });
        }
    }
    let valid = |candidate: &Value| {
        plan.engine
            .serve_stored(bundle, op, candidate, None)
            .is_some()
    };
    if !filters.is_empty() {
        let mut kept = body.clone();
        if let Some(items) = list_items_mut(&mut kept) {
            let was = items.len();
            items.retain(|item| !filters.iter().any(|filter| contradicts(item, filter)));
            let removed = was - items.len();
            if removed > 0 && items.len() as u64 >= fewest && valid(&kept) {
                if let Some(counted) =
                    deleted::with_count_corrected(rt, op, body, &kept, was, removed as u64)
                    && valid(&counted)
                {
                    kept = counted;
                }
                *body = kept;
            }
        }
    }
    let Some((_, limit)) = page else {
        return;
    };
    // A size the contract ties to another array is not this one's.
    if limit
        .maximum
        .is_some_and(|maximum| declared_count(&array_parts, "maxItems") != Some(maximum))
        || limit.items < fewest
    {
        return;
    }
    let mut capped = body.clone();
    let Some(items) = list_items_mut(&mut capped) else {
        return;
    };
    let Ok(size) = usize::try_from(limit.items) else {
        return;
    };
    if items.len() > size {
        items.truncate(size);
        if valid(&capped) {
            *body = capped;
        }
    }
}
