//! Actions (step 5 of the plan): described transitions and changes on the held target. Not executed yet: every such call generates.

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
) -> Ran {
    Ran::Generate(None)
}
