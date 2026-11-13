//! A call's guarded access to the state store.

use super::*;

/// State-store access for one call. The first failing call marks the mock
/// degraded and makes the rest of the call stateless; a successful call
/// clears the mark. While degraded, [`crate::StateHealth`] lets one call per
/// retry interval try the store; the rest skip it entirely. The breaker is
/// consulted lazily, on the first actual store call, so a call that never
/// touches the store does not use up the retry.
pub(crate) struct StateAccess<'a> {
    pub(super) rt: &'a MockRuntime,
    pub(super) gate: Gate,
    /// A read succeeded, so the session needs a [`StateAccess::touch`].
    pub(super) read: bool,
    /// A write succeeded; writes refresh the session themselves.
    pub(super) wrote: bool,
    /// This call keeps and honours deleted markers (see [`deleted`]).
    pub(super) tracks_deleted: bool,
    /// The path parameters of the call, for the parent of what it mentions.
    pub(super) call_params: Option<Box<OperationCall>>,
    /// The type a write call (a command) acts on: storing or removing a record of it
    /// marks that the session works with its own records of the type ([`Self::own`]).
    /// `None` for a read, whose stored answers mark nothing.
    pub(super) owns: Option<String>,
    /// The types this call marked already.
    marked: Vec<String>,
    /// Parent-scoped records stored under no parent this call took for its parent's
    /// (type, id): a write of one under the parent moves it there.
    unplaced: Vec<(String, String)>,
    /// Parent-scoped records this call found under the parent it names, by its key
    /// there: (type, id) ([`Self::found_placed`]).
    placed: HashSet<(String, String)>,
    /// The call's type and path parameters, for the parent of a parent-scoped record
    /// ([`scopes`]); `None` outside a call's path (a GraphQL helper), where a record's
    /// links alone name its parent.
    scope_call: Option<Box<CallScope>>,
}

/// What a call says of the parent of the records it reads and writes.
struct CallScope {
    /// The type the call acts on.
    own: Option<String>,
    /// Its route's path template.
    route: String,
    /// Its path parameters.
    params: Map<String, Value>,
}

/// Where a record of a type is, in one call (see [`StateAccess::placement`]).
enum Placement {
    /// Its type is not parent-scoped: under its own id.
    Global,
    /// Under this parent.
    In(Vec<String>),
    /// Its type is parent-scoped, and nothing names its parent: stored under its own
    /// id, found under it, and listed with every parent's.
    Unknown,
}

/// Records of a parent-scoped type read to find one under a parent, at most.
const MAX_SCOPED_SCAN: usize = 10_000;

/// The id of the [`mb_state::INTERNAL_TYPE`] document saying that a session may hold
/// records of parent-scoped type `t` stored under no parent (seeded, written where
/// nothing named a parent, or before the type was scoped): `{"epoch": ...}`, a value
/// each such write gives it anew. The session holds none while the document
/// [`unplaced_none`] names the same epoch: a list under a parent then reads that
/// parent's records alone ([`mb_state::StateStore::list_prefix`]). Otherwise it reads
/// the type, and with no marker yet (the first list of the type in the session, or
/// after a reseed, which replaces the session's documents) writes it.
///
/// It never says none while one is held. A write of one holds a pending document of
/// its own ([`unplaced_pending_prefix`]) and gives the marker a new epoch before it stores
/// the record, and drops the pending document after. "None" for an epoch is written
/// only by a call that read the marker at that epoch, then saw no pending document,
/// then read the type and found none: a write that gave the marker its epoch before
/// that read is seen pending or its record read, and one after gives it another
/// epoch, which no "none" names. Nothing deletes the marker, so a first marker is
/// written only where none is yet (`insert_seeded`) by the same order.
fn unplaced_marker(t: &str) -> String {
    format!("#unplaced/{t}")
}

/// The id of the document naming the epoch of [`unplaced_marker`] at which the
/// session held no record of parent-scoped type `t` stored under no parent.
fn unplaced_none(t: &str) -> String {
    format!("#unplaced/{t}{}none", scopes::SEP)
}

/// The ids of the pending documents of writes storing records of parent-scoped type
/// `t` under no parent ([`unplaced_marker`]) start so.
fn unplaced_pending_prefix(t: &str) -> String {
    format!("#unplaced/{t}{}pending{}", scopes::SEP, scopes::SEP)
}

/// A new value: a pending document's id ([`unplaced_pending_prefix`]) ends with one, and
/// each write gives the marker one ([`unplaced_marker`]).
fn unplaced_nonce() -> String {
    format!("{:016x}", rand::random::<u64>())
}

/// What a session's marker says of records of parent-scoped type `t` stored under no
/// parent ([`unplaced_marker`]).
enum Unplaced {
    /// No marker yet.
    Unmarked,
    /// It holds none.
    None,
    /// It may hold some, at this epoch.
    May(String),
    /// A session another mock shares may hold records that mock stores under no parent
    /// without marking them (its contract may not scope the type): never trusted.
    Untrusted,
}

/// Whether a record of type `t` under `id` is none the call layer may name: its ids
/// never hold the separator a parent-scoped key is joined by ([`scopes::SEP`]), so one
/// sent so (`/repos/c/y/issues/a%1Fx%1F5`) names no record and stores none. The
/// runtime's own documents ([`mb_state::INTERNAL_TYPE`]) key by scoped ids freely.
fn refused(t: &str, id: &str) -> bool {
    t != mb_state::INTERNAL_TYPE && id.contains(scopes::SEP)
}

