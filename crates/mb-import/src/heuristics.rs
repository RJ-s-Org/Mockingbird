//! Inference of operation kind, resource type, and identity from HTTP shape.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::Ctx;
use crate::schema::defs_name;
use mb_mcm::{
    Binding, Effect, HttpMethod, Identity, Operation, OperationKind, OutputVariant, StatusSelector,
};
use serde_json::{Map, Value};

/// The segments of `path` as the heuristics read them: each `/`-separated
/// segment, except that a segment with a resource's key inside it
/// ([`key_segment`]) reads as the segments a whole-segment parameter would make
/// it, so `/companies({company_id})/customers({customer_id})` reads as
/// `/companies/{company_id}/customers/{customer_id}` and `/carts/key={key}` as
/// `/carts/{key}`. Every piece is a slice of `path`.
fn segments(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        match key_segment(seg) {
            Some(KeySegment { stem, params }) => {
                out.extend(stem);
                out.extend(params);
            }
            None => out.push(seg),
        }
    }
    out
}

/// A path segment with literal text before its parameters that says they key a
/// resource ([`key_segment`]).
#[derive(Debug, PartialEq)]
struct KeySegment<'a> {
    /// The resource type the segment names: `customers` of
    /// `customers({customer_id})`. `None` for `key={key}`, a key of the
    /// collection before it.
    stem: Option<&'a str>,
    /// Each parameter that keys it, as its whole-segment text:
    /// `{customer_id}`. None for a composite key or a call.
    params: Vec<&'a str>,
}

/// The literal text of a segment before its first parameter, each parameter as
/// its `{name}` text, and the literal text after its last; `None` when the
/// segment has no parameter or is not a template the router reads as one (an
/// unclosed `{`, an empty name, a `{{` or `}}` literal brace).
fn template(seg: &str) -> Option<(&str, Vec<&str>, &str)> {
    if seg.contains("{{") || seg.contains("}}") {
        return None;
    }
    let first = seg.find('{')?;
    let mut params = Vec::new();
    let mut at = 0;
    while let Some(open) = seg[at..].find('{').map(|i| at + i) {
        let close = open + seg[open..].find('}')?;
        if seg[at..open].contains('}') || close == open + 1 || seg[open + 1..close].contains('{') {
            return None;
        }
        params.push(&seg[open..=close]);
        at = close + 1;
    }
    if seg[at..].contains('}') {
        return None;
    }
    Some((&seg[..first], params, &seg[at..]))
}

/// The resource a segment's literal text keys by the parameters inside it:
///
/// - an OData key, `customers({customer_id})` or `customers('{id}')`: the type
///   before the `(`, keyed by the one parameter inside as
///   `/customers/{customer_id}` would be;
/// - a composite OData key or a function call, `lines(doc={d},seq={s})` or
///   `NS.getBalance(asOf={date})`: the name before the `(` alone, as if it had
///   no parameters. A [`mb_mcm::Identity`] names one parameter, and no one part
///   of a composite key, nor an argument of a call, identifies a record, so it
///   gives none; a call after a keyed record (`/customers({id})/NS.post(x={x})`)
///   reads as the action `/customers/{id}/NS.post`;
/// - a named key, `key={key}`: the parameter alone, an alternate key of the
///   collection the segment before names, as `/carts/{key}` would be.
///
/// Any other segment stays one segment: a whole-segment parameter (`{id}`,
/// `{Sid}.json`), a parameter with only a suffix (`{id}:archive`), and other
/// text around one (`v{version}`), which says nothing of what it keys.
fn key_segment(seg: &str) -> Option<KeySegment<'_>> {
    let (prefix, params, suffix) = template(seg)?;
    let name_like = |s: &str| {
        s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
    };
    let formatted = |s: &str| s.is_empty() || FORMAT_SUFFIXES.contains(&s);
    if let Some((stem, args)) = prefix.split_once('(') {
        let closed = suffix
            .trim_start_matches(['\'', '"'])
            .strip_prefix(')')
            .is_some_and(formatted);
        if !name_like(stem) || !closed {
            return None;
        }
        // One parameter, unnamed: a key. Named or several: no key.
        let keyed = params.len() == 1 && args.trim_matches(['\'', '"']).is_empty();
        let stem = Some(stem).filter(|s| !s.is_empty());
        return match (keyed, stem) {
            (true, _) => Some(KeySegment { stem, params }),
            (false, Some(_)) => Some(KeySegment {
                stem,
                params: Vec::new(),
            }),
            (false, None) => None,
        };
    }
    let name = prefix.strip_suffix('=')?;
    let named = !name.is_empty() && name_like(name);
    (named && formatted(suffix) && params.len() == 1).then_some(KeySegment { stem: None, params })
}

/// Format suffixes some APIs put on every path (Twilio's `Messages.json`,
/// `{Sid}.json`).
const FORMAT_SUFFIXES: [&str; 2] = [".json", ".xml"];

/// `seg` without a [`FORMAT_SUFFIXES`] suffix.
fn without_format(seg: &str) -> &str {
    FORMAT_SUFFIXES
        .iter()
        .find_map(|suffix| seg.strip_suffix(suffix))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(seg)
}

/// The parameter a segment is: `{id}`, or `{Sid}.json`.
fn param_name(seg: &str) -> Option<&str> {
    without_format(seg)
        .strip_prefix('{')?
        .strip_suffix('}')
        .filter(|name| !name.contains(['{', '}']))
}

fn is_param(seg: &str) -> bool {
    param_name(seg).is_some()
}

/// Whether a segment is shaped like an API version: `v1`, `v1.2`, `v1beta2`, `3.0`,
/// `1`, `2024-01-01`. It is one only before another static segment
/// ([`version_at`]).
fn is_version(seg: &str) -> bool {
    let numbers = |s: &str| {
        !s.is_empty()
            && s.split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    };
    let date = {
        let parts: Vec<&str> = seg.split('-').collect();
        parts.iter().map(|p| p.len()).eq([4, 2, 2])
            && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
    };
    if date || numbers(seg) {
        return true;
    }
    let Some(rest) = seg.strip_prefix(['v', 'V']) else {
        return false;
    };
    // `v1`, `v1.2`, and a pre-release after the number: `v1beta2`, `v2alpha`.
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let (number, label) = rest.split_at(end);
    let letters = label.trim_end_matches(|c: char| c.is_ascii_digit());
    numbers(number) && letters.bytes().all(|b| b.is_ascii_lowercase())
}

/// Verbs that make a `POST` a read in RPC-style APIs (Plaid's
/// `/accounts/get`, `/transactions/sync`; Elasticsearch's `_search`; Slack's
/// `users.list`).
const READ_VERBS: [&str; 13] = [
    "get", "list", "search", "sync", "query", "find", "fetch", "retrieve", "lookup", "read",
    "count", "info", "history",
];

/// The resource and the read verb of an RPC-style last segment: `get` and
/// no resource for `get_by_id`, `users` and `list` for `users.list`.
fn rpc_read(segment: &str) -> Option<(Option<&str>, &str)> {
    let (resource, action) = match segment.rsplit_once('.') {
        Some((resource, action)) => (Some(resource).filter(|r| !r.is_empty()), action),
        None => (None, segment),
    };
    let action = action.trim_start_matches(['_', '$']);
    let end = action
        .char_indices()
        .skip(1)
        .find(|(_, c)| c.is_ascii_uppercase() || matches!(c, '_' | '-'))
        .map_or(action.len(), |(i, _)| i);
    let verb = &action[..end];
    READ_VERBS
        .iter()
        .find(|v| verb.eq_ignore_ascii_case(v))
        .map(|v| (resource, *v))
}

/// Whether `path` ends in an RPC-style read (see [`READ_VERBS`]).
fn ends_in_read(path: &str) -> bool {
    segments(path)
        .last()
        .is_some_and(|s| !is_param(s) && rpc_read(s).is_some())
}

pub(crate) fn infer_kind(method: HttpMethod, path: &str) -> OperationKind {
    match method {
        HttpMethod::Get | HttpMethod::Head | HttpMethod::Options => OperationKind::Query,
        HttpMethod::Post if ends_in_read(path) => OperationKind::Query,
        HttpMethod::Post => {
            let ends_static = segments(path).last().is_none_or(|s| !is_param(s));
            OperationKind::Command(if ends_static {
                Effect::Create
            } else {
                Effect::Other
            })
        }
        HttpMethod::Put | HttpMethod::Patch => OperationKind::Command(Effect::Update),
        HttpMethod::Delete => OperationKind::Command(Effect::Delete),
    }
}

/// The last static segment before any trailing `{param}`; for an
/// RPC-style read (`/accounts/get`, `users.list`), what it reads. A version
/// segment ([`version_at`]) is never the type: `/3.0/search-members` is
/// `search-members`.
pub(crate) fn infer_resource_type(path: &str) -> Option<String> {
    let segs: Vec<&str> = segments(path).into_iter().map(without_format).collect();
    let mut statics = (0..segs.len())
        .rev()
        .filter(|&at| !is_param(segs[at]) && !version_at(&segs, at))
        .map(|at| &segs[at]);
    let last = statics.next()?;
    if ends_in_read(path) {
        match rpc_read(last) {
            Some((Some(resource), _)) => return Some(resource.to_owned()),
            Some((None, _)) => {
                if let Some(before) = statics.next() {
                    return Some((*before).to_owned());
                }
            }
            None => {}
        }
    }
    Some((*last).to_owned())
}

/// Whether `segs[at]` is a version: shaped like one ([`is_version`]) and followed by
/// another static segment, as in `/v1/search`. One the path ends in or a parameter
/// follows (`/v2/{id}/cancel`) is a resource like any other.
fn version_at(segs: &[&str], at: usize) -> bool {
    is_version(segs[at]) && segs.get(at + 1).is_some_and(|next| !is_param(next))
}

fn trailing_param(path: &str) -> Option<&str> {
    segments(path).last().and_then(|s| param_name(s))
}

/// Fields that identify a created resource without an `id`, where nothing in the
/// contract's paths names its key: a create (`POST /calls` answering a `sid`) of a
/// type no route reads by path. A key a read by path names comes first
/// ([`record_keys`]); this is only what the contract's structure cannot give.
const ID_FALLBACKS: [&str; 2] = ["sid", "uuid"];

/// The members one type's reads by path go by: what [`identity_for`] gives each of
/// its `GET`s ending in a parameter, on the answer and on the record of a one-member
/// envelope. A create of the type, which has no path parameter, goes by the one of
/// these its answer has: Asana's `POST /tasks` by the `gid` that
/// `GET /tasks/{task_gid}` reads.
type Keys = HashSet<String>;

/// The [`Keys`] of each type, taught by that type's own reads only: a tag read by
/// `{tag_name}` says nothing of how a report is keyed.
#[derive(Default)]
struct TypeKeys {
    by_type: HashMap<String, Keys>,
    /// Every type's: only to tell whether two schemas go by the same member
    /// ([`reads_back`]), which gives nothing an identity.
    any: Keys,
    none: Keys,
}

impl TypeKeys {
    fn of(&self, resource_type: Option<&str>) -> &Keys {
        resource_type
            .and_then(|t| self.by_type.get(t))
            .unwrap_or(&self.none)
    }
}

/// The [`TypeKeys`] of `operations`.
fn record_keys(operations: &[Operation], defs: &Map<String, Value>) -> TypeKeys {
    let none = Keys::new();
    let mut keys = TypeKeys::default();
    for op in gets(operations, |route| {
        route.last().is_some_and(|last| is_param(last))
    })
    .filter(|op| !is_stream(op))
    {
        let (Some(t), Some((_, path)), Some(schema)) =
            (op.resource_type.as_ref(), http_path(op), answer(op))
        else {
            continue;
        };
        let param = trailing_param(path);
        keys.by_type.entry(t.clone()).or_default().extend(
            answered_things(schema, defs)
                .iter()
                .filter_map(|thing| identity_for(param, Some(thing), defs, &none))
                .map(|identity| identity.field),
        );
    }
    keys.any = keys.by_type.values().flatten().cloned().collect();
    keys
}

/// Gives a create with no identity, and no path parameter, the one of its type's
/// [`Keys`] its answer has.
fn apply_keys(operations: &mut [Operation], defs: &Map<String, Value>, keys: &TypeKeys) {
    for op in operations
        .iter_mut()
        .filter(|op| op.identity.is_none() && op.kind == OperationKind::Command(Effect::Create))
    {
        let Some((_, path)) = http_path(op) else {
            continue;
        };
        if trailing_param(path).is_none() {
            let keys = keys.of(op.resource_type.as_deref());
            op.identity = identity_for(None, answer(op), defs, keys);
        }
    }
}

/// Gives each route of a type ending in a parameter that has no identity the one the
/// type's other routes on that parameter have, when they agree and the path names
/// another member than the key: GitLab's
/// `DELETE /projects/{id}/issues/{issue_iid}`, which answers nothing, goes by `iid` as
/// its `GET` does. The record is where its own answer holds it, so a route with no
/// answer has it as itself.
fn apply_shared_identities(operations: &mut [Operation]) {
    let mut by_route: HashMap<(String, String), Option<Identity>> = HashMap::new();
    for op in operations.iter().filter(|op| !is_stream(op)) {
        let (Some(t), Some(param), Some(identity)) = (
            op.resource_type.clone(),
            http_path(op).and_then(|(_, path)| trailing_param(path)),
            op.identity.as_ref(),
        ) else {
            continue;
        };
        // Only a path naming another member than the key needs it: elsewhere the path's
        // value is the key (also Jira's `issueIdOrKey`, which names both), which a route
        // goes by with or without an identity.
        if identity.path_param.as_deref() != Some(param) || identity.path_member() == identity.field
        {
            continue;
        }
        let shared = Identity {
            record: None,
            ..identity.clone()
        };
        by_route
            .entry((t, param.to_owned()))
            .and_modify(|known| {
                if known.as_ref() != Some(&shared) {
                    *known = None;
                }
            })
            .or_insert(Some(shared));
    }
    for op in operations
        .iter_mut()
        .filter(|op| op.identity.is_none() && !is_stream(op) && answer(op).is_none())
    {
        let (Some(t), Some(param)) = (
            op.resource_type.clone(),
            http_path(op).and_then(|(_, path)| trailing_param(path)),
        ) else {
            continue;
        };
        if let Some(Some(identity)) = by_route.get(&(t, param.to_owned())) {
            op.identity = Some(identity.clone());
        }
    }
}

/// The member of `props` named `name`, ASCII case-insensitively.
fn property<'a>(props: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    props
        .keys()
        .find(|k| k.eq_ignore_ascii_case(name))
        .map(String::as_str)
}

/// Whether a member's schema is of a single value, not an object or a list: only
/// such a member can be a key.
fn is_scalar_member(schema: &Value, defs: &Map<String, Value>) -> bool {
    resolve(schema, defs).is_some_and(|schema| {
        !matches!(
            schema.get("type").and_then(Value::as_str),
            Some("object" | "array")
        ) && schema.get("properties").is_none()
    })
}

/// The response field path parameter `param` names: the property of that name, else
/// the scalar one the longest trailing group of its [`mb_mcm::words`] names, separators and
/// case aside. `issue_iid` names `iid`, `task_gid` `gid`, `issueIdOrKey` `key`,
/// `MessageSid` `sid` and `issue_number` `number`.
fn param_field<'a>(
    param: &str,
    props: &'a Map<String, Value>,
    defs: &Map<String, Value>,
) -> Option<&'a str> {
    param_match(param, props, defs).map(|(_, member)| member)
}

/// [`param_field`], with how much of the parameter the member names: how many of its
/// words, and more than all of them for the member of its very name.
fn param_match<'a>(
    param: &str,
    props: &'a Map<String, Value>,
    defs: &Map<String, Value>,
) -> Option<(usize, &'a str)> {
    let words = mb_mcm::words(param);
    if let Some(member) = property(props, param) {
        return Some((words.len() + 1, member));
    }
    (0..words.len()).find_map(|from| {
        let group = mb_mcm::joined_name(&words[from..].concat());
        props
            .iter()
            .find(|(member, schema)| {
                mb_mcm::joined_name(member) == group && is_scalar_member(schema, defs)
            })
            .map(|(member, _)| (words.len() - from, member.as_str()))
    })
}

/// The identity of the record an envelope holds as the one item of an array member
/// (`{Id, Status, Contacts: [Contact]}` for `/Contacts/{ContactID}`), when the
/// collection's own list answers that member with that item (`GET /Contacts` answering
/// `{..., Contacts: [Contact]}`): the read answers the list's shape, one record in it.
/// The item's member of the path parameter's very name must be its own key (an item
/// keyed by an `id` beside it, as an incident's updates keyed by `id` beside
/// `incident_id`, refers to its owner by it), and the envelope has no key of its own:
/// no `id` and no member of that name.
fn item_identity(
    param: &str,
    schema: &Value,
    listed: &[&Value],
    defs: &Map<String, Value>,
) -> Option<Identity> {
    let props = schema_properties(Some(schema), defs)?;
    let full = mb_mcm::words(param).len() + 1;
    let own_key = props.contains_key("id")
        || param_match(param, &props, defs).is_some_and(|(score, _)| score >= full);
    if own_key {
        return None;
    }
    let none = Keys::new();
    let in_list = |member: &str, item: &Value| {
        listed.iter().any(|list| {
            schema_properties(Some(list), defs)
                .and_then(|p| array_items(p.get(member)?, defs).cloned())
                .is_some_and(|listed_item| &listed_item == item)
        })
    };
    let mut held = props.iter().filter_map(|(member, inner)| {
        let item = array_items(inner, defs)?;
        let item_props = schema_properties(Some(item), defs)?;
        let (score, named) = param_match(param, &item_props, defs)?;
        let identity = identity_at(Some(param), Some(item), defs, &none)?;
        (score >= full && identity.field == named && in_list(member, item))
            .then_some((member, identity))
    });
    let (Some((member, mut identity)), None) = (held.next(), held.next()) else {
        return None;
    };
    identity.record = Some(mb_mcm::RecordAt::MemberItem(member.clone()));
    Some(identity)
}

