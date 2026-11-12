//! How a read by path of an id the session does not hold is answered: the mock's
//! `unknown_ids`, or, left unset, what the record's type infers.

use super::*;

/// Whether a read of type `t` answers an id the session does not hold with its 404.
/// `404` and `generate` decide for every type. Left unset ([`UnknownIds::Inferred`]),
/// a type the contract can create ([`BundleIndex::creatable`]) answers 404 once the
/// session holds one of its records it created or listed ([`holds_own`]): the app works
/// with its own records of that type, so an id it never made is missing. A type it
/// cannot create, or has not yet, is reference data the app reads from elsewhere (its
/// config, the signed-in account): generated.
pub(super) async fn not_found(state: &mut StateAccess<'_>, session: &SessionKey, t: &str) -> bool {
    let rt = state.rt;
    match rt.unknown_ids {
        UnknownIds::NotFound => true,
        UnknownIds::Generate => false,
        UnknownIds::Inferred => rt.index().creatable(t) && holds_own(state, session, t).await,
    }
}

/// Whether the session holds a record of type `t` that is not only a plain read's own
/// answer ([`OwnReads`]): one it created, listed, wrote or was seeded with. Reading a
/// config's id stores what was generated for it, which says nothing of the app making
/// records of the type.
async fn holds_own(state: &mut StateAccess<'_>, session: &SessionKey, t: &str) -> bool {
    let rt = state.rt;
    state
        .list(session, t, MAX_SCAN)
        .await
        .iter()
        .any(|r| !rt.own_reads().holds(session, r))
}
