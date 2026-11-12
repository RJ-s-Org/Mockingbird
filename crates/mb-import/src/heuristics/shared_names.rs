//! One collection name, several objects: GitHub's `/repos/{owner}/{repo}/keys`
//! (deploy keys) and `/user/keys` (SSH keys) both infer the type `keys`, Stripe's
//! `/v1/issuing/transactions` and `/v1/treasury/transactions` both `transactions`.
//! Records are stored by type and id, so two objects under one type would read,
//! list and overwrite each other's records. [`split_shared_type_names`] gives each
//! object its own type.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use mb_mcm::{Effect, HttpMethod, Operation, OperationKind};
use serde_json::{Map, Value};

use super::{
    ID_FALLBACKS, ResourceHint, http_path, is_param, is_type_name, name_stem, schema_properties,
    segments, without_format,
};
use crate::schema::defs_name;

/// Operations of one inferred type that are one object: those under one
/// collection route (the path up to the type's segment, parameters aside:
/// `/repos/{}/{}/keys`), joined with every other such family that answers a
/// definition in common.
struct Group {
    ops: Vec<usize>,
    /// The definitions its operations answer ([`record_defs`]), in contract order.
    defs: Vec<String>,
    /// The number of segments of its shortest collection route.
    depth: usize,
    /// The definition of the records it holds: what its first read by path
    /// answers ([`held_def`]), else what a create answers where no read lists or
    /// reads anything else.
    held: Option<String>,
}

/// Splits each inferred type whose operations hold different objects. The
/// operations of a type under one collection route (a family) are one object;
/// two families are one object when they answer a common definition
/// ([`record_defs`]): `/orgs/{org}/migrations` and `/user/migrations` both answer
/// `migration`, so stay `migrations`.
///
/// A type is split only when two or more of its objects hold records of their own
/// ([`Group::held`]): a read by path, or a create, answers a definition, so the
/// object is a record kind that is stored and read back by its key. An object with
/// neither, a list answering a summary definition (`GET /users/{username}/keys` of
/// `key-simple`, TMDB's `GET /3/discover/movie`) or a route that answers nothing,
/// is a view of records held elsewhere and stays with the type's name.
///
/// Of the objects holding records, one keeps the type's name: the one answering
/// the definition named for the type (`key` for `keys`, case, separators and the
/// plural aside), else the one on the shortest route (`/repos` of `repos`), else
/// the one with the most operations, else the first in contract order. Every other
/// one is named after the definition it holds: `deploy-key`, `issuing.transaction`.
/// Objects split off two types that hold one definition get one name, and a type
/// already named for that definition and holding it (`/deploy-key/{id}`) is joined:
/// each is one object. Any other name another type has already, or that an
/// `x-mockingbird-resource` declares, is prefixed with the old one
/// (`keys.deploy-key`); one taken that way too leaves the object where it was.
///
/// Operations whose path has no segment of the type's name (RPC-style calls) are
/// left as they are. An `x-mockingbird-resource` `type` and a world description's
/// `type` still win: both apply after this. Returns the types split-off objects
/// went to.
pub(super) fn split_shared_type_names(
    operations: &mut [Operation],
    hints: &[ResourceHint],
    defs: &Map<String, Value>,
) -> HashSet<String> {
    let mut types: Vec<String> = Vec::new();
    for op in operations.iter() {
        if let Some(t) = &op.resource_type
            && !types.contains(t)
        {
            types.push(t.clone());
        }
    }
    let mut taken: HashSet<String> = types.iter().cloned().collect();
    taken.extend(hints.iter().filter_map(|h| h.resource_type.clone()));
    // The types named for the definition they hold; then the objects split off,
    // in contract order.
    let mut by_def: HashMap<String, String> = HashMap::new();
    let mut split: Vec<(&str, Group)> = Vec::new();
    for t in &types {
        let mut holding: Vec<Group> = groups_of(t, operations, defs)
            .into_iter()
            .filter(|g| g.held.is_some())
            .collect();
        let own = name_stem(t);
        let canonical = holding
            .iter()
            .position(|g| g.defs.iter().any(|d| name_stem(d) == own))
            .or_else(|| {
                (0..holding.len())
                    .min_by_key(|&at| (holding[at].depth, Reverse(holding[at].ops.len()), at))
            });
        let Some(canonical) = canonical else {
            continue;
        };
        let kept = holding.remove(canonical);
        // A type named for the very definition it holds is that object's type.
        if kept.held.as_ref() == Some(t) {
            by_def.entry(t.clone()).or_insert_with(|| t.clone());
        }
        split.extend(holding.into_iter().map(|g| (t.as_str(), g)));
    }
    let mut given = HashSet::new();
    for (t, group) in split {
        let Some(def) = group.held.as_deref() else {
            continue;
        };
        // One definition held under two names is one object: `/user/memberships/orgs/{org}`
        // (of `orgs`) and `/orgs/{org}/memberships/{username}` (of `memberships`).
        let Some(name) = by_def
            .get(def)
            .cloned()
            .or_else(|| free_name(t, def, &taken))
        else {
            continue;
        };
        taken.insert(name.clone());
        by_def.insert(def.to_owned(), name.clone());
        for &index in &group.ops {
            operations[index].resource_type = Some(name.clone());
        }
        given.insert(name);
    }
    given
}

