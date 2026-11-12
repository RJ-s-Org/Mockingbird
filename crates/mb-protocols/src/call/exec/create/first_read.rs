//! The first read of a record nothing stored yet ([`first_read`]): built as a create
//! builds its record, the model writing only what it alone can, then answered as the
//! read answers a held record. The answer goes on as a generated read's does: stored as
//! its record where a generated read's would be, the records it holds of other types
//! too, and cached.
//!
//! - The record holds what the path says: the read's identity at the member the path
//!   names it by, typed as that member is (`number` of `/issues/{issue_number}`), the
//!   described parent and scope members ([`crate::call::path_set`]), and, for an object
//!   member named for a path parameter (`owner` of `/repos/{owner}/{repo}`), the record
//!   the session holds under that value, else an object holding it at the member the
//!   object's own type is read by (`owner.login`, the path member of its read by
//!   `/users/{username}`).
//! - Then what a create's record starts with ([`super::fresh`]): its lifecycle's
//!   `initial`, what its schema decides (a `const`, a `default`), its state-like
//!   members' start, and an id for each id member (a reference to a record the session
//!   holds, else minted in the form the examples or the held records show).
//! - Everything else it declares is [`mb_engine::Engine::fill_read`]'s: the model
//!   writes its free text, links, booleans and the enums, scalars and ids the persona
//!   names, asked with a schema of those alone; the rest is filled locally (the
//!   request's values, who the caller is, the faker, every scalar in its format: a
//!   moment a timestamp or, where it may be null, null; a link the faker would draw
//!   null where it may be, else the contract's example, else a URL).
//!
//! It applies to a read whose answer is large ([`LOCAL_FILL_TOKENS`]); a smaller one
//! the model still writes whole, with every related record and earlier answer in view.
//! A read with a query (a view of the record) generates as before, unless it is a
//! single-record read whose query only says how the record is shown (its language, a
//! projection: [`crate::call::query_roles`]) and asks for nothing embedded; so does a
//! seeded one, an offline one, a GraphQL selection, one whose answer holds the record in an
//! envelope, the repeat of one the session's answers cache, and one of a record with
//! writes remembered for it, which the generated read settles. When the model writes
//! none of what it was asked, or the read's answer does not render the record, the read
//! generates too, its fill's cost counted.

use mb_engine::consistency::property_schema;
use mb_engine::{FillContext, GeneratedResponse};
use mb_mcm::{Binding, OperationKind};
use mb_state::StoredResource;
use serde_json::{Map, Value};
use tracing::Instrument;

use super::{
    GAP_DEPTH, Shape, Unsent, caller, embed, fill_into, fill_schema, fits, fresh, gaps,
    holds_object, initial, members_of, non_null, preset, record_def, settle_filled, shaped,
    typed_as, unsent_reference, write_only,
};
use crate::call::{CallPlan, OperationCall, StateAccess, path_set, serve_held, state_span};

/// The estimated output tokens ([`mb_engine::budget::estimated_tokens`]) of a read's
/// answer from which its first read is filled locally: below it, the model writes the
/// record whole, as cheaply, and with every related record and earlier answer in view.
/// GitHub's `issues/get` or a TMDB movie is about 2,000; a record of a dozen scalars,
/// about 100.
const LOCAL_FILL_TOKENS: f64 = 1000.0;

/// What [`first_read`] made of a read.
pub(in crate::call::exec) enum FirstRead {
    /// The record answered as the read answers it: the generated read's answer, stored
    /// and cached as one is.
    Built(Box<GeneratedResponse>),
    /// No record (the model wrote none of what it was asked, or the read's answer does
    /// not render it): the read generates, counting what the fill cost.
    Spent(mb_llm::Usage),
}

