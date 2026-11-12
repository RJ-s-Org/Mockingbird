//! Parent-scoped record identity: which types' records are told apart by their parent,
//! how a stored id carries the parent, and which types number under one parent together.
//!
//! A record is stored by its type and id. Where a type's records are addressed by a key
//! under parent path parameters (`/repos/{owner}/{repo}/issues/{issue_number}`), and the
//! key is not unique on its own, two parents' records of one key are two records: issue
//! `#5` of `a/x` is not issue `#5` of `b/x`, nor is one repository's secret `TOKEN`
//! another's. Such a type is *parent-scoped* ([`ParentScopes::of`]) when:
//!
//! - every route addressing one of its records by a key (a read, an update, a delete, an
//!   action, a `PUT` to the record's own URI) puts parameters before the key: one with
//!   none (`/issues/{id}`, `/repositories/{id}`) addresses the record without its
//!   parent, so its key stands alone and the type stays global;
//! - and the contract does not mark the key unique by itself: a `uuid`/`guid` format, a
//!   pattern with a literal prefix (`^pi_`, `^CA`) or of a fixed long length, a
//!   `minLength` of 16 or more, or examples that all look so (a prefixed id `cus_9s8d…`,
//!   a uuid, an opaque mix of 16 or more letters and digits) keep it global. A number, a
//!   name or slug, or any key the contract says nothing more of, is scoped.
//!
//! The parent is the values of the parameters before the key on the type's own read
//! (its detail read, else its shortest route addressing a record). A call gives a record
//! its parent ([`crate::call::StateAccess`]) where its route runs through the parent's
//! segments ([`ParentScope::of_route`]: `/repos/{owner}/{repo}/issues` does,
//! `/groups/{id}/issues` names no parent of `/projects/{id}/issues/{iid}`'s issues); a
//! record also shows it in its own links: a member whose path holds the read's route
//! with the record's key (`url: …/repos/a/x/issues/5`). The call's own type is under
//! the parent its route names; another type's records under the one their links show,
//! else the call's.
//!
//! A scoped record is stored under its parent's values and its id joined by [`SEP`]
//! ([`scoped_id`]): `a␟x␟2847391056`. The call layer never sees that form: the state
//! access a call reads and writes through translates it both ways, so every lookup,
//! list, write, delete, minted number and the runtime's own documents keyed by a record
//! (the write memory, concurrency tokens) work within the parent, and an id the call
//! layer would pass holding the separator names no record. A record the call found
//! under its parent's key is that parent's in an owner check
//! ([`super::owners::placed_fits`]), unless it shows another owner itself. A reader of
//! another mock (GraphQL beside REST, `shares`) splits it with [`split_id`].
//!
//! A record stored under its own id alone (seeded through the Admin API, written where
//! nothing named its parent, or before its type was scoped) is still found under a
//! parent whose route its own links do not contradict, judged by the owner checks as
//! before; a record of the parent's own under that id hides it there. No migration
//! rewrites such records up front: a write of one under a parent moves it under that
//! parent, and a delete there removes it.
//!
//! Types numbering together: a created record's sequential key (an integer key, minted
//! one past the highest held) counts within its parent, and where two parent-scoped
//! types share a number space under one parent they share the count
//! ([`ParentScopes::numbering`]). The contract shows it when both are keyed by one
//! integer member under one parent shape, a member besides their identity (an issue's
//! `number` beside its `id`: one numbered for people, the other the record's), and
//! either their key parameters have one name (not a bare `id`), or one type's records
//! declare a member named for the other whose object holds none of the other's keys:
//! such a member cannot point at another record of that type, it marks the record as
//! being one (GitHub's issue `pull_request`, which holds links only, makes every pull
//! request an issue of the same number; its `milestone` holds the milestone's own
//! `number` and `id`, a reference, and shares nothing).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use mb_engine::consistency::property_schema;
use mb_mcm::{Binding, Bundle, Effect, Operation, OperationKind};
use serde_json::{Map, Value};

use super::bundle_index::record_schema_of;
use super::id_string;
use super::owners::{path_segments, target_param};

/// Joins a scoped id's parent values and the record's own id: the ASCII unit separator,
/// which no id, path value or JSON text a client sends holds.
pub(crate) const SEP: char = '\u{1f}';