/// Whether `r`, a record of `rt`'s stored under no parent, may be the one under
/// `scope`: its own links show no other parent ([`scopes::ParentScope::of_data`]).
pub(super) fn unplaced_fits(rt: &MockRuntime, r: &StoredResource, scope: &[String]) -> bool {
    rt.index()
        .parent_scopes()
        .get(&r.resource_type)
        .and_then(|s| s.of_data(&r.data))
        .is_none_or(|shown| shown == scope)
}

/// `r` under its own id, whatever key the store holds it by; a document of the
/// runtime's own as it is.
fn unscoped(mut r: StoredResource) -> StoredResource {
    if r.resource_type != mb_state::INTERNAL_TYPE && r.id.contains(scopes::SEP) {
        r.id = scopes::plain_id(&r.id).to_string();
    }
    r
}

/// Whether this call may use the store.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Gate {
    /// No store call yet; the breaker has not been asked.
    Unasked,
    Open,
    /// Held off by the breaker, or a store call failed.
    Closed,
}

impl<'a> StateAccess<'a> {
    pub(super) fn new(rt: &'a MockRuntime) -> Self {
        Self {
            rt,
            gate: Gate::Unasked,
            read: false,
            wrote: false,
            tracks_deleted: false,
            call_params: None,
            owns: None,
            marked: Vec::new(),
            unplaced: Vec::new(),
            placed: HashSet::new(),
            scope_call: None,
        }
    }

    /// Marks, durably, that the session works with its own records of type `t`
    /// ([`mb_state::owned_type_id`]): what `unknown_ids` left unset goes by
    /// ([`unknown_ids`]). Once per call and type.
    pub(super) async fn own(&mut self, s: &SessionKey, t: &str) {
        if t == mb_state::INTERNAL_TYPE || self.marked.iter().any(|m| m == t) || !self.open() {
            return;
        }
        let marker = StoredResource {
            resource_type: mb_state::INTERNAL_TYPE.to_string(),
            id: mb_state::owned_type_id(t),
            data: serde_json::json!({"owns": t}),
        };
        let r = self.rt.state.upsert(s, marker).await;
        if self.settle(r).is_some() {
            self.wrote = true;
            self.marked.push(t.to_string());
        }
    }

    /// Marks `t` owned when it is the type this write call acts on.
    async fn own_written(&mut self, s: &SessionKey, t: &str) {
        if self.owns.as_deref() == Some(t) {
            self.own(s, t).await;
        }
    }

    /// Keeps the session alive once per call: reads do not refresh it, so a
    /// call that only read touches it here. A no-op when nothing was read,
    /// when a write already refreshed it, or when the store is unavailable.
    pub(super) async fn touch(&mut self, s: &SessionKey) {
        if self.read && !self.wrote && self.gate == Gate::Open {
            let r = self.rt.state.touch(s).await;
            self.settle(r);
        }
    }

    /// Whether the session is live, refreshing it if so. `false` when it is not, or the
    /// store is unavailable.
    pub(super) async fn live(&mut self, s: &SessionKey) -> bool {
        if !self.open() {
            return false;
        }
        let r = self.rt.state.touch_live(s).await;
        self.settle(r).unwrap_or(false)
    }

    /// Whether the store answered this call's reads: a missing resource is
    /// then known to be missing, not merely unread.
    pub(super) fn available(&self) -> bool {
        self.gate == Gate::Open
    }

    /// Whether the next store call may run, asking the breaker on first use.
    pub(super) fn open(&mut self) -> bool {
        if self.gate == Gate::Unasked {
            self.gate = if self.rt.state_health.try_acquire() {
                Gate::Open
            } else {
                Gate::Closed
            };
        }
        self.gate == Gate::Open
    }

    /// `None` when the store is unavailable for this call.
    pub(super) fn settle<T>(&mut self, result: Result<T, StateError>) -> Option<T> {
        match result {
            Ok(v) => {
                self.rt.state_health.record_success();
                Some(v)
            }
            Err(e) => {
                tracing::warn!(mock = %self.rt.name, error = %e, "state store failed; serving statelessly");
                self.rt.state_health.record_failure();
                metrics::counter!("mb_state_errors_total", "mock" => self.rt.name.clone())
                    .increment(1);
                self.gate = Gate::Closed;
                None
            }
        }
    }

    /// Has this call place parent-scoped records ([`scopes`]) by `op`'s type and
    /// `call`'s path parameters.
    pub(super) fn scope_to(&mut self, op: &Operation, call: &OperationCall) {
        let route = match &op.binding {
            mb_mcm::Binding::Http { path, .. } => Some(path.clone()),
            _ => None,
        };
        self.scope_call = route.map(|route| {
            Box::new(CallScope {
                own: op.resource_type.clone(),
                route,
                params: call.path_params.clone(),
            })
        });
    }

    /// Where a record of type `t` (holding `data`, when known) is in this call: a type
    /// that is not parent-scoped is global; the call's own type is under the parent the
    /// call's path names, else under the one its links show; another type is under the
    /// one its links show, else the one the call's path names.
    fn placement(&self, t: &str, data: Option<&Value>) -> Placement {
        let Some(scope) = self.rt.index().parent_scopes().get(t) else {
            return Placement::Global;
        };
        let call = self.scope_call.as_deref();
        let from_call = || call.and_then(|c| scope.of_route(&c.route, &c.params));
        if call.is_some_and(|c| c.own.as_deref() == Some(t))
            && let Some(found) = from_call()
        {
            return Placement::In(found);
        }
        if let Some(found) = data.and_then(|d| scope.of_data(d)) {
            return Placement::In(found);
        }
        from_call().map_or(Placement::Unknown, Placement::In)
    }

