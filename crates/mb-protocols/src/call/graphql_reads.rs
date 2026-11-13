//! What the GraphQL adapter's connections and lookups from state (`graphql::held_lists`)
//! read through this module's state access: the records of a type, the soft delete a
//! list leaves out, and whether `unknown_ids` answers a missing record with its 404.

use super::*;

/// The resources of type `t` the session holds, most recent first, at most `limit`,
/// read as a list's are ([`StateAccess::list`]); the session is kept alive, as every
/// call that reads keeps it.
pub(crate) async fn held_of_type(
    rt: &MockRuntime,
    session: &SessionKey,
    t: &str,
    limit: usize,
) -> Vec<StoredResource> {
    let mut state = StateAccess::new(rt);
    async {
        let records = state.list(session, t, limit).await;
        state.touch(session).await;
        records
    }
    .instrument(state_span())
    .await
}

/// Whether `record`, a record of type `t`, is soft-deleted ([`lifecycle::exclusion`])
/// and a list read with `args` leaves it out: unless they send the soft delete's
/// `listed_with` parameter at its value.
pub(crate) fn soft_deleted(
    rt: &MockRuntime,
    t: &str,
    record: &Value,
    args: &serde_json::Map<String, Value>,
) -> bool {
    lifecycle::exclusion(rt, t).is_some_and(|gone| gone.excludes(record) && !gone.lifted_by(args))
}

/// Whether a read of type `t` naming a record the session does not hold answers 404
/// ([`unknown_ids::not_found`]).
pub(crate) async fn answers_404(rt: &MockRuntime, session: &SessionKey, t: &str) -> bool {
    let mut state = StateAccess::new(rt);
    unknown_ids::not_found(&mut state, session, t)
        .instrument(state_span())
        .await
}

/// The list parameter that asks for type `t`'s soft-deleted records too (its soft
/// delete's `listed_with`), which a list reads rather than generates for.
pub(crate) fn listed_with(rt: &MockRuntime, t: &str) -> Option<String> {
    lifecycle::exclusion(rt, t)?.unless.map(|(param, _)| param)
}

/// Whether `generated`, a GraphQL list `op` answers (a list or a connection of a keyed
/// type at its top, [`crate::graphql::listed_node`]), seeds nothing: no model wrote it
/// ([`no_model`]) and the session already holds records of its item type, which its
/// invented items would join in every list from state. A session holding none is
/// seeded with them as before, so a read of an item the list showed agrees with it.
pub(crate) async fn invented_list(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    generated: &GeneratedResponse,
) -> bool {
    if !matches!(op.binding, Binding::Graphql { .. }) || !no_model(generated) {
        return false;
    }
    let Some(node) = op
        .success_output()
        .and_then(|o| o.schema.as_ref())
        .and_then(|schema| crate::graphql::listed_node(state.rt, schema))
    else {
        return false;
    };
    !state
        .list(session, &node.to_lowercase(), 1)
        .await
        .is_empty()
}

/// Whether `generated` was written by no model: the faker's, as none is configured
/// (offline), the provider refused the key (auth or quota), or it is unusable for the
/// run.
pub(crate) fn no_model(generated: &GeneratedResponse) -> bool {
    lasting_fallback(generated)
        || (generated.source == Source::Fallback
            && generated
                .fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with(mb_engine::UNUSABLE_REASON)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(source: Source, reason: Option<&str>) -> GeneratedResponse {
        GeneratedResponse {
            status: 200,
            content_type: None,
            body: None,
            source,
            usage: Usage::default(),
            fallback_reason: reason.map(str::to_string),
            effects: Vec::new(),
            effect_outcomes: Vec::new(),
            dropped_effects: 0,
            new_record: false,
            patched: None,
            headers: Vec::new(),
            world_gaps: Vec::new(),
            exec_notes: serde_json::Map::new(),
        }
    }

    #[test]
    fn no_model_wrote_an_offline_refused_or_unusable_fallback() {
        let fallback = |reason: &str| answered(Source::Fallback, Some(reason));
        assert!(no_model(&fallback(OFFLINE_REASON)));
        assert!(no_model(&fallback("LLM returned HTTP 401: invalid key")));
        assert!(no_model(&fallback(mb_engine::UNUSABLE_REASON)));
        // A failure the next call may get past, and a model's answer.
        assert!(!no_model(&fallback("LLM returned HTTP 429: slow down")));
        assert!(!no_model(&answered(Source::Llm, None)));
    }
}