/// Shortest example or `minLength` an opaque key is unique by.
const OPAQUE_LEN: usize = 16;

/// `id` stored under the parent `scope` (its values, outermost first).
pub(crate) fn scoped_id(scope: &[String], id: &str) -> String {
    let mut out =
        String::with_capacity(id.len() + scope.iter().map(|s| s.len() + 1).sum::<usize>());
    for value in scope {
        out.push_str(value);
        out.push(SEP);
    }
    out.push_str(id);
    out
}

/// A stored id's parent values and the record's own id; `None` for an unscoped id.
pub(crate) fn split_id(stored: &str) -> Option<(Vec<&str>, &str)> {
    let (scope, id) = stored.rsplit_once(SEP)?;
    Some((scope.split(SEP).collect(), id))
}

/// The record's own id of a stored id, scoped or not.
pub(crate) fn plain_id(stored: &str) -> &str {
    stored.rsplit_once(SEP).map_or(stored, |(_, id)| id)
}

/// The parent-scoped types of a contract (see the module's doc).
#[derive(Debug, Default)]
pub(crate) struct ParentScopes {
    scoped: HashMap<String, ParentScope>,
}

/// How one parent-scoped type finds its parent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParentScope {
    /// The segments of its own read's route through the key's segment.
    template: Vec<String>,
    /// The parent's parameters: their segment in `template`, and their names.
    params: Vec<(usize, String)>,
    /// The key's path parameter.
    key_param: String,
    /// The record member the key names (`number`).
    key_member: String,
    /// The route's segments before the key's, parameters as `{}`: what two types must
    /// share to number together.
    shape: String,
    /// Whether the key is an integer: only such keys are numbered.
    integer: bool,
    /// The types numbering with this one under a parent, itself included, sorted.
    numbering: Vec<String>,
}

impl ParentScope {
    /// The member the key names.
    pub(crate) fn key_member(&self) -> &str {
        &self.key_member
    }

    /// The parent a call names, by its route (`route`, a path template) and its path
    /// parameters `params`: where the route runs through the parent's segments (its
    /// read's, through the last parameter: `repos/{owner}/{repo}`), literal for literal
    /// and parameter for parameter, the values of the route's parameters there. A
    /// parent of literal segments matches anywhere in the route (`/orgs/{org}/teams/
    /// {team}/repos/{owner}/{repo}` names a repository's owner); one of parameters
    /// alone only at its start. `/groups/{id}/issues` names no parent of
    /// `/projects/{id}/issues/{issue_iid}`'s issues, whatever its parameter is called.
    pub(crate) fn of_route(&self, route: &str, params: &Map<String, Value>) -> Option<Vec<String>> {
        let end = self.params.last().map_or(0, |(at, _)| at + 1);
        let parent = &self.template[..end];
        let segments: Vec<&str> = path_segments(route)
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        let anchored = parent.iter().any(|s| !s.contains('{'));
        let starts = match anchored {
            true => segments
                .len()
                .saturating_sub(parent.len().saturating_sub(1)),
            false => usize::from(segments.len() >= parent.len()),
        };
        (0..starts).find_map(|start| {
            let mut values = Vec::with_capacity(self.params.len());
            for (want, have) in parent.iter().zip(&segments[start..start + parent.len()]) {
                match (single_param(want), single_param(have)) {
                    (None, None) if want == have => {}
                    // A parameter with the same text around it (`{Sid}.json`).
                    (Some(wanted), Some(name))
                        if want.replacen(wanted, "", 1) == have.replacen(name, "", 1) =>
                    {
                        values.push(
                            params
                                .get(name)
                                .and_then(id_string)
                                .filter(|v| !v.is_empty() && !v.contains(SEP))?,
                        );
                    }
                    _ => return None,
                }
            }
            Some(values)
        })
    }

    /// The parent a record shows in its own links: a top-level member whose path holds
    /// the read's route with the record's key (`url: https://…/repos/a/x/issues/5`).
    pub(crate) fn of_data(&self, data: &Value) -> Option<Vec<String>> {
        let members = data.as_object()?;
        let key = mb_mcm::member_at(data, &self.key_member).and_then(id_string)?;
        members
            .values()
            .filter_map(Value::as_str)
            .find_map(|text| self.in_link(text, &key))
    }

