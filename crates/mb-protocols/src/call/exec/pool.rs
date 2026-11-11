//! Record pools: records of each stored type the model wrote ahead of any call, off the
//! request path, so that a read or list the session holds nothing for is answered from
//! state in milliseconds instead of by a model call.
//!
//! [`fill`] runs in the background when a mock is built (on start and on every change of
//! its world). For each type the executor stores (one with an identity, read by id or
//! listed), it asks the model for [`POOL_SIZE`] records with the mock's persona (no more
//! than one page of each of the type's lists admits, its `maxItems`), in calls of
//! [`PER_CALL`], written as the type's own record (its read by id's schema, else the first
//! shape it is read by) and then checked against every schema the type is read by
//! ([`super::create::unreadable`]): a type whose shapes no record meets, such as one with
//! a list whose items are another type's, is left unpooled. It stops at the mock's token cap, counted per build of
//! the runtime (and per replica: each fills its own), and a call is made only while a
//! generation slot (`limits.max_concurrent_generations`) stays free for requests after it
//! takes one, so requests are never queued behind it; otherwise it tries again a second
//! later. A fill holds no runtime across a model call, and only the latest build's fill
//! adds records. The pool is shared by the mock's sessions and kept across rebuilds of its
//! runtime: a type's records stay while the schemas it is read by are unchanged
//! ([`digest`]).
//!
//! Until a type's pool holds records, calls go on as before. Then:
//! - [`read`]: a read by id of a record the session does not hold, on a route whose one
//!   path parameter (or a GraphQL field whose argument) names the type's identity
//!   member, stores a pool record under that id in the session (when it still reads as
//!   the type with that id) and answers it from state;
//! - [`list`]: a list of a type the session never wrote, on a route with no path
//!   parameters and no scope asked for, stores the type's pool records in the session
//!   and answers from state; from then on every such list of the type in the session
//!   answers its records, also where the type has several lists with no path
//!   parameters (which otherwise fall back as `unscoped_list`: the model would invent
//!   each anew). That the session took them is a record of its own ([`SERVED_TYPE`]),
//!   so a reset of the session forgets it with the records.
//!
//! - [`create_fill`]: a create's members the request did not send (and the executor does
//!   not mint) are a pool record's values, with that record's id written as the new
//!   record's wherever a value embeds it; the model fills only what the pool record lacks.
//!
//! A record taken from a pool is a seeded record of the session, as a generated answer's
//! mention is; the request record notes `pool` (how many records the call took).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use mb_engine::OperationCall;
use mb_mcm::{Binding, Operation, OperationKind};
use mb_state::StoredResource;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{OpKind, list};
use crate::MockRuntime;
use crate::call::{CallPlan, StateAccess};

/// The records a pool keeps of each type.
pub const POOL_SIZE: usize = 10;
/// The records one model call writes.
pub(crate) const PER_CALL: usize = 5;
/// The smallest token cap a pool is filled under: below it, not even a few calls fit.
pub const MIN_POOL_TOKENS: u64 = 5_000;
/// The type of a session's record that it took a type's pool records for its lists, one
/// per pooled type (its id the type's name).
pub(crate) const SERVED_TYPE: &str = "__mockingbird.pool";
/// How long a fill waits before trying again when no generation slot was spare.
const BUSY_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// A mock's record pool: by type, the records the model wrote for it, with the digest
/// of the schemas they were checked against, and the tokens spent filling it.
#[derive(Debug, Default)]
pub struct RecordPool {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    types: HashMap<String, Entry>,
    /// The tokens the latest fill spent.
    spent: u64,
    /// The latest fill: an earlier one (of a build since replaced) adds nothing.
    generation: u64,
}

#[derive(Debug, Clone, Default)]
struct Entry {
    digest: String,
    records: Vec<Value>,
}

