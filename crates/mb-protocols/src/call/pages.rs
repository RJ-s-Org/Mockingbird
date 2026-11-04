//! The rest of a list answer cut to its page size, served by a token.
//!
//! [`lists::narrow`] cuts a generated list to the page size a request asks for. A
//! client then asks for the next page by the token the answer carried, which the model
//! made up: the request repeats the first's, or names a token no list knows, and the
//! answer is the first page again (from the cache) or a list made up anew. So when a cut
//! leaves items out, and the contract has a place for a token, the engine mints one.
//! A page the model answered full, at the size asked, with its token absent, `null` or
//! empty, while the session holds records the query asks for that it does not show, is
//! a cut too: those records are its rest ([`lists::stored_rest`]), after any the page
//! size cut. A token the model gave stays, with its count, as it leads to the model's own
//! next page. Such a cut is kept, capped and ended like any other (at most [`MAX_CUTS`]
//! a session). Its records are stored already, so they are not seeded again. A count of
//! the list beside the token that counts fewer than the page and its rest (see
//! [`deleted::with_count_at_least`]) is raised to them. A minted token's more flag
//! (see below) that is `false` is `true` where the answer validates so, as the last page
//! sets it back.
//!
//! The place is one text member of the answer (no `const` or `enum`), at its top level
//! or inside an object member beside the list, whose name is, or ends with the words
//! of, a text query parameter of the route other than the page size: Zoom's
//! `next_page_token` for `next_page_token`, Square's `cursor` for `cursor`, Pipedrive's
//! `additional_data.next_cursor` for `cursor`. A pair is a cursor only when the request
//! does not carry the parameter, or carries it empty (`cursor=`: a page is asked for
//! without one first), the parameter
//! names no member of the items (`status` filters them), and what the model put in the
//! member echoes no value of the request (a search's `query`). Exactly one pair, or none
//! is used; nor is a token written that leaves the answer invalid. With none, the
//! answer is as [`lists::narrow`] left it.
//!
//! A cut is kept once, as one document under the store's reserved type
//! ([`mb_state::INTERNAL_TYPE`]), like the deleted markers: the route, the path's
//! values, the request's other query values, the token's place and parameter, the page
//! size, the page as answered, and the items left out that no delete marker covers (at
//! most [`MAX_REST`]). They are seeded as a list's items are, so a read of one finds it.
//! A session keeps at most [`MAX_CUTS`] cuts: each goes in one of that many places,
//! picked by a counter this process shares across sessions, so nothing is read to count
//! them. A new cut ends whichever of the session's cuts held its place, which is not
//! always its oldest when other sessions mint meanwhile, and two nodes may pick the same
//! place: either only ends a cut early, whose token then goes to the model. The
//! session's TTL ends them all. A token is the cut's nonce, its place and the offset of the next item, in
//! [`TOKEN_LEN`] hex digits: the same token always serves the same page and the same
//! next token, and serving one writes nothing.
//!
//! A request on the same route and path, with the same other query values, carrying a
//! token of a kept cut under its parameter is served without a model call: the next
//! items, as many as the request's page size or the first's, each showing its stored
//! record (see [`lists::overlay_stored`]), then the query's filters
//! ([`lists::narrow`]) and the delete markers ([`deleted::drop_from_list`]) applied
//! again, as the session may have written since; and the next token when more remain.
//! The last page's token is `null`, left out or empty, whichever the answer validates
//! with; with none, the request is the model's. The more flag is the one boolean the
//! response schema declares in the object holding the token (`has_more`); with none, or
//! several (`has_more` beside `livemode`), there is none and no boolean changes. When
//! the first page set it `true`, it is `false` on the last page when the answer
//! validates so, and else as it was. A token with other filter values, of
//! a cut since ended, or of another route, is the model's too. Its query differs from
//! the first page's by the token, so the response cache never answers it with the
//! first page; it is not cached itself, as the token already serves it.
//!
//! A contract with no such member may have one for the next page's URL instead: one
//! text member with a URL's `format` (`uri`, `uri-reference`, `url`, `iri`,
//! `iri-reference`) or described as a URL or URI, beside the list or in objects beside
//! it, at most [`MAX_URL_DEPTH`] names deep, with `next` a word of a name on its path:
//! HAL's `_links.next.href`, a bare `next` beside `values`, JSON:API's `links.next`.
//! `self`, `prev` and `next` share a schema, so only that word tells them apart. Its
//! parameter is the one text query parameter [`slot`] could pair with a token (not
//! carried, no page size, naming no member of the items), or, of several, the one
//! sharing its first word with a page size of several words (`page[cursor]` beside
//! `page[size]`); two such, or two such members, are no place. The token is written as
//! the URL of its page: the route's path with the request's path values, the request's
//! query with the token under the parameter, at the origin of the URL the model wrote
//! there, else of one in the answer of the request's own path (its `self`), else of the
//! first in the answer, else [`FALLBACK_ORIGIN`]. A link object the model left `null`
//! is written as a copy of a link object beside it, one of the request's own path first
//! (HAL's `self`, for its `type`). A URL the model gave carrying the parameter is its
//! token. A request following the URL carries the token under the parameter and the
//! request's other values, so it is served as above; the last page's link object is
//! `null` or left out, or else the URL member, where the answer allows (never empty).
//!
//! A list paged by number or offset has no place for a token: its first page is kept
//! as a run instead, and its later pages served from it ([`runs`]).
//!
//! An LLM and `consistency: on` only, as [`deleted::applies`] says, and plain calls
//! (no scenario or forced status).

