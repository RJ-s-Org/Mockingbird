//! Ids a generated answer gives to two records.
//!
//! A model writing one answer, or one answer after another, may give a record an id
//! another record already has: the next variant of another product under the id the
//! first had. Stored, the first record wins and the second is lost; served, the client
//! sends that id back and gets the first. Before an answer is stored or served, each
//! record below its root whose identity duplicates another record of the same type in
//! the answer, or one the session holds, gets a new id in the same shape, when the two
//! are different records: a member that names a record (its `name`, `title`, `handle`,
//! ...) differs between them, and, against a held record, the record the answer shows
//! it inside is another one than the held record refers to as its parent (a variant
//! inside one product, held as another product's). The same record shown twice keeps
//! its id, and so does a held one the answer only renames.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use super::*;

/// Members whose values name a record: two records of a type that differ in one are
/// different records.
const NAMING: [&str; 7] = ["name", "title", "handle", "login", "slug", "sku", "email"];

/// How many new ids are drawn for one record before it is left as it was.
const DRAWS: u64 = 8;

/// Gives each record below `body`'s root that reuses another record's id a new one
/// (see the module doc). `skip` is the record the call acts on, which keeps its id.
pub(super) async fn remint(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    body: &mut Value,
    schema: Option<&Value>,
    skip: Option<(&str, &str)>,
) {
    let mut found = Vec::new();
    collect(rt, (body, schema), 0, (&mut Vec::new(), None), &mut found);
    // (type, id) → the naming members of the first record seen with it, and what each
    // record shown again under it became.
    let mut seen: HashMap<(String, String), Map<String, Value>> = HashMap::new();
    let mut renamed: HashMap<(String, String, String), String> = HashMap::new();
    for Found {
        path,
        t,
        member,
        id,
        parent,
    } in found
    {
        if skip == Some((t.as_str(), id.as_str())) {
            continue;
        }
        let Some(object) = at_mut(body, &path) else {
            continue;
        };
        let names = naming(object);
        let key = (t.clone(), id.clone());
        let other = match seen.get(&key) {
            Some(first) => Some(first.clone()),
            None => state
                .get(session, &t, &id)
                .await
                .filter(|held| under_another(&held.data, parent.as_ref()))
                .map(|held| naming(&held.data)),
        };
        if !other.as_ref().is_some_and(|other| differs(other, &names)) {
            seen.entry(key).or_insert(names);
            continue;
        }
        let shown = (t.clone(), id.clone(), fingerprint(&names));
        let new_id = match renamed.get(&shown) {
            Some(new_id) => Some(new_id.clone()),
            None => fresh(state, session, (&t, &id), &shown.2, &seen).await,
        };
        let Some(new_id) = new_id else {
            continue;
        };
        if let Some(object) = at_mut(body, &path) {
            mb_mcm::set_member_at(object, &member, Value::String(new_id.clone()));
        }
        tracing::info!(mock = %rt.name, resource_type = %t, "a generated record reused another's id");
        renamed.insert(shown, new_id.clone());
        seen.entry((t, new_id)).or_insert(names);
    }
}

/// A record an answer shows below its root.
struct Found {
    path: Vec<Step>,
    /// Its type.
    t: String,
    /// Its identity member, and the id there.
    member: String,
    id: String,
    /// The record it is shown inside, as (type, id).
    parent: Option<(String, String)>,
}

