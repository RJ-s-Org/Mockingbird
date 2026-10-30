//! Path normalization, path-parameter inference and merge keys.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::Value;

use super::model::DraftWarning;

/// Fewest distinct non-id literals at one position that make it a parameter.
const SLUG_MIN_DISTINCT: usize = 3;

/// Whether a concrete path segment looks like an identifier: all digits, a
/// UUID, or at least 8 hex characters containing a digit.
pub fn is_id_like(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    if segment.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    if is_uuid(segment) {
        return true;
    }
    segment.len() >= 8
        && segment.bytes().all(|b| b.is_ascii_hexdigit())
        && segment.bytes().any(|b| b.is_ascii_digit())
}

fn is_uuid(segment: &str) -> bool {
    let groups: Vec<&str> = segment.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The parameter name for a segment after the literal `previous_literal`:
/// its singular form plus `Id` (`"pets"` → `"petId"`), or `"id"`.
pub fn param_name(previous_literal: Option<&str>) -> String {
    match previous_literal.map(singular) {
        Some(base) if !base.is_empty() => format!("{base}Id"),
        _ => "id".to_string(),
    }
}

fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies") {
        return format!("{stem}y");
    }
    if word.ends_with('s') && !word.ends_with("ss") {
        return word[..word.len() - 1].to_string();
    }
    word.to_string()
}

/// A path template and its parameter names in path order.
#[derive(Clone, Debug, PartialEq)]
pub struct PathTemplate {
    pub template: String,
    pub params: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Seg {
    Literal(String),
    /// Inferred parameter, named later.
    Param,
    /// A parameter already present in the input (`{name}`).
    Named(String),
}

/// The rough shape of a recorded JSON response: its type and, for an
/// object (or an array's first object), its keys.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Shape {
    kind: &'static str,
    keys: BTreeSet<String>,
}

impl Shape {
    pub fn of(body: &Value) -> Shape {
        let (kind, object) = match body {
            Value::Array(items) => ("array", items.iter().find_map(Value::as_object)),
            Value::Object(map) => ("object", Some(map)),
            Value::String(_) => ("string", None),
            Value::Number(_) => ("number", None),
            Value::Bool(_) => ("boolean", None),
            Value::Null => ("null", None),
        };
        Shape {
            kind,
            keys: object
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default(),
        }
    }

    /// The same type and, when both have keys, at least half of all their
    /// keys in common.
    fn compatible(&self, other: &Shape) -> bool {
        if self.kind != other.kind {
            return false;
        }
        if self.keys.is_empty() || other.keys.is_empty() {
            return true;
        }
        let shared = self.keys.intersection(&other.keys).count();
        // |A ∪ B| = |A| + |B| - |A ∩ B|, without walking the union.
        shared * 2 >= self.keys.len() + other.keys.len() - shared
    }
}

/// [`infer_templates_with`] without response shapes.
pub fn infer_templates(paths: &[String]) -> BTreeMap<String, PathTemplate> {
    infer_templates_with(paths, &BTreeMap::new())
}

/// Template for every concrete path (keys are the inputs).
///
/// First every id-like segment becomes a parameter. Then, for each position,
/// paths with the same segment count that agree on every other (templated)
/// segment and show at least three distinct non-id literals there get a
/// parameter too, unless one of those literals is a collection name (some
/// path has a parameter right after it, as `/api/users/42` shows for
/// `users`) or the responses recorded for them (`shapes`, by concrete path)
/// have incompatible shapes: sibling collections such as `/api/users` and
/// `/api/orders` stay apart. Names come from [`param_name`] of the closest
/// literal to the left, suffixed `2`, `3`… when repeated within one
/// template.
pub fn infer_templates_with(
    paths: &[String],
    shapes: &BTreeMap<String, Shape>,
) -> BTreeMap<String, PathTemplate> {
    let mut segs: Vec<Vec<Seg>> = paths.iter().map(|p| split(p)).collect();
    let shapes: Vec<Option<&Shape>> = paths.iter().map(|p| shapes.get(p)).collect();
    loop {
        let mut changed = false;
        let max_len = segs.iter().map(Vec::len).max().unwrap_or(0);
        for pos in 0..max_len {
            changed |= templatize_position(&mut segs, &shapes, pos);
        }
        if !changed {
            break;
        }
    }
    paths
        .iter()
        .zip(segs)
        .map(|(path, segs)| (path.clone(), render(&segs)))
        .collect()
}

