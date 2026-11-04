//! A request body that is a JSON Patch (RFC 6902): an array of operations, each an
//! object naming its `op` and `path`.
//!
//! An update whose body is one changes what its operations change. Applied to the
//! stored target, it says which members those are, so the answer settles on them as on
//! the members a JSON body names (see `reflect`). A `test` operation asserts a value: one
//! that fails against the stored target fails the whole patch, as the API would.

use serde_json::Value;

/// What a JSON Patch body does to a held record.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    /// The body is no JSON Patch.
    NotAPatch,
    /// Every operation applied: the record after them.
    Applied(Value),
    /// A `test` operation failed against a value the record holds.
    TestFailed,
    /// An operation could not be applied for another reason (a path the record does not
    /// hold, which a partial stored copy may lack): nothing is decided.
    Unknown,
}

/// The operations of `body`, when it is a JSON Patch.
fn operations(body: &Value) -> Option<Vec<json_patch::PatchOperation>> {
    let items = body.as_array().filter(|items| !items.is_empty())?;
    let shaped = items.iter().all(|item| {
        item.get("op").is_some_and(Value::is_string)
            && item.get("path").is_some_and(Value::is_string)
    });
    if !shaped {
        return None;
    }
    serde_json::from_value::<json_patch::Patch>(body.clone())
        .ok()
        .map(|patch| patch.0)
}

/// Whether `body` is a JSON Patch.
#[must_use]
pub fn is_json_patch(body: &Value) -> bool {
    operations(body).is_some()
}

/// `body`'s operations applied in order to `held` (see [`Applied`]). A `test` fails only
/// at a path `held` holds at that point: a stored copy lacking a member decides nothing.
#[must_use]
pub fn apply(held: &Value, body: &Value) -> Applied {
    let Some(ops) = operations(body) else {
        return Applied::NotAPatch;
    };
    let mut doc = held.clone();
    for op in &ops {
        let before = doc.clone();
        match json_patch::patch(&mut doc, std::slice::from_ref(op)) {
            Ok(()) => {}
            Err(e) => {
                let held_there = before.pointer(op.path().as_str()).is_some();
                return match (&e.kind, op) {
                    (
                        json_patch::PatchErrorKind::TestFailed,
                        json_patch::PatchOperation::Test(_),
                    ) if held_there => Applied::TestFailed,
                    _ => Applied::Unknown,
                };
            }
        }
    }
    Applied::Applied(doc)
}

/// The top-level members of `after` that `before` does not hold the same: what a patch
/// that turned `before` into `after` names, as a JSON body would.
#[must_use]
pub fn changed_members(before: &Value, after: &Value) -> Value {
    let Value::Object(after) = after else {
        return Value::Object(serde_json::Map::new());
    };
    Value::Object(
        after
            .iter()
            .filter(|(k, v)| before.get(k.as_str()) != Some(*v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_patch_applies_and_its_tests_decide() {
        let held = json!({"id": "n1", "text": "t", "rev": "1-a"});
        let ok = json!([
            {"op": "test", "path": "/rev", "value": "1-a"},
            {"op": "replace", "path": "/text", "value": "u"},
        ]);
        let Applied::Applied(after) = apply(&held, &ok) else {
            panic!("applies");
        };
        assert_eq!(changed_members(&held, &after), json!({"text": "u"}));
        let stale = json!([{"op": "test", "path": "/rev", "value": "0-z"}]);
        assert_eq!(apply(&held, &stale), Applied::TestFailed);
        // A test after a replace tests the replaced value.
        let after_replace = json!([
            {"op": "replace", "path": "/rev", "value": "2-b"},
            {"op": "test", "path": "/rev", "value": "2-b"},
        ]);
        assert!(matches!(apply(&held, &after_replace), Applied::Applied(_)));
        // A member the copy lacks decides nothing.
        let missing = json!([{"op": "test", "path": "/etag", "value": "x"}]);
        assert_eq!(apply(&held, &missing), Applied::Unknown);
        assert_eq!(apply(&held, &json!({"text": "u"})), Applied::NotAPatch);
        assert_eq!(apply(&held, &json!([{"name": "x"}])), Applied::NotAPatch);
        assert_eq!(apply(&held, &json!([])), Applied::NotAPatch);
    }
}
