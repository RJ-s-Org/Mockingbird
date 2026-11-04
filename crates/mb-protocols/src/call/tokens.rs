//! Optimistic concurrency: a write carrying a stale token is refused before any LLM
//! call, as the API would.
//!
//! An API guarding its records against lost updates has the client send back what it
//! read: a `version` member in the body, an `If-Match` header holding the record's
//! etag, a JSON Patch `test` of `/rev`. Left to the model, a write sending an old value
//! was accepted. Nothing names such a member, so the mock goes by what it has seen of
//! each stored record. A top-level member of a record is a *token* when:
//!
//! - its read's schema declares it an integer or a string, not a date or a time (a
//!   timestamp changes on every write by nature, which says nothing of its being
//!   checked), and it is no identity or path member;
//! - the type's write carries it: the write's body schema declares it and the body sends
//!   it, the write declares an `If-Match`-style header (`If-Match`, or a name ending in
//!   `-If-Match`) and sends it, or its body is a JSON Patch whose `test` names it;
//! - a write sent its held value (*echoed* it back) and the answer changed it, and since
//!   that first write every 2xx update of the record the model answered changed it: one
//!   answer that left it as it was ends it for that record, for good.
//!
//! While a member is echoed or a token, an update's answer advances it: the model's new
//! value (another string, a greater integer) is stored over the copy's, which the
//! settling of an update otherwise keeps (see [`mb_engine::UpdateGuard::advances`]).
//! Each change is remembered: the value the record held before it, the newest
//! [`MAX_HISTORY`].
//!
//! A write (an update or a delete) whose token is not the record's current value, but
//! one it held before, is refused with the operation's declared `412` or `409` (an
//! `If-Match` header the `412` first, a body or a patch the `409` first) before any LLM
//! call, and nothing changes. A value the record never held is left to the model as
//! before: it may be another spelling of the token (an etag the faker made for a
//! response header, a hash of it) that the mock cannot read, and a refusal on it would
//! be a guess; a value seen and since replaced is a stale read beyond doubt. Values are
//! compared as text, with an entity tag's `W/` and quotes dropped. `If-Match: *` and a
//! request sending no token are never refused, nor is a write to an operation declaring
//! neither status, nor a record's first write.
//!
//! Kept under the store's reserved type ([`mb_state::INTERNAL_TYPE`]), one document per
//! record that has a member echoed, so every backend and node sees the same, and
//! sessions, TTLs and resets clear it; a delete of the record ends it. A session keeps
//! [`MAX_RECORDS`] of them, the newest. A write reads its record's document once, and
//! writes it after a 2xx update when a member was echoed or a token. The document is
//! read, changed and written back: two writes to one record at once can lose a change,
//! which only leaves the next stale write to the model.
//!
//! HTTP, an LLM and `consistency: on` only, with no scenario or forced status:
//! everything else is as it was.

use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Values a token is remembered to have held, the newest.
const MAX_HISTORY: usize = 16;
/// Records whose tokens a session keeps, the newest.
const MAX_RECORDS: usize = 1024;
/// The document listing them, oldest first, and the prefix of their ids.
const INDEX: &str = "#tokens";
/// Longest token value remembered, as text.
const MAX_VALUE_CHARS: usize = 256;

/// What a write found of its record's tokens, for its answer and for after it.
#[derive(Default)]
pub(super) struct Tokens {
    /// The members the update advances: its record's tokens, and those it echoed.
    pub(super) advancing: Vec<String>,
    /// The record's document as read, when it has one.
    held: Option<Map<String, Value>>,
}

/// Whether this call keeps and honours tokens.
pub(super) fn applies(rt: &MockRuntime, plan: &CallPlan<'_>) -> bool {
    rt.consistency
        && plan.engine.has_provider()
        && plain(plan)
        && matches!(plan.op.binding, Binding::Http { .. })
        && matches!(
            plan.op.kind,
            OperationKind::Command(Effect::Update | Effect::Delete)
        )
}

/// How a write sent a token.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Body,
    Header,
    Patch,
}

/// One value a write sent for a member, as text: a header names none.
struct Sent {
    member: Option<String>,
    value: String,
    channel: Channel,
}

