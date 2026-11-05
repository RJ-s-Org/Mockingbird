//! A create whose answer names its new record only in a response header: the answer
//! holds no record (no body, or one without the identity, `{threadIds}`), and the
//! contract declares a header carrying the id ([`mb_engine::headers::identity_header`]):
//! an id header (`Resource-ID`), or a `Location` whose collection has an item read one
//! segment under the create's path. The id is minted as the record's identity member
//! declares it, the request is stored as the record under it, and the header serves
//! the same id (a `Location` the item's URL), so a later read by it finds the record.

use mb_engine::consistency::property_schema;
use mb_engine::headers::{IdentityHeaderKind, identity_header};
use mb_engine::{Faker, GeneratedResponse, OperationCall};
use mb_mcm::{Binding, Operation};
use mb_state::SessionKey;
use serde_json::{Map, Value, json};

use super::{StateAccess, id_string, request_values, type_members};
use crate::MockRuntime;

/// How many ids are drawn before giving up on one the session does not hold.
const ATTEMPTS: u32 = 8;

/// Mints the id of the record `op`'s create (of type `t`) answered in a header, when
/// its answer names it only there; serves it in `generated.headers` and returns it, to
/// be stored with [`record`]. `None` (and nothing served) when the answer holds an id,
/// the request asks for one (a client-chosen id, or a path value naming the record),
/// no header names the record, or no fresh id can be drawn.
pub(super) async fn mint(
    rt: &MockRuntime,
    state: &mut StateAccess<'_>,
    (session, op, t, call): (&SessionKey, &Operation, &str, &OperationCall),
    generated: &mut GeneratedResponse,
    page_url: Option<&str>,
) -> Option<Value> {
    let identity = op.identity.as_ref();
    let field = identity.map_or("id", |i| i.field.as_str());
    let answered_id = generated
        .body
        .as_ref()
        .and_then(|b| mb_mcm::member_at(b, field))
        .is_some_and(|v| !v.is_null());
    let named_by_path = identity
        .and_then(|i| i.path_param.as_deref())
        .is_some_and(|param| call.path_params.contains_key(param));
    if answered_id
        || named_by_path
        || mb_engine::guards::asked_identity(rt.bundle(), op, call).is_some()
        || !matches!(&generated.body, None | Some(Value::Object(_)))
    {
        return None;
    }
    let header = identity_header(op, generated.status)?;
    let item_read = item_read_path(rt, t);
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let location = header.kind == IdentityHeaderKind::Location;
    // A `Location` names the record only as an item of the create's own collection.
    if location
        && !item_read
            .as_deref()
            .is_some_and(|item| is_item_of(item, path))
    {
        return None;
    }
    let declared = record_id_schema(rt, t, field);
    let header_integer = header.schema.get("type").and_then(Value::as_str) == Some("integer");
    let carried = request_values(op, call);
    for _ in 0..ATTEMPTS {
        let Some(id) = draw(rt, declared.as_ref(), &header.schema, header_integer) else {
            continue;
        };
        let Some(text) = id_string(&id) else {
            continue;
        };
        if location && !url_safe(&text) {
            continue;
        }
        if carried.contains(&text) || state.get(session, t, &text).await.is_some() {
            continue;
        }
        let value = match header.kind {
            IdentityHeaderKind::Id => text.clone(),
            IdentityHeaderKind::Location => {
                let base = page_url.map_or_else(|| filled_path(path, call), str::to_string);
                format!("{}/{text}", base.trim_end_matches('/'))
            }
        };
        tracing::debug!(
            mock = %rt.name,
            operation = %op.id,
            header = %header.name,
            id = %text,
            "create answered its id in a header: one was minted"
        );
        generated.headers.push((header.name, value));
        return Some(id);
    }
    None
}

/// The record a create named in a header stores: the members of `request` the type
/// declares (all of them when it declares none), with `id` as its identity `field`.
pub(super) fn record(
    rt: &MockRuntime,
    t: &str,
    request: Option<&Value>,
    field: &str,
    id: Value,
) -> Value {
    let declared = type_members(rt, t);
    let kept: Map<String, Value> = request
        .and_then(Value::as_object)
        .map(|sent| {
            sent.iter()
                .filter(|(name, _)| declared.is_empty() || declared.contains(*name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut record = Value::Object(kept);
    mb_mcm::set_member_at(&mut record, field, id);
    record
}

/// The path of type `t`'s item read, when it is an HTTP route.
fn item_read_path(rt: &MockRuntime, t: &str) -> Option<String> {
    let read = rt
        .bundle()
        .operations
        .get(*rt.index().detail_reads.get(t)?)?;
    match &read.binding {
        Binding::Http { path, .. } => Some(path.clone()),
        _ => None,
    }
}

/// Whether `item` is one parameter segment under `collection`: `/boards/{boardId}`
/// under `/boards`.
fn is_item_of(item: &str, collection: &str) -> bool {
    item.strip_prefix(collection.trim_end_matches('/'))
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|segment| {
            segment.starts_with('{') && segment.ends_with('}') && !segment.contains('/')
        })
}

/// The schema type `t`'s records declare their identity `field` with.
fn record_id_schema(rt: &MockRuntime, t: &str, field: &str) -> Option<Value> {
    let index = rt.index();
    let bundle = rt.bundle();
    let read = index
        .detail_reads
        .get(t)
        .and_then(|i| bundle.operations.get(*i)?.success_output()?.schema.as_ref());
    [index.record_schemas.get(t), read]
        .into_iter()
        .flatten()
        .find_map(|schema| property_schema(bundle, schema, field))
}

/// An id drawn as the record declares its identity, else as the header declares
/// itself; an integer header takes only an integer.
fn draw(
    rt: &MockRuntime,
    declared: Option<&Value>,
    header: &Value,
    header_integer: bool,
) -> Option<Value> {
    let fits = |v: &Value| match header_integer {
        true => v.is_i64() || v.is_u64(),
        false => v.is_string() || v.is_number(),
    };
    let string = json!({"type": "string"});
    let schemas = [declared, header_integer.then_some(header), Some(&string)];
    let mut rng = rand::rng();
    let faker = Faker::default();
    schemas
        .into_iter()
        .flatten()
        .map(|schema| faker.fake(&rt.bundle().with_defs(schema), &mut rng))
        .find(|v| fits(v) && id_string(v).is_some_and(|s| !s.is_empty()))
}

/// Whether `id` can stand in a URL path segment as it is.
fn url_safe(id: &str) -> bool {
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
}

/// `path` with the call's path values in place of its parameters.
fn filled_path(path: &str, call: &OperationCall) -> String {
    call.path_params
        .iter()
        .fold(path.to_string(), |path, (name, value)| {
            let value = id_string(value).unwrap_or_default();
            path.replace(&format!("{{{name}}}"), &value)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_route_is_one_parameter_segment_under_the_collection() {
        assert!(is_item_of("/boards/{boardId}", "/boards"));
        assert!(is_item_of("/a/{x}/b/{id}", "/a/{x}/b"));
        assert!(!is_item_of("/boards/{boardId}/cards", "/boards"));
        assert!(!is_item_of("/boards/main", "/boards"));
        assert!(!is_item_of("/cards/{id}", "/boards"));
    }

    #[test]
    fn only_plain_ids_are_url_safe() {
        assert!(url_safe("abc-12_x.y~"));
        assert!(!url_safe("a b"));
        assert!(!url_safe("a/b"));
    }
}
