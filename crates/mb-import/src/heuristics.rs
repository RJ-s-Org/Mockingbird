//! Inference of operation kind, resource type, and identity from HTTP shape.

use std::borrow::Cow;

use crate::schema::defs_name;
use mb_mcm::{Binding, Effect, HttpMethod, Identity, Operation, OperationKind};
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
/// RPC-style read (`/accounts/get`, `users.list`), what it reads.
pub(crate) fn infer_resource_type(path: &str) -> Option<String> {
    let segs: Vec<&str> = segments(path).into_iter().map(without_format).collect();
    let mut statics = segs.iter().rev().filter(|s| !is_param(s));
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

fn trailing_param(path: &str) -> Option<&str> {
    segments(path).last().and_then(|s| param_name(s))
}

/// Endings of id-like path parameters and the response field each names,
/// most specific first: `issueIdOrKey` → `key`, `issue_number` → `number`,
/// `MessageSid` → `sid`.
const PARAM_FIELDS: [(&[&str], &str); 5] = [
    (&["IdOrKey", "_id_or_key"], "key"),
    (&["Number", "_number"], "number"),
    (&["Sid", "_sid"], "sid"),
    (&["Key", "_key"], "key"),
    (&["Uuid", "_uuid"], "uuid"),
];

/// Fields that identify a created resource without an `id` (Twilio's
/// `sid`).
const ID_FALLBACKS: [&str; 2] = ["sid", "uuid"];

/// The member of `props` named `name`, ASCII case-insensitively.
fn property<'a>(props: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    props
        .keys()
        .find(|k| k.eq_ignore_ascii_case(name))
        .map(String::as_str)
}

/// The response field path parameter `param` names: the property of that
/// name, else the one its ending names (see [`PARAM_FIELDS`]).
fn param_field<'a>(param: &str, props: &'a Map<String, Value>) -> Option<&'a str> {
    property(props, param).or_else(|| {
        PARAM_FIELDS
            .iter()
            .filter(|(endings, _)| {
                endings
                    .iter()
                    .any(|e| param.len() > e.len() && param.ends_with(e))
            })
            .find_map(|(_, field)| property(props, field))
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
    identity_for(trailing_param(path), success_schema, defs)
}

/// The identity of a resource shaped like `schema`, addressed by
/// `path_param`.
fn identity_for(
    path_param: Option<&str>,
    success_schema: Option<&Value>,
    defs: &Map<String, Value>,
) -> Option<Identity> {
    let props = schema_properties(success_schema, defs)?;
    let props = props.as_ref();
    let named = path_param.and_then(|p| param_field(p, props));
    let field = if props.contains_key("id") {
        "id"
    } else if let Some(named) = named {
        named
    } else if path_param.is_none() {
        ID_FALLBACKS.iter().find_map(|f| property(props, f))?
    } else {
        return None;
    };
    Some(Identity {
        path_param: path_param.map(str::to_owned),
        field: field.to_owned(),
        input_field: None,
        path_field: named.filter(|n| *n != field).map(str::to_owned),
    })
}

/// A route that acts on the resource its path names, not on a collection of
/// its own: `POST /payment_intents/{intent}/confirm` acts on the
/// `payment_intents` named by `intent`.
#[derive(Debug, PartialEq)]
pub(crate) struct ActionRoute {
    resource_type: String,
    path_param: String,
}

/// The action `method path` is, given every `(path, methods)` of the
/// contract: a `POST` to `<static>/{param}/<verb>` whose `<verb>` is not
/// plural, not a read verb, not readable by a `GET` on the same path and has
/// no `<path>/{...}` route below it. Those mark `<verb>` as a collection.
pub(crate) fn action_route(
    method: HttpMethod,
    path: &str,
    all_paths: &[(&str, &[HttpMethod])],
) -> Option<ActionRoute> {
    if method != HttpMethod::Post || ends_in_read(path) {
        return None;
    }
    let segs = segments(path);
    let [.., resource, param, verb] = segs.as_slice() else {
        return None;
    };
    let path_param = param_name(param)?;
    let verb = without_format(verb);
    if resource.contains('{') || verb.contains('{') || verb.ends_with('s') {
        return None;
    }
    let is_collection = all_paths.iter().any(|(other, methods)| {
        if *other == path {
            return methods.contains(&HttpMethod::Get);
        }
        let other = segments(other);
        other.starts_with(&segs) && other.get(segs.len()).is_some_and(|s| is_param(s))
    });
    (!is_collection).then(|| ActionRoute {
        resource_type: without_format(resource).to_owned(),
        path_param: path_param.to_owned(),
    })
}

/// The `GET` of the resource an action at `action_path` acts on:
/// `<static>/{param}`, whatever that route names its parameters.
fn parent_read<'a>(operations: &'a [Operation], action_path: &str) -> Option<&'a Operation> {
    let action = segments(action_path);
    let parent = action.split_last()?.1;
    operations.iter().find(|op| {
        let Binding::Http {
            method: HttpMethod::Get,
            path,
        } = &op.binding
        else {
            return false;
        };
        let read = segments(path);
        read.len() == parent.len()
            && read
                .iter()
                .zip(parent)
                .all(|(a, b)| a == b || (is_param(a) && is_param(b)))
    })
}

