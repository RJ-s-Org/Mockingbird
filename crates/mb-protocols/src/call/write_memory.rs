//! What a 2xx write did not say, kept for the next generation of its resource.
//!
//! A create answering `{id, key}` or an action answering 204 leaves a stored
//! copy that does not hold what the request asked. Its request is remembered for
//! the resource it targets and for the parent its path names, under a reserved
//! type of the state store ([`mb_state::INTERNAL_TYPE`]), so Redis, SQL and
//! memory all keep it, sessions, TTLs and resets clear it, and it is no resource
//! (not exported, counted, seeded or on a change feed). The next generation for
//! that resource is shown the requests; a read, which could otherwise answer from
//! state and never see them, must generate. Its validated answer replaces the
//! stored copy and the memory goes, so a write costs one extra LLM call at most.
//!
//! HTTP-bound operations only: GraphQL stores what its answers hold under their
//! own types, and gRPC and SOAP are not remembered for, so their prompts and their
//! reads are as they were.

use mb_state::INTERNAL_TYPE;

use super::*;

/// Requests kept per resource, the newest.
const MAX_REMEMBERED: usize = 5;
/// Largest request body kept as it is, as serialized JSON. A larger one is cut.
const MAX_BODY_BYTES: usize = 4 * 1024;
/// A body serialized larger than this is not even previewed: the PII scrubber
/// skips very long strings, so a cut of one could keep what it did not see.
const MAX_PREVIEWED_BYTES: usize = 64 * 1024;
/// Largest memory of one resource, as serialized JSON. Oldest requests go first.
const MAX_BYTES: usize = 16 * 1024;
/// What a credential-named key's value is replaced with: the request log's text.
const REDACTED: &str = "[redacted]";

/// What a generation was told of one resource's remembered requests.
#[derive(Default)]
pub(super) struct Earlier {
    /// The requests shown, oldest first.
    pub(super) shown: Vec<Value>,
    /// What the store holds that the read could serve, were it not behind them:
    /// it answers when the LLM fails, as a faker must not invent a change.
    pub(super) stored_answer: Option<Box<GeneratedResponse>>,
}

/// One served write, for [`after_write`].
pub(super) struct Written<'a> {
    pub(super) call: &'a OperationCall,
    pub(super) generated: &'a GeneratedResponse,
    /// The id the call named or found, before any new record of its own.
    pub(super) acted_on: Option<&'a str>,
    /// Whether the session held that resource for this caller.
    pub(super) held: bool,
    /// The key holds another owner's resource.
    pub(super) foreign_key: bool,
    /// A new record that could not be stored.
    pub(super) unstored: bool,
    pub(super) earlier: &'a Earlier,
}

fn memory_id(resource_type: &str, id: &str) -> String {
    format!("{resource_type}/{id}")
}

/// Whether this call may use the memory at all: with no LLM there is nothing to
/// tell, and an offline read answers as it always did.
fn applies(rt: &MockRuntime, plan: &CallPlan<'_>) -> bool {
    rt.consistency
        && plan.engine.has_provider()
        && matches!(plan.op.binding, Binding::Http { .. })
        && matches!(
            plan.op.kind,
            OperationKind::Query | OperationKind::Command(_)
        )
}

/// The requests remembered for the resource `op` targets. A name that may be
/// another owner's, with nothing held to say whose, has none: its requests would
/// be told to the wrong owner's generation.
pub(super) async fn recall(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target_id: Option<&str>,
    held: bool,
) -> Earlier {
    let rt = state.rt;
    let (true, Some(t), Some(id)) = (
        applies(rt, plan),
        plan.op.resource_type.as_deref(),
        target_id,
    ) else {
        return Earlier::default();
    };
    if !held && guarded(rt, plan.op, call).is_some() {
        return Earlier::default();
    }
    Earlier {
        shown: requests_of(state, &plan.session, t, id).await,
        stored_answer: None,
    }
}

async fn requests_of(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    resource_type: &str,
    id: &str,
) -> Vec<Value> {
    let found = state
        .get(session, INTERNAL_TYPE, &memory_id(resource_type, id))
        .await;
    found
        .and_then(|doc| doc.data.get("requests")?.as_array().cloned())
        .unwrap_or_default()
}

/// Whether `op`'s success answer is what its resource type's read by path
/// answers, so that it can replace what the session holds.
fn reads_the_resource(rt: &MockRuntime, op: &Operation) -> bool {
    is_read_schema(rt, op, op.success_output())
}

