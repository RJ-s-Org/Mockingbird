//! The later pages of a list paged by number or offset, served from its first.
//!
//! [`pages`] serves the rest of a cut list by a token. A list paged by a page number
//! (`page=2`) or an offset (`offset=1`) has no token: the client asks for the next page
//! by the number, and the model, which does not know what the first page showed, answers
//! the first page again. So a first page that leaves items out, as [`lists::narrow`] cut
//! them or as the session's records the page does not show ([`lists::stored_rest`]), is
//! kept with them as a run, and a later page is the run's next items.
//!
//! The parameter is known by its schema alone: an integer query parameter of the list
//! (no `const` or `enum`, no bare `id`, naming no member of the items) beside the page
//! size, whose `default` and `minimum` are both `1` (a page number: WooCommerce's,
//! Zoho's, DigitalOcean's `page`) or both `0` (an offset: Box's `offset`, Contentful's
//! `skip`). Two such parameters are told apart only by one being named `page` or
//! `offset`; and one so named whose schema counts the other way (Trello's `page` from
//! `0`) leaves the list as it was. A request asks for the first page when it carries the
//! parameter at `1` (or `0`), or not at all.
//!
//! A run is kept once per route, path values and other query values (the request's but
//! the page's and the page size's), in one of [`pages::MAX_CUTS`] places those pick,
//! under the store's reserved type ([`mb_state::INTERNAL_TYPE`]): the page as answered,
//! and the items after it. A new first page of the same list replaces it; one of another
//! list whose values pick the same place ends it early, and its later pages go to the
//! model. A count of the list beside it that counts fewer than the page and the rest is
//! raised to them, and the more flag (below) that is `false` is `true` where the answer
//! validates so, as with a token.
//!
//! A request on the same route, path and other query values, carrying the page size and
//! a later page (`page=2`, `offset=1`), is served without a model call: the items at its
//! place in the run (the first page's, then the rest), as many as its page size asks,
//! each showing its stored record ([`lists::overlay_stored`]), the query's filters
//! ([`lists::narrow`]) and the delete markers ([`deleted::drop_from_list`]) applied
//! again. A page past the run's items is the model's. The one integer member beside the
//! list, or in an object beside it, named as the parameter and holding the first page's
//! value (Zoho's `page_context.page`, Box's `offset`) holds the request's; so does one
//! named as the page size. The more flag is the one boolean the answer declares beside
//! the list or in an object beside it (Zoho's `page_context.has_more_page`); with none,
//! or several, there is none. When the first page set it `true`, the last page sets it
//! `false` where the answer validates so. Such a page is not cached, as the run serves it.
//!
//! An LLM and `consistency: on` only, as [`deleted::applies`] says, and plain calls
//! (no scenario or forced status).

use mb_engine::consistency::list_member;
use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::pages::{MAX_CUTS, bound_query, path_of, placed, with_items, with_more, without_more};
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

