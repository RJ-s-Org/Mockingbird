//! Scenarios: named, owner-authored adjustments to generated responses
//! (spec §7). `instructions` steer the LLM through the prompt; `set` is a
//! JSON merge patch applied to object bodies after generation: 2xx ones, and
//! those of the scenario's own forced status.

use serde_json::Value;

/// The scenario active for one generation.
#[derive(Clone, Debug, Default)]
pub struct ScenarioSpec {
    /// The scenario's name.
    pub name: String,
    /// Free-text instructions from the mock owner, added to the prompt.
    pub instructions: Option<String>,
    /// An RFC 7386 merge patch applied to a 2xx object body, and to the
    /// body of [`Self::status`].
    pub set: Option<Value>,
    /// The scenario's forced status: when a response answers it, `set`
    /// patches that body too (e.g. a 409 error variant).
    pub status: Option<u16>,
}

/// Apply `set` to `body` as an RFC 7386 JSON merge patch: objects merge
/// recursively, `null` removes a member, anything else replaces.
pub fn apply_set(body: &Value, set: &Value) -> Value {
    let mut patched = body.clone();
    json_patch::merge(&mut patched, set);
    patched
}
