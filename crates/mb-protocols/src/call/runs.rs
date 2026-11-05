//! The later pages of a list paged by number or offset, served from its first.
//!
//! [`pages`] serves the rest of a cut list by a token. A list paged by a page number
//! (`page=2`) or an offset (`offset=1`) has no token: the client asks for the next page
//! by the number, and the model, which does not know what the first page showed, answers
//! the first page again. So a first page that leaves items out, as [`lists::narrow`] cut
//! them or as the session's records the page does not show ([`lists::stored_rest`]), is
//! kept with them as a run, and a later page is the run's next items.
//!
//! The parameter is known by the contract's structure: an integer query parameter of the
//! list (no `const` or `enum`, no bare `id`, naming no member of the items) beside the
//! page size, whose `default` and `minimum` are both `1` (a page number) or both `0` (an
//! offset), or that declares that `minimum` and no `default` where its name says it is
//! one or it is a position of the page size's bracket family (being required says
//! nothing: a required, echoed `week` from 1 is a filter); and that the contract
//! confirms as one: the answer declares one integer member named as it, beside the
//! list or in an object beside it (Zoho's `page_context.page`, Box's `offset`), or the
//! parameter's words name it (WooCommerce's bare array's `page`, `page[number]`,
//! `pageNo`, `Offset`, `page[offset]`: brackets split the words), or it is a position
//! of the page size's bracket family: `page[start]` beside `page[size]`, its bracket
//! word one of [`POSITION_WORDS`]. A bound or a cursor of that family (`page[after]`,
//! `page[before]`, `page[cursor]`) says where a page starts by an item, not by a count,
//! and is none. An integer with such bounds and no such evidence (`min_rating`,
//! `depth`) is none. Of two confirmed, only one named `page` or `offset` is; and one so
//! named whose schema counts the other way (Trello's `page` from `0`, a `page_no` from
//! `0`) leaves the list as it was. A request asks for the first page when it carries the
//! parameter at `1` (or `0`), or not at all.
//!
//! A run is kept once per route, path values and other query values (the request's but
//! the page's and the page size's), in one of [`pages::MAX_CUTS`] places those pick,
//! under the store's reserved type ([`mb_state::INTERNAL_TYPE`]): the page as answered,
//! and the items after it. Every first page answered replaces it, or ends it when it
//! leaves nothing out; one of another list whose values pick the same place ends it
//! early, and its later pages go to the model. A first page holding more items than its
//! page size (a cut the contract's `minItems` kept from being made) keeps none. A count
//! of the list beside it that counts fewer than the page and the rest is raised to them.
//! A value of another number parameter, other than its `default` or `minimum`, keeps no
//! run and is served none, as [`lists::stored_rest`] says.
//!
//! A request on the same route, path and other query values, carrying the page size and
//! a later page (`page=2`, `offset=1`), is served without a model call: the items at its
//! place in the run (the first page's, then the rest, but those deleted since, so a
//! later page starts where they leave it), as many as its page size asks,
//! each showing its stored record ([`lists::overlay_stored`]), the query's filters
//! ([`lists::narrow`]) and the delete markers ([`deleted::drop_from_list`]) applied
//! again. A page past the run's items is the model's. The member that confirmed the
//! parameter holds the request's value where it held the first page's; so does the one
//! integer member beside the list, or in an object beside it, named as the page size.
//! The more flag is the one boolean the answer declares in the nested object holding
//! that member (`page_context.has_more_page`, as [`pages`]' sits beside its token); with
//! no such member, one in the envelope itself (where `success` sits beside `page`), or
//! several booleans there, there is none. The first page keeps the model's value. The
//! last page sets it `false` where the first said `true` only when the model answered
//! more items than its page size, so that `true` said more follow; of a run the store's
//! records made, the model saw one full page, and its flag stays as given. Nor does it
//! when the run may hold fewer records than the store (a read of it that came back
//! full). Such a page is not cached, as the run serves it.
//!
//! An LLM and `consistency: on` only, as [`deleted::applies`] says, and plain calls
//! (no scenario or forced status).

use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::pages::{MAX_CUTS, booleans_at, bound_query, path_of, placed, with_items, without_more};
use super::*;

/// How a page-start parameter counts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    /// A page number, from 1: `page=2` starts after one page.
    Number,
    /// The index of the first item, from 0: `offset=1` starts at the second.
    Offset,
}