/// The objects type `t` covers ([`Group`]), in contract order.
fn groups_of(t: &str, operations: &[Operation], defs: &Map<String, Value>) -> Vec<Group> {
    let mut families: Vec<(String, Group)> = Vec::new();
    for (index, op) in operations.iter().enumerate() {
        if op.resource_type.as_deref() != Some(t) {
            continue;
        }
        let Some(route) = http_path(op).and_then(|(_, path)| collection_route(path, t)) else {
            continue;
        };
        let at = match families.iter().position(|(r, _)| *r == route) {
            Some(at) => at,
            None => {
                let depth = route.matches('/').count() + 1;
                families.push((
                    route,
                    Group {
                        ops: Vec::new(),
                        defs: Vec::new(),
                        depth,
                        held: None,
                    },
                ));
                families.len() - 1
            }
        };
        let family = &mut families[at].1;
        family.ops.push(index);
        for d in record_defs(op, defs) {
            if !family.defs.contains(&d) {
                family.defs.push(d);
            }
        }
    }
    let mut groups: Vec<Group> = Vec::new();
    for (_, mut group) in families {
        // Every group this family shares a definition with joins it, so a family
        // bridging two earlier groups makes them one.
        let mut at = 0;
        while at < groups.len() {
            if groups[at].defs.iter().any(|d| group.defs.contains(d)) {
                let joined = groups.remove(at);
                for d in joined.defs {
                    if !group.defs.contains(&d) {
                        group.defs.push(d);
                    }
                }
                group.ops.extend(joined.ops);
                group.depth = group.depth.min(joined.depth);
            } else {
                at += 1;
            }
        }
        group.ops.sort_unstable();
        groups.push(group);
    }
    for group in &mut groups {
        let ops = || group.ops.iter().map(|&index| &operations[index]);
        let is_get = |op: &Operation| http_path(op).is_some_and(|(m, _)| m == HttpMethod::Get);
        let read: Vec<String> = ops()
            .filter(|op| is_get(op))
            .flat_map(|op| record_defs(op, defs))
            .collect();
        // A create holds what it answers where a read gives it back or, where nothing
        // reads, where it answers a keyed record: Stripe's
        // `POST /v1/billing_portal/sessions` an `id`, while Spotify's
        // `POST /playlists/{id}/tracks` answers a receipt (`snapshot_id`).
        let keyed = |def: &String| {
            schema_properties(defs.get(def.as_str()), defs).is_some_and(|p| {
                p.contains_key("id") || ID_FALLBACKS.iter().any(|k| p.contains_key(*k))
            })
        };
        let created = |def: &String| read.contains(def) || (read.is_empty() && keyed(def));
        group.held = ops()
            .find_map(|op| held_def(op, HttpMethod::Get, defs))
            .or_else(|| {
                ops()
                    .filter_map(|op| held_def(op, HttpMethod::Post, defs))
                    .find(created)
            });
    }
    groups.sort_by_key(|g| g.ops.first().copied());
    groups
}