/// What `call` sends that may be a token of `held`'s: its body's members the write's
/// body schema declares, the entity tags of an `If-Match`-style header it declares, and
/// the values of its JSON Patch `test`s of a top-level member. Only a string or an
/// integer.
fn sent(rt: &MockRuntime, op: &Operation, call: &OperationCall) -> Vec<Sent> {
    let bundle = rt.bundle();
    let mut out = Vec::new();
    let body_schema = op.input.body.as_ref();
    match call.body.as_ref() {
        Some(Value::Object(members)) => {
            for (member, value) in members {
                let declared = body_schema.is_some_and(|s| {
                    mb_engine::consistency::property_schema(bundle, s, member).is_some()
                });
                if let (true, Some(value)) = (declared, as_text(value)) {
                    out.push(Sent {
                        member: Some(member.clone()),
                        value,
                        channel: Channel::Body,
                    });
                }
            }
        }
        Some(Value::Array(ops)) if is_json_patch(ops) => {
            for item in ops {
                if item.get("op").and_then(Value::as_str) != Some("test") {
                    continue;
                }
                let member = item
                    .get("path")
                    .and_then(Value::as_str)
                    .and_then(top_member);
                let value = item.get("value").and_then(as_text);
                if let (Some(member), Some(value)) = (member, value) {
                    out.push(Sent {
                        member: Some(member),
                        value,
                        channel: Channel::Patch,
                    });
                }
            }
        }
        _ => {}
    }
    for name in if_match_headers(op) {
        let Some(text) = call
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&name))
            .and_then(|(_, v)| v.as_str())
        else {
            continue;
        };
        let tags: Vec<&str> = text.split(',').map(str::trim).collect();
        // Any current representation matches `*`: no token is sent.
        if tags.contains(&"*") {
            continue;
        }
        for tag in tags.into_iter().filter(|t| !t.is_empty()) {
            out.push(Sent {
                member: None,
                value: normalized(tag),
                channel: Channel::Header,
            });
        }
    }
    out
}

/// The `If-Match`-style headers `op` declares, as declared.
fn if_match_headers(op: &Operation) -> Vec<String> {
    let Some(Value::Object(properties)) =
        op.input.headers.as_ref().and_then(|s| s.get("properties"))
    else {
        return Vec::new();
    };
    properties
        .keys()
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            lower == "if-match" || lower.ends_with("-if-match")
        })
        .cloned()
        .collect()
}

/// Whether `ops` is a JSON Patch (RFC 6902): objects, each naming its `op` and `path`.
fn is_json_patch(ops: &[Value]) -> bool {
    !ops.is_empty()
        && ops.iter().all(|item| {
            item.get("op").is_some_and(Value::is_string)
                && item.get("path").is_some_and(Value::is_string)
        })
}

/// The top-level member a JSON Pointer names (`/rev`), unescaped; none for a deeper one.
fn top_member(pointer: &str) -> Option<String> {
    let rest = pointer.strip_prefix('/')?;
    (!rest.is_empty() && !rest.contains('/')).then(|| rest.replace("~1", "/").replace("~0", "~"))
}

/// A string or an integer as the text a token is compared by.
fn as_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(s) => normalized(s),
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        _ => return None,
    };
    (text.chars().count() <= MAX_VALUE_CHARS).then_some(text)
}

/// `text` with an entity tag's weak prefix and quotes dropped: `W/"3"` is `3`.
fn normalized(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix("W/").unwrap_or(text);
    text.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text)
        .to_string()
}

/// Whether member `member` of a stored `t` may be a token by its type (see the module's
/// documentation): its read declares it an integer or a string, not a date or a time,
/// and it is no identity or path member.
fn may_be_token(rt: &MockRuntime, t: &str, member: &str) -> bool {
    let index = rt.index();
    if index.identity_fields(t).iter().any(|f| f == member)
        || index.path_fields(t).iter().any(|f| f == member)
    {
        return false;
    }
    let bundle = rt.bundle();
    let Some(schema) = write_memory::read_schema(rt, t)
        .and_then(|s| mb_engine::consistency::property_schema(bundle, s, member))
    else {
        return false;
    };
    let node = deref_schema(bundle, &schema);
    let types: Vec<&str> = match node.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let dated = node
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|f| matches!(f, "date-time" | "date" | "time"));
    types.iter().any(|t| matches!(*t, "integer" | "string")) && !dated
}

/// `schema` with `$ref`s to the bundle's definitions followed (bounded).
fn deref_schema<'a>(bundle: &'a mb_mcm::Bundle, mut schema: &'a Value) -> &'a Value {
    for _ in 0..16 {
        let Some(target) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| bundle.defs.get(name))
        else {
            break;
        };
        schema = target;
    }
    schema
}

