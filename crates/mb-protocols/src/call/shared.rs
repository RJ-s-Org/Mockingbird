//! Records another mock of the same API wrote, for mocks that `shares` one
//! name (GitHub's REST beside its GraphQL): they keep their sessions under
//! it, so each finds what the others stored. A record is the same type
//! when the names agree beyond case, punctuation and a plural `s` (`issues`,
//! `issue`), when the world says so (`types.<t>.same_as`), or, where the world
//! names none, when the group's contracts show it (see [`SharedTypes`]): a REST
//! collection named for the type (`pulls` answering `pull-request`s, `repos`) and
//! a GraphQL type (`PullRequest`, `Repository`) declaring the same identity. It is
//! answered in the reading mock's shape: members renamed between camelCase and
//! snake_case where only that differs, enum values in the reading schema's
//! spelling where only case and separators differ (`CLOSED` as `closed`), REST's
//! `node_id` as GraphQL's `id` (and back), in the objects it nests and their lists
//! too (a number where a plain string is declared as that string),
//! and a scalar one shape holds flattened, named for the object the other holds
//! it in, nested there (GraphQL's `headRefName` as REST's `head.ref`, and back).

use std::sync::{Arc, PoisonError, RwLock};

use super::links::{Twin, twin_of};
use super::*;

/// Records of one equivalent type read, at most.
const MAX_SHARED: usize = 200;

/// The type names a record of type `t` may be stored under by the mock this
/// one shares; none when it shares with no other.
pub(crate) fn equivalents(rt: &MockRuntime, t: &str) -> Vec<String> {
    if rt.shares.is_none() {
        return Vec::new();
    }
    let base: String = t
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_lowercase();
    let mut names = vec![base.clone(), format!("{base}s")];
    names.extend(base.strip_suffix("es").map(str::to_string));
    names.extend(base.strip_suffix('s').map(str::to_string));
    match rt
        .described()
        .type_desc(t)
        .filter(|d| !d.same_as.is_empty())
    {
        Some(desc) => names.extend(desc.same_as.iter().map(|n| n.to_lowercase())),
        // The world's `same_as` wins: the contracts are read only without it.
        None => names.extend(rt.shared_types.inferred(rt, t).iter().cloned()),
    }
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if !name.is_empty() && name != t && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// The record of a type equivalent to `t` that `id` names, by its key or by
/// the member `op` reads it by, and that fits `owner` (the record the path is
/// scoped under) when given, drawn into `op`'s answer shape as a record of `t`
/// under `id`.
pub(super) async fn by_shared(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, t): (&Operation, &str),
    id: &str,
    owner: Option<&Owner>,
) -> Option<StoredResource> {
    let rt = state.rt;
    let mut members: Vec<String> = Vec::new();
    if let Some(identity) = &op.identity {
        members.push(identity.path_member().to_string());
        members.push(identity.field.clone());
    }
    // A path member named for its type (`pull_number` of `pulls`) names the type's
    // own member (`number`).
    let own = normal(t);
    let unprefixed: Vec<String> = members
        .iter()
        .filter_map(|m| {
            let snaked = snake(m);
            let (prefix, rest) = snaked.split_once('_')?;
            (prefix.len() >= 3 && own.starts_with(prefix) && !rest.is_empty())
                .then(|| rest.to_string())
        })
        .collect();
    members.extend(unprefixed);
    let members: Vec<String> = members
        .iter()
        .flat_map(|m| [m.clone(), snake(m), camel(m)])
        .collect();
    let props = op
        .success_output()
        .and_then(|o| o.schema.as_ref())
        .and_then(|s| record_props(&rt.bundle, s));
    for other in equivalents(rt, t) {
        for record in state.list(session, &other, MAX_SHARED).await {
            let named = record.id == id
                || members.iter().any(|m| {
                    mb_mcm::member_at(&record.data, m)
                        .and_then(id_string)
                        .is_some_and(|v| v == id)
                });
            if !named {
                continue;
            }
            let data = match props {
                Some(props) => reshape(props, &rt.bundle.defs, &record.data),
                None => record.data.clone(),
            };
            // A `number` is unique only under its owner (a repository): another
            // owner's record of that number is passed over, not the scan ended. It is
            // judged as the record it is (its own type and key), not under the key the
            // path names, which an earlier read of another owner's may have claimed.
            let own = StoredResource {
                resource_type: other.clone(),
                id: record.id.clone(),
                data,
            };
            // The nearest parameter names the repository alone (`r`): the whole chain
            // (`a/r`) tells two owners' repositories of one name apart.
            let fits = match owner {
                None => true,
                Some(o) => {
                    let judged = StoredResource {
                        data: resolved(state, session, o, &own.data, &record.data).await,
                        ..own.clone()
                    };
                    let stored = resolved(state, session, o, &record.data, &record.data).await;
                    key_fits(rt, &state.noted(session, &judged), o)
                        && chain_fits(&chain_of(o), &stored)
                }
            };
            if fits {
                return Some(StoredResource {
                    resource_type: t.to_string(),
                    id: id.to_string(),
                    data: own.data,
                });
            }
        }
    }
    None
}

/// `data`, a record another mock stored (`stored` as it stored it) drawn into the
/// reading mock's shape, as its owner is judged: each reference naming another record
/// by an id alone (GraphQL's `repository: {id: "R_1"}`, which REST's shape holds as
/// `{node_id}`) is replaced by the record it names, when the session holds it under
/// its type (`__typename`) or the member's name, so its name and owner say whose it is.
/// One the session does not hold is left out when `owner` names its record by name
/// (`trellis` of `/repos/{owner}/{repo}/…`): its id is the other mock's, and says
/// nothing of that name. Under an owner named by id (`{project_id}`) it stays.
async fn resolved(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    owner: &Owner,
    data: &Value,
    stored: &Value,
) -> Value {
    let mut data = data.clone();
    let Value::Object(members) = &mut data else {
        return data;
    };
    let references: Vec<(String, String)> = members
        .iter()
        .filter_map(|(name, value)| {
            let inner = value.as_object()?;
            let ids_only = inner.keys().any(|k| k != "__typename")
                && inner
                    .keys()
                    .all(|k| k == "__typename" || k == "node_id" || mb_mcm::is_generic_id(k));
            let id = ids_only
                .then(|| inner.values().find_map(id_string))
                .flatten()?;
            Some((name.clone(), id))
        })
        .collect();
    for (name, id) in references {
        let typename = [name.as_str(), camel(&name).as_str()]
            .iter()
            .find_map(|n| stored.get(*n)?.get("__typename")?.as_str().map(normal));
        let mut found = None;
        for t in typename.into_iter().chain([normal(&name)]) {
            found = state.get(session, &t, &id).await;
            if found.is_some() {
                break;
            }
        }
        match found {
            Some(record) => {
                members.insert(name, record.data);
            }
            None if !id_like(&owner.param) => {
                members.remove(&name);
            }
            None => {}
        }
    }
    data
}