impl Style {
    /// The parameter's value at the first item.
    fn first(self) -> usize {
        match self {
            Self::Number => 1,
            Self::Offset => 0,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Offset => "offset",
        }
    }

    fn of(name: &str) -> Option<Self> {
        match name {
            "number" => Some(Self::Number),
            "offset" => Some(Self::Offset),
            _ => None,
        }
    }
}

/// A list's page-start parameter, confirmed by the contract (see the module's doc).
struct Position {
    param: String,
    style: Style,
    /// The path of the answer's member echoing it, when there is one.
    echo: Option<Vec<String>>,
}

/// The member holding the list in `op`'s answer (`None` for a bare array), and the
/// schema parts of its items.
fn answer_lists<'a>(
    bundle: &'a mb_mcm::Bundle,
    op: &'a Operation,
) -> Option<(Option<String>, Vec<&'a Value>)> {
    let schema = op.success_output()?.schema.as_ref()?;
    let (parts, _) = schema_parts(bundle, Some(schema));
    let array = |parts: &[&Value]| lists::types_of(parts).into_iter().eq(["array"]);
    let (list, array_parts) = if array(&parts) {
        (None, parts)
    } else {
        // As an answer's list is found: an envelope's key first (Box's `entries`
        // beside `order`), else the only list.
        let members = mb_engine::consistency::list_members(bundle, schema);
        let list = mb_engine::consistency::ENVELOPE_KEYS
            .iter()
            .find_map(|key| members.iter().find(|m| m == key))
            .or(match members.as_slice() {
                [one] => Some(one),
                _ => None,
            })?;
        let member = parts.iter().find_map(|p| p.get("properties")?.get(list))?;
        (Some(list.clone()), schema_parts(bundle, Some(member)).0)
    };
    let items = array_parts.iter().find_map(|p| p.get("items"))?;
    Some((list, schema_parts(bundle, Some(items)).0))
}

/// The members `op`'s answer declares beside its list `list`, and those each object
/// member beside it declares, with their schema parts: each by its path from the top.
fn beside_list<'a>(
    bundle: &'a mb_mcm::Bundle,
    op: &'a Operation,
    list: &str,
) -> Vec<(Vec<String>, Vec<&'a Value>)> {
    let mut out = Vec::new();
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return out;
    };
    let (parts, _) = schema_parts(bundle, Some(schema));
    for (name, member) in parts
        .iter()
        .filter_map(|p| p.get("properties").and_then(Value::as_object))
        .flatten()
        .filter(|(name, _)| name.as_str() != list)
    {
        let (member_parts, _) = schema_parts(bundle, Some(member));
        for (inner_name, inner) in member_parts
            .iter()
            .filter_map(|p| p.get("properties").and_then(Value::as_object))
            .flatten()
        {
            let (inner_parts, _) = schema_parts(bundle, Some(inner));
            out.push((vec![name.clone(), inner_name.clone()], inner_parts));
        }
        out.push((vec![name.clone()], member_parts));
    }
    out
}

/// The path of the one integer member `op`'s answer declares named `name` beside its
/// list `list`, or in an object beside it; `None` with none or several.
fn echo_of(bundle: &mb_mcm::Bundle, op: &Operation, list: &str, name: &str) -> Option<Vec<String>> {
    let mut found = beside_list(bundle, op, list)
        .into_iter()
        .filter(|(path, parts)| {
            path.last().is_some_and(|last| last == name)
                && lists::types_of(parts).into_iter().eq(["integer"])
        })
        .map(|(path, _)| path);
    let one = found.next()?;
    found.next().is_none().then_some(one)
}

/// Words beside `page` that still name a page number: `page[number]`, `pageNo`.
const PAGE_NUMBER_WORDS: [&str; 5] = ["number", "num", "no", "index", "idx"];

/// How a parameter's name says it counts: by its [`mb_mcm::words`] (brackets split, so
/// `page[number]` is `page number`), an `offset` among them is an offset, and `page`
/// alone or with a word of [`PAGE_NUMBER_WORDS`] a page number. `per_page`, a size,
/// is neither.
fn named(name: &str) -> Option<Style> {
    let words: Vec<String> = mb_mcm::words(name)
        .into_iter()
        .map(str::to_lowercase)
        .collect();
    if words.iter().any(|w| w == "offset") && words.iter().all(|w| w == "offset" || w == "page") {
        return Some(Style::Offset);
    }
    let page = words.iter().filter(|w| *w == "page").count() == 1;
    let rest_numbers = words
        .iter()
        .filter(|w| *w != "page")
        .all(|w| PAGE_NUMBER_WORDS.contains(&w.as_str()));
    (page && rest_numbers && words.len() <= 2).then_some(Style::Number)
}