    /// The parent this call places type `t`'s records under, when it is parent-scoped
    /// and the call names one.
    pub(super) fn parent_of(&self, t: &str) -> Option<Vec<String>> {
        match self.placement(t, None) {
            Placement::In(scope) => Some(scope),
            Placement::Global | Placement::Unknown => None,
        }
    }

    /// The records a new number of type `t`'s member `member` must pass (one past the
    /// highest they hold). Of a parent-scoped type whose key `member` is, those under
    /// the parent this call places `t`, of `t` and of the types numbering with it there
    /// ([`scopes::ParentScopes::numbering`]); any other member of such a type counts
    /// across every parent's records; any other type's as [`Self::list`] finds them,
    /// but where the create names its parent (`parent`: the member holding it and its
    /// id, a GraphQL input's `repository` by `repositoryId`), only those holding that
    /// record there. At most `limit` of each type.
    pub(super) async fn numbered_with(
        &mut self,
        s: &SessionKey,
        (t, parent): (&str, Option<(&str, &str)>),
        member: &str,
        limit: usize,
    ) -> Vec<StoredResource> {
        let scopes = self.rt.index().parent_scopes();
        let Some(scope) = scopes.get(t) else {
            let listed = self.list(s, t, limit).await;
            let Some((holder, id)) = parent else {
                return listed;
            };
            let under = |r: &StoredResource| {
                r.data
                    .get(holder)
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                    == Some(id)
            };
            return listed.into_iter().filter(under).collect();
        };
        match self.parent_of(t) {
            Some(parent) if scope.key_member() == member => {
                let mut out = Vec::new();
                for other in scopes.numbering(t, member) {
                    out.extend(self.list_in(s, &other, &parent, limit).await);
                }
                out
            }
            _ => self
                .raw_list(s, t, MAX_SCOPED_SCAN)
                .await
                .into_iter()
                .map(unscoped)
                .collect(),
        }
    }

    /// What a document of the call's own (the write memory's, a concurrency token's)
    /// names the record of type `t` under `id` by: its key under the parent this call
    /// places it, so that two parents' records of one id keep two documents.
    pub(super) fn key_id(&self, t: &str, id: &str) -> String {
        match self.placement(t, None) {
            Placement::In(scope) if !id.contains(scopes::SEP) => scopes::scoped_id(&scope, id),
            Placement::In(_) | Placement::Global | Placement::Unknown => id.to_string(),
        }
    }

    /// Whether this call found `r` (under its own id) under the parent it names, by
    /// its key there: the parent's own record.
    pub(super) fn found_placed(&self, r: &StoredResource) -> bool {
        self.placed
            .contains(&(r.resource_type.clone(), r.id.clone()))
    }

    /// Whether `r` is in the collection at `path` (`/repos/a/x/issues`) because this
    /// call found it under its parent's key ([`Self::found_placed`]) and `path` is where
    /// that parent's records of its type sit ([`scopes::ParentScope::read_parent`]).
    /// What the runtime noted of the collection a write put a record in is noted by the
    /// record's own id, which two parents' records may share.
    pub(super) fn placed_in(&self, r: &StoredResource, path: &str) -> bool {
        let t = r.resource_type.as_str();
        self.found_placed(r)
            && self
                .rt
                .index()
                .parent_scopes()
                .get(t)
                .zip(self.parent_of(t))
                .and_then(|(scope, parent)| scope.read_parent(&parent))
                .is_some_and(|at| at == path)
    }

    /// Whether `r`, a record stored under no parent (before its type was scoped, by
    /// the Admin API, or where nothing named one), may be the one under `scope`: its
    /// own links show no other parent.
    fn unplaced_fits(&self, r: &StoredResource, scope: &[String]) -> bool {
        unplaced_fits(self.rt, r, scope)
    }

    async fn raw_get(&mut self, s: &SessionKey, t: &str, id: &str) -> Option<StoredResource> {
        if !self.open() {
            return None;
        }
        let r = self.rt.state.get(s, t, id).await;
        let found = self.settle(r);
        self.read |= found.is_some();
        found.flatten()
    }

    async fn raw_list(&mut self, s: &SessionKey, t: &str, limit: usize) -> Vec<StoredResource> {
        if !self.open() {
            return Vec::new();
        }
        let r = self.rt.state.list(s, t, limit).await;
        let found = self.settle(r);
        self.read |= found.is_some();
        found.unwrap_or_default()
    }

    /// The record of type `t` under `id`, placed as `placement`, as the store holds it:
    /// under the parent, else an unplaced one that fits it (see
    /// [`Self::unplaced_fits`]); with no parent known, the one stored under `id` itself:
    /// a record under a parent is found by naming the parent, or listed.
    async fn held(
        &mut self,
        s: &SessionKey,
        t: &str,
        id: &str,
        placement: &Placement,
    ) -> Option<StoredResource> {
        // No id the call layer holds has the separator: one sent so is no record's.
        if refused(t, id) {
            return None;
        }
        match placement {
            Placement::In(scope) => {
                if let Some(hit) = self.raw_get(s, t, &scopes::scoped_id(scope, id)).await {
                    self.placed.insert((t.to_string(), id.to_string()));
                    return Some(hit);
                }
                let unplaced = self.raw_get(s, t, id).await?;
                let fits = self.unplaced_fits(&unplaced, scope);
                if fits {
                    self.note_unplaced(t, id);
                }
                fits.then_some(unplaced)
            }
            // A record under a parent is found by naming the parent, or listed.
            Placement::Global | Placement::Unknown => self.raw_get(s, t, id).await,
        }
    }