/// Members (path indexes) of a group and its distinct literals.
type Group = (Vec<usize>, BTreeSet<String>);

/// A segment for grouping: parameters unified as `{}`.
fn unified(seg: &Seg) -> String {
    match seg {
        Seg::Literal(l) => l.clone(),
        Seg::Param | Seg::Named(_) => "{}".to_string(),
    }
}

/// Turns position `pos` into a parameter in every group of same-length paths
/// that agree elsewhere and show enough distinct literals there, none of
/// them a collection name and their responses compatible.
fn templatize_position(segs: &mut [Vec<Seg>], shapes: &[Option<&Shape>], pos: usize) -> bool {
    // Unified prefixes `..=pos` that end in a literal followed by a parameter.
    let collections: HashSet<Vec<String>> = segs
        .iter()
        .filter(|path| {
            matches!(path.get(pos), Some(Seg::Literal(_)))
                && matches!(path.get(pos + 1), Some(Seg::Param | Seg::Named(_)))
        })
        .map(|path| path.iter().take(pos + 1).map(unified).collect())
        .collect();
    // Group key: length and every segment but `pos`, with parameters unified.
    let mut groups: BTreeMap<Vec<Option<String>>, Group> = BTreeMap::new();
    for (index, path) in segs.iter().enumerate() {
        let Some(Seg::Literal(value)) = path.get(pos) else {
            continue;
        };
        let key: Vec<Option<String>> = path
            .iter()
            .enumerate()
            .map(|(i, s)| (i != pos).then(|| unified(s)))
            .collect();
        let entry = groups.entry(key).or_default();
        entry.0.push(index);
        entry.1.insert(value.clone());
    }
    let mut changed = false;
    for (members, distinct) in groups.into_values() {
        if distinct.len() < SLUG_MIN_DISTINCT {
            continue;
        }
        let is_collection = |&i: &usize| {
            let prefix: Vec<String> = segs[i].iter().take(pos + 1).map(unified).collect();
            collections.contains(&prefix)
        };
        if members.iter().any(is_collection) {
            continue;
        }
        if !shapes_compatible(
            members
                .iter()
                .filter_map(|&i| shapes.get(i).copied().flatten()),
        ) {
            continue;
        }
        for index in members {
            segs[index][pos] = Seg::Param;
            changed = true;
        }
    }
    changed
}

/// Whether every two distinct shapes are [`Shape::compatible`]. Duplicates
/// are dropped first, and differing kinds fail before any key comparison.
fn shapes_compatible<'a>(shapes: impl Iterator<Item = &'a Shape>) -> bool {
    let distinct: BTreeSet<&Shape> = shapes.collect();
    let mut kinds = distinct.iter().map(|s| s.kind);
    if let Some(first) = kinds.next()
        && kinds.any(|k| k != first)
    {
        return false;
    }
    // A shape without keys is compatible with every shape of its kind.
    let keyed: Vec<&Shape> = distinct
        .into_iter()
        .filter(|s| !s.keys.is_empty())
        .collect();
    match bounds_decide(&keyed) {
        Some(answer) => answer,
        None => keyed
            .iter()
            .enumerate()
            .all(|(i, a)| keyed[i + 1..].iter().all(|b| a.compatible(b))),
    }
}