/// The record a read of `plan`'s operation answers when the session holds none for it
/// (see the module docs); `None` when the read generates instead, with nothing spent.
pub(in crate::call::exec) async fn first_read(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    response_schema: Option<&Value>,
) -> Option<FirstRead> {
    let rt = state.rt;
    let op = plan.op;
    if !rt.consistency
        || !plan.engine.has_provider()
        || !matches!(op.kind, OperationKind::Query)
        || !matches!(op.binding, Binding::Http { .. })
        || !(call.query.is_empty() || representation_only(state.rt, op, call))
        || mb_engine::seed::request_seed(call).is_some()
        || response_schema.is_some()
    {
        return None;
    }
    let t = op.resource_type.as_deref()?;
    let identity = op.identity.as_ref().filter(|i| i.record.is_none())?;
    let target = crate::call::target_id(op, call)?;
    let bundle = rt.bundle();
    let answer = op.success_output()?.schema.as_ref()?;
    // A repeat of a read the session's answers cache (one a generated read does not
    // store as its record) is the cache's.
    let session = &plan.session;
    if let Some(cache) = rt.state.response_cache() {
        let key = format!(
            "{}:{}",
            bundle.source_digest,
            crate::call::cache_key(bundle, op, call, response_schema)
        );
        if cache.get(session, &key).is_some() {
            return None;
        }
    }
    // A record the model writes whole in a few hundred tokens it still writes whole,
    // seeing all a generated read sees.
    let doc = bundle.with_reachable_defs(answer);
    if mb_engine::budget::estimated_tokens(&doc).is_none_or(|tokens| tokens < LOCAL_FILL_TOKENS) {
        return None;
    }
    // Writes remembered for the record (an action on it, a create telling it) are shown
    // to the generated read, which settles them.
    let earlier = crate::call::write_memory::recall(state, plan, call, Some(&target), false)
        .instrument(state_span())
        .await;
    if !earlier.shown.is_empty() {
        return None;
    }
    // The read's own answer first: its members' schemas are the ones it renders by.
    let mut shape = Shape::of(rt, op, t, None);
    let own = non_null(bundle, answer);
    shape.schemas.retain(|s| *s != own);
    shape.schemas.insert(0, own);
    shape.members = members_of(rt, &shape.schemas);
    let field = identity.field.as_str();

    // What the path says of the record.
    let mut data = Map::new();
    path_owners(state, (plan, call), &shape, &mut data)
        .instrument(state_span())
        .await;
    let from_path = path_set(rt, (op, t), call);
    shape.path_members(rt, call, &from_path, &mut data);
    // A path value set where the record holds another shape is fill's to write.
    data.retain(|name, value| {
        shape
            .members
            .iter()
            .find(|(m, _)| m == name)
            .is_none_or(|(_, member)| shaped(rt, (member, false), value))
    });
    // The path's value is the member it names the record by; a path naming it by no
    // member of its own (`{repo}` of a repository keyed by `id`) leaves the key alone.
    let named = identity.path_member();
    if (named != field || crate::call::path_names_identity(identity))
        && !data.contains_key(named)
        && let Some((_, member)) = shape.members.iter().find(|(m, _)| m == named)
    {
        let value = typed_as(bundle, member, &Value::String(target.clone()));
        if fits(rt, member, &value) {
            data.insert(named.to_owned(), value);
        }
    }
    shape.path_scoped(rt, &from_path, &mut data);

    // What a create's record starts with.
    if let Some((member, value)) = initial(rt, t, None)
        && !data.contains_key(&member)
    {
        data.insert(member, value);
    }
    let mut record = Value::Object(data);
    let fits_member = |schema: &Value, value: &Value| fits(rt, schema, value);
    for schema in &shape.schemas {
        preset(bundle, (schema, &fits_member), &mut record, &[], GAP_DEPTH);
    }
    let Value::Object(mut data) = record else {
        return None;
    };
    let starts = fresh::start_states(rt, (op, t), &shape.members, &mut data, &[]);
    let seed = crate::call::header_ids::MintSeed::new(rt, &plan.session, call);
    fresh::mint_ids(
        state,
        (plan, seed),
        (t, field),
        (&shape.members, None),
        &mut data,
        &starts.asked,
    )
    .instrument(state_span())
    .await;

    // The rest is fill's: what the reads require, then every other member it declares.
    let record = Value::Object(data);
    let mut missing = gaps(
        rt,
        (t, &shape.schemas),
        &record,
        &[],
        Unsent::All(None, None),
    );
    let required: Vec<String> = missing.iter().map(|(name, _)| name.clone()).collect();
    let Value::Object(mut data) = record else {
        return None;
    };
    for (name, member) in &shape.members {
        if !data.contains_key(name)
            && !missing.iter().any(|(m, _)| m == name)
            && !unsent_reference(rt, None, name, member)
        {
            missing.push((name.clone(), member.clone()));
        }
    }
    missing.retain(|(_, member)| !write_only(bundle, member));
    let mut usage = mb_llm::Usage::default();
    let mut called = false;
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|(name, _)| name.clone()).collect();
        let caller = caller(state, plan).instrument(state_span()).await;
        let filled = plan
            .engine
            .fill_read(
                bundle,
                &fill_schema(&missing, &required),
                &data,
                &names,
                FillContext {
                    op,
                    call,
                    sent: None,
                    caller: &caller,
                    states: &starts.asked,
                },
            )
            .await;
        usage = filled.usage;
        called = filled.called;
        // The model gave nothing: a record of the faker's alone is never stored.
        if filled.fell_back {
            return Some(FirstRead::Spent(usage));
        }
        fill_into(&mut data, filled.members);
        settle_filled(
            rt,
            (field, &shape.schemas),
            (&missing, &required),
            &mut data,
        );
    }
    let built = StoredResource {
        resource_type: t.to_owned(),
        id: target.clone(),
        data: Value::Object(data),
    };
    // Answered as the read answers a held record, or not at all: the read generates.
    let Some(mut answer) = serve_held(rt, op, &built, None, None) else {
        tracing::debug!(mock = %rt.name, operation = %op.id, "a first read's record does not render");
        return Some(FirstRead::Spent(usage));
    };
    answer.usage = usage;
    if called {
        answer.source = mb_engine::Source::Llm;
    }
    tracing::info!(
        mock = %rt.name,
        operation = %op.id,
        model_called = called,
        prompt_tokens = usage.prompt_tokens,
        completion_tokens = usage.completion_tokens,
        "answered a first read by local fill"
    );
    Some(FirstRead::Built(Box::new(answer)))
}

