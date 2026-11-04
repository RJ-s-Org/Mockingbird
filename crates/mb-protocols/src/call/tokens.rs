//! Optimistic concurrency: a write the contract says is checked, sent with a token that
//! is not the record's current one, is refused before any LLM call, as the API would.
//!
//! Only the contract ties a value to a refusal, in one of three ways:
//!
//! - **A body member.** A top-level member of the record is a token for a write when the
//!   write declares a `409` or a `412` whose description names it in code position
//!   (`` `version` ``), or whose schema declares a member of that name and type. The
//!   member is an integer or a string with no `enum` or `const` (a state, not a version)
//!   and no identity or path member. Once an update answered by the model changed it
//!   (see below), a write sending another value than the record's current one is
//!   refused. A record's first write is never refused this way, as nothing was seen of
//!   how its token moves.
//! - **An `If-Match` header** (`If-Match`, or a name ending in `-If-Match`) the write
//!   declares. It is compared only with the entity tag the mock itself served in an
//!   `ETag` response header for that record in this session, never with a member. A
//!   read serves the record's current tag again, so it stays put between writes; a 2xx
//!   create or update serves a new one. A write whose tags are none of the current one
//!   is refused; `If-Match: *`, and a record the mock served no tag for, are not.
//! - **A JSON Patch `test`.** A patch is applied in order to the stored record, and a
//!   `test` failing against a value the record holds fails it (RFC 6902), with no
//!   history needed; a path the stored copy lacks decides nothing. An update's patch
//!   also names, for its answer to settle on, the members its operations change (see
//!   [`mb_engine::json_patch_body`]).
//!
//! A refusal is the operation's declared `409` or `412` (a header the `412` first, a
//! body or a patch the `409` first); with neither declared, nothing is refused. It
//! changes nothing. Values are compared as text, an entity tag's `W/` and quotes dropped.
//! A request sending no token is never refused.
//!
//! A body token the request sends no new value for (it sends none, or the held one) is
//! *advanced* by the update: the model's new value (another string, a greater integer)
//! is stored, even for a `readOnly` member, where the settling of an update otherwise
//! keeps the copy's, and the request's echo does not stand over it. Nothing else of the
//! request is touched, and the target's identity and owner are kept (see
//! [`mb_engine::UpdateGuard::advances`]). A member the request sets to a new value is the
//! client's, as before.
//!
//! Kept under the store's reserved type ([`mb_state::INTERNAL_TYPE`]), per record: which
//! body tokens an update was seen to change, and the current entity tag. So every
//! backend and node sees the same, sessions, TTLs and resets clear it, a session keeps
//! [`MAX_RECORDS`] records' worth, the newest, and a delete of the record ends it. A
//! document is read, changed and written back: two writes to one record at once can lose
//! a change, which only leaves a stale write to the model.
//!
//! HTTP, an LLM and `consistency: on` only, with no scenario or forced status:
//! everything else is as it was.

#[cfg(feature = "http-rest")]
use axum::http::{HeaderMap, HeaderValue};
use mb_engine::json_patch_body::{self, Applied};
use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Records whose tokens a session keeps, the newest.
const MAX_RECORDS: usize = 1024;
/// The document listing them, oldest first, and the prefix of their ids.
const INDEX: &str = "#tokens";
/// Where a record's entity tag is kept, beside its tokens.
const ETAGS: &str = "#etags";
/// Longest token value compared, as text.
const MAX_VALUE_CHARS: usize = 256;
/// How deep an error schema is searched for a member.
const MAX_DEPTH: usize = 6;

/// What a write found of its record's tokens, for its answer and for after it.
#[derive(Default)]
pub(super) struct Tokens {
    /// The body tokens the update advances.
    pub(super) advancing: Vec<String>,
    /// The body tokens an update was seen to change, as read.
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
}

/// Whether `plan` is a write tokens guard.
fn guards(plan: &CallPlan<'_>) -> bool {
    matches!(
        plan.op.kind,
        OperationKind::Command(Effect::Update | Effect::Delete)
    )
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

/// `schema`'s `type`s, through `$ref`s.
fn types<'a>(bundle: &'a mb_mcm::Bundle, schema: &'a Value) -> (Vec<&'a str>, &'a Value) {
    let node = deref_schema(bundle, schema);
    let types = match node.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    (types, node)
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

/// The kind of token member `member` of a stored `t` is by its read's schema: `integer`
/// or `string`, with no `enum` or `const`, and no identity or path member.
fn token_kind(rt: &MockRuntime, t: &str, member: &str) -> Option<&'static str> {
    let index = rt.index();
    if index.identity_fields(t).iter().any(|f| f == member)
        || index.path_fields(t).iter().any(|f| f == member)
    {
        return None;
    }
    let bundle = rt.bundle();
    let schema = write_memory::read_schema(rt, t)
        .and_then(|s| mb_engine::consistency::property_schema(bundle, s, member))?;
    let (types, node) = types(bundle, &schema);
    if node.get("enum").is_some() || node.get("const").is_some() {
        return None;
    }
    if types.contains(&"integer") {
        Some("integer")
    } else if types.contains(&"string") {
        Some("string")
    } else {
        None
    }
}