/// The values of the path parameters `owner`'s collection is under, outermost first:
/// `[a, r]` of `/repos/a/r/pulls`.
fn chain_of(owner: &Owner) -> Vec<String> {
    let template = path_segments(&owner.collection.template);
    let path = path_segments(&owner.collection.path);
    template
        .iter()
        .zip(&path)
        .filter(|(t, _)| t.starts_with('{') && t.ends_with('}'))
        .map(|(_, v)| (*v).to_string())
        .collect()
}

/// The members of a record naming the repository it belongs to: GraphQL's and REST's
/// spellings. A pull request's `head` (a fork's branch) is not among them.
const REPOSITORY_MEMBERS: &[&str] = &["repository", "baseRepository", "base_repository"];

/// The members of a record (and of its repository) holding a link or path to it.
const LINK_MEMBERS: &[&str] = &[
    "url",
    "html_url",
    "htmlUrl",
    "repository_url",
    "repositoryUrl",
    "full_name",
    "nameWithOwner",
];

/// Whether `data` (a record another mock stored) belongs under the owner `chain`
/// names, as far as it shows: only the repository it belongs to counts, as its own
/// links (`url`, `html_url`, `repository_url`: `https://github.com/a/r/pull/5`)
/// show it, or its repository (`repository`, `baseRepository`, `base.repo`) does by
/// a link, a `full_name` (`a/r`) or its `name` beside an `owner`. Other nested
/// objects, a pull request's `head` (a fork) above all, say nothing. Names compare
/// without case, as GitHub's do. A record that shows no owner fits every one.
fn chain_fits(chain: &[String], data: &Value) -> bool {
    let [.., parent, name] = chain else {
        return true;
    };
    let mut shown: Vec<String> = Vec::new();
    owners_shown(data, name, &mut shown);
    let repositories = REPOSITORY_MEMBERS
        .iter()
        .filter_map(|m| data.get(*m))
        .chain(data.get("base").and_then(|b| b.get("repo")));
    for repository in repositories {
        owners_shown(repository, name, &mut shown);
        if repository
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
            && let Some(owner) = repository.get("owner")
        {
            let login = match owner {
                Value::String(login) => Some(login.as_str()),
                owner => ["login", "name", "slug"]
                    .iter()
                    .find_map(|k| owner.get(*k).and_then(Value::as_str)),
            };
            shown.extend(login.map(str::to_string));
        }
    }
    shown.is_empty() || shown.iter().any(|o| o.eq_ignore_ascii_case(parent))
}

/// The owners the link members of `object` show repository `name` under: the
/// segment before `name` in each path.
fn owners_shown(object: &Value, name: &str, out: &mut Vec<String>) {
    for member in LINK_MEMBERS {
        let Some(text) = object.get(*member).and_then(Value::as_str) else {
            continue;
        };
        if text.contains(char::is_whitespace) {
            continue;
        }
        let path = text.split_once("://").map_or(text, |(_, rest)| {
            rest.split_once('/').map_or("", |(_, p)| p)
        });
        let segments: Vec<&str> = path
            .split(['/', '?', '#'])
            .filter(|s| !s.is_empty())
            .collect();
        for pair in segments.windows(2) {
            if pair[1].eq_ignore_ascii_case(name) {
                out.push(pair[0].to_string());
            }
        }
    }
}

/// The properties of the object `schema` is (through a `$ref` and a nullable
/// `anyOf`).
pub(crate) fn record_props<'a>(
    bundle: &'a mb_mcm::Bundle,
    schema: &'a Value,
) -> Option<&'a Map<String, Value>> {
    let mut schema = schema;
    for _ in 0..4 {
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            return Some(props);
        }
        if let Some(name) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
        {
            schema = bundle.defs.get(name)?;
            continue;
        }
        schema = schema
            .get("anyOf")
            .and_then(Value::as_array)?
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))?;
    }
    None
}

/// `data`'s members `props` declares, under the name `props` uses (as is, in
/// snake_case or in camelCase), each enum value as `props` spells it ([`fit_enum`]).
pub(crate) fn reshape(
    props: &Map<String, Value>,
    defs: &Map<String, Value>,
    data: &Value,
) -> Value {
    let mut budget = RESHAPE_OBJECTS;
    reshape_at(props, defs, data, (0, &mut budget))
}

/// The properties of the object `schema` is, through `$ref`s and a nullable
/// `anyOf`, among `defs`.
fn object_props<'a>(
    defs: &'a Map<String, Value>,
    schema: &'a Value,
) -> Option<&'a Map<String, Value>> {
    let mut schema = schema;
    for _ in 0..4 {
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            return Some(props);
        }
        if let Some(name) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
        {
            schema = defs.get(name)?;
            continue;
        }
        schema = schema
            .get("anyOf")
            .and_then(Value::as_array)?
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))?;
    }
    None
}

/// How many levels of objects and lists [`reshape`] answers in the reading shape.
const RESHAPE_DEPTH: usize = 6;
/// How many objects one [`reshape`] answers in the reading shape, at most: a nested
/// object past it is kept as the record holds it.
const RESHAPE_OBJECTS: usize = 400;

/// `schema` past its `$ref`s and a nullable `anyOf` (a union of one type and `null`),
/// among `defs`.
fn unwrapped<'a>(defs: &'a Map<String, Value>, schema: &'a Value) -> &'a Value {
    let mut schema = schema;
    for _ in 0..6 {
        if let Some(def) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|n| defs.get(n))
        {
            schema = def;
            continue;
        }
        let branches: Vec<&Value> = schema
            .get("anyOf")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        match branches.as_slice() {
            [one] => schema = one,
            _ => break,
        }
    }
    schema
}

/// The properties of the object type `schema` holds `value` as: its own, else, for a
/// union or an interface (GraphQL's `owner: RepositoryOwner`, a `oneOf` of its object
/// types), the type `value`'s `__typename` names, else the one declaring most of
/// `value`'s members (the first of those declaring as many); `None` when none
/// declares any. An `anyOf` of one type and `null` is that type.
fn props_for<'a>(
    defs: &'a Map<String, Value>,
    schema: &'a Value,
    value: &Value,
) -> Option<&'a Map<String, Value>> {
    if let Some(props) = object_props(defs, schema) {
        return Some(props);
    }
    let schema = unwrapped(defs, schema);
    let branches: Vec<&Value> = ["oneOf", "anyOf"]
        .iter()
        .filter_map(|k| schema.get(*k).and_then(Value::as_array))
        .flatten()
        .collect();
    let named = value
        .get("__typename")
        .and_then(Value::as_str)
        .and_then(|t| {
            branches.iter().find(|b| {
                b.get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| r.strip_prefix("#/$defs/"))
                    == Some(t)
            })
        });
    if let Some(branch) = named {
        return object_props(defs, branch);
    }
    let members = value.as_object()?;
    let mut best: Option<(&Map<String, Value>, usize)> = None;
    for props in branches.iter().filter_map(|b| object_props(defs, b)) {
        let declared = members
            .keys()
            .filter(|k| {
                [(*k).clone(), snake(k), camel(k)]
                    .iter()
                    .any(|n| props.contains_key(n))
            })
            .count();
        if best.is_none_or(|(_, most)| declared > most) {
            best = Some((props, declared));
        }
    }
    // A value no branch declares a member of is kept as it is.
    best.filter(|(_, most)| *most > 0).map(|(props, _)| props)
}

