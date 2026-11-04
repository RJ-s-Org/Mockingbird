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
//! without `all`, or beside plain text, a search's `q` (no member is named so) and a
//! range (`DateSent<`) change nothing. Text the contract says nothing more of (no
//! `enum`, `format` or `pattern`) is often searched by part, so it keeps an item whose
//! value contains the requested one, ignoring case: `name=Ann` keeps `Anna`. A value
//! holding a digit (an id, a phone number, a code) matches whole, ignoring case, as `u1`
//! is not `u10`. Values are trimmed, and one with a leading space and a digit also
//! matches with a `+`, as an unencoded `+` decodes to a space. An item that does not
//! hold the member, or holds something of another shape, stays; but a reference
//! (`person_id`, `ownerId`: a name of more than one word whose last is `id`) holding
//! `null` refers to nothing, which the requested value contradicts.
//!
//! A text parameter whose whole name names an array of strings (`labels` for
//! `labels`, as [`named_member`] names: exactly, by case, by stem) keeps the items whose
//! array holds any of its comma-separated parts, as an `enum` set keeps any of its
//! values: whole, trimmed and ignoring case, so `labels=bug,urgent` keeps `["bug"]` and
//! `[" Urgent"]`, not `["bugfix"]`. A word of a name is no name: `tag_name`,
//! `exclude_labels`, `without_tags` and `tag_ids` change nothing. The same guards hold:
//! no `const`, parts the items' `enum` allows, and a parameter's own `enum` only beside
//! one on the items.
//!
//! A parameter that names no member changes nothing, whatever its value: a bound
//! (`before`, `oldest`) or a search naming an item's id leaves it in, as which side of
//! the bound the rest is on, and whether the bound is in it, is not said by the
//! contract. The list's main array is then cut to the page size a parameter asks for,
//! by the signal [`mb_engine::budget::requested_limit`] goes by.
//!
//! Each step is kept only when the answer still validates and keeps its `minItems`, and
//! a count beside the list that is clearly its own loses what the filter removed (as
//! [`deleted`] does for a deleted id); a page size leaves it, as a total counts every
//! page.
//!
//! Before any of that, [`overlay_stored`] looks up each item the session holds, by its
//! identity and its type, as the store has the newest writes (a ticket closed since, a
//! reply counted). An item is never removed for what the store holds: when the item
//! itself holds the path's value under a member tied to a path parameter (`project_id`
//! under `/projects/{project_id}/tasks`, by the rule owner checks go by), and the stored
//! record holds another value there, which of the two is right is a guess (a task moved,
//! a member of two lists), so the item stays exactly as generated. A member that is the
//! items' identity (`/users/{user_id}/followers`), or that the contract types as the
//! item's own type, names no parent, and a path value the item does not hold (an alias:
//! `/users/me/events`) contradicts nothing. Otherwise the item shows the stored
//! record's members, each as the item's schema declares it at every depth (see
//! [`projected`]); members several `allOf` parts declare are declared as all of them:
//! an `owner` the summary declares as `{id, name}` brings no `email`, nor does each
//! object of an array. The item's own members stay otherwise, and the generated item
//! whole when the record would not validate in its place. Nothing is stored or added:
//! an item the store holds but the answer does not name stays out of the page, as
//! which list and page it belongs on is not known well enough. A full page may still
//! leave such records for a next page ([`stored_rest`]), served by a token as a cut's
//! rest is, or by a later page number or offset ([`runs`]).
//!
//! An LLM and `consistency: on` only, as [`deleted::applies`] says: offline answers, and
//! those with consistency off, are as they were.

use std::collections::BTreeSet;

use mb_engine::SchemaValidator;
use mb_engine::budget::{PageLimit, requested_page};
use mb_engine::consistency::{list_items_mut, list_member};

/// Items of one list answer looked up in the store, at most: one read each, as
/// [`mb_state::StateStore`] has no read of many at once.
const MAX_OVERLAID: usize = 100;
/// How deep [`projected`] follows a value's schema.
const MAX_PROJECTED_DEPTH: usize = 16;

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
pub(super) fn types_of<'a>(parts: &[&'a Value]) -> BTreeSet<&'a str> {
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
    let names = declared_names(item_parts);
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

