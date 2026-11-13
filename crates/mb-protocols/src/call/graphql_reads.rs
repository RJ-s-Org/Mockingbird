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