    /// The parent's values `text`, a link, holds where its path holds the route, with
    /// `key` at the key's segment.
    fn in_link(&self, text: &str, key: &str) -> Option<Vec<String>> {
        if text.contains(char::is_whitespace) || !text.contains('/') {
            return None;
        }
        let path = text.split_once("://").map_or(text, |(_, rest)| {
            rest.split_once('/').map_or("", |(_, path)| path)
        });
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let n = self.template.len();
        (0..segments.len().saturating_sub(n - 1)).find_map(|start| {
            let mut values: Vec<Option<String>> = Vec::with_capacity(n);
            for (template, actual) in self.template.iter().zip(&segments[start..start + n]) {
                values.push(match_segment(template, actual)?);
            }
            if values.last().cloned().flatten().as_deref() != Some(key) {
                return None;
            }
            self.params
                .iter()
                .map(|(at, _)| values.get(*at).cloned().flatten())
                .collect()
        })
    }
}

/// What `actual`, one segment of a path, holds where `template`, one of a route, has a
/// parameter: `Some(None)` for a literal it equals, `Some(Some(value))` for a parameter
/// (with whatever text the segment has around it), `None` when they differ.
fn match_segment(template: &str, actual: &str) -> Option<Option<String>> {
    let Some(open) = template.find('{') else {
        return (template == actual).then_some(None);
    };
    let close = open + template[open..].find('}')?;
    let (prefix, suffix) = (&template[..open], &template[close + 1..]);
    if suffix.contains('{') {
        return None;
    }
    let value = actual.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (!value.is_empty()).then(|| Some(value.to_string()))
}

/// The one parameter a segment holds, alone or with text around it (`{Sid}.json`).
fn single_param(segment: &str) -> Option<&str> {
    let open = segment.find('{')?;
    let close = open + segment[open..].find('}')?;
    (!segment[close + 1..].contains('{')).then(|| &segment[open + 1..close])
}

/// A route addressing one record of a type by a key.
struct Route {
    /// The operation's position.
    op: usize,
    segments: Vec<String>,
    /// The key's segment.
    key_at: usize,
    key_param: String,
    /// The parameters before the key, by segment.
    params: Vec<(usize, String)>,
}

impl Route {
    fn of(op: &Operation, at: usize) -> Option<Self> {
        let Binding::Http { path, .. } = &op.binding else {
            return None;
        };
        let param = target_param(op)?;
        // A create's path parameter names where the record goes, not the record.
        if op.kind == OperationKind::Command(Effect::Create) && !op.route_names_record(&param) {
            return None;
        }
        let segments: Vec<String> = path_segments(path)
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let key_at = segments
            .iter()
            .position(|s| single_param(s) == Some(param.as_str()))?;
        let params = segments[..key_at]
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((i, single_param(s)?.to_string())))
            .collect();
        Some(Self {
            op: at,
            segments,
            key_at,
            key_param: param,
            params,
        })
    }

    /// The parent's segments, through its last parameter, parameters as `{}`:
    /// `repos/{}/{}` of `/repos/{owner}/{repo}/issues/{issue_number}`.
    fn shape(&self) -> String {
        let end = self.params.last().map_or(0, |(at, _)| at + 1);
        self.segments[..end]
            .iter()
            .map(|s| if s.contains('{') { "{}" } else { s.as_str() })
            .collect::<Vec<_>>()
            .join("/")
    }
}