impl RecordPool {
    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut inner)
    }

    /// The records kept of type `t`, when they were checked against schemas of `digest`.
    fn records(&self, t: &str, digest: &str) -> Vec<Value> {
        self.with(|i| {
            i.types
                .get(t)
                .filter(|e| e.digest == digest)
                .map(|e| e.records.clone())
                .unwrap_or_default()
        })
    }

    /// How many records each type holds.
    #[must_use]
    pub fn sizes(&self) -> HashMap<String, usize> {
        self.with(|i| {
            i.types
                .iter()
                .map(|(t, e)| (t.clone(), e.records.len()))
                .collect()
        })
    }

    /// The tokens the latest fill spent so far.
    #[must_use]
    pub fn spent(&self) -> u64 {
        self.with(|i| i.spent)
    }

    /// Starts a fill: from now on only it adds records, and its spend counts from zero.
    fn begin(&self) -> u64 {
        self.with(|i| {
            i.generation = i.generation.wrapping_add(1);
            i.spent = 0;
            i.generation
        })
    }

    /// Whether the fill begun as `generation` is the latest.
    fn current(&self, generation: u64) -> bool {
        self.with(|i| i.generation == generation)
    }

    /// Adds `records` to type `t`'s, dropping what was kept for other schemas, up to
    /// [`POOL_SIZE`], when the fill begun as `generation` is the latest.
    fn add(&self, generation: u64, t: &str, digest: &str, records: Vec<Value>) {
        self.with(|i| {
            if i.generation != generation {
                return;
            }
            let entry = i.types.entry(t.to_string()).or_default();
            if entry.digest != digest {
                *entry = Entry {
                    digest: digest.to_string(),
                    records: Vec::new(),
                };
            }
            for r in records {
                if entry.records.len() < POOL_SIZE {
                    entry.records.push(r);
                }
            }
        });
    }

    /// Charges a call's `tokens` to the fill begun as `generation`, when it is the latest.
    fn charge(&self, generation: u64, tokens: u64) {
        self.with(|i| {
            if i.generation == generation {
                i.spent = i.spent.saturating_add(tokens);
            }
        });
    }
}

