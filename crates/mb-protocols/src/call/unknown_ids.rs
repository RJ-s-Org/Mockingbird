//! How a read by path of an id the session does not hold is answered: the mock's
//! `unknown_ids`, or, left unset, what the record's type infers.

use super::*;

/// Whether a read of type `t` answers an id the session does not hold with its 404.
/// `404` and `generate` decide for every type; `null` is `404` (only a GraphQL lookup
/// answers it differently, `null` without an error). Left unset ([`UnknownIds::Inferred`]),
/// a type the contract can create ([`BundleIndex::creatable`]) answers 404 once the
/// session works with its own records of it ([`owns`]): the app makes them, so an id it
/// never made is missing. A type it cannot create, or has not written yet, is reference
/// data the app reads from elsewhere (its config, the signed-in account): generated.
pub(super) async fn not_found(state: &mut StateAccess<'_>, session: &SessionKey, t: &str) -> bool {
    let rt = state.rt;
    match rt.unknown_ids {
        UnknownIds::NotFound | UnknownIds::Null => true,
        UnknownIds::Generate => false,
        UnknownIds::Inferred => rt.index().creatable(t) && owns(state, session, t).await,
    }
}

/// Whether the session works with its own records of type `t`: a write of the type
/// stored, changed or deleted one, or the Admin API seeded one, which leaves a marker in
/// the store ([`mb_state::owned_type_id`], written by [`StateAccess::own`]). It is
/// durable (a restart, a rebuilt runtime or another node reads it alike) and outlives
/// the records: a session that deleted its last message still owns messages. What only a
/// read stored (a generated answer, a list's or a search's items) leaves none.
async fn owns(state: &mut StateAccess<'_>, session: &SessionKey, t: &str) -> bool {
    state
        .get(
            session,
            mb_state::INTERNAL_TYPE,
            &mb_state::owned_type_id(t),
        )
        .await
        .is_some()
}