/// The names of the members the parts of an item declare, each once.
fn declared_names<'a>(item_parts: &[&'a Value]) -> Vec<&'a str> {
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
    names
}

/// What the elements of `member` are, when it is an array of text only (`labels`):
/// a [`Member`] of the element's parts, under the array's name.
fn string_elements<'a>(bundle: &'a mb_mcm::Bundle, member: &Member<'a>) -> Option<Member<'a>> {
    if !member.types().into_iter().eq(["array"]) || member.constant().is_some() {
        return None;
    }
    let items = member.parts.iter().find_map(|p| p.get("items"))?;
    let (parts, _) = schema_parts(bundle, Some(items));
    let element = Member {
        name: member.name,
        parts,
    };
    (element.is_text() && element.constant().is_none()).then_some(element)
}

/// The array of text query parameter `param`, declared by `query` as text or an array,
/// filters: the member its whole name names (see [`named_member`]), when that is one.
/// Its elements' [`Member`], under the array's name.
fn array_filter_member<'a>(
    bundle: &'a mb_mcm::Bundle,
    query: Option<&'a Value>,
    item_parts: &[&'a Value],
    param: &str,
) -> Option<Member<'a>> {
    if mb_mcm::is_generic_id(param) {
        return None;
    }
    let declared = query?.get("properties")?.get(param)?;
    let (param_parts, _) = schema_parts(bundle, Some(declared));
    let param_types = types_of(&param_parts);
    if !(param_types.iter().eq(["string"].iter()) || param_types.iter().eq(["array"].iter())) {
        return None;
    }
    let element = string_elements(bundle, &named_member(bundle, item_parts, param)?)?;
    // A parameter's own `enum` beside elements with none may hold words of its own.
    let own_words = param_parts
        .iter()
        .any(|p| p.get("enum").is_some() || p.get("items").and_then(|i| i.get("enum")).is_some());
    (!(own_words && element.enumerated().is_none())).then_some(element)
}

/// The parts query value `raw` asks an array of text to hold one of: its
/// comma-separated parts, or an array's strings, each one the elements' `enum` and
/// `format` allow.
fn wanted_parts(element: &Member<'_>, raw: &Value) -> Option<Vec<Value>> {
    let texts: Vec<String> = match raw {
        Value::String(text) => text.split(',').map(|p| p.trim().to_string()).collect(),
        Value::Array(values) => values
            .iter()
            .map(|v| v.as_str().map(|s| s.trim().to_string()))
            .collect::<Option<_>>()?,
        _ => return None,
    };
    let texts: Vec<String> = texts.into_iter().filter(|t| !t.is_empty()).collect();
    if texts.is_empty() {
        return None;
    }
    texts.iter().map(|t| typed(element, t)).collect()
}

/// Whether `name` is a reference to another record: more than one word, the last `id`
/// (`person_id`, `ownerId`).
fn is_reference(name: &str) -> bool {
    let words = mb_mcm::words(name);
    words.len() > 1 && words.last().is_some_and(|w| w.eq_ignore_ascii_case("id"))
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

/// What a filter asks of its member.
enum Test {
    /// To hold one of the values: as part of its text when `loose` (see
    /// [`Member::is_free_text`]); and, for a reference, not to be `null`.
    Equal { loose: bool, reference: bool },
    /// For an array of text, to hold any of the values, whole, trimmed and ignoring
    /// case.
    HoldsAny,
}

/// A filter: the member, the values it asks of it, and how.
struct Filter<'a> {
    member: &'a str,
    values: Vec<Value>,
    test: Test,
}