/// The query parameter of `op` that says where a page starts, beside page size
/// `size_param`, and how it counts (see the module's doc).
fn position(bundle: &mb_mcm::Bundle, op: &Operation, size_param: &str) -> Option<(String, Style)> {
    let declared = op.input.query.as_ref()?.get("properties")?.as_object()?;
    let mut found: Vec<(&str, Style)> = Vec::new();
    for (name, schema) in declared {
        if name == size_param || mb_mcm::is_generic_id(name) {
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
        let style = match (bound("default"), bound("minimum")) {
            (Some(d), Some(m)) if d == 1.0 && m == 1.0 => Style::Number,
            (Some(d), Some(m)) if d == 0.0 && m == 0.0 => Style::Offset,
            _ => continue,
        };
        // A name saying the other way (Trello's `page` from 0) leaves it unknown.
        let said = match mb_mcm::joined_name(name).as_str() {
            "page" => Some(Style::Number),
            "offset" => Some(Style::Offset),
            _ => None,
        };
        if said.is_some_and(|said| said != style) {
            return None;
        }
        found.push((name, style));
    }
    let named = |name: &str| matches!(mb_mcm::joined_name(name).as_str(), "page" | "offset");
    let (name, style) = match found.as_slice() {
        [one] => *one,
        _ => {
            let mut by_name = found.iter().filter(|(name, _)| named(name));
            let one = *by_name.next()?;
            if by_name.next().is_some() {
                return None;
            }
            one
        }
    };
    Some((name.to_string(), style))
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

/// Whether `call` asks for the first page of a list `op` pages by a page number or an
/// offset beside page size `size_param`: it carries the parameter at its first value, or
/// not at all.
pub(super) fn starts(
    bundle: &mb_mcm::Bundle,
    (op, call): (&Operation, &OperationCall),
    size_param: &str,
) -> bool {
    position(bundle, op, size_param).is_some_and(|(param, style)| match call.query.get(&param) {
        None => true,
        Some(value) => whole(value) == Some(style.first()),
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
            page: value.get_mut("page")?.take(),
            rest,
        })
    }
}

/// The members `op`'s answer declares beside list answer `body`'s list, and those each
/// object member beside it declares, with their schema parts: each by its path from the
/// top.
fn beside_list<'a>(
    bundle: &'a mb_mcm::Bundle,
    op: &'a Operation,
    body: &Value,
) -> Vec<(Vec<String>, Vec<&'a Value>)> {
    let mut out = Vec::new();
    let (Some(schema), Some(list)) = (
        op.success_output().and_then(|o| o.schema.as_ref()),
        list_member(body),
    ) else {
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

/// The one boolean `op`'s answer declares beside list answer `body`'s list, or in an
/// object member beside it (Zoho's `page_context.has_more_page`): the path of the object
/// holding it, and its name. With none, or several, which says more pages follow is not
/// known, and there is none.
fn flag(bundle: &mb_mcm::Bundle, op: &Operation, body: &Value) -> Option<(Vec<String>, String)> {
    let mut flags: Vec<Vec<String>> = Vec::new();
    for (path, parts) in beside_list(bundle, op, body) {
        if lists::types_of(&parts).into_iter().eq(["boolean"]) && !flags.contains(&path) {
            flags.push(path);
        }
    }
    let [one] = flags.as_slice() else {
        return None;
    };
    let (name, parents) = one.split_last()?;
    Some((parents.to_vec(), name.clone()))
}

/// `body` with the one integer member beside its list, or in an object beside it, named
/// `name` and holding `was` (Zoho's `page_context.page`, Box's `offset`) holding `now`;
/// `None` with none or several.
fn echoed(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    body: &Value,
    name: &str,
    (was, now): (usize, usize),
) -> Option<Value> {
    let found: Vec<Vec<String>> = beside_list(bundle, op, body)
        .into_iter()
        .filter(|(path, parts)| {
            path.last().is_some_and(|last| last == name)
                && lists::types_of(parts).into_iter().eq(["integer"])
                && path
                    .iter()
                    .try_fold(body, |at, name| at.get(name))
                    .and_then(whole)
                    == Some(was)
        })
        .map(|(path, _)| path)
        .collect();
    let [path] = found.as_slice() else {
        return None;
    };
    placed(body, path, Some(Value::from(now)))
}

/// The first page `body` of a list paged by number or offset, as it is served with
/// `rest` after it, and the run that keeps them (see the module's doc); `None` when
/// `call` asks for no first page, or no such page validates.
pub(super) fn first_page(
    rt: &MockRuntime,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    body: &Value,
    (size, size_param): (usize, &str),
    rest: Vec<Value>,
) -> Option<(Value, StoredResource)> {
    let bundle = rt.bundle();
    let op = plan.op;
    let (param, style) = position(bundle, op, size_param)?;
    if !starts(bundle, (op, call), size_param) {
        return None;
    }
    // A parameter that names a member of the items filters them.
    let schema = op.success_output()?.schema.as_ref()?;
    let mut held = None;
    let (_, item_parts) = lists::list_parts(bundle, schema, body, &mut held)?;
    if lists::named_member(bundle, &item_parts, &param).is_some() {
        return None;
    }
    let valid = |candidate: &Value| {
        plan.engine
            .serve_stored(bundle, op, candidate, None)
            .is_some()
    };
    // A count of the list that counts less than the page and the rest counts them.
    let known = list_items(body).map_or(0, Vec::len) + rest.len();
    let counted = deleted::with_count_at_least(rt, op, body, known);
    let more = flag(bundle, op, body);
    let page = counted
        .iter()
        .chain(std::iter::once(body))
        .find_map(|base| {
            // More to come, where the answer allows saying so.
            more.as_ref()
                .and_then(|(parents, flag)| with_more(base, parents, flag))
                .filter(|more| valid(more))
                .or_else(|| valid(base).then(|| base.clone()))
        })?;
    let query = bound_query(call, &param, size_param);
    let path = path_of(call);
    let id = key(op, &path, &query);
    let run = Run {
        op: op.id.clone(),
        path,
        query,
        param,
        style,
        size,
        size_param: size_param.to_string(),
        page: page.clone(),
        rest,
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
    let (param, style) = position(bundle, op, size_param)?;
    let at = whole(call.query.get(&param)?)?;
    let size = usize::try_from(limit.items).ok()?;
    let start = match style {
        Style::Number => at.checked_sub(1)?.checked_mul(size)?,
        Style::Offset => at,
    };
    if start == 0 {
        return None;
    }
    let path = path_of(call);
    let query = bound_query(call, &param, size_param);
    let found = state
        .get(&plan.session, INTERNAL_TYPE, &key(op, &path, &query))
        .await?;
    let run = Run::of(found.data)?;
    if run.op != op.id
        || run.path != path
        || run.query != query
        || run.param != param
        || run.style != style
        || run.size_param != size_param
    {
        return None;
    }
    let mut sequence = list_items(&run.page)?.clone();
    sequence.extend(run.rest);
    // Past the items the run knows, the page is the model's.
    if start >= sequence.len() {
        return None;
    }
    let end = start.saturating_add(size).min(sequence.len());
    let mut body = with_items(&run.page, sequence.get(start..end)?.to_vec())?;
    // What the session wrote since, and the request's filters, as on a first page.
    let t = op.resource_type.as_deref();
    lists::overlay_stored(state, (plan, call), t, &mut body).await;
    let _ = lists::narrow(rt, plan, call, &mut body);
    let valid = |candidate: &Value| plan.engine.serve_stored(bundle, op, candidate, None);
    // Where the answer says which page it is, and its size, it says this one's.
    for (name, was, now) in [
        (param.as_str(), style.first(), at),
        (size_param, run.size, size),
    ] {
        if was != now
            && let Some(said) = echoed(bundle, op, &body, name, (was, now))
            && valid(&said).is_some()
        {
            body = said;
        }
    }
    let mut served = if end < sequence.len() {
        valid(&body)?
    } else {
        // The last page: no more, as the first page said there was, where the answer
        // allows.
        flag(bundle, op, &body)
            .and_then(|(parents, flag)| without_more(&body, &run.page, &parents, &flag))
            .and_then(|done| valid(&done))
            .or_else(|| valid(&body))?
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