use std::sync::atomic::{AtomicU64, Ordering};

use mb_engine::consistency::{list_items_mut, list_member};
use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::runs;
use super::*;

/// Cuts kept per session, the newest.
pub(super) const MAX_CUTS: u64 = 64;
/// Items left out that a cut keeps, at most.
const MAX_REST: usize = 500;
/// Hex digits of a token: the nonce's, the place's and the offset's.
const TOKEN_LEN: usize = 40;
const NONCE_LEN: usize = 28;
const PLACE_LEN: usize = 4;
/// The prefix of a cut's document.
const PREFIX: &str = "#cut/";

/// Cuts minted by this process: which place each takes, and what makes each nonce
/// unlike the last.
static MINTED: AtomicU64 = AtomicU64::new(0);

/// Where an answer of `op` carries its token: the member's path from the top, and the
/// query parameter the token comes back under.
pub(super) struct Slot {
    pub(super) path: Vec<String>,
    param: String,
    /// For a member holding the next page's URL, not the token itself, the origin the
    /// URL is written with: the token goes in its query, under `param`.
    url: Option<String>,
}

impl Slot {
    /// The token the model gave at this place of `body`: the text there, or, for a URL,
    /// what its query holds under the parameter. `None` when absent or empty.
    pub(super) fn given(&self, body: &Value) -> Option<String> {
        let text = self
            .path
            .iter()
            .try_fold(body, |at, name| at.get(name))
            .and_then(Value::as_str)?;
        let token = match self.url {
            Some(_) => query_value(text, &self.param)?,
            None => text.to_string(),
        };
        (!token.trim().is_empty()).then_some(token)
    }
}

/// Whether member `name` is, or ends with the words of, parameter `param`.
fn names_param(name: &str, param: &str) -> bool {
    mb_mcm::ends_with_words(name, param)
}

/// The origin a next page's URL is written with when the answer shows none, as the
/// faker writes a `uri`.
const FALLBACK_ORIGIN: &str = "https://example.com";

/// The `format`s of a member holding a URL.
const URL_FORMATS: [&str; 5] = ["uri", "uri-reference", "url", "iri", "iri-reference"];

/// Names deep a URL member is looked for, the list's envelope aside: HAL's
/// `_links.next.href` is three.
const MAX_URL_DEPTH: usize = 3;

/// The value `url`'s query holds under `param`; a relative URL is read as one.
fn query_value(url: &str, param: &str) -> Option<String> {
    let parsed = url::Url::parse(url)
        .or_else(|_| url::Url::parse(FALLBACK_ORIGIN).and_then(|base| base.join(url)))
        .ok()?;
    parsed
        .query_pairs()
        .find(|(name, _)| name == param)
        .map(|(_, value)| value.into_owned())
}

/// `text` when it is an absolute `http` or `https` URL.
fn absolute(text: &str) -> Option<url::Url> {
    let url = url::Url::parse(text).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then_some(url)
}

/// The absolute URLs `value` holds (see [`absolute`]), depth first, but in member
/// `skip` of the top.
fn urls_in(value: &Value, skip: Option<&str>, depth: usize, out: &mut Vec<url::Url>) {
    if depth > 8 {
        return;
    }
    match value {
        Value::String(text) => out.extend(absolute(text)),
        Value::Array(items) => {
            for item in items {
                urls_in(item, None, depth + 1, out);
            }
        }
        Value::Object(members) => {
            for (_, member) in members
                .iter()
                .filter(|(name, _)| Some(name.as_str()) != skip)
            {
                urls_in(member, None, depth + 1, out);
            }
        }
        _ => {}
    }
}