/// Whether member `name` names a record (`id`, `node_id`, `owner_id`, `ownerId`): one
/// whose value is not turned into another JSON type, but left to what the shape maps
/// to it or fill writes.
fn names_a_record(name: &str) -> bool {
    let snaked = snake(name);
    snaked == "id" || snaked.ends_with("_id")
}

/// `value`, a number another shape holds where `schema` declares a plain string (no
/// `format`, `enum` or `pattern`), as that string; `None` otherwise.
fn stringified(schema: &Value, defs: &Map<String, Value>, value: &Value) -> Option<Value> {
    let Value::Number(n) = value else {
        return None;
    };
    let text = Value::String(n.to_string());
    let plain = unwrapped(defs, schema);
    let constrained = ["format", "enum", "pattern", "const"]
        .iter()
        .any(|k| plain.get(*k).is_some());
    (plain.get("type").and_then(Value::as_str) == Some("string")
        && !constrained
        && of_type(schema, defs, &text))
    .then_some(text)
}

/// [`reshape`] `depth` levels down, `budget` objects more at most: an object member,
/// and each object of a list member, is reshaped as its own schema declares it (a
/// GraphQL `repository: {id: "R_1"}` where REST's repository id is a number loses its
/// id; REST's `owner: {id: 42, node_id: "U_1"}` read as GraphQL's `RepositoryOwner`
/// shows `id: "U_1"`), to [`RESHAPE_DEPTH`] levels.
fn reshape_at(
    props: &Map<String, Value>,
    defs: &Map<String, Value>,
    data: &Value,
    (depth, budget): (usize, &mut usize),
) -> Value {
    let mut out = Map::new();
    for (key, value) in data.as_object().into_iter().flatten() {
        let Some((name, schema)) = [key.clone(), snake(key), camel(key)]
            .into_iter()
            .find_map(|n| props.get(&n).map(|s| (n, s)))
        else {
            continue;
        };
        let mut value = fit_enum(schema, defs, value);
        // A value of another JSON type than the member holds (GraphQL's `"I_1"` id
        // where REST's is a number) is left out for what the shape maps to it (REST's
        // `node_id` as GraphQL's `id`) or fill to write; a number where a plain string
        // is declared is that string, unless it names a record.
        if !of_type(schema, defs, &value) {
            match stringified(schema, defs, &value).filter(|_| !names_a_record(&name)) {
                Some(text) => value = text,
                None => continue,
            }
        }
        if depth < RESHAPE_DEPTH {
            value = reshape_inner(schema, defs, value, (depth, &mut *budget));
        }
        out.insert(name, value);
    }
    // REST's `node_id` is GraphQL's `id`: each fills the other where the reading
    // shape declares it and the record holds none it can take.
    for (to, from) in [("node_id", "id"), ("id", "node_id")] {
        if out.contains_key(to) {
            continue;
        }
        let (Some(schema), Some(value)) = (props.get(to), data.get(from)) else {
            continue;
        };
        if value.is_string() && of_type(schema, defs, value) {
            out.insert(to.to_string(), value.clone());
        }
    }
    unflattened(props, defs, data, &mut out);
    flattened(props, defs, data, &mut out);
    Value::Object(out)
}

/// `value`, a member `schema` declares, with each object it is or a list of it holds
/// reshaped as `schema` declares that object ([`props_for`]), while `budget` lasts.
fn reshape_inner(
    schema: &Value,
    defs: &Map<String, Value>,
    value: Value,
    (depth, budget): (usize, &mut usize),
) -> Value {
    match value {
        Value::Object(_) => {
            let Some(inner) = props_for(defs, schema, &value) else {
                return value;
            };
            if *budget == 0 {
                return value;
            }
            *budget -= 1;
            reshape_at(inner, defs, &value, (depth + 1, budget))
        }
        Value::Array(items) => {
            let Some(item_schema) = unwrapped(defs, schema).get("items") else {
                return Value::Array(items);
            };
            Value::Array(
                items
                    .into_iter()
                    .map(|item| match item {
                        // An enum value of the list.
                        Value::String(_) => fit_enum(item_schema, defs, &item),
                        item => reshape_inner(item_schema, defs, item, (depth, &mut *budget)),
                    })
                    .collect(),
            )
        }
        other => other,
    }
}

/// The names a member whose words are `words` may go by in a shape: as is in
/// snake_case or in camelCase.
fn spellings(words: &[String]) -> [String; 2] {
    let snaked = words.join("_");
    let camelled = camel(&snaked);
    [snaked, camelled]
}

/// The word lists a flattened member named by `words` may stand for: its words, then,
/// when they end in `name`, those before it (`headRefName` is the name of the head's
/// ref, REST's `head.ref`).
fn flat_forms(words: &[String]) -> Vec<&[String]> {
    let mut forms = vec![words];
    if words.len() > 2 && words.last().is_some_and(|w| w == "name") {
        forms.push(&words[..words.len() - 1]);
    }
    forms
}

/// Whether `value` is a scalar a flattened member can carry.
fn scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

/// The scalar members of `data` that the reading shape (`props`) holds one object down
/// rather than at this level, set there in `out`: GraphQL's `headRefName` as REST's
/// `head.ref`, a member named by the object's words and then the member's (with a
/// trailing `name` read as the member itself, see [`flat_forms`]), of a JSON type the
/// member allows. A member the shape declares at this level, a value `out` holds
/// already, and a place two members of `data` would both fill are left alone.
fn unflattened(
    props: &Map<String, Value>,
    defs: &Map<String, Value>,
    data: &Value,
    out: &mut Map<String, Value>,
) {
    let mut found: Vec<(String, String, Value)> = Vec::new();
    let mut doubled: Vec<(String, String)> = Vec::new();
    for (key, value) in data.as_object().into_iter().flatten() {
        if !scalar(value)
            || [key.clone(), snake(key), camel(key)]
                .iter()
                .any(|n| props.contains_key(n))
        {
            continue;
        }
        let named = words(key);
        let place = flat_forms(&named).into_iter().find_map(|form| {
            (1..form.len()).find_map(|split| {
                spellings(&form[..split]).into_iter().find_map(|outer| {
                    let inner_props = object_props(defs, props.get(&outer)?)?;
                    spellings(&form[split..]).into_iter().find_map(|inner| {
                        let schema = inner_props.get(&inner)?;
                        let value = fit_enum(schema, defs, value);
                        of_type(schema, defs, &value).then(|| (outer.clone(), inner, value))
                    })
                })
            })
        });
        let Some((outer, inner, value)) = place else {
            continue;
        };
        if found.iter().any(|(o, i, _)| *o == outer && *i == inner) {
            doubled.push((outer, inner));
            continue;
        }
        found.push((outer, inner, value));
    }
    for (outer, inner, value) in found {
        if doubled.iter().any(|(o, i)| *o == outer && *i == inner) {
            continue;
        }
        let holder = out
            .entry(outer)
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(holder) = holder {
            holder.entry(inner).or_insert(value);
        }
    }
}

