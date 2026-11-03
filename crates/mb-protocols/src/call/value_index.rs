//! The records a request refers to by a value no member rule resolves, found through an
//! index of what this runtime stored.
//!
//! [`references_in`] follows `<x>Id` members, global ids and members named after a type,
//! and [`world::named_by_value`] looks a whole id-like value up as a stored id. A request
//! can still refer to a record otherwise: by an id inside a URI (Spotify's
//! `spotify:track:<id>`), by a short id in an object named after its type (Jira's
//! `transition: {id: "21"}`), or by a member value only that record holds (a ticket
//! created with its requester's `email`). Each record this runtime stores is indexed, per
//! session, by its identity and by the plain string members its read declares. A
//! request's values, at any depth of its body, query or path, are matched against it:
//!
//! - a whole value of an id's shape (see [`id_like`]) matches the one record it is the
//!   identity of;
//! - failing that, a whole value matches the one record holding it in a member of the
//!   same name as the request's member, by stem or by whole words (`requester_email` names
//!   `email`; `zone` names no `timezone`). Only a value that identifies what holds it
//!   counts ([`identifies`]): a member of `format` `email`, `uuid`, `uri` or `hostname`,
//!   or a value holding an `@`, or of eight or more with letters and digits both, never a
//!   date or a time; a time zone or a status word is no reference, however unique.
//!   Records of the type the operation creates or targets are its siblings and left out;
//!   a value an id lookup already resolved, and a member that is a reference (`<x>_id`, or
//!   named after a type), never take this path;
//! - a delimited token of a longer string (split on what is not alphanumeric) matches
//!   only a record whose identity it is, exactly;
//! - a short value under an identity member of an object matches only a record whose
//!   identity it is and whose type that object's member names ([`entity_type`]).
//!
//! A match must be unique: a value two live records hold names neither. Trivial values
//! are never indexed or matched: what is too short, words, booleans, numbers too small to
//! be an id, enum and `const` values and dates the schema declares, and what a
//! credential's member holds or a credential looks like. A hit is scrubbed for the prompt
//! ([`write_memory::matched_for_context`]) and joins the related resources as context,
//! within the bounds [`world::named_by_value`] keeps. It is not one of the records a
//! create marks as referenced ([`world::referenced_by_value`]), and an answer's body is
//! never stored on it.
//!
//! The index is a hint of this runtime's writes, as [`PathAliases`] is: records the
//! Admin API seeds or upserts are not in it, and do not match through it. A delete the
//! store's change feed reports, made anywhere, drops its record at once
//! ([`ValueIndex::follow`]). Resets, mock resets, seeds and expiry publish no change,
//! and the runtime does not know the store's session TTL, so every holder of a value is
//! also read back before uniqueness is decided: a record the store says is gone or
//! changed is forgotten, and neither matches nor makes a value ambiguous. A read the
//! store failed, or the breaker held off, forgets nothing and decides nothing. A record
//! the request's other lookups already showed costs no read. Each session keeps at most
//! [`MAX_SESSION_ENTRIES`] entries, the oldest records going first, and the least
//! recently written of more than [`MAX_SESSIONS`] sessions is dropped whole.
//!
//! An identity is unique by construction, so matching identities is safe whatever the
//! store. A member value's uniqueness is known only where this process sees every write:
//! with a store other nodes write to (SQL, Redis, a bus to other nodes; see
//! [`mb_state::StateStore::local_only`]), members are not indexed and only identities
//! match. A single node's store, the in-memory one behind a local bus, is local only.

use std::collections::BTreeMap;
use std::sync::Arc;

use mb_state::INTERNAL_TYPE;

use super::*;