/// Decides [`shapes_compatible`] for keyed shapes of one kind from key-set
/// sizes and the keys all of them share, without comparing pairs, when
/// those bounds settle it; `None` when only the pairwise check can.
///
/// Two key sets `A` and `B` are compatible when `3 |A ∩ B| >= |A| + |B|`.
/// So the smallest and largest sets fail when `largest > 2 * smallest`
/// (`|A ∩ B| <= smallest`), and every pair passes when `3 |common| >= s1 +
/// s2`, where `common` is the keys every set has (`|A ∩ B| >= |common|`) and
/// `s1`, `s2` are the two largest sizes.
fn bounds_decide(keyed: &[&Shape]) -> Option<bool> {
    if keyed.len() < 2 {
        return Some(true);
    }
    let smallest = keyed.iter().min_by_key(|s| s.keys.len())?;
    let (mut s1, mut s2) = (0usize, 0usize);
    for shape in keyed {
        let len = shape.keys.len();
        if len > s1 {
            (s1, s2) = (len, s1);
        } else if len > s2 {
            s2 = len;
        }
    }
    if s1 > 2 * smallest.keys.len() {
        return Some(false);
    }
    let common = smallest
        .keys
        .iter()
        .filter(|k| keyed.iter().all(|s| s.keys.contains(*k)))
        .count();
    (3 * common >= s1 + s2).then_some(true)
}

/// The warning for an operation whose path parameters were inferred from
/// distinct non-id path segments (`slugs`: parameter name and values).
pub fn slug_warning(target: &str, slugs: &[(String, Vec<String>)]) -> DraftWarning {
    const SHOWN: usize = 5;
    let described: Vec<String> = slugs
        .iter()
        .map(|(name, values)| {
            let mut unique: Vec<&str> = Vec::new();
            for v in values {
                if !unique.contains(&v.as_str()) {
                    unique.push(v);
                }
            }
            let more = if unique.len() > SHOWN { ", …" } else { "" };
            let shown: Vec<&str> = unique.into_iter().take(SHOWN).collect();
            format!("{name} ({}{more})", shown.join(", "))
        })
        .collect();
    DraftWarning::new(
        "conflicting_schemas",
        format!(
            "{target}: the path parameter{} {} {} inferred from distinct path segments; \
             if they name different resources, split the operation",
            if slugs.len() == 1 { "" } else { "s" },
            described.join(", "),
            if slugs.len() == 1 { "was" } else { "were" },
        ),
    )
    .for_operation(target.to_string())
}

fn split(path: &str) -> Vec<Seg> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if let Some(name) = s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Seg::Named(name.to_string())
            } else if is_id_like(s) {
                Seg::Param
            } else {
                Seg::Literal(s.to_string())
            }
        })
        .collect()
}

fn render(segs: &[Seg]) -> PathTemplate {
    let mut params: Vec<String> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut last_literal: Option<&str> = None;
    for seg in segs {
        match seg {
            Seg::Literal(l) => {
                last_literal = Some(l);
                parts.push(l.clone());
            }
            Seg::Named(name) => {
                params.push(name.clone());
                parts.push(format!("{{{name}}}"));
            }
            Seg::Param => {
                let base = param_name(last_literal);
                let mut name = base.clone();
                let mut n = 2;
                while params.contains(&name) {
                    name = format!("{base}{n}");
                    n += 1;
                }
                parts.push(format!("{{{name}}}"));
                params.push(name);
            }
        }
    }
    PathTemplate {
        template: format!("/{}", parts.join("/")),
        params,
    }
}

/// Merge key: (upper-case method, path with every `{name}` as `{}`, no
/// trailing slash except `/`).
pub fn path_key(method: &str, path: &str) -> (String, String) {
    let mut key = String::with_capacity(path.len());
    let mut in_param = false;
    for c in path.chars() {
        match c {
            '{' if !in_param => {
                in_param = true;
                key.push_str("{}");
            }
            '}' if in_param => in_param = false,
            _ if in_param => {}
            _ => key.push(c),
        }
    }
    while key.len() > 1 && key.ends_with('/') {
        key.pop();
    }
    if key.is_empty() {
        key.push('/');
    }
    (method.to_ascii_uppercase(), key)
}

