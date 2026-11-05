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
//! size cut. So is such a page whose token the model gave: the stored records come
//! before the model's own next page: a minted token takes the place of the model's,
//! which is kept with the cut, and the last page of the rest carries it, where the
//! answer validates so, leading on to the model's own pages. The model's page for that
//! token is generated as any, but the items the rest served are taken out of it (where
//! it still validates), its token stays as the model gave it, and the store is not read
//! for a rest of its own ([`Followed`]); a page left with no item ends the list
//! instead (no token, its more flag `false` where it has one). The count stays, raised
//! only as below. A cut the page size made of the model's own items alone writes its
//! token over the model's, which the last page does not carry; a page that is no cut
//! keeps the model's token as it is. Such a cut is kept, capped and ended like any other (at most [`MAX_CUTS`]
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
//! A contract with no such member may have one for the next page's URL instead, on an
//! HTTP route of a list not paged by number or offset ([`runs::starts`]): one text
//! member with a URL's `format` (`uri`, `uri-reference`, `url`, `iri`, `iri-reference`)
//! or described as a URL or URI, beside the list or in objects beside it, at most
//! [`MAX_URL_DEPTH`] names deep, with a name on its path that is `next` or whose first
//! word is (after any dotted prefix): HAL's `_links.next.href`, a bare `next` beside
//! `values`, JSON:API's `links.next`, `next_page_url`, `@odata.nextLink`. `self`, `prev`
//! and `next` share a schema, so only the link relation's name tells them apart. Its
//! parameter is one [`slot`] could pair with a token (not carried, no page size, naming
//! no member of the items) that no `format` or `pattern` shapes (`since`, a date-time),
//! found by, in this order: a link of this very request the model wrote outside the list
//! (of the route, showing every value of the request's query: its `self`) carrying it
//! empty, as a first page's cursor is (a filter it carries with a value, `status=open`,
//! is none), the one of those, or of several the one below; the one parameter of several
//! words sharing its first word with a page size of several words (`page[cursor]` beside
//! `page[size]`, not a bare `page`); the one whose description names the `next` member
//! as code (`` `next` ``, `` `_links.next` ``), or that the `next` member's own
//! description (not a shared link schema's) names as code or as a query key (`from=`).
//! Prose never ties one (`q`, "sent again with the next request"). None, two, or two
//! such members, are no place. A URL the model wrote there to another page of the route by anything but the
//! cursor (`page=2`) stays as it is: there is no place. One carrying the cursor is its
//! token, and stays too when no stored records are left for a next page: a cut the page
//! size made of the model's own items leaves it. A cut holding stored records takes its
//! place, and its last page carries the model's URL, as for a token. The token is written as the URL of its page:
//! the mock's own, as the request reached it ([`CallPlan::page_url`]: the scheme and
//! host [`crate::base_url::of`] gives, the mount prefix, and the request's path as sent,
//! its values encoded as they came), never a host the model wrote, with the request's
//! query and the token under the parameter. A link object the model left `null` is
//! written as a copy of a link object beside it, one of the request's own route first
//! (HAL's `self`, for its `type`). A request following the URL carries the token under
//! the parameter and the request's other values, so it is served as above; the last
//! page's link object is `null` or left out, or else the URL member, where the answer
//! allows (never empty).
//!
//! A list paged by number or offset has no place for a token: its first page is kept
//! as a run instead, and its later pages served from it ([`runs`]).
//!
//! A list may carry its cursor in headers instead, as Heroku's does: a [`HeaderSlot`].
//! The request's `Range` header names the member the list is ordered by and the values
//! that bound it, and a `206 Partial Content` answer's `Next-Range` header is the
//! `Range` of the following page. Nothing is kept for it: the cursor is the last value
//! served, and [`ranges`] orders and cuts every such page from what the session holds.
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
    /// For a member holding the next page's URL, not the token itself, the URL the
    /// first page's call reached ([`CallPlan::page_url`]): the next page's is it with the
    /// call's query and the token under `param`.
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
        let token = match self.url.as_deref() {
            Some(base) => query_value(text, &url::Url::parse(base).ok()?, &self.param)?,
            None => text.to_string(),
        };
        (!token.trim().is_empty()).then_some(token)
    }
}