/// Whether an answer of `output` has the schema of `op`'s type's read by path.
fn is_read_schema(
    rt: &MockRuntime,
    op: &Operation,
    output: Option<&mb_mcm::OutputVariant>,
) -> bool {
    let Some(t) = op.resource_type.as_deref() else {
        return false;
    };
    let detail = rt.index().detail_reads.get(t).and_then(|i| {
        rt.bundle()
            .operations
            .get(*i)?
            .success_output()?
            .schema
            .as_ref()
    });
    let own = output.and_then(|o| o.schema.as_ref());
    detail.is_some() && detail == own
}

impl Earlier {
    /// Whether this read must generate though the store could answer it.
    pub(super) fn forces(
        &self,
        rt: &MockRuntime,
        plan: &CallPlan<'_>,
        call: &OperationCall,
    ) -> bool {
        !self.shown.is_empty()
            && matches!(plan.op.kind, OperationKind::Query)
            && plain(plan)
            && mb_engine::seed::request_seed(call).is_none()
            && reads_the_resource(rt, plan.op)
    }
}

/// What a write with no answer of its own acts on, when its route does not say
/// which field its parameter names: such an operation has no identity, as nothing
/// in an answer shows one. As a delete does, it goes by what the type's reads go by
/// (Jira's `PUT /issue/{issueIdOrKey}` with a key), or it would be taken for a new
/// resource under that key, which then answers reads in place of the real one.
pub(super) async fn silent_write_target(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target_id: Option<&str>,
) -> Option<StoredResource> {
    let rt = state.rt;
    let op = plan.op;
    if !applies(rt, plan)
        || !matches!(
            op.kind,
            OperationKind::Command(Effect::Update | Effect::Other)
        )
        || op.success_output().is_none_or(|o| o.schema.is_some())
    {
        return None;
    }
    let (t, id) = (op.resource_type.as_deref()?, target_id?);
    let param = target_param(op)?;
    if id_like(&param) {
        return None;
    }
    let mut owner = guarded(rt, op, call);
    if let Some(owner) = &mut owner {
        owner.writing = true;
    }
    let fields: Vec<&str> = rt
        .index()
        .path_fields(t)
        .iter()
        .map(String::as_str)
        .collect();
    by_path_fields(state, &plan.session, t, &fields, id, owner.as_ref()).await
}

/// Whether a write leaves the stored copy alone: an update answering nothing,
/// whose request is remembered instead. Storing that request in the copy's place
/// would lose what it did not repeat, identity included; the read that catches up
/// is shown both. Offline there is no such read, and the request still replaces it.
pub(super) fn keeps_stored(
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    generated: &GeneratedResponse,
    held: bool,
) -> bool {
    held && generated.body.is_none()
        && applies(rt, plan)
        && plan.op.kind == OperationKind::Command(Effect::Update)
}