/// Shortest value matched as a whole or as a token, in bytes; a number needs as many
/// digits.
const MIN_LEN: usize = 6;
/// Smallest whole number matched as a whole.
const MIN_NUMBER: u64 = 100_000;
/// Shortest id matched under an identity member of an object named after its type.
const MIN_SHORT_LEN: usize = 2;
/// Longest value indexed or matched, in bytes.
const MAX_LEN: usize = 128;
/// A token with no digit must be at least this long to be an id rather than a word.
const MIN_WORD_TOKEN_LEN: usize = 16;
/// Members of one record indexed, at most, besides its identity.
const MAX_MEMBERS: usize = 16;
/// Entries one session keeps at most; the oldest records go first.
const MAX_SESSION_ENTRIES: usize = 2048;
/// Sessions the index keeps at most; the least recently written goes first.
const MAX_SESSIONS: usize = 64;
/// Shortest member value matched by its shape alone, with letters and digits both.
const MIN_SHAPED_LEN: usize = 8;
/// The `format`s whose values identify what holds them.
const IDENTIFYING_FORMATS: [&str; 4] = ["email", "uuid", "uri", "hostname"];
/// Values of one request matched, at most.
const MAX_CANDIDATES: usize = 16;
/// How deep a request is walked.
const MAX_DEPTH: usize = 8;
/// Holders of one value read back, at most: more is ambiguous without reading.
const MAX_HOLDERS: usize = 4;
/// Store reads one request pays to read holders back.
const MAX_READS: usize = 8;

/// Who holds an indexed value: a record, by its identity (`member` `None`) or a member.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Holder {
    resource_type: String,
    id: String,
    member: Option<String>,
}

impl Holder {
    fn is(&self, t: &str, id: &str) -> bool {
        self.resource_type == t && self.id == id
    }
}

/// A record as one session's index holds it: when it was indexed, and under what.
#[derive(Debug)]
struct Record {
    at: u64,
    values: Vec<String>,
}

/// One session's index.
#[derive(Debug, Default)]
struct SessionEntries {
    /// Per value, who holds it.
    by_value: HashMap<String, Vec<Holder>>,
    /// Per record (type, id), what it is indexed under.
    by_record: HashMap<(String, String), Record>,
    /// The records by when they were indexed, oldest first.
    order: BTreeMap<u64, (String, String)>,
    /// Entries held: holders, over every value.
    len: usize,
    /// When the session was last written.
    used: u64,
}

impl SessionEntries {
    fn forget(&mut self, t: &str, id: &str) {
        let key = (t.to_string(), id.to_string());
        let Some(record) = self.by_record.remove(&key) else {
            return;
        };
        self.order.remove(&record.at);
        for value in record.values {
            if let Some(holders) = self.by_value.get_mut(&value) {
                let before = holders.len();
                holders.retain(|h| !h.is(t, id));
                self.len = self.len.saturating_sub(before - holders.len());
                if holders.is_empty() {
                    self.by_value.remove(&value);
                }
            }
        }
    }

    /// Forgets the oldest records until `room` more entries fit.
    fn make_room(&mut self, room: usize) {
        while self.len + room > MAX_SESSION_ENTRIES {
            let Some((_, (t, id))) = self.order.pop_first() else {
                return;
            };
            self.forget(&t, &id);
        }
    }
}

#[derive(Debug, Default)]
struct Entries {
    sessions: HashMap<SessionKey, SessionEntries>,
    clock: u64,
}

/// The identities and member values of the records this runtime stored, per session
/// (see the module's doc).
#[derive(Debug, Default)]
pub(crate) struct ValueIndex(std::sync::Mutex<Entries>);

/// The task following the store's feed for a [`ValueIndex`] (see [`ValueIndex::follow`]),
/// stopped when dropped.
#[derive(Debug)]
pub(crate) struct ValueFeed(tokio::task::JoinHandle<()>);

impl Drop for ValueFeed {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ValueIndex {
    /// Indexes `record` as it is now stored, in place of what it was indexed under.
    #[cfg(test)]
    fn remember(&self, rt: &MockRuntime, session: &SessionKey, record: &StoredResource) {
        self.remember_indexed(session, Indexed::of(rt, record));
    }