/// The scalar members of an object member of `data` that the reading shape (`props`)
/// does not declare, set in `out` at this level where the shape holds them flattened:
/// REST's `head.ref` as GraphQL's `headRefName`, the reverse of [`unflattened`] (the
/// object's words and the member's, else with `name` after them). A member `out` holds
/// already, and one two members would both fill, are left alone.
fn flattened(
    props: &Map<String, Value>,
    defs: &Map<String, Value>,
    data: &Value,
    out: &mut Map<String, Value>,
) {
    let mut found: Vec<(String, Value)> = Vec::new();
    let mut doubled: Vec<String> = Vec::new();
    for (key, value) in data.as_object().into_iter().flatten() {
        let Value::Object(inner) = value else {
            continue;
        };
        if [key.clone(), snake(key), camel(key)]
            .iter()
            .any(|n| props.contains_key(n))
        {
            continue;
        }
        for (member, value) in inner.iter().filter(|(_, v)| scalar(v)) {
            let mut path = words(key);
            path.extend(words(member));
            let named: Vec<String> = path.iter().cloned().chain(["name".to_string()]).collect();
            let place = [path.as_slice(), named.as_slice()]
                .into_iter()
                .find_map(|form| {
                    spellings(form).into_iter().find_map(|name| {
                        let schema = props.get(&name)?;
                        let value = fit_enum(schema, defs, value);
                        of_type(schema, defs, &value).then_some((name, value))
                    })
                });
            let Some((name, value)) = place else {
                continue;
            };
            if found.iter().any(|(n, _)| *n == name) {
                doubled.push(name);
                continue;
            }
            found.push((name, value));
        }
    }
    for (name, value) in found {
        if !doubled.contains(&name) {
            out.entry(name).or_insert(value);
        }
    }
}

/// Whether `value` is of a JSON type `schema` allows (through a `$ref` and the
/// branches of an `anyOf`/`oneOf`); true where it declares none.
fn of_type(schema: &Value, defs: &Map<String, Value>, value: &Value) -> bool {
    fn allows(schema: &Value, defs: &Map<String, Value>, value: &Value, depth: usize) -> bool {
        if depth > 8 {
            return true;
        }
        if let Some(def) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|n| defs.get(n))
        {
            return allows(def, defs, value, depth + 1);
        }
        let branches: Vec<&Value> = ["anyOf", "oneOf"]
            .iter()
            .filter_map(|k| schema.get(*k).and_then(Value::as_array))
            .flatten()
            .collect();
        if !branches.is_empty() {
            return branches.iter().any(|b| allows(b, defs, value, depth + 1));
        }
        let types: Vec<&str> = match schema.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            _ => return true,
        };
        types.iter().any(|t| match *t {
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "object" => value.is_object(),
            "array" => value.is_array(),
            "null" => value.is_null(),
            _ => true,
        })
    }
    allows(schema, defs, value, 0)
}

/// `value` as the enum `schema` lists spells it, when only its case and separators
/// differ (GraphQL's `CLOSED` as REST's `closed`, `REQUIRES_ACTION` as
/// `requires-action` or `RequiresAction`): kept as it is when the enum lists it, or
/// lists no value, or more than one, spelled so.
fn fit_enum(schema: &Value, defs: &Map<String, Value>, value: &Value) -> Value {
    let Value::String(s) = value else {
        return value.clone();
    };
    let mut schema = schema;
    for _ in 0..4 {
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            let listed: Vec<&str> = values.iter().filter_map(Value::as_str).collect();
            if listed.contains(&s.as_str()) {
                return value.clone();
            }
            let spelled = enum_spelling(s);
            let mut alike = listed.into_iter().filter(|v| enum_spelling(v) == spelled);
            return match (alike.next(), alike.next()) {
                (Some(v), None) => Value::String(v.to_string()),
                _ => value.clone(),
            };
        }
        if let Some(def) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|n| defs.get(n))
        {
            schema = def;
            continue;
        }
        match schema.get("anyOf").and_then(Value::as_array).and_then(|b| {
            b.iter()
                .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))
        }) {
            Some(branch) => schema = branch,
            None => break,
        }
    }
    value.clone()
}