/// `op`'s path with the values of `call`; `None` for a route of another protocol.
fn concrete_path(op: &Operation, call: &OperationCall) -> Option<String> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    Some(
        call.path_params
            .iter()
            .fold(path.clone(), |path, (name, value)| {
                let text = match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                path.replace(&format!("{{{name}}}"), &text)
            }),
    )
}

/// Whether `schema` holds a URL: free text (see [`is_free_text`]) whose `format` is a
/// URL's, or that its description calls a URL or URI.
fn is_url(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
    if !is_free_text(bundle, schema) {
        return false;
    }
    let (parts, _) = schema_parts(bundle, Some(schema));
    parts.iter().any(|part| {
        part.get("format")
            .and_then(Value::as_str)
            .is_some_and(|format| URL_FORMATS.contains(&format))
            || part
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|text| {
                    text.split(|c: char| !c.is_alphanumeric()).any(|word| {
                        ["url", "uri", "urls", "uris"].contains(&word.to_ascii_lowercase().as_str())
                    })
                })
    })
}

/// The paths of the URL members (see [`is_url`]) of object `schema`, at most
/// [`MAX_URL_DEPTH`] names deep, leaving out member `skip` of the top (the list).
fn url_members(
    bundle: &mb_mcm::Bundle,
    schema: &Value,
    skip: &str,
    path: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    let (parts, _) = schema_parts(bundle, Some(schema));
    for (name, member) in parts
        .iter()
        .filter_map(|p| p.get("properties").and_then(Value::as_object))
        .flatten()
    {
        if path.is_empty() && name == skip {
            continue;
        }
        path.push(name.clone());
        if is_url(bundle, member) {
            if !out.contains(path) {
                out.push(path.clone());
            }
        } else if path.len() < MAX_URL_DEPTH {
            url_members(bundle, member, skip, path, out);
        }
        path.pop();
    }
}

/// The cursor a URL of the next page carries: the one parameter of `params` (those
/// [`slot`] could pair with a token), else the one of them that shares its first word
/// with a page size of several words (`page[cursor]` beside `page[size]`, where
/// `filter` is another). Two such, or none, is no cursor.
fn cursor_param<'a>(params: &[&'a str], size_param: &str) -> Option<&'a str> {
    if let [one] = params {
        return Some(one);
    }
    let size_words = mb_mcm::words(size_param);
    let [lead, _, ..] = size_words.as_slice() else {
        return None;
    };
    let lead = mb_mcm::joined_name(lead);
    let mut sharing = params.iter().filter(|param| {
        let words = mb_mcm::words(param);
        words.len() > 1 && mb_mcm::joined_name(words[0]) == lead
    });
    let first = sharing.next()?;
    sharing.next().is_none().then_some(*first)
}

/// The one place answer `body` (of schema `schema`, its list `list`) has for the URL
/// of its next page, when it has none for a token: a URL member (see [`is_url`])
/// beside the list or in objects beside it, at most [`MAX_URL_DEPTH`] names deep,
/// with `next` a word of a name on its path (`self`, `prev` and `next` share one
/// schema: only the name tells them apart), and a cursor for it among `params` (see
/// [`cursor_param`]). The origin is that of the model's URL there, else of the first
/// URL of the request's own path the answer shows outside the list (its `self`), else
/// of the first URL there, else [`FALLBACK_ORIGIN`].
fn url_slot(
    bundle: &mb_mcm::Bundle,
    (op, call): (&Operation, &OperationCall),
    (schema, list): (&Value, &str),
    body: &Value,
    (params, size_param): (&[&str], &str),
) -> Option<Slot> {
    let param = cursor_param(params, size_param)?;
    let mut members = Vec::new();
    url_members(bundle, schema, list, &mut Vec::new(), &mut members);
    let mut next = members.into_iter().filter(|path| {
        path.iter().any(|name| {
            mb_mcm::words(name)
                .iter()
                .any(|word| word.eq_ignore_ascii_case("next"))
        })
    });
    let path = next.next()?;
    if next.next().is_some() {
        return None;
    }
    let own = concrete_path(op, call)?;
    let given = path
        .iter()
        .try_fold(body, |at, name| at.get(name))
        .and_then(Value::as_str)
        .and_then(absolute);
    let mut shown = Vec::new();
    urls_in(body, Some(list), 0, &mut shown);
    let at_own = shown.iter().position(|url| url.path() == own);
    let origin = given
        .or_else(|| at_own.map(|at| shown.swap_remove(at)))
        .or_else(|| shown.into_iter().next())
        .map_or_else(
            || FALLBACK_ORIGIN.to_string(),
            |url| url.origin().ascii_serialization(),
        );
    Some(Slot {
        path,
        param: param.to_string(),
        url: Some(origin),
    })
}