    /// Indexes a record now stored under what [`Indexed::of`] took from it, in place of
    /// what it was indexed under.
    pub(super) fn remember_indexed(&self, session: &SessionKey, indexed: Option<Indexed>) {
        let Some(Indexed {
            resource_type,
            id,
            values,
        }) = indexed
        else {
            return;
        };
        let Ok(mut entries) = self.0.lock() else {
            return;
        };
        entries.clock += 1;
        let at = entries.clock;
        if !entries.sessions.contains_key(session) && entries.sessions.len() >= MAX_SESSIONS {
            let oldest = entries
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.used)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                entries.sessions.remove(&oldest);
            }
        }
        let held = entries.sessions.entry(session.clone()).or_default();
        held.used = at;
        held.forget(&resource_type, &id);
        if values.is_empty() {
            return;
        }
        held.make_room(values.len());
        let mut kept = Vec::with_capacity(values.len());
        for (member, value) in values {
            let holder = Holder {
                resource_type: resource_type.clone(),
                id: id.clone(),
                member,
            };
            let holders = held.by_value.entry(value.clone()).or_default();
            if !holders.contains(&holder) {
                holders.push(holder);
                held.len += 1;
            }
            if !kept.contains(&value) {
                kept.push(value);
            }
        }
        let key = (resource_type, id);
        held.order.insert(at, key.clone());
        held.by_record.insert(key, Record { at, values: kept });
    }

    /// Forgets record `id` of type `t` of `session`: deleted, or found gone or changed.
    pub(super) fn forget(&self, session: &SessionKey, t: &str, id: &str) {
        if let Ok(mut entries) = self.0.lock()
            && let Some(held) = entries.sessions.get_mut(session)
        {
            held.forget(t, id);
        }
    }

    /// Forgets everything: what the index missed of the store's feed could be anything.
    fn clear(&self) {
        if let Ok(mut entries) = self.0.lock() {
            entries.sessions.clear();
        }
    }

    /// Follows `state`'s change feed on its own task, so that a delete the store reports,
    /// made anywhere (the Admin API, another node), drops its record at once, holders a
    /// request would not read back included. Resets and seeds publish no change, so their
    /// records go when a read back finds them gone. A feed that lagged clears the index.
    /// `None` outside a Tokio runtime, or with no feed to follow.
    pub(crate) fn follow(index: &Arc<Self>, state: &dyn mb_state::StateStore) -> Option<ValueFeed> {
        use tokio::sync::broadcast::error::RecvError;
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let mut changes = state.subscribe();
        let index = Arc::downgrade(index);
        let task = handle.spawn(async move {
            loop {
                let change = changes.recv().await;
                let Some(index) = index.upgrade() else {
                    return;
                };
                match change {
                    Ok(change) if change.kind == mb_state::ChangeKind::Deleted => {
                        index.forget(&change.session, &change.resource_type, &change.id);
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => index.clear(),
                    Err(RecvError::Closed) => return,
                }
            }
        });
        Some(ValueFeed(task))
    }

    fn holders(&self, session: &SessionKey, value: &str) -> Vec<Holder> {
        self.0
            .lock()
            .ok()
            .and_then(|entries| entries.sessions.get(session)?.by_value.get(value).cloned())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn len(&self, session: &SessionKey) -> usize {
        self.0
            .lock()
            .ok()
            .and_then(|e| e.sessions.get(session).map(|s| s.len))
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn sessions(&self) -> usize {
        self.0.lock().map(|e| e.sessions.len()).unwrap_or_default()
    }
}

/// Whether `text` has an id's shape: of an id's length, with no space and something
/// that is not a letter (`jane@example.com`, `PROJ-1042`, not `closed`).
fn id_like(text: &str) -> bool {
    (MIN_LEN..=MAX_LEN).contains(&text.len())
        && !text.chars().any(char::is_whitespace)
        && text.chars().any(|c| !c.is_alphabetic())
}

/// Whether a request member named `asked` may carry what a stored member named `stored`
/// holds: the same stem (`emails`, `email`), or one's whole words naming the other
/// (`requester_email`, `email`).
fn same_name(asked: &str, stored: &str) -> bool {
    !asked.is_empty()
        && (mb_mcm::stem(asked) == mb_mcm::stem(stored)
            || mb_mcm::names_member(asked, stored)
            || mb_mcm::names_member(stored, asked))
}

/// Whether `schema`, a member's, says its value is no reference: one of a declared set
/// (`enum`, `const`), a date or time, not a string, or not to be shown again.
fn trivial_member(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
    schema_parts(bundle, Some(schema)).0.iter().any(|part| {
        let format = part.get("format").and_then(Value::as_str);
        part.get("enum").is_some()
            || part.get("const").is_some()
            || part.get("writeOnly").and_then(Value::as_bool) == Some(true)
            || matches!(format, Some("date" | "date-time" | "time" | "password"))
            || part
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t != "string")
    })
}

/// Whether `text` is shaped like a date or a time: digits and only the separators a date
/// or a time uses (`2026-11-03`, `10:00:00Z`, `20261103T100000Z`, `2026/11/03`). A phone
/// number has the same shape, and is no identifier either.
fn date_or_time_shaped(text: &str) -> bool {
    text.chars().any(|c| c.is_ascii_digit())
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | '/' | '.' | '+' | 'T' | 'Z'))
}

