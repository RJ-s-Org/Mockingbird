//! The records a request refers to by a value no member rule resolves, found through an
//! index of what this runtime stored.
//!
//! [`references_in`] follows `<x>Id` members, global ids and members named after a type,
//! and [`world::named_by_value`] looks a whole id-like value up as a stored id. A request
//! can still refer to a record otherwise: by an id inside a URI (Spotify's
//! `spotify:track:<id>`), by a short id in an object named after its type (Jira's
//! `transition: {id: "21"}`), or by a member value only that record holds (a ticket
//! created with its requester's `email`). Each record this runtime stores is indexed by
//! its identity and by the plain string members its read declares. A request's values,
//! at any depth of its body, query or path, are matched against that index:
//!
//! - a whole value of an id's shape (see [`id_like`]) matches the one record it is the
//!   identity of, else the one record holding it in a member;
//! - a delimited token of a longer string (split on what is not alphanumeric) matches
//!   only a record whose identity it is, exactly;
//! - a short value under an identity member of an object matches only a record whose
//!   identity it is and whose type that object's member names ([`entity_type`]).
//!
//! A match must be unique: a value two records hold, as an identity or as a member,
//! names neither. Trivial values are never indexed or matched: what is too short, words,
//! booleans, numbers too small to be an id, enum and `const` values and dates the schema
//! declares, and what a credential's member holds or a credential looks like. A hit is
//! read once to check it still holds the value, scrubbed for the prompt
//! ([`write_memory::matched_for_context`]), and joins the related resources as context,
//! within the bounds [`world::named_by_value`] keeps. It is not one of the records a
//! create marks as referenced ([`world::referenced_by_value`]), and an answer's body is
//! never stored on it.
//!
//! Like [`PathAliases`], the index is only a hint of this runtime's writes: what another
//! node, the Admin API or a restart stored is not in it, and a stale entry costs at most
//! the read that finds the record changed or gone.

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
/// Members of one record indexed, at most.
const MAX_MEMBERS: usize = 16;
/// Entries the index holds at most; it is emptied when full.
const MAX_ENTRIES: usize = 16_384;
/// Values of one request matched, at most.
const MAX_CANDIDATES: usize = 16;
/// How deep a request is walked.
const MAX_DEPTH: usize = 8;
/// Store reads one request pays to check its matches.
const MAX_READS: usize = 8;

/// Who holds an indexed value: a record, by its identity (`member` `None`) or a member.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Holder {
    resource_type: String,
    id: String,
    member: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RecordKey {
    session: SessionKey,
    resource_type: String,
    id: String,
}

#[derive(Debug, Default)]
struct Entries {
    /// Per session and value, who holds it.
    by_value: HashMap<(SessionKey, String), Vec<Holder>>,
    /// Per record, the values it is indexed under.
    by_record: HashMap<RecordKey, Vec<String>>,
    len: usize,
}

impl Entries {
    fn forget(&mut self, key: &RecordKey) {
        let Some(values) = self.by_record.remove(key) else {
            return;
        };
        for value in values {
            let slot = (key.session.clone(), value);
            if let Some(holders) = self.by_value.get_mut(&slot) {
                let before = holders.len();
                holders.retain(|h| h.resource_type != key.resource_type || h.id != key.id);
                self.len = self.len.saturating_sub(before - holders.len());
                if holders.is_empty() {
                    self.by_value.remove(&slot);
                }
            }
        }
    }
}

/// The identities and unique-candidate member values of the records this runtime stored,
/// per session (see the module's doc).
#[derive(Debug, Default)]
pub(crate) struct ValueIndex(std::sync::Mutex<Entries>);

impl ValueIndex {
    /// Indexes `record` as it is now stored, in place of what it was indexed under.
    #[cfg(test)]
    fn remember(&self, rt: &MockRuntime, session: &SessionKey, record: &StoredResource) {
        let values = Indexed::of(rt, record);
        self.remember_indexed(session, values);
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
        let key = RecordKey {
            session: session.clone(),
            resource_type,
            id,
        };
        entries.forget(&key);
        if values.is_empty() {
            return;
        }
        if entries.len + values.len() > MAX_ENTRIES {
            *entries = Entries::default();
        }
        let mut kept = Vec::with_capacity(values.len());
        for (member, value) in values {
            let holder = Holder {
                resource_type: key.resource_type.clone(),
                id: key.id.clone(),
                member,
            };
            let holders = entries
                .by_value
                .entry((session.clone(), value.clone()))
                .or_default();
            if !holders.contains(&holder) {
                holders.push(holder);
                entries.len += 1;
            }
            if !kept.contains(&value) {
                kept.push(value);
            }
        }
        entries.by_record.insert(key, kept);
    }

