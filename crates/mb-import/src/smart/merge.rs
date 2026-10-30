//! Merges the operations proposed for the same method and path (by
//! [`path_key`]) into one, keeping the best-supported details.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::assemble::{compile_draft, schema_warnings};
use super::infer::schema_richness;
use super::model::{
    DraftBody, DraftOperation, DraftParam, DraftResponse, DraftWarning, MAX_PROVENANCE,
    ParamLocation,
};
use super::model::{DraftMeta, OperationMeta};
use super::paths::path_key;
use super::{DraftResult, MAX_EXAMPLES, SmartError, compile_error};
use crate::{SourceFormat, detect_format, parse_document};

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

/// The contract version an import merges into.
#[derive(Clone, Debug)]
pub struct MergeBase {
    pub version: i64,
    pub source: Vec<u8>,
    pub meta: Option<DraftMeta>,
}

const HTTP_METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// `(method, path)` of every operation in an OpenAPI document's `paths`.
fn document_operations(doc: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (path, item) in doc
        .get("paths")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        for method in HTTP_METHODS {
            if item.get(method).is_some_and(Value::is_object) {
                out.push((method.to_string(), path.clone()));
            }
        }
    }
    out
}

/// Every `operationId` in a document's `paths`.
fn operation_ids(doc: &Value) -> HashSet<String> {
    document_operations(doc)
        .iter()
        .filter_map(|(method, path)| {
            doc.get("paths")?
                .get(path)?
                .get(method)?
                .get("operationId")?
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

/// `base`'s document with `incoming`'s operations added: an operation with
/// the same method and path (by [`path_key`]) is replaced, nothing is
/// removed. Base `info` and `servers` are kept (new incoming servers
/// appended); components merge per name with the base winning (a differing
/// incoming one gives a `conflicting_component` warning); an incoming
/// `operationId` that a kept base operation uses gets a `_2`, `_3`… suffix
/// and `x-original-operation-id`. The result is compiled.
///
/// CPU-bound: callers run it on the blocking pool.
///
/// # Errors
///
/// [`SmartError::Unsupported`] when the base version is not an OpenAPI 3
/// document, and [`SmartError::Compile`] when the merged draft does not
/// compile.
pub fn merge_drafts(base: &MergeBase, incoming: DraftResult) -> Result<DraftResult, SmartError> {
    let refuse = || {
        SmartError::Unsupported(format!(
            "contract version {} is not an OpenAPI 3 document, so the import cannot merge into \
             it; import with mode replace",
            base.version
        ))
    };
    let mut doc = parse_document(&base.source).map_err(|_| refuse())?;
    if !matches!(detect_format(&doc), Ok(SourceFormat::OpenApi3)) {
        return Err(refuse());
    }
    let mut warnings: Vec<DraftWarning> = Vec::new();

    // Base operations the incoming ones replace (by method and path).
    let incoming_ops = document_operations(&incoming.document);
    let incoming_keys: HashSet<(String, String)> =
        incoming_ops.iter().map(|(m, p)| path_key(m, p)).collect();
    for (method, path) in document_operations(&doc) {
        if !incoming_keys.contains(&path_key(&method, &path)) {
            continue;
        }
        if let Some(Value::Object(paths)) = doc.get_mut("paths") {
            let empty = match paths.get_mut(&path) {
                Some(Value::Object(item)) => {
                    item.remove(&method);
                    !HTTP_METHODS.iter().any(|m| item.contains_key(*m))
                }
                _ => false,
            };
            if empty {
                paths.remove(&path);
            }
        }
    }

    // Incoming operations, renamed away from the kept base ids.
    let mut taken = operation_ids(&doc);
    let mut renamed: HashMap<String, String> = HashMap::new();
    let mut incoming_ids: HashSet<String> = HashSet::new();
    for (method, path) in &incoming_ops {
        let Some(mut op) = incoming
            .document
            .get("paths")
            .and_then(|p| p.get(path))
            .and_then(|i| i.get(method))
            .cloned()
        else {
            continue;
        };
        if let Some(own) = op
            .get("operationId")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            let mut id = own.clone();
            let mut n = 2;
            while taken.contains(&id) {
                id = format!("{own}_{n}");
                n += 1;
            }
            if id != own
                && let Value::Object(o) = &mut op
            {
                o.insert("operationId".into(), json!(id));
                o.entry("x-original-operation-id")
                    .or_insert_with(|| json!(own));
                renamed.insert(own, id.clone());
            }
            taken.insert(id.clone());
            incoming_ids.insert(id);
        }
        let Value::Object(root) = &mut doc else {
            return Err(refuse());
        };
        let paths = root
            .entry("paths")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(paths) = paths {
            let item = paths
                .entry(path.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(item) = item {
                item.insert(method.clone(), op);
            }
        }
    }

    let Value::Object(root) = &mut doc else {
        return Err(refuse());
    };
    // Servers: the base's, then new incoming ones.
    let incoming_servers: Vec<Value> = incoming
        .document
        .get("servers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !incoming_servers.is_empty() {
        let servers = root
            .entry("servers")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(servers) = servers {
            for server in incoming_servers {
                let url = server.get("url");
                if !servers.iter().any(|s| s.get("url") == url) {
                    servers.push(server);
                }
            }
        }
    }
    // Components: per name, the base wins.
    if let Some(Value::Object(sections)) = incoming.document.get("components") {
        let components = root
            .entry("components")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(components) = components {
            for (section, entries) in sections {
                let Value::Object(entries) = entries else {
                    continue;
                };
                let target = components
                    .entry(section.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                let Value::Object(target) = target else {
                    continue;
                };
                for (name, value) in entries {
                    match target.get(name) {
                        None => {
                            target.insert(name.clone(), value.clone());
                        }
                        Some(existing) if existing != value => {
                            warnings.push(DraftWarning::new(
                                "conflicting_component",
                                format!(
                                    "components/{section}/{name} differs from contract version {}; \
                                     the version's definition was kept",
                                    base.version
                                ),
                            ));
                        }
                        Some(_) => {}
                    }
                }
            }
        }
    }

    let (source, bundle) = compile_draft(&doc).map_err(|issues| compile_error(&issues))?;

    let base_meta = base.meta.as_ref();
    let mut incoming_meta: BTreeMap<String, OperationMeta> = BTreeMap::new();
    for (id, meta) in &incoming.meta.operations {
        let id = renamed.get(id).cloned().unwrap_or_else(|| id.clone());
        incoming_meta.insert(id, meta.clone());
    }
    let operations: BTreeMap<String, OperationMeta> = bundle
        .operations
        .iter()
        .map(|op| {
            let meta = if incoming_ids.contains(&op.id) {
                incoming_meta.get(&op.id).cloned()
            } else {
                base_meta.and_then(|m| m.operations.get(&op.id)).cloned()
            };
            let meta = meta.unwrap_or(OperationMeta {
                confidence: 1.0,
                provenance: Vec::new(),
                edited: false,
            });
            (op.id.clone(), meta)
        })
        .collect();
    // Base warnings about an operation the import touched (by method and
    // path, so `{id}` and `{petId}` match), and about a merge the base
    // itself skipped, are superseded by the import's.
    let superseded = |w: &DraftWarning| {
        w.code == "merge_skipped"
            || w.operation.as_deref().is_some_and(|t| {
                t.split_once(' ')
                    .is_some_and(|(m, p)| incoming_keys.contains(&path_key(m, p)))
            })
    };
    let mut all_warnings: Vec<DraftWarning> = base_meta
        .map(|m| m.warnings.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.code != "schema_too_large" && !superseded(w))
        .collect();
    all_warnings.extend(
        incoming
            .meta
            .warnings
            .iter()
            .filter(|w| w.code != "schema_too_large")
            .cloned(),
    );
    all_warnings.extend(warnings);
    all_warnings.extend(schema_warnings(&bundle));
    let mut seen = HashSet::new();
    all_warnings.retain(|w| seen.insert((w.code.clone(), w.operation.clone(), w.message.clone())));
    let meta = DraftMeta {
        warnings: all_warnings,
        operations,
        merged_from: Some(base.version),
        ..incoming.meta
    };
    Ok(DraftResult {
        document: doc,
        source,
        bundle,
        meta,
    })
}
