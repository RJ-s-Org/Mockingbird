//! Response cache keys and canonical JSON hashing.

use super::*;

/// The response cache key of a read: its operation, path parameters,
/// query (sorted), a query operation's body, the narrowed selection, the
/// call's faker seed (`X-Mockingbird-Seed`) and, when it carries any, the
/// declared headers its prompt shows ([`mb_engine::prompt::shown_headers`]), so a
/// read in another language or at another version is not answered from this one.
pub(super) fn cache_key(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> String {
    use std::hash::{DefaultHasher, Hasher};
    let body = match op.kind {
        OperationKind::Query => call.body.as_ref().map(canonical),
        _ => None,
    };
    let selection = response_schema.map(|schema| {
        let mut hasher = DefaultHasher::new();
        hash_canonical(schema, &mut hasher);
        format!("{:016x}", hasher.finish())
    });
    let mut key = serde_json::json!([
        op.id,
        canonical(&Value::Object(call.path_params.clone())),
        canonical(&Value::Object(call.query.clone())),
        body,
        selection,
        mb_engine::seed::request_seed(call),
    ]);
    // Only a read carrying such a header keys by it: other keys stay as they were.
    let headers = mb_engine::prompt::shown_headers(bundle, op, call);
    if !headers.is_empty()
        && let Some(parts) = key.as_array_mut()
    {
        parts.push(canonical(&Value::Object(headers)));
    }
    key.to_string()
}

/// `value` with every object's keys sorted, whatever map order serde_json
/// keeps.
pub(crate) fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(obj) => {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            // Build an array of pairs: ordered with or without `preserve_order`.
            Value::Array(
                keys.into_iter()
                    .map(|k| Value::Array(vec![Value::String(k.clone()), canonical(&obj[k])]))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Feeds `value` to `hasher` with every object's keys sorted, like hashing
/// [`canonical`]'s output without building it: equal values (whatever their
/// map order) hash equally.
pub(crate) fn hash_canonical<H: std::hash::Hasher>(value: &Value, hasher: &mut H) {
    use std::hash::Hash;
    std::mem::discriminant(value).hash(hasher);
    match value {
        Value::Null => {}
        Value::Bool(b) => b.hash(hasher),
        Value::Number(n) => n.hash(hasher),
        Value::String(s) => s.hash(hasher),
        Value::Array(items) => {
            items.len().hash(hasher);
            for item in items {
                hash_canonical(item, hasher);
            }
        }
        Value::Object(obj) => hash_canonical_members(obj, hasher),
    }
}

/// [`hash_canonical`] of an object's members.
pub(crate) fn hash_canonical_members<H: std::hash::Hasher>(
    obj: &Map<String, Value>,
    hasher: &mut H,
) {
    use std::hash::Hash;
    let mut members: Vec<(&String, &Value)> = obj.iter().collect();
    members.sort_unstable_by(|a, b| a.0.cmp(b.0));
    members.len().hash(hasher);
    for (key, member) in members {
        key.hash(hasher);
        hash_canonical(member, hasher);
    }
}
