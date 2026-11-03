//! Inference of operation kind, resource type, and identity from HTTP shape.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::Ctx;
use crate::schema::defs_name;
use mb_mcm::{
    Binding, Effect, HttpMethod, Identity, Operation, OperationKind, OutputVariant, StatusSelector,
};
use serde_json::{Map, Value};

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
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
    property(props, param).or_else(|| {
        let words = mb_mcm::words(param);
        (0..words.len()).find_map(|from| {
            let group = mb_mcm::joined_name(&words[from..].concat());
            props
                .iter()
                .find(|(member, schema)| {
                    mb_mcm::joined_name(member) == group && is_scalar_member(schema, defs)
                })
                .map(|(member, _)| member.as_str())
        })
    })
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
/// branches: an answer has them all.
fn schema_properties<'a>(
    schema: Option<&'a Value>,
    defs: &'a Map<String, Value>,
) -> Option<Cow<'a, Map<String, Value>>> {
    let schema = resolve(schema?, defs)?;
    let own = schema.get("properties").and_then(Value::as_object);
    let Some(branches) = schema.get("allOf").and_then(Value::as_array) else {
        return own.map(Cow::Borrowed);
    };
    let mut merged = own.cloned().unwrap_or_default();
    let mut found = own.is_some();
    let mut pending: Vec<&Value> = branches.iter().collect();
    let mut visits = 0;
    while let Some(branch) = pending.pop() {
        visits += 1;
        if visits > MAX_ALL_OF_BRANCHES {
            break;
        }
        let Some(branch) = resolve(branch, defs) else {
            continue;
        };
        if let Some(props) = branch.get("properties").and_then(Value::as_object) {
            found = true;
            merged.extend(props.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        pending.extend(
            branch
                .get("allOf")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
    }
    found.then_some(Cow::Owned(merged))
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
}