/// Where a list read carries its cursor in headers (the `Range` convention, as Heroku
/// pages its lists): it declares a `Range` request header, and a `206` answer with a
/// schema and a `Next-Range` (or `Content-Range`) response header, beside another 2xx
/// answer with the same schema, which holds every remaining item. Only an HTTP read.
pub(super) struct HeaderSlot {
    /// The request header, by its declared name.
    pub(super) header: String,
    /// The `206`'s `Next-Range` header, by its declared name; `Next-Range` when it
    /// declares only `Content-Range`. Served by the HTTP handler only.
    #[cfg_attr(not(feature = "http-rest"), allow(dead_code))]
    pub(super) next: String,
    /// The status of a page more items follow: `206`.
    pub(super) partial: u16,
    /// The status of a page holding every remaining item (`200`).
    pub(super) full: u16,
}

/// The [`HeaderSlot`] of `op`, when it has one.
pub(super) fn header_slot(op: &Operation) -> Option<HeaderSlot> {
    if !matches!(op.kind, OperationKind::Query) || !matches!(op.binding, Binding::Http { .. }) {
        return None;
    }
    let Some(Value::Object(declared)) = op.input.headers.as_ref().and_then(|s| s.get("properties"))
    else {
        return None;
    };
    let header = declared
        .keys()
        .find(|name| name.eq_ignore_ascii_case("range"))?
        .clone();
    let partial = op.outputs.iter().find(|variant| {
        variant.status == mb_mcm::StatusSelector::Code(206) && variant.schema.is_some()
    })?;
    let named = |wanted: &str| {
        partial
            .headers
            .keys()
            .find(|name| name.eq_ignore_ascii_case(wanted))
            .cloned()
    };
    let next = match named("next-range") {
        Some(name) => name,
        None => {
            named("content-range")?;
            "Next-Range".to_string()
        }
    };
    let full = op
        .outputs
        .iter()
        .filter_map(|variant| match variant.status {
            mb_mcm::StatusSelector::Code(code)
                if (200..300).contains(&code)
                    && code != 206
                    && variant.schema == partial.schema =>
            {
                Some(code)
            }
            _ => None,
        })
        .min()?;
    Some(HeaderSlot {
        header,
        next,
        partial: 206,
        full,
    })
}

/// Whether member `name` is, or ends with the words of, parameter `param`.
fn names_param(name: &str, param: &str) -> bool {
    mb_mcm::ends_with_words(name, param)
}

/// The `format`s of a member holding a URL.
const URL_FORMATS: [&str; 5] = ["uri", "uri-reference", "url", "iri", "iri-reference"];

/// Names deep a URL member is looked for, the list's envelope aside: HAL's
/// `_links.next.href` is three.
const MAX_URL_DEPTH: usize = 3;

/// `text` as a URL: an absolute one, else one relative to `base`.
fn parse_url(text: &str, base: &url::Url) -> Option<url::Url> {
    url::Url::parse(text).or_else(|_| base.join(text)).ok()
}

/// The value `url`'s query holds under `param`, a relative URL read against `base`.
fn query_value(url: &str, base: &url::Url, param: &str) -> Option<String> {
    parse_url(url, base)?
        .query_pairs()
        .find(|(name, _)| name == param)
        .map(|(_, value)| value.into_owned())
}

/// The query of `url` as sorted pairs, but those under `param`.
fn query_without(url: &url::Url, param: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| name != param)
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    pairs.sort();
    pairs
}

/// Whether `url` is of the route `own` is, which `op`'s path template has `segments`
/// segments of: the last that many segments of their paths are the same (a server's
/// base path or a mount prefix aside).
fn same_route(url: &url::Url, own: &url::Url, segments: usize) -> bool {
    let tail = |url: &url::Url| -> Vec<String> {
        let all: Vec<String> = url
            .path()
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        all[all.len().saturating_sub(segments)..].to_vec()
    };
    segments > 0 && tail(url) == tail(own)
}

/// The segments of `op`'s path template.
fn template_segments(op: &Operation) -> usize {
    match &op.binding {
        Binding::Http { path, .. } => path.split('/').filter(|s| !s.is_empty()).count(),
        _ => 0,
    }
}