/// Whether `call`'s query only says how a single-record read represents its record
/// ([`crate::call::query_roles`]: its language, a projection), the record read being the
/// one its path names whatever the query: none asks for members to be embedded, which a
/// generated read builds and keeps with the record.
fn representation_only(
    rt: &crate::MockRuntime,
    op: &mb_mcm::Operation,
    call: &OperationCall,
) -> bool {
    crate::call::query_roles::single_record_read(rt, op)
        && crate::call::query_roles::embeds(rt, op, call)
            .asked
            .is_empty()
}

/// Sets on `record` each object member named for a path parameter of `call` (`owner`
/// of `{owner}`, letters compared without case or separators, a reference's `_id`
/// aside) that nothing set: the record the session holds under the parameter's value,
/// projected to the member's schema, else an object holding the value at the member the
/// object's own type is read by (its read by id's path member: `login` of
/// `/users/{username}`), where that fits.
async fn path_owners(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    shape: &Shape,
    record: &mut Map<String, Value>,
) {
    let rt = state.rt;
    let bundle = rt.bundle();
    let squeezed = |name: &str| {
        name.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase()
    };
    for (param, value) in &call.path_params {
        let named = squeezed(crate::call::reference_prefix(param).unwrap_or(param));
        let Some((name, member)) = shape
            .members
            .iter()
            .find(|(name, _)| squeezed(name) == named || squeezed(name) == squeezed(param))
        else {
            continue;
        };
        if record.contains_key(name) || !holds_object(bundle, &non_null(bundle, member)) {
            continue;
        }
        if let Some(held) = embed(state, &plan.session, (name, member), value).await {
            record.insert(name.clone(), held);
            continue;
        }
        let object = non_null(bundle, member);
        let index = rt.index();
        let Some(key) =
            crate::call::entity_type(index, record_def(bundle, member).as_deref(), name)
                .and_then(|u| index.detail_reads.get(&u).copied())
                .and_then(|i| bundle.operations.get(i))
                .and_then(|read| read.identity.as_ref())
                .map(|identity| identity.path_member().to_owned())
        else {
            continue;
        };
        let Some(schema) = property_schema(bundle, &object, &key) else {
            continue;
        };
        let typed = typed_as(bundle, &schema, value);
        if fits(rt, &schema, &typed) {
            let mut held = Map::new();
            held.insert(key, typed);
            record.insert(name.clone(), Value::Object(held));
        }
    }
}