/// Gives a route ending in a parameter, whose answer holds its record as the one item
/// of a list's array ([`item_identity`]), that record's identity, over one tying the
/// parameter to an envelope member by part of its name (`{ContactID}` to `Id`).
fn apply_item_envelopes(operations: &mut [Operation], defs: &Map<String, Value>) {
    let lists: Vec<(String, String, Value)> = gets(operations, |route| {
        route.last().is_some_and(|last| !is_param(last))
    })
    .filter(|op| !is_stream(op))
    .filter_map(|op| {
        let (_, path) = http_path(op)?;
        Some((
            op.resource_type.clone()?,
            path.to_owned(),
            answer(op)?.clone(),
        ))
    })
    .collect();
    for op in operations.iter_mut().filter(|op| !is_stream(op)) {
        let Some((_, path)) = http_path(op) else {
            continue;
        };
        let Some(param) = trailing_param(path) else {
            continue;
        };
        let weak = op.identity.as_ref().is_none_or(|i| {
            i.record.is_none() && i.field != "id" && !i.field.eq_ignore_ascii_case(param)
        });
        let (true, Some(t), Some(schema)) = (weak, op.resource_type.as_deref(), answer(op)) else {
            continue;
        };
        let collection = path.rsplit_once('/').map_or("", |(head, _)| head);
        let listed: Vec<&Value> = lists
            .iter()
            .filter(|(lt, lp, _)| lt == t && lp == collection)
            .map(|(_, _, schema)| schema)
            .collect();
        if listed.is_empty() {
            continue;
        }
        let param = param.to_owned();
        if let Some(identity) = item_identity(&param, schema, &listed, defs) {
            op.identity = Some(identity);
        }
    }
}

/// `allOf` branches [`schema_properties`] reads, so ref cycles terminate.
const MAX_ALL_OF_BRANCHES: usize = 64;

/// Follow `$ref` chains through `defs` (bounded, so ref cycles terminate).
fn resolve<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> Option<&'a Value> {
    let mut cur = schema;
    for _ in 0..16 {
        match cur.get("$ref").and_then(Value::as_str) {
            Some(r) => cur = defs.get(defs_name(r)?)?,
            None => return Some(cur),
        }
    }
    None
}

/// The members a schema declares at its top level, with those of its `allOf`
/// branches: an answer has them all. An answer meets every branch, so a member two
/// branches declare is held to both: one that adds no constraint (`result: {}` on a
/// base envelope) leaves the other's (`result: $ref Record`), and of two that both
/// constrain it, the later branch's, an extension coming after its base, is kept.
/// A schema's own `properties` come after its `allOf`, which it extends.
fn schema_properties<'a>(
    schema: Option<&'a Value>,
    defs: &'a Map<String, Value>,
) -> Option<Cow<'a, Map<String, Value>>> {
    let schema = resolve(schema?, defs)?;
    let own = schema.get("properties").and_then(Value::as_object);
    if schema.get("allOf").and_then(Value::as_array).is_none() {
        return own.map(Cow::Borrowed);
    }
    let mut merged = Map::new();
    let mut visits = 0;
    let found = merge_all_of(schema, defs, &mut merged, &mut visits);
    found.then_some(Cow::Owned(merged))
}

/// Adds the members `schema` declares, through its `allOf` branches in their order
/// and then its own, to `merged` (see [`schema_properties`]). Whether any level
/// declares `properties`.
fn merge_all_of(
    schema: &Value,
    defs: &Map<String, Value>,
    merged: &mut Map<String, Value>,
    visits: &mut usize,
) -> bool {
    *visits += 1;
    if *visits > MAX_ALL_OF_BRANCHES {
        tracing::debug!(
            limit = MAX_ALL_OF_BRANCHES,
            "allOf branches past the limit are not read: their members are left out"
        );
        return false;
    }
    let mut found = false;
    for branch in schema
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(branch) = resolve(branch, defs) {
            found |= merge_all_of(branch, defs, merged, visits);
        }
    }
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        found = true;
        for (member, inner) in props {
            let next = match merged.get(member) {
                None => inner.clone(),
                Some(held) => both(held, inner, defs, 0),
            };
            merged.insert(member.clone(), next);
        }
    }
    found
}

/// How deep [`both`] merges the members of objects two branches declare.
const MAX_MERGE_DEPTH: usize = 8;

/// One schema for a member two `allOf` branches declare, `held` the earlier one's: an
/// answer meets both. One that adds no constraint ([`unconstrained`]) leaves the
/// other; two objects declaring members have them all, merged member by member the
/// same way, and require what either requires; else the later one is kept.
fn both(held: &Value, next: &Value, defs: &Map<String, Value>, depth: usize) -> Value {
    if unconstrained(next, defs) {
        return held.clone();
    }
    if unconstrained(held, defs) || depth >= MAX_MERGE_DEPTH {
        return next.clone();
    }
    let members = |schema: &Value| {
        let resolved = resolve(schema, defs)?;
        let props = resolved.get("properties")?.as_object()?.clone();
        Some((resolved.clone(), props))
    };
    let (Some((held_schema, held_props)), Some((next_schema, next_props))) =
        (members(held), members(next))
    else {
        return next.clone();
    };
    let mut props = held_props;
    for (member, inner) in next_props {
        let merged = match props.get(&member) {
            None => inner,
            Some(earlier) => both(earlier, &inner, defs, depth + 1),
        };
        props.insert(member, merged);
    }
    let mut required: Vec<Value> = Vec::new();
    for schema in [&held_schema, &next_schema] {
        for name in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !required.contains(name) {
                required.push(name.clone());
            }
        }
    }
    let mut out = held_schema.as_object().cloned().unwrap_or_default();
    out.extend(next_schema.as_object().cloned().unwrap_or_default());
    out.insert("properties".into(), Value::Object(props));
    if !required.is_empty() {
        out.insert("required".into(), Value::Array(required));
    }
    Value::Object(out)
}

/// Whether a member's schema says nothing an answer must meet beyond being an
/// object, if that: `{}`, `{type: object}`, `properties: {}`, `additionalProperties`
/// `true` or `{}`, or annotations only (`description`, `title`, `example`s). Beside
/// another branch's schema for the same member, it adds no constraint.
fn unconstrained(schema: &Value, defs: &Map<String, Value>) -> bool {
    const ANNOTATIONS: [&str; 6] = [
        "description",
        "title",
        "example",
        "examples",
        "default",
        "nullable",
    ];
    let Some(schema) = resolve(schema, defs).and_then(Value::as_object) else {
        return false;
    };
    schema.iter().all(|(key, value)| match key.as_str() {
        "type" => value == "object",
        "additionalProperties" => {
            value == &Value::Bool(true) || value.as_object().is_some_and(Map::is_empty)
        }
        "properties" => value.as_object().is_some_and(Map::is_empty),
        key => ANNOTATIONS.contains(&key),
    })
}

pub(crate) fn infer_identity(
    _kind: &OperationKind,
    path: &str,
    success_schema: Option<&Value>,
    defs: &Map<String, Value>,
) -> Option<Identity> {
    // The contract's keys are known once every route is compiled:
    // [`apply_action_routes`] goes over what has none with them.
    identity_for(trailing_param(path), success_schema, defs, &Keys::new())
}

/// The identity of a resource shaped like `schema`, addressed by `path_param`: its
/// `id`, else the member the parameter names ([`param_field`]). With no parameter,
/// the one scalar member that is one of the contract's `keys`; with none of those,
/// an [`ID_FALLBACKS`] member, and with two, none.
fn identity_for(
    path_param: Option<&str>,
    success_schema: Option<&Value>,
    defs: &Map<String, Value>,
    keys: &Keys,
) -> Option<Identity> {
    identity_at(path_param, success_schema, defs, keys)
}

/// [`identity_for`]'s rule, which [`item_identity`] also applies to an item.
fn identity_at(
    path_param: Option<&str>,
    success_schema: Option<&Value>,
    defs: &Map<String, Value>,
    keys: &Keys,
) -> Option<Identity> {
    let props = schema_properties(success_schema, defs)?;
    let props = props.as_ref();
    let named = path_param.and_then(|p| param_field(p, props, defs));
    let field = if props.contains_key("id") {
        "id"
    } else if let Some(named) = named {
        named
    } else if path_param.is_none() {
        let mut keyed = props
            .iter()
            .filter(|(member, schema)| keys.contains(*member) && is_scalar_member(schema, defs))
            .map(|(member, _)| member.as_str());
        match (keyed.next(), keyed.next()) {
            (Some(key), None) => key,
            (None, _) => ID_FALLBACKS.iter().find_map(|f| property(props, f))?,
            (Some(_), Some(_)) => return None,
        }
    } else {
        return None;
    };
    Some(Identity {
        path_param: path_param.map(str::to_owned),
        field: field.to_owned(),
        input_field: None,
        path_field: named.filter(|n| *n != field).map(str::to_owned),
        record: None,
    })
}

/// The one scalar member of `props` named for its own type, as `user_id` is for
/// `users` and `category_id` for `categories`. The mock already reads `<x>_id` as a
/// reference to `<x>s`, and a list's items as keyed by it, so on a record of that type
/// it can only be the record's own key. An object or a list holds no key.
fn own_type_member<'a>(
    resource_type: &str,
    props: &'a Map<String, Value>,
    defs: &Map<String, Value>,
) -> Option<&'a str> {
    let own = mb_mcm::stem(resource_type);
    let named = |member: &&String| {
        let lower = member.to_ascii_lowercase();
        let name = lower
            .strip_suffix("_id")
            .or_else(|| lower.strip_suffix("id"));
        name.is_some_and(|name| !name.is_empty() && mb_mcm::stem(name) == own)
    };
    let mut members = props.keys().filter(named);
    let (Some(field), None) = (members.next(), members.next()) else {
        return None;
    };
    is_scalar_member(props.get(field)?, defs).then_some(field.as_str())
}

/// Whether a path parameter's name says it holds an id: `id`, `orderId`, `movie_id`.
fn id_like(param: &str) -> bool {
    param.eq_ignore_ascii_case("id") || param.ends_with("Id") || param.ends_with("_id")
}

/// Gives what has no identity the member named for its own type, on a type routes
/// reach by a last path parameter, each of which says it holds an id. Another
/// parameter (`/users/{handle}`) names something else, and the answer's member would be
/// rewritten to its value. With no such route (`POST /tokens`) nothing could find the
/// record again.
fn apply_own_type_members(operations: &mut [Operation], defs: &Map<String, Value>) {
    let mut by_id = HashSet::new();
    let mut by_other = HashSet::new();
    for op in operations.iter().filter(|op| !is_stream(op)) {
        let (Some(t), Some(param)) = (
            op.resource_type.clone(),
            http_path(op).and_then(|(_, path)| trailing_param(path)),
        ) else {
            continue;
        };
        if id_like(param) {
            by_id.insert(t);
        } else {
            by_other.insert(t);
        }
    }
    for op in operations.iter_mut().filter(|op| op.identity.is_none()) {
        let Some(t) = op
            .resource_type
            .as_deref()
            .filter(|t| by_id.contains(*t) && !by_other.contains(*t))
        else {
            continue;
        };
        let Some((_, path)) = http_path(op) else {
            continue;
        };
        let Some(props) = schema_properties(answer(op), defs) else {
            continue;
        };
        let Some(field) = own_type_member(t, &props, defs) else {
            continue;
        };
        let identity = Identity {
            path_param: trailing_param(path).map(str::to_owned),
            field: field.to_owned(),
            input_field: None,
            path_field: None,
            record: None,
        };
        op.identity = Some(identity);
    }
}

/// The members a schema requires at its top level, with those of its `allOf` branches.
fn required_members(schema: &Value, defs: &Map<String, Value>) -> HashSet<String> {
    let mut levels = vec![schema];
    levels.extend(all_of_branches(schema, defs));
    levels
        .into_iter()
        .filter_map(|level| resolve(level, defs)?.get("required")?.as_array())
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// The item schema of an answer that is an array.
fn array_items<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> Option<&'a Value> {
    let schema = resolve(schema, defs)?;
    let is_array = schema.get("type").and_then(Value::as_str) == Some("array");
    resolve(schema.get("items").filter(|_| is_array)?, defs)
}

/// Gives a read answering an array of items a path parameter names a required member
/// of (SendGrid's `GET /bounces/{email}` over bounces requiring `email`) that member as
/// its identity, the record being the array's one item: the route is a read of that
/// record, filtered by it. A parameter names a member by its name, in any case, or by
/// their shared [`mb_mcm::stem`]. The routes of the type that end in that parameter and
/// answer nothing (its `DELETE`) go by it too.
fn apply_item_members(operations: &mut [Operation], defs: &Map<String, Value>) {
    let mut found: Vec<(String, String, String)> = Vec::new();
    for op in operations.iter_mut() {
        let (true, None, Some(t)) = (
            op.kind == OperationKind::Query && !is_stream(op),
            op.identity.as_ref(),
            op.resource_type.clone(),
        ) else {
            continue;
        };
        let Some(param) = http_path(op).and_then(|(_, path)| trailing_param(path)) else {
            continue;
        };
        let param = param.to_owned();
        let Some(items) = answer(op).and_then(|a| array_items(a, defs)) else {
            continue;
        };
        let (Some(props), required) = (
            schema_properties(Some(items), defs),
            required_members(items, defs),
        ) else {
            continue;
        };
        let names = |member: &&String| {
            required.contains(*member)
                && (member.eq_ignore_ascii_case(&param)
                    || mb_mcm::stem(member) == mb_mcm::stem(&param))
        };
        let mut named = props.keys().filter(names);
        let (Some(member), None) = (named.next(), named.next()) else {
            continue;
        };
        // The member the path names is the key, whatever else the item has: the
        // record is stored, found and forced by it, as on the type's other routes.
        let member = member.clone();
        op.identity = Some(Identity {
            path_param: Some(param.clone()),
            field: member.clone(),
            input_field: None,
            path_field: None,
            record: Some(mb_mcm::RecordAt::Item),
        });
        found.push((t, param, member));
    }
    for op in operations
        .iter_mut()
        .filter(|op| op.identity.is_none() && !is_stream(op) && answer(op).is_none())
    {
        let Some(param) = http_path(op).and_then(|(_, path)| trailing_param(path)) else {
            continue;
        };
        let same = |(t, p, _): &&(String, String, String)| {
            op.resource_type.as_deref() == Some(t.as_str()) && p == param
        };
        if let Some((_, param, member)) = found.iter().find(same) {
            op.identity = Some(Identity {
                path_param: Some(param.clone()),
                field: member.clone(),
                input_field: None,
                path_field: None,
                record: None,
            });
        }
    }
}

/// The record an envelope holds, as its member and its schema: the one member of an
/// object with no identity of its own, when that member is an object with one
/// (`{payment: Payment}`), or of several members the one that is exactly one of
/// `records`, the schemas its type's routes answer as a record (`{ticket, audit}`).
fn envelope_record(
    schema: &Value,
    records: &[&Value],
    defs: &Map<String, Value>,
    keys: &Keys,
) -> Option<(String, Value)> {
    if identity_for(None, Some(schema), defs, keys).is_some() {
        return None;
    }
    let props = schema_properties(Some(schema), defs)?;
    let is_record = |member: &Value| {
        schema_properties(Some(member), defs).is_some_and(|p| !p.is_empty())
            && identity_for(None, Some(member), defs, keys).is_some()
    };
    if props.len() == 1 {
        let (member, inner) = props.iter().next()?;
        let inner = resolve(inner, defs)?;
        return is_record(inner).then(|| (member.clone(), inner.clone()));
    }
    let mut held = props.iter().filter_map(|(member, inner)| {
        let inner = resolve(inner, defs)?;
        records.contains(&inner).then_some((member, inner))
    });
    let (Some((member, inner)), None) = (held.next(), held.next()) else {
        return None;
    };
    is_record(inner).then(|| (member.clone(), inner.clone()))
}

/// Sees through an envelope ([`envelope_record`]) to the record it holds: a route with
/// no identity, answering its record in one, takes the record's identity, with the
/// member it is under. Square's `GET /payments/{payment_id}` answering `{payment}`
/// reads payment `id`; Zendesk's `PUT /tickets/{id}` answering `{ticket, audit}` writes
/// the ticket, as `ticket` is what the type's other routes answer.
fn apply_envelopes(operations: &mut [Operation], defs: &Map<String, Value>, keys: &TypeKeys) {
    let mut records: HashMap<String, Vec<Value>> = HashMap::new();
    for op in operations.iter().filter(|op| !is_stream(op)) {
        let (Some(t), Some(schema)) = (op.resource_type.as_deref(), answer(op)) else {
            continue;
        };
        let held = records.entry(t.to_owned()).or_default();
        if let Some((_, inner)) = envelope_record(schema, &[], defs, keys.of(Some(t))) {
            held.push(inner);
        }
        if op.kind == OperationKind::Query {
            held.extend(
                listed_things(schema, defs)
                    .into_iter()
                    .filter(|item| !is_scalar_schema(item)),
            );
            if op.identity.as_ref().is_some_and(|i| i.path_param.is_some())
                && let Some(record) = resolve(schema, defs)
            {
                held.push(record.clone());
            }
        }
    }
    for op in operations.iter_mut() {
        let writes = matches!(
            op.kind,
            OperationKind::Query | OperationKind::Command(Effect::Create | Effect::Update)
        );
        let (true, None, Some(t)) = (
            writes && !is_stream(op),
            op.identity.as_ref(),
            op.resource_type.as_deref(),
        ) else {
            continue;
        };
        let known: Vec<&Value> = records.get(t).into_iter().flatten().collect();
        let keys = keys.of(Some(t));
        let Some((member, inner)) = answer(op).and_then(|a| envelope_record(a, &known, defs, keys))
        else {
            continue;
        };
        let path_param = http_path(op).and_then(|(_, path)| trailing_param(path));
        if let Some(mut identity) = identity_for(path_param, Some(&inner), defs, keys) {
            identity.record = Some(mb_mcm::RecordAt::Member(member));
            op.identity = Some(identity);
        }
    }
}