/// Bracket words that say a page starts at a count: `page[start]`, `page[number]`,
/// `page[offset]`. A bound or cursor (`page[after]`, `page[cursor]`) is none of them.
const POSITION_WORDS: [&str; 9] = [
    "start", "number", "num", "no", "offset", "index", "idx", "skip", "position",
];

/// Whether `name` is a position of the page size `size_param`'s paging bracket family:
/// `page[start]` beside `page[size]`. The shared base must name pages (its words are
/// `page`), so `q[min_stars]` beside `q[per_page]` is no family; and `name`'s own
/// bracket word must be one of [`POSITION_WORDS`], so `page[after]`, a keyset bound,
/// is no position.
fn family_position(name: &str, size_param: &str) -> bool {
    let split = |n: &str| {
        let open = n.find('[')?;
        let inner = n.strip_suffix(']')?.get(open + 1..)?;
        Some((n[..open].to_lowercase(), inner.to_string()))
    };
    let (Some((base, inner)), Some((size_base, _))) = (split(name), split(size_param)) else {
        return false;
    };
    let inner = mb_mcm::words(&inner);
    base == size_base
        && !base.is_empty()
        && mb_mcm::words(&base)
            .iter()
            .all(|w| w.eq_ignore_ascii_case("page"))
        && !inner.is_empty()
        && inner
            .iter()
            .all(|w| POSITION_WORDS.contains(&w.to_lowercase().as_str()))
}

/// The query parameter of `op` that says where a page starts, beside page size
/// `size_param`, as the contract confirms it (see the module's doc).
fn position(bundle: &mb_mcm::Bundle, op: &Operation, size_param: &str) -> Option<Position> {
    let query = op.input.query.as_ref()?;
    let declared = query.get("properties")?.as_object()?;
    let (list, item_parts) = answer_lists(bundle, op)?;
    let mut found: Vec<Position> = Vec::new();
    for (name, schema) in declared {
        if name == size_param
            || mb_mcm::is_generic_id(name)
            // A parameter naming a member of the items filters them.
            || lists::named_member(bundle, &item_parts, name).is_some()
        {
            continue;
        }
        let (parts, _) = schema_parts(bundle, Some(schema));
        if !lists::types_of(&parts).into_iter().eq(["integer"])
            || parts
                .iter()
                .any(|p| p.get("const").is_some() || p.get("enum").is_some())
        {
            continue;
        }
        let bound = |at: &str| {
            let mut values = parts
                .iter()
                .filter_map(|p| p.get(at).and_then(Value::as_f64));
            let first = values.next()?;
            values.all(|v| v == first).then_some(first)
        };
        let said = named(name);
        let family = family_position(name, size_param);
        let style = match (bound("default"), bound("minimum")) {
            (Some(d), Some(m)) if d == 1.0 && m == 1.0 => Style::Number,
            (Some(d), Some(m)) if d == 0.0 && m == 0.0 => Style::Offset,
            // A `minimum` alone, where the name or a position of the page size's
            // bracket family says more (`page[number]`, `Offset`, `page[start]`).
            // Being required says nothing: a required, echoed `week` from 1 is a filter.
            (None, Some(m)) if m == 1.0 && (said.is_some() || family) => Style::Number,
            (None, Some(m)) if m == 0.0 && (said.is_some() || family) => Style::Offset,
            _ => continue,
        };
        // A name saying the other way (Trello's `page` from 0) leaves it unknown.
        if said.is_some_and(|said| said != style) {
            return None;
        }
        let echo = list
            .as_deref()
            .and_then(|list| echo_of(bundle, op, list, name));
        // Bounds alone (`min_rating`, `depth`) are no evidence.
        if echo.is_none() && said.is_none() && !family {
            continue;
        }
        found.push(Position {
            param: name.clone(),
            style,
            echo,
        });
    }
    if found.len() > 1 {
        found.retain(|p| named(&p.param).is_some());
    }
    let one = found.pop()?;
    found.is_empty().then_some(one)
}