/// Each object below the root with a record type and an identity (see [`Found`]).
fn collect<'a>(
    rt: &'a MockRuntime,
    (value, schema): (&Value, Option<&'a Value>),
    depth: usize,
    (path, parent): (&mut Vec<Step>, Option<&(String, String)>),
    out: &mut Vec<Found>,
) {
    if depth > MAX_ENTITY_DEPTH || out.len() >= MAX_ENTITIES {
        return;
    }
    let place = Place::of(rt, schema);
    match value {
        Value::Array(items) => {
            let item = place.parts.iter().find_map(|p| p.get("items"));
            for (i, value) in items.iter().enumerate() {
                path.push(Step::Index(i));
                collect(rt, (value, item), depth + 1, (path, parent), out);
                path.pop();
            }
        }
        Value::Object(fields) => {
            let typename = fields
                .get("__typename")
                .and_then(Value::as_str)
                .map(str::to_lowercase);
            let record = place.record_type(rt.index()).or(typename).and_then(|t| {
                let (member, id) = identity_members(rt, &t)
                    .into_iter()
                    .find_map(|m| Some((m, fields.get(m).and_then(Value::as_str)?)))?;
                Some((t, member.to_owned(), id.to_owned()))
            });
            if depth > 0
                && let Some((t, member, id)) = &record
            {
                out.push(Found {
                    path: path.clone(),
                    t: t.clone(),
                    member: member.clone(),
                    id: id.clone(),
                    parent: parent.cloned(),
                });
            }
            let this = record.map(|(t, _, id)| (t, id));
            let inner = this.as_ref().or(parent);
            for (field, value) in fields {
                if value.is_object() || value.is_array() {
                    path.push(Step::Key(field.clone()));
                    collect(
                        rt,
                        (value, place.property(field)),
                        depth + 1,
                        (path, inner),
                        out,
                    );
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

/// Whether `held` refers to a parent of `parent`'s type (a member named for the type,
/// or one whose `__typename` is it, holding an object with an `id`) other than
/// `parent` itself. False without a parent, or when `held` refers to none.
fn under_another(held: &Value, parent: Option<&(String, String)>) -> bool {
    let (Some((t, id)), Some(members)) = (parent, held.as_object()) else {
        return false;
    };
    members.iter().any(|(name, value)| {
        let typename = value
            .get("__typename")
            .and_then(Value::as_str)
            .map(str::to_lowercase);
        let of_type = name.to_lowercase() == *t || typename.as_deref() == Some(t.as_str());
        of_type
            && value
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|held_parent| held_parent != id)
    })
}

/// One step into a JSON value.
#[derive(Clone)]
enum Step {
    Key(String),
    Index(usize),
}

fn at_mut<'v>(value: &'v mut Value, path: &[Step]) -> Option<&'v mut Value> {
    path.iter().try_fold(value, |v, step| match step {
        Step::Key(k) => v.get_mut(k.as_str()),
        Step::Index(i) => v.get_mut(*i),
    })
}

/// The members of `record` that name it, with text values.
fn naming(record: &Value) -> Map<String, Value> {
    NAMING
        .iter()
        .filter_map(|m| {
            let value = record.get(*m)?;
            value.is_string().then(|| ((*m).to_owned(), value.clone()))
        })
        .collect()
}

/// Whether a member both name maps hold has different values.
fn differs(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    a.iter().any(|(k, v)| b.get(k).is_some_and(|w| w != v))
}

fn fingerprint(names: &Map<String, Value>) -> String {
    Value::Object(names.clone()).to_string()
}

/// A new id in `id`'s shape (its last run of digits redrawn, at the same length, else
/// a numbered suffix) that neither the session nor this answer gives a record of type
/// `t`; `None` when no draw is free.
async fn fresh(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (t, id): (&str, &str),
    names: &str,
    seen: &HashMap<(String, String), Map<String, Value>>,
) -> Option<String> {
    for salt in 0..DRAWS {
        let mut hasher = DefaultHasher::new();
        (t, id, names, salt).hash(&mut hasher);
        let candidate = reshaped(id, hasher.finish(), salt);
        if candidate == id || seen.contains_key(&(t.to_owned(), candidate.clone())) {
            continue;
        }
        if state.get(session, t, &candidate).await.is_none() {
            return Some(candidate);
        }
    }
    None
}

/// `id` with its last run of ASCII digits replaced by digits drawn from `draw`, at the
/// same length (no leading zero where it had none); without digits, `id-<salt + 2>`.
fn reshaped(id: &str, draw: u64, salt: u64) -> String {
    let bytes = id.as_bytes();
    let Some(end) = bytes.iter().rposition(u8::is_ascii_digit).map(|i| i + 1) else {
        return format!("{id}-{}", salt + 2);
    };
    let start = bytes[..end]
        .iter()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |i| i + 1);
    let mut digits = String::with_capacity(end - start);
    let mut x = draw;
    for i in 0..end - start {
        let low = if i == 0 && bytes[start] != b'0' { 1 } else { 0 };
        let d = low + (x % (10 - low));
        x = (x / 10) ^ x.rotate_left(17);
        digits.push(char::from(b'0' + u8::try_from(d).unwrap_or(0)));
    }
    format!("{}{}{}", &id[..start], digits, &id[end..])
}

#[cfg(test)]
mod tests {
    use super::reshaped;

    #[test]
    fn a_new_id_keeps_the_shape() {
        let id = "gid://shopify/ProductVariant/44123456789013";
        let new = reshaped(id, 987_654_321, 0);
        assert_ne!(new, id);
        assert!(new.starts_with("gid://shopify/ProductVariant/"), "{new}");
        assert_eq!(new.len(), id.len());
        assert_eq!(reshaped("cedar", 1, 0), "cedar-2");
    }
}