/// An enum value as [`fit_enum`] compares it: lowercased, its `_`, `-` and spaces
/// dropped (so a camelCase boundary is no different).
fn enum_spelling(value: &str) -> String {
    value
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// The record types of the mocks of each `shares` group, by workspace and mock: what
/// the names of a type in the group's other mocks are inferred from, when the world
/// names none (a REST collection and a GraphQL type of one identity, see
/// `docs/configuration.md`, `mocks.<name>.shares`). Clones share one registry. A
/// server sets it to the mocks it has mounted ([`SharedTypes::mounted`]) each time
/// it publishes them, so a mock deleted, unmounted or left without a contract
/// leaves it; [`SharedTypes::join`] adds one runtime (tests, embedders).
#[derive(Clone, Debug, Default)]
pub struct SharedTypes(Arc<RwLock<Registry>>);

/// What a mock adds to [`SharedTypes`]: its `shares` group and the types it stores
/// records under. Made once per built runtime ([`SharedEntry::of`]).
#[derive(Clone, Debug)]
pub struct SharedEntry {
    group: String,
    types: Arc<Vec<RecordType>>,
    /// Its types by each form of their name ([`forms`]): two types are one by name when
    /// they share a form, so a lookup here stands for a pass over every type (thousands
    /// in GitHub's GraphQL schema, for each type an answer reaches).
    by_form: Arc<HashMap<String, Vec<usize>>>,
    /// What each of its types read by path shows of where its records live, by type.
    twins: Arc<HashMap<String, Arc<Twin>>>,
    /// Its parent-scoped types ([`scopes`]), by type.
    scopes: Arc<HashMap<String, scopes::ParentScope>>,
}

impl SharedEntry {
    /// `rt`'s entry; `None` when it shares with no other mock.
    #[must_use]
    pub fn of(rt: &MockRuntime) -> Option<Self> {
        let group = rt.shares.clone()?;
        let types = record_types(&rt.bundle);
        let twins = types
            .iter()
            .filter_map(|r| Some((r.key.to_lowercase(), Arc::new(twin_of(rt, &r.key)?))))
            .collect();
        let scopes = types
            .iter()
            .filter_map(|r| {
                let scope = rt.index().parent_scopes().get(&r.key)?;
                Some((r.key.to_lowercase(), scope.clone()))
            })
            .collect();
        let mut entry = Self::with(group, types, twins);
        entry.scopes = Arc::new(scopes);
        Some(entry)
    }

    fn with(group: String, types: Vec<RecordType>, twins: HashMap<String, Arc<Twin>>) -> Self {
        let mut by_form: HashMap<String, Vec<usize>> = HashMap::new();
        for (at, r) in types.iter().enumerate() {
            for form in forms(&r.key) {
                by_form.entry(form).or_default().push(at);
            }
        }
        Self {
            group,
            types: Arc::new(types),
            by_form: Arc::new(by_form),
            twins: Arc::new(twins),
            scopes: Arc::default(),
        }
    }

    /// Whether one of its types is the type named `forms` by name, other than `except`.
    fn named(&self, forms: &[String], except: Option<&str>) -> bool {
        forms.iter().any(|f| {
            self.by_form.get(f).is_some_and(|at| {
                at.iter()
                    .any(|&i| except.is_none_or(|key| self.types[i].key != key))
            })
        })
    }
}

#[derive(Debug, Default)]
struct Registry {
    /// Each mock's entry, by workspace and mock name.
    mocks: HashMap<(String, String), SharedEntry>,
    /// What [`SharedTypes::inferred`] found, by workspace, mock and type; cleared
    /// whenever the mocks change.
    inferred: HashMap<(String, String, String), Arc<Vec<String>>>,
    /// What [`SharedTypes::twin`] found, likewise.
    twins: HashMap<(String, String, String), Option<Arc<Twin>>>,
    /// Counts the changes of the mocks: a twin found across one is not kept.
    generation: u64,
}

impl Registry {
    /// Forgets what was worked out of the mocks, which changed.
    fn changed(&mut self) {
        self.inferred.clear();
        self.twins.clear();
        self.generation = self.generation.wrapping_add(1);
    }
}

/// Inferred names kept at most, before the memo starts over.
const MAX_INFERRED: usize = 4096;

/// A type a mock stores records under, as [`SharedTypes`] compares it with the
/// types of the other mocks of its group.
#[derive(Debug, Clone, Default, PartialEq)]
struct RecordType {
    /// The name its records are stored under, as written.
    key: String,
    /// Each name it goes by: its own, and the schema its records are answered in
    /// (REST's `pulls` holds `pull-request`s).
    names: Vec<Name>,
    /// The identity members its records declare.
    number: bool,
    id: bool,
    node_id: bool,
}

impl RecordType {
    /// Adds the name of `words`, unless it has it.
    fn named(&mut self, words: Vec<String>) {
        if !self.names.iter().any(|n| n.words == words) {
            self.names.push(Name::of(words));
        }
    }
}

/// A name of a [`RecordType`], with what [`names_match`] compares of it worked out
/// once: a mock's types are compared with every type of the group's other mocks.
#[derive(Debug, Clone, Default, PartialEq)]
struct Name {
    /// Its words (see [`words`]).
    words: Vec<String>,
    /// Its words run together.
    whole: String,
    /// `whole` and its [`singulars`]: two names are one word but for a plural when
    /// they share one.
    forms: Vec<String>,
    /// For a one-word name, its singulars of three letters or more ([`abbreviates`]).
    short: Vec<String>,
}

impl Name {
    fn of(words: Vec<String>) -> Self {
        let whole = words.concat();
        let forms = singulars(&whole).into_iter().map(str::to_string).collect();
        let short = match words.as_slice() {
            [word] => singulars(word)
                .into_iter()
                .filter(|s| s.len() >= 3)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        Self {
            words,
            whole,
            forms,
            short,
        }
    }
}

impl SharedTypes {
    /// Records `rt`'s types under its `shares` group (and none when it shares with
    /// no other), in place of what it recorded before.
    pub fn join(&self, rt: &MockRuntime) {
        let key = (rt.workspace.clone(), rt.name.clone());
        let entry = SharedEntry::of(rt);
        let mut registry = self.0.write().unwrap_or_else(PoisonError::into_inner);
        registry.changed();
        match entry {
            Some(entry) => {
                registry.mocks.insert(key, entry);
            }
            None => {
                registry.mocks.remove(&key);
            }
        }
    }

    /// Holds exactly `mocks` (by workspace and name): what a server has mounted.
    pub fn mounted(&self, mocks: impl IntoIterator<Item = ((String, String), SharedEntry)>) {
        let mocks: HashMap<(String, String), SharedEntry> = mocks.into_iter().collect();
        let mut registry = self.0.write().unwrap_or_else(PoisonError::into_inner);
        registry.changed();
        registry.mocks = mocks;
    }

    /// The types of the other mocks of `rt`'s group that are `t` (see
    /// [`names_match`] and [`same_identity`]). None when a type of the group is `t`
    /// by name already (see [`equivalents`]), and a peer type is passed over when
    /// another type of `rt`'s own is it by name.
    fn inferred(&self, rt: &MockRuntime, t: &str) -> Arc<Vec<String>> {
        if rt.shares.is_none() {
            return Arc::default();
        }
        let memo = (rt.workspace.clone(), rt.name.clone(), t.to_string());
        if let Some(hit) = self
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .inferred
            .get(&memo)
        {
            return Arc::clone(hit);
        }
        // Computed under the write lock, so a change of the mocks in between cannot
        // leave a stale answer in the memo.
        let mut registry = self.0.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(hit) = registry.inferred.get(&memo) {
            return Arc::clone(hit);
        }
        let found = Arc::new(registry.infer((&rt.workspace, &rt.name), t));
        if registry.inferred.len() >= MAX_INFERRED {
            registry.inferred.clear();
        }
        registry.inferred.insert(memo, Arc::clone(&found));
        found
    }
}

impl SharedTypes {
    /// How another mock of `rt`'s group keeps its type `t` under a parent, when it does
    /// ([`scopes`]): what a delete of such a record through `rt` marks it by.
    pub(crate) fn parent_scope(&self, rt: &MockRuntime, t: &str) -> Option<scopes::ParentScope> {
        rt.shares.as_ref()?;
        let registry = self.0.read().unwrap_or_else(PoisonError::into_inner);
        let group = &registry
            .mocks
            .get(&(rt.workspace.clone(), rt.name.clone()))?
            .group;
        let mut peers: Vec<(&String, &SharedEntry)> = registry
            .mocks
            .iter()
            .filter(|((workspace, mock), entry)| {
                *workspace == rt.workspace && *mock != rt.name && entry.group == *group
            })
            .map(|((_, mock), entry)| (mock, entry))
            .collect();
        peers.sort_by(|a, b| a.0.cmp(b.0));
        let t = t.to_lowercase();
        peers
            .iter()
            .find_map(|(_, entry)| entry.scopes.get(&t).cloned())
    }

    /// The type of another mock of `rt`'s group that is `t` (see [`equivalents`]) and
    /// shows where its records live ([`Twin`]): what a type with no read by path builds
    /// its own links from. Remembered per type until the mocks change.
    pub(crate) fn twin(&self, rt: &MockRuntime, t: &str) -> Option<Arc<Twin>> {
        rt.shares.as_ref()?;
        let memo = (rt.workspace.clone(), rt.name.clone(), t.to_string());
        let generation = {
            let registry = self.0.read().unwrap_or_else(PoisonError::into_inner);
            if let Some(hit) = registry.twins.get(&memo) {
                return hit.clone();
            }
            registry.generation
        };
        // Outside the lock: what `t` goes by in the group reads the registry itself.
        // Its own name first: another mock may store the type under it.
        let mut names = vec![t.to_lowercase()];
        names.extend(equivalents(rt, t));
        let mut registry = self.0.write().unwrap_or_else(PoisonError::into_inner);
        let found = registry.find_twin(rt, &names);
        if registry.generation == generation {
            if registry.twins.len() >= MAX_INFERRED {
                registry.twins.clear();
            }
            registry.twins.insert(memo, found.clone());
        }
        found
    }
}

impl Registry {
    /// The first twin a type of another mock of `rt`'s group named one of `names`
    /// has, by mock name.
    fn find_twin(&self, rt: &MockRuntime, names: &[String]) -> Option<Arc<Twin>> {
        let group = &self
            .mocks
            .get(&(rt.workspace.clone(), rt.name.clone()))?
            .group;
        let mut peers: Vec<(&String, &SharedEntry)> = self
            .mocks
            .iter()
            .filter(|((workspace, mock), entry)| {
                *workspace == rt.workspace && *mock != rt.name && entry.group == *group
            })
            .map(|((_, mock), entry)| (mock, entry))
            .collect();
        peers.sort_by(|a, b| a.0.cmp(b.0));
        names.iter().find_map(|name| {
            let name = name.to_lowercase();
            peers
                .iter()
                .find_map(|(_, entry)| entry.twins.get(&name).cloned())
        })
    }

    /// [`SharedTypes::inferred`] without the memo.
    fn infer(&self, (workspace, name): (&str, &str), t: &str) -> Vec<String> {
        let Some(mine) = self.mocks.get(&(workspace.to_string(), name.to_string())) else {
            return Vec::new();
        };
        let Some(own) = mine.types.iter().find(|r| r.key.eq_ignore_ascii_case(t)) else {
            return Vec::new();
        };
        let peers: Vec<&SharedEntry> = self
            .mocks
            .iter()
            .filter(|((w, mock), entry)| {
                *w == workspace && *mock != name && entry.group == mine.group
            })
            .map(|(_, entry)| entry)
            .collect();
        let own_forms = forms(&own.key);
        if peers.iter().any(|p| p.named(&own_forms, None)) {
            return Vec::new();
        }
        let mut found: Vec<String> = Vec::new();
        for p in peers.iter().flat_map(|entry| entry.types.iter()) {
            // Cheap checks first: most peer types are another identity or name.
            if !same_identity(own, p) || !names_match(own, p) || found.contains(&p.key) {
                continue;
            }
            // A peer type another of this mock's types is by name is that one's.
            if !mine.named(&forms(&p.key), Some(&own.key)) {
                found.push(p.key.clone());
            }
        }
        found
    }
}

/// The types `bundle` stores records under: each operation's resource type, with the
/// schema its success answer holds a record of, and for a GraphQL contract each object
/// type with an `id` (stored under its name in lower case), whose definition adds its
/// name and identity to the operations' type of that name.
fn record_types(bundle: &mb_mcm::Bundle) -> Vec<RecordType> {
    let mut out: Vec<RecordType> = Vec::new();
    let mut graphql = false;
    for op in &bundle.operations {
        graphql |= matches!(op.binding, Binding::Graphql { .. });
        let Some(key) = op.resource_type.as_deref() else {
            continue;
        };
        let at = match out.iter().position(|r| r.key == key) {
            Some(at) => at,
            None => {
                out.push(RecordType {
                    key: key.to_string(),
                    names: vec![Name::of(words(key))],
                    ..RecordType::default()
                });
                out.len() - 1
            }
        };
        let Some((name, props)) = op
            .success_output()
            .and_then(|o| o.schema.as_ref())
            .and_then(|s| record_schema(bundle, s))
        else {
            continue;
        };
        let entry = &mut out[at];
        if let Some(name) = name {
            entry.named(words(name));
        }
        entry.number |= props.contains_key("number");
        entry.id |= props.contains_key("id");
        entry.node_id |= props.contains_key("node_id");
    }
    if graphql {
        for (name, def) in &bundle.defs {
            let Some(props) = def.get("properties").and_then(Value::as_object) else {
                continue;
            };
            let key = name.to_lowercase();
            if !props.contains_key("id") {
                continue;
            }
            let number = props.contains_key("number");
            let node_id = props.contains_key("node_id");
            // A type a mutation creates is stored under its name too, but its success
            // answer is the mutation's payload (`CreatePullRequestPayload`), which
            // declares none of the type's identity: the type's own definition does.
            match out.iter_mut().find(|r| r.key.to_lowercase() == key) {
                Some(entry) => {
                    entry.named(words(name));
                    entry.number |= number;
                    entry.id = true;
                    entry.node_id |= node_id;
                }
                None => out.push(RecordType {
                    key,
                    names: vec![Name::of(words(name))],
                    number,
                    id: true,
                    node_id,
                }),
            }
        }
    }
    out
}

/// The definition name (when it is one) and the properties of the record `schema`
/// answers: through `$ref`s, a nullable `anyOf` and a list's items.
fn record_schema<'a>(
    bundle: &'a mb_mcm::Bundle,
    schema: &'a Value,
) -> Option<(Option<&'a str>, &'a Map<String, Value>)> {
    let mut schema = schema;
    let mut name = None;
    for _ in 0..6 {
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            return Some((name, props));
        }
        if let Some(def) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
        {
            name = Some(def);
            schema = bundle.defs.get(def)?;
            continue;
        }
        if let Some(items) = schema.get("items") {
            schema = items;
            continue;
        }
        schema = schema
            .get("anyOf")
            .and_then(Value::as_array)?
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))?;
    }
    None
}