/// Whether `item` holds another value than the filter's under its member: one of the
/// same shape, so a missing member, another shape, or `null` other than in a reference
/// says nothing.
fn contradicts(item: &Value, filter: &Filter<'_>) -> bool {
    let (member, wanted) = (filter.member, &filter.values);
    let Some(held) = item.get(member) else {
        return false;
    };
    let (loose, reference) = match filter.test {
        Test::Equal { loose, reference } => (loose, reference),
        Test::HoldsAny => {
            let Some(elements) = held.as_array() else {
                return false;
            };
            let Some(texts) = elements
                .iter()
                .map(|e| e.as_str().map(|t| t.trim().to_lowercase()))
                .collect::<Option<Vec<_>>>()
            else {
                return false;
            };
            return !wanted.iter().any(|w| {
                w.as_str()
                    .is_some_and(|w| texts.contains(&w.trim().to_lowercase()))
            });
        }
    };
    if held.is_null() {
        return reference;
    }
    let comparable = wanted.iter().any(|w| {
        matches!(
            (held, w),
            (Value::String(_), Value::String(_))
                | (Value::Bool(_), Value::Bool(_))
                | (Value::Number(_), Value::Number(_))
        )
    });
    comparable && !is_wanted(held, wanted, loose)
}

/// The schema parts of `op`'s list answer `body`'s main array, and of its items.
pub(super) fn list_parts<'a>(
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

/// What cutting a list answer to its page size left out.
pub(super) struct Cut {
    /// The items past the page, in order.
    pub(super) rest: Vec<Value>,
    /// The page size.
    pub(super) size: usize,
    /// The query parameter that asked for it.
    pub(super) size_param: String,
    /// How many of the last items of `rest` the session's records gave (see
    /// [`stored_rest`]), not the answer: they are stored already.
    pub(super) stored: usize,
    /// Whether more records may follow `rest` than were read: a read of the store that
    /// came back full.
    pub(super) truncated: bool,
}

/// Applies a list read's equality filters and page size to its generated answer (see
/// the module's doc), and says what the page size left out.
pub(super) fn narrow(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    body: &mut Value,
) -> Option<Cut> {
    if call.query.is_empty() {
        return None;
    }
    let bundle = rt.bundle();
    let op = plan.op;
    let schema = op.success_output().and_then(|o| o.schema.as_ref())?;
    let mut held = None;
    let (array_parts, item_parts) = list_parts(bundle, schema, body, &mut held)?;
    let fewest = declared_count(&array_parts, "minItems").unwrap_or(0);
    let query = op.input.query.as_ref();
    let doc = std::cell::OnceCell::new();
    let page = requested_page(
        query,
        || doc.get_or_init(|| bundle.with_reachable_defs(schema)),
        call,
    );
    let filters: Vec<Filter<'_>> = filters_of(bundle, query, &item_parts, call, page)
        .into_iter()
        .map(|(_, filter)| filter)
        .collect();
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
    let (_, limit) = page?;
    let size = page_size(limit, &array_parts)?;
    let mut capped = body.clone();
    let items = list_items_mut(&mut capped)?;
    if items.len() <= size {
        return None;
    }
    let rest = items.split_off(size);
    if !valid(&capped) {
        return None;
    }
    *body = capped;
    Some(Cut {
        rest,
        size,
        size_param: page.map(|(name, _)| name.to_string())?,
        stored: 0,
        truncated: false,
    })
}

/// The page size `limit` asks of the list whose array `array_parts` describe: none when
/// the contract ties it to another array, or it is under the array's `minItems`.
fn page_size(limit: PageLimit, array_parts: &[&Value]) -> Option<usize> {
    let fewest = declared_count(array_parts, "minItems").unwrap_or(0);
    // A size the contract ties to another array is not this one's.
    if limit
        .maximum
        .is_some_and(|maximum| declared_count(array_parts, "maxItems") != Some(maximum))
        || limit.items < fewest
    {
        return None;
    }
    usize::try_from(limit.items).ok()
}

