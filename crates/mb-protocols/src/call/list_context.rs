//! The held records a list read's prompt shows: those its page can show, as its items
//! declare them, so the prompt stays bounded however many records the session holds.
//!
//! A list read (no target) is shown the records of its type the session holds, which
//! its answer is to include unchanged (see [`relations::related`]). A record a read by
//! id stored holds the whole detail answer: every member the detail declares, the parts
//! a query embedded (`append_to_response`, `expand`), long text. A list shows none of
//! that but its items' members, so each record is shown as the list's items declare it
//! ([`lists::projected`]). Only records the page can show are: one whose member holds
//! another value than the request's filter asks ([`lists::contradicts`], the described
//! query's filters and scope where the world describes them), or than the path's parent
//! parameter names, is left out, as the answer's own filters would take it out again.
//! Then the newest, at most the requested page size up to
//! [`mb_engine::prompt::MAX_RELATED`], are kept, and at most [`MAX_LISTED_BYTES`] of
//! them.
//!
//! Where the answer's items are overlaid with what the store holds
//! ([`lists::overlay_stored`]: REST, with consistency on, items that declare their
//! members and are of the listed type), a text longer than [`MAX_SHOWN_TEXT`]
//! characters is cut, but for the identity and in a record shown whole: the item the
//! model names by the record's identity shows the stored text whole again.

use super::*;

/// Records of the list's type a narrowed list read looks through for those its page can
/// show; one that narrows nothing looks at [`LIST_CONTEXT_LIMIT`], as before.
const MAX_SCANNED: usize = 100;

/// The most characters a text of a listed record shows where the answer is overlaid.
const MAX_SHOWN_TEXT: usize = 200;

/// The most bytes the listed records shown take together; the first is always shown.
const MAX_LISTED_BYTES: usize = 24 * 1024;

/// The records of `op`'s type `t` that a list read `call` is shown, nearest first: see
/// the module's doc. A list whose item schema is not known is shown the
/// [`LIST_CONTEXT_LIMIT`] most recent records, as they are held.
pub(super) async fn listed(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    call: &OperationCall,
    t: &str,
) -> Vec<StoredResource> {
    let rt = state.rt;
    let bundle = rt.bundle();
    let Some(items_schema) = list_query::item_schema(bundle, op) else {
        return state.list(session, t, LIST_CONTEXT_LIMIT).await;
    };
    let (item_parts, _) = schema_parts(bundle, Some(&items_schema));
    let q = list_query::of(rt, op, call);
    let page = list_query::requested_page(rt, op, call);
    let page_ref = page.as_ref().map(|(name, limit)| (name.as_str(), *limit));
    let filters: Vec<lists::Filter<'_>> = lists::filters_of(
        bundle,
        op,
        op.input.query.as_ref(),
        &item_parts,
        call,
        page_ref,
    )
    .into_iter()
    .filter(|(param, _)| !q.as_ref().is_some_and(|q| q.pages_with(param)))
    .map(|(_, filter)| filter)
    .collect();
    let field = item_identity(bundle, op, &Value::Null);
    let parents = parents(rt, t, &field, &item_parts, call);
    let narrows = !filters.is_empty() || q.is_some() || !parents.is_empty();
    let scanned = if narrows {
        MAX_SCANNED
    } else {
        LIST_CONTEXT_LIMIT
    };
    // Only where the answer's own items are overlaid with the records they name, by
    // members the items declare and under this very type, is a cut text served whole.
    let item_type = def_name(&items_schema)
        .and_then(|d| rt.index().types_by_def.get(d).cloned())
        .or_else(|| op.resource_type.clone());
    let cuts_text = rt.consistency
        && !matches!(op.binding, Binding::Graphql { .. })
        && op.identity.as_ref().is_none_or(|i| i.record.is_none())
        && !lists::declared_names(&item_parts).is_empty()
        && item_type.as_deref() == Some(t);
    let mut shown: Vec<(StoredResource, bool)> = Vec::new();
    for r in state.list(session, t, scanned).await {
        let item = lists::projected(bundle, Some(&items_schema), &r.data, 0);
        // A record the items' schema does not describe (it lost its identity) is shown
        // whole, as it is held.
        let whole = mb_mcm::member_at(&item, &field)
            .and_then(id_string)
            .is_none()
            && mb_mcm::member_at(&r.data, &field).is_some();
        let item = if whole { r.data.clone() } else { item };
        let other_parent = parents.iter().any(|(member, value)| {
            item.get(*member)
                .and_then(id_string)
                .is_some_and(|held| &held != value)
        });
        if other_parent
            || !list_query::keeps_answered(q.as_ref(), &item, |i| {
                !filters.iter().any(|filter| lists::contradicts(i, filter))
            })
        {
            continue;
        }
        shown.push((StoredResource { data: item, ..r }, whole));
    }
    let size = page
        .and_then(|(_, limit)| {
            let array = list_query::array_schema(bundle, op)?;
            let (array_parts, _) = schema_parts(bundle, Some(&array));
            lists::page_size(limit, &array_parts)
        })
        .unwrap_or(mb_engine::prompt::MAX_RELATED)
        .min(mb_engine::prompt::MAX_RELATED);
    shown.truncate(size);
    let mut bytes = 0;
    let mut out = Vec::with_capacity(shown.len());
    for (mut r, whole) in shown {
        if cuts_text
            && !whole
            && let Value::Object(members) = &mut r.data
        {
            for (name, member) in members.iter_mut() {
                if *name != field {
                    cut_texts(member, 0);
                }
            }
        }
        bytes += r.data.to_string().len();
        if !out.is_empty() && bytes > MAX_LISTED_BYTES {
            break;
        }
        out.push(r);
    }
    out
}

