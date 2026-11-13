//! Per-runtime memory of path aliases, own reads, numbers and written types.

use super::*;

/// Aliases [`PathAliases`] holds at most; it is emptied when full.
pub(super) const MAX_PATH_ALIASES: usize = 4096;

/// Which stored resource a path field value names (GitHub's issue `number`
/// 412 → the issue stored under its `id`), remembered from this runtime's
/// writes so that a read by number gets the resource instead of scanning
/// the session's resources of the type. Only a hint: a hit is used only
/// when the resource it names still holds that value, and a read that
/// misses (written elsewhere: the Admin API, another node, before a
/// restart) still scans, then remembers what it found.
#[derive(Debug, Default)]
pub(crate) struct PathAliases(pub(super) std::sync::Mutex<HashMap<PathAlias, String>>);

#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct PathAlias {
    pub(super) session: SessionKey,
    pub(super) resource_type: String,
    pub(super) field: String,
    pub(super) value: String,
}

impl PathAliases {
    /// Remembers that resource `id` of type `t` holds each of `values`
    /// (field, value).
    pub(super) fn remember(
        &self,
        session: &SessionKey,
        t: &str,
        id: &str,
        values: Vec<(&str, String)>,
    ) {
        if values.is_empty() {
            return;
        }
        let Ok(mut aliases) = self.0.lock() else {
            return;
        };
        if aliases.len() + values.len() > MAX_PATH_ALIASES {
            aliases.clear();
        }
        for (field, value) in values {
            let key = PathAlias {
                session: session.clone(),
                resource_type: t.to_string(),
                field: field.to_string(),
                value,
            };
            aliases.insert(key, id.to_string());
        }
    }

    /// The id last remembered for `field` = `value`.
    pub(super) fn id_of(
        &self,
        session: &SessionKey,
        t: &str,
        field: &str,
        value: &str,
    ) -> Option<String> {
        let key = PathAlias {
            session: session.clone(),
            resource_type: t.to_string(),
            field: field.to_string(),
            value: value.to_string(),
        };
        self.0.lock().ok()?.get(&key).cloned()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.0.lock().map(|a| a.len()).unwrap_or_default()
    }
}

/// Answers [`OwnReads`] holds at most; it is emptied when full.
pub(super) const MAX_OWN_READS: usize = 4096;