/// What a member's schema holds, as far as an envelope is concerned.
#[derive(Debug, Clone, PartialEq)]
enum Held {
    /// A single value: a string, a number, a boolean, or nothing declared.
    Value,
    /// A list.
    List,
    /// An object, with its members when one schema declares them.
    Object(Option<Map<String, Value>>),
}

/// Whether a schema is only `null`: an `anyOf` branch that makes a member nullable.
fn is_null_schema(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("null")
        || schema
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|e| e.iter().all(Value::is_null))
}

/// What `schema` holds ([`Held`]), through `$ref`s, `allOf`, `anyOf` and `oneOf` (their
/// `null` branches aside) and a list of types: an object nullable by any of these is
/// an object, and a choice between two objects is one with no members to go by.
fn held(schema: &Value, defs: &Map<String, Value>) -> Held {
    held_at(schema, defs, 0)
}

fn held_at(schema: &Value, defs: &Map<String, Value>, depth: usize) -> Held {
    let Some(schema) = resolve(schema, defs).filter(|_| depth < 16) else {
        return Held::Value;
    };
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if types.contains(&"array") || schema.get("items").is_some() {
        return Held::List;
    }
    let mut branches: Vec<Held> = Vec::new();
    for key in ["anyOf", "oneOf"] {
        let choices = schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        branches.extend(
            choices
                .filter(|c| resolve(c, defs).is_some_and(|c| !is_null_schema(c)))
                .map(|c| held_at(c, defs, depth + 1)),
        );
    }
    let parts: Vec<Held> = schema
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|part| held_at(part, defs, depth + 1))
        .collect();
    if branches.contains(&Held::List) || parts.contains(&Held::List) {
        return Held::List;
    }
    let objects: Vec<&Held> = branches
        .iter()
        .filter(|b| matches!(b, Held::Object(_)))
        .collect();
    let own = types.contains(&"object")
        || schema.get("properties").is_some()
        || parts.iter().any(|p| matches!(p, Held::Object(_)));
    match (own, objects.as_slice()) {
        (false, []) => Held::Value,
        (false, [one]) if branches.len() == 1 => (*one).clone(),
        (true, []) => Held::Object(schema_properties(Some(schema), defs).map(Cow::into_owned)),
        _ => Held::Object(None),
    }
}

/// Whether a member is a single value ([`Held::Value`]): only such a member can be a
/// key, or sit beside a record in its envelope.
fn holds_value(schema: &Value, defs: &Map<String, Value>) -> bool {
    held(schema, defs) == Held::Value
}

/// The record of an envelope of several members that only its structure tells: the
/// one member that holds an object with members, every other member a single value
/// (`{code, message, invoice: {...}}`, `{success, data: {...}}`,
/// `{item: {...}, request_id}`). Two objects, or a list beside it, leave it unknown.
/// It is the member, its schema, and the record's members.
fn wrapped_member(
    schema: &Value,
    defs: &Map<String, Value>,
) -> Option<(String, Value, Map<String, Value>)> {
    let props = schema_properties(Some(schema), defs)?;
    if props.len() < 2 {
        return None;
    }
    let mut others = props.iter().filter(|(_, inner)| !holds_value(inner, defs));
    let (Some((member, inner)), None) = (others.next(), others.next()) else {
        return None;
    };
    let Held::Object(Some(record)) = held(inner, defs) else {
        return None;
    };
    (!record.is_empty()).then(|| (member.clone(), inner.clone(), record))
}

/// The [`mb_mcm::stem`] of a name, separators and case aside.
fn name_stem(name: &str) -> String {
    mb_mcm::stem(&mb_mcm::joined_name(name))
}

/// The [`name_stem`] of the words of `name` before a last `id` (`invoice_id`,
/// `invoiceId`, `InvoiceID`: `invoice`), when it ends in one after other words.
fn id_owner(name: &str) -> Option<String> {
    let words = mb_mcm::words(name);
    let (last, owner) = words.split_last()?;
    (last.eq_ignore_ascii_case("id") && !owner.is_empty()).then(|| name_stem(&owner.concat()))
}

/// The record's `id`, when it is a single value.
fn record_id(record: &Map<String, Value>, defs: &Map<String, Value>) -> Option<&'static str> {
    record
        .get("id")
        .is_some_and(|s| holds_value(s, defs))
        .then_some("id")
}

/// The one single-value member of `record` named as the key of the route's type: an
/// `<x>_id` or `<x>Id` whose `<x>` has the [`name_stem`] of `resource_type`
/// (`invoice_id` on `invoices`). The envelope's member names no key: under `request`,
/// `request_id` is the request's. When the record also has a key named for that
/// member, of another stem than the type (`{payment: {payment_id, invoice_id}}` on
/// `invoices`), the record is the member's, and the type's key refers to it.
fn owned_key<'a>(
    record: &'a Map<String, Value>,
    member: &str,
    resource_type: &str,
    defs: &Map<String, Value>,
) -> Option<&'a str> {
    let (own, held_as) = (name_stem(resource_type), name_stem(member));
    let named_for = |stem: &str| {
        record
            .iter()
            .filter(|(name, schema)| {
                id_owner(name).as_deref() == Some(stem) && holds_value(schema, defs)
            })
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
    };
    if held_as != own && !named_for(&held_as).is_empty() {
        return None;
    }
    match named_for(&own).as_slice() {
        [key] => Some(key),
        _ => None,
    }
}

/// The member of `record` path parameter `param` names, a single value: the one of its
/// name, else of the longest group of its last [`mb_mcm::words`] ([`param_field`]'s
/// reading). A group short of the whole name leaves words before it, which must be
/// named for `member` or `resource_type`: `{order_number}` names a `number` under
/// `order`, not a `customer`'s; `{environment_id}` an `environment`'s `id`.
fn wrapped_param_member<'a>(
    param: &str,
    member: &str,
    resource_type: &str,
    record: &'a Map<String, Value>,
    defs: &Map<String, Value>,
) -> Option<&'a str> {
    let value = |name: &&String| record.get(*name).is_some_and(|s| holds_value(s, defs));
    if let Some((name, _)) = record
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(param) && value(name))
    {
        return Some(name.as_str());
    }
    let words = mb_mcm::words(param);
    (0..words.len()).find_map(|from| {
        let group = mb_mcm::joined_name(&words[from..].concat());
        let (name, _) = record
            .iter()
            .find(|(name, _)| mb_mcm::joined_name(name) == group && value(name))?;
        let owner = name_stem(&words[..from].concat());
        (from == 0 || owner == name_stem(member) || owner == name_stem(resource_type))
            .then_some(name.as_str())
    })
}

/// The identity of a route ending in path parameter `param` whose answer holds
/// `record` under `member` ([`wrapped_member`]), of type `resource_type`. A bare `{id}`
/// reads the record's `id`, else its [`owned_key`]. Another parameter must name a member
/// of the record ([`wrapped_param_member`]). Then the record goes by its `id` when it
/// has one, the path being the member it names, as [`identity_for`] has it. A
/// parameter that names nothing gives no identity.
fn wrapped_by_path(
    param: &str,
    member: &str,
    resource_type: &str,
    record: &Map<String, Value>,
    defs: &Map<String, Value>,
) -> Option<Identity> {
    let (field, named) = if mb_mcm::is_generic_id(param) {
        let field =
            record_id(record, defs).or_else(|| owned_key(record, member, resource_type, defs))?;
        (field, None)
    } else {
        let named = wrapped_param_member(param, member, resource_type, record, defs)?;
        (record_id(record, defs).unwrap_or(named), Some(named))
    };
    Some(Identity {
        path_param: Some(param.to_owned()),
        field: field.to_owned(),
        input_field: None,
        path_field: named.filter(|n| *n != field).map(str::to_owned),
        record: None,
    })
}

/// The members a request body sets, through `$ref`s, `allOf` and the `null`-less
/// branches of `anyOf` and `oneOf` (any of which a request may send). `None` when a
/// part of it cannot be read as members: a list, a single value, an object that
/// declares none.
fn body_members(
    schema: &Value,
    defs: &Map<String, Value>,
    depth: usize,
) -> Option<HashSet<String>> {
    let schema = resolve(schema, defs).filter(|_| depth < 16)?;
    if held(schema, defs) == Held::List {
        return None;
    }
    let mut members: HashSet<String> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    for key in ["allOf", "anyOf", "oneOf"] {
        let parts = schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for part in parts {
            if resolve(part, defs).is_some_and(is_null_schema) {
                continue;
            }
            match body_members(part, defs, depth + 1) {
                Some(part) => members.extend(part),
                // An `allOf` part may only constrain (`{required: [...]}`); a body
                // is one of its `anyOf` or `oneOf` branches, each of which must read.
                None if key == "allOf" && held(part, defs) != Held::List => {}
                None => return None,
            }
        }
    }
    // An object that declares no members (a free-form `PATCH`, a merge patch) is
    // read as setting none: it says nothing either way. A single value is not read.
    (!members.is_empty() || matches!(held(schema, defs), Held::Object(_))).then_some(members)
}

/// What a type's writes say of the object an envelope holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    /// No create or update of the type sends a body.
    Nothing,
    /// A body that cannot be read as members: nothing can be told.
    Unreadable,
    /// A body sets a member of the envelope's own: the envelope is the record.
    Envelope,
    /// Every body sets more members of the object than not, and none of the
    /// envelope's own.
    Object,
    /// Neither: bodies that set none of the envelope's own, nor mostly the object's.
    Unclear,
}

/// What `bodies` (each `None` where unreadable) say of `record`, held under `member`
/// of an envelope with the members `envelope`. An envelope member the record also has
/// (`status` beside `data: {status}`) is not the envelope's own: the body may set the
/// record's.
fn sent_to(
    bodies: &[Option<HashSet<String>>],
    envelope: &[String],
    member: &str,
    record: &Map<String, Value>,
) -> Sent {
    if bodies
        .iter()
        .all(|b| b.as_ref().is_some_and(HashSet::is_empty))
    {
        return Sent::Nothing;
    }
    let own: Vec<&String> = envelope
        .iter()
        .filter(|m| *m != member && !record.contains_key(*m))
        .collect();
    let mut mostly_object = true;
    for body in bodies {
        let Some(body) = body else {
            return Sent::Unreadable;
        };
        if body.is_empty() {
            continue;
        }
        if own.iter().any(|m| body.contains(*m)) {
            return Sent::Envelope;
        }
        let held = body.iter().filter(|m| record.contains_key(*m)).count();
        mostly_object &= held * 2 > body.len();
    }
    if mostly_object {
        Sent::Object
    } else {
        Sent::Unclear
    }
}

/// Gives a route that still has no identity, answering its record in an envelope of
/// several members that only structure tells ([`wrapped_member`]), the record's key,
/// with the member it is under. Structure alone does not say the object is the
/// route's record and not a related one (`{order_number, customer: {id}}`) or a part
/// of the answer (`{count, page: {id}}`), so it takes one of two corroborations, and
/// never against what the type's writes send ([`Sent::Envelope`], or a body it cannot
/// read):
///
/// - The writes' bodies set mostly the object's members ([`Sent::Object`]), and two
///   or more of the type's routes, one a `GET` by path, answer the same schema under
///   the same member, and another type's routes answer an envelope of the same
///   single values beside its object: the contract's convention, not one type's
///   record holding a related one.
/// - The member is named for the route's type (`invoice` on `invoices`) and the object
///   carries a key: on a route by path or a write, its `id` or one named for the type
///   ([`owned_key`]); on a query with no path parameter, only the latter, since a
///   page or a cursor named for its type has an `id` too.
///
/// A route ending in a path parameter then goes by what [`wrapped_by_path`] finds. One
/// with none goes by the key the type's routes by path read the same schema under the
/// same member by; when they find none, or disagree, it has none either. With no such
/// route, the record's `id`, else its [`owned_key`].
fn apply_wrapped_records(operations: &mut [Operation], defs: &Map<String, Value>) {
    type Wrapped = (String, String, String);
    struct Found {
        key: Wrapped,
        param: Option<String>,
        record: Map<String, Value>,
        envelope: Vec<String>,
    }
    impl Found {
        /// The envelope's members beside the object, sorted.
        fn beside(&self) -> Vec<String> {
            let mut beside: Vec<String> = self
                .envelope
                .iter()
                .filter(|m| **m != self.key.1)
                .cloned()
                .collect();
            beside.sort();
            beside
        }
    }
    let wrapped = |op: &Operation| -> Option<Found> {
        if is_stream(op) {
            return None;
        }
        let (_, path) = http_path(op)?;
        let t = op.resource_type.clone()?;
        let answer = answer(op)?;
        let (member, inner, record) = wrapped_member(answer, defs)?;
        let envelope = schema_properties(Some(answer), defs)?
            .keys()
            .cloned()
            .collect();
        Some(Found {
            key: (t, member, inner.to_string()),
            param: trailing_param(path).map(str::to_owned),
            record,
            envelope,
        })
    };
    let writes = |op: &Operation| {
        matches!(
            op.kind,
            OperationKind::Command(Effect::Create | Effect::Update)
        )
    };
    // The members each type's writes send in their request bodies.
    let mut sent: HashMap<String, Vec<Option<HashSet<String>>>> = HashMap::new();
    for op in operations
        .iter()
        .filter(|op| !is_stream(op) && http_path(op).is_some() && writes(op))
    {
        if let (Some(t), Some(body)) = (op.resource_type.clone(), op.input.body.as_ref()) {
            sent.entry(t).or_default().push(body_members(body, defs, 0));
        }
    }
    // How many of each type's routes answer each envelope, and whether a `GET` by
    // path is one of them.
    let mut answered: HashMap<Wrapped, (usize, bool)> = HashMap::new();
    for op in operations.iter() {
        let Some(found) = wrapped(op) else {
            continue;
        };
        let read_by_path =
            found.param.is_some() && http_path(op).is_some_and(|(m, _)| m == HttpMethod::Get);
        let seen = answered.entry(found.key).or_default();
        seen.0 += 1;
        seen.1 |= read_by_path;
    }
    // The types whose routes answer each envelope shape: the names of the single
    // values beside its object. `{success, data}` or `{code, message, X}` is the
    // contract's convention when several types answer it; `{order_number, status,
    // customer}`, one type's alone, is that type's record holding a related one.
    let mut conventions: HashMap<Vec<String>, HashSet<String>> = HashMap::new();
    for op in operations.iter() {
        if let Some(found) = wrapped(op) {
            conventions
                .entry(found.beside())
                .or_default()
                .insert(found.key.0.clone());
        }
    }
    let shared = |found: &Found| {
        conventions
            .get(&found.beside())
            .is_some_and(|types| types.iter().any(|t| *t != found.key.0))
    };
    let corroborated = |op: &Operation, found: &Found| {
        let (t, member, _) = &found.key;
        let bodies = sent.get(t).map(Vec::as_slice).unwrap_or_default();
        let told = sent_to(bodies, &found.envelope, member, &found.record);
        if matches!(told, Sent::Envelope | Sent::Unreadable) {
            return false;
        }
        let routes = told == Sent::Object
            && shared(found)
            && answered
                .get(&found.key)
                .is_some_and(|&(count, read)| count >= 2 && read);
        let typed = owned_key(&found.record, member, t, defs).is_some();
        let keyed = typed || record_id(&found.record, defs).is_some();
        let named = name_stem(member) == name_stem(t)
            && keyed
            && (found.param.is_some() || writes(op) || typed);
        routes || named
    };
    let applies = |op: &Operation| {
        op.identity.is_none()
            && matches!(
                op.kind,
                OperationKind::Query | OperationKind::Command(Effect::Create | Effect::Update)
            )
    };
    // What each type's routes by path read the record of an envelope by, per member
    // and schema; `None` where one finds no key or two disagree.
    let mut taught: HashMap<Wrapped, Option<String>> = HashMap::new();
    for op in operations.iter().filter(|op| applies(op)) {
        let Some(found) = wrapped(op).filter(|f| corroborated(op, f)) else {
            continue;
        };
        let Some(param) = &found.param else {
            continue;
        };
        let (t, member, _) = &found.key;
        let field = wrapped_by_path(param, member, t, &found.record, defs).map(|i| i.field);
        taught
            .entry(found.key)
            .and_modify(|known| {
                if *known != field {
                    *known = None;
                }
            })
            .or_insert(field);
    }
    for op in operations.iter_mut().filter(|op| applies(op)) {
        let Some(found) = wrapped(op).filter(|f| corroborated(op, f)) else {
            continue;
        };
        let (t, member, _) = &found.key;
        let record = &found.record;
        let identity = match &found.param {
            Some(param) => wrapped_by_path(param, member, t, record, defs),
            None => {
                let field = match taught.get(&found.key) {
                    Some(learned) => learned.clone(),
                    None => record_id(record, defs)
                        .or_else(|| owned_key(record, member, t, defs))
                        .map(str::to_owned),
                };
                field.map(|field| Identity {
                    path_param: None,
                    field,
                    input_field: None,
                    path_field: None,
                    record: None,
                })
            }
        };
        if let Some(mut identity) = identity {
            identity.record = Some(mb_mcm::RecordAt::Member(member.clone()));
            op.identity = Some(identity);
        }
    }
}

