//! Records another mock of the same API wrote, for mocks that `shares` one
//! name (GitHub's REST beside its GraphQL): they keep their sessions under
//! it, so each finds what the others stored. A record is the same type
//! when the names agree beyond case, punctuation and a plural `s` (`issues`,
//! `issue`), when the world says so (`types.<t>.same_as`), or, where the world
//! names none, when the group's contracts show it (see [`SharedTypes`]): a REST
//! collection named for the type (`pulls` answering `pull-request`s, `repos`) and
//! a GraphQL type (`PullRequest`, `Repository`) declaring the same identity. It is
//! answered in the reading mock's shape: members renamed between camelCase and
//! snake_case where only that differs, enum values in the reading schema's case,
//! and REST's `node_id` as GraphQL's `id` (and back).

use std::sync::{Arc, PoisonError, RwLock};

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
            if owner.is_none_or(|o| key_fits(rt, session, &own, o)) {
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
/// snake_case or in camelCase), each enum value in the case `props` lists it.
pub(crate) fn reshape(
    props: &Map<String, Value>,
    defs: &Map<String, Value>,
    data: &Value,
) -> Value {
    reshape_at(props, defs, data, 0)
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

/// [`reshape`] `depth` objects down: an object member is reshaped as its own
/// schema declares it (a GraphQL `repository: {id: "R_1"}` where REST's
/// repository id is a number loses its id), to four levels.
fn reshape_at(
    props: &Map<String, Value>,
    defs: &Map<String, Value>,
    data: &Value,
    depth: usize,
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
        // A value of another JSON type than the member holds (GraphQL's `"I_1"`
        // id where REST's is a number) is left out for fill to write.
        if !of_type(schema, defs, &value) {
            continue;
        }
        if value.is_object()
            && depth < 4
            && let Some(inner) = object_props(defs, schema)
        {
            value = reshape_at(inner, defs, &value, depth + 1);
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
    Value::Object(out)
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

/// `value` as the enum `schema` lists spells it, when only its case differs.
fn fit_enum(schema: &Value, defs: &Map<String, Value>, value: &Value) -> Value {
    let Value::String(s) = value else {
        return value.clone();
    };
    let mut schema = schema;
    for _ in 0..4 {
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return values
                .iter()
                .filter_map(Value::as_str)
                .find(|v| *v == s.as_str() || v.eq_ignore_ascii_case(s))
                .map_or_else(|| value.clone(), |v| Value::String(v.to_string()));
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
}

impl SharedEntry {
    /// `rt`'s entry; `None` when it shares with no other mock.
    #[must_use]
    pub fn of(rt: &MockRuntime) -> Option<Self> {
        Some(Self {
            group: rt.shares.clone()?,
            types: Arc::new(record_types(&rt.bundle)),
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
}

/// Inferred names kept at most, before the memo starts over.
const MAX_INFERRED: usize = 4096;

/// A type a mock stores records under, as [`SharedTypes`] compares it with the
/// types of the other mocks of its group.
#[derive(Debug, Clone, Default, PartialEq)]
struct RecordType {
    /// The name its records are stored under, as written.
    key: String,
    /// The words of each name it goes by: its own, and the schema its records are
    /// answered in (REST's `pulls` holds `pull-request`s).
    names: Vec<Vec<String>>,
    /// The identity members its records declare.
    number: bool,
    id: bool,
    node_id: bool,
}

impl SharedTypes {
    /// Records `rt`'s types under its `shares` group (and none when it shares with
    /// no other), in place of what it recorded before.
    pub fn join(&self, rt: &MockRuntime) {
        let key = (rt.workspace.clone(), rt.name.clone());
        let entry = SharedEntry::of(rt);
        let mut registry = self.0.write().unwrap_or_else(PoisonError::into_inner);
        registry.inferred.clear();
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
        registry.inferred.clear();
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
        let found = Arc::new(registry.infer(rt, t));
        if registry.inferred.len() >= MAX_INFERRED {
            registry.inferred.clear();
        }
        registry.inferred.insert(memo, Arc::clone(&found));
        found
    }
}

impl Registry {
    /// [`SharedTypes::inferred`] without the memo.
    fn infer(&self, rt: &MockRuntime, t: &str) -> Vec<String> {
        let Some(mine) = self.mocks.get(&(rt.workspace.clone(), rt.name.clone())) else {
            return Vec::new();
        };
        let Some(own) = mine.types.iter().find(|r| r.key.eq_ignore_ascii_case(t)) else {
            return Vec::new();
        };
        let peers: Vec<&RecordType> = self
            .mocks
            .iter()
            .filter(|((workspace, mock), entry)| {
                *workspace == rt.workspace && *mock != rt.name && entry.group == mine.group
            })
            .flat_map(|(_, entry)| entry.types.iter())
            .collect();
        let by_name = |a: &RecordType, b: &RecordType| same_word(&normal(&a.key), &normal(&b.key));
        if peers.iter().any(|p| by_name(own, p)) {
            return Vec::new();
        }
        let mut found: Vec<String> = Vec::new();
        for p in peers {
            // A peer type another of this mock's types is by name is that one's.
            let taken = mine.types.iter().any(|o| o.key != own.key && by_name(o, p));
            if !taken && same_identity(own, p) && names_match(own, p) && !found.contains(&p.key) {
                found.push(p.key.clone());
            }
        }
        found
    }
}

/// The types `bundle` stores records under: each operation's resource type, with the
/// schema its success answer holds a record of, and for a GraphQL contract each object
/// type with an `id` (stored under its name in lower case).
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
                    names: vec![words(key)],
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
            let name = words(name);
            if !entry.names.contains(&name) {
                entry.names.push(name);
            }
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
            if !props.contains_key("id") || out.iter().any(|r| r.key.to_lowercase() == key) {
                continue;
            }
            out.push(RecordType {
                key,
                names: vec![words(name)],
                number: props.contains_key("number"),
                id: true,
                node_id: props.contains_key("node_id"),
            });
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

/// Whether `a` and `b` are one word but for a plural.
fn same_word(a: &str, b: &str) -> bool {
    singulars(a).iter().any(|x| singulars(b).contains(x))
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
        b.names
            .iter()
            .any(|y| same_word(&x.concat(), &y.concat()) || abbreviates(x, y) || abbreviates(y, x))
    })
}

/// Whether the one word `x` (a REST collection: `repos`, `orgs`, `pulls`) is a short
/// form of the whole of `y`: the start of its one word, at least three letters
/// longer (`repository`, `organization`), or a listed short form ([`SHORT_FORMS`]).
fn abbreviates(x: &[String], y: &[String]) -> bool {
    let [x] = x else {
        return false;
    };
    let whole = y.concat();
    singulars(x)
        .into_iter()
        .filter(|s| s.len() >= 3)
        .any(|s| match y {
            [word] if word.len() >= s.len() + 3 && word.starts_with(s) => true,
            _ => SHORT_FORMS
                .iter()
                .any(|(short, long)| *short == s && *long == whole),
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

    fn record_type(names: &[&str], identity: &[&str]) -> RecordType {
        RecordType {
            key: names[0].to_lowercase(),
            names: names.iter().map(|n| words(n)).collect(),
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
}