/// The URL of the page of `call` that carries `token` under `param`: `origin`, `op`'s
/// path with the call's values, and the call's query with `token` for what it held
/// under `param`. `None` for a route of another protocol, or a query value that is an
/// object.
fn page_url(
    origin: &str,
    (op, call): (&Operation, &OperationCall),
    param: &str,
    token: &str,
) -> Option<String> {
    let concrete = concrete_path(op, call)?;
    let mut url = url::Url::parse(origin).ok()?;
    url.set_path(&concrete);
    let mut pairs: Vec<(&str, String)> = Vec::new();
    for (name, value) in call.query.iter().filter(|(name, _)| name.as_str() != param) {
        let values = match value {
            Value::Array(values) => values.iter().collect(),
            other => vec![other],
        };
        for value in values {
            match value {
                Value::Null => {}
                Value::Object(_) => return None,
                Value::String(text) => pairs.push((name, text.clone())),
                other => pairs.push((name, other.to_string())),
            }
        }
    }
    pairs.push((param, token.to_string()));
    url.query_pairs_mut()
        .clear()
        .extend_pairs(pairs.iter().map(|(name, value)| (*name, value.as_str())));
    Some(url.into())
}

/// The answers `body` to `call` may be with `token` at `slot`, in the order tried: the
/// token there, or, for a URL, the page's URL (see [`page_url`]) there, else in a copy
/// of an object beside the one holding it that holds the same member (HAL's
/// `_links.self` for a `_links.next` that is `null`, as a link has other members), one
/// whose URL is of the request's own path first.
fn filled(
    body: &Value,
    slot: &Slot,
    (op, call): (&Operation, &OperationCall),
    token: &str,
) -> Vec<Value> {
    let Some(origin) = slot.url.as_deref() else {
        return placed(body, &slot.path, Some(Value::String(token.to_string())))
            .into_iter()
            .collect();
    };
    let Some(url) = page_url(origin, (op, call), &slot.param, token) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = placed(body, &slot.path, Some(Value::String(url.clone())))
        .into_iter()
        .collect();
    if let Some((leaf, link)) = slot.path.split_last()
        && let Some((own, holder)) = link.split_last()
        && !holder.is_empty()
    {
        let siblings = holder
            .iter()
            .try_fold(body, |at, name| at.get(name))
            .and_then(Value::as_object);
        let path = concrete_path(op, call);
        let mut siblings: Vec<(&String, &Value)> = siblings
            .into_iter()
            .flatten()
            .filter(|(name, sibling)| {
                *name != own && sibling.get(leaf).is_some_and(Value::is_string)
            })
            .collect();
        // Of the request's own path first: `self`, not `documentation`.
        siblings.sort_by_key(|(_, sibling)| {
            let at = sibling.get(leaf).and_then(Value::as_str).and_then(absolute);
            at.map(|url| url.path().to_string()) != path
        });
        for (_, sibling) in siblings {
            let mut copy = sibling.clone();
            if let Some(members) = copy.as_object_mut() {
                members.insert(leaf.clone(), Value::String(url.clone()));
            }
            out.extend(placed(body, link, Some(copy)));
        }
    }
    out
}

/// The ends `body` may have at `slot` on the last page, in the order tried: `null`,
/// nothing or empty text there. For a URL, no empty text, and when it sits in a link
/// object of its own (HAL's `_links.next.href`), that object `null` or left out first.
fn ended(body: &Value, slot: &Slot) -> Vec<Value> {
    let mut out = Vec::new();
    if slot.url.is_some() && slot.path.len() > 2 {
        let link = &slot.path[..slot.path.len() - 1];
        out.extend(
            [Some(Value::Null), None]
                .into_iter()
                .filter_map(|end| placed(body, link, end)),
        );
    }
    let empty = slot
        .url
        .is_none()
        .then(|| Some(Value::String(String::new())));
    out.extend(
        [Some(Value::Null), None]
            .into_iter()
            .chain(empty)
            .filter_map(|end| placed(body, &slot.path, end)),
    );
    out
}