/// Whether a schema is of a value with no members to compare: a string, a number.
fn is_scalar_schema(schema: &Value) -> bool {
    matches!(
        schema.get("type").and_then(Value::as_str),
        Some("string" | "number" | "integer" | "boolean")
    )
}

/// Makes a lookup read as the type its items are: a read whose path has no
/// parameter, of a type no other route has, every listed array of which holds what
/// one type's `GET` by path serves (`/users-by-email` beside `/users/{id}`). So it sees
/// that type's records and what it lists goes under them. Another shape beside them
/// would be stored as that type, so it leaves the route as it was. So do a parameter,
/// under which the path decides who is listed, and two types serving the items.
///
/// Any other list read takes the type whose `GET` by path serves exactly what every
/// listed array holds, the same schema and not a shorter or longer form of it: Notion's
/// `POST /data_sources/{id}/query` lists pages, and Jira's `GET /search/jql` issues,
/// whatever other routes its own type has.
///
/// A create then takes the type that reads exactly what it stores ([`created_type`]).
fn apply_lookups(operations: &mut [Operation], hints: &[ResourceHint], defs: &Map<String, Value>) {
    for index in 0..hints.len() {
        let op = &operations[index];
        let is_list = hints[index].resource_type.is_none()
            && op.kind == OperationKind::Query
            && !is_stream(op);
        let alone = !operations
            .iter()
            .enumerate()
            .any(|(other, o)| other != index && o.resource_type == op.resource_type);
        let is_lookup = alone && http_path(op).is_some_and(|(_, path)| !path.contains('{'));
        // A record with its own identity is no list, whatever arrays it holds.
        if !is_list || !(is_lookup || op.identity.is_none()) {
            continue;
        }
        let Some(schema) = answer(op) else {
            continue;
        };
        let items: Vec<Value> = listed_things(schema, defs)
            .into_iter()
            .filter(|item| !is_scalar_schema(item))
            .collect();
        if items.is_empty() || items.iter().any(|item| is_opaque(Some(item), defs)) {
            continue;
        }
        let serves = |read: &Operation| {
            let Some(served) = record_served(read, defs) else {
                return false;
            };
            items.iter().all(|item| {
                item == &served
                    || (is_lookup && self::answer(read).is_some_and(|a| same_record(item, a, defs)))
            })
        };
        let mut types = gets(operations, |route| {
            route.last().is_some_and(|last| is_param(last))
        })
        .filter(|read| serves(read))
        .map(|read| read.resource_type.clone());
        let Some(found) = types.next() else {
            continue;
        };
        let one_type = types.all(|other| other == found);
        drop(types);
        if one_type {
            operations[index].resource_type = found;
        }
    }
    for index in 0..hints.len() {
        if let Some(found) = created_type(index, &hints[index], operations, defs) {
            operations[index].resource_type = Some(found);
        } else if let Some((found, identity)) =
            read_as_by_path(index, &hints[index], operations, defs)
        {
            let op = &mut operations[index];
            op.resource_type = Some(found);
            op.identity = Some(identity);
        }
    }
}

/// The type and identity of a create with no identity of its own whose answer is
/// exactly what one other type's `GET` by path answers, as its own type has no such
/// read: Asana's `POST /tasks/{task_gid}/subtasks` answers what `GET /tasks/{task_gid}`
/// does, so it creates a `tasks` record, keyed and held as that read's.
fn read_as_by_path(
    index: usize,
    hint: &ResourceHint,
    operations: &[Operation],
    defs: &Map<String, Value>,
) -> Option<(String, Identity)> {
    let op = &operations[index];
    let creates = hint.resource_type.is_none()
        && op.identity.is_none()
        && op.kind == OperationKind::Command(Effect::Create)
        && !is_stream(op);
    let answered = resolve(record_schema(op).filter(|_| creates)?, defs)?;
    if is_scalar_schema(answered) || is_opaque(Some(answered), defs) {
        return None;
    }
    let reads: Vec<&Operation> = gets(operations, |route| {
        route.last().is_some_and(|last| is_param(last))
    })
    .collect();
    if reads
        .iter()
        .any(|read| read.resource_type == op.resource_type)
    {
        return None;
    }
    let mut serving = reads.into_iter().filter(|read| {
        read.identity.is_some() && answer(read).and_then(|a| resolve(a, defs)) == Some(answered)
    });
    let read = serving.next()?;
    if serving.any(|other| other.resource_type != read.resource_type) {
        return None;
    }
    let known = read.identity.as_ref()?;
    let identity = Identity {
        path_param: None,
        field: known.field.clone(),
        input_field: None,
        path_field: None,
        record: known.record.clone(),
    };
    Some((read.resource_type.clone()?, identity))
}

/// The type a create at `index` stores its record as, when another type reads exactly
/// that schema and its own type has no `GET` by path: the type whose `GET` by path
/// serves it, else the one type whose lists list it and nothing else. So Freshdesk's
/// `POST /tickets/{id}/notes` and `/reply` store the `conversations` that
/// `GET /tickets/{id}/conversations` lists, Trello's comment is one of a card's
/// `actions`, and a subtask created under a task is a `tasks` record. Two such types
/// are not one, and a shorter or longer form of the record does not count.
fn created_type(
    index: usize,
    hint: &ResourceHint,
    operations: &[Operation],
    defs: &Map<String, Value>,
) -> Option<String> {
    let op = &operations[index];
    let creates = hint.resource_type.is_none()
        && op.kind == OperationKind::Command(Effect::Create)
        && !is_stream(op);
    let identity = op.identity.as_ref().filter(|_| creates)?;
    let record = record_held(record_schema(op)?, Some(identity), defs)?;
    if is_scalar_schema(&record) || is_opaque(Some(&record), defs) {
        return None;
    }
    let reads = || {
        gets(operations, |route| {
            route.last().is_some_and(|last| is_param(last))
        })
    };
    if reads().any(|read| read.resource_type == op.resource_type) {
        return None;
    }
    let one_type = |mut types: Vec<Option<String>>| {
        let first = types.pop()??;
        types
            .iter()
            .all(|t| t.as_deref() == Some(first.as_str()))
            .then_some(first)
    };
    let served = reads()
        .filter(|read| record_served(read, defs).as_ref() == Some(&record))
        .map(|read| read.resource_type.clone())
        .collect();
    let lists = |list: &&Operation| {
        let items: Vec<Value> = answer(list)
            .map(|a| listed_things(a, defs))
            .unwrap_or_default()
            .into_iter()
            .filter(|item| !is_scalar_schema(item))
            .collect();
        list.kind == OperationKind::Query
            && list.identity.is_none()
            && !is_stream(list)
            && !items.is_empty()
            && items.iter().all(|item| item == &record)
    };
    let listed = || {
        operations
            .iter()
            .filter(lists)
            .map(|list| list.resource_type.clone())
            .collect()
    };
    one_type(served).or_else(|| one_type(listed()))
}

/// The schema of the record a `GET` by path serves: its answer, or what its identity
/// says the answer holds it in.
fn record_served(read: &Operation, defs: &Map<String, Value>) -> Option<Value> {
    record_held(answer(read)?, read.identity.as_ref(), defs)
}

/// The schema of the record `schema` holds by `identity`: itself, the member of its
/// envelope, or its item.
fn record_held(
    schema: &Value,
    identity: Option<&Identity>,
    defs: &Map<String, Value>,
) -> Option<Value> {
    let served = resolve(schema, defs)?;
    match identity.and_then(|i| i.record.as_ref()) {
        None => Some(served.clone()),
        Some(mb_mcm::RecordAt::Member(member)) => {
            let props = schema_properties(Some(served), defs)?;
            resolve(props.get(member)?, defs).cloned()
        }
        Some(mb_mcm::RecordAt::Item) => array_items(served, defs).cloned(),
        Some(mb_mcm::RecordAt::MemberItem(member)) => {
            let props = schema_properties(Some(served), defs)?;
            array_items(props.get(member)?, defs).cloned()
        }
    }
}

/// The resource a route acts on, named by its path:
/// `POST /payment_intents/{intent}/confirm` acts on the `payment_intents`
/// named by `intent`.
#[derive(Debug, PartialEq)]
pub(crate) struct ParentTarget {
    resource_type: String,
    path_param: String,
    /// How many static segments follow the parameter.
    tail: usize,
}

/// The target of a path that ends `<static>/{param}/<static>...`. Two
/// parameters in a row name one resource together, so neither is its target.
fn parent_target(path: &str) -> Option<ParentTarget> {
    let segs = segments(path);
    let tail = segs.iter().rev().take_while(|s| !s.contains('{')).count();
    if tail == 0 {
        return None;
    }
    let [.., resource, param] = segs.get(..segs.len() - tail)? else {
        return None;
    };
    let path_param = param_name(param)?;
    (!resource.contains('{')).then(|| ParentTarget {
        resource_type: without_format(resource).to_owned(),
        path_param: path_param.to_owned(),
        tail,
    })
}

/// What the contract says of a `POST` to a [`parent_target`] route.
#[derive(Debug, PartialEq)]
enum Route {
    /// It acts on its parent.
    Action(ParentTarget),
    /// It adds a record of its own, of the type and identity given here
    /// where they are not what its path and compiled answer give.
    Collection {
        resource_type: Option<String>,
        identity: Option<Identity>,
    },
    /// The contract does not say: what the route's shape implies stands.
    Undecided,
}

/// Whether two routes are one, whatever they name their parameters and
/// whichever of them has a format suffix.
fn same_route(a: &[&str], b: &[&str]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| (is_param(a) && is_param(b)) || (without_format(a) == without_format(b)))
}

fn http_path(op: &Operation) -> Option<(HttpMethod, &str)> {
    match &op.binding {
        Binding::Http { method, path } => Some((*method, path)),
        _ => None,
    }
}

fn is_stream(op: &Operation) -> bool {
    matches!(
        op.kind,
        OperationKind::StreamServer | OperationKind::StreamClient | OperationKind::StreamBidi
    )
}

/// The `GET`s of `operations` on the route `route` selects.
fn gets(
    operations: &[Operation],
    route: impl Fn(&[&str]) -> bool,
) -> impl Iterator<Item = &Operation> {
    operations.iter().filter(move |op| {
        http_path(op)
            .is_some_and(|(method, path)| method == HttpMethod::Get && route(&segments(path)))
    })
}

fn answer(op: &Operation) -> Option<&Value> {
    op.success_output().and_then(|o| o.schema.as_ref())
}

/// Whether every success declares no body, so the client is told nothing and
/// could not read back anything made. `201` is left out: it says something
/// was made, and where may be in a header.
fn declares_no_body(op: &Operation) -> bool {
    let mut successes = op
        .outputs
        .iter()
        .filter_map(|o| match o.status {
            StatusSelector::Code(c) if (200..300).contains(&c) => Some((c, o)),
            _ => None,
        })
        .peekable();
    successes.peek().is_some()
        && successes.all(|(code, o)| code != 201 && o.schema.is_none() && o.content_type.is_none())
}

/// Whether a schema declares nothing to compare: no members and no typed
/// items.
fn is_opaque(schema: Option<&Value>, defs: &Map<String, Value>) -> bool {
    let has_members = |s: Option<&Value>| schema_properties(s, defs).is_some_and(|p| !p.is_empty());
    let Some(schema) = schema.and_then(|s| resolve(s, defs)) else {
        return true;
    };
    !has_members(Some(schema)) && !has_members(schema.get("items"))
}

/// What an answer is about: itself, and the object a one-member envelope
/// holds. A lone string is the answer's own member, not what it wraps.
fn answered_things(schema: &Value, defs: &Map<String, Value>) -> Vec<Value> {
    let mut things = Vec::new();
    if let Some(resolved) = resolve(schema, defs) {
        things.push(resolved.clone());
    }
    if let Some(props) = schema_properties(Some(schema), defs)
        && props.len() == 1
        && let Some(sole) = props.values().next().and_then(|v| resolve(v, defs))
        && schema_properties(Some(sole), defs).is_some_and(|p| !p.is_empty())
    {
        things.push(sole.clone());
    }
    things
}

/// The items a read lists: those of its array, or of an array member of its
/// envelope.
fn listed_things(schema: &Value, defs: &Map<String, Value>) -> Vec<Value> {
    let items_of = |s: &Value| {
        let items = resolve(s, defs)?.get("items")?;
        resolve(items, defs).cloned()
    };
    let mut things: Vec<Value> = items_of(schema).into_iter().collect();
    if let Some(props) = schema_properties(Some(schema), defs) {
        things.extend(props.values().filter_map(items_of));
    }
    things
}

/// Whether a read answering `read` gives back what `answer` is: the same
/// schema, or the same identity member, so a list of shorter items counts.
/// With nothing declared on either side there is nothing to tell them apart.
fn reads_back(
    read: Option<&Value>,
    answer: Option<&Value>,
    defs: &Map<String, Value>,
    keys: &Keys,
) -> bool {
    let (Some(read), Some(answer)) = (read, answer) else {
        return true;
    };
    if is_opaque(Some(read), defs) || is_opaque(Some(answer), defs) {
        return true;
    }
    let mut read_things = answered_things(read, defs);
    read_things.extend(listed_things(read, defs));
    let identity = |s: &Value| identity_for(None, Some(s), defs, keys).map(|i| i.field);
    answered_things(answer, defs).iter().any(|a| {
        read_things
            .iter()
            .any(|r| a == r || identity(a).is_some_and(|field| identity(r) == Some(field)))
    })
}

/// The `allOf` branches of `schema`, with those of its branches, resolved.
fn all_of_branches<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> Vec<&'a Value> {
    let mut branches = Vec::new();
    let mut pending = vec![schema];
    while let Some(next) = pending.pop() {
        let parts = resolve(next, defs)
            .and_then(|s| s.get("allOf"))
            .and_then(Value::as_array);
        for part in parts.into_iter().flatten() {
            if branches.len() >= MAX_ALL_OF_BRANCHES {
                return branches;
            }
            if let Some(part) = resolve(part, defs) {
                branches.push(part);
                pending.push(part);
            }
        }
    }
    branches
}

/// Whether two answers are one record: the same schema, through a one-member
/// envelope on either side, or one a shorter form the other composes with
/// `allOf`. Nothing here reads names.
fn same_record(a: &Value, b: &Value, defs: &Map<String, Value>) -> bool {
    let (a, b) = (answered_things(a, defs), answered_things(b, defs));
    let composes = |long: &Value, short: &Value| all_of_branches(long, defs).contains(&short);
    a.iter()
        .any(|a| b.iter().any(|b| a == b || composes(a, b) || composes(b, a)))
}

/// The type a `GET` by path of another resource than `target` serves `answer`
/// under, when one type does: that is the contract's statement that it is a
/// record there. The parent's own reads are what it acts on.
fn read_elsewhere<'a>(
    answer: &Value,
    target: &ParentTarget,
    operations: &'a [Operation],
    defs: &Map<String, Value>,
) -> Option<&'a Operation> {
    if is_opaque(Some(answer), defs) {
        return None;
    }
    let mut reads = gets(operations, |route| {
        route.last().is_some_and(|last| is_param(last))
    })
    .filter(|read| {
        read.resource_type.as_deref() != Some(&target.resource_type)
            && self::answer(read).is_some_and(|served| same_record(answer, served, defs))
    });
    let read = reads.next()?;
    reads
        .all(|other| other.resource_type == read.resource_type)
        .then_some(read)
}

/// The output of a declared `201`, which says the route made something,
/// whichever other successes it lists.
fn created_output(op: &Operation) -> Option<&OutputVariant> {
    op.outputs
        .iter()
        .find(|o| o.status == StatusSelector::Code(201))
}

/// What the route answers about what it made: its `201`, else its lowest
/// success.
fn record_schema(op: &Operation) -> Option<&Value> {
    created_output(op)
        .and_then(|o| o.schema.as_ref())
        .or_else(|| answer(op))
}

/// The identity of a record the route adds: the one the read that serves it
/// knows, with no path parameter since the route's parameter is its parent's;
/// else what its answer has.
fn record_identity(
    op: &Operation,
    read: Option<&Operation>,
    defs: &Map<String, Value>,
    keys: &Keys,
) -> Option<Identity> {
    let known = read.and_then(|r| r.identity.as_ref()).map(|i| Identity {
        path_param: None,
        field: i.field.clone(),
        input_field: None,
        path_field: None,
        record: None,
    });
    known.or_else(|| identity_for(None, record_schema(op), defs, keys))
}