    /// The record of type `t` under `id`, under the parent this call places it (see
    /// [`scopes`]); always under its own id, whatever key the store holds it by.
    pub(super) async fn get(
        &mut self,
        s: &SessionKey,
        t: &str,
        id: &str,
    ) -> Option<StoredResource> {
        let placement = self.placement(t, None);
        self.held(s, t, id, &placement).await.map(unscoped)
    }

    /// Records of type `t`, most recent first, at most `limit`: of a parent-scoped type
    /// those under the parent this call places it (and unplaced ones that fit it), else
    /// all of them; always under their own ids.
    pub(super) async fn list(
        &mut self,
        s: &SessionKey,
        t: &str,
        limit: usize,
    ) -> Vec<StoredResource> {
        match self.placement(t, None) {
            Placement::In(scope) => self.list_in(s, t, &scope, limit).await,
            Placement::Global | Placement::Unknown => self
                .raw_list(s, t, limit)
                .await
                .into_iter()
                .map(unscoped)
                .collect(),
        }
    }

    /// [`Self::list`] of type `t` under parent `scope`.
    pub(super) async fn list_in(
        &mut self,
        s: &SessionKey,
        t: &str,
        scope: &[String],
        limit: usize,
    ) -> Vec<StoredResource> {
        let prefix = scopes::scoped_id(scope, "");
        let under = |id: &str| {
            id.strip_prefix(prefix.as_str())
                .filter(|rest| !rest.contains(scopes::SEP))
                .map(str::to_string)
        };
        // The parent's own records only, unless the session may hold the type's records
        // under no parent ([`unplaced_marker`]): those are found by reading the
        // type, so the two are listed in one order.
        let marked = self.unplaced_marked(s, t).await;
        if let Unplaced::None = marked {
            let held = self.raw_list_prefix(s, t, &prefix, limit).await;
            let mut out = Vec::with_capacity(held.len());
            for r in held {
                if let Some(id) = under(&r.id) {
                    self.placed.insert((t.to_string(), id));
                    out.push(unscoped(r));
                }
            }
            return out;
        }
        // What the type's read finds is written to the marker only when no write was in
        // flight after the marker was read ([`unplaced_marker`]).
        let notes = !matches!(marked, Unplaced::Untrusted) && !self.unplaced_in_flight(s, t).await;
        let all = self.raw_list(s, t, MAX_SCOPED_SCAN).await;
        let unplaced = all.iter().any(|r| !r.id.contains(scopes::SEP));
        if notes && all.len() < MAX_SCOPED_SCAN && self.open() {
            match marked {
                // Where the session holds some of the type, so the marker creates no
                // session.
                Unplaced::Unmarked if !all.is_empty() => {
                    self.mark_unplaced_first(s, t, unplaced).await;
                }
                Unplaced::May(epoch) if !unplaced => self.mark_unplaced_none(s, t, epoch).await,
                Unplaced::Unmarked | Unplaced::May(_) | Unplaced::None | Unplaced::Untrusted => {}
            }
        }
        let placed: HashSet<String> = all.iter().filter_map(|r| under(&r.id)).collect();
        let mut out = Vec::new();
        for r in all {
            if out.len() >= limit {
                break;
            }
            if let Some(id) = under(&r.id) {
                self.placed.insert((t.to_string(), id));
                out.push(unscoped(r));
            } else if !r.id.contains(scopes::SEP)
                && !placed.contains(&r.id)
                && self.unplaced_fits(&r, scope)
            {
                self.note_unplaced(t, &r.id);
                out.push(r);
            }
        }
        out
    }

    async fn raw_list_prefix(
        &mut self,
        s: &SessionKey,
        t: &str,
        prefix: &str,
        limit: usize,
    ) -> Vec<StoredResource> {
        if !self.open() {
            return Vec::new();
        }
        let r = self.rt.state.list_prefix(s, t, prefix, limit).await;
        let found = self.settle(r);
        self.read |= found.is_some();
        found.unwrap_or_default()
    }

