//! Creates (step 2 of the plan): the request's record, its minted id, its initial state and the members fill writes. Not executed yet: every such call generates.

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