/// Whether a member value identifies what holds it, by its schema or its shape: a member
/// whose `format` is an identifier's ([`IDENTIFYING_FORMATS`]), or a value holding an `@`,
/// or of at least [`MIN_SHAPED_LEN`] with letters and digits both. A time zone
/// (`America/New_York`), a status (`in_progress`), a locale or a media type is none, and
/// a date or a time never is.
fn identifies(bundle: &mb_mcm::Bundle, schema: &Value, text: &str) -> bool {
    if date_or_time_shaped(text) {
        return false;
    }
    let by_format = schema_parts(bundle, Some(schema)).0.iter().any(|part| {
        part.get("format")
            .and_then(Value::as_str)
            .is_some_and(|f| IDENTIFYING_FORMATS.contains(&f))
    });
    let by_shape = text.contains('@')
        || (text.len() >= MIN_SHAPED_LEN
            && text.chars().any(|c| c.is_ascii_digit())
            && text.chars().any(char::is_alphabetic));
    by_format || by_shape
}

/// What a record is indexed under, taken before it is handed to the store.
pub(super) struct Indexed {
    resource_type: String,
    id: String,
    values: Vec<(Option<String>, String)>,
}

impl Indexed {
    /// What `record` is indexed under, with consistency on and for a type of the
    /// contract's (see [`indexed_values`]).
    pub(super) fn of(rt: &MockRuntime, record: &StoredResource) -> Option<Self> {
        (rt.consistency && record.resource_type != INTERNAL_TYPE).then(|| Self {
            resource_type: record.resource_type.clone(),
            id: record.id.clone(),
            values: indexed_values(rt, record),
        })
    }
}

