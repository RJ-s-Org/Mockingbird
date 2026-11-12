//! The held records a single-record read's prompt shows ([`query_roles::single_record_read`]):
//! each trimmed to what its answer needs, so the prompt stays bounded however much the
//! session holds.
//!
//! Since a single-record read stores its record by its path's id whatever its query
//! asks, the record holds every part a query embedded (`append_to_response`), and a
//! read in another language restates it ([`variants`]). A generation over it is shown
//! the target ([`shown_target`]):
//! - a read in another language: without the parts a query embedded that it does not
//!   ask for; the engine then shows what of it holds text, with the values beside that
//!   text, as its text is all that read answers of its own
//!   (`mb_engine::GenerationContext::shown_target`);
//! - a read generating parts it asks to be embedded that the record lacks: the record's
//!   own top-level values, long text cut, as what tells the record apart; the answer is
//!   overlaid with the whole stored record, so only the parts it generates are its own.
//!
//! The related records it is shown leave out the parts a query embedded in them (those
//! their type's detail read offers), and take at most [`MAX_RELATED_BYTES`]
//! ([`shown_related`]).
//!
//! The prompt only: the answer is reflected over the whole stored target, so what a
//! prompt leaves out is answered as stored.

use super::*;

/// The most bytes the related records a single-record read is shown take together; the
/// first is always shown.
const MAX_RELATED_BYTES: usize = 24 * 1024;

/// `target` as the prompt of `call`, a single-record read of `op` generated over it,
/// shows it (see the module's doc); `None` shows it whole. `restates`: the read asks for
/// it in another language ([`variants`]).
pub(super) fn shown_target(
    rt: &MockRuntime,
    op: &Operation,
    call: &OperationCall,
    target: &StoredResource,
    restates: bool,
) -> Option<Value> {
    if !query_roles::single_record_read(rt, op) {
        return None;
    }
    let Value::Object(members) = &target.data else {
        return None;
    };
    let embeds = query_roles::embeds(rt, op, call);
    if restates {
        let unasked = |name: &String| embeds.offered.contains(name) && !embeds.asked.contains(name);
        if !members.keys().any(unasked) {
            return None;
        }
        return Some(Value::Object(
            members
                .iter()
                .filter(|(name, _)| !unasked(name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        ));
    }
    if embeds.missing(&target.data).is_empty() {
        return None;
    }
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let shown = members
        .iter()
        .filter(|(_, value)| !matches!(value, Value::Object(_) | Value::Array(_)))
        .map(|(name, value)| {
            let mut value = value.clone();
            if name != field {
                list_context::cut_texts(&mut value, 0);
            }
            (name.clone(), value)
        })
        .collect();
    Some(Value::Object(shown))
}

/// `related` as the prompt of a single-record read of `op` shows it, nearest first:
/// each record without the parts its type's detail read offers to embed, and at most
/// [`MAX_RELATED_BYTES`] of them. `None` for any other read, or when that leaves them
/// as they are.
pub(super) fn shown_related(
    rt: &MockRuntime,
    op: &Operation,
    related: &[StoredResource],
) -> Option<Vec<StoredResource>> {
    if related.is_empty() || !query_roles::single_record_read(rt, op) {
        return None;
    }
    let bundle = rt.bundle();
    let mut offered: HashMap<&str, Vec<String>> = HashMap::new();
    let mut bytes = 0;
    let mut changed = false;
    let mut out = Vec::new();
    for r in related.iter().take(mb_engine::prompt::MAX_RELATED) {
        let parts = offered.entry(r.resource_type.as_str()).or_insert_with(|| {
            rt.index()
                .detail_reads
                .get(&r.resource_type)
                .and_then(|i| bundle.operations.get(*i))
                .map(|read| query_roles::offered(rt, read))
                .unwrap_or_default()
        });
        let mut shown = r.clone();
        if let Value::Object(members) = &mut shown.data {
            for part in parts.iter() {
                changed |= members.remove(part).is_some();
            }
        }
        bytes += shown.data.to_string().len();
        if !out.is_empty() && bytes > MAX_RELATED_BYTES {
            changed = true;
            break;
        }
        out.push(shown);
    }
    changed.then_some(out)
}