/// The digest of the schemas type `t`'s records are read by.
fn digest(rt: &MockRuntime, t: &str) -> String {
    let schemas = super::read_schemas(rt, t);
    let text = serde_json::to_string(&schemas).unwrap_or_default();
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The types a pool is kept for, each with the operation whose answer the model writes
/// its records as: the type's read by id, else a list of it.
fn pooled_types(rt: &MockRuntime) -> Vec<(String, usize, usize)> {
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    for (i, op) in rt.bundle().operations.iter().enumerate() {
        let kind = rt
            .described()
            .op(&op.id)
            .and_then(|d| d.kind)
            .or_else(|| OpKind::of(op));
        let t = match kind {
            Some(OpKind::List) if list::identified(rt, op, OpKind::List) => list::item_type(rt, op),
            Some(OpKind::Read) if matches!(op.kind, OperationKind::Query) => {
                op.resource_type.as_deref()
            }
            _ => None,
        };
        let Some(t) = t else { continue };
        if super::read_schemas(rt, t).is_empty() {
            continue;
        }
        // As many records as one page of each list of the type can hold, so a list
        // that answers them all validates.
        let cap = match kind {
            Some(OpKind::List) => crate::call::state_lists::Shape::of(rt, op)
                .and_then(|shape| shape.max_items(rt))
                .unwrap_or(POOL_SIZE),
            _ => POOL_SIZE,
        }
        .min(POOL_SIZE);
        match out.iter_mut().find(|(have, _, _)| have == t) {
            Some(entry) => {
                // A read by id says best what one record is.
                if kind == Some(OpKind::Read) {
                    entry.1 = i;
                }
                entry.2 = entry.2.min(cap);
            }
            None => out.push((t.to_string(), i, cap)),
        }
    }
    out.retain(|(_, _, size)| *size > 0);
    out
}

/// Fills the pool of the runtime `rt` points to, in the background: each type up to
/// [`POOL_SIZE`] records, until `cap` tokens are spent by this fill. Stops when the
/// runtime is gone (rebuilt or unmounted) or a later fill began; a later runtime's fill
/// goes on from what is kept.
pub async fn fill(rt: Weak<MockRuntime>, cap: u64) {
    if cap < MIN_POOL_TOKENS {
        return;
    }
    let Some((types, pool)) = rt
        .upgrade()
        .map(|rt| (pooled_types(&rt), Arc::clone(&rt.pool)))
    else {
        return;
    };
    let generation = pool.begin();
    // The most one call has cost: a call that would pass the cap is not made.
    let mut dearest = 0u64;
    for (t, op_index, size) in types {
        loop {
            if !pool.current(generation) {
                return;
            }
            // What the call needs, taken from the runtime, which is then let go: a model
            // call does not keep a replaced or unmounted mock alive.
            let (engine, bundle, op, schema, digest, id_member, taken, count) = {
                let Some(rt) = rt.upgrade() else { return };
                if pool.spent().saturating_add(dearest) >= cap {
                    tracing::info!(mock = %rt.name, spent = pool.spent(), cap, "record pool stopped at its token cap");
                    return;
                }
                if rt
                    .engine
                    .guards()
                    .limiter
                    .as_ref()
                    .is_some_and(|l| l.max() < 2)
                {
                    tracing::info!(mock = %rt.name, "record pool left empty: it needs a generation slot requests do not");
                    return;
                }
                let digest = digest(&rt, &t);
                let have = pool.records(&t, &digest);
                if have.len() >= size {
                    break;
                }
                let Some(op) = rt.bundle().operations.get(op_index).cloned() else {
                    break;
                };
                // The model writes the type's own record: its read by id's schema, else
                // the first shape it is read by. Never every shape at once: a list whose items
                // are another type's (a list of genres typed as movies) would make the
                // model, or the local patching of its answer, take that type's ids and
                // members. Each record is then checked against every shape the type is
                // read by, and a type whose shapes no record meets is left unpooled.
                let Some(schema) = super::read_schemas(&rt, &t).into_iter().next() else {
                    break;
                };
                let id_member = crate::call::exprs::identity_member(&rt, &t);
                let taken: Vec<String> = have
                    .iter()
                    .filter_map(|r| id_text(r.get(&id_member)?))
                    .collect();
                let count = PER_CALL.min(size - have.len());
                (
                    Arc::clone(&rt.engine),
                    Arc::clone(rt.bundle()),
                    op,
                    schema,
                    digest,
                    id_member,
                    taken,
                    count,
                )
            };
            let pooled = engine
                .pool_records(&bundle, &op, &schema, &t, count, &taken)
                .await;
            drop((engine, bundle));
            if pooled.busy {
                tokio::time::sleep(BUSY_RETRY).await;
                continue;
            }
            let cost =
                u64::from(pooled.usage.prompt_tokens) + u64::from(pooled.usage.completion_tokens);
            dearest = dearest.max(cost);
            pool.charge(generation, cost);
            let Some(rt) = rt.upgrade() else { return };
            let mut ids = taken;
            let fresh: Vec<Value> = pooled
                .records
                .into_iter()
                .filter(|r| {
                    let Some(id) = r.get(&id_member).and_then(id_text) else {
                        tracing::debug!(mock = %rt.name, r#type = %t, member = %id_member, "record pool left a record without its identity out");
                        return false;
                    };
                    if ids.contains(&id) {
                        return false;
                    }
                    if let Some(why) = super::create::unreadable(&rt, &t, r, None) {
                        tracing::debug!(mock = %rt.name, r#type = %t, %why, "record pool left a record out");
                        return false;
                    }
                    ids.push(id);
                    true
                })
                .collect();
            if fresh.is_empty() {
                tracing::info!(mock = %rt.name, r#type = %t, "record pool call wrote nothing usable; type left");
                break;
            }
            pool.add(generation, &t, &digest, fresh);
        }
    }
    if pool.current(generation)
        && let Some(rt) = rt.upgrade()
    {
        tracing::info!(mock = %rt.name, sizes = ?pool.sizes(), spent = pool.spent(), "record pool filled");
    }
}

fn id_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Values for the members `names` of a record of type `t` (keyed by its member
/// `field`) a create makes under `key`, from the pool (see the module's doc): those a
/// pool record (the same `key` takes the same one) holds, with the pool record's own id
/// written as `key` where a value names it as a path or a prefix (see [`rewrite_id`]).
/// `None` when the pool holds no record of `t`, or none of `names`.
pub(super) fn create_fill(
    rt: &MockRuntime,
    (t, field): (&str, &str),
    key: &str,
    names: &[String],
) -> Option<serde_json::Map<String, Value>> {
    if !rt.consistency || names.is_empty() {
        return None;
    }
    let records = rt.pool.records(t, &digest(rt, t));
    if records.is_empty() {
        return None;
    }
    let pick = Sha256::digest(key.as_bytes());
    let record = records.get(usize::from(pick[0]) % records.len())?;
    let own_id = mb_mcm::member_at(record, field).and_then(id_text);
    let mut out = serde_json::Map::new();
    for name in names {
        if let Some(value) = record.get(name) {
            let mut value = value.clone();
            if let Some(own) = own_id.as_deref() {
                rewrite_id(&mut value, own, key);
            }
            out.insert(name.clone(), value);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Writes `to` for `from` in every string of `value` where `from` names the record:
/// the whole string, or an occurrence that starts the string or follows a `/` or `=`
/// (a path, a query value) and is not followed by a letter or digit (`pi_1` in
/// `pi_1_secret_x` and `/widgets/pi_1`, never in `/widgets/pi_12`, nor `123` in
/// `555-123-4567`).
fn rewrite_id(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(s) if !from.is_empty() && s.contains(from) => *s = rewritten(s, from, to),
        Value::Array(items) => items.iter_mut().for_each(|v| rewrite_id(v, from, to)),
        Value::Object(members) => members.values_mut().for_each(|v| rewrite_id(v, from, to)),
        _ => {}
    }
}

fn rewritten(s: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut before: Option<char> = None;
    while let Some(at) = rest.find(from) {
        let prev = rest[..at].chars().last().or(before);
        let next = rest[at + from.len()..].chars().next();
        let names_it =
            prev.is_none_or(|c| c == '/' || c == '=') && next.is_none_or(|c| !c.is_alphanumeric());
        out.push_str(&rest[..at]);
        out.push_str(if names_it { to } else { from });
        before = from.chars().last();
        rest = &rest[at + from.len()..];
    }
    out.push_str(rest);
    out
}

/// Whether `op` names its record by its key alone: an HTTP route with exactly one path
/// parameter (one after an owner's would not tell owners apart), or a GraphQL field.
fn keyed_alone(op: &Operation) -> bool {
    match &op.binding {
        Binding::Http { path, .. } => path.matches('{').count() == 1,
        Binding::Graphql { .. } => true,
        _ => false,
    }
}

/// A read by id of a record the session does not hold, from the pool (see the module's
/// doc): the record stored in the session under `id`.
pub(super) async fn read(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Option<StoredResource> {
    let rt = state.rt;
    let op = plan.op;
    if !rt.consistency || !keyed_alone(op) {
        return None;
    }
    let t = op.resource_type.as_deref()?;
    let id_member = crate::call::exprs::identity_member(rt, t);
    if op.identity.as_ref()?.field != id_member {
        return None;
    }
    let id = crate::call::named_target(rt, op, call)?;
    let records = rt.pool.records(t, &digest(rt, t));
    if records.is_empty() {
        return None;
    }
    // The same id takes the same record in every session.
    let pick = Sha256::digest(id.as_bytes());
    let at = usize::from(pick[0]) % records.len();
    let mut data = records.into_iter().nth(at)?;
    // The record under the asked id, as its type writes ids; an id it cannot take (a
    // word where the type's ids are numbers) is not the pool's to answer.
    let typed = match data.get(&id_member) {
        Some(Value::Number(_)) => Value::from(id.parse::<i64>().ok()?),
        _ => Value::from(id.clone()),
    };
    data.as_object_mut()?.insert(id_member, typed);
    // Still a record of the type as every read of it shows one (the id is its own now).
    if let Some(why) = super::create::unreadable(rt, t, &data, None) {
        tracing::debug!(mock = %rt.name, r#type = %t, %why, "a pool record under the asked id does not read as its type");
        return None;
    }
    let stored = StoredResource {
        resource_type: t.to_string(),
        id,
        data,
    };
    if !state.insert_seeded(&plan.session, stored.clone()).await {
        return None;
    }
    // As a generated read's own answer is.
    rt.own_reads().remember(&plan.session, &stored);
    plan.exprs.pooled(1);
    Some(stored)
}

/// Whether a list of type `t` answers from the pool's records (see the module's doc):
/// the session took them before, or never wrote the type and takes them now, storing
/// them as seeded records. Once it has, every list of `t` in the session that names no
/// scope answers its records, as one list of the type would.
pub(super) async fn list(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    t: &str,
    scoped: bool,
) -> bool {
    let rt = state.rt;
    if !rt.consistency || scoped || !call.path_params.is_empty() {
        return false;
    }
    if state.get(&plan.session, SERVED_TYPE, t).await.is_some() {
        return true;
    }
    let records = rt.pool.records(t, &digest(rt, t));
    if records.is_empty()
        || !state.list(&plan.session, t, 1).await.is_empty()
        || !crate::call::state_lists::created_order(state, &plan.session, t)
            .await
            .is_empty()
        || crate::call::state_lists::deleted_some(state, &plan.session, t).await
    {
        return false;
    }
    let id_member = crate::call::exprs::identity_member(rt, t);
    let resources: Vec<StoredResource> = records
        .into_iter()
        .filter_map(|data| {
            let id = id_text(data.get(&id_member)?)?;
            Some(StoredResource {
                resource_type: t.to_string(),
                id,
                data,
            })
        })
        .collect();
    let stored = state
        .insert_seeded_all(&plan.session, resources)
        .await
        .into_iter()
        .filter(|s| *s)
        .count();
    if stored == 0 {
        return false;
    }
    plan.exprs.pooled(stored);
    let served = StoredResource {
        resource_type: SERVED_TYPE.to_string(),
        id: t.to_string(),
        data: serde_json::json!({}),
    };
    state.upsert(&plan.session, served).await;
    true
}

#[cfg(test)]
mod tests {
    use super::{RecordPool, rewritten};

    #[test]
    fn an_id_is_rewritten_only_where_it_names_the_record() {
        assert_eq!(rewritten("w1", "w1", "new"), "new");
        assert_eq!(
            rewritten("https://x.test/widgets/w1", "w1", "new"),
            "https://x.test/widgets/new"
        );
        assert_eq!(
            rewritten("/widgets/w1/parts?id=w1", "w1", "new"),
            "/widgets/new/parts?id=new"
        );
        assert_eq!(rewritten("pi_1_secret_x", "pi_1", "pi_9"), "pi_9_secret_x");
        assert_eq!(rewritten("/widgets/w12", "w1", "new"), "/widgets/w12");
        assert_eq!(rewritten("+1 555-123-4567", "123", "77"), "+1 555-123-4567");
        assert_eq!(rewritten("Order #1010", "101", "7"), "Order #1010");
        assert_eq!(
            rewritten("A red widget, discovered", "red", "blue"),
            "A red widget, discovered"
        );
        assert_eq!(rewritten("/items/7", "7", "42"), "/items/42");
    }
    use serde_json::json;

    #[test]
    fn a_fill_begun_before_the_latest_adds_and_charges_nothing() {
        let pool = RecordPool::default();
        let old = pool.begin();
        pool.charge(old, 100);
        let latest = pool.begin();
        assert_eq!(pool.spent(), 0, "each fill counts its own spend");
        pool.add(old, "widgets", "d", vec![json!({"id": "w1"})]);
        pool.charge(old, 50);
        assert!(pool.sizes().is_empty());
        assert_eq!(pool.spent(), 0);
        pool.add(latest, "widgets", "d", vec![json!({"id": "w2"})]);
        pool.charge(latest, 70);
        assert_eq!(pool.sizes().get("widgets"), Some(&1));
        assert_eq!(pool.spent(), 70);
        assert!(!pool.current(old) && pool.current(latest));
    }
}