/// What `record` is indexed under: its identity and, with a store only this process
/// writes to, the string members its read's schema declares that have an id's shape,
/// identify what holds them ([`identifies`]), are not trivial ([`trivial_member`]), are
/// no reference to another record and hold no credential.
fn indexed_values(rt: &MockRuntime, record: &StoredResource) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    let id = record.id.as_str();
    if (MIN_SHORT_LEN..=MAX_LEN).contains(&id.len())
        && !id.chars().any(char::is_whitespace)
        && !mb_mcm::credentials::is_credential_value(id)
    {
        out.push((None, id.to_string()));
    }
    if !rt.state.local_only() {
        return out;
    }
    let (Value::Object(members), Some(schema)) = (
        &record.data,
        write_memory::read_schema(rt, &record.resource_type),
    ) else {
        return out;
    };
    let bundle = rt.bundle();
    let parts = schema_parts(bundle, Some(schema)).0;
    let mut kept = 0;
    for (name, value) in members {
        if kept >= MAX_MEMBERS {
            break;
        }
        let Value::String(text) = value else {
            continue;
        };
        if text == id
            || !id_like(text)
            || reference_prefix(name).is_some()
            || entity_type(rt.index(), None, name).is_some()
            || write_memory::is_credential_name(rt, name)
            || mb_mcm::credentials::hides_in_context(name)
            || mb_mcm::credentials::is_credential_value(text)
        {
            continue;
        }
        let declared = parts
            .iter()
            .find_map(|part| part.get("properties")?.get(name.as_str()));
        if declared.is_none_or(|member| {
            trivial_member(bundle, member) || !identifies(bundle, member, text)
        }) {
            continue;
        }
        out.push((Some(name.clone()), text.clone()));
        kept += 1;
    }
    out
}

/// A request value to match, and how it may match (see the module's doc).
#[derive(Debug, PartialEq)]
enum Candidate {
    /// A whole value, under request member `member`.
    Whole {
        value: String,
        member: String,
    },
    Token(String),
    /// A short value under `member` of an object found under `parent`.
    Short {
        value: String,
        member: String,
        parent: String,
    },
}

/// Adds the candidates `value`, under member `name` of an object found under `parent`,
/// holds to `out`: an object's own values before what it nests, so the bound reaches
/// what is nearest first. What a credential's member holds, or a credential looks like,
/// is not matched.
fn candidates(
    rt: &MockRuntime,
    (parent, name): (&str, &str),
    value: &Value,
    depth: usize,
    out: &mut Vec<Candidate>,
) {
    if depth > MAX_DEPTH
        || out.len() >= MAX_CANDIDATES
        || write_memory::is_credential_name(rt, name)
    {
        return;
    }
    let mut push = |candidate: Candidate| {
        if out.len() < MAX_CANDIDATES && !out.contains(&candidate) {
            out.push(candidate);
        }
    };
    let short = |text: &str| Candidate::Short {
        value: text.to_string(),
        member: name.to_string(),
        parent: parent.to_string(),
    };
    match value {
        Value::Object(members) => {
            let (nested, scalars): (Vec<_>, Vec<_>) = members
                .iter()
                .partition(|(_, inner)| inner.is_object() || inner.is_array());
            for (key, inner) in scalars.into_iter().chain(nested) {
                candidates(rt, (name, key), inner, depth + 1, out);
            }
        }
        Value::Array(items) => {
            let (nested, scalars): (Vec<_>, Vec<_>) = items
                .iter()
                .partition(|inner| inner.is_object() || inner.is_array());
            for item in scalars.into_iter().chain(nested) {
                candidates(rt, (parent, name), item, depth + 1, out);
            }
        }
        Value::String(text) => {
            if mb_mcm::credentials::is_credential_value(text) {
                return;
            }
            if id_like(text) {
                push(Candidate::Whole {
                    value: text.clone(),
                    member: name.to_string(),
                });
            } else if !parent.is_empty()
                && (MIN_SHORT_LEN..MIN_LEN).contains(&text.len())
                && text.chars().all(char::is_alphanumeric)
            {
                push(short(text));
            }
            if text.chars().any(|c| !c.is_alphanumeric()) {
                for token in text.split(|c: char| !c.is_alphanumeric()) {
                    let qualifies = (MIN_LEN..=MAX_LEN).contains(&token.len())
                        && (token.chars().any(|c| c.is_ascii_digit())
                            || token.len() >= MIN_WORD_TOKEN_LEN);
                    if qualifies && token != text {
                        push(Candidate::Token(token.to_string()));
                    }
                }
            }
        }
        Value::Number(number) => {
            if let Some(n) = number.as_u64() {
                let text = n.to_string();
                if n >= MIN_NUMBER {
                    push(Candidate::Whole {
                        value: text,
                        member: name.to_string(),
                    });
                } else if !parent.is_empty() && text.len() >= MIN_SHORT_LEN {
                    push(short(&text));
                }
            }
        }
        _ => {}
    }
}

