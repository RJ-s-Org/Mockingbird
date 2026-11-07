//! Records held inside their parent: a type whose described `parent.embedded` names the
//! parent's array member its records live in (an incident's `incident_updates`) has no
//! records of its own. A read, update or delete of one by its own route finds the held
//! parent whose array holds an item with the target's id (the parent the path names,
//! when its `parent.path_param` is in the path), and acts on that item: a write changes
//! or removes it in place and stores the parent ([`super::Write::Within`]), never a
//! record of the child's type.

use mb_state::StoredResource;
use serde_json::Value;

use super::list::item_type;
use crate::call::{CallPlan, MAX_SCAN, OperationCall, StateAccess, id_string, records};

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

/// The record `id` of the call's type held in a parent's array member, when the type
/// describes `parent.embedded` and a held parent holds it.
pub(super) async fn find(
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
    parents.into_iter().find_map(|held| {
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
    })
}