    /// Forgets record `id` of type `t`, deleted.
    pub(super) fn forget(&self, session: &SessionKey, t: &str, id: &str) {
        if let Ok(mut entries) = self.0.lock() {
            entries.forget(&RecordKey {
                session: session.clone(),
                resource_type: t.to_string(),
                id: id.to_string(),
            });
        }
    }

    fn holders(&self, session: &SessionKey, value: &str) -> Vec<Holder> {
        self.0
            .lock()
            .ok()
            .and_then(|entries| {
                entries
                    .by_value
                    .get(&(session.clone(), value.to_string()))
                    .cloned()
            })
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().map(|e| e.len).unwrap_or_default()
    }
}

/// Whether `text` has an id's shape: of an id's length, with no space and something
/// that is not a letter (`jane@example.com`, `PROJ-1042`, not `closed`).
fn id_like(text: &str) -> bool {
    (MIN_LEN..=MAX_LEN).contains(&text.len())
        && !text.chars().any(char::is_whitespace)
        && text.chars().any(|c| !c.is_alphabetic())
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

/// What `record` is indexed under: its identity, and the string members its read's
/// schema declares that have an id's shape, are not trivial ([`trivial_member`]) and
/// hold no credential.
fn indexed_values(rt: &MockRuntime, record: &StoredResource) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    let id = record.id.as_str();
    if (MIN_SHORT_LEN..=MAX_LEN).contains(&id.len())
        && !id.chars().any(char::is_whitespace)
        && !mb_mcm::credentials::is_credential_value(id)
    {
        out.push((None, id.to_string()));
    }
    let (Value::Object(members), Some(schema)) = (
        &record.data,
        write_memory::read_schema(rt, &record.resource_type),
    ) else {
        return out;
    };
    let bundle = rt.bundle();
    let parts = schema_parts(bundle, Some(schema)).0;
    for (name, value) in members {
        if out.len() > MAX_MEMBERS {
            break;
        }
        let Value::String(text) = value else {
            continue;
        };
        if text == id
            || !id_like(text)
            || write_memory::is_credential_name(rt, name)
            || mb_mcm::credentials::hides_in_context(name)
            || mb_mcm::credentials::is_credential_value(text)
        {
            continue;
        }
        let declared = parts
            .iter()
            .find_map(|part| part.get("properties")?.get(name.as_str()));
        if declared.is_none_or(|member| trivial_member(bundle, member)) {
            continue;
        }
        out.push((Some(name.clone()), text.clone()));
    }
    out
}

/// A request value to match, and how it may match (see the module's doc).
#[derive(Debug, PartialEq)]
enum Candidate {
    Whole(String),
    Token(String),
    /// A short value under `member` of an object found under `parent`.
    Short {
        value: String,
        member: String,
        parent: String,
    },
}

/// Adds the candidates `value`, under member `name` of an object found under `parent`,
/// holds to `out`. What a credential's member holds, or a credential looks like, is not
/// matched.
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
            for (key, inner) in members {
                candidates(rt, (name, key), inner, depth + 1, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                candidates(rt, (parent, name), item, depth + 1, out);
            }
        }
        Value::String(text) => {
            if mb_mcm::credentials::is_credential_value(text) {
                return;
            }
            if id_like(text) {
                push(Candidate::Whole(text.clone()));
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
                    push(Candidate::Whole(text));
                } else if !parent.is_empty() && text.len() >= MIN_SHORT_LEN {
                    push(short(&text));
                }
            }
        }
        _ => {}
    }
}