/// Points every [`action_route`] among the compiled `operations` at its
/// parent resource. Its kind is an update when it answers with what the
/// parent's `GET` answers, so the answer replaces the resource; otherwise
/// it stores nothing and the resource is only its context. Streams stay
/// streams.
pub(crate) fn apply_action_routes(
    operations: &mut [Operation],
    all_paths: &[(&str, &[HttpMethod])],
    defs: &Map<String, Value>,
) {
    for i in 0..operations.len() {
        let op = &operations[i];
        let Binding::Http { method, path } = &op.binding else {
            continue;
        };
        if !matches!(op.kind, OperationKind::Command(_)) {
            continue;
        }
        let Some(action) = action_route(*method, path, all_paths) else {
            continue;
        };
        let read = parent_read(operations, path);
        let answer = op.success_output().and_then(|o| o.schema.as_ref());
        let replaces =
            answer.is_some() && answer == read.and_then(|r| r.success_output()?.schema.as_ref());
        let param = action.path_param;
        let (effect, identity) = if replaces {
            let identity = identity_for(Some(&param), answer, defs);
            let identity = identity.unwrap_or_else(|| Identity {
                path_param: Some(param.clone()),
                field: "id".to_owned(),
                input_field: None,
                path_field: None,
            });
            (Effect::Update, identity)
        } else {
            // The answer is not the resource, so the identity is the read's,
            // except that a member named for the parameter names the target.
            let read = read.and_then(|r| r.identity.as_ref());
            let names_target =
                schema_properties(answer, defs).is_some_and(|p| p.contains_key(&param));
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
            };
            (Effect::Other, identity)
        };
        let op = &mut operations[i];
        op.kind = OperationKind::Command(effect);
        op.resource_type = Some(action.resource_type);
        op.identity = Some(identity);
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

    #[test]
    fn action_route_needs_a_singular_tail_under_a_static_and_a_param() {
        let posts = [HttpMethod::Post];
        let gets = [HttpMethod::Get];
        let confirm = "/v1/payment_intents/{intent}/confirm";
        let alone = [(confirm, posts.as_slice())];
        assert_eq!(
            action_route(HttpMethod::Post, confirm, &alone),
            Some(ActionRoute {
                resource_type: "payment_intents".into(),
                path_param: "intent".into(),
            })
        );
        assert_eq!(action_route(HttpMethod::Put, confirm, &alone), None);
        for path in [
            "/v1/customers/{id}/sources",
            "/v1/calls/{Sid}/Messages.json",
            "/repos/{o}/{r}/merge",
            "/a/{x}/{verb}",
            "/{x}/confirm",
            "/a/{x}/confirm/{y}",
            "/confirm",
            "/v1/databases/{id}/query",
        ] {
            assert_eq!(action_route(HttpMethod::Post, path, &[]), None, "{path}");
        }
        assert_eq!(
            action_route(HttpMethod::Post, "/v1/Calls/{CallSid}/Feedback.json", &[]),
            Some(ActionRoute {
                resource_type: "Calls".into(),
                path_param: "CallSid".into(),
            }),
        );
        let readable = [(confirm, gets.as_slice())];
        assert_eq!(action_route(HttpMethod::Post, confirm, &readable), None);
        let detail = format!("{confirm}/{{id}}");
        let below = [(detail.as_str(), gets.as_slice())];
        assert_eq!(action_route(HttpMethod::Post, confirm, &below), None);
        // A longer static name is not a route below it.
        let sibling = format!("{confirm}ation/{{id}}");
        let beside = [(sibling.as_str(), gets.as_slice())];
        assert!(action_route(HttpMethod::Post, confirm, &beside).is_some());
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