/// Normalizes a raw path or URL to an OpenAPI path: a full URL becomes its
/// path, the query and fragment are dropped, `:id` and `{{v}}` become `{id}`
/// and `{v}`, `//` is collapsed, a leading `/` is ensured and a trailing `/`
/// removed (except for `/`).
pub fn normalize_path(raw: &str) -> String {
    let mut rest = raw.trim();
    if let Some((_, after_scheme)) = rest.split_once("://") {
        rest = after_scheme.find('/').map_or("", |i| &after_scheme[i..]);
    }
    if let Some(i) = rest.find(['?', '#']) {
        rest = &rest[..i];
    }
    let unbraced = rest.replace("{{", "{").replace("}}", "}");
    let segments: Vec<String> = unbraced
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| match s.strip_prefix(':') {
            Some(name) if !name.is_empty() => format!("{{{name}}}"),
            _ => s.to_string(),
        })
        .collect();
    format!("/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(keys: &[&str]) -> Shape {
        Shape {
            kind: "object",
            keys: keys.iter().map(|k| (*k).to_string()).collect(),
        }
    }

    fn pairwise(shapes: &[Shape]) -> bool {
        shapes
            .iter()
            .enumerate()
            .all(|(i, a)| shapes[i + 1..].iter().all(|b| a.compatible(b)))
    }

    /// Every subset of a small key universe, as shapes.
    fn all_shapes(universe: &[&str]) -> Vec<Shape> {
        (0..1u32 << universe.len())
            .map(|mask| {
                let keys: Vec<&str> = universe
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, k)| *k)
                    .collect();
                shape(&keys)
            })
            .collect()
    }

    #[test]
    fn bounds_agree_with_the_pairwise_check() {
        let shapes = all_shapes(&["a", "b", "c", "d", "e"]);
        let mut checked = 0;
        // Every pair and every triple of subsets of a 5-key universe.
        for (i, a) in shapes.iter().enumerate() {
            for (j, b) in shapes.iter().enumerate().skip(i) {
                let pair = [a.clone(), b.clone()];
                assert_eq!(shapes_compatible(pair.iter()), pairwise(&pair), "{pair:?}");
                for c in shapes.iter().skip(j) {
                    let triple = [a.clone(), b.clone(), c.clone()];
                    assert_eq!(
                        shapes_compatible(triple.iter()),
                        pairwise(&triple),
                        "{triple:?}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 5000);
    }

    #[test]
    fn bounds_settle_the_common_cases() {
        // Shared keys plus one extra each: accepted without pairs.
        let many: Vec<Shape> = (0..50)
            .map(|i| {
                let extra = format!("k{i}");
                shape(&["a", "b", "c", extra.as_str()])
            })
            .collect();
        let refs: Vec<&Shape> = many.iter().collect();
        assert_eq!(bounds_decide(&refs), Some(true));
        // One set over twice the size of another: rejected without pairs.
        let (small, big) = (shape(&["a"]), shape(&["a", "b", "c"]));
        assert_eq!(bounds_decide(&[&small, &big]), Some(false));
        // Pairwise overlap the shared keys cannot prove.
        let (x, y, z) = (shape(&["a", "b"]), shape(&["b", "c"]), shape(&["a", "c"]));
        assert_eq!(bounds_decide(&[&x, &y, &z]), None);
        assert!(!shapes_compatible([x, y, z].iter()));
    }

    #[test]
    fn keyless_and_mixed_kinds() {
        let empty = shape(&[]);
        let a = shape(&["a"]);
        let b = shape(&["b"]);
        assert!(shapes_compatible([empty.clone(), a.clone()].iter()));
        assert!(!shapes_compatible([empty, a.clone(), b].iter()));
        let array = Shape {
            kind: "array",
            keys: BTreeSet::new(),
        };
        assert!(!shapes_compatible([a, array].iter()));
    }
}