/// Whether `op` ties member `member` of a stored `t` to a refusal (see the module's
/// documentation): it is a token by its kind, and a `409` or `412` of `op` names it in
/// code position in its description, or declares a member of that name and kind.
fn tied(rt: &MockRuntime, op: &Operation, t: &str, member: &str) -> bool {
    let Some(kind) = token_kind(rt, t, member) else {
        return false;
    };
    let bundle = rt.bundle();
    op.outputs.iter().any(|variant| {
        if !matches!(
            variant.status,
            mb_mcm::StatusSelector::Code(409) | mb_mcm::StatusSelector::Code(412)
        ) {
            return false;
        }
        let described = variant.description.as_deref().is_some_and(|d| {
            mb_engine::contract_values::code_spans(d)
                .iter()
                .any(|s| s == member)
        });
        described
            || variant
                .schema
                .as_ref()
                .is_some_and(|s| declares(bundle, s, member, kind, 0))
    })
}

/// Whether `schema` declares a member `member` of `kind` at any depth.
fn declares(
    bundle: &mb_mcm::Bundle,
    schema: &Value,
    member: &str,
    kind: &str,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let node = deref_schema(bundle, schema);
    if let Some(Value::Object(properties)) = node.get("properties") {
        for (name, inner) in properties {
            if name == member {
                let (found, _) = types(bundle, inner);
                let fits = match kind {
                    "integer" => found.contains(&"integer") || found.contains(&"number"),
                    _ => found.contains(&kind),
                };
                if fits {
                    return true;
                }
            }
            if declares(bundle, inner, member, kind, depth + 1) {
                return true;
            }
        }
    }
    let parts = ["allOf", "oneOf", "anyOf"]
        .into_iter()
        .filter_map(|k| node.get(k).and_then(Value::as_array))
        .flatten();
    let items = node.get("items").into_iter();
    parts
        .chain(items)
        .any(|inner| declares(bundle, inner, member, kind, depth + 1))
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

/// The entity tags `call` sends in a declared `If-Match`-style header, normalized; none
/// for `*`, which any current representation matches.
fn sent_tags(op: &Operation, call: &OperationCall) -> Vec<String> {
    let mut out = Vec::new();
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
        if tags.contains(&"*") {
            continue;
        }
        out.extend(tags.into_iter().filter(|t| !t.is_empty()).map(normalized));
    }
    out
}