/// Which stored resources are a plain read's own answer, not a write's or a
/// fixture's: a read naming a seed is not answered from them. Remembered
/// from this runtime's reads with a hash of the answer, so any later write
/// that changes the resource ends it with no bookkeeping. Only a hint: on
/// another node, after a restart or once emptied, the resource counts as
/// written and answers such a read as any stored one does.
#[derive(Debug, Default)]
pub(crate) struct OwnReads(pub(super) std::sync::Mutex<HashMap<OwnRead, u64>>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct OwnRead {
    pub(super) session: SessionKey,
    pub(super) resource_type: String,
    pub(super) id: String,
}

impl OwnReads {
    pub(super) fn key(session: &SessionKey, r: &StoredResource) -> OwnRead {
        OwnRead {
            session: session.clone(),
            resource_type: r.resource_type.clone(),
            id: r.id.clone(),
        }
    }

    pub(super) fn digest(data: &Value) -> u64 {
        use std::hash::{DefaultHasher, Hasher};
        let mut hasher = DefaultHasher::new();
        hash_canonical(data, &mut hasher);
        hasher.finish()
    }

    pub(super) fn remember(&self, session: &SessionKey, r: &StoredResource) {
        let Ok(mut reads) = self.0.lock() else {
            return;
        };
        if reads.len() >= MAX_OWN_READS {
            reads.clear();
        }
        reads.insert(Self::key(session, r), Self::digest(&r.data));
    }

    /// Whether `r` is, as it stands, a plain read's own answer.
    pub(super) fn holds(&self, session: &SessionKey, r: &StoredResource) -> bool {
        self.0
            .lock()
            .is_ok_and(|reads| reads.get(&Self::key(session, r)) == Some(&Self::digest(&r.data)))
    }
}

/// Numbers a create's answer must take above those the session holds: each
/// integer field a read of the resource type takes from its path (GitHub's
/// issue `number`). A clashing number moves past the highest stored one under the
/// record's parent, of its type or one numbering with it there
/// ([`StateAccess::numbered_with`]), and URLs naming it (`…/issues/412`) follow.
pub(super) async fn next_numbers(
    rt: &MockRuntime,
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    resource_type: &str,
    generated: &mut GeneratedResponse,
) {
    let fields = rt.index().path_fields(resource_type);
    if fields.is_empty() {
        return;
    }
    let Some(body) = generated.body.as_mut() else {
        return;
    };
    for field in fields.iter().map(String::as_str) {
        let Some(number) = mb_mcm::member_at(body, field).and_then(Value::as_i64) else {
            continue;
        };
        // Under the record's parent, with the types numbering with it there.
        let stored = state
            .numbered_with(session, resource_type, field, MAX_SCAN)
            .await;
        let highest = stored
            .iter()
            .filter_map(|r| mb_mcm::member_at(&r.data, field).and_then(Value::as_i64))
            .max();
        let Some(highest) = highest.filter(|h| number <= *h) else {
            continue;
        };
        let next = highest + 1;
        mb_mcm::set_member_at(body, field, Value::from(next));
        let (old, new) = (
            format!("/{resource_type}/{number}"),
            format!("/{resource_type}/{next}"),
        );
        renumber_urls(body, &old, &new);
    }
}

/// Replaces path `old` with `new` in every string of `value` where it ends
/// a path segment (`…/issues/412`, `…/issues/412/comments`).
pub(super) fn renumber_urls(value: &mut Value, old: &str, new: &str) {
    match value {
        Value::String(text) => {
            let mut out = String::with_capacity(text.len());
            let mut rest = text.as_str();
            while let Some(at) = rest.find(old) {
                let after = &rest[at + old.len()..];
                let ends = after.is_empty() || after.starts_with(['/', '?', '#']);
                out.push_str(&rest[..at]);
                out.push_str(if ends { new } else { old });
                rest = after;
            }
            out.push_str(rest);
            *text = out;
        }
        Value::Array(items) => items.iter_mut().for_each(|v| renumber_urls(v, old, new)),
        Value::Object(obj) => obj.values_mut().for_each(|v| renumber_urls(v, old, new)),
        _ => {}
    }
}

/// Sessions [`WrittenTypes`] holds at most; it is emptied when full.
pub(super) const MAX_WRITTEN_SESSIONS: usize = 1024;

/// Types [`WrittenTypes`] holds per session at most, the latest.
pub(super) const MAX_WRITTEN_TYPES: usize = 32;

/// Which types each session last wrote through this runtime, latest first. Only an
/// order of looking (see [`world::named_by_value`]): a contract can have more types than
/// a call may read, and what the session wrote is what it refers to. A session written
/// to elsewhere (the Admin API, another node) is looked up in the contract's order.
#[derive(Debug, Default)]
pub(crate) struct WrittenTypes(pub(super) std::sync::Mutex<HashMap<SessionKey, Vec<String>>>);

impl WrittenTypes {
    pub(super) fn remember(&self, session: &SessionKey, t: &str) {
        let Ok(mut written) = self.0.lock() else {
            return;
        };
        if written.len() >= MAX_WRITTEN_SESSIONS && !written.contains_key(session) {
            written.clear();
        }
        let types = written.entry(session.clone()).or_default();
        if types.first().is_some_and(|first| first == t) {
            return;
        }
        types.retain(|held| held != t);
        types.insert(0, t.to_string());
        types.truncate(MAX_WRITTEN_TYPES);
    }

    pub(super) fn of(&self, session: &SessionKey) -> Vec<String> {
        self.0
            .lock()
            .ok()
            .and_then(|written| written.get(session).cloned())
            .unwrap_or_default()
    }
}

/// Sessions [`MintCounts`] holds at most; the one that first minted longest ago is
/// dropped for a new one.
const MAX_MINTING_SESSIONS: usize = 4096;
/// First calls [`MintCounts`] counts sessions under, at most; emptied when full.
const MAX_FIRST_CALLS: usize = 16_384;

/// How many ids each session has drawn through this runtime, by type and member, and
/// what the session is known by: the call it first minted in (its operation, path,
/// query and body, [`mb_engine::seed::call_parts`]) and how many sessions before it
/// first minted in that same call. The two seed the next draw
/// ([`header_ids::MintSeed`]), so successive mints in one session differ, a retry after
/// a collision draws anew, sessions differ from each other, and the same calls in a
/// fresh runtime draw the same ids whatever the sessions are named (a client's session
/// names often hold the time) and in whatever order sessions that first mint in
/// different calls start. Only sessions whose first minting calls are alike depend on
/// the order they start in, and concurrent calls of one session on the order they
/// arrive in.
///
/// A session dropped when [`MAX_MINTING_SESSIONS`] are held is counted anew when it
/// mints again, by a new place among its first call's sessions; after the first calls
/// are emptied ([`MAX_FIRST_CALLS`]) places start over, so a new session may draw as a
/// held one does. Either way every draw is still checked against what the session
/// holds, so its ids stay its own, but they depend on the order of sessions.
#[derive(Debug, Default)]
pub(crate) struct MintCounts(std::sync::Mutex<Minting>);

#[derive(Debug, Default)]
pub(crate) struct Minting {
    sessions: HashMap<SessionKey, SessionMints>,
    /// The sessions held, the one that first minted longest ago first.
    order: std::collections::VecDeque<SessionKey>,
    /// How many sessions first minted in each call, by its fingerprint.
    first_calls: HashMap<u64, u64>,
}

/// What one session is known by (its first minting call's fingerprint and its place
/// among the sessions that first minted in it), and its draws.
#[derive(Debug)]
struct SessionMints {
    known_by: (u64, u64),
    counts: HashMap<(String, String), u64>,
}

impl MintCounts {
    /// What `session` is known by (its first minting call's fingerprint, `first` when
    /// this is its first mint, and its place among the sessions that first minted in
    /// that call), and the number of `t`'s `field` it drew before this one, counted.
    pub(super) fn next(
        &self,
        session: &SessionKey,
        first: impl FnOnce() -> u64,
        (t, field): (&str, &str),
    ) -> ((u64, u64), u64) {
        let mut minting = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Minting {
            sessions,
            order,
            first_calls,
        } = &mut *minting;
        if !sessions.contains_key(session) {
            while sessions.len() >= MAX_MINTING_SESSIONS {
                let Some(oldest) = order.pop_front() else {
                    sessions.clear();
                    break;
                };
                sessions.remove(&oldest);
            }
            if first_calls.len() >= MAX_FIRST_CALLS {
                first_calls.clear();
            }
            let fingerprint = first();
            let before = first_calls.entry(fingerprint).or_default();
            let place = *before;
            *before = before.saturating_add(1);
            order.push_back(session.clone());
            sessions.insert(
                session.clone(),
                SessionMints {
                    known_by: (fingerprint, place),
                    counts: HashMap::new(),
                },
            );
        }
        let Some(mints) = sessions.get_mut(session) else {
            return ((0, 0), 0);
        };
        let count = mints
            .counts
            .entry((t.to_string(), field.to_string()))
            .or_default();
        let n = *count;
        *count = count.saturating_add(1);
        (mints.known_by, n)
    }
}