impl ParentScopes {
    /// The parent-scoped types of `bundle`, each with the read its parent is taken from:
    /// the type's read in `detail_reads` where that addresses it by a key under a parent,
    /// else its shortest such route.
    pub(crate) fn of(bundle: &Bundle, detail_reads: &HashMap<String, usize>) -> Self {
        let mut routes: BTreeMap<&str, Vec<Route>> = BTreeMap::new();
        for (at, op) in bundle.operations.iter().enumerate() {
            if let (Some(t), Some(route)) = (op.resource_type.as_deref(), Route::of(op, at)) {
                routes.entry(t).or_default().push(route);
            }
        }
        let mut scoped: HashMap<String, ParentScope> = HashMap::new();
        let mut identities: HashMap<String, String> = HashMap::new();
        for (t, routes) in &routes {
            // A route addressing the record with no parent: its key stands alone.
            if routes.iter().any(|r| r.params.is_empty()) {
                continue;
            }
            let canonical = detail_reads
                .get(*t)
                .and_then(|at| routes.iter().find(|r| r.op == *at))
                .or_else(|| routes.iter().min_by_key(|r| (r.segments.len(), r.op)));
            let Some(route) = canonical else {
                continue;
            };
            let op = &bundle.operations[route.op];
            let Some(identity) = op.identity.as_ref() else {
                continue;
            };
            let key_member = identity.path_member().to_string();
            if key_unique(bundle, op, &route.key_param, &key_member) {
                continue;
            }
            identities.insert((*t).to_string(), identity.field.clone());
            let integer = key_integer(bundle, op, &route.key_param, &key_member);
            scoped.insert(
                (*t).to_string(),
                ParentScope {
                    template: route.segments[..=route.key_at].to_vec(),
                    params: route.params.clone(),
                    key_param: route.key_param.clone(),
                    key_member,
                    shape: route.shape(),
                    integer,
                    numbering: vec![(*t).to_string()],
                },
            );
        }
        let groups = numbering_groups(bundle, &scoped, &identities, &routes);
        for (t, group) in groups {
            if let Some(scope) = scoped.get_mut(&t) {
                scope.numbering = group;
            }
        }
        Self { scoped }
    }

    /// How type `t`'s records find their parent, when they are parent-scoped.
    pub(crate) fn get(&self, t: &str) -> Option<&ParentScope> {
        self.scoped.get(t)
    }

    /// The types numbering `member` of type `t` together under one parent, `t` first:
    /// its numbering group when `member` is its key, else `t` alone.
    pub(crate) fn numbering(&self, t: &str, member: &str) -> Vec<String> {
        let mut out = vec![t.to_string()];
        if let Some(scope) = self.scoped.get(t).filter(|s| s.key_member == member) {
            out.extend(scope.numbering.iter().filter(|o| *o != t).cloned());
        }
        out
    }

