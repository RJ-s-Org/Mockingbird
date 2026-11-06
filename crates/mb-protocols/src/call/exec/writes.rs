//! Updates, replaces and deletes (step 3 of the plan). Not executed yet: every such call
//! generates.

use mb_mcm::world::OpKind;
use serde_json::Value;

use super::{Held, Ran};
use crate::call::{CallPlan, OperationCall, StateAccess};

/// Whether the executor takes these calls.
pub(super) const EXECUTES: bool = false;

/// Executes the call.
pub(super) async fn run(
    _state: &mut StateAccess<'_>,
    _plan: &CallPlan<'_>,
    _call: &OperationCall,
    _held: Held<'_>,
    _response_schema: Option<&Value>,
    _kind: OpKind,
) -> Ran {
    Ran::Generate(None)
}