/// The equality filters `call`'s query asks of the items `item_parts` describe, each
/// with its parameter: all but the page size's `page`.
fn filters_of<'a, 'c>(
    bundle: &'a mb_mcm::Bundle,
    query: Option<&'a Value>,
    item_parts: &[&'a Value],
    call: &'c OperationCall,
    page: Option<(&str, PageLimit)>,
) -> Vec<(&'c str, Filter<'a>)> {
    let mut filters = Vec::new();
    for (param, raw) in &call.query {
        if page.is_some_and(|(name, _)| name == param) {
            continue;
        }
        if let Some(member) = filtered_member(bundle, query, item_parts, param) {
            let Some(values) = wanted(&member, raw) else {
                continue;
            };
            // `name=Ann` keeps `Anna`; `owner_id=u1` does not keep `u10`.
            let loose = member.is_free_text()
                && !values.iter().any(|v| v.as_str().is_some_and(holds_a_digit));
            filters.push((
                param.as_str(),
                Filter {
                    member: member.name,
                    values,
                    test: Test::Equal {
                        loose,
                        reference: is_reference(member.name),
                    },
                },
            ));
        } else if let Some(element) = array_filter_member(bundle, query, item_parts, param)
            && let Some(values) = wanted_parts(&element, raw)
        {
            filters.push((
                param.as_str(),
                Filter {
                    member: element.name,
                    values,
                    test: Test::HoldsAny,
                },
            ));
        }
    }
    filters
}

/// Whether `item` holds what `filter` asks, as a member of the same shape: a missing
/// member or `null` holds nothing.
fn holds_wanted(item: &Value, filter: &Filter<'_>) -> bool {
    let Some(held) = item.get(filter.member).filter(|held| !held.is_null()) else {
        return false;
    };
    match filter.test {
        Test::Equal { loose, .. } => is_wanted(held, &filter.values, loose),
        Test::HoldsAny => held.is_array() && !contradicts(item, filter),
    }
}

/// Whether `value` holds `wanted` whole, as text or a number, at any depth up to
/// [`MAX_PROJECTED_DEPTH`].
fn mentions(value: &Value, wanted: &str, depth: usize) -> bool {
    if depth >= MAX_PROJECTED_DEPTH {
        return false;
    }
    match value {
        Value::Object(members) => members.values().any(|v| mentions(v, wanted, depth + 1)),
        Value::Array(items) => items.iter().any(|v| mentions(v, wanted, depth + 1)),
        other => id_string(other).is_some_and(|held| held.trim() == wanted),
    }
}

/// Whether stored `r` shows it is in the collection `call`'s path names: as recorded
/// there by this runtime, or, for each path parameter, by a member tied to it holding
/// its value (as [`overlay_stored`] ties them), or an object holding it. The record
/// [`Collections`] keeps is this process's own, so on a store several nodes share, a
/// record another node wrote counts only by its members. A route with no path
/// parameter is one collection; [`stored_rest`] reads one only when it is the type's
/// sole such list (see [`BundleIndex::sole_written_list`]).
fn in_collection(
    rt: &MockRuntime,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    item_type: &str,
    field: &str,
    r: &StoredResource,
) -> bool {
    if call.path_params.is_empty() {
        return true;
    }
    if let Some(collection) = collection_of(plan.op, call)
        && rt.collections().get(&plan.session, r) == Some(collection)
    {
        return true;
    }
    let Some(members) = r.data.as_object() else {
        return false;
    };
    call.path_params.iter().all(|(param, value)| {
        let Some(value) = id_string(value) else {
            return false;
        };
        members.iter().any(|(member, held)| {
            member != field
                && !mb_mcm::is_generic_id(member)
                && tied_to_param(member, param)
                && !typed_as_own(rt, item_type, member)
                && match held {
                    Value::Object(inner) => inner
                        .values()
                        .any(|v| id_string(v).as_deref() == Some(value.as_str())),
                    held => id_string(held).as_deref() == Some(value.as_str()),
                }
        })
    })
}

/// A value of a list read's query that is no filter, page size or list of fields, which
/// a stored record must show to be served on a next page (see [`stored_rest`]).
struct Shown<'q> {
    param: &'q str,
    texts: Vec<String>,
    /// Free text (no `enum`, `const`, `format` or `pattern`): a value holding no digit
    /// is shown by any text holding it, ignoring case.
    loose: bool,
}

impl Shown<'_> {
    /// Whether stored `r` shows every one of the values: the create that stored it
    /// carried it under the same parameter, or `r` holds it whole, or, for free text,
    /// as part of a text.
    fn holds(&self, rt: &MockRuntime, session: &SessionKey, r: &StoredResource) -> bool {
        self.texts.iter().all(|value| {
            rt.collections().carried(session, r, self.param, value)
                || mentions(&r.data, value, 0)
                || (self.loose && !holds_a_digit(value) && contains_text(&r.data, value, 0))
        })
    }
}

