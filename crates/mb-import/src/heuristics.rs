//! Inference of operation kind, resource type, and identity from HTTP shape.

use std::borrow::Cow;

use crate::Ctx;
use crate::schema::defs_name;
use mb_mcm::{Binding, Effect, HttpMethod, Identity, Operation, OperationKind, StatusSelector};
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
    /// It adds a record of its own, of the type a read elsewhere gives it,
    /// else the type its path gives it.
    Collection(Option<String>),
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

/// What an answer is about: itself, and what a one-member envelope holds.
fn answered_things(schema: &Value, defs: &Map<String, Value>) -> Vec<Value> {
    let mut things = Vec::new();
    if let Some(resolved) = resolve(schema, defs) {
        things.push(resolved.clone());
    }
    if let Some(props) = schema_properties(Some(schema), defs)
        && props.len() == 1
        && let Some(sole) = props.values().next().and_then(|v| resolve(v, defs))
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
fn reads_back(read: Option<&Value>, answer: Option<&Value>, defs: &Map<String, Value>) -> bool {
    let (Some(read), Some(answer)) = (read, answer) else {
        return true;
    };
    if is_opaque(Some(read), defs) || is_opaque(Some(answer), defs) {
        return true;
    }
    let mut read_things = answered_things(read, defs);
    read_things.extend(listed_things(read, defs));
    let identity = |s: &Value| identity_for(None, Some(s), defs).map(|i| i.field);
    answered_things(answer, defs).iter().any(|a| {
        read_things
            .iter()
            .any(|r| a == r || identity(a).is_some_and(|field| identity(r) == Some(field)))
    })
}

/// The type of the read that serves a record shaped like `answer`, from
/// another resource than `target`: by path (`GET /refunds/{id}`), else in a
/// list. The parent's own reads do not count: those are what it acts on.
fn read_elsewhere(
    answer: &Value,
    target: &ParentTarget,
    operations: &[Operation],
    defs: &Map<String, Value>,
) -> Option<String> {
    let schema = resolve(answer, defs)?;
    let others = || {
        gets(operations, |_| true)
            .filter(|read| read.resource_type.as_deref() != Some(&target.resource_type))
    };
    let by_path = others().find(|read| {
        http_path(read).is_some_and(|(_, path)| trailing_param(path).is_some())
            && self::answer(read).and_then(|a| resolve(a, defs)) == Some(schema)
    });
    let listed = || {
        others().find(|read| {
            self::answer(read).is_some_and(|a| listed_things(a, defs).iter().any(|i| i == schema))
        })
    };
    by_path.or_else(listed)?.resource_type.clone()
}

/// The structural test of a `POST` to `<static>/{param}/<static>...`, in
/// order. The spelling of the path is not part of it (only [`READ_VERBS`] is,
/// and that yields to a declared body and to `x-mockingbird-resource`).
///
/// 1. No success declares a body ([`declares_no_body`]): it acts on the
///    parent, whatever is beside it, and at any depth of tail. Nothing below
///    applies to a tail of two or more segments, which stays [`Route::Undecided`].
/// 2. A route below it (`<path>/{...}`) makes it a collection.
/// 3. A `GET` on its path that [`reads_back`] its answer makes it a collection.
///    A `GET` of something else does not.
/// 4. An answer that is what the parent's `GET` answers replaces the parent.
/// 5. An answer with an identity of its own is a record of its own when it is
///    [`read_elsewhere`], under that read's type, or when the status is `201`.
/// 6. Otherwise nothing can read what it answers, and it acts on the parent.
fn infer_route(op: &Operation, operations: &[Operation], defs: &Map<String, Value>) -> Route {
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
    let segs = segments(path);
    let has_detail = operations.iter().any(|other| {
        http_path(other).is_some_and(|(_, other)| {
            let other = segments(other);
            other.len() > segs.len()
                && same_route(&other[..segs.len()], &segs)
                && is_param(other[segs.len()])
        })
    });
    if has_detail {
        return Route::Collection(None);
    }
    let answered = answer(op);
    if gets(operations, |route| same_route(route, &segs))
        .any(|read| reads_back(answer(read), answered, defs))
    {
        return Route::Collection(None);
    }
    let replaces_parent = parent_read(operations, path, target.tail)
        .is_some_and(|read| answered.is_some() && answer(read) == answered);
    if !replaces_parent
        && op.identity.is_some()
        && let Some(answered) = answered
    {
        if let Some(read) = read_elsewhere(answered, &target, operations, defs) {
            return Route::Collection(Some(read));
        }
        if op.success_output().map(|o| o.status) == Some(StatusSelector::Code(201)) {
            return Route::Collection(None);
        }
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
/// update when it answers with what the parent's `GET` answers, so the answer
/// replaces the resource; otherwise it stores nothing and the resource is
/// only its context.
fn act_on_parent(
    index: usize,
    target: ParentTarget,
    operations: &mut [Operation],
    defs: &Map<String, Value>,
) {
    let op = &operations[index];
    let read = http_path(op).and_then(|(_, path)| parent_read(operations, path, target.tail));
    let answered = answer(op);
    let replaces = answered.is_some() && answered == read.and_then(answer);
    let param = target.path_param;
    let (effect, identity) = if replaces {
        let identity = identity_for(Some(&param), answered, defs);
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

/// The valid members of an operation's [`RESOURCE_EXT`].
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ResourceHint {
    resource_type: Option<String>,
    identity: Option<String>,
    action: Option<bool>,
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
            "type" if text().is_some() => hint.resource_type = text(),
            "identity" if text().is_some() => hint.identity = text(),
            "action" if value.is_boolean() => hint.action = value.as_bool(),
            "type" | "identity" => invalid(format!("member {name} is not a non-empty string")),
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
    let answered = answer(op);
    if has_it(answered) || schema_properties(answered, defs).is_none_or(|p| p.is_empty()) {
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
    let is_read = |op: &Operation| http_path(op).is_some_and(|(m, _)| m == HttpMethod::Get);
    let mut order: Vec<usize> = (0..hints.len()).collect();
    order.sort_by_key(|&i| !is_read(&operations[i]));
    for index in order {
        let hint = &hints[index];
        let op = &operations[index];
        let inferred = || infer_route(op, operations, defs);
        let route = match hint.action {
            Some(true) => declared_target(op, ctx).map_or_else(inferred, Route::Action),
            Some(false) => match inferred() {
                Route::Action(_) => Route::Collection(None),
                route => route,
            },
            None => inferred(),
        };
        let read_verb_write = hint.action == Some(false)
            && op.kind == OperationKind::Query
            && http_path(op).is_some_and(|(method, _)| method == HttpMethod::Post);
        match route {
            Route::Action(target) => act_on_parent(index, target, operations, defs),
            Route::Collection(Some(read)) => operations[index].resource_type = Some(read),
            Route::Collection(None) | Route::Undecided => {}
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
