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
//! - a read generating parts it asks to be embedded that the record lacks: the record
//!   without the parts a query embedded that it does not ask for, long text cut and
//!   long lists cut to their first items with a count of the rest ([`embed_view`]), so
//!   the parts it generates line up with the record's nested values (a show's seasons,
//!   say); the answer is overlaid with the whole stored record, so only the parts it
//!   generates are its own.
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
    let unasked = |name: &String| embeds.offered.contains(name) && !embeds.asked.contains(name);
    if restates {
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
    Some(embed_view(members, field, unasked))
}

/// The most bytes the nested values [`embed_view`] shows take together, their names
/// counted, before it shows fewer of their items.
const MAX_EMBED_VIEW_BYTES: usize = 3 * 1024;

/// How many items of a list [`embed_view`] shows, tried in turn until its nested values
/// fit [`MAX_EMBED_VIEW_BYTES`]; when none does, they are left out.
const SHOWN_ITEMS: [usize; 3] = [6, 2, 1];

/// Bounds [`project`] (recursive data).
const MAX_PROJECT_DEPTH: usize = 16;

/// `members`, a record a read generates embedded parts over, as its prompt shows it,
/// in its order: without the members `left_out` names; its long texts cut, except the
/// identity `field`'s; and its nested values (objects and lists) [`project`]ed with the
/// most items of [`SHOWN_ITEMS`] that keep them within [`MAX_EMBED_VIEW_BYTES`]. When
/// none does, every nested value is left out and its top-level values alone are shown.
fn embed_view(
    members: &Map<String, Value>,
    field: &str,
    left_out: impl Fn(&String) -> bool,
) -> Value {
    let nested = |value: &Value| matches!(value, Value::Object(_) | Value::Array(_));
    let kept = || members.iter().filter(|(name, _)| !left_out(name));
    let shown = SHOWN_ITEMS.iter().find_map(|items| {
        let projected: Map<String, Value> = kept()
            .filter(|(_, value)| nested(value))
            .map(|(name, value)| (name.clone(), project(value, *items, 0)))
            .collect();
        let bytes: usize = projected
            .iter()
            .map(|(name, value)| name.len() + value.to_string().len())
            .sum();
        (bytes <= MAX_EMBED_VIEW_BYTES).then_some(projected)
    });
    let view = kept()
        .filter_map(|(name, value)| {
            if nested(value) {
                let value = shown.as_ref()?.get(name)?.clone();
                return Some((name.clone(), value));
            }
            let mut value = value.clone();
            if name != field {
                list_context::cut_texts(&mut value, 0);
            }
            Some((name.clone(), value))
        })
        .collect();
    Value::Object(view)
}

/// `value` with each text cut ([`list_context::cut_texts`]) and each list longer than
/// `items` cut to its first `items`, then a note of how many more it holds
/// ([`mb_engine::prompt::cut_list_note`]).
fn project(value: &Value, items: usize, depth: usize) -> Value {
    if depth > MAX_PROJECT_DEPTH {
        return Value::Null;
    }
    match value {
        Value::Object(members) => Value::Object(
            members
                .iter()
                .map(|(name, member)| (name.clone(), project(member, items, depth + 1)))
                .collect(),
        ),
        Value::Array(list) => {
            let mut shown: Vec<Value> = list
                .iter()
                .take(items)
                .map(|item| project(item, items, depth + 1))
                .collect();
            if list.len() > items {
                shown.push(Value::String(mb_engine::prompt::cut_list_note(
                    list.len() - items,
                )));
            }
            Value::Array(shown)
        }
        scalar => {
            let mut scalar = scalar.clone();
            list_context::cut_texts(&mut scalar, 0);
            scalar
        }
    }
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn view(record: Value, left_out: &[&str]) -> Value {
        let Value::Object(members) = record else {
            panic!("not an object")
        };
        embed_view(&members, "id", |name| left_out.contains(&name.as_str()))
    }

    #[test]
    fn an_embed_view_keeps_nested_values_and_cuts_long_lists() {
        let seasons: Vec<Value> = (1..=9).map(|n| json!({"season_number": n})).collect();
        let shown = view(
            json!({"id": 1, "seasons": seasons, "genres": [{"id": 18}], "videos": {"results": []}}),
            &["videos"],
        );
        assert_eq!(
            shown,
            json!({
                "id": 1,
                "seasons": [{"season_number": 1}, {"season_number": 2},
                            {"season_number": 3}, {"season_number": 4},
                            {"season_number": 5}, {"season_number": 6}, "… 3 more not shown"],
                "genres": [{"id": 18}]
            })
        );
    }

    #[test]
    fn an_embed_view_too_long_shows_fewer_items_then_its_own_values() {
        let long = "x".repeat(190);
        let items: Vec<Value> = (0..20)
            .map(|n| json!({"n": n, "a": long, "b": long, "c": long}))
            .collect();
        let shown = view(json!({"id": 1, "items": items, "more": items}), &[]);
        assert_eq!(shown["items"].as_array().map(Vec::len), Some(3), "{shown}");
        assert_eq!(shown["items"][2], "… 18 more not shown");

        let huge = "y".repeat(190);
        let wide: Map<String, Value> = (0..40).map(|n| (format!("k{n}"), json!(huge))).collect();
        let shown = view(json!({"id": 1, "name": "A", "wide": wide}), &[]);
        assert_eq!(shown, json!({"id": 1, "name": "A"}));
    }
}