/// The URLs `value` holds, depth first, but in member `skip` of the top, each read
/// against `base`.
fn urls_in(
    value: &Value,
    skip: Option<&str>,
    base: &url::Url,
    depth: usize,
    out: &mut Vec<url::Url>,
) {
    if depth > 8 {
        return;
    }
    match value {
        Value::String(text) if text.contains('/') || text.contains('?') => {
            out.extend(parse_url(text, base));
        }
        Value::Array(items) => {
            for item in items {
                urls_in(item, None, base, depth + 1, out);
            }
        }
        Value::Object(members) => {
            for (_, member) in members
                .iter()
                .filter(|(name, _)| Some(name.as_str()) != skip)
            {
                urls_in(member, None, base, depth + 1, out);
            }
        }
        _ => {}
    }
}

/// The descriptions of `schema`'s parts.
fn descriptions<'a>(bundle: &'a mb_mcm::Bundle, schema: Option<&'a Value>) -> Vec<&'a str> {
    let (parts, _) = schema_parts(bundle, schema);
    parts
        .iter()
        .filter_map(|p| p.get("description").and_then(Value::as_str))
        .collect()
}

/// Whether `url` shows every value of `call`'s query: a link of this very request.
fn shows_request(url: &url::Url, call: &OperationCall) -> bool {
    let shown = query_without(url, "");
    call_pairs(call, "").is_some_and(|asked| asked.iter().all(|pair| shown.contains(pair)))
}

/// Whether `text` holds `name` as code: in backticks (`` `from` ``).
fn quotes(text: &str, name: &str) -> bool {
    text.contains(&format!("`{name}`"))
}

/// Whether `text` holds `name` set off as a query key: `?from=`, `&from=`, or `from=`
/// not inside a longer name.
fn names_as_key(text: &str, name: &str) -> bool {
    let key = format!("{name}=");
    text.match_indices(&key).any(|(at, _)| {
        text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '[' || c == '.'))
    })
}

/// The description `schema` holds itself, not one of a schema it refers to (a shared
/// link schema's).
fn own_description(schema: &Value) -> Option<&str> {
    schema.get("description").and_then(Value::as_str)
}

/// Whether `schema` holds a URL: free text (see [`is_free_text`]) whose `format` is a
/// URL's, or that its description calls a URL or URI.
pub(super) fn is_url(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
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

/// Whether member name `name` names the next page, as the link relation does: `next`
/// itself, or a name whose first word is `next` (`nextLink`, `next_page_url`), after
/// any dotted prefix (`@odata.nextLink`).
fn names_next(name: &str) -> bool {
    let last = name.rsplit('.').next().unwrap_or(name);
    mb_mcm::words(last)
        .first()
        .is_some_and(|word| word.eq_ignore_ascii_case("next"))
}

/// The members of object `schema` on the path to a URL member that [`names_next`]
/// names (itself, or an object holding it), at most [`MAX_URL_DEPTH`] names deep,
/// leaving out member `skip` of the top (the list), each with the schemas on its path.
fn next_urls<'a>(
    bundle: &'a mb_mcm::Bundle,
    schema: &'a Value,
    skip: &str,
    path: &mut Vec<(String, &'a Value)>,
    out: &mut Vec<Vec<(String, &'a Value)>>,
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
        path.push((name.clone(), member));
        if is_url(bundle, member) {
            let named = path.iter().any(|(name, _)| names_next(name));
            if named && !out.iter().any(|seen| same_path(seen, path)) {
                out.push(path.clone());
            }
        } else if path.len() < MAX_URL_DEPTH {
            next_urls(bundle, member, skip, path, out);
        }
        path.pop();
    }
}

fn same_path(a: &[(String, &Value)], b: &[(String, &Value)]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|((x, _), (y, _))| x == y)
}

/// The one of `params` of several words that shares its first word with a page size of
/// several words (`page[cursor]` beside `page[size]`, where `filter`, or a bare `page`,
/// is another). Two such, or none, is none.
fn sharing_size<'a>(params: &[&'a str], size_param: &str) -> Option<&'a str> {
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