    /// The parent-scoped types, sorted, each with its parent's parameter names, its key
    /// member and its numbering group (a report of what [`ParentScopes::of`] decided).
    #[cfg(test)]
    pub(crate) fn report(&self) -> Vec<(String, Vec<String>, String, Vec<String>)> {
        let mut out: Vec<_> = self
            .scoped
            .iter()
            .map(|(t, s)| {
                (
                    t.clone(),
                    s.params.iter().map(|(_, name)| name.clone()).collect(),
                    s.key_member.clone(),
                    s.numbering.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }
}

/// Whether the contract marks the key `op` addresses its record by (path parameter
/// `param`, record member `member`) unique on its own (see the module's doc).
fn key_unique(bundle: &Bundle, op: &Operation, param: &str, member: &str) -> bool {
    let param_schema = op
        .input
        .path
        .as_ref()
        .and_then(|p| p.get("properties"))
        .and_then(|p| p.get(param));
    let member_schema = record_schema(bundle, op).and_then(|s| property_schema(bundle, &s, member));
    param_schema
        .into_iter()
        .chain(member_schema.as_ref())
        .any(|s| marks_unique(bundle, s, 0))
}

/// Whether the key `op` addresses its record by is an integer, as its record member
/// declares it, else its path parameter.
fn key_integer(bundle: &Bundle, op: &Operation, param: &str, member: &str) -> bool {
    let member_schema = record_schema(bundle, op).and_then(|s| property_schema(bundle, &s, member));
    let param_schema = op
        .input
        .path
        .as_ref()
        .and_then(|p| p.get("properties"))
        .and_then(|p| p.get(param))
        .cloned();
    member_schema.or(param_schema).is_some_and(|s| {
        let s = deref(bundle, &s);
        match s.get("type") {
            Some(Value::String(t)) => t == "integer",
            Some(Value::Array(ts)) => ts.iter().any(|t| t == "integer"),
            _ => false,
        }
    })
}

/// The schema of the record `op` answers.
fn record_schema(bundle: &Bundle, op: &Operation) -> Option<Value> {
    let schema = op.success_output()?.schema.as_ref()?;
    match op.identity.as_ref() {
        Some(identity) if identity.record.is_some() => record_schema_of(bundle, identity, schema),
        _ => Some(schema.clone()),
    }
}

/// `schema` with `$ref`s to the bundle's definitions followed (bounded).
fn deref<'a>(bundle: &'a Bundle, mut schema: &'a Value) -> &'a Value {
    for _ in 0..16 {
        let Some(target) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| bundle.defs.get(name))
        else {
            break;
        };
        schema = target;
    }
    schema
}

/// Whether a key's `schema` says it is unique on its own: its format, pattern, minimum
/// length or examples, through `$ref`s and the branches of a combination.
fn marks_unique(bundle: &Bundle, schema: &Value, depth: usize) -> bool {
    if depth > 4 {
        return false;
    }
    let schema = deref(bundle, schema);
    if ["anyOf", "oneOf", "allOf"].iter().any(|k| {
        schema
            .get(*k)
            .and_then(Value::as_array)
            .is_some_and(|parts| parts.iter().any(|p| marks_unique(bundle, p, depth + 1)))
    }) {
        return true;
    }
    let format = schema.get("format").and_then(Value::as_str);
    if format.is_some_and(|f| f.eq_ignore_ascii_case("uuid") || f.eq_ignore_ascii_case("guid")) {
        return true;
    }
    if schema
        .get("pattern")
        .and_then(Value::as_str)
        .is_some_and(pattern_unique)
    {
        return true;
    }
    if schema
        .get("minLength")
        .and_then(Value::as_u64)
        .is_some_and(|n| n >= OPAQUE_LEN as u64)
    {
        return true;
    }
    let mut examples: Vec<String> = Vec::new();
    examples.extend(schema.get("example").and_then(id_string));
    if let Some(listed) = schema.get("examples").and_then(Value::as_array) {
        examples.extend(listed.iter().filter_map(id_string));
    }
    !examples.is_empty() && examples.iter().all(|e| looks_unique(e))
}

/// Whether a key `pattern` pins a literal prefix of two or more letters or digits
/// (`^pi_`, `^CA[0-9a-f]{32}$`), a uuid, or a length of [`OPAQUE_LEN`] or more.
fn pattern_unique(pattern: &str) -> bool {
    let body = pattern.strip_prefix('^').unwrap_or(pattern);
    let literal: String = body
        .chars()
        .take_while(|c| !"[(\\.*+?{|$".contains(*c))
        .collect();
    let prefixed = literal.chars().filter(char::is_ascii_alphanumeric).count() >= 2
        && literal.len() < body.len();
    let uuid = pattern.contains("{8}-") && pattern.contains("{12}");
    let long = pattern
        .split('{')
        .skip(1)
        .filter_map(|q| q.split([',', '}']).next()?.trim().parse::<usize>().ok())
        .any(|n| n >= OPAQUE_LEN);
    prefixed || uuid || long
}

/// Whether an example key looks unique on its own: a uuid, a prefixed id (`cus_9s8dF2`,
/// `pi_3MtwBw…`), or an opaque mix of [`OPAQUE_LEN`] or more letters and digits.
fn looks_unique(example: &str) -> bool {
    let uuid = example.len() == 36
        && example.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    let prefixed = example.split_once('_').is_some_and(|(prefix, rest)| {
        (2..=8).contains(&prefix.len())
            && prefix.chars().all(|c| c.is_ascii_alphabetic())
            && rest.len() >= 8
            && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    let opaque = example.len() >= OPAQUE_LEN
        && example
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && example.chars().any(|c| c.is_ascii_digit())
        && example.chars().any(|c| c.is_ascii_alphabetic());
    uuid || prefixed || opaque
}

/// Whether member `name` is named for type `t`: `t`'s words (by stem) begin its own
/// (`pull_request` for `pulls`).
fn names_type(name: &str, t: &str) -> bool {
    let stems =
        |s: &str| -> Vec<String> { mb_mcm::words(s).into_iter().map(mb_mcm::stem).collect() };
    let (named, own) = (stems(name), stems(t));
    !own.is_empty() && named.starts_with(&own)
}

/// The numbering groups of the parent-scoped types `scoped` (see the module's doc),
/// each sorted, by type: `identities` holds each type's identity member, `routes` the
/// routes addressing each type.
fn numbering_groups(
    bundle: &Bundle,
    scoped: &HashMap<String, ParentScope>,
    identities: &HashMap<String, String>,
    routes: &BTreeMap<&str, Vec<Route>>,
) -> HashMap<String, Vec<String>> {
    let types: Vec<&String> = {
        let mut t: Vec<&String> = scoped.keys().collect();
        t.sort();
        t
    };
    // Each type's record members, as its own read answers it.
    let members: HashMap<&str, Map<String, Value>> = types
        .iter()
        .filter_map(|t| {
            let at = routes
                .get(t.as_str())?
                .iter()
                .find(|r| scoped[*t].template == r.segments[..=r.key_at])?
                .op;
            let schema = record_schema(bundle, &bundle.operations[at])?;
            let props = deref(bundle, &schema)
                .get("properties")?
                .as_object()?
                .clone();
            Some((t.as_str(), props))
        })
        .collect();
    let mut parent: HashMap<&str, &str> = types.iter().map(|t| (t.as_str(), t.as_str())).collect();
    fn root<'a>(parent: &HashMap<&'a str, &'a str>, mut t: &'a str) -> &'a str {
        while let Some(p) = parent.get(t).filter(|p| **p != t) {
            t = p;
        }
        t
    }
    for (i, a) in types.iter().enumerate() {
        for b in &types[i + 1..] {
            let (sa, sb) = (&scoped[*a], &scoped[*b]);
            if sa.shape != sb.shape
                || sa.key_member != sb.key_member
                || !sa.integer
                || !sb.integer
                || identities.get(*a) == Some(&sa.key_member)
                || identities.get(*b) == Some(&sb.key_member)
            {
                continue;
            }
            let one_param = sa.key_param == sb.key_param && !mb_mcm::is_generic_id(&sa.key_param);
            let marks = |x: &str, y: &str| {
                let Some(props) = members.get(x) else {
                    return false;
                };
                let keys: BTreeSet<&str> = [scoped[y].key_member.as_str()]
                    .into_iter()
                    .chain(identities.get(y).map(String::as_str))
                    .collect();
                props.iter().any(|(name, schema)| {
                    names_type(name, y)
                        && deref(bundle, schema)
                            .get("properties")
                            .and_then(Value::as_object)
                            .or_else(|| {
                                ["anyOf", "oneOf", "allOf"].iter().find_map(|k| {
                                    schema.get(*k)?.as_array()?.iter().find_map(|p| {
                                        deref(bundle, p).get("properties")?.as_object()
                                    })
                                })
                            })
                            .is_some_and(|inner| {
                                !inner.is_empty() && keys.iter().all(|k| !inner.contains_key(*k))
                            })
                })
            };
            if one_param || marks(a, b) || marks(b, a) {
                let (ra, rb) = (root(&parent, a), root(&parent, b));
                if ra != rb {
                    parent.insert(rb, ra);
                }
            }
        }
    }
    let mut groups: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for t in &types {
        groups
            .entry(root(&parent, t))
            .or_default()
            .push((*t).clone());
    }
    let mut out = HashMap::new();
    for group in groups.into_values().filter(|g| g.len() > 1) {
        for t in &group {
            out.insert(t.clone(), group.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scoped_id_splits_back_into_its_parent_and_id() {
        let stored = scoped_id(&["a".into(), "x".into()], "5");
        assert_eq!(split_id(&stored), Some((vec!["a", "x"], "5")));
        assert_eq!(plain_id(&stored), "5");
        assert_eq!(split_id("5"), None);
        assert_eq!(plain_id("5"), "5");
    }

    #[test]
    fn keys_unique_by_their_shape() {
        assert!(looks_unique("cus_NffrFeUfNV2Hib"));
        assert!(looks_unique("pi_3MtwBwLkdIwHu7ix28a3tqPa"));
        assert!(looks_unique("123e4567-e89b-12d3-a456-426614174000"));
        assert!(looks_unique("CA0123456789abcdef0123456789abcdef"));
        assert!(!looks_unique("42"));
        assert!(!looks_unique("main"));
        assert!(!looks_unique("my_secret"));
        assert!(!looks_unique("12345678901234567890"));
        assert!(pattern_unique("^pi_[A-Za-z0-9]+$"));
        assert!(pattern_unique("^CA[0-9a-fA-F]{32}$"));
        assert!(!pattern_unique("^[a-z0-9-]{1,100}$"));
        assert!(!pattern_unique("^[A-Z_][A-Z0-9_]*$"));
    }

    #[test]
    fn a_link_shows_its_record_s_parent() {
        let scope = ParentScope {
            template: ["repos", "{owner}", "{repo}", "issues", "{issue_number}"]
                .map(str::to_string)
                .to_vec(),
            params: vec![(1, "owner".into()), (2, "repo".into())],
            key_param: "issue_number".into(),
            key_member: "number".into(),
            shape: "repos/{}/{}".into(),
            integer: true,
            numbering: vec!["issues".into()],
        };
        let issue = serde_json::json!({
            "number": 5,
            "repository_url": "https://api.github.com/repos/a/x",
            "url": "https://api.github.com/api/v3/repos/a/x/issues/5?x=1",
        });
        assert_eq!(scope.of_data(&issue), Some(vec!["a".into(), "x".into()]));
        let other = serde_json::json!({"number": 6, "url": "https://h/repos/a/x/issues/5"});
        assert_eq!(scope.of_data(&other), None);
        assert!(names_type("pull_request", "pulls"));
        assert!(!names_type("milestone", "pulls"));
        assert_eq!(
            match_segment("{Sid}.json", "CA1.json"),
            Some(Some("CA1".into()))
        );
    }

    /// Issues, pulls, milestones and secrets under a repository; labels also read with
    /// no parent; a source keyed by a prefixed id.
    const REPOS: &str = r##"
openapi: 3.0.3
info: {title: Repos, version: "1"}
paths:
  /repos/{owner}/{repo}/issues/{issue_number}:
    get:
      operationId: getIssue
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: issue_number, in: path, required: true, schema: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}}
  /repos/{owner}/{repo}/pulls/{pull_number}:
    get:
      operationId: getPull
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: pull_number, in: path, required: true, schema: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Pull"}}}}}
  /repos/{owner}/{repo}/milestones/{milestone_number}:
    get:
      operationId: getMilestone
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: milestone_number, in: path, required: true, schema: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Milestone"}}}}}
  /repos/{owner}/{repo}/secrets/{secret_name}:
    get:
      operationId: getSecret
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: secret_name, in: path, required: true, schema: {type: string}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Secret"}}}}}
  /repos/{owner}/{repo}/labels/{label_id}:
    get:
      operationId: getLabel
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: label_id, in: path, required: true, schema: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Label"}}}}}
  /labels/{label_id}:
    get:
      operationId: getLabelById
      parameters: [{name: label_id, in: path, required: true, schema: {type: integer}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Label"}}}}}
  /customers/{customer}/sources/{id}:
    get:
      operationId: getSource
      parameters:
        - {name: customer, in: path, required: true, schema: {type: string}}
        - {name: id, in: path, required: true, schema: {type: string, example: "src_1MwkG2LkdIwHu7ixmpVVQUxi"}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Source"}}}}}
  /customers/{customer}/devices/{device_id}:
    get:
      operationId: getDevice
      parameters:
        - {name: customer, in: path, required: true, schema: {type: string}}
        - {name: device_id, in: path, required: true, schema: {type: string, format: uuid}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Device"}}}}}
  /customers/{customer}/tokens/{token}:
    get:
      operationId: getToken
      parameters:
        - {name: customer, in: path, required: true, schema: {type: string}}
        - {name: token, in: path, required: true, schema: {type: string, minLength: 32}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Token"}}}}}
components:
  schemas:
    Milestone:
      type: object
      properties: {id: {type: integer}, number: {type: integer}, title: {type: string}}
    Issue:
      type: object
      properties:
        id: {type: integer}
        number: {type: integer}
        milestone: {$ref: "#/components/schemas/Milestone"}
        pull_request: {type: object, properties: {url: {type: string}}}
    Pull:
      type: object
      properties: {id: {type: integer}, number: {type: integer}}
    Secret:
      type: object
      properties: {name: {type: string}}
    Label:
      type: object
      properties: {id: {type: integer}, name: {type: string}}
    Source:
      type: object
      properties: {id: {type: string}}
    Device:
      type: object
      properties: {device_id: {type: string}}
    Token:
      type: object
      properties: {token: {type: string}}
"##;

    #[test]
    fn records_under_a_parent_by_a_key_not_unique_alone_are_parent_scoped() {
        let bundle = mb_import::compile(REPOS.as_bytes()).unwrap();
        let index = crate::call::BundleIndex::new(&bundle);
        let scopes = index.parent_scopes();
        let report: Vec<(String, Vec<String>, String, Vec<String>)> = scopes.report();
        let repo = || vec!["owner".to_string(), "repo".to_string()];
        let both = || vec!["issues".to_string(), "pulls".to_string()];
        let alone = |t: &str| vec![t.to_string()];
        // Labels are read with no parent too; a source's key is prefixed, a device's a
        // uuid, a token's 32 characters or more: global.
        assert_eq!(
            report,
            vec![
                ("issues".into(), repo(), "number".into(), both()),
                (
                    "milestones".into(),
                    repo(),
                    "number".into(),
                    alone("milestones")
                ),
                ("pulls".into(), repo(), "number".into(), both()),
                ("secrets".into(), repo(), "name".into(), alone("secrets")),
            ]
        );
        // An issue's `pull_request` holds none of a pull's keys: they number together;
        // its `milestone` holds the milestone's own `number`, so milestones do not.
        assert_eq!(scopes.numbering("issues", "number"), ["issues", "pulls"]);
        assert_eq!(scopes.numbering("pulls", "number"), ["pulls", "issues"]);
        assert_eq!(scopes.numbering("issues", "id"), ["issues"]);
        let params = serde_json::json!({"owner": "a", "repo": "x", "issue_number": 5});
        let params = params.as_object().unwrap();
        let scope = scopes.get("issues").unwrap();
        let a_x = Some(vec!["a".to_string(), "x".to_string()]);
        assert_eq!(scope.of_route("/repos/{owner}/{repo}/issues", params), a_x);
        // The parent's segments anywhere in the route, by its own parameters' names.
        let deeper = serde_json::json!({"org": "o", "o2": "a", "r2": "x"});
        assert_eq!(
            scope.of_route(
                "/orgs/{org}/teams/{team}/repos/{o2}/{r2}/issues",
                deeper.as_object().unwrap()
            ),
            a_x
        );
        // Another parent's route names no parent of these, whatever its parameters.
        assert_eq!(scope.of_route("/users/{owner}/{repo}/issues", params), None);
        assert_eq!(scope.of_route("/repos/{owner}", params), None);
    }

    /// GitLab's shape: issues read under a project by `iid`; a group's issue list names
    /// no project, though its parameter is `{id}` too.
    #[test]
    fn a_route_under_another_parent_names_none() {
        const PROJECTS: &str = r##"
openapi: 3.0.3
info: {title: Projects, version: "1"}
paths:
  /projects/{id}/issues/{issue_iid}:
    get:
      operationId: getIssue
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
        - {name: issue_iid, in: path, required: true, schema: {type: integer}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}}
  /groups/{id}/issues:
    get:
      operationId: listGroupIssues
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Issue"}}}}}}
components:
  schemas:
    Issue:
      type: object
      properties: {id: {type: integer}, iid: {type: integer}, project_id: {type: integer}}
"##;
        let bundle = mb_import::compile(PROJECTS.as_bytes()).unwrap();
        let index = crate::call::BundleIndex::new(&bundle);
        let scope = index.parent_scopes().get("issues").unwrap();
        let params = serde_json::json!({"id": "7"});
        let params = params.as_object().unwrap();
        assert_eq!(
            scope.of_route("/projects/{id}/issues", params),
            Some(vec!["7".to_string()])
        );
        assert_eq!(scope.of_route("/groups/{id}/issues", params), None);
    }
}
