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

/// A record as [`Collections`] notes it: by its type and its key under the parent the
/// call's state access places it ([`StateAccess::noted`]), so two parents' records of one
/// id are two; a record of a type that is not parent-scoped by its own id.
#[derive(Debug, Clone)]
pub(super) struct Noted<'r> {
    pub(super) key: OwnRead,
    pub(super) r: &'r StoredResource,
}

impl Noted<'_> {
    /// The same record, under the same key, as `r` holds it (a candidate answer for it).
    pub(super) fn with<'s>(&self, r: &'s StoredResource) -> Noted<'s> {
        Noted {
            key: self.key.clone(),
            r,
        }
    }
}

impl Collections {
    pub(super) fn remember(&self, r: &Noted<'_>, collection: Collection) {
        self.place(r, collection, false, false);
    }

    /// Records where this runtime's write of `r` put it, keeping the scope noted for it
    /// (see [`Scope`]), as of `r` now.
    pub(super) fn record(&self, r: &Noted<'_>, collection: Collection, created: bool) {
        self.place(r, collection, created, true);
    }

    /// Records where `r` is, keeping the scope noted for it: as of `r` now when
    /// `written`, this runtime's own write; as it was otherwise (a list showing `r`),
    /// so a scope no longer known ([`ScopeOf::Noted`]) does not come back.
    pub(super) fn place(
        &self,
        r: &Noted<'_>,
        collection: Collection,
        created: bool,
        written: bool,
    ) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        held.make_room();
        let order = held.next_order();
        let key = r.key.clone();
        let digest = OwnReads::digest(&r.r.data);
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

    pub(super) fn forget(&self, r: &Noted<'_>) {
        self.forget_key(&r.key);
    }

    /// Forgets the resource under `key`, its scope with it.
    pub(super) fn forget_key(&self, key: &OwnRead) {
        if let Ok(mut held) = self.0.lock() {
            held.records.remove(key);
            held.top_level.remove(key);
        }
    }

    /// Records the query values the create that stored `r` just carried, beside where
    /// it was placed (see [`Scope`]); nothing where it was placed nowhere known. Nothing
    /// is stored.
    pub(super) fn note_scope(&self, r: &Noted<'_>, query: &Map<String, Value>) {
        let pairs = query_texts(query);
        let scope = (!pairs.is_empty()).then(|| Scope {
            pairs,
            digest: OwnReads::digest(&r.r.data),
        });
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        let key = &r.key;
        if let Some(record) = held.records.get_mut(key) {
            record.scope = scope;
        } else if let Some(top) = held.top_level.get_mut(key) {
            top.scope = scope;
        }
    }

    /// Brings the scope noted for `r` up to date with this runtime's own write of it,
    /// other than its create: it stays, as of `r` now.
    pub(super) fn refresh_scope(&self, r: &Noted<'_>) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        let key = &r.key;
        let digest = OwnReads::digest(&r.r.data);
        if let Some(scope) = held.records.get_mut(key).and_then(|r| r.scope.as_mut()) {
            scope.digest = digest;
        }
        if let Some(scope) = held.top_level.get_mut(key).and_then(|t| t.scope.as_mut()) {
            scope.digest = digest;
        }
    }