/// Whether `value` holds a text containing `wanted`, ignoring case, at any depth up to
/// [`MAX_PROJECTED_DEPTH`].
fn contains_text(value: &Value, wanted: &str, depth: usize) -> bool {
    if depth >= MAX_PROJECTED_DEPTH || wanted.is_empty() {
        return false;
    }
    match value {
        Value::Object(members) => members
            .values()
            .any(|v| contains_text(v, wanted, depth + 1)),
        Value::Array(items) => items.iter().any(|v| contains_text(v, wanted, depth + 1)),
        Value::String(text) => text.to_lowercase().contains(&wanted.to_lowercase()),
        _ => false,
    }
}

/// Records of the list's item type a list read looks at for what a full page left out,
/// at most.
const MAX_STORED_REST: usize = 100;

/// `cut` with, after the items it left out, the records the session holds that a full
/// page of list answer `body` does not show but that its query asks for: of the items'
/// type, in the route's collection (see [`in_collection`]), holding what each of the
/// query's filters asks, and showing the request's every other value, unless the
/// parameter is an array or free text (no `enum` or `const`) and all of that value's
/// comma-separated words name members of the items (`fields=name,notes`), or the value
/// is the parameter's `default` (`type=scheduled`), which asks what leaving it out asks.
/// A record shows a value when the create that stored it carried the same value under
/// the same parameter (a scope, `organization_id=…`, as this runtime's writes tell; see
/// [`Collections::carried`]), when it holds the value whole at any depth, or, for free
/// text (no `enum`, `const`, `format` or `pattern`) and a value holding no digit, when
/// one of its texts holds the value, ignoring case (`q=walk` in `Walk the dog`). A value
/// the mock cannot read (an expression, `filter=any(email,[…])`) is shown by no record
/// whole, so it keeps every record off the next page, and the next page goes to the
/// model as before: a debug event names the parameters, as nothing else tells. No
/// meaning is guessed for such a value. A value of a
/// number parameter, which could be found anywhere, leaves `cut` as it is, unless it is
/// the parameter's `default` or `minimum` (`offset=0`), which asks for nothing; so does a
/// value of any other kind. Each record shows as the items declare it ([`projected`]), and only where it
/// validates as one; one with no identity, or the identity of an item served or left
/// out, is not taken. A page is full when it holds as many items as the request's page
/// size asks. The store is read only when the answer has a place for a token
/// ([`pages::slot`]), or else when the request asks for the first page of a list paged
/// by number or offset ([`runs::starts`]). A token the model gave there is read past:
/// records the store holds for a next page come before the model's own next page, so
/// [`pages::mint`] writes a token of the mock's over it. A route with no path parameter is read only
/// when it is the type's sole list read with none and a write of the type shares its
/// path ([`BundleIndex::sole_written_list`]). The records go least recently written
/// first, as the store orders them by their last write. The items' type `t` when their
/// schema names no type of its own. Nothing is written.
pub(super) async fn stored_rest(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: Option<&str>,
    body: &Value,
    cut: Option<Cut>,
) -> Option<Cut> {
    let rt = state.rt;
    let bundle = rt.bundle();
    let op = plan.op;
    if !plain(plan) {
        return cut;
    }
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return cut;
    };
    let mut held = None;
    let Some((array_parts, item_parts)) = list_parts(bundle, schema, body, &mut held) else {
        return cut;
    };
    let Some(items_schema) = array_parts.iter().find_map(|p| p.get("items")) else {
        return cut;
    };
    let query = op.input.query.as_ref();
    let doc = std::cell::OnceCell::new();
    let page = requested_page(
        query,
        || doc.get_or_init(|| bundle.with_reachable_defs(schema)),
        call,
    );
    let Some((size_param, size)) =
        page.and_then(|(name, limit)| Some((name, page_size(limit, &array_parts)?)))
    else {
        return cut;
    };
    let Some(served) = list_items(body).filter(|items| size > 0 && items.len() >= size) else {
        return cut;
    };
    // No place for a token, and no first page of a list paged by number or offset: no
    // next page to serve, and the store is not read.
    let slot = pages::slot(
        bundle,
        (op, call, plan.page_url.as_deref()),
        body,
        size_param,
    );
    if slot.is_none() && !runs::starts(bundle, (op, call), size_param) {
        return cut;
    }
    let Some(item_type) = rt
        .index()
        .item_type(bundle, op, body)
        .or_else(|| t.map(str::to_string))
    else {
        return cut;
    };
    // A route with no path parameter holds all of the type's records only when it is
    // the type's one such list, and a write of the type goes to its path.
    if call.path_params.is_empty() {
        let Binding::Http { path, .. } = &op.binding else {
            return cut;
        };
        if !rt.index().sole_written_list(path, &item_type) {
            return cut;
        }
    }
    let filters = filters_of(bundle, query, &item_parts, call, page);
    let declared = declared_names(&item_parts);
    let declared_params = query.and_then(|q| q.get("properties"));
    let mut shown: Vec<Shown<'_>> = Vec::new();
    for (param, raw) in &call.query {
        if param == size_param || filters.iter().any(|(name, _)| name == param) {
            continue;
        }
        let texts: Vec<String> = match raw {
            Value::Array(values) => values.iter().filter_map(id_string).collect(),
            other => id_string(other).into_iter().collect(),
        };
        let texts: Vec<String> = texts
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if texts.is_empty() {
            // `cursor=` asks for nothing; a value of another kind is not known.
            if matches!(raw, Value::Bool(_) | Value::Object(_)) {
                return cut;
            }
            continue;
        }
        let (param_parts, _) =
            schema_parts(bundle, declared_params.and_then(|d| d.get(param.as_str())));
        let param_types = types_of(&param_parts);
        // The parameter's `default` asks what leaving it out asks: nothing.
        let at_default = texts.len() == 1
            && param_parts
                .iter()
                .filter_map(|p| p.get("default").and_then(id_string))
                .any(|default| default.trim() == texts[0]);
        if at_default {
            continue;
        }
        // A number is found anywhere (a count, a price): such a value is not known, but
        // for the parameter's `default` or `minimum` (`offset=0`), which asks nothing.
        if param_types.contains("integer") || param_types.contains("number") {
            let bound = |at: &str| {
                param_parts
                    .iter()
                    .filter_map(|p| p.get(at).and_then(Value::as_f64))
                    .collect::<Vec<_>>()
            };
            let bounds = [bound("default"), bound("minimum")].concat();
            let at_bound =
                texts.len() == 1 && texts[0].parse::<f64>().is_ok_and(|n| bounds.contains(&n));
            if at_bound {
                continue;
            }
            return cut;
        }
        // Only an array or free text lists fields: an `enum` value is a choice.
        let may_project = !param_parts
            .iter()
            .any(|p| p.get("enum").is_some() || p.get("const").is_some())
            && (param_types.iter().eq(["array"].iter())
                || param_types.iter().eq(["string"].iter())
                || param_types.is_empty());
        let names_members = may_project
            && texts.iter().all(|text| {
                text.split(',').all(|word| {
                    let word = word.trim();
                    let head = word.split('.').next().unwrap_or(word);
                    declared.contains(&head)
                })
            });
        if !names_members {
            // Text the contract says nothing more of is often searched by part.
            let loose = param_types.iter().eq(["string"].iter())
                && !param_parts.iter().any(|p| {
                    ["enum", "const", "format", "pattern"]
                        .iter()
                        .any(|at| p.get(*at).is_some())
                });
            shown.push(Shown {
                param: param.as_str(),
                texts,
                loose,
            });
        }
    }
    let field = item_identity(bundle, op, body);
    let identity = |item: &Value| item.get(&field).and_then(id_string);
    let mut known: Vec<String> = served.iter().filter_map(identity).collect();
    if let Some(cut) = &cut {
        known.extend(cut.rest.iter().filter_map(identity));
    }
    let valid = |item: &Value| {
        let mut candidate = body.clone();
        let Some(slot) = list_items_mut(&mut candidate).and_then(|items| items.first_mut()) else {
            return false;
        };
        *slot = item.clone();
        plan.engine
            .serve_stored(bundle, op, &candidate, None)
            .is_some()
    };
    let mut records = state.list(&plan.session, &item_type, MAX_STORED_REST).await;
    let full_read = records.len() >= MAX_STORED_REST;
    // The store gives the most recently written first; the least recently written go
    // first here, which for records only created is the order they were made in.
    records.reverse();
    let mut missing = Vec::new();
    // Records withheld only for a value of the request none of them shows.
    let mut withheld = 0usize;
    for r in &records {
        if !in_collection(rt, (plan, call), &item_type, &field, r) {
            continue;
        }
        let item = projected(bundle, Some(items_schema), &r.data, 0);
        let Some(id) = identity(&item) else {
            continue;
        };
        if known.contains(&id)
            || !filters
                .iter()
                .all(|(_, filter)| holds_wanted(&item, filter))
        {
            continue;
        }
        if !shown.iter().all(|shown| shown.holds(rt, &plan.session, r)) {
            withheld += 1;
            continue;
        }
        if !valid(&item) {
            continue;
        }
        known.push(id);
        missing.push(item);
    }
    if missing.is_empty() {
        if withheld > 0 {
            let params: Vec<&str> = shown.iter().map(|shown| shown.param).collect();
            tracing::debug!(
                operation = %op.id,
                withheld,
                ?params,
                "stored records kept off a next page: no record shows a value of the request"
            );
        }
        return cut;
    }
    let stored = missing.len();
    let mut cut = cut.unwrap_or_else(|| Cut {
        rest: Vec::new(),
        size,
        size_param: size_param.to_string(),
        stored: 0,
        truncated: false,
    });
    cut.rest.extend(missing);
    cut.stored = stored;
    cut.truncated = full_read;
    Some(cut)
}