/// The document of `t` `id`'s tokens.
fn key(t: &str, id: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [t, id] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hash: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{INDEX}/{hash}")
}

/// Whether the kept `entry` is a token: written, and changed by every write. An entry
/// keeps a member's observed `writes`, the values it held before each (`history`), and
/// whether one left it as it was (`ended`).
fn is_token(entry: &Value) -> bool {
    entry.get("writes").and_then(Value::as_u64).unwrap_or(0) > 0
        && entry.get("ended").and_then(Value::as_bool) != Some(true)
}

/// Whether the kept `entry` remembers `value` as one its member held before.
fn held_before(entry: &Value, value: &str) -> bool {
    entry
        .get("history")
        .and_then(Value::as_array)
        .is_some_and(|h| h.iter().any(|v| v.as_str() == Some(value)))
}

/// What a write `plan` to `target` decides of its tokens, before any generation: the
/// refusal of a stale one (see the module's documentation), or what its answer
/// advances.
pub(super) async fn check(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: &StoredResource,
) -> Result<Tokens, GeneratedResponse> {
    let rt = state.rt;
    let op = plan.op;
    let t = target.resource_type.as_str();
    let sent = sent(rt, op, call);
    let held = read(state, &plan.session, t, &target.id).await;
    let current = |member: &str| target.data.get(member).and_then(as_text);
    let live: Vec<(&String, &Value)> = held
        .iter()
        .flatten()
        .filter(|(member, entry)| is_token(entry) && may_be_token(rt, t, member))
        .collect();
    // A body's or a patch's token names its member.
    let named = sent.iter().find(|s| {
        let Some(member) = s.member.as_deref() else {
            return false;
        };
        live.iter().any(|(m, entry)| {
            m.as_str() == member
                && current(member).is_some_and(|now| now != s.value)
                && held_before(entry, &s.value)
        })
    });
    // An entity tag names none: stale when it is no token's current value, and one a
    // token held before.
    let tags: Vec<&Sent> = sent
        .iter()
        .filter(|s| s.channel == Channel::Header)
        .collect();
    let tagged = !tags.is_empty()
        && !tags.iter().any(|s| {
            live.iter()
                .any(|(m, _)| current(m).is_some_and(|now| now == s.value))
        })
        && tags.iter().any(|s| {
            live.iter().any(|(m, entry)| {
                current(m).is_some_and(|now| now != s.value) && held_before(entry, &s.value)
            })
        });
    let codes: &[u16] = match (named, tagged) {
        (Some(_), _) => &[409, 412],
        (None, true) => &[412, 409],
        (None, false) => &[],
    };
    if let Some(refusal) = codes
        .iter()
        .find_map(|code| plan.engine.refuse_from_state(rt.bundle(), op, call, *code))
    {
        tracing::info!(
            mock = %rt.name,
            operation = %op.id,
            status = refusal.status,
            "write refused: it sent a concurrency token its record held before"
        );
        return Err(refusal);
    }
    let mut advancing = tokens_of(rt, t, target, held.as_ref());
    if op.kind == OperationKind::Command(Effect::Update) {
        for member in echoed(rt, t, target, &sent) {
            let ended = held
                .as_ref()
                .and_then(|h| h.get(&member))
                .is_some_and(|e| e.get("ended") == Some(&Value::Bool(true)));
            if !ended && !advancing.contains(&member) {
                advancing.push(member);
            }
        }
    }
    Ok(Tokens { advancing, held })
}

/// The tokens `held` keeps of `target` that it still holds.
fn tokens_of(
    rt: &MockRuntime,
    t: &str,
    target: &StoredResource,
    held: Option<&Map<String, Value>>,
) -> Vec<String> {
    held.into_iter()
        .flatten()
        .filter(|(member, entry)| {
            is_token(entry)
                && target.data.get(member.as_str()).is_some()
                && may_be_token(rt, t, member)
        })
        .map(|(member, _)| member.clone())
        .collect()
}