/// `value` with every credential-named key's value replaced, at any depth, as
/// [`is_sensitive`] decides for headers.
fn redact_credentials(value: &mut Value) {
    match value {
        Value::Object(members) => {
            for (key, member) in members.iter_mut() {
                if is_sensitive(key) {
                    *member = Value::from(REDACTED);
                } else {
                    redact_credentials(member);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_credentials),
        _ => {}
    }
}

/// A request body as it may be kept: credentials redacted, then the request
/// log's own redaction, then the PII the engine scrubs from answers and effects.
fn scrubbed(rt: &MockRuntime, plan: &CallPlan<'_>, body: &Value) -> Value {
    let mut body = body.clone();
    redact_credentials(&mut body);
    if let Some(recorder) = &rt.recorder {
        recorder.redact(&mut body);
    }
    plan.engine.scrub(&mut body);
    body
}

/// `body` within [`MAX_BODY_BYTES`]: as it is, or cut to a string that says so.
fn within_cap(body: Value) -> Value {
    let text = body.to_string();
    if text.len() <= MAX_BODY_BYTES {
        return body;
    }
    if text.len() > MAX_PREVIEWED_BYTES {
        return serde_json::json!({"truncated": true, "bytes": text.len()});
    }
    let mut end = MAX_BODY_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Value::String(format!("{}[truncated]", &text[..end]))
}

/// Whether `answer` holds what `request` said. A string counts wherever it is
/// (`customerName` may come back as `customer.name`); a number or a flag only
/// under the same member, as a `1` or a `false` turns up anywhere. What was
/// redacted cannot be compared and was never going to be shown.
fn carries(request: &Value, answer: &Value) -> bool {
    fn strings<'a>(value: &'a Value, member: &'a str, out: &mut Vec<(&'a str, &'a Value)>) {
        match value {
            Value::Object(members) => {
                for (key, inner) in members {
                    strings(inner, key, out);
                }
            }
            Value::Array(items) => {
                for inner in items {
                    strings(inner, member, out);
                }
            }
            Value::Null => {}
            leaf => out.push((member, leaf)),
        }
    }
    let (mut said, mut held) = (Vec::new(), Vec::new());
    strings(request, "", &mut said);
    strings(answer, "", &mut held);
    said.iter().all(|(member, leaf)| match leaf {
        Value::String(text) => {
            text.is_empty()
                || text == REDACTED
                || held.iter().any(|(_, v)| v.as_str() == Some(text.as_str()))
        }
        leaf => held.iter().any(|(m, v)| m == member && v == leaf),
    })
}

/// The id of what a create stored: the answer's (or a body-less answer's
/// request's) identity field, else that of the member wrapping its own type.
fn created_id(op: &Operation, stored: &Value) -> Option<String> {
    let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    if let Some(id) = stored.get(field).and_then(id_string) {
        return Some(id);
    }
    let t = op.resource_type.as_deref()?;
    let members = stored.as_object()?;
    let wrapped = members
        .iter()
        .find(|(member, value)| {
            value.is_object() && (member.as_str() == t || format!("{member}s") == t)
        })?
        .1;
    WRAPPED_IDS
        .iter()
        .find_map(|f| wrapped.get(*f).and_then(id_string))
}

/// The nearest resource before what `op` targets that its path names and the
/// session holds, other than the target: the capture of
/// `/captures/{id}/refunds`, by the parameter's name or the collection before it.
/// A name must be the path's owner's to count (see [`key_fits`]).
async fn parent_of(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    call: &OperationCall,
    own: Option<(&str, &str)>,
) -> Option<StoredResource> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let rt = state.rt;
    let segments = path_segments(path);
    let target = match op.kind {
        OperationKind::Command(Effect::Create) => None,
        _ => target_param(op),
    };
    for (at, segment) in segments.iter().enumerate().rev() {
        let Some(param) = segment_param(segment).filter(|p| target.as_deref() != Some(*p)) else {
            continue;
        };
        let Some(value) = call.path_params.get(param).and_then(id_string) else {
            continue;
        };
        let collection = at
            .checked_sub(1)
            .map(|before| segments[before])
            .filter(|s| segment_param(s).is_none());
        let names = [
            reference_prefix(param),
            Some(param).filter(|p| !mb_mcm::is_generic_id(p)),
            collection.and_then(|c| c.strip_suffix('s')),
            collection,
        ];
        let owner = owner_of_param(op, call, param).filter(|_| !id_like(param));
        for name in names.into_iter().flatten() {
            let Some(t) = entity_type(rt.index(), None, name) else {
                continue;
            };
            let direct = state
                .get(session, &t, &value)
                .await
                .filter(|hit| owner.as_ref().is_none_or(|o| key_fits(rt, session, hit, o)));
            let found = match direct {
                Some(hit) => Some(hit),
                None if id_like(param) => None,
                None => {
                    let mut fields = name_fields(param);
                    fields.extend(rt.index().path_fields(&t).iter().map(String::as_str));
                    by_path_fields(state, session, &t, &fields, &value, owner.as_ref()).await
                }
            };
            if let Some(hit) = found
                && own != Some((t.as_str(), hit.id.as_str()))
            {
                return Some(hit);
            }
        }
    }
    None
}

/// Adds `entry` to what `resource_type`'s `id` remembers, within the caps.
async fn remember(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    resource_type: &str,
    id: &str,
    entry: &Value,
) {
    let mut requests = requests_of(state, session, resource_type, id).await;
    requests.push(entry.clone());
    let excess = requests.len().saturating_sub(MAX_REMEMBERED);
    requests.drain(..excess);
    let doc = |requests: &[Value]| serde_json::json!({ "requests": requests });
    while requests.len() > 1 && doc(&requests).to_string().len() > MAX_BYTES {
        requests.remove(0);
    }
    state
        .upsert(
            session,
            StoredResource {
                resource_type: INTERNAL_TYPE.to_string(),
                id: memory_id(resource_type, id),
                data: doc(&requests),
            },
        )
        .await;
}

