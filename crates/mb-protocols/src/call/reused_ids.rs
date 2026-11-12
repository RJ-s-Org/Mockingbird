//! Ids a generated answer gives to two records.
//!
//! A model writing one answer, or one answer after another, may give a record an id
//! another record already has: the next variant of another product under the id the
//! first had. Stored, the first record wins and the second is lost; served, the client
//! sends that id back and gets the first. Before an answer is stored or served, a
//! record whose identity another record of its type has gets a new id in the same
//! shape, when the two are different records:
//!
//! - in the same answer, a member that names a record (its `name`, `title`, `handle`,
//!   ...) differs between them;
//! - against a record the session holds, the answer shows it inside another record than
//!   the held one refers to as its parent (a variant inside one product, held as
//!   another product's), the parent counted by the id it has after this; or, for the
//!   answer's root, the call names it by a member (`product(handle:)`) the held record
//!   has another value in.
//!
//! The record the call acts on, the same record shown twice, and a held one the answer
//! only renames keep their ids.
//!
//! What the session holds of each record that keeps a held id is handed back
//! ([`Shown`]), as it is read here anyway, and [`show_held`] has a read's answer show
//! it: below the answer's root, each such object takes the stored record's members its
//! place in the schema declares, each projected as declared (see
//! [`lists::projected`]) and only where the projected value validates against the
//! member's schema. The record's identity stays, and so do the generated members the
//! record lacks. A comment a create stored, generated again inside its issue with
//! another body and flags, shows the stored ones. The main list's items of a list
//! read are left to [`lists::overlay_stored`], which knows their parent.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use super::*;

/// Members whose values name a record: two records of a type that differ in one are
/// different records.
const NAMING: [&str; 7] = ["name", "title", "handle", "login", "slug", "sku", "email"];

/// Two texts: a (type, id), or a (member, value).
type Pair<'a> = (&'a str, &'a str);

/// The naming members and parent id of the first record an answer shows under an id.
type Seen = (Map<String, Value>, Option<String>);

/// The record another is shown inside: its type, id and path.
type Parent = (String, String, Vec<Step>);

/// How many new ids are drawn for one record before it is left as it was.
const DRAWS: u64 = 8;

/// A record of an answer that keeps an id the session holds, with the held record.
pub(super) struct Shown<'a> {
    path: Vec<Step>,
    /// Its identity member, and the id there.
    member: String,
    id: String,
    /// Its place's schema.
    schema: Option<&'a Value>,
    held: StoredResource,
}

/// Generated objects that take a held record's members, per answer.
const MAX_SHOWN: usize = 50;

/// Gives each record of `body` that reuses another record's id a new one (see the
/// module doc), and returns those that keep an id the session holds, with what it
/// holds. `skip` is the record the call acts on, which keeps its id; `named_by`
/// the member and value the call names its record by besides its id (GraphQL's
/// `product(handle:)`): the answer's root under a held id whose record has another
/// value there is another record.
pub(super) async fn remint<'a>(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &'a MockRuntime,
    body: &mut Value,
    schema: Option<&'a Value>,
    (skip, named_by): (Option<Pair<'_>>, Option<Pair<'_>>),
) -> Vec<Shown<'a>> {
    let mut found = Vec::new();
    collect(rt, (body, schema), 0, (&mut Vec::new(), None), &mut found);
    // (type, id) → the naming members and parent of the first record seen with it;
    // what each record shown again under an id became; the new id of the record at
    // each path.
    let mut seen: HashMap<(String, String), Seen> = HashMap::new();
    let mut renamed: HashMap<(String, String, String), String> = HashMap::new();
    let mut moved: HashMap<Vec<Step>, String> = HashMap::new();
    // The held record under each (type, id) read, and the records that keep one.
    let mut held_by: HashMap<(String, String), StoredResource> = HashMap::new();
    let mut shown = Vec::new();
    for Found {
        path,
        t,
        member,
        id,
        parent,
        schema,
    } in found
    {
        if skip == Some((t.as_str(), id.as_str())) {
            continue;
        }
        let Some(object) = at_mut(body, &path) else {
            continue;
        };
        let names = naming(object);
        // The record it is shown inside, by the id it now has.
        let parent = parent.map(|(pt, pid, ppath)| {
            let pid = moved.get(&ppath).cloned().unwrap_or(pid);
            (pt, pid)
        });
        let parent_id = parent.as_ref().map(|(_, id)| id.clone());
        let key = (t.clone(), id.clone());
        let another = match seen.get(&key) {
            Some((first, _)) => differs(first, &names),
            None => match state.get(session, &t, &id).await {
                None => false,
                Some(held) => {
                    let another = if path.is_empty() {
                        named_by.is_some_and(|(m, v)| {
                            held.data
                                .get(m)
                                .and_then(Value::as_str)
                                .is_some_and(|h| h != v)
                        })
                    } else {
                        under_another(&held.data, parent.as_ref())
                    };
                    held_by.insert(key.clone(), held);
                    another
                }
            },
        };
        if !another {
            if let Some(held) = held_by.get(&key).filter(|_| shown.len() < MAX_SHOWN) {
                shown.push(Shown {
                    path,
                    member,
                    id,
                    schema,
                    held: held.clone(),
                });
            }
            seen.entry(key).or_insert((names, parent_id));
            continue;
        }
        let shown = (
            t.clone(),
            id.clone(),
            format!(
                "{}{}",
                fingerprint(&names),
                parent_id.clone().unwrap_or_default()
            ),
        );
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
        moved.insert(path, new_id.clone());
        seen.entry((t, new_id)).or_insert((names, parent_id));
    }
    shown
}