/// The one record `candidate` names among what the index holds, if just one.
fn resolve(
    rt: &MockRuntime,
    session: &SessionKey,
    candidate: &Candidate,
) -> Option<(Holder, String)> {
    let index = rt.value_index();
    let unique = |holders: Vec<Holder>| {
        let first = holders.first()?;
        holders
            .iter()
            .all(|h| h.resource_type == first.resource_type && h.id == first.id)
            .then(|| first.clone())
    };
    let identities = |holders: &[Holder]| -> Vec<Holder> {
        holders
            .iter()
            .filter(|h| h.member.is_none())
            .cloned()
            .collect()
    };
    match candidate {
        Candidate::Whole(value) => {
            let holders = index.holders(session, value);
            let named = identities(&holders);
            // A record's identity is what it is called; others holding it refer to it.
            let found = if named.is_empty() {
                unique(holders)
            } else {
                unique(named)
            }?;
            Some((found, value.clone()))
        }
        Candidate::Token(value) => {
            let found = unique(identities(&index.holders(session, value)))?;
            Some((found, value.clone()))
        }
        Candidate::Short {
            value,
            member,
            parent,
        } => {
            let t = entity_type(rt.index(), None, parent)?;
            let fields = rt.index().identity_fields(&t);
            let names_identity = if fields.is_empty() {
                mb_mcm::is_generic_id(member)
            } else {
                fields.iter().any(|f| f == member)
            };
            if !names_identity {
                return None;
            }
            let of_type: Vec<Holder> = identities(&index.holders(session, value))
                .into_iter()
                .filter(|h| h.resource_type == t)
                .collect();
            Some((unique(of_type)?, value.clone()))
        }
    }
}

/// The stored records `call` refers to by value through the index (see the module's
/// doc), at most `room`, other than what `known(type, id)` says is in hand: each once,
/// checked against the store and scrubbed for the prompt. Context only.
pub(super) async fn matched(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    room: usize,
    known: impl Fn(&str, &str) -> bool,
) -> Vec<StoredResource> {
    let rt = state.rt;
    if room == 0 || !write_memory::applies(rt, plan) {
        return Vec::new();
    }
    let mut found_values = Vec::new();
    for (name, value) in &call.path_params {
        candidates(rt, ("", name), value, 1, &mut found_values);
    }
    for (name, value) in &call.query {
        candidates(rt, ("", name), value, 1, &mut found_values);
    }
    if let Some(body) = &call.body {
        candidates(rt, ("", ""), body, 0, &mut found_values);
    }
    let session = &plan.session;
    let mut reads = 0;
    let mut out: Vec<StoredResource> = Vec::new();
    for candidate in &found_values {
        if out.len() >= room || reads >= MAX_READS {
            break;
        }
        let Some((holder, value)) = resolve(rt, session, candidate) else {
            continue;
        };
        let shown =
            |r: &StoredResource| r.resource_type == holder.resource_type && r.id == holder.id;
        if known(&holder.resource_type, &holder.id) || out.iter().any(shown) {
            continue;
        }
        reads += 1;
        let Some(hit) = state.get(session, &holder.resource_type, &holder.id).await else {
            continue;
        };
        let holds_it = match &holder.member {
            None => hit.id == value,
            Some(member) => hit.data.get(member).and_then(Value::as_str) == Some(value.as_str()),
        };
        if !holds_it {
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

    fn session(name: &str) -> SessionKey {
        SessionKey {
            workspace: "w".into(),
            mock: "m".into(),
            session: name.into(),
        }
    }

    fn runtime() -> MockRuntime {
        let contract = "openapi: 3.0.0\ninfo: {title: t, version: '1'}\npaths: {}\n";
        let state: std::sync::Arc<dyn mb_state::StateStore> = std::sync::Arc::new(
            mb_state::MemoryStore::new(std::time::Duration::from_secs(3600)),
        );
        MockRuntime::new(
            "m",
            std::sync::Arc::new(mb_import::compile(contract.as_bytes()).unwrap()),
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

    #[test]
    fn the_index_is_bounded_and_forgets() {
        let rt = runtime();
        let index = ValueIndex::default();
        let s = session("a");
        for n in 0..(MAX_ENTRIES + 10) {
            let record = StoredResource {
                resource_type: "things".into(),
                id: format!("thing-{n:06}"),
                data: serde_json::json!({}),
            };
            index.remember(&rt, &s, &record);
        }
        assert!(index.len() <= MAX_ENTRIES);
        index.forget(&s, "things", &format!("thing-{:06}", MAX_ENTRIES + 9));
        assert!(
            index
                .holders(&s, &format!("thing-{:06}", MAX_ENTRIES + 9))
                .is_empty()
        );
    }
}