/// A query value as a whole number.
fn whole(value: &Value) -> Option<usize> {
    let n = match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }?;
    usize::try_from(n).ok()
}

/// Whether every number `call`'s query carries, but under `skip`, is its parameter's
/// `default` or `minimum`, which asks for nothing: any other (a count, a price) could be
/// found anywhere, so which items it keeps is not known (as [`lists::stored_rest`] has it).
fn numbers_ask_nothing(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    call: &OperationCall,
    skip: [&str; 2],
) -> bool {
    let declared = op.input.query.as_ref().and_then(|q| q.get("properties"));
    call.query
        .iter()
        .filter(|(param, _)| !skip.contains(&param.as_str()))
        .all(|(param, raw)| {
            let (parts, _) = schema_parts(bundle, declared.and_then(|d| d.get(param.as_str())));
            let types = lists::types_of(&parts);
            if !types.contains("integer") && !types.contains("number") {
                return true;
            }
            let text = match raw {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.trim().to_string(),
                _ => return false,
            };
            if text.is_empty() {
                return true;
            }
            let Ok(n) = text.parse::<f64>() else {
                return false;
            };
            ["default", "minimum"].iter().any(|at| {
                parts
                    .iter()
                    .any(|p| p.get(*at).and_then(Value::as_f64) == Some(n))
            })
        })
}

/// Whether `call` asks for the first page of a list `op` pages by a page number or an
/// offset beside page size `size_param`: it carries the parameter at its first value, or
/// not at all.
pub(super) fn starts(
    bundle: &mb_mcm::Bundle,
    (op, call): (&Operation, &OperationCall),
    size_param: &str,
) -> bool {
    position(bundle, op, size_param).is_some_and(|p| match call.query.get(&p.param) {
        None => true,
        Some(value) => whole(value) == Some(p.style.first()),
    })
}

/// The prefix of a run's document.
const PREFIX: &str = "#run/";

/// The place of the run of `op` on `path` with other query values `query`: one of
/// [`MAX_CUTS`], picked by them, so a first page and its later ones find the same.
fn key(op: &Operation, path: &Value, query: &Value) -> String {
    let mut hasher = Sha256::new();
    for part in [op.id.clone(), path.to_string(), query.to_string()] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    format!("{PREFIX}{:04x}", u64::from_be_bytes(head) % MAX_CUTS)
}

/// A kept first page of a list paged by number or offset, and the items after it.
struct Run {
    op: String,
    path: Value,
    query: Value,
    param: String,
    style: Style,
    size: usize,
    size_param: String,
    /// The first page as answered.
    page: Value,
    rest: Vec<Value>,
    /// Whether the store may hold more records than `rest`.
    truncated: bool,
    /// Whether the model answered more items than the page size, so that its more
    /// flag, if `true`, said there were more.
    from_cut: bool,
}

impl Run {
    fn to_value(&self) -> Value {
        serde_json::json!({
            "op": self.op,
            "path": self.path,
            "query": self.query,
            "param": self.param,
            "style": self.style.name(),
            "size": self.size,
            "size_param": self.size_param,
            "page": self.page,
            "rest": self.rest,
            "truncated": self.truncated,
            "from_cut": self.from_cut,
        })
    }

    fn of(mut value: Value) -> Option<Self> {
        let text = |value: &Value, key: &str| value.get(key)?.as_str().map(str::to_string);
        let rest = match value.get_mut("rest")?.take() {
            Value::Array(items) => items,
            _ => return None,
        };
        Some(Self {
            op: text(&value, "op")?,
            path: value.get("path")?.clone(),
            query: value.get("query")?.clone(),
            param: text(&value, "param")?,
            style: Style::of(value.get("style")?.as_str()?)?,
            size: usize::try_from(value.get("size")?.as_u64()?).ok()?,
            size_param: text(&value, "size_param")?,
            truncated: value.get("truncated")?.as_bool()?,
            from_cut: value.get("from_cut")?.as_bool()?,
            page: value.get_mut("page")?.take(),
            rest,
        })
    }
}