/// The structural test of a `POST` to `<static>/{param}/<static>...`. When the
/// contract does not decide, it acts on the parent, which is harmless: the
/// answer is served and the parent changes only through validated effects.
/// The spelling of the path is not part of it (only [`READ_VERBS`] is, and that
/// yields to a declared body and to `x-mockingbird-resource`). In order:
///
/// 1. No success declares a body ([`declares_no_body`]): it acts on the parent.
///    Nothing below applies to a tail of two or more segments ([`Route::Undecided`]).
/// 2. A route below it (`<path>/{...}`), or a `GET` on its path that
///    [`reads_back`] its answer: a collection.
/// 3. The parent's `GET` answers the same record: it acts on the parent. The
///    runtime goes by the identity the answer carries.
/// 4. A `GET` by path elsewhere serves it ([`read_elsewhere`]): a record there.
/// 5. A declared `201`: a record of its own, which stores nothing with no identity.
/// 6. Otherwise it acts on the parent.
fn infer_route(
    op: &Operation,
    operations: &[Operation],
    defs: &Map<String, Value>,
    all_keys: &TypeKeys,
) -> Route {
    let keys = all_keys.of(op.resource_type.as_deref());
    let Some((HttpMethod::Post, path)) = http_path(op) else {
        return Route::Undecided;
    };
    let Some(target) = parent_target(path).filter(|_| !is_stream(op)) else {
        return Route::Undecided;
    };
    if declares_no_body(op) {
        return Route::Action(target);
    }
    if target.tail != 1 || ends_in_read(path) {
        return Route::Undecided;
    }
    let own = |resource_type, read| Route::Collection {
        resource_type,
        identity: record_identity(op, read, defs, keys),
    };
    let segs = segments(path);
    let has_detail = operations.iter().any(|other| {
        http_path(other).is_some_and(|(_, other)| {
            let other = segments(other);
            other.len() > segs.len()
                && same_route(&other[..segs.len()], &segs)
                && is_param(other[segs.len()])
        })
    });
    let answered = record_schema(op);
    if has_detail
        || gets(operations, |route| same_route(route, &segs))
            .any(|read| reads_back(answer(read), answered, defs, &all_keys.any))
    {
        return own(None, None);
    }
    if let Some(answered) = answered {
        let is_parents = parent_read(operations, path, target.tail)
            .and_then(answer)
            .is_some_and(|read| same_record(answered, read, defs));
        if is_parents {
            return Route::Action(target);
        }
        if let Some(read) = read_elsewhere(answered, &target, operations, defs) {
            return own(read.resource_type.clone(), Some(read));
        }
    }
    if created_output(op).is_some() {
        return own(None, None);
    }
    Route::Action(target)
}

/// The `GET` of the resource a route with a `tail` of static segments acts
/// on: `<static>/{param}`, whatever that route names its parameters.
fn parent_read<'a>(
    operations: &'a [Operation],
    action_path: &str,
    tail: usize,
) -> Option<&'a Operation> {
    let action = segments(action_path);
    let parent = action.get(..action.len().checked_sub(tail)?)?;
    gets(operations, |read| same_route(read, parent)).next()
}

/// Points the operation at `index` at its parent resource. Its kind is an
/// update when it answers the very schema of the parent's `GET`, and that
/// schema declares something; otherwise it stores nothing and the resource is
/// only its context.
fn act_on_parent(
    index: usize,
    target: ParentTarget,
    operations: &mut [Operation],
    defs: &Map<String, Value>,
    keys: &Keys,
) {
    let op = &operations[index];
    let read = http_path(op).and_then(|(_, path)| parent_read(operations, path, target.tail));
    let answered = answer(op);
    let replaces = !is_opaque(answered, defs) && answered == read.and_then(answer);
    let param = target.path_param;
    // The parent's record in the envelope its read answers it in: Square's
    // `POST /payments/{id}/complete` answering `{payment}`.
    let enveloped = read.and_then(|read| {
        let Some(mb_mcm::RecordAt::Member(member)) = read.identity.as_ref()?.record.as_ref() else {
            return None;
        };
        let record = record_served(read, defs)?;
        let (held, inner) = envelope_record(answered?, &[&record], defs, keys)?;
        (&held == member && inner == record).then_some(held)
    });
    let (effect, identity) = if let Some(member) = enveloped {
        let record = read.and_then(|read| record_served(read, defs));
        let mut identity =
            identity_for(Some(&param), record.as_ref(), defs, keys).unwrap_or_else(|| Identity {
                path_param: Some(param.clone()),
                field: "id".to_owned(),
                input_field: None,
                path_field: None,
                record: None,
            });
        identity.record = Some(mb_mcm::RecordAt::Member(member));
        (Effect::Update, identity)
    } else if replaces {
        let identity = identity_for(Some(&param), answered, defs, keys);
        let identity = identity.unwrap_or_else(|| Identity {
            path_param: Some(param.clone()),
            field: "id".to_owned(),
            input_field: None,
            path_field: None,
            record: None,
        });
        (Effect::Update, identity)
    } else {
        // The answer is not the resource, so the identity is the read's,
        // except that a member named for the parameter names the target.
        // A bare `{id}` names nothing there: an `id` is the answer's own.
        let read = read.and_then(|r| r.identity.as_ref());
        let names_target = !mb_mcm::is_generic_id(&param)
            && schema_properties(answered, defs).is_some_and(|p| p.contains_key(&param));
        let field = if names_target {
            param.clone()
        } else {
            read.map_or_else(|| "id".to_owned(), |r| r.field.clone())
        };
        let identity = Identity {
            path_param: Some(param),
            field,
            input_field: None,
            path_field: read.and_then(|r| r.path_field.clone()),
            record: None,
        };
        (Effect::Other, identity)
    };
    let op = &mut operations[index];
    op.kind = OperationKind::Command(effect);
    op.resource_type = Some(target.resource_type);
    op.identity = Some(identity);
}

/// The operation extension that corrects what inference decides.
const RESOURCE_EXT: &str = "x-mockingbird-resource";

/// The warning of a value of [`RESOURCE_EXT`] that is not used.
const INVALID_RESOURCE_EXT: &str = "invalid_resource_extension";

/// A resource type Mockingbird keeps for itself (`mb_state::INTERNAL_TYPE`): a
/// contract's resources under it would be hidden from exports and change feeds.
const RESERVED_TYPE: &str = "__mockingbird.earlier_requests";

/// The valid members of an operation's [`RESOURCE_EXT`].
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ResourceHint {
    resource_type: Option<String>,
    identity: Option<String>,
    action: Option<bool>,
}

/// Whether `name` can be a resource type: letters, digits, `_`, `-` and `.`,
/// starting with a letter, a digit or `_`.
fn is_type_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_alphanumeric() || c == '_')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Reads operation `op`'s [`RESOURCE_EXT`], with a warning for each value
/// that is not valid, which then does not count.
pub(crate) fn resource_hint(op: &Value, op_id: &str, ctx: &mut Ctx) -> ResourceHint {
    let mut hint = ResourceHint::default();
    let Some(ext) = op.get(RESOURCE_EXT) else {
        return hint;
    };
    let mut invalid = |what: String| {
        ctx.warn(
            INVALID_RESOURCE_EXT,
            Some(op_id),
            format!("operation {op_id}: {RESOURCE_EXT} {what}; it is ignored"),
        );
    };
    let Some(members) = ext.as_object() else {
        invalid("is not an object".to_owned());
        return hint;
    };
    for (name, value) in members {
        let text = || value.as_str().filter(|t| !t.is_empty()).map(str::to_owned);
        match name.as_str() {
            "type" if text().as_deref() == Some(RESERVED_TYPE) => {
                invalid(format!("member type names the reserved {RESERVED_TYPE}"));
            }
            "type" if text().is_some_and(|t| is_type_name(&t)) => hint.resource_type = text(),
            "identity" if text().is_some() => hint.identity = text(),
            "action" if value.is_boolean() => hint.action = value.as_bool(),
            "type" => invalid("member type is not an identifier-like name".to_owned()),
            "identity" => invalid("member identity is not a non-empty string".to_owned()),
            "action" => invalid("member action is not a boolean".to_owned()),
            _ => invalid(format!("has an unknown member {name}")),
        }
    }
    hint
}

/// What a declared `action: true` of `op` targets; a warning if it cannot
/// act on anything.
fn declared_target(op: &Operation, ctx: &mut Ctx) -> Option<ParentTarget> {
    let (method, path) = http_path(op)?;
    let reason = if is_stream(op) {
        "cannot make a stream an action"
    } else if matches!(
        method,
        HttpMethod::Get | HttpMethod::Head | HttpMethod::Options
    ) {
        "cannot make a read an action"
    } else {
        let target = parent_target(path);
        if target.is_some() {
            return target;
        }
        "action needs a path ending <static>/{param}/<static>"
    };
    ctx.warn(
        INVALID_RESOURCE_EXT,
        Some(&op.id),
        format!(
            "operation {}: {RESOURCE_EXT} {reason}; it is ignored",
            op.id
        ),
    );
    None
}

/// Whether `field` is a member of what `op` answers or, for an action, of
/// what it acts on. An answer that declares no members cannot contradict it.
fn identity_is_declared(
    field: &str,
    op: &Operation,
    operations: &[Operation],
    defs: &Map<String, Value>,
) -> bool {
    let has_it = |schema: Option<&Value>| {
        schema_properties(schema, defs).is_some_and(|p| p.contains_key(field))
    };
    let answers = [answer(op), record_schema(op)];
    if answers.iter().any(|a| has_it(*a))
        || answers
            .iter()
            .all(|a| schema_properties(*a, defs).is_none_or(|p| p.is_empty()))
    {
        return true;
    }
    http_path(op)
        .filter(|_| op.is_action())
        .and_then(|(_, path)| parent_read(operations, path, parent_target(path)?.tail))
        .is_some_and(|read| has_it(answer(read)))
}

/// Applies [`infer_route`] to every compiled operation, and over it the
/// operation's [`ResourceHint`] (`hints` runs parallel to `operations`). Reads
/// go first: the routes are judged by them, with their hints.
pub(crate) fn apply_action_routes(
    operations: &mut [Operation],
    hints: &[ResourceHint],
    defs: &Map<String, Value>,
    ctx: &mut Ctx,
) {
    let keys = record_keys(operations, defs);
    apply_keys(operations, defs, &keys);
    apply_envelopes(operations, defs, &keys);
    apply_item_envelopes(operations, defs);
    apply_item_members(operations, defs);
    apply_own_type_members(operations, defs);
    let is_read = |op: &Operation| http_path(op).is_some_and(|(m, _)| m == HttpMethod::Get);
    let mut order: Vec<usize> = (0..hints.len()).collect();
    order.sort_by_key(|&i| !is_read(&operations[i]));
    for index in order {
        let hint = &hints[index];
        let op = &operations[index];
        let own_keys = keys.of(op.resource_type.as_deref());
        let inferred = || infer_route(op, operations, defs, &keys);
        let route = match hint.action {
            Some(true) => declared_target(op, ctx).map_or_else(inferred, Route::Action),
            Some(false) => match inferred() {
                Route::Action(_) => Route::Collection {
                    resource_type: None,
                    identity: record_identity(op, None, defs, own_keys),
                },
                route => route,
            },
            None => inferred(),
        };
        let read_verb_write = hint.action == Some(false)
            && op.kind == OperationKind::Query
            && http_path(op).is_some_and(|(method, _)| method == HttpMethod::Post);
        let writes_its_own = !is_stream(op)
            && http_path(op).is_some_and(|(method, _)| method == HttpMethod::Post)
            && matches!(
                op.kind,
                OperationKind::Query | OperationKind::Command(Effect::Create)
            );
        if hint.action == Some(false) && !writes_its_own {
            let id = op.id.clone();
            ctx.warn(
                INVALID_RESOURCE_EXT,
                Some(&id),
                format!(
                    "operation {id}: {RESOURCE_EXT} action false applies to a POST that could write a record of its own; it is ignored"
                ),
            );
        }
        match route {
            Route::Action(target) => {
                let parent_keys = keys.of(Some(&target.resource_type)).clone();
                act_on_parent(index, target, operations, defs, &parent_keys);
            }
            Route::Collection {
                resource_type,
                identity,
            } => {
                let op = &mut operations[index];
                op.resource_type = resource_type.or(op.resource_type.take());
                // What its own answer holds its record in stands.
                let enveloped = op.identity.as_ref().is_some_and(|i| i.record.is_some());
                if !enveloped {
                    op.identity = identity.or(op.identity.take());
                }
            }
            Route::Undecided => {}
        }
        let op = &mut operations[index];
        if read_verb_write {
            op.kind = OperationKind::Command(Effect::Create);
            op.resource_type = http_path(op)
                .and_then(|(_, path)| segments(path).last().map(|s| without_format(s).to_owned()));
        }
        if let Some(resource_type) = &hint.resource_type {
            op.resource_type = Some(resource_type.clone());
        }
        if let Some(field) = &hint.identity {
            set_identity(index, field, operations, defs, ctx);
        }
    }
    apply_shared_identities(operations);
    apply_lookups(operations, hints, defs);
    apply_wrapped_records(operations, defs);
    apply_nested_identities(operations, defs);
}

/// Keys what no other rule gave an identity by a member of one of its object members:
/// a type whose reads by path (`GET /configmaps/{name}`) answer a record with no
/// top-level member the parameter names, and one object member (`metadata`) holding a
/// scalar member it names wholly ([`nested_key`]). The routes on that parameter go by
/// the dotted path (`metadata.name`), and a create of the type whose answer holds the
/// member by it too. Two reads that disagree key nothing. Only an object member that is
/// record metadata ([`Metadata`]) keys a record: a related record held inline
/// (`manager: Person`) does not.
fn apply_nested_identities(operations: &mut [Operation], defs: &Map<String, Value>) {
    let metadata = Metadata::of(operations, defs);
    let mut by_route: HashMap<(String, String), Option<String>> = HashMap::new();
    for op in gets(operations, |route| {
        route.last().is_some_and(|last| is_param(last))
    })
    .filter(|op| op.identity.is_none() && !is_stream(op))
    {
        let (Some(t), Some((_, path)), Some(schema)) =
            (op.resource_type.as_ref(), http_path(op), answer(op))
        else {
            continue;
        };
        let Some(param) = trailing_param(path).filter(|_| nests(path)) else {
            continue;
        };
        let Some(field) = nested_key(param, schema, &metadata, defs) else {
            continue;
        };
        by_route
            .entry((t.clone(), param.to_owned()))
            .and_modify(|known| {
                if known.as_ref() != Some(&field) {
                    *known = None;
                }
            })
            .or_insert(Some(field));
    }
    let mut by_type: HashMap<String, Option<String>> = HashMap::new();
    for ((t, _), field) in &by_route {
        by_type
            .entry(t.clone())
            .and_modify(|known| {
                if known != field {
                    *known = None;
                }
            })
            .or_insert_with(|| field.clone());
    }
    for op in operations
        .iter_mut()
        .filter(|op| op.identity.is_none() && !is_stream(op))
    {
        let (Some(t), Some((_, path))) = (op.resource_type.clone(), http_path(op)) else {
            continue;
        };
        let (param, field) = match trailing_param(path) {
            Some(param) => match by_route.get(&(t, param.to_owned())) {
                Some(Some(field)) => (Some(param.to_owned()), field.clone()),
                _ => continue,
            },
            None => match by_type.get(&t) {
                Some(Some(field))
                    if op.kind == OperationKind::Command(Effect::Create)
                        && schema_properties(answer(op), defs)
                            .is_some_and(|props| nested_member(field, &props, defs)) =>
                {
                    (None, field.clone())
                }
                _ => continue,
            },
        };
        op.identity = Some(Identity {
            path_param: param,
            field,
            input_field: None,
            path_field: None,
            record: None,
        });
    }
}

/// The dotted path (`metadata.name`) of the key of a record shaped like `schema` read
/// by path parameter `param`, when it has no `id`, no top-level member the parameter
/// names, and exactly one object member holding a scalar member the parameter names
/// wholly ([`param_match`] by all its words). A bare `{id}` names no such member: an
/// `id` in an object member is that object's own. Nor does a parameter that names the
/// object member itself (`{order_id}` and `order`): that member is the record, held in
/// an envelope, which the envelope rules judge. The object member must be record
/// metadata ([`Metadata::holds`]).
fn nested_key(
    param: &str,
    schema: &Value,
    metadata: &Metadata<'_>,
    defs: &Map<String, Value>,
) -> Option<String> {
    let props = schema_properties(Some(schema), defs)?;
    if mb_mcm::is_generic_id(param)
        || props.contains_key("id")
        || param_match(param, &props, defs).is_some()
    {
        return None;
    }
    let whole = mb_mcm::words(param).len();
    let mut found = props.iter().filter_map(|(member, inner)| {
        if mb_mcm::names_member(param, member) || !metadata.holds(member, inner) {
            return None;
        }
        let inner = resolve(inner, defs)?;
        if inner.get("type").and_then(Value::as_str) == Some("array") {
            return None;
        }
        let inner = schema_properties(Some(inner), defs)?;
        let (score, leaf) = param_match(param, &inner, defs)?;
        (score >= whole).then(|| format!("{member}.{leaf}"))
    });
    match (found.next(), found.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// The object members of records that are those records' metadata, as the shared
/// metadata convention (Kubernetes' `ObjectMeta`) has them: the same object, by the same
/// `$ref` or the same members, under the same member name in the records of two or more
/// types. One no other type holds may be a related record held inline (`manager:
/// Person`), so it is not metadata; nor is one that is itself a type's record, or that
/// has an `id` of its own.
struct Metadata<'a> {
    /// The (member, [`shape`]) pairs that two or more types' records hold.
    shared: HashSet<(String, String)>,
    /// The record schemas the operations read by path and create answer.
    records: Vec<&'a Value>,
    defs: &'a Map<String, Value>,
}

impl<'a> Metadata<'a> {
    fn of(operations: &'a [Operation], defs: &'a Map<String, Value>) -> Self {
        let mut types: HashMap<(String, String), HashSet<&str>> = HashMap::new();
        let mut records = Vec::new();
        let read_by_path = |op: &Operation| {
            http_path(op).is_some_and(|(method, path)| {
                method == HttpMethod::Get && trailing_param(path).is_some()
            })
        };
        for op in operations.iter().filter(|op| {
            !is_stream(op)
                && (read_by_path(op) || op.kind == OperationKind::Command(Effect::Create))
        }) {
            let Some(schema) = answer(op) else {
                continue;
            };
            if let Some(record) = resolve(schema, defs) {
                records.push(record);
            }
            let (Some(t), Some(props)) = (
                op.resource_type.as_deref(),
                schema_properties(Some(schema), defs),
            ) else {
                continue;
            };
            for (member, inner) in props.iter() {
                if let Some(shape) = shape(inner, defs) {
                    types.entry((member.clone(), shape)).or_default().insert(t);
                }
            }
        }
        let shared = types
            .into_iter()
            .filter(|(_, types)| types.len() >= 2)
            .map(|(key, _)| key)
            .collect();
        Self {
            shared,
            records,
            defs,
        }
    }

    /// Whether `member`, declared by `inner`, is record metadata.
    fn holds(&self, member: &str, inner: &Value) -> bool {
        let Some(shape) = shape(inner, self.defs) else {
            return false;
        };
        let Some(resolved) = resolve(inner, self.defs) else {
            return false;
        };
        self.shared.contains(&(member.to_owned(), shape))
            && !self.records.contains(&resolved)
            && !schema_properties(Some(resolved), self.defs).is_some_and(|p| p.contains_key("id"))
    }
}

/// What tells two object member schemas apart: the `$ref` naming it, else the members
/// it declares. A schema declaring no members has none.
fn shape(schema: &Value, defs: &Map<String, Value>) -> Option<String> {
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        return Some(format!("$ref {r}"));
    }
    let props = schema_properties(Some(schema), defs)?;
    let mut members: Vec<&str> = props.keys().map(String::as_str).collect();
    members.sort_unstable();
    (!members.is_empty()).then(|| format!("members {}", members.join(",")))
}

