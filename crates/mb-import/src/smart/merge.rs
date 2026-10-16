//! Merges the operations proposed for the same method and path (by
//! [`path_key`]) into one, keeping the best-supported details.

use std::collections::BTreeMap;

use serde_json::Value;

use super::MAX_EXAMPLES;
use super::infer::schema_richness;
use super::model::{
    DraftBody, DraftOperation, DraftParam, DraftResponse, DraftWarning, MAX_PROVENANCE,
    ParamLocation,
};
use super::paths::path_key;

/// Merges duplicates and sorts the result by `(path, method)`.
///
/// The highest-confidence operation of a group is the base: its path text,
/// id and texts win (missing texts are filled from the others). Provenance,
/// tags, parameters (by location and name), examples and responses (by
/// status) are united; two different schemas at one place keep the richer
/// one (the base's on a tie) with a `conflicting_schemas` warning.
pub fn merge_operations(ops: Vec<DraftOperation>) -> (Vec<DraftOperation>, Vec<DraftWarning>) {
    let mut groups: BTreeMap<(String, String), Vec<DraftOperation>> = BTreeMap::new();
    for op in ops {
        groups
            .entry(path_key(&op.method, &op.path))
            .or_default()
            .push(op);
    }
    let mut merged = Vec::with_capacity(groups.len());
    let mut warnings = Vec::new();
    for (_, mut group) in groups {
        // Stable: the first of equally confident operations is the base.
        group.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        let mut rest = group.into_iter();
        let Some(mut base) = rest.next() else {
            continue;
        };
        let mut conflicts: Vec<String> = Vec::new();
        for other in rest {
            merge_into(&mut base, other, &mut conflicts);
        }
        if !conflicts.is_empty() {
            let target = base.target();
            warnings.push(
                DraftWarning::new(
                    "conflicting_schemas",
                    format!(
                        "{target}: sources disagree on the schema of {}; the richer one was kept",
                        conflicts.join(", ")
                    ),
                )
                .for_operation(target),
            );
        }
        merged.push(base);
    }
    merged.sort_by(|a, b| (&a.path, &a.method).cmp(&(&b.path, &b.method)));
    (merged, warnings)
}

/// Names of the `{params}` of a path template, in order.
fn path_params(path: &str) -> Vec<&str> {
    path.split('/')
        .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
        .collect()
}

fn merge_into(base: &mut DraftOperation, other: DraftOperation, conflicts: &mut Vec<String>) {
    base.confidence = base.confidence.max(other.confidence);
    fill(&mut base.operation_id, other.operation_id);
    fill(&mut base.summary, other.summary);
    fill(&mut base.description, other.description);
    for p in other.provenance {
        if base.provenance.len() >= MAX_PROVENANCE {
            break;
        }
        base.provenance.push(p);
    }
    for tag in other.tags {
        if !base.tags.contains(&tag) {
            base.tags.push(tag);
        }
    }

    // Path parameters are renamed positionally to the base's names.
    let renames: Vec<(String, String)> = path_params(&other.path)
        .into_iter()
        .zip(path_params(&base.path))
        .map(|(from, to)| (from.to_string(), to.to_string()))
        .collect();
    for mut param in other.parameters {
        if param.location == ParamLocation::Path
            && let Some((_, to)) = renames.iter().find(|(from, _)| *from == param.name)
        {
            param.name = to.clone();
        }
        let existing = base
            .parameters
            .iter_mut()
            .find(|p| p.location == param.location && p.name == param.name);
        match existing {
            Some(existing) => merge_param(existing, param, conflicts),
            None => base.parameters.push(param),
        }
    }

    match (&mut base.request, other.request) {
        (Some(body), Some(other_body)) => merge_body(body, other_body, conflicts),
        (slot @ None, other_body) => *slot = other_body,
        (Some(_), None) => {}
    }

    for response in other.responses {
        match base
            .responses
            .iter_mut()
            .find(|r| r.status == response.status)
        {
            Some(existing) => merge_response(existing, response, conflicts),
            None => base.responses.push(response),
        }
    }
    base.responses.sort_by_key(|r| r.status);
}

fn fill(slot: &mut Option<String>, other: Option<String>) {
    if slot.as_deref().is_none_or(|s| s.trim().is_empty()) && other.is_some() {
        *slot = other;
    }
}

fn merge_param(base: &mut DraftParam, other: DraftParam, conflicts: &mut Vec<String>) {
    base.required |= other.required;
    fill(&mut base.description, other.description);
    let place = format!("parameter {}", base.name);
    merge_schema(&mut base.schema, other.schema, &place, conflicts);
    merge_examples(&mut base.examples, other.examples);
}

fn merge_body(base: &mut DraftBody, other: DraftBody, conflicts: &mut Vec<String>) {
    merge_schema(
        &mut base.schema,
        other.schema,
        "the request body",
        conflicts,
    );
    merge_examples(&mut base.examples, other.examples);
}

fn merge_response(base: &mut DraftResponse, other: DraftResponse, conflicts: &mut Vec<String>) {
    fill(&mut base.description, other.description);
    fill(&mut base.content_type, other.content_type);
    let place = format!("the response {}", base.status);
    merge_schema(&mut base.schema, other.schema, &place, conflicts);
    merge_examples(&mut base.examples, other.examples);
}

/// Keeps the richer of two different schemas (the base's on a tie).
fn merge_schema(
    base: &mut Option<Value>,
    other: Option<Value>,
    place: &str,
    conflicts: &mut Vec<String>,
) {
    let Some(other) = other.filter(|o| !o.is_null()) else {
        return;
    };
    match base {
        Some(existing) if !existing.is_null() => {
            if *existing != other {
                if !conflicts.iter().any(|c| c == place) {
                    conflicts.push(place.to_string());
                }
                if schema_richness(&other) > schema_richness(existing) {
                    *existing = other;
                }
            }
        }
        _ => *base = Some(other),
    }
}

/// Union of examples, without duplicates, capped at [`MAX_EXAMPLES`].
fn merge_examples(base: &mut Vec<Value>, other: Vec<Value>) {
    for example in other {
        if base.len() >= MAX_EXAMPLES {
            break;
        }
        if !base.contains(&example) {
            base.push(example);
        }
    }
}