/// The more flag of a list whose page-start parameter is `position`: the one boolean
/// the answer declares in the object holding its echo, its parents and name, when that
/// object is nested (Zoho's `page_context`): in the envelope itself, a boolean beside
/// the echo (`success`) says nothing of pages.
fn flag(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    position: &Position,
) -> Option<(Vec<String>, String)> {
    let (_, parents) = position.echo.as_ref()?.split_last()?;
    if parents.is_empty() {
        return None;
    }
    match booleans_at(bundle, op, parents).as_slice() {
        [one] => Some((parents.to_vec(), one.clone())),
        _ => None,
    }
}

/// `body` with the member at `path` holding `now` where it holds `was`.
fn echoed(body: &Value, path: &[String], (was, now): (usize, usize)) -> Option<Value> {
    let held = path.iter().try_fold(body, |at, name| at.get(name))?;
    (whole(held) == Some(was)).then(|| placed(body, path, Some(Value::from(now))))?
}

/// Forgets the run a first page `call` of a list paged by number or offset replaces,
/// when the page keeps none.
pub(super) async fn forget(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
) {
    let bundle = state.rt.bundle();
    let op = plan.op;
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let doc = std::cell::OnceCell::new();
    let Some((size_param, _)) = mb_engine::budget::requested_page(
        op.input.query.as_ref(),
        || doc.get_or_init(|| bundle.with_reachable_defs(schema)),
        call,
    ) else {
        return;
    };
    let Some(position) = position(bundle, op, size_param) else {
        return;
    };
    if !starts(bundle, (op, call), size_param) {
        return;
    }
    let id = key(
        op,
        &path_of(call),
        &bound_query(call, &position.param, size_param),
    );
    state.delete(&plan.session, INTERNAL_TYPE, &id).await;
}

/// The first page `body` of a list paged by number or offset, as it is served with
/// `rest` after it (and whether the store may hold more), and the run that keeps them
/// (see the module's doc); `None` when `call` asks for no first page, or no such page
/// validates.
pub(super) fn first_page(
    rt: &MockRuntime,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    body: &Value,
    (size, size_param): (usize, &str),
    (rest, truncated, from_cut): (Vec<Value>, bool, bool),
) -> Option<(Value, StoredResource)> {
    let bundle = rt.bundle();
    let op = plan.op;
    let position = position(bundle, op, size_param)?;
    if !starts(bundle, (op, call), size_param)
        || !numbers_ask_nothing(bundle, op, call, [&position.param, size_param])
    {
        return None;
    }
    // A page longer than its size shows items a later page would repeat.
    if list_items(body).map(Vec::len) != Some(size) {
        return None;
    }
    let valid = |candidate: &Value| {
        plan.engine
            .serve_stored(bundle, op, candidate, None)
            .is_some()
    };
    // A count of the list that counts less than the page and the rest counts them.
    let known = size + rest.len();
    let page = deleted::with_count_at_least(rt, op, body, known)
        .filter(|counted| valid(counted))
        .or_else(|| valid(body).then(|| body.clone()))?;
    let query = bound_query(call, &position.param, size_param);
    let path = path_of(call);
    let id = key(op, &path, &query);
    let run = Run {
        op: op.id.clone(),
        path,
        query,
        param: position.param,
        style: position.style,
        size,
        size_param: size_param.to_string(),
        page: page.clone(),
        rest,
        truncated,
        from_cut,
    };
    let document = StoredResource {
        resource_type: INTERNAL_TYPE.to_string(),
        id,
        data: run.to_value(),
    };
    Some((page, document))
}