/// The route of `path` up to its last segment named `t`, parameters as `{}`:
/// `repos/{}/{}/keys` for `/repos/{owner}/{repo}/keys/{key_id}` and type `keys`.
/// `None` when no segment is named `t`.
fn collection_route(path: &str, t: &str) -> Option<String> {
    let segs = segments(path);
    let last = segs
        .iter()
        .rposition(|s| !is_param(s) && without_format(s) == t)?;
    let route: Vec<&str> = segs[..=last]
        .iter()
        .map(|s| if is_param(s) { "{}" } else { without_format(s) })
        .collect();
    Some(route.join("/"))
}

/// The definitions `op`'s success answers as its record or records: the answer
/// itself, its items, the sole member of a one-member envelope, or the items of an
/// envelope's list. Only a definition with members counts ([`named_def`]).
fn record_defs(op: &Operation, defs: &Map<String, Value>) -> Vec<String> {
    let Some(answer) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return Vec::new();
    };
    let named = |schema: &Value| named_def(schema, defs);
    let items = |schema: &Value| schema.get("items").and_then(named);
    let mut found = vec![named(answer), items(answer)];
    if answer.get("$ref").is_none()
        && let Some(props) = answer.get("properties").and_then(Value::as_object)
    {
        if props.len() == 1 {
            found.push(props.values().next().and_then(named));
        }
        found.extend(props.values().map(items));
    }
    let mut out: Vec<String> = Vec::new();
    for name in found.into_iter().flatten() {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// The definition of the one record `op` answers when it is a read by path (a
/// `GET` ending in a parameter) or, for `method` `POST`, a create: a definition
/// with members, or a one-member envelope of one. A page of items is neither
/// (TMDB's `GET /3/trending/movie/{time_window}`).
fn held_def(op: &Operation, method: HttpMethod, defs: &Map<String, Value>) -> Option<String> {
    let (at, path) = http_path(op)?;
    let by_path = segments(path).last().is_some_and(|s| is_param(s));
    let holds = match method {
        HttpMethod::Get => by_path,
        HttpMethod::Post => op.kind == OperationKind::Command(Effect::Create),
        _ => false,
    };
    if at != method || !holds {
        return None;
    }
    let answer = op.success_output().and_then(|o| o.schema.as_ref())?;
    let sole = answer
        .get("properties")
        .and_then(Value::as_object)
        .filter(|p| p.len() == 1)
        .and_then(|p| p.values().next());
    [Some(answer), sole]
        .into_iter()
        .flatten()
        .find_map(|schema| named_def(schema, defs))
}

/// The definition `schema` is a `$ref` to, when that declares members: GitHub's
/// `empty-object` says nothing of what a route holds.
fn named_def(schema: &Value, defs: &Map<String, Value>) -> Option<String> {
    let name = defs_name(schema.get("$ref")?.as_str()?)?;
    schema_properties(defs.get(name), defs)
        .is_some_and(|p| !p.is_empty())
        .then(|| name.to_owned())
}

/// The name for an object of type `t` split off as definition `def`: `def`, else
/// `t.def` when another type has that.
fn free_name(t: &str, def: &str, taken: &HashSet<String>) -> Option<String> {
    [def.to_owned(), format!("{t}.{def}")]
        .into_iter()
        .find(|name| !taken.contains(name) && is_type_name(name))
}