/// Whether `schema` is text only, with no `const` or `enum`.
fn is_free_text(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
    let (parts, _) = schema_parts(bundle, Some(schema));
    lists::types_of(&parts).into_iter().eq(["string"])
        && !parts
            .iter()
            .any(|p| p.get("const").is_some() || p.get("enum").is_some())
}

/// The text values `call` holds in its path and query, empty ones aside.
fn request_values(call: &OperationCall) -> Vec<String> {
    let mut out = Vec::new();
    for value in call.path_params.values().chain(call.query.values()) {
        match value {
            Value::Array(values) => out.extend(values.iter().filter_map(id_string)),
            other => out.extend(id_string(other)),
        }
    }
    out.retain(|value| !value.trim().is_empty());
    out
}

/// Whether `call` carries query parameter `name` with a value: `cursor=` carries none.
fn carries(call: &OperationCall, name: &str) -> bool {
    call.query.get(name).is_some_and(|value| match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        _ => true,
    })
}

/// The booleans the response schema of `op` declares in the object at `parents`, each
/// once.
pub(super) fn booleans_at(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    parents: &[String],
) -> Vec<String> {
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return Vec::new();
    };
    let (mut parts, _) = schema_parts(bundle, Some(schema));
    for name in parents {
        let Some(member) = parts.iter().find_map(|p| p.get("properties")?.get(name)) else {
            return Vec::new();
        };
        parts = schema_parts(bundle, Some(member)).0;
    }
    let mut flags: Vec<String> = Vec::new();
    for (name, member) in parts
        .iter()
        .filter_map(|p| p.get("properties").and_then(Value::as_object))
        .flatten()
    {
        let (member_parts, _) = schema_parts(bundle, Some(member));
        if lists::types_of(&member_parts).into_iter().eq(["boolean"]) && !flags.contains(name) {
            flags.push(name.clone());
        }
    }
    flags
}

