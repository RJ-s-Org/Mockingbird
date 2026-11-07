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

    pub(super) async fn get(
        &mut self,
        s: &SessionKey,
        t: &str,
        id: &str,
    ) -> Option<StoredResource> {
        if !self.open() {
            return None;
        }
        let r = self.rt.state.get(s, t, id).await;
        let found = self.settle(r);
        self.read |= found.is_some();
        found.flatten()
    }

    pub(super) async fn list(
        &mut self,
        s: &SessionKey,
        t: &str,
        limit: usize,
    ) -> Vec<StoredResource> {
        if !self.open() {
            return Vec::new();
        }
        let r = self.rt.state.list(s, t, limit).await;
        let found = self.settle(r);
        self.read |= found.is_some();
        found.unwrap_or_default()
    }

    /// Whether the store took the write.
    pub(super) async fn upsert(&mut self, s: &SessionKey, r: StoredResource) -> bool {
        if !self.open() {
            return false;
        }
        let rt = self.rt;
        let values = path_values(rt.index().path_fields(&r.resource_type), &r.data);
        let named = (!values.is_empty()).then(|| (r.resource_type.clone(), r.id.clone()));
        let t = r.resource_type.clone();
        let indexed = value_index::Indexed::of(rt, &r);
        let r = rt.state.upsert(s, r).await;
        let ok = self.settle(r).is_some();
        self.wrote |= ok;
        if ok && t != mb_state::INTERNAL_TYPE {
            rt.written_types().remember(s, &t);
        }
        if ok {
            rt.value_index().remember_indexed(s, indexed);
        }
        if ok && let Some((t, id)) = named {
            rt.path_aliases().remember(s, &t, &id, values);
        }
        ok
    }

    /// Whether the store took the delete.
    pub(super) async fn delete(&mut self, s: &SessionKey, t: &str, id: &str) -> bool {
        if !self.open() {
            return false;
        }
        let r = self.rt.state.delete(s, t, id).await;
        let held = self.settle(r);
        let ok = held.is_some();
        self.wrote |= ok;
        if ok {
            self.rt.value_index().forget(s, t, id);
            self.rt.collections().forget_id(s, t, id);
        }
        // A record the store held is gone: the session wrote its type.
        if held == Some(true) && t != mb_state::INTERNAL_TYPE {
            state_lists::note_deleted(self, s, t).await;
        }
        ok
    }

    /// Stores a seeded resource unless one is stored under its id or it was
    /// deleted; `true` when stored.
    pub(crate) async fn insert_seeded(&mut self, s: &SessionKey, r: StoredResource) -> bool {
        if !self.open() {
            return false;
        }
        if self.tracks_deleted
            && deleted::mention_is_deleted(self, s, &r.resource_type, &r.id).await
        {
            return false;
        }
        let rt = self.rt;
        let values = path_values(rt.index().path_fields(&r.resource_type), &r.data);
        let named = (!values.is_empty()).then(|| (r.resource_type.clone(), r.id.clone()));
        let t = r.resource_type.clone();
        let indexed = value_index::Indexed::of(rt, &r);
        let r = rt.state.insert_seeded(s, r).await;
        let stored = self.settle(r).unwrap_or(false);
        self.wrote |= stored;
        if stored {
            rt.written_types().remember(s, &t);
            rt.value_index().remember_indexed(s, indexed);
        }
        if stored && let Some((t, id)) = named {
            rt.path_aliases().remember(s, &t, &id, values);
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
