//! References a single-record read asks to be expanded, and parts the contract returns
//! only when asked for ([`super::query_roles`]).
//!
//! A **reference member** of a record holds the id of another record:
//! - by its schema: a choice (`anyOf`/`oneOf`) of an id string and an object whose
//!   definition a resource type is read as (Stripe's PaymentIntent `customer`:
//!   `anyOf: [string, $ref customer, $ref deleted_customer]`);
//! - by its name: a member holding a string, named after a resource type (`customer`)
//!   or as its id (`customer_id`), whose value names a record the session holds.
//!
//! The record keeps the id. A read whose embedding parameter names the member
//! (`expand[]=customer`, or a dotted path through expanded records,
//! `expand[]=latest_invoice.payment_intent`) is answered the referenced record in its
//! place, as the session holds it, shaped to its type's schema ([`expand`]). Expansion
//! is a view: what is stored is never changed by it, and a read that does not ask for a
//! reference by schema answers its id. A reference by schema (at the top of the record)
//! to a record the session does not hold yet, nor deleted, is built as a first read
//! builds one: the read generates, the record it generated in the member's place is
//! stored under the id the parent names ([`take_built`]), else one the faker draws
//! ([`build_unheld`]), and every later read and expansion answers that one. A record
//! the session holds always wins over one an answer generated. A dotted step, or a
//! reference by name, to a record the session does not hold stays its id.
//!
//! A dotted path also goes through embedded parts and arrays: through a list embed's
//! items (`line_items.data.price.product`, each item's price's product), and an array
//! of references by schema (an invoice's `discounts`, each an id or the discount) has
//! each id the session holds a record under expanded in its place.
//!
//! An **includable part** is a member the contract says is returned only when the read
//! asks for it ([`includable_parts`]): its description says so ("This field is not
//! included by default"), or the record's schema lists it among its `x-expandableFields`
//! and it is an optional paged list (Stripe's Checkout Session `line_items`: an object
//! of `data`, `has_more` and `url` that the session does not require). A read that does
//! not ask for one leaves it out, as it leaves out other embedded parts.

use std::collections::HashSet;

use mb_engine::consistency::{declared_members, property_schema, required_members};
use mb_mcm::Operation;
use mb_state::StoredResource;
use serde_json::Value;

use super::{
    MockRuntime, OperationCall, SessionKey, StateAccess, entity_type, holds_a_string, id_string,
    query_roles, reference_prefix, reference_types, schema_parts, type_check,
};

/// How many steps a dotted expansion follows, at most (Stripe's own limit).
const MAX_EXPAND_DEPTH: usize = 4;

/// The types member `member` of `schema` refers to by its schema: those whose
/// definitions are the object branches of a choice that also admits an id string. Empty
/// when it is no such choice.
fn schema_reference(rt: &MockRuntime, schema: &Value, member: &str) -> Vec<String> {
    property_schema(rt.bundle(), schema, member).map_or_else(Vec::new, |p| choice_types(rt, &p))
}

/// The types `property` refers to: those whose definitions are the object branches of
/// a choice that also admits an id string. Empty when it is no such choice.
fn choice_types(rt: &MockRuntime, property: &Value) -> Vec<String> {
    let bundle = rt.bundle();
    let (parts, defs) = schema_parts(bundle, Some(property));
    let choice = parts
        .iter()
        .any(|p| p.get("anyOf").or_else(|| p.get("oneOf")).is_some());
    if !choice || !holds_a_string(bundle, property) {
        return Vec::new();
    }
    let index = rt.index();
    let mut types: Vec<String> = Vec::new();
    for def in defs {
        if let Some(t) = index.types_by_def.get(def)
            && !types.contains(t)
        {
            types.push(t.clone());
        }
    }
    types
}

/// The types member `member` of `schema` refers to by its name, when the schema has it
/// hold a string: `<t>_id`/`<t>Id`'s, else the type named `<t>`; only types a read
/// names by path.
fn named_reference(rt: &MockRuntime, schema: &Value, member: &str) -> Vec<String> {
    let bundle = rt.bundle();
    if !property_schema(bundle, schema, member).is_some_and(|p| holds_a_string(bundle, &p)) {
        return Vec::new();
    }
    let index = rt.index();
    let types = match reference_prefix(member) {
        Some(prefix) => reference_types(Some(index), prefix),
        None => entity_type(index, None, member).into_iter().collect(),
    };
    types
        .into_iter()
        .filter(|t| index.detail_reads.contains_key(t))
        .collect()
}

/// A reference member's types, and whether its schema makes it one (else its name).
fn reference(rt: &MockRuntime, schema: &Value, member: &str) -> Option<(Vec<String>, bool)> {
    let by_schema = schema_reference(rt, schema, member);
    if !by_schema.is_empty() {
        return Some((by_schema, true));
    }
    let named = named_reference(rt, schema, member);
    (!named.is_empty()).then_some((named, false))
}