    /// What the create that stored `r` carried under query parameter `param` (see
    /// [`ScopeOf`]).
    pub(super) fn carried(&self, r: &Noted<'_>, param: &str) -> ScopeOf {
        let Ok(held) = self.0.lock() else {
            return ScopeOf::Unknown;
        };
        let key = &r.key;
        let Some(scope) = held
            .records
            .get(key)
            .and_then(|record| record.scope.as_ref())
            .or_else(|| held.top_level.get(key).and_then(|t| t.scope.as_ref()))
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
        } else if scope.digest == OwnReads::digest(&r.r.data) {
            ScopeOf::Carried(values)
        } else {
            ScopeOf::Noted
        }
    }

    /// Records that a top-level create stored `r`, in no collection.
    pub(super) fn place_top_level(&self, r: &Noted<'_>) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        held.make_room();
        let order = held.next_order();
        let key = r.key.clone();
        held.records.remove(&key);
        held.top_level.insert(key, TopLevel { order, scope: None });
    }

    /// Where `r` is, as far as this runtime's writes tell: `Some(Some(_))` in a
    /// collection (as [`Collections::get`] says), `Some(None)` stored by a top-level
    /// create in none, and `None` unknown (another node's, a seed's, or forgotten).
    pub(super) fn placement(&self, r: &Noted<'_>) -> Option<Option<Collection>> {
        if let Some(collection) = self.get(r) {
            return Some(Some(collection));
        }
        let held = self.0.lock().ok()?;
        held.top_level.contains_key(&r.key).then_some(None)
    }

    /// The collection `r` was recorded under, as it stands.
    pub(super) fn get(&self, r: &Noted<'_>) -> Option<Collection> {
        let held = self.0.lock().ok()?;
        let record = held.records.get(&r.key)?;
        (record.digest == OwnReads::digest(&r.r.data)).then(|| record.collection.clone())
    }

    /// Whether this runtime's create stored the record under `key` (as
    /// [`StateAccess::noted_id`] gives it), in a collection or at the top level, whatever
    /// was written to it since.
    pub(super) fn created(&self, key: &OwnRead) -> bool {
        self.0.lock().is_ok_and(|held| {
            held.records.get(key).is_some_and(|record| record.created)
                || held.top_level.contains_key(key)
        })
    }

    /// Whether a record for `r`'s key exists, as `r` may not be what it recorded.
    pub(super) fn has(&self, r: &Noted<'_>) -> bool {
        self.0
            .lock()
            .is_ok_and(|held| held.records.contains_key(&r.key))
    }

    /// `old`'s place, in a collection or at the top level, moves to `new`, the record
    /// a write moved to a new key, with its order, its create's mark and its scope;
    /// what was known of `old` as it stood stays known of `new`.
    pub(super) fn rename(&self, old: &Noted<'_>, new: &Noted<'_>) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        let from = &old.key;
        let (was, now) = (OwnReads::digest(&old.r.data), OwnReads::digest(&new.r.data));
        let moved = |scope: &mut Option<Scope>| {
            if let Some(scope) = scope.as_mut().filter(|scope| scope.digest == was) {
                scope.digest = now;
            }
        };
        if let Some(mut record) = held.records.remove(from) {
            if record.digest == was {
                record.digest = now;
            }
            moved(&mut record.scope);
            held.records.insert(new.key.clone(), record);
        }
        if let Some(mut top) = held.top_level.remove(from) {
            moved(&mut top.scope);
            held.top_level.insert(new.key.clone(), top);
        }
    }

    /// What was noted under `from` (a record stored under no parent) is noted under `to`
    /// (its key under the parent a write moved it to), as it stood: its place, order,
    /// create's mark and scope.
    pub(super) fn move_key(&self, from: &OwnRead, to: &OwnRead) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        if let Some(record) = held.records.remove(from) {
            held.records.insert(to.clone(), record);
        }
        if let Some(top) = held.top_level.remove(from) {
            held.top_level.insert(to.clone(), top);
        }
    }

    /// `old`'s record goes on to what replaced it, `new`, a create's mark with it.
    pub(super) fn carry(&self, old: &Noted<'_>, new: &Noted<'_>) {
        let created = {
            let Ok(held) = self.0.lock() else {
                return;
            };
            held.records
                .get(&old.key)
                .is_some_and(|record| record.created)
        };
        if let Some(collection) = self.get(old) {
            self.record(new, collection, created);
        }
    }

    /// The resources a create put in a collection `fits` in `session`, as (type, id) by
    /// their own ids (two parents' of one id twice), the newest first, at most `max`.
    /// They may since have changed or gone: the caller reads them under its own parent
    /// and checks with [`Collections::get`].
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
            .map(|(key, _)| {
                let id = scopes::plain_id(&key.id).to_string();
                (key.resource_type.clone(), id)
            })
            .collect()
    }

    /// Whether `r` is `owner`'s, when a record can say.
    pub(super) fn fits(&self, r: &Noted<'_>, owner: &Owner) -> Option<bool> {
        let collection = self.get(r)?;
        (collection.template == owner.collection.template)
            .then(|| collection.path == owner.collection.path)
    }
}
