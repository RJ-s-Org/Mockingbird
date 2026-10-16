//! Path normalization, path-parameter inference and merge keys.

use std::collections::{BTreeMap, BTreeSet};

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

/// Template for every concrete path (keys are the inputs).
///
/// First every id-like segment becomes a parameter. Then, for each position,
/// paths with the same segment count that agree on every other (templated)
/// segment and show at least three distinct non-id literals there get a
/// parameter too. Names come from [`param_name`] of the closest literal to
/// the left, suffixed `2`, `3`… when repeated within one template.
pub fn infer_templates(paths: &[String]) -> BTreeMap<String, PathTemplate> {
    let mut segs: Vec<Vec<Seg>> = paths.iter().map(|p| split(p)).collect();
    loop {
        let mut changed = false;
        let max_len = segs.iter().map(Vec::len).max().unwrap_or(0);
        for pos in 0..max_len {
            changed |= templatize_position(&mut segs, pos);
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

/// Turns position `pos` into a parameter in every group of same-length paths
/// that agree elsewhere and show enough distinct literals there.
fn templatize_position(segs: &mut [Vec<Seg>], pos: usize) -> bool {
    // Group key: length and every segment but `pos`, with parameters unified.
    let mut groups: BTreeMap<Vec<Option<String>>, Group> = BTreeMap::new();
    for (index, path) in segs.iter().enumerate() {
        let Some(Seg::Literal(value)) = path.get(pos) else {
            continue;
        };
        let key: Vec<Option<String>> = path
            .iter()
            .enumerate()
            .map(|(i, s)| match s {
                _ if i == pos => None,
                Seg::Literal(l) => Some(l.clone()),
                Seg::Param | Seg::Named(_) => Some("{}".to_string()),
            })
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
        for index in members {
            segs[index][pos] = Seg::Param;
            changed = true;
        }
    }
    changed
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