/// The success schema of `op`.
fn answer_schema(op: &Operation) -> Option<&Value> {
    op.success_output().and_then(|o| o.schema.as_ref())
}

/// Whether `member` of the answer of `op` is a reference member (see the module docs).
pub(super) fn is_reference(rt: &MockRuntime, op: &Operation, member: &str) -> bool {
    answer_schema(op).is_some_and(|schema| reference(rt, schema, member).is_some())
}

/// The item schema of `schema`, when it is an array.
fn array_item(bundle: &mb_mcm::Bundle, schema: &Value) -> Option<Value> {
    schema_parts(bundle, Some(schema))
        .0
        .iter()
        .find_map(|p| p.get("items").cloned())
}

/// Whether `member` of the answer of `op` is an array of references by schema (an
/// invoice's `discounts`: each an id or the discount).
pub(super) fn is_reference_array(rt: &MockRuntime, op: &Operation, member: &str) -> bool {
    answer_schema(op)
        .and_then(|schema| property_schema(rt.bundle(), schema, member))
        .and_then(|property| array_item(rt.bundle(), &property))
        .is_some_and(|item| !choice_types(rt, &item).is_empty())
}

/// The member a record of type `t` is keyed by: its detail read's identity, else `id`.
fn identity_field(rt: &MockRuntime, t: &str) -> String {
    rt.index()
        .detail_reads
        .get(t)
        .and_then(|i| rt.bundle().operations.get(*i)?.identity.as_ref())
        .map_or_else(|| "id".to_string(), |identity| identity.field.clone())
}

/// The schema a record of type `t` is answered with by its read.
fn record_schema(rt: &MockRuntime, t: &str) -> Option<Value> {
    let (read, shape) = type_check(rt, t)?;
    shape.cloned().or_else(|| answer_schema(read).cloned())
}

/// Phrases of a member's description saying the member is returned only when asked for.
const INCLUDABLE_PHRASES: &[&str] = &["not included by default"];

/// Phrases that say so together with the word "expand".
const EXPANDED_PHRASES: &[&str] = &[
    "omitted unless",
    "only included",
    "only returned",
    "only present",
    "unless expanded",
    "when expanded",
    "if expanded",
];

/// Whether `description` says its member is returned only when the read asks for it.
fn says_includable(description: &str) -> bool {
    let text = description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    INCLUDABLE_PHRASES.iter().any(|p| text.contains(p))
        || (text.contains("expand") && EXPANDED_PHRASES.iter().any(|p| text.contains(p)))
}

/// Whether `schema` is a paged list: an object holding an array member, and a
/// `has_more` flag or a `url`/`next` to page it by.
fn paged_list(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
    let typed = |s: &Value, name: &str| match s.get("type") {
        Some(Value::String(t)) => t == name,
        Some(Value::Array(types)) => types.iter().any(|t| t == name),
        _ => false,
    };
    schema_parts(bundle, Some(schema)).0.iter().any(|part| {
        let Some(properties) = part.get("properties").and_then(Value::as_object) else {
            return false;
        };
        let array = properties.values().any(|member| {
            schema_parts(bundle, Some(member))
                .0
                .iter()
                .any(|p| typed(p, "array"))
        });
        array
            && ["has_more", "hasMore", "url", "next", "next_page"]
                .iter()
                .any(|k| properties.contains_key(*k))
    })
}

/// The members of `op`'s answer the contract returns only when a read asks for them
/// (see the module docs). The read must have an embedding parameter for them to be left
/// out ([`query_roles::embeds`]).
pub(super) fn includable_parts(rt: &MockRuntime, op: &Operation) -> Vec<String> {
    let Some(schema) = answer_schema(op) else {
        return Vec::new();
    };
    let bundle = rt.bundle();
    let Some(declared) = declared_members(bundle, schema) else {
        return Vec::new();
    };
    let required: HashSet<String> = required_members(bundle, schema).into_iter().collect();
    let expandable: HashSet<&str> = schema_parts(bundle, Some(schema))
        .0
        .iter()
        .filter_map(|p| p.get("x-expandableFields").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut out: Vec<String> = declared
        .into_iter()
        .filter(|member| !required.contains(member))
        .filter(|member| {
            let Some(property) = property_schema(bundle, schema, member) else {
                return false;
            };
            let described = schema_parts(bundle, Some(&property))
                .0
                .iter()
                .chain(std::iter::once(&&property))
                .filter_map(|p| p.get("description").and_then(Value::as_str))
                .any(says_includable);
            described
                || (expandable.contains(member.as_str())
                    && paged_list(bundle, &property)
                    && schema_reference(rt, schema, member).is_empty())
        })
        .collect();
    out.sort();
    out
}

/// The asked references of `call` (by schema, at the top of the answer) that `data`,
/// the held record it reads, names by an id the session holds no record of: the read
/// generates them ([`take_built`]).
pub(super) async fn unbuilt(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, call): (&Operation, &OperationCall),
    data: &Value,
) -> Vec<String> {
    let rt = state.rt;
    let embeds = query_roles::embeds(rt, op, call);
    let Some(schema) = answer_schema(op) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for member in &embeds.references {
        let Some(id) = data.get(member).and_then(Value::as_str) else {
            continue;
        };
        let types = schema_reference(rt, schema, member);
        if types.is_empty() {
            continue;
        }
        if held(state, session, &types, id).await.is_some() {
            continue;
        }
        // One the session deleted is no record to build.
        let mut deleted = false;
        if state.tracks_deleted {
            for t in &types {
                deleted |= super::deleted::mention_is_deleted(state, session, t, id).await;
            }
        }
        if !deleted {
            out.push(member.clone());
        }
    }
    out
}