/// What a match is decided against: the operation's own type, whose records are
/// siblings of what it creates or targets, and whether an id lookup already resolved a
/// value.
struct Context<'a, R: Fn(&str) -> bool> {
    own_type: Option<&'a str>,
    resolved: R,
}

/// The holders `candidate` may name, before they are read back (see the module's doc).
fn eligible<R: Fn(&str) -> bool>(
    rt: &MockRuntime,
    session: &SessionKey,
    candidate: &Candidate,
    cx: &Context<'_, R>,
) -> (Vec<Holder>, String) {
    let index = rt.value_index();
    let identities = |holders: &[Holder]| -> Vec<Holder> {
        holders
            .iter()
            .filter(|h| h.member.is_none())
            .cloned()
            .collect()
    };
    match candidate {
        Candidate::Whole { value, member } => {
            let holders = index.holders(session, value);
            let named = identities(&holders);
            if !named.is_empty() || (cx.resolved)(value) {
                return (named, value.clone());
            }
            let held: Vec<Holder> = holders
                .into_iter()
                .filter(|h| {
                    h.member.as_deref().is_some_and(|m| same_name(member, m))
                        && cx.own_type != Some(h.resource_type.as_str())
                })
                .collect();
            (held, value.clone())
        }
        Candidate::Token(value) => (identities(&index.holders(session, value)), value.clone()),
        Candidate::Short {
            value,
            member,
            parent,
        } => {
            let Some(t) = entity_type(rt.index(), None, parent) else {
                return (Vec::new(), value.clone());
            };
            let fields = rt.index().identity_fields(&t);
            let names_identity = if fields.is_empty() {
                mb_mcm::is_generic_id(member)
            } else {
                fields.iter().any(|f| f == member)
            };
            if !names_identity {
                return (Vec::new(), value.clone());
            }
            let of_type = identities(&index.holders(session, value))
                .into_iter()
                .filter(|h| h.resource_type == t)
                .collect();
            (of_type, value.clone())
        }
    }
}