/// The cursor of `params`: the one there, else the one [`sharing_size`] picks.
fn cursor_param<'a>(params: &[&'a str], size_param: &str) -> Option<&'a str> {
    match params {
        [one] => Some(one),
        _ => sharing_size(params, size_param),
    }
}

/// The one place answer `body` to `call`, which reached `page_url` (of schema `schema`,
/// its list `list`), has for the URL of its next page, when it has none for a token (see
/// the module's doc). `params` are the parameters [`slot`] could pair with a token, with
/// their schemas.
fn url_slot(
    bundle: &mb_mcm::Bundle,
    (op, call, page_url): (&Operation, &OperationCall, &str),
    (schema, list): (&Value, &str),
    body: &Value,
    (params, size_param): (&[(&str, &Value)], &str),
) -> Option<Slot> {
    let own = url::Url::parse(page_url).ok()?;
    let segments = template_segments(op);
    let mut found = Vec::new();
    next_urls(bundle, schema, list, &mut Vec::new(), &mut found);
    let [path] = found.as_slice() else {
        return None;
    };
    let names: Vec<String> = path.iter().map(|(name, _)| name.clone()).collect();
    let at = names.iter().position(|name| names_next(name))?;
    let next_name = &names[at];
    // The `next` member as its own parent declares it, not a shared schema it refers to.
    let next_own = own_description(path[at].1);
    // The URLs of this very request the model wrote outside the list (its `self`), but
    // the `next` member itself: a next link is another page's, whatever it carries.
    let without_next = placed(body, &names[..=at], None)?;
    let mut shown = Vec::new();
    urls_in(&without_next, Some(list), &own, 0, &mut shown);
    shown.retain(|url| same_route(url, &own, segments) && shows_request(url, call));
    let declared = op.input.query.as_ref().and_then(|q| q.get("properties"));
    // No format or pattern shapes a cursor (`since`, a date-time).
    let candidates: Vec<&str> = params
        .iter()
        .filter(|(_, schema)| {
            let (parts, _) = schema_parts(bundle, Some(schema));
            !parts
                .iter()
                .any(|p| p.get("format").is_some() || p.get("pattern").is_some())
        })
        .map(|(name, _)| *name)
        .collect();
    // Carried empty by a link of this request: a first page's cursor, which a filter
    // the request did not send (`status=open`) is not.
    let carried: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|name| {
            shown.iter().any(|url| {
                url.query_pairs()
                    .any(|(key, value)| key == *name && value.trim().is_empty())
            })
        })
        .collect();
    // Named as code: the parameter's description names the `next` member (`` `next` ``,
    // `_links.next`), or the `next` member's own description names the parameter
    // (`` `from` ``, `from=`).
    let full = names[..=at].join(".");
    let told: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|name| {
            descriptions(bundle, declared.and_then(|d| d.get(*name)))
                .iter()
                .any(|text| quotes(text, next_name) || (at > 0 && quotes(text, &full)))
                || next_own.is_some_and(|text| quotes(text, name) || names_as_key(text, name))
        })
        .collect();
    // What a link of the request carries, then the page size's first word, then what
    // the contract's descriptions say.
    let param = cursor_param(&carried, size_param)
        .or_else(|| sharing_size(&candidates, size_param))
        .or(match told.as_slice() {
            [one] => Some(*one),
            _ => None,
        })?;

    // A URL the model wrote there, of this route, to another page by anything but the
    // cursor (`page=2`) stays as it is: the page it names is the model's.
    let given = names
        .iter()
        .try_fold(body, |at, name| at.get(name))
        .and_then(Value::as_str)
        .and_then(|text| parse_url(text, &own));
    if let Some(given) = given
        && same_route(&given, &own, segments)
        && call_pairs(call, param).is_none_or(|asked| query_without(&given, param) != asked)
    {
        return None;
    }
    Some(Slot {
        path: names,
        param: param.to_string(),
        url: Some(page_url.to_string()),
    })
}

/// The query of `call` as sorted text pairs, but those under `param`; `None` when a
/// value is an object.
fn call_pairs(call: &OperationCall, param: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for (name, value) in call.query.iter().filter(|(name, _)| name.as_str() != param) {
        let values = match value {
            Value::Array(values) => values.iter().collect(),
            other => vec![other],
        };
        for value in values {
            match value {
                Value::Null => {}
                Value::Object(_) => return None,
                Value::String(text) => pairs.push((name.clone(), text.clone())),
                other => pairs.push((name.clone(), other.to_string())),
            }
        }
    }
    pairs.sort();
    Some(pairs)
}