/// The members of `target` whose held value the write `sent` back: by its body or a
/// patch's `test`, or as an entity tag.
fn echoed(rt: &MockRuntime, t: &str, target: &StoredResource, sent: &[Sent]) -> Vec<String> {
    let Value::Object(members) = &target.data else {
        return Vec::new();
    };
    members
        .iter()
        .filter(|(member, value)| {
            let Some(now) = as_text(value) else {
                return false;
            };
            sent.iter()
                .any(|s| s.value == now && s.member.as_deref().is_none_or(|m| m == member.as_str()))
                && may_be_token(rt, t, member)
        })
        .map(|(member, _)| member.clone())
        .collect()
}

/// After a 2xx update of `before` the model answered: what each member it advanced did.
/// One that changed keeps the value it held before; one left as it was is no token of
/// this record any more.
pub(super) async fn after_update(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    before: &StoredResource,
    tokens: &Tokens,
) {
    if tokens.advancing.is_empty() {
        return;
    }
    let t = before.resource_type.as_str();
    let after = state.get(session, t, &before.id).await;
    let mut doc = tokens.held.clone().unwrap_or_default();
    for member in &tokens.advancing {
        let was = before.data.get(member).and_then(as_text);
        let now = after
            .as_ref()
            .and_then(|a| a.data.get(member))
            .and_then(as_text);
        let mut entry = doc
            .remove(member)
            .and_then(|e| match e {
                Value::Object(e) => Some(e),
                _ => None,
            })
            .unwrap_or_default();
        let writes = entry.get("writes").and_then(Value::as_u64).unwrap_or(0) + 1;
        entry.insert("writes".into(), Value::from(writes));
        match (was, now) {
            (Some(was), Some(now)) if was != now => {
                let mut history: Vec<Value> = entry
                    .get("history")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                history.retain(|v| v.as_str() != Some(was.as_str()));
                history.push(Value::String(was));
                let excess = history.len().saturating_sub(MAX_HISTORY);
                history.drain(..excess);
                entry.insert("history".into(), Value::Array(history));
            }
            _ => {
                entry.insert("ended".into(), Value::Bool(true));
            }
        }
        doc.insert(member.clone(), Value::Object(entry));
    }
    let id = key(t, &before.id);
    let new = tokens.held.is_none();
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: id.clone(),
                data: Value::Object(doc),
            },
        )
        .await;
    if new {
        let mut listed: Vec<String> = match state.get(session, INTERNAL_TYPE, INDEX).await {
            Some(doc) => serde_json::from_value(doc.data).unwrap_or_default(),
            None => Vec::new(),
        };
        listed.retain(|held| *held != id);
        listed.push(id);
        let excess = listed.len().saturating_sub(MAX_RECORDS);
        for oldest in listed.drain(..excess) {
            state.delete(session, INTERNAL_TYPE, &oldest).await;
        }
        state
            .upsert(
                session,
                StoredResource {
                    resource_type: INTERNAL_TYPE.to_string(),
                    id: INDEX.to_string(),
                    data: Value::from(listed),
                },
            )
            .await;
    }
}

/// After a 2xx delete of `t` `id`: its tokens go with it. The index keeps the entry,
/// which the bound takes out.
pub(super) async fn after_delete(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    id: &str,
    tokens: &Tokens,
) {
    if tokens.held.is_some() {
        state.delete(session, INTERNAL_TYPE, &key(t, id)).await;
    }
}

/// The document of `t` `id`'s tokens, when it has one.
async fn read(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    id: &str,
) -> Option<Map<String, Value>> {
    match state.get(session, INTERNAL_TYPE, &key(t, id)).await?.data {
        Value::Object(doc) => Some(doc),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entity_tags_compare_without_weakness_or_quotes() {
        assert_eq!(normalized("W/\"3\""), "3");
        assert_eq!(normalized("\"abc\""), "abc");
        assert_eq!(normalized(" abc "), "abc");
        assert_eq!(as_text(&json!(7)), Some("7".into()));
        assert_eq!(as_text(&json!(7.5)), None);
        assert_eq!(as_text(&json!(true)), None);
    }

    #[test]
    fn a_patch_test_names_a_top_level_member_only() {
        assert_eq!(top_member("/rev"), Some("rev".into()));
        assert_eq!(top_member("/a~1b"), Some("a/b".into()));
        assert_eq!(top_member("/meta/rev"), None);
        assert_eq!(top_member("/"), None);
        assert!(is_json_patch(&[
            json!({"op": "test", "path": "/rev", "value": 1})
        ]));
        assert!(!is_json_patch(&[json!({"name": "x"})]));
        assert!(!is_json_patch(&[]));
    }
}