/// The stored records `call` refers to by value through the index (see the module's
/// doc), at most `room`, other than what `known(type, id)` says is in hand: each once,
/// read back and scrubbed for the prompt. `resolved(value)` says an id lookup already
/// found the record `value` is the id of. Context only.
pub(super) async fn matched(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    room: usize,
    known: impl Fn(&str, &str) -> bool,
    resolved: impl Fn(&str) -> bool,
) -> Vec<StoredResource> {
    let rt = state.rt;
    if room == 0 || !write_memory::applies(rt, plan) {
        return Vec::new();
    }
    let mut values = Vec::new();
    for (name, value) in &call.path_params {
        candidates(rt, ("", name), value, 1, &mut values);
    }
    for (name, value) in &call.query {
        candidates(rt, ("", name), value, 1, &mut values);
    }
    if let Some(body) = &call.body {
        candidates(rt, ("", ""), body, 0, &mut values);
    }
    let cx = Context {
        own_type: plan.op.resource_type.as_deref(),
        resolved,
    };
    let session = &plan.session;
    let mut reads = 0;
    let mut out: Vec<StoredResource> = Vec::new();
    'values: for candidate in &values {
        if out.len() >= room {
            break;
        }
        let (holders, value) = eligible(rt, session, candidate, &cx);
        // A record in hand already is shown: no read for it, and none for its rivals.
        if holders.is_empty()
            || holders.len() > MAX_HOLDERS
            || holders.iter().any(|h| known(&h.resource_type, &h.id))
        {
            continue;
        }
        // Each holder read back: what is gone or changed counts for nothing.
        let mut live: Vec<StoredResource> = Vec::new();
        for holder in &holders {
            if live.iter().any(|r| holder.is(&r.resource_type, &r.id)) {
                continue;
            }
            if reads >= MAX_READS {
                break 'values;
            }
            reads += 1;
            let hit = state.get(session, &holder.resource_type, &holder.id).await;
            let holds_it = hit.as_ref().is_some_and(|hit| match &holder.member {
                None => hit.id == value,
                Some(member) => {
                    hit.data.get(member).and_then(Value::as_str) == Some(value.as_str())
                }
            });
            match hit {
                Some(hit) if holds_it => live.push(hit),
                // Forgotten only on the store's word: a failed read or a closed breaker
                // says nothing of the record, and nothing more is decided this call.
                _ if !state.available() => break 'values,
                _ => rt
                    .value_index()
                    .forget(session, &holder.resource_type, &holder.id),
            }
        }
        // Two live records, or none: no match.
        if live.len() != 1 {
            continue;
        }
        let Some(hit) = live.pop() else {
            continue;
        };
        if known(&hit.resource_type, &hit.id)
            || out
                .iter()
                .any(|r| r.resource_type == hit.resource_type && r.id == hit.id)
        {
            continue;
        }
        if let Some(data) =
            write_memory::matched_for_context(rt, plan, &hit.resource_type, &hit.data)
        {
            out.push(StoredResource { data, ..hit });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: t, version: "1"}
paths:
  /contacts/{contact_id}:
    get:
      operationId: getContact
      parameters: [{name: contact_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}
  /accounts/{account_id}:
    get:
      operationId: getAccount
      parameters: [{name: account_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}
components:
  schemas:
    Contact:
      type: object
      properties:
        id: {type: string}
        email: {type: string}
        account_id: {type: string}
        account: {type: string}
        state: {type: string, enum: [on-hold]}
"##;

    fn session(name: &str) -> SessionKey {
        SessionKey {
            workspace: "w".into(),
            mock: "m".into(),
            session: name.into(),
        }
    }

    fn runtime() -> MockRuntime {
        let state: std::sync::Arc<dyn mb_state::StateStore> = std::sync::Arc::new(
            mb_state::MemoryStore::new(std::time::Duration::from_secs(3600)),
        );
        MockRuntime::new(
            "m",
            std::sync::Arc::new(mb_import::compile(CONTRACT.as_bytes()).unwrap()),
            std::sync::Arc::new(mb_engine::Engine::new(
                None,
                mb_engine::EngineConfig::default(),
            )),
            state,
        )
    }

    fn walk(value: Value) -> Vec<Candidate> {
        let rt = runtime();
        let mut out = Vec::new();
        candidates(&rt, ("", ""), &value, 0, &mut out);
        out
    }

    fn thing(n: usize) -> StoredResource {
        StoredResource {
            resource_type: "things".into(),
            id: format!("thing-{n:06}"),
            data: serde_json::json!({}),
        }
    }

    #[test]
    fn a_uri_gives_its_id_as_a_token_and_not_its_words() {
        let out = walk(serde_json::json!({"uris": ["spotify:track:4uLU6hMCjMI75M1A2tKUQC"]}));
        assert!(out.contains(&Candidate::Token("4uLU6hMCjMI75M1A2tKUQC".into())));
        assert!(!out.contains(&Candidate::Token("spotify".into())));
        assert!(!out.contains(&Candidate::Token("track".into())));
    }

    #[test]
    fn short_values_count_only_in_a_nested_object() {
        let out = walk(serde_json::json!({"count": "21", "transition": {"id": "21"}, "n": 7}));
        assert_eq!(
            out,
            vec![Candidate::Short {
                value: "21".into(),
                member: "id".into(),
                parent: "transition".into(),
            }]
        );
    }

    /// An object's own values come before what it nests, so the bound reaches them.
    #[test]
    fn values_at_a_level_come_before_nested_ones() {
        let mut nested = serde_json::Map::new();
        for n in 0..MAX_CANDIDATES {
            nested.insert(format!("m{n:02}"), Value::from(format!("deep-{n:04}")));
        }
        let out = walk(serde_json::json!({"a": nested, "z": "near-0001"}));
        assert_eq!(out.len(), MAX_CANDIDATES);
        assert_eq!(
            out[0],
            Candidate::Whole {
                value: "near-0001".into(),
                member: "z".into(),
            }
        );
    }

    /// What a member value must look like to identify what holds it.
    #[test]
    fn a_member_value_identifies_by_format_or_shape() {
        let rt = runtime();
        let bundle = rt.bundle();
        let plain = serde_json::json!({"type": "string"});
        let uri = serde_json::json!({"type": "string", "format": "uri"});
        for text in ["jane@example.com", "acct4711x", "A1B2C3D4"] {
            assert!(identifies(bundle, &plain, text), "{text}");
        }
        for text in [
            "America/New_York",
            "in_progress",
            "en-US",
            "image/png",
            "2026-11-03",
            "2026-11-03T10:00:00Z",
            "10:00:00",
            "+1-555-0100",
        ] {
            assert!(!identifies(bundle, &plain, text), "{text}");
        }
        assert!(identifies(bundle, &uri, "https://example.com/a"));
        assert!(!identifies(bundle, &uri, "2026-11-03"), "a date never");
    }

    #[test]
    fn names_match_by_stem_or_whole_words() {
        assert!(same_name("email", "email"));
        assert!(same_name("emails", "email"));
        assert!(same_name("requester_email", "email"));
        assert!(same_name("contactEmail", "email"));
        assert!(!same_name("zone", "timezone"));
        assert!(!same_name("contact", "email"));
        assert!(!same_name("", "email"));
    }

    /// References, enum values and undeclared members are not indexed; the member cap
    /// leaves the identity out.
    #[test]
    fn what_a_record_is_indexed_under() {
        let rt = runtime();
        let record = StoredResource {
            resource_type: "contacts".into(),
            id: "ct-000101".into(),
            data: serde_json::json!({
                "id": "ct-000101", "email": "jane@example.com", "account_id": "acc-000001",
                "account": "acc-000001", "state": "on-hold", "undeclared": "und-000001",
            }),
        };
        let values = indexed_values(&rt, &record);
        assert_eq!(
            values,
            vec![
                (None, "ct-000101".to_string()),
                (Some("email".to_string()), "jane@example.com".to_string()),
            ]
        );
    }

    #[test]
    fn the_index_is_bounded_per_session_and_forgets() {
        let rt = runtime();
        let index = ValueIndex::default();
        let (a, b) = (session("a"), session("b"));
        index.remember(&rt, &b, &thing(0));
        for n in 0..(MAX_SESSION_ENTRIES + 10) {
            index.remember(&rt, &a, &thing(n));
        }
        assert_eq!(index.len(&a), MAX_SESSION_ENTRIES);
        // The oldest went, the newest stay, and the other session is untouched.
        assert!(index.holders(&a, &thing(0).id).is_empty());
        let newest = thing(MAX_SESSION_ENTRIES + 9).id;
        assert_eq!(index.holders(&a, &newest).len(), 1);
        assert_eq!(index.holders(&b, &thing(0).id).len(), 1);
        index.forget(&a, "things", &newest);
        assert!(index.holders(&a, &newest).is_empty());
    }

    #[test]
    fn the_least_recently_written_session_goes_first() {
        let rt = runtime();
        let index = ValueIndex::default();
        for n in 0..=MAX_SESSIONS {
            index.remember(&rt, &session(&format!("s{n}")), &thing(n));
        }
        assert_eq!(index.sessions(), MAX_SESSIONS);
        assert!(index.holders(&session("s0"), &thing(0).id).is_empty());
        let last = session(&format!("s{MAX_SESSIONS}"));
        assert_eq!(index.holders(&last, &thing(MAX_SESSIONS).id).len(), 1);
    }
}