    /// What the session's marker says of records of parent-scoped type `t` stored under
    /// no parent ([`unplaced_marker`]).
    async fn unplaced_marked(&mut self, s: &SessionKey, t: &str) -> Unplaced {
        if self.rt.shares.is_some() {
            return Unplaced::Untrusted;
        }
        let epoch = |doc: &StoredResource| {
            doc.data
                .get("epoch")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let Some(marker) = self
            .raw_get(s, mb_state::INTERNAL_TYPE, &unplaced_marker(t))
            .await
        else {
            // Unreadable reads as unmarked: the type is read, and nothing is written.
            return match self.open() {
                true => Unplaced::Unmarked,
                false => Unplaced::Untrusted,
            };
        };
        let Some(at) = epoch(&marker) else {
            return Unplaced::Untrusted;
        };
        let none = self
            .raw_get(s, mb_state::INTERNAL_TYPE, &unplaced_none(t))
            .await;
        match none.as_ref().and_then(epoch) {
            Some(none) if none == at => Unplaced::None,
            _ => Unplaced::May(at),
        }
    }

    /// Writes the first marker of parent-scoped type `t` ([`unplaced_marker`]) where none
    /// is yet, and, when it did and the session holds none (`unplaced` false), that it
    /// holds none at its epoch.
    async fn mark_unplaced_first(&mut self, s: &SessionKey, t: &str, unplaced: bool) {
        let epoch = unplaced_nonce();
        let doc = StoredResource {
            resource_type: mb_state::INTERNAL_TYPE.to_string(),
            id: unplaced_marker(t),
            data: serde_json::json!({"epoch": epoch}),
        };
        let r = self.rt.state.insert_seeded(s, doc).await;
        if self.settle(r) == Some(true) && !unplaced {
            self.mark_unplaced_none(s, t, epoch).await;
        }
    }

    /// Writes that the session holds no record of parent-scoped type `t` stored under no
    /// parent at the marker's `epoch` ([`unplaced_marker`]).
    async fn mark_unplaced_none(&mut self, s: &SessionKey, t: &str, epoch: String) {
        if !self.open() {
            return;
        }
        let doc = StoredResource {
            resource_type: mb_state::INTERNAL_TYPE.to_string(),
            id: unplaced_none(t),
            data: serde_json::json!({"epoch": epoch}),
        };
        let r = self.rt.state.upsert(s, doc).await;
        self.settle(r);
    }

    /// Whether a write storing a record of parent-scoped type `t` under no parent may
    /// be in flight: its pending document is held ([`unplaced_marker`]), or the store
    /// could not say.
    async fn unplaced_in_flight(&mut self, s: &SessionKey, t: &str) -> bool {
        if !self.open() {
            return true;
        }
        let r = self
            .rt
            .state
            .list_prefix(s, mb_state::INTERNAL_TYPE, &unplaced_pending_prefix(t), 1)
            .await;
        self.settle(r).is_none_or(|held| !held.is_empty())
    }

    /// Before a record of parent-scoped type `t` is stored under no parent: holds the
    /// write's pending document, then gives the marker a new epoch
    /// ([`unplaced_marker`]). The pending document's id, to drop once the record is
    /// stored ([`Self::unplaced_stored`]); `None` when the store did not take both, and
    /// the record is not to be stored.
    async fn unplaced_storing(&mut self, s: &SessionKey, t: &str) -> Option<String> {
        if !self.open() {
            return None;
        }
        let pending = format!("{}{}", unplaced_pending_prefix(t), unplaced_nonce());
        let doc = |id: String, data: Value| StoredResource {
            resource_type: mb_state::INTERNAL_TYPE.to_string(),
            id,
            data,
        };
        let r = self
            .rt
            .state
            .upsert(s, doc(pending.clone(), serde_json::json!({})))
            .await;
        self.settle(r)?;
        let marker = doc(
            unplaced_marker(t),
            serde_json::json!({"epoch": unplaced_nonce()}),
        );
        let r = self.rt.state.upsert(s, marker).await;
        if self.settle(r).is_none() {
            self.unplaced_stored(s, &pending).await;
            return None;
        }
        Some(pending)
    }

    /// Drops a write's pending document ([`Self::unplaced_storing`]) once its record is
    /// stored, or was not. Tried whatever the call's gate: one left behind (the call
    /// dropped between the two) only keeps the type's marker from saying none again
    /// for the session's life.
    async fn unplaced_stored(&mut self, s: &SessionKey, pending: &str) {
        let _ = self
            .rt
            .state
            .delete(s, mb_state::INTERNAL_TYPE, pending)
            .await;
    }

    /// After a record of parent-scoped type `t` stored under no parent left the session
    /// (deleted, or moved under its parent): when the marker says the session may hold
    /// some, the type is read, as a list under a parent would read it then, and the
    /// marker says none when it holds none ([`unplaced_marker`]).
    async fn unplaced_left(&mut self, s: &SessionKey, t: &str) {
        let Unplaced::May(epoch) = self.unplaced_marked(s, t).await else {
            return;
        };
        if self.unplaced_in_flight(s, t).await {
            return;
        }
        let all = self.raw_list(s, t, MAX_SCOPED_SCAN).await;
        if !self.open() || all.len() >= MAX_SCOPED_SCAN {
            return;
        }
        if !all.iter().any(|r| !r.id.contains(scopes::SEP)) {
            self.mark_unplaced_none(s, t, epoch).await;
        }
    }

    /// Records of type `t` as the store holds them, most recent first, at most `limit`,
    /// each with the parent it is stored under (see [`scopes::split_id`]) and under its
    /// own id: for a reader of another mock's records, which its own contract does not
    /// scope.
    #[cfg(feature = "graphql")]
    pub(super) async fn list_with_parents(
        &mut self,
        s: &SessionKey,
        t: &str,
        limit: usize,
    ) -> Vec<(Option<Vec<String>>, StoredResource)> {
        self.raw_list(s, t, limit)
            .await
            .into_iter()
            .map(|r| {
                let parent = scopes::split_id(&r.id)
                    .map(|(scope, _)| scope.into_iter().map(str::to_string).collect());
                (parent, unscoped(r))
            })
            .collect()
    }

    /// Whether the store took the write. A parent-scoped record goes under the parent
    /// this call places it ([`Self::placement`]), else under its own id.
    pub(super) async fn upsert(&mut self, s: &SessionKey, r: StoredResource) -> bool {
        if refused(&r.resource_type, &r.id) || !self.open() {
            return false;
        }
        let rt = self.rt;
        let values = path_values(rt.index().path_fields(&r.resource_type), &r.data);
        let named = (!values.is_empty()).then(|| (r.resource_type.clone(), r.id.clone()));
        let t = r.resource_type.clone();
        let plain = r.id.clone();
        let indexed = value_index::Indexed::of(rt, &r);
        let r = self.placed(r);
        let moved = r.id != plain;
        let unplaced = !moved && rt.index().parent_scopes().get(&t).is_some();
        // Marked before it is stored: a list between the two reads the type.
        let pending = match unplaced {
            true => match self.unplaced_storing(s, &t).await {
                Some(pending) => Some(pending),
                None => return false,
            },
            false => None,
        };
        let r = rt.state.upsert(s, r).await;
        let ok = self.settle(r).is_some();
        if let Some(pending) = pending {
            self.unplaced_stored(s, &pending).await;
        }
        self.wrote |= ok;
        // The record found under no parent is this parent's now: it goes, so no other
        // parent finds what it was.
        if ok && moved {
            self.retire_unplaced(s, &t, &plain).await;
        }
        if ok && t != mb_state::INTERNAL_TYPE {
            rt.written_types().remember(s, &t);
        }
        if ok {
            rt.value_index().remember_indexed(s, indexed);
        }
        if ok && let Some((t, id)) = named {
            rt.path_aliases().remember(s, &t, &id, values);
        }
        if ok {
            self.own_written(s, &t).await;
        }
        ok
    }

    /// `r` under the key the store holds it by: under its parent, for a parent-scoped
    /// record this call places under one.
    fn placed(&self, r: StoredResource) -> StoredResource {
        match self.placement(&r.resource_type, Some(&r.data)) {
            Placement::In(scope) => StoredResource {
                id: scopes::scoped_id(&scope, &r.id),
                ..r
            },
            Placement::Global | Placement::Unknown => r,
        }
    }

    /// Notes that this call took `t` `id`, stored under no parent, for the parent it
    /// places the type under.
    fn note_unplaced(&mut self, t: &str, id: &str) {
        if !self.unplaced.iter().any(|(ut, uid)| ut == t && uid == id) {
            self.unplaced.push((t.to_string(), id.to_string()));
        }
    }

    /// Removes `t` `id`, stored under no parent, when this call took it for its parent's
    /// (see [`Self::note_unplaced`]) and has now stored it there.
    async fn retire_unplaced(&mut self, s: &SessionKey, t: &str, id: &str) {
        let Some(at) = self
            .unplaced
            .iter()
            .position(|(ut, uid)| ut == t && uid == id)
        else {
            return;
        };
        self.unplaced.swap_remove(at);
        if self.open() {
            let r = self.rt.state.delete(s, t, id).await;
            if self.settle(r) == Some(true) {
                self.unplaced_left(s, t).await;
            }
        }
    }

    /// Whether the store took the delete of the record of type `t` under `id`, under
    /// the parent this call places it.
    pub(super) async fn delete(&mut self, s: &SessionKey, t: &str, id: &str) -> bool {
        if refused(t, id) || !self.open() {
            return false;
        }
        let stored = match self.placement(t, None) {
            Placement::Global => id.to_string(),
            placement => match self.held(s, t, id, &placement).await {
                Some(held) => held.id,
                None => match placement {
                    Placement::In(scope) => scopes::scoped_id(&scope, id),
                    Placement::Global | Placement::Unknown => id.to_string(),
                },
            },
        };
        if !self.open() {
            return false;
        }
        let r = self.rt.state.delete(s, t, &stored).await;
        let mut held = self.settle(r);
        // Whether a record stored under no parent went.
        let scoped = self.rt.index().parent_scopes().get(t).is_some();
        let mut unplaced_gone = scoped && !stored.contains(scopes::SEP) && held == Some(true);
        // An unplaced record of that id that fits the parent goes too: it would read
        // back as the deleted one.
        if stored != id
            && let Placement::In(scope) = self.placement(t, None)
            && let Some(unplaced) = self.raw_get(s, t, id).await
            && self.unplaced_fits(&unplaced, &scope)
        {
            let r = self.rt.state.delete(s, t, id).await;
            if let Some(gone) = self.settle(r) {
                held = Some(held.unwrap_or(false) || gone);
                unplaced_gone |= gone;
            }
        }
        if unplaced_gone {
            self.unplaced_left(s, t).await;
        }
        self.deleted(s, t, id, held).await
    }

    /// Whether the store took the delete of the record of parent-scoped type `t` under
    /// `id` stored under `parent`, wherever this call places `t`.
    #[cfg(feature = "graphql")]
    pub(super) async fn delete_under(
        &mut self,
        s: &SessionKey,
        t: &str,
        parent: &[String],
        id: &str,
    ) -> bool {
        if refused(t, id) || !self.open() {
            return false;
        }
        let r = self
            .rt
            .state
            .delete(s, t, &scopes::scoped_id(parent, id))
            .await;
        let held = self.settle(r);
        self.deleted(s, t, id, held).await
    }

    /// After a delete of the record of type `t` under `id` the store answered `held`
    /// (`Some(true)`: it held one), whether it took it.
    async fn deleted(&mut self, s: &SessionKey, t: &str, id: &str, held: Option<bool>) -> bool {
        let ok = held.is_some();
        self.wrote |= ok;
        if ok {
            self.rt.value_index().forget(s, t, id);
            self.rt.collections().forget_id(s, t, id);
        }
        // A record the store held is gone: the session wrote its type.
        if held == Some(true) && t != mb_state::INTERNAL_TYPE {
            state_lists::note_deleted(self, s, t).await;
            self.own_written(s, t).await;
        }
        ok
    }

    /// Stores a seeded resource unless one is stored under its id (under the parent
    /// this call places it, an unplaced one that fits it included) or it was deleted;
    /// `true` when stored.
    pub(crate) async fn insert_seeded(&mut self, s: &SessionKey, r: StoredResource) -> bool {
        if refused(&r.resource_type, &r.id) || !self.open() {
            return false;
        }
        if self.tracks_deleted
            && deleted::mention_is_deleted(self, s, &r.resource_type, &r.id).await
        {
            return false;
        }
        if let Placement::In(scope) = self.placement(&r.resource_type, Some(&r.data))
            && self
                .raw_get(s, &r.resource_type, &r.id)
                .await
                .is_some_and(|unplaced| self.unplaced_fits(&unplaced, &scope))
        {
            return false;
        }
        let rt = self.rt;
        let values = path_values(rt.index().path_fields(&r.resource_type), &r.data);
        let named = (!values.is_empty()).then(|| (r.resource_type.clone(), r.id.clone()));
        let t = r.resource_type.clone();
        let indexed = value_index::Indexed::of(rt, &r);
        let r = self.placed(r);
        let unplaced = !r.id.contains(scopes::SEP) && rt.index().parent_scopes().get(&t).is_some();
        if !self.open() {
            return false;
        }
        // Marked before it is stored, as [`Self::upsert`] marks.
        let pending = match unplaced {
            true => match self.unplaced_storing(s, &t).await {
                Some(pending) => Some(pending),
                None => return false,
            },
            false => None,
        };
        let r = rt.state.insert_seeded(s, r).await;
        let stored = self.settle(r).unwrap_or(false);
        if let Some(pending) = pending {
            self.unplaced_stored(s, &pending).await;
        }
        self.wrote |= stored;
        if stored {
            rt.written_types().remember(s, &t);
            rt.value_index().remember_indexed(s, indexed);
        }
        if stored && let Some((t, id)) = named {
            rt.path_aliases().remember(s, &t, &id, values);
        }
        if stored {
            self.own_written(s, &t).await;
        }
        stored
    }

    /// [`StateAccess::insert_seeded`] of each of `resources` as one write, so they
    /// list in their order (as [`mb_state::StateStore::insert_seeded_all`] does): the
    /// last is inserted first, each settled on its own, so what a store failing midway
    /// took is still known. The first of two of one id is the one stored. Whether each
    /// was stored, in their order.
    pub(crate) async fn insert_seeded_all(
        &mut self,
        s: &SessionKey,
        resources: Vec<StoredResource>,
    ) -> Vec<bool> {
        let mut stored = vec![false; resources.len()];
        let mut firsts: Vec<(usize, StoredResource)> = Vec::with_capacity(resources.len());
        for (at, r) in resources.into_iter().enumerate() {
            let again = |(_, k): &(usize, StoredResource)| {
                k.resource_type == r.resource_type && k.id == r.id
            };
            if !firsts.iter().any(again) {
                firsts.push((at, r));
            }
        }
        for (at, r) in firsts.into_iter().rev() {
            stored[at] = self.insert_seeded(s, r).await;
        }
        stored
    }
}

/// Stores `resources` in the session: merged over what it holds
/// (`merge`), else only those it does not hold yet. Keeps the session
/// alive.
#[cfg(feature = "graphql")]
pub(crate) async fn store_resources(
    rt: &MockRuntime,
    session: &SessionKey,
    resources: Vec<StoredResource>,
    merge: bool,
) {
    let mut state = StateAccess::new(rt);
    async {
        for mut resource in resources {
            if !merge {
                state.insert_seeded(session, resource).await;
                continue;
            }
            if let Some(held) = state
                .get(session, &resource.resource_type, &resource.id)
                .await
                && let (Value::Object(held), Value::Object(new)) = (held.data, &resource.data)
            {
                let mut merged = held.clone();
                merged.extend(new.clone());
                // Already stored as it is (a create's own write): no
                // second change event.
                if merged == held {
                    continue;
                }
                resource.data = Value::Object(merged);
            }
            state.upsert(session, resource).await;
        }
        state.touch(session).await;
    }
    .instrument(state_span())
    .await;
}

/// The resource of type `t` the session holds under `id`.
#[cfg(feature = "graphql")]
pub(crate) async fn stored(
    rt: &MockRuntime,
    session: &SessionKey,
    t: &str,
    id: &str,
) -> Option<StoredResource> {
    let mut state = StateAccess::new(rt);
    async { state.get(session, t, id).await }
        .instrument(state_span())
        .await
}

/// The resources of type `t` the session holds, most recent first, at most `limit`,
/// each with the parent it is stored under when another mock stores the type
/// parent-scoped (see [`scopes`]).
#[cfg(feature = "graphql")]
pub(crate) async fn stored_with_parents(
    rt: &MockRuntime,
    session: &SessionKey,
    t: &str,
    limit: usize,
) -> Vec<(Option<Vec<String>>, StoredResource)> {
    let mut state = StateAccess::new(rt);
    async { state.list_with_parents(session, t, limit).await }
        .instrument(state_span())
        .await
}

/// Removes from the session the record of a type the mock this one shares stores
/// equivalent to `t` ([`shared::equivalents`]) that is `t`'s record `id`, as a read
/// of `t` finds it ([`stored_with_parents`]): one stored under a parent by the id its
/// shape as `t` gives it (REST's `node_id` as `id`), else by its own; one stored under
/// none by its own. Two parents' records answering `id` leave both. `true` when one
/// went.
#[cfg(feature = "graphql")]
pub(crate) async fn delete_shared(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    (t, id): (&str, &str),
) -> bool {
    let session = &plan.session;
    let Some(props) = rt
        .bundle
        .defs
        .iter()
        .find(|(name, _)| name.to_lowercase() == t)
        .and_then(|(_, def)| def.get("properties")?.as_object())
    else {
        return false;
    };
    let mut state = StateAccess::new(rt);
    async {
        for other in shared::equivalents(rt, t) {
            let held = state
                .list_with_parents(session, &other, MAX_SCOPED_SCAN)
                .await;
            let mut found = held.into_iter().filter(|(parent, record)| {
                let shaped = parent.as_ref().and_then(|_| {
                    shared::reshape(props, &rt.bundle.defs, &record.data)
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
                shaped.as_deref().unwrap_or(&record.id) == id
            });
            let (Some((parent, record)), None) = (found.next(), found.next()) else {
                continue;
            };
            // A type with a described soft delete keeps the record, moved to its
            // deleted state, as [`delete_resource`] keeps it.
            let mut kept = record.data.clone();
            if lifecycle::soft_delete(rt, &other, &mut kept) {
                if kept != record.data {
                    let stored = match &parent {
                        Some(parent) => scopes::scoped_id(parent, &record.id),
                        None => record.id.clone(),
                    };
                    let r = rt
                        .state
                        .upsert(
                            session,
                            StoredResource {
                                data: kept,
                                id: stored,
                                ..record
                            },
                        )
                        .await;
                    state.settle(r);
                }
                state.touch(session).await;
                return true;
            }
            let gone = match &parent {
                Some(parent) => {
                    state
                        .delete_under(session, &other, parent, &record.id)
                        .await
                }
                None => state.delete(session, &other, &record.id).await,
            };
            if gone {
                rt.collections().forget(session, &record);
                if deleted::marks_by_id(rt, plan) {
                    // Gone for a read here, and for the other mock's read under its
                    // parent: by its key there and by its own id, as its delete marks.
                    deleted::mark(&mut state, session, (plan.op, call), t, id, None).await;
                    let under = parent.as_ref().and_then(|parent| {
                        let scope = rt.shared_types.parent_scope(rt, &other)?;
                        Some((scope.read_parent(parent)?, scope))
                    });
                    if let Some((at, scope)) = under {
                        let mut ids = vec![record.id.clone()];
                        if let Some(key) = record.data.get(scope.key_member()).and_then(id_text) {
                            ids.insert(0, key);
                        }
                        deleted::mark_under(&mut state, session, &other, &at, &ids).await;
                    }
                }
                state.touch(session).await;
                return true;
            }
        }
        false
    }
    .instrument(state_span())
    .await
}

/// The text a key's value is named by in a path: a string as is, a number written out.
#[cfg(feature = "graphql")]
fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Removes the resource of type `t` under `id` from the session, if held.
#[cfg(feature = "graphql")]
pub(crate) async fn forget_stored(rt: &MockRuntime, session: &SessionKey, t: &str, id: &str) {
    let mut state = StateAccess::new(rt);
    async {
        if state.delete(session, t, id).await {
            state.touch(session).await;
        }
    }
    .instrument(state_span())
    .await;
}

/// The resources of type `t` the session holds, most recent first, at most
/// `limit`.
#[cfg(feature = "graphql")]
pub(crate) async fn stored_of_type(
    rt: &MockRuntime,
    session: &SessionKey,
    t: &str,
    limit: usize,
) -> Vec<StoredResource> {
    let mut state = StateAccess::new(rt);
    async { state.list(session, t, limit).await }
        .instrument(state_span())
        .await
}

/// Removes `gone`, a resource the session holds, from it, as `op`'s delete
/// `call` asked, and, with `marks`, leaves the marker a REST delete leaves (see
/// [`deleted`]), so that a later read of it is not generated back. Keeps the
/// session alive.
///
/// A type with a described soft delete keeps the record instead, moved to its
/// deleted state ([`lifecycle::soft_delete`]) and with no marker, as a REST
/// delete does: also when the call's own write already soft-deleted it, or its
/// answer, stored since, wrote another state over it.
#[cfg(feature = "graphql")]
pub(crate) async fn delete_resource(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    gone: StoredResource,
    marks: bool,
) {
    let session = &plan.session;
    let mut state = StateAccess::new(rt);
    async {
        let mut kept = gone.data.clone();
        if lifecycle::soft_delete(rt, &gone.resource_type, &mut kept) {
            if kept != gone.data {
                let resource = StoredResource { data: kept, ..gone };
                state.upsert(session, resource).await;
            }
            state.touch(session).await;
            return;
        }
        if state.delete(session, &gone.resource_type, &gone.id).await {
            rt.collections().forget(session, &gone);
        }
        if marks && deleted::marks_by_id(rt, plan) {
            let (t, id) = (gone.resource_type.as_str(), gone.id.as_str());
            deleted::mark(&mut state, session, (plan.op, call), t, id, Some(&gone)).await;
        }
        state.touch(session).await;
    }
    .instrument(state_span())
    .await;
}