/// Whether dotted `key` (`metadata.name`) is a scalar member of an object member of
/// `props`, one step down.
fn nested_member(key: &str, props: &Map<String, Value>, defs: &Map<String, Value>) -> bool {
    let Some((outer, leaf)) = key.split_once('.') else {
        return false;
    };
    props
        .get(outer)
        .and_then(|outer| schema_properties(Some(outer), defs))
        .and_then(|inner| inner.get(leaf).map(|s| is_scalar_member(s, defs)))
        .unwrap_or(false)
}

/// Whether a record read by `path` may be keyed by a nested member ([`nested_key`]):
/// its trailing parameter does not name the collection it follows. Of
/// `/namespaces/{namespace}` the parameter names the record's own type (an object
/// metadata's `namespace` there is the scope its records refer to), while
/// `/configmaps/{name}` names a member.
fn nests(path: &str) -> bool {
    let segs = segments(path);
    let [.., collection, last] = segs.as_slice() else {
        return true;
    };
    let Some(param) = param_name(last) else {
        return false;
    };
    is_param(collection)
        || mb_mcm::stem(&without_format(collection).to_ascii_lowercase())
            != mb_mcm::stem(&param.to_ascii_lowercase())
}

/// Makes `field` the identity of the operation at `index`, unless it does
/// not declare that member.
fn set_identity(
    index: usize,
    field: &str,
    operations: &mut [Operation],
    defs: &Map<String, Value>,
    ctx: &mut Ctx,
) {
    let op = &operations[index];
    if !identity_is_declared(field, op, operations, defs) {
        let id = op.id.clone();
        ctx.warn(
            INVALID_RESOURCE_EXT,
            Some(&id),
            format!(
                "operation {id}: {RESOURCE_EXT} member identity names {field}, which it does not declare; it is ignored"
            ),
        );
        return;
    }
    let path_param = http_path(op)
        .and_then(|(_, path)| trailing_param(path))
        .map(str::to_owned);
    let op = &mut operations[index];
    match &mut op.identity {
        Some(identity) => {
            identity.field = field.to_owned();
            identity.path_field.take_if(|named| named == field);
        }
        None => {
            op.identity = Some(Identity {
                path_param,
                field: field.to_owned(),
                input_field: None,
                path_field: None,
                record: None,
            });
        }
    }
}

/// Verb prefixes of operation names, with the kind each implies.
const VERBS: [(&[&str], OperationKind); 4] = [
    (
        &["get", "list", "find", "search", "query", "fetch", "read"],
        OperationKind::Query,
    ),
    (
        &["create", "add", "insert", "new", "register"],
        OperationKind::Command(Effect::Create),
    ),
    (
        &["update", "set", "modify", "edit", "patch", "replace"],
        OperationKind::Command(Effect::Update),
    ),
    (
        &["delete", "remove", "cancel", "destroy"],
        OperationKind::Command(Effect::Delete),
    ),
];

/// The verb prefix of `name` and the kind it implies. The prefix matches
/// case-insensitively and must be followed by an uppercase letter, `_` or
/// the end of the name.
fn verb_prefix(name: &str) -> Option<(&str, &OperationKind)> {
    VERBS.iter().find_map(|(verbs, kind)| {
        verbs.iter().find_map(|verb| {
            let (head, rest) = name.split_at_checked(verb.len())?;
            let boundary = rest
                .chars()
                .next()
                .is_none_or(|c| c.is_ascii_uppercase() || c == '_');
            (head.eq_ignore_ascii_case(verb) && boundary).then_some((rest, kind))
        })
    })
}

/// The operation kind implied by a verb-prefixed name (`getUser`,
/// `create_order`), else `default`.
pub(crate) fn kind_from_name(name: &str, default: OperationKind) -> OperationKind {
    verb_prefix(name).map_or(default, |(_, kind)| *kind)
}