/// The members of the items `item_parts` declare that are tied to a path parameter of
/// `call`, as [`lists::overlay_stored`] ties them, each with the parameter's value: not
/// the items' identity `field`, nor one typed as the items' own type `t`.
fn parents<'a>(
    rt: &MockRuntime,
    t: &str,
    field: &str,
    item_parts: &[&'a Value],
    call: &OperationCall,
) -> Vec<(&'a str, String)> {
    let declared = lists::declared_names(item_parts);
    call.path_params
        .iter()
        .filter_map(|(param, value)| Some((param, id_string(value)?)))
        .flat_map(|(param, value)| {
            declared
                .iter()
                .filter(|member| {
                    **member != field
                        && !mb_mcm::is_generic_id(member)
                        && tied_to_param(member, param)
                        && !typed_as_own(rt, t, member)
                })
                .map(move |member| (*member, value.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// `value` with each text longer than [`MAX_SHOWN_TEXT`] characters cut there, `…`
/// marking the cut.
pub(super) fn cut_texts(value: &mut Value, depth: usize) {
    if depth > 16 {
        return;
    }
    match value {
        Value::String(text) => {
            if let Some((at, _)) = text.char_indices().nth(MAX_SHOWN_TEXT) {
                text.truncate(at);
                text.push('…');
            }
        }
        Value::Object(members) => {
            for member in members.values_mut() {
                cut_texts(member, depth + 1);
            }
        }
        Value::Array(elements) => {
            for element in elements {
                cut_texts(element, depth + 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_long_text_is_cut_and_a_short_one_kept() {
        let long = "é".repeat(MAX_SHOWN_TEXT + 5);
        let mut value = json!({"overview": long, "title": "Fight Club", "tags": [long]});
        cut_texts(&mut value, 0);
        let cut = value["overview"].as_str().unwrap_or_default();
        assert_eq!(cut.chars().count(), MAX_SHOWN_TEXT + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(value["title"], "Fight Club");
        assert_eq!(value["tags"][0], value["overview"]);
    }
}