/// The document of `t` `id`'s `kind` (tokens or entity tag).
fn key(kind: &str, t: &str, id: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [kind, t, id] {
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
    if !guards(plan) {
        return Ok(Tokens::default());
    }
    let rt = state.rt;
    let op = plan.op;
    let t = target.resource_type.as_str();
    let session = &plan.session;
    let refuse = |codes: &[u16]| {
        codes
            .iter()
            .find_map(|code| plan.engine.refuse_from_state(rt.bundle(), op, call, *code))
    };

    // A JSON Patch: its tests, and what it changes.
    let mut changed: Option<Value> = None;
    if let Some(body) = call.body.as_ref() {
        match json_patch_body::apply(&target.data, body) {
            Applied::TestFailed => {
                if let Some(refusal) = refuse(&[409, 412]) {
                    log_refusal(rt, op, &refusal, "a JSON Patch test failed");
                    return Err(refusal);
                }
            }
            Applied::Applied(after) => {
                changed = Some(json_patch_body::changed_members(&target.data, &after));
            }
            Applied::NotAPatch | Applied::Unknown => {}
        }
    }

    // An entity tag, against the one the mock served.
    let tags = sent_tags(op, call);
    if !tags.is_empty()
        && let Some(current) = current_etag(state, session, t, &target.id).await
        && !tags.contains(&normalized(&current))
        && let Some(refusal) = refuse(&[412, 409])
    {
        log_refusal(
            rt,
            op,
            &refusal,
            "its If-Match is not the record's entity tag",
        );
        return Err(refusal);
    }

    // A body member the contract ties to a refusal.
    let doc = state
        .get(session, INTERNAL_TYPE, &key(INDEX, t, &target.id))
        .await;
    let confirmed: Vec<String> = doc
        .as_ref()
        .and_then(|d| d.data.get("confirmed"))
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default();
    let body = match (&changed, call.body.as_ref()) {
        (None, Some(Value::Object(members))) => Some(members),
        _ => None,
    };
    let body_schema = op.input.body.as_ref();
    for (member, sent) in body.into_iter().flatten() {
        let declared = body_schema.is_some_and(|s| {
            mb_engine::consistency::property_schema(rt.bundle(), s, member).is_some()
        });
        let (Some(sent), Some(now)) = (as_text(sent), target.data.get(member).and_then(as_text))
        else {
            continue;
        };
        if declared
            && sent != now
            && confirmed.contains(member)
            && tied(rt, op, t, member)
            && let Some(refusal) = refuse(&[409, 412])
        {
            log_refusal(rt, op, &refusal, "its body sent a stale concurrency token");
            return Err(refusal);
        }
    }

    // What the update advances: a tied member it sets no new value for.
    let mut advancing = Vec::new();
    if op.kind == OperationKind::Command(Effect::Update)
        && let Value::Object(held) = &target.data
    {
        let sets = changed.as_ref().and_then(Value::as_object).or(body);
        for (member, value) in held {
            let new = sets
                .and_then(|s| s.get(member))
                .is_some_and(|sent| sent != value && as_text(sent) != as_text(value));
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

fn log_refusal(rt: &MockRuntime, op: &Operation, refusal: &GeneratedResponse, why: &str) {
    tracing::info!(
        mock = %rt.name,
        operation = %op.id,
        status = refusal.status,
        reason = why,
        "write refused before generation"
    );
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
    let id = key(INDEX, t, &before.id);
    let data = serde_json::json!({ "confirmed": confirmed });
    store(state, session, id, data, !tokens.kept).await;
}

/// Writes document `id`, listing it in the index when it is `new`; the index keeps the
/// newest [`MAX_RECORDS`] and ends those past it.
async fn store(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    id: String,
    data: Value,
    new: bool,
) {
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: id.clone(),
                data,
            },
        )
        .await;
    if !new {
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

/// After a 2xx delete of `t` `id`: its tokens and entity tag go with it. The index keeps
/// the entries, which the bound takes out.
pub(super) async fn after_delete(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    id: &str,
    tokens: &Tokens,
) {
    if tokens.kept {
        state
            .delete(session, INTERNAL_TYPE, &key(INDEX, t, id))
            .await;
    }
    state
        .delete(session, INTERNAL_TYPE, &key(ETAGS, t, id))
        .await;
}

/// The entity tag the mock last served for `t` `id`.
async fn current_etag(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    id: &str,
) -> Option<String> {
    let doc = state
        .get(session, INTERNAL_TYPE, &key(ETAGS, t, id))
        .await?;
    doc.data.get("current")?.as_str().map(str::to_string)
}

#[cfg(feature = "http-rest")]
/// The record a 2xx answer of `plan` is about, for its `ETag`: a create's by the identity
/// its answer holds, any other's by its path.
pub(crate) fn etag_record(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    generated: &GeneratedResponse,
) -> Option<(String, String)> {
    let op = plan.op;
    if !applies(rt, plan) || !(200..300).contains(&generated.status) {
        return None;
    }
    let t = op.resource_type.clone()?;
    let id = match op.kind {
        OperationKind::Command(Effect::Create) => {
            let identity = op.identity.as_ref();
            let body = generated.body.as_ref()?;
            let record = match identity.filter(|i| i.record.is_some()) {
                Some(identity) => identity.record_in(body)?,
                None => body,
            };
            let field = identity.map_or("id", |i| i.field.as_str());
            id_string(record.get(field)?)?
        }
        _ => target_id(op, call)?,
    };
    Some((t, id))
}

#[cfg(feature = "http-rest")]
/// After an answer about `record` was given its declared headers: its `ETag`, when it has
/// one. A read serves the record's current tag again, or makes the one it drew current;
/// a create or an update makes the one it drew current; a delete ends it.
pub(crate) async fn serve_etag(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    record: Option<(String, String)>,
    headers: &mut HeaderMap,
) {
    let Some((t, id)) = record else { return };
    let session = &plan.session;
    let mut state = StateAccess::new(rt);
    let doc = key(ETAGS, &t, &id);
    // A delete's ends with its record (see [`after_delete`]).
    if plan.op.kind == OperationKind::Command(Effect::Delete) {
        return;
    }
    let Some(drawn) = headers
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    else {
        return;
    };
    let current = current_etag(&mut state, session, &t, &id).await;
    if matches!(plan.op.kind, OperationKind::Query)
        && let Some(current) = &current
    {
        if let Ok(value) = HeaderValue::from_str(current) {
            headers.insert("etag", value);
        }
        return;
    }
    let data = serde_json::json!({ "current": drawn });
    store(&mut state, session, doc, data, current.is_none()).await;
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
}