/// The member of `op`'s answer that says whether more pages follow the token at `slot`:
/// the one boolean the response schema declares in the object holding the slot
/// (`has_more`). With none, or several (`has_more` beside `livemode`), which says so is
/// not known, and there is none.
fn more_flag(bundle: &mb_mcm::Bundle, op: &Operation, slot: &[String]) -> Option<String> {
    let (_, parents) = slot.split_last()?;
    match booleans_at(bundle, op, parents).as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// `body` with boolean `flag` of the object at `parents` set to `value` where it holds
/// `!value`; `None` when it does not.
fn flag_set(body: &Value, parents: &[String], flag: &str, value: bool) -> Option<Value> {
    let mut out = body.clone();
    let mut at = &mut out;
    for name in parents {
        at = at.get_mut(name)?;
    }
    let held = at.as_object_mut()?.get_mut(flag)?;
    if held.as_bool() != Some(!value) {
        return None;
    }
    *held = Value::Bool(value);
    Some(out)
}

/// `body` with the [`more_flag`] `flag` of the object at `parents` set to `false` when
/// `first` holds it `true`: a `has_more` the first page set, on the last.
pub(super) fn without_more(
    body: &Value,
    first: &Value,
    parents: &[String],
    flag: &str,
) -> Option<Value> {
    let said = parents
        .iter()
        .try_fold(first, |at, name| at.get(name))
        .and_then(|object| object.get(flag))
        .and_then(Value::as_bool);
    if said != Some(true) {
        return None;
    }
    flag_set(body, parents, flag, false)
}

/// `body` with the [`more_flag`] `flag` of the object at `parents` set to `true` where it
/// is `false`: a `has_more` beside a minted token, as [`without_more`] ends it on the
/// last page.
pub(super) fn with_more(body: &Value, parents: &[String], flag: &str) -> Option<Value> {
    flag_set(body, parents, flag, true)
}

/// The parents of a token at `slot`: the object holding it.
fn parents_of(slot: &[String]) -> &[String] {
    slot.split_last().map_or(&[], |(_, parents)| parents)
}

/// The one place answer `body` to `call` has for a token (see the module's doc).
pub(super) fn slot(
    bundle: &mb_mcm::Bundle,
    (op, call): (&Operation, &OperationCall),
    body: &Value,
    size_param: &str,
) -> Option<Slot> {
    let list = list_member(body)?;
    let schema = op.success_output()?.schema.as_ref()?;
    let mut held = None;
    let (_, item_parts) = lists::list_parts(bundle, schema, body, &mut held)?;
    let declared = op.input.query.as_ref()?.get("properties")?.as_object()?;
    let params: Vec<&str> = declared
        .iter()
        .filter(|(name, schema)| {
            name.as_str() != size_param
                && !mb_mcm::is_generic_id(name)
                && !carries(call, name)
                && lists::named_member(bundle, &item_parts, name).is_none()
                && is_free_text(bundle, schema)
        })
        .map(|(name, _)| name.as_str())
        .collect();
    let echoed = request_values(call);
    let param_of = |name: &str, generated: Option<&Value>| -> Option<String> {
        if generated
            .and_then(id_string)
            .is_some_and(|value| echoed.contains(&value))
        {
            return None;
        }
        let mut named = params.iter().filter(|param| names_param(name, param));
        let first = named.next()?;
        named.next().is_none().then(|| (*first).to_string())
    };
    let (parts, _) = schema_parts(bundle, Some(schema));
    let mut found: Vec<Slot> = Vec::new();
    for part in &parts {
        let Some(members) = part.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for (name, member) in members.iter().filter(|(name, _)| name.as_str() != list) {
            if is_free_text(bundle, member) {
                if let Some(param) = param_of(name, body.get(name)) {
                    found.push(Slot {
                        path: vec![name.clone()],
                        param,
                        url: None,
                    });
                }
                continue;
            }
            let (inner, _) = schema_parts(bundle, Some(member));
            for (inner_name, inner_member) in inner
                .iter()
                .filter_map(|p| p.get("properties").and_then(Value::as_object))
                .flatten()
            {
                let generated = body.get(name).and_then(|outer| outer.get(inner_name));
                if is_free_text(bundle, inner_member)
                    && let Some(param) = param_of(inner_name, generated)
                {
                    found.push(Slot {
                        path: vec![name.clone(), inner_name.clone()],
                        param,
                        url: None,
                    });
                }
            }
        }
    }
    match found.pop() {
        Some(first) => found.is_empty().then_some(first),
        // No place for the token itself: a place for the next page's URL, if any.
        None => url_slot(
            bundle,
            (op, call),
            (schema, list),
            body,
            (&params, size_param),
        ),
    }
}

/// `body` with `value` at `path`, an object made for it where its parent is missing or
/// `null`; `None` for no value, which takes the member out.
pub(super) fn placed(body: &Value, path: &[String], value: Option<Value>) -> Option<Value> {
    let mut out = body.clone();
    let (last, parents) = path.split_last()?;
    let mut at = &mut out;
    for name in parents {
        let object = at.as_object_mut()?;
        let slot = object
            .entry(name.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if slot.is_null() {
            *slot = Value::Object(Map::new());
        }
        at = slot;
    }
    let object = at.as_object_mut()?;
    match value {
        Some(value) => {
            object.insert(last.clone(), value);
        }
        None => {
            object.remove(last);
        }
    }
    Some(out)
}

/// A nonce not minted before, and the place its cut takes.
fn new_cut(session: &SessionKey, op: &Operation) -> (String, u64) {
    let count = MINTED.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hasher = Sha256::new();
    for part in [
        session.workspace.as_str(),
        session.mock.as_str(),
        session.session.as_str(),
        op.id.as_str(),
        &count.to_string(),
        &nanos.to_string(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    (hex[..NONCE_LEN].to_string(), count % MAX_CUTS)
}

/// The token of the item at `offset` of the cut `nonce` names in `place`.
fn token(nonce: &str, place: u64, offset: usize) -> String {
    format!("{nonce}{place:04x}{offset:08x}")
}

/// A token's nonce, place and offset, when `text` is one.
fn read_token(text: &str) -> Option<(&str, u64, usize)> {
    if text.len() != TOKEN_LEN || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let (nonce, rest) = text.split_at(NONCE_LEN);
    let (place, offset) = rest.split_at(PLACE_LEN);
    let place = u64::from_str_radix(place, 16).ok()?;
    let offset = usize::from_str_radix(offset, 16).ok()?;
    (place < MAX_CUTS).then_some((nonce, place, offset))
}

fn place_key(place: u64) -> String {
    format!("{PREFIX}{place:04x}")
}

/// The query values a token is bound to: the request's, but the token's and the page
/// size's.
pub(super) fn bound_query(call: &OperationCall, param: &str, size_param: &str) -> Value {
    let mut query = call.query.clone();
    query.remove(param);
    query.remove(size_param);
    canonical(&Value::Object(query))
}

/// A kept cut.
struct Cut {
    nonce: String,
    op: String,
    path: Value,
    query: Value,
    slot: Slot,
    size: usize,
    size_param: String,
    /// The page as answered, with its token.
    page: Value,
    rest: Vec<Value>,
}

impl Cut {
    fn to_value(&self) -> Value {
        serde_json::json!({
            "nonce": self.nonce,
            "op": self.op,
            "path": self.path,
            "query": self.query,
            "slot": self.slot.path,
            "param": self.slot.param,
            "url": self.slot.url,
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
            nonce: text(&value, "nonce")?,
            op: text(&value, "op")?,
            path: value.get("path")?.clone(),
            query: value.get("query")?.clone(),
            slot: Slot {
                path: value
                    .get("slot")?
                    .as_array()?
                    .iter()
                    .map(|s| s.as_str().map(str::to_string))
                    .collect::<Option<_>>()?,
                param: text(&value, "param")?,
                url: text(&value, "url"),
            },
            size: usize::try_from(value.get("size")?.as_u64()?).ok()?,
            size_param: text(&value, "size_param")?,
            page: value.get_mut("page")?.take(),
            rest,
        })
    }
}

/// The path's values of `call`, as a cut keeps them.
pub(super) fn path_of(call: &OperationCall) -> Value {
    canonical(&Value::Object(call.path_params.clone()))
}

/// `body` with its list's items `items`.
pub(super) fn with_items(body: &Value, items: Vec<Value>) -> Option<Value> {
    let mut out = body.clone();
    *list_items_mut(&mut out)? = items;
    Some(out)
}

/// Writes a token for what `cut` left out of `generated`'s answer, when the answer has
/// a place for one, keeps the cut and seeds the items left out (see the module's doc).
/// With no place, a first page of a list paged by number or offset keeps its run
/// instead ([`runs`]). Items of type `t`. Whether anything was kept.
pub(super) async fn mint(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: Option<&str>,
    cut: lists::Cut,
    generated: &mut GeneratedResponse,
) -> bool {
    let rt = state.rt;
    let op = plan.op;
    let session = &plan.session;
    let Some(body) = generated.body.as_ref() else {
        return false;
    };
    let slot = slot(rt.bundle(), (op, call), body, &cut.size_param);
    if slot.is_none() && !runs::starts(rt.bundle(), (op, call), &cut.size_param) {
        return false;
    }
    // What a delete marker covers is no later page's.
    let mut rest = Vec::with_capacity(cut.rest.len().min(MAX_REST));
    let mut truncated = cut.truncated;
    // The records the store gave are stored already, as they stand.
    let generated_rest = cut.rest.len().saturating_sub(cut.stored);
    let mut seeded = Vec::new();
    let field = item_identity(rt.bundle(), op, body);
    let scope = deleted::scope(op, call);
    for (at, item) in cut.rest.into_iter().enumerate() {
        if rest.len() >= MAX_REST {
            truncated = true;
            break;
        }
        let gone = match (t, item.get(&field).and_then(id_string)) {
            (Some(t), Some(id)) if state.tracks_deleted => {
                deleted::is_deleted(state, session, t, scope.as_deref(), &id).await
            }
            _ => false,
        };
        if !gone {
            if at < generated_rest {
                seeded.push(item.clone());
            }
            rest.push(item);
        }
    }
    if rest.is_empty() {
        return false;
    }
    let kept = match slot {
        Some(slot) => tokened(rt, plan, call, body, slot, (cut.size, cut.size_param), rest),
        None => runs::first_page(
            rt,
            (plan, call),
            body,
            (cut.size, &cut.size_param),
            (rest, truncated, generated_rest > 0),
        ),
    };
    let Some((page, document)) = kept else {
        return false;
    };
    // The items left out are the session's, as a list's items are.
    if let Some(t) = t
        && !seeded.is_empty()
        && let Some(seeded) = with_items(&page, seeded)
    {
        let seeding = GeneratedResponse {
            body: Some(seeded),
            ..generated.clone()
        };
        write_state(
            state,
            WriteRequest {
                session,
                op,
                resource_type: t,
                target_id: None,
                generated: &seeding,
                request_body: None,
                collection: collection_of(op, call),
                origin: WriteOrigin::Seeded,
                held: false,
            },
        )
        .await;
    }
    // A cut not kept serves no token: the answer keeps the model's.
    let kept = state.upsert(session, document).await;
    if kept {
        generated.body = Some(page);
    }
    kept
}

/// The first page `body` with a token at `slot` for `rest`, and the cut that serves it;
/// `None` when no such page validates.
fn tokened(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    body: &Value,
    slot: Slot,
    (size, size_param): (usize, String),
    rest: Vec<Value>,
) -> Option<(Value, StoredResource)> {
    let op = plan.op;
    let (nonce, place) = new_cut(&plan.session, op);
    let first = token(&nonce, place, 0);
    let valid = |candidate: &Value| {
        plan.engine
            .serve_stored(rt.bundle(), op, candidate, None)
            .is_some()
    };
    // A count of the list that counts less than the page and the rest counts them.
    let known = list_items(body).map_or(0, Vec::len) + rest.len();
    let counted = deleted::with_count_at_least(rt, op, body, known);
    let flag = more_flag(rt.bundle(), op, &slot.path);
    let page = counted
        .iter()
        .chain(std::iter::once(body))
        .flat_map(|base| filled(base, &slot, (op, call), &first))
        .find_map(|page| {
            // More to come, where the answer allows saying so.
            flag.as_deref()
                .and_then(|flag| with_more(&page, parents_of(&slot.path), flag))
                .filter(|more| valid(more))
                .or_else(|| valid(&page).then_some(page))
        })?;
    let kept = Cut {
        nonce,
        op: op.id.clone(),
        path: path_of(call),
        query: bound_query(call, &slot.param, &size_param),
        slot,
        size,
        size_param,
        page: page.clone(),
        rest,
    };
    let document = StoredResource {
        resource_type: INTERNAL_TYPE.to_string(),
        id: place_key(place),
        data: kept.to_value(),
    };
    Some((page, document))
}

/// The next page a token `call` carries serves, with no model call (see the module's
/// doc); `None` when it carries none of a kept cut of this route, path and query.
pub(super) async fn serve(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Option<GeneratedResponse> {
    let rt = state.rt;
    let op = plan.op;
    let session = &plan.session;
    if !matches!(op.kind, OperationKind::Query) || !deleted::applies(rt, plan) || !plain(plan) {
        return None;
    }
    let mut found = None;
    for (param, value) in &call.query {
        let Some((nonce, place, offset)) = value.as_str().map(str::trim).and_then(read_token)
        else {
            continue;
        };
        let Some(doc) = state.get(session, INTERNAL_TYPE, &place_key(place)).await else {
            continue;
        };
        if let Some(cut) = Cut::of(doc.data)
            && cut.nonce == nonce
            && cut.op == op.id
            && cut.slot.param == *param
            && cut.path == path_of(call)
            && cut.query == bound_query(call, &cut.slot.param, &cut.size_param)
            && offset <= cut.rest.len()
        {
            found = Some((cut, place, offset));
            break;
        }
    }
    let Some((cut, place, offset)) = found else {
        // No token: a later page of a list paged by number or offset, if any.
        return runs::serve(state, plan, call).await;
    };
    let size = call
        .query
        .get(&cut.size_param)
        .and_then(|v| match v {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        })
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .unwrap_or(cut.size);
    let next = offset.saturating_add(size).min(cut.rest.len());
    let items = cut.rest.get(offset..next)?.to_vec();
    let mut body = with_items(&cut.page, items)?;
    // What the session wrote since, and the request's filters, as on a first page.
    let t = op.resource_type.as_deref();
    lists::overlay_stored(state, (plan, call), t, &mut body).await;
    let _ = lists::narrow(rt, plan, call, &mut body);
    let valid = |candidate: &Value| plan.engine.serve_stored(rt.bundle(), op, candidate, None);
    let mut served = if next < cut.rest.len() {
        let token = token(&cut.nonce, place, next);
        filled(&body, &cut.slot, (op, call), &token)
            .iter()
            .find_map(valid)?
    } else {
        // The last page: no token, as the answer allows.
        let flag = more_flag(rt.bundle(), op, &cut.slot.path);
        let end = ended(&body, &cut.slot).into_iter().find_map(|candidate| {
            // No more, as the first page said there was, where the answer allows.
            flag.as_deref()
                .and_then(|flag| {
                    without_more(&candidate, &cut.page, parents_of(&cut.slot.path), flag)
                })
                .and_then(|done| valid(&done))
                .or_else(|| valid(&candidate))
        });
        if end.is_none() {
            tracing::debug!(
                mock = %rt.name,
                operation = %op.id,
                "no end of a page token validates; the last page is generated"
            );
        }
        end?
    };
    if let Some(t) = t
        && state.tracks_deleted
    {
        let scope = deleted::scope(op, call);
        deleted::drop_from_list(
            state,
            session,
            (plan, call),
            t,
            scope.as_deref(),
            &mut served,
        )
        .await;
    }
    Some(served)
}