/// `value` as `schema` declares it, at every depth: of an object, the members its parts
/// declare, each in turn as its own schema declares it, and others only where
/// `additionalProperties` gives them a schema; of an array, each element as its
/// `items` declares it. An object whose schema declares no members and no
/// `additionalProperties: false` is free-form, and stays as it is, as does a scalar.
fn projected(
    bundle: &mb_mcm::Bundle,
    schema: Option<&Value>,
    value: &Value,
    depth: usize,
) -> Value {
    let Some(schema) = schema.filter(|_| depth < MAX_PROJECTED_DEPTH) else {
        return value.clone();
    };
    let (parts, _) = schema_parts(bundle, Some(schema));
    match value {
        Value::Object(members) => {
            // A member several parts declare (`allOf`) is declared as all of them.
            let declared = |name: &str| -> Option<Value> {
                let mut found: Vec<&Value> = parts
                    .iter()
                    .filter_map(|p| p.get("properties")?.get(name))
                    .collect();
                match found.len() {
                    0 => None,
                    1 => found.pop().cloned(),
                    _ => Some(serde_json::json!({"allOf": found})),
                }
            };
            let additional = parts.iter().find_map(|p| p.get("additionalProperties"));
            let declares_members = parts.iter().any(|p| p.get("properties").is_some());
            let closed = matches!(additional, Some(Value::Bool(false)));
            if !declares_members && !closed && !additional.is_some_and(Value::is_object) {
                return value.clone();
            }
            let mut out = Map::new();
            for (name, member) in members {
                let schema =
                    declared(name).or_else(|| additional.filter(|a| a.is_object()).cloned());
                if let Some(schema) = schema {
                    out.insert(
                        name.clone(),
                        projected(bundle, Some(&schema), member, depth + 1),
                    );
                }
            }
            Value::Object(out)
        }
        Value::Array(elements) => {
            let items = parts.iter().find_map(|p| p.get("items"));
            Value::Array(
                elements
                    .iter()
                    .map(|element| projected(bundle, items, element, depth + 1))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

/// Has each item of list answer `body` that the session holds, by its identity and its
/// type, show the stored record's members as the item declares them, but for an item
/// whose own parent the record contradicts, which stays as generated (see the module's
/// doc). The items of type `t` when their schema names no type of its own.
pub(super) async fn overlay_stored(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: Option<&str>,
    body: &mut Value,
) {
    let rt = state.rt;
    let bundle = rt.bundle();
    let op = plan.op;
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let mut held = None;
    let Some((_, item_parts)) = list_parts(bundle, schema, body, &mut held) else {
        return;
    };
    let declared = declared_names(&item_parts);
    if declared.is_empty() {
        return;
    }
    let Some(item_type) = rt
        .index()
        .item_type(bundle, op, body)
        .or_else(|| t.map(str::to_string))
    else {
        return;
    };
    let field = item_identity(bundle, op, body);
    // The members tied to the path's parameters, as owner checks tie them, that may
    // name a parent: not the items' identity, nor one typed as the items' own type.
    let parents: Vec<(&str, String)> = call
        .path_params
        .iter()
        .filter_map(|(param, value)| Some((param, id_string(value)?)))
        .flat_map(|(param, value)| {
            declared
                .iter()
                .filter(|member| {
                    **member != field
                        && !mb_mcm::is_generic_id(member)
                        && tied_to_param(member, param)
                        && !typed_as_own(rt, &item_type, member)
                })
                .map(move |member| (*member, value.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    let Some(items) = list_items(body) else {
        return;
    };
    let mut overlaid: Vec<(usize, Value)> = Vec::new();
    for (at, item) in items.iter().enumerate().take(MAX_OVERLAID) {
        let (Some(own), Some(id)) = (item.as_object(), item.get(&field).and_then(id_string)) else {
            continue;
        };
        let Some(stored) = state.get(&plan.session, &item_type, &id).await else {
            continue;
        };
        // The item says it is the path's, the record says another's: a guess either
        // way, so the item stays as generated.
        let contradicted = parents.iter().any(|(member, value)| {
            let says = |data: &Value| data.get(*member).and_then(id_string);
            says(item).as_deref() == Some(value.as_str())
                && says(&stored.data).is_some_and(|held| &held != value)
        });
        if contradicted {
            continue;
        }
        let mut merged = own.clone();
        let mut changed = false;
        for name in &declared {
            let Some(value) = stored.data.get(*name) else {
                continue;
            };
            let parts: Vec<&Value> = item_parts
                .iter()
                .filter_map(|p| p.get("properties")?.get(*name))
                .collect();
            let schema = match parts.as_slice() {
                [] => None,
                [one] => Some((*one).clone()),
                many => Some(serde_json::json!({"allOf": many})),
            };
            let value = projected(bundle, schema.as_ref(), value, 0);
            if own.get(*name) != Some(&value) {
                merged.insert((*name).to_string(), value);
                changed = true;
            }
        }
        if changed {
            overlaid.push((at, Value::Object(merged)));
        }
    }
    if overlaid.is_empty() {
        return;
    }
    let valid = |candidate: &Value| {
        plan.engine
            .serve_stored(bundle, op, candidate, None)
            .is_some()
    };
    let with = |base: &Value, changes: &[(usize, Value)]| {
        let mut candidate = base.clone();
        if let Some(items) = list_items_mut(&mut candidate) {
            for (at, item) in changes {
                if let Some(slot) = items.get_mut(*at) {
                    *slot = item.clone();
                }
            }
        }
        candidate
    };
    let all = with(body, &overlaid);
    if valid(&all) {
        *body = all;
        return;
    }
    // One record that would not validate in its place leaves only its own item as it was.
    for change in &overlaid {
        let candidate = with(body, std::slice::from_ref(change));
        if valid(&candidate) {
            *body = candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn free_text_is_contained_ignoring_case_at_any_depth() {
        let record = json!({"name": "Walk the Dog", "updates": [{"body": "Mains outage"}]});
        assert!(contains_text(&record, "walk", 0));
        assert!(contains_text(&record, "OUTAGE", 0));
        assert!(!contains_text(&record, "cat", 0));
        assert!(!contains_text(&record, "", 0));
        assert!(!contains_text(&json!({"count": 12}), "12", 0));
    }

    #[test]
    fn an_expression_is_held_by_no_record() {
        let record = json!({"email": "a@x.org"});
        let expr = r#"any(email,["a@x.org"])"#;
        assert!(!mentions(&record, expr, 0));
        assert!(!contains_text(&record, expr, 0));
    }
}
