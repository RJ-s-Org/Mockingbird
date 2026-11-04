//! Optimistic concurrency the contract declares: a write sending a version that is not
//! its record's current one is refused before any LLM call, as the API would.
//!
//! Only the contract ties a member to a refusal. A top-level member of a stored record
//! is a *token* for a write when:
//!
//! - a `409` or a `412` the write declares names it in code position in its description
//!   (`` `version` ``, `<code>version</code>`). A member an error schema merely declares
//!   is none: a problem's `title` is no record's;
//! - the server manages it: the record's read marks it `readOnly`, or the write's input
//!   cannot set it (its body schema does not declare it, or marks it `readOnly`). So
//!   "a project with this `name` already exists" ties no `name` the write sets;
//! - it is an integer or a string with no `enum` or `const` (a state, not a version),
//!   and no identity or path member.
//!
//! Once an update answered by the model was seen to change a token, a write sending it in
//! its body with another value than the record's current one is answered with the
//! declared `409` (else the `412`), before any LLM call, and nothing changes. A record's
//! first write is never refused this way, as nothing was seen of how its token moves; nor
//! is a request sending no token, nor an operation declaring neither status. Values are
//! compared as text.
//!
//! A token the request sends no new value for (it sends none, or the held one) is
//! *advanced* by the update: the model's new value (another string, a greater integer)
//! is stored, even for a `readOnly` member, where the settling of an update otherwise
//! keeps the copy's, and the request's echo does not stand over it. Nothing else of the
//! request is touched, and the target's identity and owner are kept (see
//! [`mb_engine::UpdateGuard::advances`]).
//!
//! Which tokens an update was seen to change is kept per record under the store's
//! reserved type ([`mb_state::INTERNAL_TYPE`]), so every backend and node sees the same,
//! sessions, TTLs and resets clear it, a session keeps [`MAX_RECORDS`] records' worth,
//! the newest, and a delete of the record ends it. A document is read, changed and
//! written back: two writes to one record at once can lose a change, which only leaves a
//! stale write to the model.
//!
//! HTTP, an LLM and `consistency: on` only, with no scenario or forced status:
//! everything else is as it was.

use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Records whose tokens a session keeps, the newest.
const MAX_RECORDS: usize = 1024;
/// The document listing them, oldest first, and the prefix of their ids.
const INDEX: &str = "#tokens";
/// Longest token value compared, as text.
const MAX_VALUE_CHARS: usize = 256;

/// What a write found of its record's tokens, for its answer and for after it.
#[derive(Default)]
pub(super) struct Tokens {
    /// The tokens the update advances.
    pub(super) advancing: Vec<String>,
    /// The tokens an update was seen to change, as read.
    confirmed: Vec<String>,
    /// Whether the record has a document of its tokens.
    kept: bool,
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

/// A string or an integer as the text a token is compared by.
fn as_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        _ => return None,
    };
    (text.chars().count() <= MAX_VALUE_CHARS).then_some(text)
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

/// Whether `schema` marks itself `readOnly`, through `$ref`s and `allOf`s.
fn read_only(bundle: &mb_mcm::Bundle, schema: &Value, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    let node = deref_schema(bundle, schema);
    if schema.get("readOnly").and_then(Value::as_bool) == Some(true)
        || node.get("readOnly").and_then(Value::as_bool) == Some(true)
    {
        return true;
    }
    node.get("allOf")
        .and_then(Value::as_array)
        .is_some_and(|parts| parts.iter().any(|p| read_only(bundle, p, depth + 1)))
}

/// Whether `op` ties member `member` of a stored `t` to a refusal (see the module's
/// documentation).
fn tied(rt: &MockRuntime, op: &Operation, t: &str, member: &str) -> bool {
    let named = op.outputs.iter().any(|variant| {
        matches!(
            variant.status,
            mb_mcm::StatusSelector::Code(409) | mb_mcm::StatusSelector::Code(412)
        ) && variant.description.as_deref().is_some_and(|d| {
            mb_engine::contract_values::code_spans(d)
                .iter()
                .any(|s| s == member)
        })
    });
    if !named {
        return false;
    }
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
    if node.get("enum").is_some() || node.get("const").is_some() {
        return false;
    }
    let types: Vec<&str> = match node.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !types.iter().any(|t| matches!(*t, "integer" | "string")) {
        return false;
    }
    // Server-managed: the client cannot set it.
    let settable = op
        .input
        .body
        .as_ref()
        .and_then(|s| mb_engine::consistency::property_schema(bundle, s, member))
        .is_some_and(|input| !read_only(bundle, &input, 0));
    read_only(bundle, &schema, 0) || !settable
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
    let doc = state.get(&plan.session, INTERNAL_TYPE, &key(t, &target.id)).await;
    let confirmed: Vec<String> = doc
        .as_ref()
        .and_then(|d| d.data.get("confirmed"))
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default();
    let body = match call.body.as_ref() {
        Some(Value::Object(members)) => Some(members),
        _ => None,
    };
    for (member, sent) in body.into_iter().flatten() {
        let (Some(sent), Some(now)) = (as_text(sent), target.data.get(member).and_then(as_text))
        else {
            continue;
        };
        if sent != now
            && confirmed.contains(member)
            && tied(rt, op, t, member)
            && let Some(refusal) = [409, 412]
                .iter()
                .find_map(|code| plan.engine.refuse_from_state(rt.bundle(), op, call, *code))
        {
            tracing::info!(
                mock = %rt.name,
                operation = %op.id,
                status = refusal.status,
                "write refused before generation: it sent a stale concurrency token"
            );
            return Err(refusal);
        }
    }

    // What the update advances: a token it sets no new value for.
    let mut advancing = Vec::new();
    if op.kind == OperationKind::Command(Effect::Update)
        && let Value::Object(held) = &target.data
    {
        for (member, value) in held {
            let new = body
                .and_then(|b| b.get(member))
                .is_some_and(|sent| as_text(sent) != as_text(value));
            if !new && as_text(value).is_some() && tied(rt, op, t, member) {
                advancing.push(member.clone());
            }
        }
    }
    Ok(Tokens {
        advancing,
        confirmed,
        kept: doc.is_some(),
    })
}

/// After a 2xx update of `before` the model answered: each advancing token it changed is
/// confirmed, so a later write sending another value is refused.
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
    let Some(after) = state.get(session, t, &before.id).await else {
        return;
    };
    let mut confirmed = tokens.confirmed.clone();
    for member in &tokens.advancing {
        let was = before.data.get(member).and_then(as_text);
        let now = after.data.get(member).and_then(as_text);
        if was.is_some() && now.is_some() && was != now && !confirmed.contains(member) {
            confirmed.push(member.clone());
        }
    }
    if confirmed == tokens.confirmed {
        return;
    }
    let id = key(t, &before.id);
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: id.clone(),
                data: serde_json::json!({ "confirmed": confirmed }),
            },
        )
        .await;
    if tokens.kept {
        return;
    }
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

/// After a 2xx delete of `t` `id`: its tokens go with it. The index keeps the entry,
/// which the bound takes out.
pub(super) async fn after_delete(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    id: &str,
    tokens: &Tokens,
) {
    if tokens.kept {
        state.delete(session, INTERNAL_TYPE, &key(t, id)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tokens_compare_as_text() {
        assert_eq!(as_text(&json!(7)), Some("7".into()));
        assert_eq!(as_text(&json!("7")), Some("7".into()));
        assert_eq!(as_text(&json!(7.5)), None);
        assert_eq!(as_text(&json!(true)), None);
    }
}
