//! The records each collection holds in a session, and their scopes.

use super::*;

/// What [`Collections`] holds at most; it is emptied when full.
pub(super) const MAX_COLLECTIONS: usize = 4096;

/// Which collection a stored resource was listed or written under, from this
/// runtime's calls, with a hash of the resource: a change this runtime did not
/// record (a reset, a fixture) ends it with no bookkeeping. It decides only for
/// a read with the same template, which the contract says is the same
/// collection: `/orgs/octo/teams` for `/orgs/{org}/teams/{slug}`, whatever the
/// resource's members, but not `/orgs/octo/repos` for `/repos/{owner}/{repo}`.
/// Only a hint: otherwise, as on another node or after a restart, the resource
/// must show its owner (see [`shows_owner`]).
#[derive(Debug, Default)]
pub(crate) struct Collections(pub(super) std::sync::Mutex<CollectionRecords>);

/// What [`Collections`] keeps.
#[derive(Debug, Default)]
pub(super) struct CollectionRecords {
    pub(super) records: HashMap<OwnRead, CollectionRecord>,
    /// The resources a top-level create stored, in no collection, until a write puts
    /// them in one.
    pub(super) top_level: HashMap<OwnRead, TopLevel>,
    /// The order of the next record.
    pub(super) next: u64,
}

impl CollectionRecords {
    /// Makes room for one more: when full, the oldest quarter goes, whichever session
    /// it is of, so no session's records are all lost at once.
    pub(super) fn make_room(&mut self) {
        let len = self.records.len() + self.top_level.len();
        if len < MAX_COLLECTIONS {
            return;
        }
        let mut orders: Vec<u64> = self
            .records
            .values()
            .map(|r| r.order)
            .chain(self.top_level.values().map(|t| t.order))
            .collect();
        orders.sort_unstable();
        let keep_from = orders.get(len / 4).copied().unwrap_or(u64::MAX);
        self.records.retain(|_, r| r.order >= keep_from);
        self.top_level.retain(|_, t| t.order >= keep_from);
    }

    pub(super) fn next_order(&mut self) -> u64 {
        let order = self.next;
        self.next += 1;
        order
    }
}

/// The query values the create that stored a resource carried (`org=acme`), as
/// trimmed text, with the hash of the resource as this runtime last wrote it: a list
/// asking them again asks for what the create was made in. This runtime's own writes
/// of the resource keep it, as a write does not move a resource out of its scope, and
/// bring the hash up to date. A change this runtime did not write (another node's, an
/// effect stored past [`write_state`], a reset of the session followed by a record
/// with other members) leaves it noted but no longer known ([`ScopeOf::Noted`]). A
/// reset of the session is not told to the runtime, so a record stored again after one
/// with the very same members finds the scope its create had.
#[derive(Debug, Clone)]
pub(super) struct Scope {
    pub(super) pairs: Vec<(String, String)>,
    pub(super) digest: u64,
}

/// What this runtime knows of the scope a stored resource's create carried, for one
/// query parameter.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ScopeOf {
    /// The create carried these values under it, and the resource is as this runtime
    /// last wrote it.
    Carried(Vec<String>),
    /// The create carried it, but the resource changed since in a way this runtime did
    /// not write: its values are not known to hold, nor is any part of its text.
    Noted,
    /// Not known: no scope noted, or the create did not carry the parameter.
    Unknown,
}

/// A resource a top-level create stored: when, and the scope its create carried.
#[derive(Debug)]
pub(super) struct TopLevel {
    pub(super) order: u64,
    pub(super) scope: Option<Scope>,
}

/// The values of `query` as trimmed, non-empty text, each under its parameter: every
/// element of an array, and nothing of an object.
pub(super) fn query_texts(query: &Map<String, Value>) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (name, value) in query {
        let values = match value {
            Value::Array(values) => values.iter().collect(),
            other => vec![other],
        };
        for value in values {
            if let Some(text) = id_string(value) {
                let text = text.trim();
                if !text.is_empty() {
                    pairs.push((name.clone(), text.to_string()));
                }
            }
        }
    }
    pairs
}

/// Where a resource was recorded, its hash then, when, whether a create put it there,
/// and the scope that create carried (see [`Scope`]).
#[derive(Debug)]
pub(super) struct CollectionRecord {
    pub(super) collection: Collection,
    pub(super) digest: u64,
    pub(super) order: u64,
    pub(super) created: bool,
    pub(super) scope: Option<Scope>,
}

