//! Records held inside their parent: a type whose described `parent.embedded` names the
//! parent's array member its records live in (an incident's `incident_updates`) has no
//! records of its own. A read, update or delete of one by its own route finds the held
//! parent whose array holds an item with the target's id (the parent the path names,
//! when its `parent.path_param` is in the path), and acts on that item: a write changes
//! or removes it in place and stores the parent ([`super::Write::Within`]), never a
//! record of the child's type.
//!
//! Items a parent holds that have no read of their own (Checkout's `line_items`) are
//! listed at the parent's route for them from the held parent ([`sub_list`]).

use mb_engine::GeneratedResponse;
use mb_mcm::{Binding, HttpMethod};
use mb_state::StoredResource;
use serde_json::Value;
use tracing::Instrument;

use super::Held;
use super::list::item_type;
use crate::call::{
    CallPlan, MAX_SCAN, OperationCall, StateAccess, id_string, records, state_lists, state_span,
    write_memory,
};

/// A record held in its parent's array member.
pub(super) struct Embedded {
    /// The parent holding it, as stored.
    pub(super) parent: StoredResource,
    /// The (dotted) array member of the parent.
    member: String,
    /// Its place in the array.
    at: usize,
    /// The item, as a record of its own type.
    pub(super) child: StoredResource,
}

impl Embedded {
    /// The parent's record with the item replaced by `item`.
    pub(super) fn with(&self, item: Value) -> Value {
        self.replaced(Some(item))
    }

    /// The parent's record with the item removed.
    pub(super) fn without(&self) -> Value {
        self.replaced(None)
    }

    fn replaced(&self, item: Option<Value>) -> Value {
        let mut data = self.parent.data.clone();
        let mut items = mb_mcm::member_at(&data, &self.member)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        match item {
            Some(item) if self.at < items.len() => items[self.at] = item,
            None if self.at < items.len() => {
                items.remove(self.at);
            }
            _ => {}
        }
        mb_mcm::set_member_at(&mut data, &self.member, Value::Array(items));
        data
    }
}

/// The call's target held in its parent's array member, when the session does not
/// hold it as a record of its own ([`find`]) and no write the parent does not show yet
/// is remembered for the parent.
pub(super) async fn current(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    held: &Held<'_>,
) -> Option<Embedded> {
    let (None, Some(id)) = (held.target, held.target_id) else {
        return None;
    };
    let found = find(state, plan, call, id).instrument(state_span()).await?;
    let parent = &found.parent;
    let behind = write_memory::remembers(state, &plan.session, &parent.resource_type, &parent.id)
        .instrument(state_span())
        .await;
    (!behind).then_some(found)
}

/// The record `id` of the call's type held in a parent's array member, when the type
/// describes `parent.embedded` and one held parent holds it: the parent the path names,
/// else the one held parent of the type holding an item with that id (none when more
/// than one does).
async fn find(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
    id: &str,
) -> Option<Embedded> {
    let rt = state.rt;
    let op = plan.op;
    let t = item_type(rt, op)?;
    let parent = rt.described().type_desc(t)?.parent.as_ref()?;
    let member = parent.embedded.as_deref()?;
    let field = records::identity_member(rt, op, t);
    // The parent the path names, else every held one of its type.
    let named = parent
        .path_param
        .as_ref()
        .and_then(|param| call.path_params.get(param))
        .and_then(id_string);
    let parents = match &named {
        Some(pid) => state
            .get(&plan.session, &parent.r#type, pid)
            .await
            .into_iter()
            .collect(),
        None => state.list(&plan.session, &parent.r#type, MAX_SCAN).await,
    };
    let mut holding = parents.into_iter().filter_map(|held| {
        let items = mb_mcm::member_at(&held.data, member)?.as_array()?;
        let at = items.iter().position(|item| {
            mb_mcm::member_at(item, field)
                .and_then(id_string)
                .is_some_and(|own| own == id)
        })?;
        let child = StoredResource {
            resource_type: t.to_owned(),
            id: id.to_owned(),
            data: items[at].clone(),
        };
        Some(Embedded {
            parent: held,
            member: member.to_owned(),
            at,
            child,
        })
    });
    let found = holding.next()?;
    holding.next().is_none().then_some(found)
}

/// A list of the items a held parent holds in a member, served at a route one literal
/// segment past the parent's read by id, named for the member
/// (`GET /v1/checkout/sessions/{session}/line_items` beside the session's `line_items`,
/// an array or a list object's `data`), when its items have no records of their own:
/// the parent's items, each projected to the list's item, paged as a list from state
/// is. `None` when the route is no such list, or the session holds no such parent or
/// no such member in it.
pub(super) async fn sub_list(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Option<GeneratedResponse> {
    let rt = state.rt;
    let op = plan.op;
    let bundle = rt.bundle();
    let Binding::Http {
        method: HttpMethod::Get,
        path,
    } = &op.binding
    else {
        return None;
    };
    let (parent_path, member) = path.rsplit_once('/')?;
    if member.is_empty() || member.contains('{') {
        return None;
    }
    let param = parent_path
        .rsplit('/')
        .next()?
        .strip_prefix('{')?
        .strip_suffix('}')?;
    let types: Vec<&String> = rt
        .index()
        .detail_reads
        .iter()
        .filter(|(_, at)| {
            bundle.operations.get(**at).is_some_and(
                |read| matches!(&read.binding, Binding::Http { path, .. } if path == parent_path),
            )
        })
        .map(|(t, _)| t)
        .collect();
    let [t] = types.as_slice() else {
        return None;
    };
    let id = call.path_params.get(param).and_then(id_string)?;
    let parent = records::held_by_key(state, &plan.session, t, &id)
        .instrument(state_span())
        .await?;
    let items = match parent.data.get(member)? {
        Value::Array(items) => items,
        Value::Object(list) => list.get("data")?.as_array()?,
        _ => return None,
    };
    let shape = state_lists::Shape::of(rt, op)?;
    let envelope = super::list::envelope(rt, plan, call, &shape)?;
    let listed: Vec<(Value, Value)> = items
        .iter()
        .map(|item| {
            let item = crate::call::lists::projected(bundle, Some(&shape.items), item, 0);
            (item, Value::Null)
        })
        .collect();
    let page = (&shape, &envelope);
    state_lists::page(
        state,
        (plan, call),
        (member, None),
        page,
        listed,
        (None, true),
    )
    .await
    .ok()
}