/// `PullRequest`, `pull-request` and `pull_request` → `[pull, request]`.
fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut after_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            after_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && after_lower && !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
        after_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        word.push(c.to_ascii_lowercase());
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// `t` in lower case without punctuation.
fn normal(t: &str) -> String {
    t.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_lowercase()
}

/// `w` and what it is without a plural `s` or `es`.
fn singulars(w: &str) -> Vec<&str> {
    let mut out = vec![w];
    out.extend(w.strip_suffix("es"));
    out.extend(w.strip_suffix('s'));
    out.retain(|s| !s.is_empty());
    out
}

/// The forms of a type's name `key`: in lower case without punctuation, as it is and
/// without a plural ([`singulars`]). Two keys are one word but for a plural exactly
/// when they share a form.
fn forms(key: &str) -> Vec<String> {
    singulars(&normal(key))
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// REST collections that are a short form of another type's whole name, as their
/// singular and that name without punctuation: GitHub's `pulls` and `PullRequest`.
const SHORT_FORMS: &[(&str, &str)] = &[("pull", "pullrequest")];

/// Whether a name of `a` and one of `b` name the same type: their whole names agree
/// but for case, punctuation and a plural (REST `pulls` answering `pull-request`s
/// and GraphQL `PullRequest`), or one is a short form of the other (see
/// [`abbreviates`]). A name is never matched by one of its words alone
/// (`users` is not `UserStatus`).
fn names_match(a: &RecordType, b: &RecordType) -> bool {
    a.names.iter().any(|x| {
        b.names.iter().any(|y| {
            x.forms.iter().any(|f| y.forms.contains(f)) || abbreviates(x, y) || abbreviates(y, x)
        })
    })
}

/// Whether the one word `x` (a REST collection: `repos`, `orgs`, `pulls`) is a short
/// form of the whole of `y`: the start of its one word, at least three letters
/// longer (`repository`, `organization`), or a listed short form ([`SHORT_FORMS`]).
fn abbreviates(x: &Name, y: &Name) -> bool {
    x.short.iter().any(|s| match y.words.as_slice() {
        [word] if word.len() >= s.len() + 3 && word.starts_with(s.as_str()) => true,
        _ => SHORT_FORMS
            .iter()
            .any(|(short, long)| short == s && *long == y.whole),
    })
}

/// Whether `a` and `b` declare the same identity: a `number` both or neither, and
/// then an `id` or `node_id` each (REST's `node_id` is GraphQL's `id`).
fn same_identity(a: &RecordType, b: &RecordType) -> bool {
    a.number == b.number && (a.number || ((a.id || a.node_id) && (b.id || b.node_id)))
}

/// `createdAt` → `created_at`.
fn snake(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            if !out.is_empty() {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `created_at` → `createdAt`.
fn camel(name: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for c in name.chars() {
        if c == '_' {
            upper = !out.is_empty();
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A GraphQL type a mutation creates, read through no query of its own, has the
    /// identity its definition declares, not its payload's.
    #[test]
    fn a_type_created_through_a_payload_has_its_definition_s_identity() {
        let bundle = mb_import::compile(
            br#"
type PullRequest { id: ID! number: Int! title: String! }
input CreatePullRequestInput { title: String! }
type CreatePullRequestPayload { pullRequest: PullRequest }
type Query { viewer: String }
type Mutation { createPullRequest(input: CreatePullRequestInput!): CreatePullRequestPayload }
"#,
        )
        .unwrap();
        let types = record_types(&bundle);
        let own = types.iter().find(|r| r.key == "pullrequest").unwrap();
        assert!(own.number && own.id, "{own:?}");
        assert!(
            own.names.iter().any(|n| n.words == words("PullRequest")),
            "{own:?}"
        );
    }

    fn record_type(names: &[&str], identity: &[&str]) -> RecordType {
        RecordType {
            key: names[0].to_lowercase(),
            names: names.iter().map(|n| Name::of(words(n))).collect(),
            number: identity.contains(&"number"),
            id: identity.contains(&"id"),
            node_id: identity.contains(&"node_id"),
        }
    }

    #[test]
    fn types_are_inferred_by_name_and_identity() {
        let linked = |a: &RecordType, b: &RecordType| same_identity(a, b) && names_match(a, b);
        let pulls = record_type(&["pulls", "pull-request"], &["id", "node_id", "number"]);
        let repos = record_type(&["repos", "full-repository"], &["id", "node_id"]);
        let orgs = record_type(&["orgs", "organization-full"], &["id", "node_id"]);
        let issues = record_type(&["issues", "issue"], &["id", "node_id", "number"]);
        assert_eq!(words("PullRequest"), ["pull", "request"]);
        assert_eq!(words("check-runs"), ["check", "runs"]);
        assert!(linked(
            &pulls,
            &record_type(&["PullRequest"], &["id", "number"])
        ));
        // A listed short form, without the record schema's name.
        assert!(linked(
            &record_type(&["pulls"], &["id", "number"]),
            &record_type(&["PullRequest"], &["id", "number"])
        ));
        assert!(linked(&repos, &record_type(&["Repository"], &["id"])));
        assert!(linked(&orgs, &record_type(&["Organization"], &["id"])));
        // Not a short form: three words, or a word barely longer.
        assert!(!linked(
            &pulls,
            &record_type(&["PullRequestReview"], &["id"])
        ));
        assert!(!linked(&repos, &record_type(&["Report"], &["id"])));
        // A collection is never matched by the first word of a compound name.
        for (rest, graphql) in [
            ("users", "UserStatus"),
            ("users", "UserList"),
            ("releases", "ReleaseAsset"),
            ("gists", "GistComment"),
            ("commits", "CommitComment"),
            ("deployments", "DeploymentStatus"),
        ] {
            let rest = record_type(&[rest], &["id", "node_id"]);
            assert!(
                !linked(&rest, &record_type(&[graphql], &["id"])),
                "{graphql}"
            );
        }
        // A name alone is not enough: one has a `number`, the other none.
        assert!(!linked(&issues, &record_type(&["IssueComment"], &["id"])));
        assert!(!linked(&pulls, &record_type(&["Pull"], &["id"])));
    }

    /// A registry of a GraphQL mock (`gql`) and a REST one (`rest`) of one group.
    fn registry(gql: Vec<RecordType>, rest: Vec<RecordType>) -> Registry {
        let entry = |types| SharedEntry::with("api".into(), types, HashMap::new());
        let mut registry = Registry::default();
        registry
            .mocks
            .insert(("w".into(), "gql".into()), entry(gql));
        registry
            .mocks
            .insert(("w".into(), "rest".into()), entry(rest));
        registry
    }

    #[test]
    fn a_type_is_inferred_unless_a_type_is_it_by_name() {
        let registry = registry(
            vec![
                record_type(&["PullRequest"], &["id", "number"]),
                record_type(&["Repository"], &["id"]),
                record_type(&["Issue"], &["id", "number"]),
                record_type(&["UserStatus"], &["id"]),
                record_type(&["Organization"], &["id"]),
                record_type(&["Orgs"], &["id"]),
            ],
            vec![
                record_type(&["pulls", "pull-request"], &["id", "node_id", "number"]),
                record_type(&["repos", "full-repository"], &["id", "node_id"]),
                record_type(&["issues", "issue"], &["id", "node_id", "number"]),
                record_type(&["users"], &["id", "node_id"]),
                record_type(&["orgs", "organization-full"], &["id", "node_id"]),
            ],
        );
        let infer = |t: &str| registry.infer(("w", "gql"), t);
        assert_eq!(infer("pullrequest"), ["pulls"]);
        assert_eq!(infer("Repository"), ["repos"]);
        // The group has an `issues` already: equivalent by name, nothing to infer.
        assert!(infer("issue").is_empty());
        assert!(infer("userstatus").is_empty());
        // `orgs` is this mock's own `Orgs` by name, not `Organization`.
        assert!(infer("organization").is_empty());
        // Another workspace, or a type the mock does not store, infers nothing.
        assert!(registry.infer(("v", "gql"), "pullrequest").is_empty());
        assert!(infer("comment").is_empty());
        // And the other way round.
        assert_eq!(registry.infer(("w", "rest"), "pulls"), ["pullrequest"]);
    }

    /// GitHub's GraphQL schema has about a thousand record types and its REST
    /// contract hundreds; an answer through `node(id:)` reaches hundreds of them. Each
    /// is inferred by lookups, not a pass over every pair of types (which took a
    /// minute for one such answer).
    #[test]
    fn inferring_every_type_of_large_contracts_is_quick() {
        let gql: Vec<RecordType> = (0..1500)
            .map(|i| record_type(&[&format!("Thing{i}Kind")], &["id"]))
            .collect();
        let rest: Vec<RecordType> = (0..500)
            .map(|i| record_type(&[&format!("thing{i}-kinds")], &["id", "node_id"]))
            .collect();
        let registry = registry(gql, rest);
        let started = std::time::Instant::now();
        let mut found = 0;
        for i in 0..1500 {
            found += registry.infer(("w", "gql"), &format!("thing{i}kind")).len();
        }
        // The first 500 are a REST type by name already; the others are none, after a
        // look at every REST type.
        assert_eq!(found, 0);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_record_fits_the_owner_chain_it_shows() {
        let chain = ["a".to_string(), "r".to_string()];
        let fits = |data: Value| chain_fits(&chain, &data);
        assert!(fits(json!({"number": 5})));
        assert!(fits(json!({"url": "https://github.com/a/r/pull/5"})));
        assert!(!fits(json!({"url": "https://github.com/b/r/pull/5"})));
        assert!(fits(json!({"repository": {"full_name": "a/r"}})));
        assert!(!fits(
            json!({"repository": {"name": "r", "owner": {"login": "b"}}})
        ));
        assert!(fits(
            json!({"repository": {"name": "r", "owner": {"login": "a"}}})
        ));
        assert!(!fits(json!({"baseRepository": {"nameWithOwner": "b/r"}})));
        // Prose with a slash is no path, and a member that is no link says nothing.
        assert!(fits(json!({"title": "fix b/r and more", "body": "b/r"})));
        // A fork's pull request belongs to its base: its head names another owner.
        let fork = json!({"base": {"repo": {"full_name": "b/r"}},
                          "head": {"repo": {"full_name": "a/r"}}});
        assert!(!fits(fork.clone()));
        assert!(chain_fits(&["b".to_string(), "r".to_string()], &fork));
        let fork = json!({"baseRepository": {"name": "r", "owner": {"login": "b"}},
                          "headRepository": {"name": "r", "owner": {"login": "a"}},
                          "headRepositoryOwner": {"login": "a"}});
        assert!(!fits(fork));
        // Logins and names compare without case.
        assert!(fits(json!({"url": "https://github.com/A/R/pull/5"})));
        assert!(fits(
            json!({"repository": {"name": "R", "owner": {"login": "A"}}})
        ));
        // A one-parameter chain says nothing beyond the nearest parameter.
        assert!(chain_fits(
            &chain[1..],
            &json!({"repository": {"full_name": "b/r"}})
        ));
    }

    #[test]
    fn reshape_renames_by_case_style_and_fits_enums() {
        let props = json!({
            "number": {"type": "integer"},
            "created_at": {"type": "string"},
            "state": {"type": "string", "enum": ["open", "closed"]}
        });
        let data = json!({"number": 7, "createdAt": "2024-01-01T00:00:00Z", "state": "CLOSED", "author": {}});
        assert_eq!(
            reshape(props.as_object().unwrap(), &Map::new(), &data),
            json!({"number": 7, "created_at": "2024-01-01T00:00:00Z", "state": "closed"})
        );
    }

    #[test]
    fn an_enum_value_takes_the_one_spelling_alike_or_stays() {
        let schema = |values: &[&str]| json!({"type": "string", "enum": values});
        let fit =
            |values: &[&str], value: &str| fit_enum(&schema(values), &Map::new(), &json!(value));
        let reading = ["requires_action", "succeeded"];
        for stored in ["REQUIRES_ACTION", "requires-action", "RequiresAction"] {
            assert_eq!(fit(&reading, stored), json!("requires_action"), "{stored}");
        }
        // No value spelled alike, or two: the stored value stays.
        assert_eq!(fit(&reading, "CANCELED"), json!("CANCELED"));
        assert_eq!(fit(&["on-hold", "on_hold"], "ON HOLD"), json!("ON HOLD"));
        // One the enum lists stays, though another is spelled alike.
        assert_eq!(fit(&["on-hold", "on_hold"], "on_hold"), json!("on_hold"));
    }

    #[test]
    fn reshape_nests_and_flattens_a_member_named_for_its_object() {
        // REST's `head.ref` from GraphQL's `headRefName`; `headRefOid` names nothing
        // REST's head declares, so it is left out.
        let rest = json!({
            "number": {"type": "integer"},
            "head": {"type": "object", "properties": {
                "ref": {"type": "string"}, "sha": {"type": "string"}}},
            "base": {"type": "object", "properties": {"ref": {"type": "string"}}}
        });
        let graphql = json!({"number": 7, "headRefName": "fix-empty-path",
                             "headRefOid": "4b7e", "baseRefName": "main"});
        assert_eq!(
            reshape(rest.as_object().unwrap(), &Map::new(), &graphql),
            json!({"number": 7, "head": {"ref": "fix-empty-path"}, "base": {"ref": "main"}})
        );
        // And back: REST's `head.ref` as GraphQL's `headRefName`; a member of
        // another JSON type (`headRef`, an object) is passed over.
        let gql = json!({
            "number": {"type": "integer"},
            "headRef": {"type": "object", "properties": {"name": {"type": "string"}}},
            "headRefName": {"type": "string"},
            "baseRefName": {"type": "string"}
        });
        let stored = json!({"number": 7, "head": {"ref": "fix-empty-path", "sha": "4b7e"},
                            "base": {"ref": "main"}});
        assert_eq!(
            reshape(gql.as_object().unwrap(), &Map::new(), &stored),
            json!({"number": 7, "headRefName": "fix-empty-path", "baseRefName": "main"})
        );
        // A place two members would both fill is left alone.
        let both = json!({"number": 7, "headRef": "a", "headRefName": "b"});
        assert_eq!(
            reshape(rest.as_object().unwrap(), &Map::new(), &both),
            json!({"number": 7})
        );
    }
}