impl Collections {
    pub(super) fn remember(
        &self,
        session: &SessionKey,
        r: &StoredResource,
        collection: Collection,
    ) {
        self.place(session, r, collection, false, false);
    }

    /// Records where this runtime's write of `r` put it, keeping the scope noted for it
    /// (see [`Scope`]), as of `r` now.
    pub(super) fn record(
        &self,
        session: &SessionKey,
        r: &StoredResource,
        collection: Collection,
        created: bool,
    ) {
        self.place(session, r, collection, created, true);
    }

    /// Records where `r` is, keeping the scope noted for it: as of `r` now when
    /// `written`, this runtime's own write; as it was otherwise (a list showing `r`),
    /// so a scope no longer known ([`ScopeOf::Noted`]) does not come back.
    pub(super) fn place(
        &self,
        session: &SessionKey,
        r: &StoredResource,
        collection: Collection,
        created: bool,
        written: bool,
    ) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        held.make_room();
        let order = held.next_order();
        let key = OwnReads::key(session, r);
        let digest = OwnReads::digest(&r.data);
        let scope = held
            .top_level
            .remove(&key)
            .and_then(|top| top.scope)
            .or_else(|| held.records.get(&key).and_then(|was| was.scope.clone()))
            .map(|scope| match written {
                true => Scope { digest, ..scope },
                false => scope,
            });
        // A later write or list there leaves what a create put there.
        let created = created
            || held
                .records
                .get(&key)
                .is_some_and(|was| was.created && was.collection == collection);
        let record = CollectionRecord {
            collection,
            digest,
            order,
            created,
            scope,
        };
        held.records.insert(key, record);
    }

    pub(super) fn forget(&self, session: &SessionKey, r: &StoredResource) {
        self.forget_key(&OwnReads::key(session, r));
    }

    /// Forgets the resource of type `t` and id `id` in `session`, its scope with it.
    pub(super) fn forget_id(&self, session: &SessionKey, t: &str, id: &str) {
        self.forget_key(&OwnRead {
            session: session.clone(),
            resource_type: t.to_string(),
            id: id.to_string(),
        });
    }

    pub(super) fn forget_key(&self, key: &OwnRead) {
        if let Ok(mut held) = self.0.lock() {
            held.records.remove(key);
            held.top_level.remove(key);
        }
    }

    /// Records the query values the create that stored `r` just carried, beside where
    /// it was placed (see [`Scope`]); nothing where it was placed nowhere known. Nothing
    /// is stored.
    pub(super) fn note_scope(
        &self,
        session: &SessionKey,
        r: &StoredResource,
        query: &Map<String, Value>,
    ) {
        let pairs = query_texts(query);
        let scope = (!pairs.is_empty()).then(|| Scope {
            pairs,
            digest: OwnReads::digest(&r.data),
        });
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        let key = OwnReads::key(session, r);
        if let Some(record) = held.records.get_mut(&key) {
            record.scope = scope;
        } else if let Some(top) = held.top_level.get_mut(&key) {
            top.scope = scope;
        }
    }

    /// Brings the scope noted for `r` up to date with this runtime's own write of it,
    /// other than its create: it stays, as of `r` now.
    pub(super) fn refresh_scope(&self, session: &SessionKey, r: &StoredResource) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        let key = OwnReads::key(session, r);
        let digest = OwnReads::digest(&r.data);
        if let Some(scope) = held.records.get_mut(&key).and_then(|r| r.scope.as_mut()) {
            scope.digest = digest;
        }
        if let Some(scope) = held.top_level.get_mut(&key).and_then(|t| t.scope.as_mut()) {
            scope.digest = digest;
        }
    }

    /// What the create that stored `r` carried under query parameter `param` (see
    /// [`ScopeOf`]).
    pub(super) fn carried(&self, session: &SessionKey, r: &StoredResource, param: &str) -> ScopeOf {
        let Ok(held) = self.0.lock() else {
            return ScopeOf::Unknown;
        };
        let key = OwnReads::key(session, r);
        let Some(scope) = held
            .records
            .get(&key)
            .and_then(|record| record.scope.as_ref())
            .or_else(|| held.top_level.get(&key).and_then(|t| t.scope.as_ref()))
        else {
            return ScopeOf::Unknown;
        };
        let values: Vec<String> = scope
            .pairs
            .iter()
            .filter(|(p, _)| p == param)
            .map(|(_, v)| v.clone())
            .collect();
        if values.is_empty() {
            ScopeOf::Unknown
        } else if scope.digest == OwnReads::digest(&r.data) {
            ScopeOf::Carried(values)
        } else {
            ScopeOf::Noted
        }
    }

    /// Records that a top-level create stored `r`, in no collection.
    pub(super) fn place_top_level(&self, session: &SessionKey, r: &StoredResource) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        held.make_room();
        let order = held.next_order();
        let key = OwnReads::key(session, r);
        held.records.remove(&key);
        held.top_level.insert(key, TopLevel { order, scope: None });
    }

    /// Where `r` is, as far as this runtime's writes tell: `Some(Some(_))` in a
    /// collection (as [`Collections::get`] says), `Some(None)` stored by a top-level
    /// create in none, and `None` unknown (another node's, a seed's, or forgotten).
    pub(super) fn placement(
        &self,
        session: &SessionKey,
        r: &StoredResource,
    ) -> Option<Option<Collection>> {
        if let Some(collection) = self.get(session, r) {
            return Some(Some(collection));
        }
        let held = self.0.lock().ok()?;
        held.top_level
            .contains_key(&OwnReads::key(session, r))
            .then_some(None)
    }

    /// The collection `r` was recorded under, as it stands.
    pub(super) fn get(&self, session: &SessionKey, r: &StoredResource) -> Option<Collection> {
        let held = self.0.lock().ok()?;
        let record = held.records.get(&OwnReads::key(session, r))?;
        (record.digest == OwnReads::digest(&r.data)).then(|| record.collection.clone())
    }

    /// Whether this runtime's create stored the record of type `t` under `id` in
    /// `session`, in a collection or at the top level, whatever was written to it since.
    pub(super) fn created(&self, session: &SessionKey, t: &str, id: &str) -> bool {
        let key = OwnRead {
            session: session.clone(),
            resource_type: t.to_string(),
            id: id.to_string(),
        };
        self.0.lock().is_ok_and(|held| {
            held.records.get(&key).is_some_and(|record| record.created)
                || held.top_level.contains_key(&key)
        })
    }

    /// Whether a record for `r`'s key exists, as `r` may not be what it recorded.
    pub(super) fn has(&self, session: &SessionKey, r: &StoredResource) -> bool {
        self.0
            .lock()
            .is_ok_and(|held| held.records.contains_key(&OwnReads::key(session, r)))
    }

    /// `old`'s record goes on to what replaced it, `new`, a create's mark with it.
    pub(super) fn carry(&self, session: &SessionKey, old: &StoredResource, new: &StoredResource) {
        let created = {
            let Ok(held) = self.0.lock() else {
                return;
            };
            held.records
                .get(&OwnReads::key(session, old))
                .is_some_and(|record| record.created)
        };
        if let Some(collection) = self.get(session, old) {
            self.record(session, new, collection, created);
        }
    }

    /// The resources a create put in a collection `fits` in `session`, as (type, id),
    /// the newest first, at most `max`. They may since have changed or gone: the caller
    /// reads them and checks with [`Collections::get`].
    pub(super) fn created_in(
        &self,
        session: &SessionKey,
        fits: impl Fn(&Collection) -> bool,
        max: usize,
    ) -> Vec<(String, String)> {
        let Ok(held) = self.0.lock() else {
            return Vec::new();
        };
        let mut found: Vec<(&OwnRead, u64)> = held
            .records
            .iter()
            .filter(|(key, record)| {
                record.created && key.session == *session && fits(&record.collection)
            })
            .map(|(key, record)| (key, record.order))
            .collect();
        found.sort_by_key(|(_, order)| std::cmp::Reverse(*order));
        found
            .into_iter()
            .take(max)
            .map(|(key, _)| (key.resource_type.clone(), key.id.clone()))
            .collect()
    }

    /// Whether `r` is `owner`'s, when a record can say.
    pub(super) fn fits(
        &self,
        session: &SessionKey,
        r: &StoredResource,
        owner: &Owner,
    ) -> Option<bool> {
        let collection = self.get(session, r)?;
        (collection.template == owner.collection.template)
            .then(|| collection.path == owner.collection.path)
    }
}