/// The URL of the page of `call` that carries `token` under `param`: `page_url` (the
/// URL the call reached, see [`CallPlan::page_url`]), with the call's query, `token`
/// for what it held under `param`. `None` for a query value that is an object.
fn page_url(page_url: &str, call: &OperationCall, param: &str, token: &str) -> Option<String> {
    let mut url = url::Url::parse(page_url).ok()?;
    let mut pairs = call_pairs(call, param)?;
    pairs.push((param.to_string(), token.to_string()));
    url.query_pairs_mut().clear().extend_pairs(
        pairs
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    );
    Some(url.into())
}

/// The answers `body` to `call` may be with `token` at `slot`, in the order tried: the
/// token there, or, for a URL, the page's URL (see [`page_url`]) there, else in a copy
/// of an object beside the one holding it that holds the same member (HAL's
/// `_links.self` for a `_links.next` that is `null`, as a link has other members), one
/// whose URL is of the request's own route first.
fn filled(
    body: &Value,
    slot: &Slot,
    (op, call): (&Operation, &OperationCall),
    token: &str,
) -> Vec<Value> {
    let Some(base) = slot.url.as_deref() else {
        return placed(body, &slot.path, Some(Value::String(token.to_string())))
            .into_iter()
            .collect();
    };
    let (Some(url), Ok(own)) = (
        page_url(base, call, &slot.param, token),
        url::Url::parse(base),
    ) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = placed(body, &slot.path, Some(Value::String(url.clone())))
        .into_iter()
        .collect();
    if let Some((leaf, link)) = slot.path.split_last()
        && let Some((own_name, holder)) = link.split_last()
        && !holder.is_empty()
    {
        let siblings = holder
            .iter()
            .try_fold(body, |at, name| at.get(name))
            .and_then(Value::as_object);
        let segments = template_segments(op);
        let mut siblings: Vec<(&String, &Value)> = siblings
            .into_iter()
            .flatten()
            .filter(|(name, sibling)| {
                *name != own_name && sibling.get(leaf).is_some_and(Value::is_string)
            })
            .collect();
        // Of the request's own route first: `self`, not `documentation`.
        siblings.sort_by_key(|(_, sibling)| {
            !sibling
                .get(leaf)
                .and_then(Value::as_str)
                .and_then(|text| parse_url(text, &own))
                .is_some_and(|at| same_route(&at, &own, segments))
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

/// The text query parameters of `op` a token may come back under: declared free text
/// ([`is_free_text`]), no page size `size_param`, no bare `id`, naming no member of the
/// items `item_parts` describe; and, with `call`, none it carries.
fn token_candidates<'a>(
    bundle: &'a mb_mcm::Bundle,
    op: &'a Operation,
    item_parts: &[&'a Value],
    size_param: &str,
    call: Option<&OperationCall>,
) -> Vec<(&'a str, &'a Value)> {
    let Some(declared) = op
        .input
        .query
        .as_ref()
        .and_then(|q| q.get("properties"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    declared
        .iter()
        .filter(|(name, schema)| {
            name.as_str() != size_param
                && !mb_mcm::is_generic_id(name)
                && call.is_none_or(|call| !carries(call, name))
                && lists::named_member(bundle, item_parts, name).is_none()
                && is_free_text(bundle, schema)
        })
        .map(|(name, schema)| (name.as_str(), schema))
        .collect()
}

/// The places for a token in answers of schema `schema` beside the list `list`: each
/// free-text member, at the top level or in an object member, whose name is, or ends
/// with the words of, exactly one of `params`, and whose value in `body` echoes none of
/// `echoed` (a search's `query`).
fn token_slots(
    bundle: &mb_mcm::Bundle,
    (schema, list): (&Value, &str),
    body: &Value,
    params: &[&str],
    echoed: &[String],
) -> Vec<Slot> {
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
    found
}

/// The one place answer `body` to `call` has for a token (see the module's doc).
pub(super) fn slot(
    bundle: &mb_mcm::Bundle,
    (op, call, page_url): (&Operation, &OperationCall, Option<&str>),
    body: &Value,
    size_param: &str,
) -> Option<Slot> {
    let list = list_member(body)?;
    let schema = op.success_output()?.schema.as_ref()?;
    let mut held = None;
    let (_, item_parts) = lists::list_parts(bundle, schema, body, &mut held)?;
    let candidates = token_candidates(bundle, op, &item_parts, size_param, Some(call));
    let params: Vec<&str> = candidates.iter().map(|(name, _)| *name).collect();
    let echoed = request_values(call);
    let mut found = token_slots(bundle, (schema, list), body, &params, &echoed);
    match found.pop() {
        Some(first) => found.is_empty().then_some(first),
        // No place for the token itself: a place for the next page's URL, if any, unless
        // the list is paged by number or offset.
        None if runs::starts(bundle, (op, call), size_param) => None,
        None => url_slot(
            bundle,
            (op, call, page_url?),
            (schema, list),
            body,
            (&candidates, size_param),
        ),
    }
}

/// The one parameter, and its value, under which `call` may carry a token a first
/// page of `op` gave: of those [`slot`] could pair with a member of the answer `body`
/// (a token's member, else, with one next-page URL member, a cursor no `format` or
/// `pattern` shapes), the one `call` carries a non-empty text value under. `None` when
/// it carries none or several.
fn carried_token(
    bundle: &mb_mcm::Bundle,
    (op, call): (&Operation, &OperationCall),
    body: &Value,
    size_param: &str,
) -> Option<(String, String)> {
    let list = list_member(body)?;
    let schema = op.success_output()?.schema.as_ref()?;
    let mut held = None;
    let (_, item_parts) = lists::list_parts(bundle, schema, body, &mut held)?;
    let candidates = token_candidates(bundle, op, &item_parts, size_param, None);
    let params: Vec<&str> = candidates.iter().map(|(name, _)| *name).collect();
    let slots = token_slots(bundle, (schema, list), body, &params, &[]);
    let paired: Vec<String> = match slots.as_slice() {
        [one] => vec![one.param.clone()],
        [] if !runs::starts(bundle, (op, call), size_param) => {
            let mut urls = Vec::new();
            next_urls(bundle, schema, list, &mut Vec::new(), &mut urls);
            if urls.len() == 1 {
                candidates
                    .iter()
                    .filter(|(_, schema)| {
                        let (parts, _) = schema_parts(bundle, Some(schema));
                        !parts
                            .iter()
                            .any(|p| p.get("format").is_some() || p.get("pattern").is_some())
                    })
                    .map(|(name, _)| (*name).to_string())
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };
    let mut carried = paired.into_iter().filter_map(|param| {
        let token = call.query.get(&param)?.as_str()?.trim().to_string();
        (!token.is_empty()).then_some((param, token))
    });
    let first = carried.next()?;
    carried.next().is_none().then_some(first)
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

/// The prefix of a [`Followed`] document.
const FOLLOWED_PREFIX: &str = "#then/";

/// What a cut's last page led on to: the token the model gave on the first page (see
/// the module's doc), where it sat, and the ids of the items the cut served before it.
/// A request carrying the token under its parameter, with the first page's other query
/// values, is the model's, but those items are not served again on it. One document
/// per route, path values and parameter, under the store's reserved type: every first
/// page there that keeps a cut over a token the model gave writes it anew, or ends it
/// when nothing leads on, and so does one that keeps the model's token, so a token the
/// model gives again (`abc`) never finds an earlier walk's. The session's TTL ends it.
pub(super) struct Followed {
    op: String,
    path: Value,
    query: Value,
    token: String,
    slot: Slot,
    size_param: String,
    pub(super) ids: Vec<String>,
}

impl Followed {
    fn to_value(&self) -> Value {
        serde_json::json!({
            "op": self.op,
            "path": self.path,
            "query": self.query,
            "token": self.token,
            "slot": self.slot.path,
            "param": self.slot.param,
            "url": self.slot.url,
            "size_param": self.size_param,
            "ids": self.ids,
        })
    }

    fn of(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key)?.as_str().map(str::to_string);
        Some(Self {
            op: text("op")?,
            path: value.get("path")?.clone(),
            query: value.get("query")?.clone(),
            token: text("token")?,
            slot: Slot {
                path: value
                    .get("slot")?
                    .as_array()?
                    .iter()
                    .map(|s| s.as_str().map(str::to_string))
                    .collect::<Option<_>>()?,
                param: text("param")?,
                url: text("url"),
            },
            size_param: text("size_param")?,
            ids: value
                .get("ids")?
                .as_array()?
                .iter()
                .map(|id| id.as_str().map(str::to_string))
                .collect::<Option<_>>()?,
        })
    }
}

/// The key of the [`Followed`] document of `op`'s route, `call`'s path values and the
/// token parameter `param`.
fn followed_key(op: &Operation, call: &OperationCall, param: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [op.id.as_str(), &path_of(call).to_string(), param] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("{FOLLOWED_PREFIX}{hex}")
}

/// The page size parameter of a list read `call` of `op`, as the cut's are found.
fn size_param_of(bundle: &mb_mcm::Bundle, op: &Operation, call: &OperationCall) -> Option<String> {
    let schema = op.success_output()?.schema.as_ref()?;
    let doc = std::cell::OnceCell::new();
    mb_engine::budget::requested_page(
        op.input.query.as_ref(),
        || doc.get_or_init(|| bundle.with_reachable_defs(schema)),
        call,
    )
    .map(|(name, _)| name.to_string())
}

/// The [`Followed`] note of the model's token `call` carries, whose page the model
/// answers with `body`; `None` when it carries none. At most one read: of the one
/// parameter [`carried_token`] finds, and none when the request carries no value there
/// (a first page). For a next-page URL as for a token, the request matches only when
/// its other query values are exactly the first page's (page size aside): a URL the
/// model wrote with another value, or one more, is no follow of the note, and its page
/// is the model's as it answers it.
pub(super) async fn followed(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    body: &Value,
) -> Option<Followed> {
    let rt = state.rt;
    let op = plan.op;
    let size_param = size_param_of(rt.bundle(), op, call)?;
    let (param, token) = carried_token(rt.bundle(), (op, call), body, &size_param)?;
    let key = followed_key(op, call, &param);
    let doc = state.get(&plan.session, INTERNAL_TYPE, &key).await?;
    let followed = Followed::of(&doc.data)?;
    (followed.op == op.id
        && followed.slot.param == param
        && followed.token == token
        && followed.path == path_of(call)
        && followed.query == bound_query(call, &param, &followed.size_param))
    .then_some(followed)
}

/// Ends the [`Followed`] note of the token answer `body` to the first page `call`
/// holds where the model gave it: a page that keeps the model's token leads on to
/// nothing the mock served.
pub(super) async fn forget_followed(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    body: &Value,
) {
    let rt = state.rt;
    let op = plan.op;
    let Some(size_param) = size_param_of(rt.bundle(), op, call) else {
        return;
    };
    let Some(slot) = slot(
        rt.bundle(),
        (op, call, plan.page_url.as_deref()),
        body,
        &size_param,
    ) else {
        return;
    };
    if slot.given(body).is_some() {
        let key = followed_key(op, call, &slot.param);
        state.delete(&plan.session, INTERNAL_TYPE, &key).await;
    }
}

/// `body`, the model's page for `followed`'s token, without the items whose identity
/// is one of the ids the cut served, where the answer still validates so. A page left
/// with no item ends the list instead: no token where the token sat (`null`, left out
/// or empty, whichever validates), and its more flag `false` where it has one.
pub(super) fn drop_served(
    plan: &CallPlan<'_>,
    rt: &MockRuntime,
    body: &mut Value,
    followed: &Followed,
) {
    let op = plan.op;
    let field = item_identity(rt.bundle(), op, body);
    let mut kept = body.clone();
    let Some(items) = list_items_mut(&mut kept) else {
        return;
    };
    let was = items.len();
    items.retain(|item| {
        mb_mcm::member_at(item, &field)
            .and_then(id_string)
            .is_none_or(|id| !followed.ids.contains(&id))
    });
    if items.len() == was {
        return;
    }
    let empty = items.is_empty();
    let valid = |candidate: &Value| {
        plan.engine
            .serve_stored(rt.bundle(), op, candidate, None)
            .is_some()
    };
    if !empty {
        if valid(&kept) {
            *body = kept;
        }
        return;
    }
    let flag = more_flag(rt.bundle(), op, &followed.slot.path);
    let parents = parents_of(&followed.slot.path);
    let end = ended(&kept, &followed.slot)
        .into_iter()
        .find_map(|candidate| {
            flag.as_deref()
                .and_then(|flag| flag_set(&candidate, parents, flag, false))
                .filter(|done| valid(done))
                .or_else(|| valid(&candidate).then_some(candidate))
        });
    if let Some(end) = end {
        *body = end;
    }
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
    /// The token or URL the model gave at the slot, which the last page carries on to
    /// the model's own next page.
    then: Option<Value>,
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
            "then": self.then,
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
            then: value.get("then").filter(|then| !then.is_null()).cloned(),
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
    let slot = slot(
        rt.bundle(),
        (op, call, plan.page_url.as_deref()),
        body,
        &cut.size_param,
    )
    // A URL the model gave to its own next page stays, as no cut serves it, unless the
    // store holds records for a next page: those come first, and the URL follows them
    // on the last of their pages.
    .filter(|slot| slot.url.is_none() || slot.given(body).is_none() || cut.stored > 0);
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
        let gone = match (t, mb_mcm::member_at(&item, &field).and_then(id_string)) {
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
    // The model's token the last page carries on, with what the rest serves before it;
    // any other first page here ends an earlier note (see [`Followed`]).
    let note = slot.as_ref().map(|slot| {
        let then = slot.given(body).filter(|_| cut.stored > 0).map(|token| {
            let ids: Vec<String> = rest
                .iter()
                .filter_map(|item| mb_mcm::member_at(item, &field).and_then(id_string))
                .collect();
            (token, ids)
        });
        let at = Slot {
            path: slot.path.clone(),
            param: slot.param.clone(),
            url: slot.url.clone(),
        };
        (at, then)
    });
    let size_param = cut.size_param.clone();
    let kept = match slot {
        Some(slot) => tokened(
            rt,
            (plan, call),
            body,
            slot,
            (cut.size, cut.size_param),
            (rest, cut.stored > 0),
        ),
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
                query: None,
                collection: collection_of(op, call),
                origin: WriteOrigin::Seeded,
                held: false,
                header_id: None,
            },
        )
        .await;
    }
    // A cut not kept serves no token: the answer keeps the model's.
    let kept = state.upsert(session, document).await;
    if kept && let Some((at, then)) = note {
        let key = followed_key(op, call, &at.param);
        match then {
            Some((token, ids)) => {
                let followed = Followed {
                    op: op.id.clone(),
                    path: path_of(call),
                    query: bound_query(call, &at.param, &size_param),
                    token,
                    slot: at,
                    size_param,
                    ids,
                };
                let document = StoredResource {
                    resource_type: INTERNAL_TYPE.to_string(),
                    id: key,
                    data: followed.to_value(),
                };
                state.upsert(session, document).await;
            }
            None => {
                state.delete(session, INTERNAL_TYPE, &key).await;
            }
        }
    }
    if kept {
        generated.body = Some(page);
    }
    kept
}

/// The first page `body` with a token at `slot` for `rest`, and the cut that serves it;
/// `None` when no such page validates.
fn tokened(
    rt: &MockRuntime,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    body: &Value,
    slot: Slot,
    (size, size_param): (usize, String),
    (rest, stored): (Vec<Value>, bool),
) -> Option<(Value, StoredResource)> {
    let op = plan.op;
    // What the model gave there leads on after stored records, to the model's own next
    // page. A rest of the model's own items only is what its token passed already.
    let then = slot.given(body).filter(|_| stored).and_then(|_| {
        slot.path
            .iter()
            .try_fold(body, |at, name| at.get(name))
            .cloned()
    });
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
        then,
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
    } else if let Some(then) = cut
        .then
        .as_ref()
        .and_then(|then| placed(&body, &cut.slot.path, Some(then.clone())))
        .and_then(|page| valid(&page))
    {
        // The last page of the rest leads on to the model's own next page.
        then
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