/// Takes from `id`'s memory the requests `shown`, and nothing a write added
/// since: this answer was not told of those.
async fn forget_shown(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    resource_type: &str,
    id: &str,
    shown: &[Value],
) {
    let requests = requests_of(state, session, resource_type, id).await;
    let left: Vec<Value> = requests
        .into_iter()
        .filter(|r| !shown.contains(r))
        .collect();
    let key = memory_id(resource_type, id);
    if left.is_empty() {
        state.delete(session, INTERNAL_TYPE, &key).await;
    } else {
        state
            .upsert(
                session,
                StoredResource {
                    resource_type: INTERNAL_TYPE.to_string(),
                    id: key,
                    data: serde_json::json!({ "requests": left }),
                },
            )
            .await;
    }
}

/// Whether an answer's effect changed that resource.
fn effect_applied(generated: &GeneratedResponse, resource_type: &str, id: &str) -> bool {
    generated.effect_outcomes.iter().any(|o| {
        o.rejected.is_none() && o.effect.resource_type == resource_type && o.effect.id == id
    })
}

/// After a 2xx write: remembers its request for the resource it targets, and for
/// the parent its path names when that differs, unless that resource already
/// shows it. The target is told when the answer said little or nothing:
///
/// - a create, when its answer is shorter than its read and does not hold what
///   the request said (see [`carries`]). One answering the read's own schema was
///   written with the request in front of it, as a regeneration would be, and
///   what that schema has no member for no read could show;
/// - an update, only when no answer replaced the target: a body-less answer, or
///   a faker's standing in for the LLM. One that did replace it was told what was
///   remembered, which is now done;
/// - any other action, unless an effect of its answer changed the target.
///
/// A write with a response body and no request body said what it did and asked
/// nothing that could be lost, and the LLM shown its target listed no change, so
/// it is not remembered for the target. The parent is different: an answer about
/// a new child says nothing to it, so a create is remembered for its parent even
/// with no body, the path being all there is (`POST /captures/{id}/refund`).
pub(super) async fn after_write(
    state: &mut StateAccess<'_>,
    rt: &MockRuntime,
    plan: &CallPlan<'_>,
    w: &Written<'_>,
) {
    let op = plan.op;
    let session = &plan.session;
    let OperationKind::Command(effect) = op.kind else {
        return;
    };
    let (true, Some(t)) = (applies(rt, plan), op.resource_type.as_deref()) else {
        return;
    };
    // Refused for another's key, or a new record with nowhere to go.
    if w.foreign_key || w.unstored || !(200..300).contains(&w.generated.status) {
        return;
    }
    let generated = w.generated;
    let request = w
        .call
        .body
        .as_ref()
        .filter(|b| !b.is_null() && b.as_object().is_none_or(|o| !o.is_empty()))
        .map(|b| scrubbed(rt, plan, b));
    let answered = generated.body.is_some() && !transient_fallback(generated);
    let telling = request.is_some() || generated.body.is_none();
    // A name that may be another's, with nothing held, is nobody's to remember.
    let acted_on = w
        .acted_on
        .filter(|_| w.held || guarded(rt, op, w.call).is_none());
    let mut target: Option<String> = None;
    match effect {
        Effect::Create => {
            // A body-less answer stores the request, which then holds itself.
            let stored = generated.body.as_ref().or(request.as_ref());
            let whole = answered && is_read_schema(rt, op, op.output_for_status(generated.status));
            let carried = match (&request, stored) {
                (Some(request), Some(stored)) => {
                    (answered || generated.body.is_none()) && carries(request, stored)
                }
                _ => false,
            };
            if telling && !whole && !carried {
                target = stored.and_then(|stored| created_id(op, stored));
            }
        }
        Effect::Update if generated.new_record => {}
        Effect::Update => {
            if answered {
                if let Some(id) = acted_on.filter(|_| !w.earlier.shown.is_empty()) {
                    forget_shown(state, session, t, id, &w.earlier.shown).await;
                }
            } else if telling {
                target = acted_on.map(str::to_string);
            }
        }
        Effect::Delete => {
            if let Some(id) = acted_on {
                state
                    .delete(session, INTERNAL_TYPE, &memory_id(t, id))
                    .await;
            }
        }
        Effect::Other => {
            if telling
                && let Some(id) = acted_on
                && !effect_applied(generated, t, id)
            {
                target = Some(id.to_string());
            }
        }
    }
    let own = match effect {
        Effect::Create => target.as_deref(),
        _ => w.acted_on,
    };
    let parent = parent_of(state, session, op, w.call, own.map(|id| (t, id)))
        .await
        .filter(|p| !effect_applied(generated, &p.resource_type, &p.id))
        .filter(|_| telling || effect == Effect::Create);
    if target.is_none() && parent.is_none() {
        return;
    }
    let mut entry = Map::new();
    entry.insert("operation".to_string(), Value::from(op.id.as_str()));
    entry.insert("path".to_string(), Value::from(called_path(op, w.call)));
    if let Some(request) = request {
        entry.insert("body".to_string(), within_cap(request));
    }
    entry.insert(
        "at".to_string(),
        Value::from(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
    );
    let entry = Value::Object(entry);
    if let Some(id) = target {
        remember(state, session, t, &id, &entry).await;
    }
    if let Some(parent) = parent {
        remember(state, session, &parent.resource_type, &parent.id, &entry).await;
    }
}

/// The path a call was made to. Its query is left out: it may carry credentials.
fn called_path(op: &Operation, call: &OperationCall) -> String {
    let Binding::Http { path, .. } = &op.binding else {
        return String::new();
    };
    fill_path(call, &path_segments(path)).map_or_else(|| path.clone(), |filled| filled.path)
}

/// What the identity of `held` says, over `answer`: a regeneration is of the same
/// resource, however the LLM wrote its id.
fn keeping_identity(rt: &MockRuntime, held: &StoredResource, answer: &Value) -> Value {
    let mut answer = answer.clone();
    let index = rt.index();
    let fields = ["id", "number"].into_iter().chain(
        index
            .identity_fields(&held.resource_type)
            .iter()
            .map(String::as_str),
    );
    if let Value::Object(members) = &mut answer {
        for field in fields {
            if let Some(value) = held.data.get(field) {
                members.insert(field.to_string(), value.clone());
            }
        }
    }
    answer
}

/// After a generated read that was told of remembered requests: when it is the
/// whole resource, its answer takes the stored copy's place and those requests
/// are done. A read with a query is one view, and leaves both as they are; its
/// repeat is the response cache's. A write landing during the generation leaves
/// them too: this answer did not know of it.
pub(super) async fn settle_read(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    target: (Option<&str>, Option<&StoredResource>),
    earlier: &Earlier,
    generated: &mut GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
    let session = &plan.session;
    let (target_id, target) = target;
    let (Some(t), Some(id)) = (op.resource_type.as_deref(), target_id) else {
        return;
    };
    if !reads_the_resource(rt, op) {
        seed_own_read(state, session, rt, op, call, target_id, generated).await;
        return;
    }
    let Some(held) = target else {
        seed_own_read(state, session, rt, op, call, target_id, generated).await;
        let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
        let own = generated
            .body
            .as_ref()
            .and_then(|b| b.get(field))
            .and_then(id_string);
        for key in [Some(id.to_string()), own].into_iter().flatten() {
            if state.get(session, t, &key).await.is_some() {
                forget_shown(state, session, t, id, &earlier.shown).await;
                break;
            }
        }
        return;
    };
    let Some(answer) = generated.body.as_ref() else {
        return;
    };
    let answer = keeping_identity(rt, held, answer);
    if !entity_fits(rt, t, &answer) {
        return;
    }
    let current = state.get(session, t, &held.id).await;
    if current.as_ref().map(|c| &c.data) != Some(&held.data) {
        return;
    }
    let resource = StoredResource {
        resource_type: t.to_string(),
        id: held.id.clone(),
        data: answer.clone(),
    };
    if !state.upsert(session, resource.clone()).await {
        return;
    }
    rt.collections().carry(session, held, &resource);
    generated.body = Some(answer);
    forget_shown(state, session, t, id, &earlier.shown).await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::HeaderMap;
    use mb_engine::{Engine, EngineConfig};
    use mb_llm::{Provider, ScriptedProvider};
    use mb_state::{MemoryStore, StateStore};
    use serde_json::json;

    use super::*;

    const SDL: &str = "
type Item { id: ID! title: String! }
type Query { item(id: ID!): Item }
type Mutation { createItem(title: String!): Item }
";
    const PROTO: &str = r#"
syntax = "proto3";
package items;
message Item { string id = 1; string title = 2; }
message GetItemRequest { string id = 1; }
message CreateItemRequest { string title = 1; }
service Items {
  rpc GetItem(GetItemRequest) returns (Item);
  rpc CreateItem(CreateItemRequest) returns (Item);
}
"#;

    /// Runs a write of `contract`'s first command and returns the store's memory.
    async fn memory_after_a_write(
        contract: &[u8],
        answer: Value,
    ) -> (Vec<StoredResource>, Binding) {
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer.to_string())]));
        let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(60)));
        let rt = MockRuntime::new(
            "m",
            Arc::new(mb_import::compile(contract).unwrap()),
            Arc::new(Engine::new(
                Some(provider as Arc<dyn Provider>),
                EngineConfig::default(),
            )),
            Arc::clone(&store),
        );
        let op = rt
            .bundle()
            .operations
            .iter()
            .find(|o| matches!(o.kind, OperationKind::Command(_)))
            .unwrap();
        let plan = plan(&rt, op, &HeaderMap::new(), Presented::default())
            .await
            .unwrap();
        let call = OperationCall {
            operation_id: op.id.clone(),
            body: Some(json!({"title": "never echoed", "name": "never echoed"})),
            ..OperationCall::default()
        };
        generate_once(&rt, &plan, &call).await.unwrap();
        let session = plan.session.clone();
        let memory = store.list(&session, INTERNAL_TYPE, 10).await.unwrap();
        (memory, op.binding.clone())
    }

    /// The memory is for HTTP-bound operations: gRPC, SOAP and GraphQL calls
    /// through the same pipeline remember nothing, so they generate as before.
    #[tokio::test]
    async fn only_http_operations_are_remembered() {
        let wsdl = format!(
            "{}/../../fixtures/wsdl/accounts-rpc.wsdl",
            env!("CARGO_MANIFEST_DIR")
        );
        let (memory, binding) =
            memory_after_a_write(PROTO.as_bytes(), json!({"id": "i1", "title": "x"})).await;
        assert!(matches!(binding, Binding::Grpc { .. }), "{binding:?}");
        assert!(memory.is_empty(), "{memory:?}");

        let (memory, binding) =
            memory_after_a_write(SDL.as_bytes(), json!({"id": "i1", "title": "x"})).await;
        assert!(matches!(binding, Binding::Graphql { .. }), "{binding:?}");
        assert!(memory.is_empty(), "{memory:?}");

        let (memory, binding) = memory_after_a_write(
            &std::fs::read(wsdl).unwrap(),
            json!({"id": "a1", "name": "x"}),
        )
        .await;
        assert!(matches!(binding, Binding::Soap { .. }), "{binding:?}");
        assert!(memory.is_empty(), "{memory:?}");
    }

    #[test]
    fn carries_compares_strings_anywhere_and_small_values_by_member() {
        let answer = json!({"id": "1", "customer": {"name": "Ann"}, "count": 2, "open": true});
        assert!(carries(&json!({"customerName": "Ann"}), &answer));
        assert!(carries(&json!({"count": 2, "open": true}), &answer));
        assert!(!carries(&json!({"quantity": 2}), &answer));
        assert!(!carries(&json!({"closed": true}), &answer));
        assert!(!carries(&json!({"customerName": "Bo"}), &answer));
        assert!(carries(&json!({"password": REDACTED}), &answer));
        assert!(carries(&json!({"note": "", "tag": null}), &answer));
        assert!(carries(&json!({"names": ["Ann", "1"]}), &answer));
        assert!(!carries(&json!({"names": ["Ann", "Bo"]}), &answer));
    }

    #[test]
    fn a_body_over_the_cap_is_cut_on_a_character_boundary() {
        let kept = json!({"note": "short"});
        assert_eq!(within_cap(kept.clone()), kept);
        let cut = within_cap(json!({"note": "é".repeat(MAX_BODY_BYTES)}));
        let text = cut.as_str().unwrap();
        assert!(text.ends_with("[truncated]") && text.len() <= MAX_BODY_BYTES + 16);
        let marker = within_cap(json!({"note": "y".repeat(MAX_PREVIEWED_BYTES)}));
        assert_eq!(marker["truncated"], true);
    }

    #[test]
    fn credentials_are_redacted_at_any_depth() {
        let mut body = json!({"a": {"api_key": "k", "b": [{"Password": "p", "c": 1}]}});
        redact_credentials(&mut body);
        assert_eq!(
            body,
            json!({"a": {"api_key": REDACTED, "b": [{"Password": REDACTED, "c": 1}]}})
        );
    }
}