/// Has each object of `body`, a read's answer, that [`remint`] found keeping a held id
/// show the held record's members (see the module doc): not the root, and with
/// `listed`, not the items of the answer's main list.
pub(super) fn show_held(rt: &MockRuntime, body: &mut Value, shown: &[Shown<'_>], listed: bool) {
    let bundle = rt.bundle();
    // Where the main list's items are: the body's own elements, or its list member's.
    let body_is_list = body.is_array();
    let list_key = mb_engine::consistency::list_member(body).map(str::to_owned);
    let main_item = |path: &[Step]| match path {
        [Step::Index(_)] => body_is_list,
        [Step::Key(key), Step::Index(_)] => list_key.as_deref() == Some(key.as_str()),
        _ => false,
    };
    for one in shown {
        if one.path.is_empty() || (listed && main_item(&one.path)) {
            continue;
        }
        let place = Place::of(rt, one.schema);
        let (Some(Value::Object(object)), Value::Object(stored)) =
            (at_mut(body, &one.path), &one.held.data)
        else {
            continue;
        };
        // An earlier overlay replaced it with another value.
        if object.get(&one.member).and_then(Value::as_str) != Some(one.id.as_str()) {
            continue;
        }
        for (name, value) in stored {
            if *name == one.member {
                continue;
            }
            let Some(schema) = place.property(name) else {
                continue;
            };
            let value = lists::projected(bundle, Some(schema), value, 0);
            if object.get(name) == Some(&value) {
                continue;
            }
            let fits = mb_engine::SchemaValidator::for_output(&bundle.with_reachable_defs(schema))
                .is_ok_and(|validator| validator.is_valid(&value));
            if fits {
                object.insert(name.clone(), value);
            }
        }
    }
}

/// A record an answer shows.
struct Found<'a> {
    path: Vec<Step>,
    /// Its type.
    t: String,
    /// Its identity member, and the id there.
    member: String,
    id: String,
    /// The record it is shown inside, as (type, id, path).
    parent: Option<Parent>,
    /// Its place's schema.
    schema: Option<&'a Value>,
}

/// Each object below the root with a record type and an identity (see [`Found`]).
fn collect<'a>(
    rt: &'a MockRuntime,
    (value, schema): (&Value, Option<&'a Value>),
    depth: usize,
    (path, parent): (&mut Vec<Step>, Option<&Parent>),
    out: &mut Vec<Found<'a>>,
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
            if let Some((t, member, id)) = &record {
                out.push(Found {
                    path: path.clone(),
                    t: t.clone(),
                    member: member.clone(),
                    id: id.clone(),
                    parent: parent.cloned(),
                    schema,
                });
            }
            let this = record.map(|(t, _, id)| (t, id, path.clone()));
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
#[derive(Clone, PartialEq, Eq, Hash)]
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
    seen: &HashMap<(String, String), Seen>,
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