/// Builds, for each reference `parent` (the held record a read of `op` generated over)
/// names that the session still holds no record of once the read's answer was taken
/// ([`take_built`]), a record of its type as the faker draws its schema, under that id:
/// a read whose answer held no record there does not leave the next one to generate
/// again, and every read after it agrees.
pub(super) async fn build_unheld(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, call): (&Operation, &OperationCall),
    parent: &Value,
) {
    let rt = state.rt;
    let Some(schema) = answer_schema(op) else {
        return;
    };
    for member in unbuilt(state, session, (op, call), parent).await {
        let (Some(id), Some(t)) = (
            parent.get(&member).and_then(Value::as_str),
            schema_reference(rt, schema, &member).into_iter().next(),
        ) else {
            continue;
        };
        let Some(record_schema) = record_schema(rt, &t) else {
            continue;
        };
        let mut data = rt.engine.fake(rt.bundle(), &record_schema);
        let Value::Object(members) = &mut data else {
            continue;
        };
        members.insert(identity_field(rt, &t), Value::String(id.to_string()));
        let record = StoredResource {
            resource_type: t,
            id: id.to_string(),
            data,
        };
        state.insert_seeded(session, record).await;
    }
}

/// The record of one of `types` the session holds under `id`.
async fn held(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    types: &[String],
    id: &str,
) -> Option<StoredResource> {
    for t in types {
        if let Some(found) = state.get(session, t, id).await {
            return Some(found);
        }
    }
    None
}

/// The records `body`, a generated answer of single-record read `call` of `op`, holds
/// in its references by schema, each keyed by the id `stored` (the record the read was
/// generated over) names there, or by its own when the read held no record; `body` then
/// holds each one's id in its place, as the record is stored. A held record that names
/// none there keeps none: what the answer generated is only shown. Only for a read that
/// has an embedding parameter.
pub(super) fn take_built(
    rt: &MockRuntime,
    (op, call): (&Operation, &OperationCall),
    stored: Option<&Value>,
    body: &mut Value,
) -> Vec<StoredResource> {
    let mut out = Vec::new();
    if !query_roles::embeds(rt, op, call).embedding {
        return out;
    }
    let (Some(schema), Value::Object(members)) = (answer_schema(op), body) else {
        return out;
    };
    let names: Vec<String> = members
        .iter()
        .filter(|(_, value)| value.is_object())
        .map(|(name, _)| name.clone())
        .collect();
    for name in names {
        let Some(t) = schema_reference(rt, schema, &name).into_iter().next() else {
            continue;
        };
        let field = identity_field(rt, &t);
        let Some(Value::Object(record)) = members.get_mut(&name) else {
            continue;
        };
        let id = match stored {
            Some(stored) => stored
                .get(&name)
                .and_then(Value::as_str)
                .map(str::to_string),
            None => record.get(&field).and_then(id_string),
        };
        let Some(id) = id else {
            continue;
        };
        record.insert(field, Value::String(id.clone()));
        let data = Value::Object(std::mem::take(record));
        members.insert(name, Value::String(id.clone()));
        out.push(StoredResource {
            resource_type: t,
            id,
            data,
        });
    }
    out
}

/// `record`, a held record of type `t`, as an expansion answers it: the members its
/// type's schema declares, without the parts its type's read embeds only when asked.
fn shaped(rt: &MockRuntime, t: &str, record: &StoredResource) -> Value {
    let mut data = record.data.clone();
    let bundle = rt.bundle();
    let Value::Object(members) = &mut data else {
        return data;
    };
    if let Some(schema) = record_schema(rt, t)
        && let Some(declared) = declared_members(bundle, &schema)
        && !declared.is_empty()
    {
        members.retain(|name, _| declared.contains(name));
    }
    if let Some(read) = rt
        .index()
        .detail_reads
        .get(t)
        .and_then(|i| bundle.operations.get(*i))
    {
        for part in query_roles::offered(rt, read) {
            members.remove(&part);
        }
    }
    data
}

