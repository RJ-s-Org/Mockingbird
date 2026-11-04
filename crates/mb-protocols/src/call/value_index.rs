//! The records a request refers to by a value no member rule resolves, found through an
//! index of what this runtime stored.
//!
//! [`references_in`] follows `<x>Id` members, global ids and members named after a type,
//! and [`world::named_by_value`] looks a whole id-like value up as a stored id. A request
//! can still refer to a record otherwise: by an id inside a URI (Spotify's
//! `spotify:track:<id>`), by a short id in an object named after its type (Jira's
//! `transition: {id: "21"}`), or by a member value only that record holds (a ticket
//! created with its requester's `email`). Each record this runtime stores is indexed, per
//! session, by its identity and by the string members its read declares that identify
//! it ([`Kind::Identifying`]), whatever the store. A
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
//! A create whose operation declares a refusal status is also shown the records of the
//! type it creates that already hold what it asks for ([`same_values`]): a top-level
//! request member's whole string value, held by a stored record of that type under the
//! very same member. These are found by asking the store, not through the index, so
//! every backend finds the same, records another node, the Admin API or a seed stored
//! included. A value that identifies what holds it (as above) matches the records
//! holding it, a few at most. A plain string (a name, a slug), never a reference,
//! matches only the one record holding it, and only when that record agrees with every
//! other value the request sets that it holds, one of them a reference, and when a
//! refusal status the create may be offered names the member in its description
//! ([`mb_engine::guards::a_refusal_names`]): a folder's name in the same parent, not in
//! another, and never a title alone. Any record must agree with the references the body
//! sets, and must be in the collection the create runs in (see [`same_values`]).
//! Trivial values (enums, dates, booleans, short words) never match, and a value two
//! records hold is no plain match.
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
//! store. A member value's uniqueness the index alone knows only where this process
//! sees every write: a single node's store, the in-memory one or a private in-memory
//! SQLite database behind a local bus ([`mb_state::StateStore::local_only`]). With a
//! store other processes may write to (a SQLite file, Postgres, Redis, a bus to other
//! nodes), a member match the index finds also asks the store ([`held_once`]): the
//! session's records of the holder's type, and of each type whose read declares the
//! member, must show no other holder, and the holder's own type must show the hit
//! holding the value, in listings of fewer than [`MAX_SCAN`] (500) records each and
//! within the request's [`MAX_READS`]. A listing that may be cut short (500 records or
//! more of a type), or a type left unread, is no match.
//! Records are keyed by session (workspace, mock and session), on the index and in the
//! store, so neither another tenant's nor another session's records ever match.

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
/// Shortest plain string a create may repeat, in bytes (see [`plain_value`]).
const MIN_PLAIN_LEN: usize = 6;
/// Records [`same_values`] shows, at most.
const MAX_SAME_VALUES: usize = 3;
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
        for value in &record.values {
            if let Some(holders) = self.by_value.get_mut(value) {
                let before = holders.len();
                holders.retain(|h| !h.is(t, id));
                self.len = self.len.saturating_sub(before - holders.len());
                if holders.is_empty() {
                    self.by_value.remove(value);
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
        let mut kept = Vec::new();
        for IndexedValue { member, value } in values {
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

/// One value a record is indexed under: its identity (`member` `None`), or a member's.
#[derive(Debug, PartialEq)]
struct IndexedValue {
    member: Option<String>,
    value: String,
}

/// What a record is indexed under, taken before it is handed to the store.
pub(super) struct Indexed {
    resource_type: String,
    id: String,
    values: Vec<IndexedValue>,
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

/// Whether `text` is a plain string a create may repeat: of [`MIN_PLAIN_LEN`] to
/// [`MAX_LEN`] bytes, with a letter, and not a date or a time.
fn plain_value(text: &str) -> bool {
    (MIN_PLAIN_LEN..=MAX_LEN).contains(&text.len())
        && text.chars().any(char::is_alphabetic)
        && !date_or_time_shaped(text)
}

/// How a string member of a record counts for a match (see [`member_kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A value that identifies what holds it ([`identifies`]), of an id's shape.
    Identifying,
    /// A [`plain_value`], which identifies nothing: it only says what a create would
    /// repeat ([`same_values`]), and is no reference.
    Plain,
}

/// How member `name`, holding `text`, of a record whose read's schema has `parts` counts
/// for a match: not at all when the schema does not declare it, it is trivial
/// ([`trivial_member`]), a reference to another record, or a credential's (by name or
/// value); else [`Kind::Identifying`] or [`Kind::Plain`] by its value, or not at all.
fn member_kind(rt: &MockRuntime, parts: &[&Value], name: &str, text: &str) -> Option<Kind> {
    if reference_prefix(name).is_some()
        || entity_type(rt.index(), None, name).is_some()
        || write_memory::is_credential_name(rt, name)
        || mb_mcm::credentials::hides_in_context(name)
        || mb_mcm::credentials::is_credential_value(text)
    {
        return None;
    }
    let bundle = rt.bundle();
    let member = parts
        .iter()
        .find_map(|part| part.get("properties")?.get(name))
        .filter(|member| !trivial_member(bundle, member))?;
    if id_like(text) && identifies(bundle, member, text) {
        Some(Kind::Identifying)
    } else {
        plain_value(text).then_some(Kind::Plain)
    }
}

/// What `record` is indexed under: its identity and up to [`MAX_MEMBERS`] of its
/// string members that identify it ([`Kind::Identifying`]), other than its identity.
fn indexed_values(rt: &MockRuntime, record: &StoredResource) -> Vec<IndexedValue> {
    let mut out = Vec::new();
    let id = record.id.as_str();
    if (MIN_SHORT_LEN..=MAX_LEN).contains(&id.len())
        && !id.chars().any(char::is_whitespace)
        && !mb_mcm::credentials::is_credential_value(id)
    {
        out.push(IndexedValue {
            member: None,
            value: id.to_string(),
        });
    }
    let (Value::Object(members), Some(schema)) = (
        &record.data,
        write_memory::read_schema(rt, &record.resource_type),
    ) else {
        return out;
    };
    let parts = schema_parts(rt.bundle(), Some(schema)).0;
    let mut kept = 0;
    for (name, value) in members {
        if kept >= MAX_MEMBERS {
            break;
        }
        let Value::String(text) = value else {
            continue;
        };
        if text != id && member_kind(rt, &parts, name, text) == Some(Kind::Identifying) {
            out.push(IndexedValue {
                member: Some(name.clone()),
                value: text.clone(),
            });
            kept += 1;
        }
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
        // A member value on a store others write to: the store must show it unique.
        let member = holders
            .iter()
            .find(|h| h.is(&hit.resource_type, &hit.id))
            .and_then(|h| h.member.as_deref());
        if let Some(member) = member
            && !rt.state.local_only()
        {
            match held_once(state, session, (&hit, member), &value, &mut reads).await {
                Some(true) => {}
                Some(false) => continue,
                None => break 'values,
            }
        }
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

/// Whether `value`, which `hit` holds under `member`, is held by `hit` alone among the
/// records the store shows: of `hit`'s type, whose complete listing must show `hit`
/// holding it, then of each other type whose read declares `member`, in one listing
/// each. `Some(false)` when another holds it, `hit` is not listed holding it, a
/// listing reaches [`MAX_SCAN`] (500) records, so it may be cut short, or the checks
/// would pass [`MAX_READS`]: the value cannot be told unique. A session holding 500 or
/// more records of a type that declares `member` therefore matches nothing by that
/// member on such a store. `None` when the store failed or the breaker held it off,
/// which decides nothing more this call. On a store other processes write to, they may
/// have stored the same value, so what the index found is checked so; on a store only
/// this process writes to, the index sees every write and needs no listing.
async fn held_once(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (hit, member): (&StoredResource, &str),
    value: &str,
    reads: &mut usize,
) -> Option<bool> {
    let rt = state.rt;
    let mut types = vec![hit.resource_type.clone()];
    for t in &rt.index().effect_types {
        let declares = write_memory::read_schema(rt, t).is_some_and(|schema| {
            schema_parts(rt.bundle(), Some(schema))
                .0
                .iter()
                .any(|part| part.get("properties").and_then(|p| p.get(member)).is_some())
        });
        if declares && !types.contains(t) {
            types.push(t.clone());
        }
    }
    for t in types {
        if *reads >= MAX_READS {
            return Some(false);
        }
        *reads += 1;
        let listed = state.list(session, &t, MAX_SCAN).await;
        if !state.available() {
            return None;
        }
        if listed.len() >= MAX_SCAN {
            return Some(false);
        }
        let holds = |r: &StoredResource| r.data.get(member).and_then(Value::as_str) == Some(value);
        let is_hit = |r: &StoredResource| r.resource_type == hit.resource_type && r.id == hit.id;
        if t == hit.resource_type && !listed.iter().any(|r| is_hit(r) && holds(r)) {
            return Some(false);
        }
        if listed.iter().any(|r| !is_hit(r) && holds(r)) {
            return Some(false);
        }
    }
    Some(true)
}

/// Whether `asked`, what a request sets, agrees with `held`, what a stored record holds
/// at the same place: equal scalars (numbers by value), or an object whose members
/// agree with those `held` has, or an array equal to `held`.
fn agrees(asked: &Value, held: &Value) -> bool {
    match (asked, held) {
        (Value::Object(asked), Value::Object(held)) => asked
            .iter()
            .all(|(k, v)| held.get(k).is_none_or(|h| agrees(v, h))),
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => asked == held,
    }
}

/// Whether body member `name`, holding `value`, refers to another record: a reference's
/// name (`org_id`, `orgId`), a type's name, or an object holding an `id` (`parent:
/// {id}`). What a create sets under one places it, so a record it repeats must agree.
fn refers(rt: &MockRuntime, name: &str, value: &Value) -> bool {
    reference_prefix(name).is_some()
        || entity_type(rt.index(), None, name).is_some()
        || value.as_object().is_some_and(|members| {
            members
                .iter()
                .any(|(k, v)| mb_mcm::is_generic_id(k) && (v.is_string() || v.is_number()))
        })
}

/// Whether `hit`, a record holding a value of `body` under member `name`, is what the
/// create would repeat: every reference of the body it holds agrees, and a `plain` value
/// needs more, as it identifies nothing: every other member of the body it holds agrees,
/// and one of them at least is a reference it holds (a folder's `parent`).
fn repeated_by(
    rt: &MockRuntime,
    body: &serde_json::Map<String, Value>,
    name: &str,
    plain: bool,
    hit: &StoredResource,
) -> bool {
    let mut placed = false;
    for (k, v) in body.iter().filter(|(k, _)| *k != name) {
        let Some(held) = hit.data.get(k) else {
            continue;
        };
        let reference = refers(rt, k, v);
        if (reference || plain) && !agrees(v, held) {
            return false;
        }
        placed |= reference;
    }
    !plain || placed
}

/// The stored records of the type `call`, a create, makes that already hold a value of
/// its request (see the module's doc), at most [`MAX_SAME_VALUES`], each with the member
/// it holds it under, other than what `known(type, id)` says is in hand (its target).
/// Only for a create that may be offered a refusal ([`mb_engine::Engine::may_refuse`]),
/// with no forced status or scenario to decide its answer, that carries no idempotency
/// key ([`mb_engine::guards::carries_an_idempotency_key`]), which makes a repeat a retry,
/// and that sets a top-level string that could match: one that identifies what holds it
/// ([`Kind::Identifying`]), or a plain one ([`Kind::Plain`]) under a member a refusal
/// the create may be offered names ([`mb_engine::guards::a_refusal_names`]). Anything
/// else finds nothing and pays nothing.
///
/// First of all, a create whose body asks for its record's id under the identity field
/// ([`mb_engine::guards::asked_identity`], a client-chosen `id`) is shown the record the
/// session holds under that id, under that member, when it is in the collection the
/// create runs in (as below): the one record it can only repeat, read by its key on any
/// store, whatever the shape of its value.
///
/// The store is asked, whatever it is: the session's records of the type, at most
/// [`MAX_SCAN`], newest first, in one listing. A listing the store failed, or the breaker
/// held off, decides nothing. A value more than [`MAX_HOLDERS`] records hold is common;
/// a listing cut at [`MAX_SCAN`] may leave holders out, so it decides only a value it
/// shows one record holding, and never a plain one.
///
/// Every record must agree with the references the body sets ([`repeated_by`]), and be
/// in the collection the create runs in. Where a record is, only this runtime's writes
/// tell ([`Collections`]): when the contract creates the type under a parent path too
/// (`POST /groups/{group}/contacts`), a record counts only where this runtime knows it
/// is, in that very collection for a create under a parent path, in none for a
/// top-level one, so another node's records, a seed's or what was forgotten never count.
/// When it does not, every record of the type is in none, and a top-level create counts
/// any. Each record is returned as stored; what the prompt shows of it is the caller's to
/// scrub.
pub(super) async fn same_values(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    known: impl Fn(&str, &str) -> bool,
) -> Vec<(StoredResource, String)> {
    let rt = state.rt;
    let op = plan.op;
    let (Some(own), Some(Value::Object(body))) = (op.resource_type.as_deref(), &call.body) else {
        return Vec::new();
    };
    if op.kind != OperationKind::Command(Effect::Create)
        || plan.forced_status.is_some()
        || plan.scenario.is_some()
        || !write_memory::applies(rt, plan)
        || !plan.engine.may_refuse(rt.bundle(), op)
        || mb_engine::guards::carries_an_idempotency_key(rt.bundle(), op, call)
    {
        return Vec::new();
    }
    let session = &plan.session;
    let mut out: Vec<(StoredResource, String)> = Vec::new();
    // The id the request asks for, held already in the collection the create runs in:
    // the record a create can only repeat, whatever its shape.
    if let Some(id) = mb_engine::guards::asked_identity(rt.bundle(), op, call)
        && !known(own, &id)
        && let Some(hit) = state.get(session, own, &id).await
        && in_create_collection(rt, session, op, call, &hit)
    {
        let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
        out.push((hit, field.to_string()));
    }
    let Some(schema) = write_memory::read_schema(rt, own) else {
        return out;
    };
    let parts = schema_parts(rt.bundle(), Some(schema)).0;
    let asked: Vec<(&String, &str, Kind)> = body
        .iter()
        .filter_map(|(name, value)| {
            let text = value.as_str()?;
            let kind = member_kind(rt, &parts, name, text)?;
            (kind == Kind::Identifying || mb_engine::guards::a_refusal_names(op, name))
                .then_some((name, text, kind))
        })
        .collect();
    if asked.is_empty() {
        return out;
    }
    let listed = state.list(session, own, MAX_SCAN).await;
    if !state.available() {
        return out;
    }
    let whole = listed.len() < MAX_SCAN;
    let in_collection = |hit: &StoredResource| in_create_collection(rt, session, op, call, hit);
    for (name, text, kind) in asked {
        if out.len() >= MAX_SAME_VALUES {
            break;
        }
        let holders: Vec<&StoredResource> = listed
            .iter()
            .filter(|r| r.id != text && r.data.get(name).and_then(Value::as_str) == Some(text))
            .collect();
        let plain = kind == Kind::Plain;
        let unique = holders.len() == 1;
        // A value this many hold is common, whatever its shape; a plain value two hold
        // is no match; and a listing cut short decides only a value one record holds.
        if holders.is_empty()
            || holders.len() > MAX_HOLDERS
            || (plain && !unique)
            || (!whole && (plain || !unique))
        {
            continue;
        }
        for hit in holders {
            if out.len() >= MAX_SAME_VALUES {
                break;
            }
            if !in_collection(hit)
                || !repeated_by(rt, body, name, plain, hit)
                || known(&hit.resource_type, &hit.id)
                || out
                    .iter()
                    .any(|(r, _)| r.resource_type == hit.resource_type && r.id == hit.id)
            {
                continue;
            }
            out.push((hit.clone(), name.clone()));
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
        name: {type: string}
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

    /// What a create may repeat as a plain value, and when the rest of its request
    /// agrees with the record holding it.
    #[test]
    fn plain_values_and_agreement() {
        for text in ["Quarterly Reports", "my-project", "thumbsup"] {
            assert!(plain_value(text), "{text}");
        }
        for text in ["admin", "2026-11-03", "123456", "+1-555-0100"] {
            assert!(!plain_value(text), "{text}");
        }
        let held = serde_json::json!({"id": "0", "type": "folder", "size": 1.0});
        assert!(agrees(&serde_json::json!({"id": "0"}), &held));
        assert!(agrees(&serde_json::json!({"id": "0", "size": 1}), &held));
        assert!(!agrees(&serde_json::json!({"id": "77"}), &held));
        assert!(!agrees(&serde_json::json!("0"), &held));
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
        let values: Vec<(Option<String>, String)> = indexed_values(&rt, &record)
            .into_iter()
            .map(|v| (v.member, v.value))
            .collect();
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