/// A later page of a run this session keeps, with no model call (see the module's doc);
/// `None` when `call` asks for none. The caller checks the call is a plain list read.
pub(super) async fn serve(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Option<GeneratedResponse> {
    let rt = state.rt;
    let bundle = rt.bundle();
    let op = plan.op;
    let schema = op.success_output()?.schema.as_ref()?;
    let doc = std::cell::OnceCell::new();
    let (size_param, limit) = mb_engine::budget::requested_page(
        op.input.query.as_ref(),
        || doc.get_or_init(|| bundle.with_reachable_defs(schema)),
        call,
    )?;
    let position = position(bundle, op, size_param)?;
    let at = whole(call.query.get(&position.param)?)?;
    if !numbers_ask_nothing(bundle, op, call, [&position.param, size_param]) {
        return None;
    }
    let size = usize::try_from(limit.items).ok()?;
    let start = match position.style {
        Style::Number => at.checked_sub(1)?.checked_mul(size)?,
        Style::Offset => at,
    };
    if start == 0 {
        return None;
    }
    let path = path_of(call);
    let query = bound_query(call, &position.param, size_param);
    let found = state
        .get(&plan.session, INTERNAL_TYPE, &key(op, &path, &query))
        .await?;
    let run = Run::of(found.data)?;
    if run.op != op.id
        || run.path != path
        || run.query != query
        || run.param != position.param
        || run.style != position.style
        || run.size_param != size_param
    {
        return None;
    }
    // The run's items, the first page's then the rest, but those deleted since: a later
    // page starts where they leave it, as the API's would. Read up to one past the page,
    // which says whether more follow.
    let t = op.resource_type.as_deref();
    let field = item_identity(bundle, op, &run.page);
    let scope = deleted::scope(op, call);
    let wanted = start.saturating_add(size);
    let mut live = Vec::new();
    for item in list_items(&run.page)?.iter().cloned().chain(run.rest) {
        if live.len() > wanted {
            break;
        }
        let gone = match (t, mb_mcm::member_at(&item, &field).and_then(id_string)) {
            (Some(t), Some(id)) if state.tracks_deleted => {
                deleted::is_deleted(state, &plan.session, t, scope.as_deref(), &id).await
            }
            _ => false,
        };
        if !gone {
            live.push(item);
        }
    }
    // Past the items the run knows, the page is the model's.
    if start >= live.len() {
        return None;
    }
    let more = live.len() > wanted;
    let end = wanted.min(live.len());
    let mut body = with_items(&run.page, live.get(start..end)?.to_vec())?;
    // What the session wrote since, and the request's filters, as on a first page.
    lists::overlay_stored(state, (plan, call), t, &mut body).await;
    let _ = lists::narrow(rt, plan, call, &mut body);
    let valid = |candidate: &Value| plan.engine.serve_stored(bundle, op, candidate, None);
    // Where the answer says which page it is, and its size, it says this one's.
    let list = answer_lists(bundle, op).and_then(|(list, _)| list);
    let size_echo = list
        .as_deref()
        .and_then(|list| echo_of(bundle, op, list, size_param));
    for (echo, was, now) in [
        (position.echo.as_ref(), position.style.first(), at),
        (size_echo.as_ref(), run.size, size),
    ] {
        if was != now
            && let Some(said) = echo.and_then(|path| echoed(&body, path, (was, now)))
            && valid(&said).is_some()
        {
            body = said;
        }
    }
    // Only a model that answered more than a page said, with `true`, there was more: of
    // one full page it could not know, and its flag stays as given.
    let last = !more && !run.truncated && run.from_cut;
    let mut served = if last {
        // The last page: no more, as the first page said there was, where the answer
        // allows.
        flag(bundle, op, &position)
            .and_then(|(parents, flag)| without_more(&body, &run.page, &parents, &flag))
            .and_then(|done| valid(&done))
            .or_else(|| valid(&body))?
    } else {
        valid(&body)?
    };
    if let Some(t) = t
        && state.tracks_deleted
    {
        let scope = deleted::scope(op, call);
        deleted::drop_from_list(
            state,
            &plan.session,
            (plan, call),
            t,
            scope.as_deref(),
            &mut served,
        )
        .await;
    }
    Some(served)
}

#[cfg(test)]
mod tests {
    use super::family_position;

    #[test]
    fn a_paging_bracket_family_is_one() {
        assert!(family_position("page[start]", "page[size]"));
        assert!(family_position("Page[Offset]", "page[limit]"));
        assert!(family_position("page[number]", "page[size]"));
    }

    #[test]
    fn a_bracket_family_that_does_not_page_is_none() {
        assert!(!family_position("q[min_stars]", "q[per_page]"));
        assert!(!family_position("filter[min]", "filter[limit]"));
        assert!(!family_position("start", "size"));
    }

    /// A keyset bound or cursor of the page size's family says where a page starts by
    /// an item, not by a count.
    #[test]
    fn a_bound_or_cursor_of_the_family_is_no_position() {
        for name in [
            "page[after]",
            "page[before]",
            "page[cursor]",
            "page[since]",
            "page[until]",
            "page[from]",
            "page[to]",
            "page[]",
        ] {
            assert!(!family_position(name, "page[size]"), "{name}");
        }
    }
}