/// A JSON pointer token for `name`.
fn token(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

/// Expands in `body`, the answer of single-record read `call` of `op`, each reference
/// the read asks to be expanded, by the record the session holds (see the module docs).
/// A reference to a record the session does not hold stays its id. A reference by
/// schema the read does not ask for is its id, though the record held an object there.
pub(super) async fn expand(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, call): (&Operation, &OperationCall),
    body: &mut Value,
) {
    let rt = state.rt;
    let embeds = query_roles::embeds(rt, op, call);
    if !embeds.embedding {
        return;
    }
    let Some(top) = answer_schema(op).cloned() else {
        return;
    };
    if let Value::Object(members) = body {
        for (name, value) in members.iter_mut() {
            if embeds.references.contains(name) || !value.is_object() {
                continue;
            }
            let Some(t) = schema_reference(rt, &top, name).into_iter().next() else {
                continue;
            };
            if let Some(id) = value.get(identity_field(rt, &t)).and_then(id_string) {
                *value = Value::String(id);
            }
        }
    }
    for path in &embeds.paths {
        let steps: Vec<&String> = path.iter().take(MAX_EXPAND_DEPTH).collect();
        // The places the path reached: a pointer into `body`, the schema of the object
        // there, and the steps taken to it. A step through an array goes on in each of
        // its items (a list embed's `data`).
        let mut reached: Vec<(String, Value, usize)> = vec![(String::new(), top.clone(), 0)];
        while let Some((at, schema, taken)) = reached.pop() {
            let Some(step) = steps.get(taken) else {
                continue;
            };
            let pointer = format!("{at}/{}", token(step));
            let property = property_schema(rt.bundle(), &schema, step);
            let next = taken + 1;
            match body.pointer(&pointer) {
                Some(Value::Array(items)) => {
                    let count = items.len();
                    let Some(item) = property.and_then(|p| array_item(rt.bundle(), &p)) else {
                        continue;
                    };
                    let types = choice_types(rt, &item);
                    for at in 0..count {
                        let pointer = format!("{pointer}/{at}");
                        if types.is_empty() {
                            reached.push((pointer, item.clone(), next));
                        } else if let Some(schema) =
                            expand_at(state, session, body, &pointer, &types).await
                        {
                            reached.push((pointer, schema, next));
                        }
                    }
                }
                Some(_) => match reference(rt, &schema, step) {
                    Some((types, _)) => {
                        if let Some(schema) =
                            expand_at(state, session, body, &pointer, &types).await
                        {
                            reached.push((pointer, schema, next));
                        }
                    }
                    // An embedded part (a list object, an item's object) is followed into.
                    None => {
                        if let Some(property) = property
                            && body.pointer(&pointer).is_some_and(Value::is_object)
                        {
                            reached.push((pointer, property, next));
                        }
                    }
                },
                None => {}
            }
        }
    }
}

/// Expands the reference at `pointer` in `body`, of one of `types`: an id is the record
/// the session holds under it, an object (expanded by an earlier path, or held so) is
/// followed by its type. The schema of the record there, to follow on; `None` when the
/// session holds no record under the id.
async fn expand_at(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    body: &mut Value,
    pointer: &str,
    types: &[String],
) -> Option<Value> {
    let rt = state.rt;
    let t = match body.pointer(pointer) {
        Some(Value::String(id)) => {
            let id = id.clone();
            let record = held(state, session, types, &id).await?;
            let t = record.resource_type.clone();
            let view = shaped(rt, &t, &record);
            if let Some(slot) = body.pointer_mut(pointer) {
                *slot = view;
            }
            t
        }
        Some(Value::Object(_)) => types.first()?.clone(),
        _ => return None,
    };
    record_schema(rt, &t)
}

#[cfg(test)]
mod tests {
    use super::says_includable;

    #[test]
    fn descriptions_say_what_is_returned_only_when_asked() {
        for text in [
            "The line items purchased by the customer. This field is not included by \
             default. To include it in the response, expand the field.",
            "The card's CVC. For security reasons, this is only available for virtual \
             cards, and will be omitted unless you explicitly request it with the \
             `expand` parameter.",
        ] {
            assert!(says_includable(text), "{text}");
        }
        for text in [
            "The line items purchased by the customer.",
            "ID of the Customer this PaymentIntent belongs to, if one exists.",
            "Only returned when the card is active.",
        ] {
            assert!(!says_includable(text), "{text}");
        }
    }
}