/// The resource a name refers to: the name without its verb prefix,
/// lowercased, with one trailing `s` (not `ss`) dropped; `None` if empty.
pub(crate) fn resource_from_name(name: &str) -> Option<String> {
    let rest = verb_prefix(name).map_or(name, |(rest, _)| rest);
    let mut resource = rest.trim_start_matches('_').to_lowercase();
    if resource.ends_with('s') && !resource.ends_with("ss") {
        resource.pop();
    }
    (!resource.is_empty()).then_some(resource)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_parent_target_is_a_static_and_a_param_before_a_static_tail() {
        for (path, resource, param, tail) in [
            (
                "/v1/payment_intents/{intent}/confirm",
                "payment_intents",
                "intent",
                1,
            ),
            ("/v1/customers/{id}/sources", "customers", "id", 1),
            ("/v1/Calls/{CallSid}/Feedback.json", "Calls", "CallSid", 1),
            ("/x/{id}/y/z", "x", "id", 2),
            ("/Calls.json/{Sid}.json/Feedback", "Calls", "Sid", 1),
        ] {
            assert_eq!(
                parent_target(path),
                Some(ParentTarget {
                    resource_type: resource.into(),
                    path_param: param.into(),
                    tail,
                }),
                "{path}"
            );
        }
        for path in [
            "/repos/{o}/{r}/merge",
            "/a/{x}/{verb}",
            "/{x}/confirm",
            "/a/{x}/confirm/{y}",
            "/a/{x}",
            "/confirm",
            "/",
        ] {
            assert_eq!(parent_target(path), None, "{path}");
        }
    }

    #[test]
    fn a_key_inside_a_segment_reads_as_whole_segment_parameters() {
        for (path, expected) in [
            (
                "/companies({company_id})/customers({customer_id})",
                &["companies", "{company_id}", "customers", "{customer_id}"][..],
            ),
            ("/customers('{id}')", &["customers", "{id}"]),
            ("/entity(k1={a},k2={b})/x", &["entity", "x"]),
            ("/lines({doc},{seq})", &["lines"]),
            ("/customers(id={id})", &["customers"]),
            (
                "/customers({id})/NS.getBalance(asOf={date})",
                &["customers", "{id}", "NS.getBalance"],
            ),
            (
                "/me/calendarView(startDateTime={s},endDateTime={e})",
                &["me", "calendarView"],
            ),
            ("/(a={a})", &["(a={a})"]),
            ("/carts/key={key}", &["carts", "{key}"]),
            ("/carts/key={key}.json", &["carts", "{key}"]),
            ("/({id})", &["{id}"]),
            // One segment, as before: a whole-segment parameter with or
            // without a format, a parameter with only a suffix, text that
            // says nothing of a key, and what is not a template.
            ("/files/{name}.json", &["files", "{name}.json"]),
            ("/items/{id}:archive", &["items", "{id}:archive"]),
            ("/pairs/{a}-{b}", &["pairs", "{a}-{b}"]),
            ("/api/v{version}/items", &["api", "v{version}", "items"]),
            ("/c({id}", &["c({id}"]),
            ("/c({id})x", &["c({id})x"]),
            ("/k=({id})", &["k=({id})"]),
            ("/a b={id}", &["a b={id}"]),
            ("/k={a},{b}", &["k={a},{b}"]),
            ("/c({{id}})", &["c({{id}})"]),
            ("/c({})", &["c({})"]),
            ("/c(}{id})", &["c(}{id})"]),
        ] {
            assert_eq!(segments(path), expected, "{path}");
        }
    }

    /// What every operation of a compiled contract is, by id.
    fn shapes(bundle: &mb_mcm::Bundle) -> Vec<(String, String, Option<String>, Option<Identity>)> {
        let mut shapes: Vec<_> = bundle
            .operations
            .iter()
            .map(|op| {
                (
                    op.id.clone(),
                    serde_json::to_string(&op.kind).unwrap(),
                    op.resource_type.clone(),
                    op.identity.clone(),
                )
            })
            .collect();
        shapes.sort_by(|a, b| a.0.cmp(&b.0));
        shapes
    }

    /// A contract whose keyed routes are `keyed(collection, param)`.
    fn keyed_contract(keyed: impl Fn(&str, &str) -> String) -> mb_mcm::Bundle {
        let record = |id: &str| json!({"$ref": format!("#/components/schemas/{id}")});
        let answers = |id: &str, schema: &str| answering(id, &record(schema));
        let mut paths = serde_json::Map::new();
        let company = keyed("companies", "company_id");
        let customers = format!("{company}/customers");
        let customer = keyed(&customers, "customer_id");
        paths.insert(company.clone(), json!({"parameters": path_param("company_id"), "get": answers("getCompany", "Company")}));
        let mut create = answers("createCustomer", "Customer");
        create["requestBody"] =
            json!({"content": {"application/json": {"schema": record("Customer")}}});
        paths.insert(
            customers,
            json!({"parameters": path_param("company_id"), "post": create}),
        );
        let both = json!([path_param("company_id")[0], path_param("customer_id")[0]]);
        paths.insert(
            customer.clone(),
            json!({
                "parameters": both,
                "get": answers("getCustomer", "Customer"),
                "patch": answers("updateCustomer", "Customer"),
                "delete": {"operationId": "deleteCustomer", "responses": {"204": {"description": "gone"}}},
            }),
        );
        paths.insert(
            format!("{customer}/NS.block"),
            json!({"parameters": both, "post": {"operationId": "blockCustomer", "responses": {"204": {"description": "done"}}}}),
        );
        paths.insert(
            keyed(&format!("{company}/vendors"), "vendor_id"),
            json!({"parameters": json!([path_param("company_id")[0], path_param("vendor_id")[0]]), "get": answers("getVendor", "Vendor")}),
        );
        compiled(
            &Value::Object(paths),
            &json!({
                "Company": {"type": "object", "properties": {"id": {"type": "string"}}},
                "Customer": {"type": "object", "properties": {"id": {"type": "string"}, "name": {"type": "string"}}},
                "Vendor": {"type": "object", "properties": {"vendor_id": {"type": "string"}}},
            }),
        )
    }

    #[test]
    fn an_odata_key_compiles_as_a_whole_segment_parameter_does() {
        let odata = keyed_contract(|collection, param| format!("{collection}({{{param}}})"));
        let whole = keyed_contract(|collection, param| format!("{collection}/{{{param}}}"));
        assert_eq!(shapes(&odata), shapes(&whole));
        let op = |id: &str| odata.operations.iter().find(|op| op.id == id).unwrap();
        for id in ["getCustomer", "updateCustomer", "blockCustomer"] {
            assert_eq!(op(id).resource_type.as_deref(), Some("customers"), "{id}");
            let identity = op(id).identity.clone().unwrap();
            assert_eq!(identity.path_param.as_deref(), Some("customer_id"), "{id}");
            assert_eq!(identity.field, "id", "{id}");
        }
        assert_eq!(
            op("deleteCustomer").resource_type.as_deref(),
            Some("customers")
        );
        assert_eq!(
            op("blockCustomer").kind,
            OperationKind::Command(Effect::Other)
        );
        assert!(op("blockCustomer").is_action());
        assert_eq!(op("getVendor").resource_type.as_deref(), Some("vendors"));
        assert_eq!(
            op("getVendor").identity.as_ref().map(|i| i.field.as_str()),
            Some("vendor_id")
        );
    }

    #[test]
    fn a_named_key_is_an_alternate_key_of_the_collection_before_it() {
        let bundle = compiled(
            &json!({
                "/carts/{id}": {"parameters": path_param("id"), "get": answering("getCart", &json!({"$ref": "#/components/schemas/Cart"}))},
                "/carts/key={key}": {"parameters": path_param("key"), "get": answering("getCartByKey", &json!({"$ref": "#/components/schemas/Cart"}))},
                "/files/{name}.json": {"parameters": path_param("name"), "get": answering("getFile", &json!({"$ref": "#/components/schemas/File"}))},
            }),
            &json!({
                "Cart": {"type": "object", "properties": {"id": {"type": "string"}, "key": {"type": "string"}}},
                "File": {"type": "object", "properties": {"name": {"type": "string"}}},
            }),
        );
        let op = |id: &str| bundle.operations.iter().find(|op| op.id == id).unwrap();
        assert_eq!(op("getCartByKey").resource_type.as_deref(), Some("carts"));
        assert_eq!(
            op("getCartByKey").identity,
            Some(Identity {
                path_param: Some("key".into()),
                field: "id".into(),
                input_field: None,
                path_field: Some("key".into()),
                record: None,
            })
        );
        assert_eq!(op("getFile").resource_type.as_deref(), Some("files"));
        assert_eq!(
            op("getFile").identity.clone().and_then(|i| i.path_param),
            Some("name".into())
        );
    }

    #[test]
    fn a_composite_key_or_a_call_names_no_identity() {
        let line = json!({"$ref": "#/components/schemas/Line"});
        let customer = json!({"$ref": "#/components/schemas/Customer"});
        let both = json!([path_param("documentId")[0], path_param("sequence")[0]]);
        let bundle = compiled(
            &json!({
                "/lines": {"post": answering("createLine", &line)},
                "/lines(documentId={documentId},sequence={sequence})": {
                    "parameters": both,
                    "get": answering("getLine", &line),
                    "patch": answering("updateLine", &line),
                    "delete": {"operationId": "deleteLine", "responses": {"204": {"description": "gone"}}},
                },
                "/customers({id})": {"parameters": path_param("id"), "get": answering("getCustomer", &customer)},
                "/customers({id})/NS.getBalance(asOf={date})": {
                    "parameters": [path_param("id")[0], path_param("date")[0]],
                    "get": answering("getBalance", &json!({"type": "object", "properties": {"balance": {"type": "number"}}})),
                },
                "/customers({id})/NS.post(on={date})": {
                    "parameters": [path_param("id")[0], path_param("date")[0]],
                    "post": {"operationId": "postCustomer", "responses": {"204": {"description": "done"}}},
                },
                "/me/calendarView(startDateTime={s},endDateTime={e})": {
                    "parameters": [path_param("s")[0], path_param("e")[0]],
                    "get": answering("calendarView", &json!({"type": "object", "properties": {"id": {"type": "string"}, "e": {"type": "string"}}})),
                },
            }),
            &json!({
                "Line": {"type": "object", "properties": {"documentId": {"type": "string"}, "sequence": {"type": "integer"}, "amount": {"type": "number"}}},
                "Customer": {"type": "object", "properties": {"id": {"type": "string"}}},
            }),
        );
        let op = |id: &str| bundle.operations.iter().find(|op| op.id == id).unwrap();
        for id in ["createLine", "getLine", "updateLine", "deleteLine"] {
            assert_eq!(op(id).resource_type.as_deref(), Some("lines"), "{id}");
            assert!(
                op(id)
                    .identity
                    .as_ref()
                    .is_none_or(|i| i.path_param.is_none()
                        && i.field != "sequence"
                        && i.field != "documentId"),
                "{id}: {:?}",
                op(id).identity
            );
        }
        // A call keys nothing by its arguments.
        for id in ["getBalance", "calendarView"] {
            assert!(
                op(id)
                    .identity
                    .as_ref()
                    .is_none_or(|i| i.path_param.is_none()),
                "{id}: {:?}",
                op(id).identity
            );
        }
        assert_eq!(
            op("calendarView").resource_type.as_deref(),
            Some("calendarView")
        );
        // A call after a keyed record acts on it, as one with no arguments does.
        let post = op("postCustomer");
        assert_eq!(post.resource_type.as_deref(), Some("customers"));
        assert_eq!(post.kind, OperationKind::Command(Effect::Other));
        assert_eq!(
            post.identity.as_ref().and_then(|i| i.path_param.as_deref()),
            Some("id")
        );
        assert!(post.is_action());
    }

    #[test]
    fn a_suffix_after_a_parameter_keeps_the_route_it_had() {
        let item = json!({"$ref": "#/components/schemas/Item"});
        let bundle = compiled(
            &json!({
                "/items/{id}": {"parameters": path_param("id"), "get": answering("getItem", &item)},
                "/items/{id}:archive": {"parameters": path_param("id"), "post": answering("archiveItem", &item)},
            }),
            &json!({"Item": {"type": "object", "properties": {"id": {"type": "string"}}}}),
        );
        let archive = bundle
            .operations
            .iter()
            .find(|op| op.id == "archiveItem")
            .unwrap();
        assert_eq!(archive.resource_type.as_deref(), Some("items"));
        assert_eq!(archive.kind, OperationKind::Command(Effect::Create));
    }

    #[test]
    fn a_name_splits_into_words_at_separators_and_case() {
        for (name, expected) in [
            ("issueIdOrKey", &["issue", "Id", "Or", "Key"][..]),
            ("task_gid", &["task", "gid"]),
            ("MessageSid", &["Message", "Sid"]),
            ("HTTPStatus", &["HTTP", "Status"]),
            ("issue-iid", &["issue", "iid"]),
            ("sha256Hash", &["sha256", "Hash"]),
            ("id", &["id"]),
        ] {
            assert_eq!(mb_mcm::words(name), expected, "{name}");
        }
    }

    #[test]
    fn a_parameter_names_the_member_its_trailing_words_name() {
        let defs = Map::new();
        let props = json!({
            "id": {"type": "integer"},
            "iid": {"type": "integer"},
            "gid": {"type": "string"},
            "key": {"type": "string"},
            "id_or_key": {"type": "string"},
            "sid": {"type": "string"},
            "kind": {"type": "object"},
        });
        let props = props.as_object().unwrap();
        for (param, field) in [
            ("issue_iid", Some("iid")),
            ("task_gid", Some("gid")),
            ("issueIdOrKey", Some("id_or_key")),
            ("issueKey", Some("key")),
            ("MessageSid", Some("sid")),
            ("userId", Some("id")),
            ("board_kind", None),
            ("handle", None),
        ] {
            assert_eq!(param_field(param, props, &defs), field, "{param}");
        }
    }

    #[test]
    fn versions_are_judged_by_their_shape() {
        for seg in [
            "v1",
            "V2",
            "v1.2",
            "v1beta2",
            "v2alpha",
            "3.0",
            "1",
            "2024-01-01",
        ] {
            assert!(is_version(seg), "{seg}");
        }
        for seg in [
            "v",
            "values",
            "vault",
            "V8Engine",
            "users",
            "3.x",
            "2024-1-01",
            "a1",
        ] {
            assert!(!is_version(seg), "{seg}");
        }
    }

    #[test]
    fn rpc_style_posts_that_read_are_queries() {
        for (path, resource) in [
            ("/accounts/get", "accounts"),
            ("/accounts/balance/get", "balance"),
            ("/transactions/sync", "transactions"),
            ("/institutions/get_by_id", "institutions"),
            ("/api/users.list", "users"),
            ("/{index}/_search", "_search"),
            ("/v1/customers/search", "customers"),
            ("/getAccounts", "getAccounts"),
        ] {
            assert_eq!(
                infer_kind(HttpMethod::Post, path),
                OperationKind::Query,
                "{path}"
            );
            assert_eq!(
                infer_resource_type(path).as_deref(),
                Some(resource),
                "{path}"
            );
        }
        for path in [
            "/link/token/create",
            "/item/public_token/exchange",
            "/v1/payment_intents",
            "/chat.postMessage",
            "/listings",
            "/getaway",
        ] {
            assert_eq!(
                infer_kind(HttpMethod::Post, path),
                OperationKind::Command(Effect::Create),
                "{path}"
            );
        }
        assert_eq!(
            infer_resource_type("/orders/{id}").as_deref(),
            Some("orders")
        );
        assert_eq!(infer_resource_type("/search").as_deref(), Some("search"));
    }

    #[test]
    fn heuristics_kind_and_resource_from_name() {
        let other = || OperationKind::Command(Effect::Other);
        assert_eq!(kind_from_name("getUser", other()), OperationKind::Query);
        assert_eq!(resource_from_name("getUser").as_deref(), Some("user"));
        assert_eq!(
            kind_from_name("Add", other()),
            OperationKind::Command(Effect::Create)
        );
        assert_eq!(resource_from_name("Add"), None);
        assert_eq!(
            kind_from_name("settle", OperationKind::Query),
            OperationKind::Query
        );
        assert_eq!(resource_from_name("settle").as_deref(), Some("settle"));
        assert_eq!(kind_from_name("listOrders", other()), OperationKind::Query);
        assert_eq!(resource_from_name("listOrders").as_deref(), Some("order"));
        assert_eq!(
            kind_from_name("remove_item", other()),
            OperationKind::Command(Effect::Delete)
        );
        assert_eq!(resource_from_name("remove_item").as_deref(), Some("item"));
        assert_eq!(
            kind_from_name("SetStatus", other()),
            OperationKind::Command(Effect::Update)
        );
        assert_eq!(kind_from_name("newsletter", other()), other());
        assert_eq!(resource_from_name("addAddress").as_deref(), Some("address"));
        assert_eq!(resource_from_name("search"), None);
        assert_eq!(kind_from_name("ébauche", other()), other());
    }

    /// A contract of `paths` over `schemas`, compiled.
    fn compiled(paths: &Value, schemas: &Value) -> mb_mcm::Bundle {
        let doc = json!({
            "openapi": "3.0.3",
            "info": {"title": "T", "version": "1"},
            "paths": paths,
            "components": {"schemas": schemas},
        });
        crate::compile(doc.to_string().as_bytes()).unwrap()
    }

    fn answering(id: &str, schema: &Value) -> Value {
        json!({
            "operationId": id,
            "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": schema}}}},
        })
    }

    fn path_param(name: &str) -> Value {
        json!([{"name": name, "in": "path", "required": true, "schema": {"type": "string"}}])
    }

    fn identity_of(bundle: &mb_mcm::Bundle, id: &str) -> Option<Identity> {
        bundle
            .operations
            .iter()
            .find(|op| op.id == id)
            .and_then(|op| op.identity.clone())
    }

    fn wrapped(path_param: Option<&str>, field: &str, member: &str) -> Option<Identity> {
        Some(Identity {
            path_param: path_param.map(str::to_owned),
            field: field.to_owned(),
            input_field: None,
            path_field: None,
            record: Some(mb_mcm::RecordAt::Member(member.to_owned())),
        })
    }

    #[test]
    fn a_record_beside_status_scalars_goes_by_its_own_key() {
        let envelope = json!({"type": "object", "properties": {
            "code": {"type": "integer"},
            "message": {"type": "string"},
            "widget": {"$ref": "#/components/schemas/Widget"},
        }});
        let bundle = compiled(
            &json!({
                "/widgets": {"post": answering("create", &envelope)},
                "/widgets/{widget_id}": {
                    "parameters": path_param("widget_id"),
                    "get": answering("read", &envelope),
                    "put": answering("update", &envelope),
                },
            }),
            &json!({"Widget": {"type": "object", "properties": {
                "widget_id": {"type": "string"},
                "owner_id": {"type": "string"},
                "total": {"type": "number"},
            }}}),
        );
        assert_eq!(
            identity_of(&bundle, "create"),
            wrapped(None, "widget_id", "widget")
        );
        for id in ["read", "update"] {
            assert_eq!(
                identity_of(&bundle, id),
                wrapped(Some("widget_id"), "widget_id", "widget"),
                "{id}"
            );
        }
    }

    #[test]
    fn a_record_under_a_generic_member_goes_by_its_id() {
        let envelope = json!({"type": "object", "properties": {
            "success": {"type": "boolean"},
            "data": {"$ref": "#/components/schemas/Gadget"},
        }});
        let bundle = compiled(
            &with_peer(
                json!({
                    "/gadgets": {"post": sending("create", &json!({"type": "object", "properties": {"name": {"type": "string"}}}), &envelope)},
                    "/gadgets/{id}": {"parameters": path_param("id"), "get": answering("read", &envelope)},
                }),
                &envelope,
            ),
            &json!({"Gadget": {"type": "object", "properties": {
                "id": {"type": "integer"},
                "name": {"type": "string"},
                "owner_id": {"type": "integer"},
            }}}),
        );
        assert_eq!(identity_of(&bundle, "create"), wrapped(None, "id", "data"));
        assert_eq!(
            identity_of(&bundle, "read"),
            wrapped(Some("id"), "id", "data")
        );
    }

    #[test]
    fn a_record_named_for_its_type_beside_a_request_id_goes_by_the_types_key() {
        let envelope = json!({"type": "object", "properties": {
            "link": {"$ref": "#/components/schemas/Link"},
            "request_id": {"type": "string"},
        }});
        let bundle = compiled(
            &json!({"/link/get": {"post": answering("linkGet", &envelope)}}),
            &json!({"Link": {"type": "object", "properties": {
                "link_id": {"type": "string"},
                "partner_id": {"type": "string"},
                "products": {"type": "array", "items": {"type": "string"}},
            }}}),
        );
        assert_eq!(
            identity_of(&bundle, "linkGet"),
            wrapped(None, "link_id", "link")
        );
    }

    fn obj(props: Value) -> Value {
        json!({"type": "object", "properties": props})
    }

    /// `envelope` answered by `POST /<collection>` (sending a `name`) and
    /// `GET /<collection>/{<param>}` (ids `create` and `read`), so two of the type's
    /// routes, one a read by path, and a body setting the record's `name` corroborate
    /// whatever member structure picks.
    fn corroborated(collection: &str, param: &str, envelope: &Value) -> mb_mcm::Bundle {
        compiled(
            &with_peer(
                json!({
                    format!("/{collection}"): {"post": sending("create", &obj(json!({"name": {"type": "string"}})), envelope)},
                    format!("/{collection}/{{{param}}}"): {
                        "parameters": path_param(param),
                        "get": answering("read", envelope),
                    },
                }),
                envelope,
            ),
            &json!({"Gadget": obj(json!({"id": {"type": "string"}, "name": {"type": "string"}}))}),
        )
    }

    /// `paths` with `GET /peers/{peer_id}` answering `envelope`'s single values beside
    /// another type's object, so the envelope is a convention of the contract's.
    fn with_peer(mut paths: Value, envelope: &Value) -> Value {
        let mut peer = envelope.clone();
        if let Some(props) = peer.get_mut("properties").and_then(Value::as_object_mut) {
            for schema in props.values_mut() {
                if schema.get("properties").is_some()
                    || schema.get("$ref").is_some()
                    || schema.get("anyOf").is_some()
                {
                    *schema = obj(json!({"peer_id": {"type": "string"}}));
                }
            }
        }
        if let Some(paths) = paths.as_object_mut() {
            paths.insert(
                "/peers/{peer_id}".into(),
                json!({"parameters": path_param("peer_id"), "get": answering("peer", &peer)}),
            );
        }
        paths
    }

    fn unknown(bundle: &mb_mcm::Bundle, ids: &[&str]) {
        for id in ids {
            assert_eq!(identity_of(bundle, id), None, "{id}");
        }
    }

    #[test]
    fn an_envelope_with_two_objects_stays_unknown() {
        let record = |key: &str| obj(json!({key: {"type": "string"}, "name": {"type": "string"}}));
        let two = obj(json!({
            "code": {"type": "integer"},
            "order": record("order_id"),
            "customer": record("customer_id"),
        }));
        unknown(
            &corroborated("orders", "order_id", &two),
            &["create", "read"],
        );
    }

    #[test]
    fn a_nullable_or_composed_record_is_an_object_beside_another() {
        let gadget = json!({"$ref": "#/components/schemas/Gadget"});
        let error = obj(json!({"id": {"type": "string"}, "detail": {"type": "string"}}));
        let page = obj(json!({"id": {"type": "string"}, "next": {"type": "string"}}));
        let nullable_all_of = obj(json!({
            "success": {"type": "boolean"},
            "data": {"allOf": [gadget], "nullable": true},
            "error": error,
        }));
        unknown(
            &corroborated("gadgets", "id", &nullable_all_of),
            &["create", "read"],
        );
        let any_of_null = obj(json!({
            "ok": {"type": "boolean"},
            "data": {"anyOf": [gadget, {"type": "null"}]},
            "meta": page,
        }));
        unknown(
            &corroborated("gadgets", "id", &any_of_null),
            &["create", "read"],
        );
        let typed_null = obj(json!({
            "ok": {"type": "boolean"},
            "data": gadget,
            "meta": {"type": ["object", "null"], "properties": {"id": {"type": "string"}}},
        }));
        unknown(
            &corroborated("gadgets", "id", &typed_null),
            &["create", "read"],
        );
        // Alone beside single values, the nullable record is the record.
        let alone = obj(json!({
            "ok": {"type": "boolean"},
            "data": {"anyOf": [gadget, {"type": "null"}]},
        }));
        let bundle = corroborated("gadgets", "id", &alone);
        assert_eq!(identity_of(&bundle, "create"), wrapped(None, "id", "data"));
        assert_eq!(
            identity_of(&bundle, "read"),
            wrapped(Some("id"), "id", "data")
        );
    }

    #[test]
    fn an_uncorroborated_object_is_not_the_record() {
        let related = obj(json!({"id": {"type": "string"}, "name": {"type": "string"}}));
        let bundle = compiled(
            &json!({
                // A related record beside the answer's own members, by a bare `{id}`.
                "/orders/{id}": {"parameters": path_param("id"), "put": answering("orderPut", &obj(json!({
                    "order_number": {"type": "string"},
                    "status": {"type": "string"},
                    "customer": related,
                })))},
                // Creates no read by path corroborates.
                "/notes": {"post": answering("noteCreate", &obj(json!({
                    "note_key": {"type": "string"},
                    "title": {"type": "string"},
                    "owner": related,
                })))},
                "/exports": {"post": answering("exportCreate", &obj(json!({
                    "status": {"type": "string"},
                    "job": obj(json!({"id": {"type": "string"}, "state": {"type": "string"}})),
                })))},
                // A foreign key named for the type, in another member's record.
                "/invoices": {"post": answering("invoiceCreate", &obj(json!({
                    "code": {"type": "integer"},
                    "payment": obj(json!({"amount": {"type": "number"}, "invoice_id": {"type": "string"}})),
                })))},
                // A query's pagination object.
                "/reports": {"get": answering("reportList", &obj(json!({
                    "count": {"type": "integer"},
                    "page": obj(json!({"id": {"type": "string"}, "next": {"type": "string"}})),
                })))},
            }),
            &json!({}),
        );
        unknown(
            &bundle,
            &[
                "orderPut",
                "noteCreate",
                "exportCreate",
                "invoiceCreate",
                "reportList",
            ],
        );
    }

    #[test]
    fn a_parameter_that_names_nothing_gives_no_identity_and_teaches_none() {
        let envelope = obj(json!({
            "success": {"type": "boolean"},
            "data": {"$ref": "#/components/schemas/Gadget"},
        }));
        // `{sku}` names no member: neither the read nor, taught by it, the create
        // falls back to the record's `id`.
        unknown(
            &corroborated("products", "sku", &envelope),
            &["create", "read"],
        );
        // `{owner_id}` names the `id` only by its last word, for neither the member
        // nor the type.
        let part = obj(
            json!({"name": {"type": "string"}, "meta": {"$ref": "#/components/schemas/Gadget"}}),
        );
        unknown(
            &corroborated("stages", "owner_id", &part),
            &["create", "read"],
        );
    }

    #[test]
    fn a_key_named_for_the_member_is_not_the_types() {
        let request = obj(json!({
            "status": {"type": "string"},
            "request": obj(json!({"request_id": {"type": "string"}, "name": {"type": "string"}})),
        }));
        unknown(
            &corroborated("widgets", "id", &request),
            &["create", "read"],
        );
        let error = obj(json!({
            "ok": {"type": "boolean"},
            "error": obj(json!({"error_id": {"type": "string"}, "name": {"type": "string"}})),
        }));
        unknown(&corroborated("widgets", "id", &error), &["create", "read"]);
        // The record has a key named for its member beside the type's: it is the
        // member's record, and the type's key refers to it.
        let payment = obj(json!({
            "code": {"type": "integer"},
            "payment": obj(json!({"payment_id": {"type": "string"}, "invoice_id": {"type": "string"}, "name": {"type": "string"}})),
        }));
        unknown(
            &corroborated("invoices", "id", &payment),
            &["create", "read"],
        );
        // A key named for the type, under a generic member, is the record's.
        let data = obj(json!({
            "ok": {"type": "boolean"},
            "data": obj(json!({"ticket_id": {"type": "string"}, "owner_id": {"type": "string"}, "name": {"type": "string"}})),
        }));
        let bundle = corroborated("tickets", "id", &data);
        assert_eq!(
            identity_of(&bundle, "create"),
            wrapped(None, "ticket_id", "data")
        );
        assert_eq!(
            identity_of(&bundle, "read"),
            wrapped(Some("id"), "ticket_id", "data")
        );
    }

    #[test]
    fn a_create_goes_by_the_key_its_read_by_path_teaches() {
        // The read by `{code}` goes by `code`; the create's own fallback would be
        // `widget_id`, but the type's read says otherwise.
        let envelope = obj(json!({
            "ok": {"type": "boolean"},
            "widget": obj(json!({"code": {"type": "string"}, "widget_id": {"type": "string"}})),
        }));
        let bundle = corroborated("widgets", "code", &envelope);
        assert_eq!(
            identity_of(&bundle, "create"),
            wrapped(None, "code", "widget")
        );
        assert_eq!(
            identity_of(&bundle, "read"),
            wrapped(Some("code"), "code", "widget")
        );
        // A record with an `id` goes by it, the path naming the member it reads.
        let with_id = obj(json!({
            "ok": {"type": "boolean"},
            "widget": obj(json!({"id": {"type": "string"}, "widget_id": {"type": "string"}})),
        }));
        let bundle = corroborated("widgets", "widget_id", &with_id);
        assert_eq!(
            identity_of(&bundle, "read"),
            Some(Identity {
                path_param: Some("widget_id".into()),
                field: "id".into(),
                input_field: None,
                path_field: Some("widget_id".into()),
                record: Some(mb_mcm::RecordAt::Member("widget".into())),
            })
        );
        assert_eq!(
            identity_of(&bundle, "create"),
            wrapped(None, "id", "widget")
        );
    }

    /// `answering(id, schema)` with a JSON request body of `body`.
    fn sending(id: &str, body: &Value, schema: &Value) -> Value {
        let mut op = answering(id, schema);
        op["requestBody"] = json!({"content": {"application/json": {"schema": body}}});
        op
    }

    #[test]
    fn what_the_writes_send_tells_the_record_from_a_related_object() {
        // The writes set the envelope's own members: it is the record, and the
        // object beside them a related one, though two routes answer it.
        let order = obj(json!({
            "order_number": {"type": "string"},
            "status": {"type": "string"},
            "customer": obj(json!({"id": {"type": "string"}, "name": {"type": "string"}})),
        }));
        let bundle = compiled(
            &json!({"/orders/{id}": {
                "parameters": path_param("id"),
                "get": answering("read", &order),
                "put": sending("update", &obj(json!({"order_number": {"type": "string"}, "status": {"type": "string"}})), &order),
            }}),
            &json!({}),
        );
        unknown(&bundle, &["read", "update"]);
        // A body setting members of both says nothing either way.
        let both = obj(json!({"status": {"type": "string"}, "name": {"type": "string"}}));
        let bundle = compiled(
            &json!({"/orders/{id}": {
                "parameters": path_param("id"),
                "get": answering("read", &order),
                "put": sending("update", &both, &order),
            }}),
            &json!({}),
        );
        unknown(&bundle, &["read", "update"]);
        // The writes set the object's members and none of the envelope's: it is
        // the record.
        let envelope = obj(json!({
            "success": {"type": "boolean"},
            "data": obj(json!({"id": {"type": "integer"}, "name": {"type": "string"}, "owner_id": {"type": "integer"}})),
        }));
        let body = obj(json!({"name": {"type": "string"}, "owner_id": {"type": "integer"}}));
        let bundle = compiled(
            &with_peer(
                json!({
                    "/accounts": {"post": sending("create", &body, &envelope)},
                    "/accounts/{id}": {
                        "parameters": path_param("id"),
                        "get": answering("read", &envelope),
                        "patch": sending("update", &body, &envelope),
                    },
                }),
                &envelope,
            ),
            &json!({}),
        );
        assert_eq!(identity_of(&bundle, "create"), wrapped(None, "id", "data"));
        for id in ["read", "update"] {
            assert_eq!(
                identity_of(&bundle, id),
                wrapped(Some("id"), "id", "data"),
                "{id}"
            );
        }
    }

    fn order_envelope() -> Value {
        obj(json!({
            "order_number": {"type": "string"},
            "status": {"type": "string"},
            "customer": obj(json!({"id": {"type": "string"}, "name": {"type": "string"}})),
        }))
    }

    #[test]
    fn routes_alone_do_not_corroborate_without_a_body_to_read() {
        let order = order_envelope();
        // A read and a delete, or a read and an update sending nothing.
        let read_delete = compiled(
            &json!({"/orders/{id}": {
                "parameters": path_param("id"),
                "get": answering("read", &order),
                "delete": answering("delete", &order),
            }}),
            &json!({}),
        );
        unknown(&read_delete, &["read", "delete"]);
        let bodyless = compiled(
            &json!({"/orders/{id}": {
                "parameters": path_param("id"),
                "get": answering("read", &order),
                "put": answering("update", &order),
            }}),
            &json!({}),
        );
        unknown(&bodyless, &["read", "update"]);
        // Two reads by path, no write.
        let reads = compiled(
            &json!({
                "/orders/{id}": {"parameters": path_param("id"), "get": answering("read", &order)},
                "/orders/by-number/{order_number}": {"parameters": path_param("order_number"), "get": answering("byNumber", &order)},
            }),
            &json!({}),
        );
        unknown(&reads, &["read"]);
    }

    #[test]
    fn a_composed_body_is_read_through_its_branches() {
        let order = order_envelope();
        let body = json!({"oneOf": [
            obj(json!({"status": {"type": "string"}})),
            obj(json!({"order_number": {"type": "string"}})),
        ]});
        let bundle = compiled(
            &json!({
                "/orders": {"post": sending("create", &body, &order)},
                "/orders/{id}": {"parameters": path_param("id"), "get": answering("read", &order)},
            }),
            &json!({}),
        );
        unknown(&bundle, &["create", "read"]);
        // A body that cannot be read as members tells nothing, even of a member
        // named for the type.
        let widget = obj(json!({
            "ok": {"type": "boolean"},
            "widget": obj(json!({"id": {"type": "string"}, "name": {"type": "string"}})),
        }));
        let list = json!({"type": "array", "items": {"type": "string"}});
        let bundle = compiled(
            &json!({
                "/widgets": {"post": sending("create", &list, &widget)},
                "/widgets/{id}": {"parameters": path_param("id"), "get": answering("read", &widget)},
            }),
            &json!({}),
        );
        unknown(&bundle, &["create", "read"]);
        // An `allOf` part that only constrains is read past.
        let constrained = json!({"allOf": [
            obj(json!({"name": {"type": "string"}})),
            {"type": "object", "required": ["name"]},
        ]});
        let bundle = compiled(
            &json!({
                "/widgets": {"post": sending("create", &constrained, &widget)},
                "/widgets/{id}": {"parameters": path_param("id"), "get": answering("read", &widget)},
            }),
            &json!({}),
        );
        assert_eq!(
            identity_of(&bundle, "create"),
            wrapped(None, "id", "widget")
        );
    }

    #[test]
    fn a_body_that_sets_a_related_objects_member_among_others_is_not_enough() {
        let order = order_envelope();
        let guest = obj(json!({
            "name": {"type": "string"},
            "items": {"type": "array", "items": {"type": "string"}},
        }));
        let bundle = compiled(
            &json!({
                "/orders": {"post": sending("create", &guest, &order)},
                "/orders/{id}": {"parameters": path_param("id"), "get": answering("read", &order)},
            }),
            &json!({}),
        );
        unknown(&bundle, &["create", "read"]);
    }

    #[test]
    fn an_envelope_member_the_record_also_has_is_the_records_to_set() {
        let envelope = obj(json!({
            "status": {"type": "string"},
            "data": obj(json!({"id": {"type": "string"}, "name": {"type": "string"}, "status": {"type": "string"}})),
        }));
        let body = obj(json!({"name": {"type": "string"}, "status": {"type": "string"}}));
        let bundle = compiled(
            &with_peer(
                json!({
                    "/accounts": {"post": sending("create", &body, &envelope)},
                    "/accounts/{id}": {"parameters": path_param("id"), "get": answering("read", &envelope)},
                }),
                &envelope,
            ),
            &json!({}),
        );
        assert_eq!(identity_of(&bundle, "create"), wrapped(None, "id", "data"));
        assert_eq!(
            identity_of(&bundle, "read"),
            wrapped(Some("id"), "id", "data")
        );
    }

    #[test]
    fn a_parameter_naming_a_member_by_its_last_words_must_be_named_for_it() {
        let envelope = obj(json!({
            "ok": {"type": "boolean"},
            "order": obj(json!({"id": {"type": "string"}, "number": {"type": "string"}})),
        }));
        let at = |param: &str| {
            compiled(
                &json!({format!("/orders/{{{param}}}"): {
                    "parameters": path_param(param),
                    "get": answering("read", &envelope),
                }}),
                &json!({}),
            )
        };
        // `{customer_number}` is a customer's number, not this order's.
        unknown(&at("customer_number"), &["read"]);
        assert_eq!(
            identity_of(&at("order_number"), "read"),
            Some(Identity {
                path_param: Some("order_number".into()),
                field: "id".into(),
                input_field: None,
                path_field: Some("number".into()),
                record: Some(mb_mcm::RecordAt::Member("order".into())),
            })
        );
    }

    #[test]
    fn a_query_with_no_path_parameter_needs_a_key_named_for_its_type() {
        // A page named for its type has an `id` too.
        let pages = obj(json!({
            "count": {"type": "integer"},
            "page": obj(json!({"id": {"type": "string"}, "next": {"type": "string"}})),
        }));
        let bundle = compiled(
            &json!({"/pages": {"get": answering("list", &pages)}}),
            &json!({}),
        );
        unknown(&bundle, &["list"]);
    }

    #[test]
    fn an_envelope_only_one_type_answers_is_that_types_record() {
        // A guest checkout sends the customer's members, and two routes answer the
        // same envelope; but no other type answers `{order_number, status, X}`, so
        // it is the order, holding a related customer.
        let order = obj(json!({
            "order_number": {"type": "string"},
            "status": {"type": "string"},
            "customer": obj(json!({"id": {"type": "string"}, "name": {"type": "string"}, "email": {"type": "string"}})),
        }));
        let guest = obj(json!({"name": {"type": "string"}, "email": {"type": "string"}}));
        let bundle = compiled(
            &json!({
                "/orders": {"post": sending("create", &guest, &order)},
                "/orders/{id}": {"parameters": path_param("id"), "get": answering("read", &order)},
            }),
            &json!({}),
        );
        unknown(&bundle, &["create", "read"]);
    }

    #[test]
    fn a_free_form_body_says_nothing_either_way() {
        let widget = obj(json!({
            "ok": {"type": "boolean"},
            "widget": obj(json!({"id": {"type": "string"}, "name": {"type": "string"}})),
        }));
        let patch = json!({"type": "object", "additionalProperties": true});
        let bundle = compiled(
            &json!({
                "/widgets/{id}": {
                    "parameters": path_param("id"),
                    "get": answering("read", &widget),
                    "patch": sending("update", &patch, &widget),
                },
            }),
            &json!({}),
        );
        for id in ["read", "update"] {
            assert_eq!(
                identity_of(&bundle, id),
                wrapped(Some("id"), "id", "widget"),
                "{id}"
            );
        }
    }

    #[test]
    fn an_all_of_member_no_branch_constrains_keeps_the_one_that_does() {
        let defs = json!({
            "Base": {"type": "object", "properties": {
                "ok": {"type": "boolean"},
                "result": {},
                "meta": {"type": "string"},
            }},
            "Gadget": obj(json!({"id": {"type": "string"}})),
        })
        .as_object()
        .cloned()
        .unwrap();
        let schema = json!({"allOf": [
            {"$ref": "#/$defs/Base"},
            {"type": "object", "properties": {
                "result": {"$ref": "#/$defs/Gadget"},
                "meta": {"type": "integer"},
            }},
        ]});
        let props = schema_properties(Some(&schema), &defs).unwrap();
        assert_eq!(props["result"], json!({"$ref": "#/$defs/Gadget"}));
        // Both constrain it: the extension, after its base, is kept.
        assert_eq!(props["meta"], json!({"type": "integer"}));
        assert_eq!(props["ok"], json!({"type": "boolean"}));
        // The base after the extension: its `{}` still adds nothing.
        let reversed = json!({"allOf": [
            {"type": "object", "properties": {"result": {"$ref": "#/$defs/Gadget"}}},
            {"$ref": "#/$defs/Base"},
        ]});
        let props = schema_properties(Some(&reversed), &defs).unwrap();
        assert_eq!(props["result"], json!({"$ref": "#/$defs/Gadget"}));
        // A schema's own members extend its `allOf`.
        let own = json!({
            "allOf": [{"$ref": "#/$defs/Base"}],
            "properties": {"result": {"$ref": "#/$defs/Gadget"}},
        });
        let props = schema_properties(Some(&own), &defs).unwrap();
        assert_eq!(props["result"], json!({"$ref": "#/$defs/Gadget"}));
    }

    #[test]
    fn an_envelope_extending_a_base_with_a_placeholder_holds_its_record() {
        let schemas = json!({
            "Envelope": {"type": "object", "required": ["success", "result"], "properties": {
                "success": {"type": "boolean"},
                "messages": {"type": "array", "items": {"type": "string"}},
                "result": {},
            }},
            "Gadget": obj(json!({"id": {"type": "string"}, "name": {"type": "string"}})),
        });
        let single = json!({"allOf": [
            {"$ref": "#/components/schemas/Envelope"},
            {"type": "object", "properties": {"result": {"$ref": "#/components/schemas/Gadget"}}},
        ]});
        let listing = json!({"allOf": [
            {"$ref": "#/components/schemas/Envelope"},
            {"type": "object", "properties": {"result": {
                "type": "array", "items": {"$ref": "#/components/schemas/Gadget"},
            }}},
        ]});
        let bundle = compiled(
            &json!({
                "/gadgets": {"get": answering("list", &listing)},
                "/gadgets/{gadget_id}": {
                    "parameters": path_param("gadget_id"),
                    "get": answering("read", &single),
                    "put": sending("replace", &obj(json!({"name": {"type": "string"}})), &single),
                },
            }),
            &schemas,
        );
        for id in ["read", "replace"] {
            assert_eq!(
                identity_of(&bundle, id),
                wrapped(Some("gadget_id"), "id", "result"),
                "{id}"
            );
        }
    }

    /// An order keyed by its own `id`, whose lines refer back to it by the path
    /// parameter's name, is the record, not its one line.
    #[test]
    fn a_record_s_own_key_beats_an_item_naming_the_parameter() {
        let line = obj(json!({"order_id": {"type": "string"}, "sku": {"type": "string"}}));
        let keyed = obj(json!({
            "id": {"type": "string"},
            "total": {"type": "number"},
            "lines": {"type": "array", "items": line.clone()},
        }));
        let unkeyed = obj(json!({
            "total": {"type": "number"},
            "lines": {"type": "array", "items": line},
        }));
        for order in [keyed, unkeyed] {
            let bundle = compiled(
                &json!({
                    "/orders/{order_id}": {
                        "parameters": path_param("order_id"),
                        "get": answering("getOrder", &order),
                    },
                }),
                &json!({}),
            );
            let record = identity_of(&bundle, "getOrder").and_then(|i| i.record);
            assert!(
                !matches!(record, Some(mb_mcm::RecordAt::MemberItem(_))),
                "{record:?}"
            );
        }
    }

    /// A read by `{ThingID}` answering the collection list's envelope holds the one
    /// thing in its array, keyed by `ThingID`, not by the envelope's `Id`.
    #[test]
    fn a_read_answering_its_list_s_envelope_holds_the_one_item() {
        let schemas = json!({
            "Things": obj(json!({
                "Id": {"type": "string"},
                "Status": {"type": "string"},
                "Things": {"type": "array", "items": {"$ref": "#/components/schemas/Thing"}},
            })),
            "Thing": obj(json!({"ThingID": {"type": "string"}, "Name": {"type": "string"}})),
        });
        let things = json!({"$ref": "#/components/schemas/Things"});
        let with_list = compiled(
            &json!({
                "/Things": {"get": answering("list", &things)},
                "/Things/{ThingID}": {"parameters": path_param("ThingID"), "get": answering("read", &things)},
            }),
            &schemas,
        );
        let identity = identity_of(&with_list, "read").unwrap();
        assert_eq!(identity.field, "ThingID");
        assert_eq!(
            identity.record,
            Some(mb_mcm::RecordAt::MemberItem("Things".into()))
        );
        // No list answers that shape: nothing says the array is the collection.
        let alone = compiled(
            &json!({
                "/Things/{ThingID}": {"parameters": path_param("ThingID"), "get": answering("read", &things)},
            }),
            &schemas,
        );
        let record = identity_of(&alone, "read").and_then(|i| i.record);
        assert!(
            !matches!(record, Some(mb_mcm::RecordAt::MemberItem(_))),
            "{record:?}"
        );
    }

    #[test]
    fn two_constraining_all_of_objects_have_both_members() {
        let defs = json!({
            "Base": obj(json!({"result": obj(json!({"id": {"type": "string"}}))})),
            "Gadget": obj(json!({"id": {"type": "string"}})),
        })
        .as_object()
        .cloned()
        .unwrap();
        let schema = json!({"allOf": [
            {"$ref": "#/$defs/Base"},
            obj(json!({"result": obj(json!({"name": {"type": "string"}}))})),
        ]});
        let props = schema_properties(Some(&schema), &defs).unwrap();
        let result = props["result"]["properties"].as_object().unwrap();
        assert!(
            result.contains_key("id") && result.contains_key("name"),
            "{}",
            props["result"]
        );
        for placeholder in [
            json!({"type": "object", "properties": {}}),
            json!({"type": "object", "additionalProperties": {}}),
            json!({"additionalProperties": true}),
        ] {
            let schema = json!({"allOf": [
                obj(json!({"result": {"$ref": "#/$defs/Gadget"}})),
                obj(json!({"result": placeholder})),
            ]});
            let props = schema_properties(Some(&schema), &defs).unwrap();
            assert_eq!(props["result"], json!({"$ref": "#/$defs/Gadget"}));
        }
    }
}
