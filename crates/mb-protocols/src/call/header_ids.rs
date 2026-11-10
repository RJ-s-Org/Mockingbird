//! A create whose answer names its new record only in a response header: the answer
//! holds no record (no body, or one without the identity, `{threadIds}`), and the
//! contract declares a header carrying the id ([`mb_engine::headers::identity_header`]):
//! an id header (`Resource-ID`), or a `Location` whose collection has an item read one
//! segment under the create's path. The id is minted as the record's identity member
//! declares it, the request is stored as the record under it, and the header serves
//! the same id (a `Location` the item's URL), so a later read by it finds the record.
//! A create whose answer holds its id serves that id in the same header.

use mb_engine::consistency::property_schema;
use mb_engine::headers::IdentityHeaderKind;
use mb_engine::{Faker, GeneratedResponse, OperationCall, SchemaValidator};
use mb_mcm::{Binding, Operation};
use mb_state::SessionKey;
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde_json::{Map, Value, json};

use super::{StateAccess, id_string, records, request_values, type_members};
use crate::MockRuntime;

/// How many ids are drawn before giving up on one the session does not hold.
const ATTEMPTS: u32 = 8;

/// What seeds the ids a call mints: the call's [`mb_engine::SEED_HEADER`], else the
/// engine's faker seed, and the session the call is in. With a seed, each draw is
/// seeded by it mixed with the contract, the mock, the type, the member, what the
/// session is known by (the call it first minted in, and how many sessions first minted
/// in a call alike before it) and how many ids of that member it drew before
/// ([`crate::call::MintCounts`]): the same seed and the same calls mint the same ids in
/// a fresh runtime, whatever the sessions are named (a client often makes the name of
/// the time) and in whatever order sessions first minting in different calls start,
/// and every draw (a retry after a collision too) differs from the others of the
/// runtime. Concurrent calls of one session take their counts in the order they arrive,
/// so their ids may swap from one run to the next. Without a seed the draws are random,
/// as the engine's are.
#[derive(Clone, Copy)]
pub(super) struct MintSeed<'a> {
    seed: Option<u64>,
    session: &'a SessionKey,
    call: &'a OperationCall,
}

impl<'a> MintSeed<'a> {
    pub(super) fn new(rt: &MockRuntime, session: &'a SessionKey, call: &'a OperationCall) -> Self {
        Self {
            seed: mb_engine::seed::request_seed(call).or(rt.engine.faker_seed()),
            session,
            call,
        }
    }

    /// The session the ids are minted in.
    pub(super) fn session(self) -> &'a SessionKey {
        self.session
    }

    /// The seed of the session's next draw of type `t`'s member `field`, and how many it
    /// drew before; `None` without a seed (nothing is counted then).
    pub(super) fn next(self, rt: &MockRuntime, (t, field): (&str, &str)) -> Option<(u64, u64)> {
        let seed = self.seed?;
        let call = self.call;
        let first = || {
            let parts = mb_engine::seed::call_parts(call);
            mb_engine::seed::derive(
                0,
                &["first-mint", &call.operation_id],
                &[&parts[0], &parts[1], &parts[2]],
            )
        };
        let ((fingerprint, place), drawn) = rt.mint_counts().next(self.session, first, (t, field));
        let s = self.session;
        let labels = [
            rt.bundle().source_digest.as_str(),
            "mint",
            t,
            field,
            &s.workspace,
            &s.mock,
        ];
        let counts = [
            Value::from(fingerprint),
            Value::from(place),
            Value::from(drawn),
        ];
        let derived = mb_engine::seed::derive(seed, &labels, &[&counts[0], &counts[1], &counts[2]]);
        Some((derived, drawn))
    }

    /// The generator and faker of the next draw of an id of type `t`'s member `field`.
    fn draws(self, rt: &MockRuntime, (t, field): (&str, &str)) -> (StdRng, Faker) {
        match self.next(rt, (t, field)) {
            Some((derived, _)) => (
                StdRng::seed_from_u64(derived),
                Faker::default().with_now(mb_engine::seed::SEEDED_NOW),
            ),
            None => (rand::make_rng::<StdRng>(), Faker::default()),
        }
    }
}

/// Mints the id of the record `op`'s create (of type `t`) answered in a header, when
/// its answer names it only there; serves it in `generated.headers` and returns it, to
/// be stored with [`record`]. When the answer holds an id, the header serves that one
/// and `None` is returned. `None` (and nothing served) when the request asks for one
/// (a client-chosen id, or a path value naming the record), no header names the
/// record, or no fresh id can be drawn. An integer header on a record whose identity
/// is a string mints a string of digits, which the header serves as the integer.
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
        .filter(|v| !v.is_null())
        .and_then(id_string);
    // The answer names its record: the header serves that same id.
    if let Some(text) = answered_id {
        serve_answered(rt, (op, t, call), generated, page_url, &text);
        return None;
    }
    let named_by_path = identity
        .and_then(|i| i.path_param.as_deref())
        .is_some_and(|param| call.path_params.contains_key(param));
    if named_by_path
        || mb_engine::guards::asked_identity(rt.bundle(), op, call).is_some()
        || !matches!(&generated.body, None | Some(Value::Object(_)))
    {
        return None;
    }
    let header = records::identity_header(rt, op, generated.status)?;
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let location = header.kind == IdentityHeaderKind::Location;
    // A `Location` names the record only as an item of the create's own collection.
    if location && !of_own_collection(rt, t, path) {
        return None;
    }
    let (declared, held) = record_id_schema(rt, t, field);
    let string_identity = declared
        .as_ref()
        .and_then(|d| d.get("type"))
        .and_then(Value::as_str)
        == Some("string");
    let checks = IdChecks::new(rt, held.as_ref(), &header.schema, !location)?;
    let carried = request_values(op, call);
    let seed = MintSeed::new(rt, session, call);
    for _ in 0..ATTEMPTS {
        let Some(id) = draw(
            rt,
            (seed, t, field),
            (held.as_ref(), declared.as_ref()),
            &header.schema,
            string_identity,
            &checks,
        ) else {
            continue;
        };
        // A type whose identity has a pattern holds the id in that form, which the
        // header and the record must still hold.
        let drawn = id_string(&id).map(|text| (records::in_pattern(rt, t, &text), text));
        let (id, text) = match drawn {
            Some((Some(formed), _)) => {
                let held = Value::String(formed.clone());
                if !checks.header_holds(&formed) || !checks.identity_holds(&held) {
                    continue;
                }
                (held, formed)
            }
            Some((None, text)) => (id, text),
            None => continue,
        };
        if location && !url_safe(&text) {
            continue;
        }
        if carried.contains(&text) || state.get(session, t, &text).await.is_some() {
            continue;
        }
        let value = match header.kind {
            IdentityHeaderKind::Id => text.clone(),
            IdentityHeaderKind::Location => item_url(path, call, page_url, &text),
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

/// Serves the id `text` a create's answer holds in the header that names its record,
/// when the contract declares one: the id itself, or the item's URL in a `Location` of
/// the create's own collection. An id header takes only an id its schema holds: an
/// integer one an integer, a `format` or `pattern` one an id of that form.
fn serve_answered(
    rt: &MockRuntime,
    (op, t, call): (&Operation, &str, &OperationCall),
    generated: &mut GeneratedResponse,
    page_url: Option<&str>,
    text: &str,
) {
    let Some(header) = records::identity_header(rt, op, generated.status) else {
        return;
    };
    let Binding::Http { path, .. } = &op.binding else {
        return;
    };
    let value = match header.kind {
        IdentityHeaderKind::Id => {
            let holds = IdChecks::new(rt, None, &header.schema, true)
                .is_some_and(|checks| checks.header_holds(text));
            if !holds {
                return;
            }
            text.to_string()
        }
        IdentityHeaderKind::Location => {
            // The header holds a URL, which no id constrains beyond its type.
            if !of_own_collection(rt, t, path) || !url_safe(text) {
                return;
            }
            item_url(path, call, page_url, text)
        }
    };
    generated.headers.push((header.name, value));
}

/// Whether type `t`'s item read is one parameter segment under the create's `path`,
/// so a `Location` the create answers names an item of its own collection.
fn of_own_collection(rt: &MockRuntime, t: &str, path: &str) -> bool {
    item_read_path(rt, t)
        .as_deref()
        .is_some_and(|item| is_item_of(item, path))
}

/// The URL of the item `id` under the create's collection: the URL the call reached
/// the mock at when known, else its `path` filled with the call's path values.
fn item_url(path: &str, call: &OperationCall, page_url: Option<&str>, id: &str) -> String {
    let base = page_url.map_or_else(|| filled_path(path, call), str::to_string);
    format!("{}/{id}", base.trim_end_matches('/'))
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

/// The schema type `t`'s records declare their identity `field` with, and the one
/// every read of them holds it by: the record schema's or its read by id's, else the
/// first read's ([`super::exec::read_schemas`]: a type read only by its lists); every
/// read's member schema, together (an `allOf`), when they are more than one.
fn record_id_schema(rt: &MockRuntime, t: &str, field: &str) -> (Option<Value>, Option<Value>) {
    let index = rt.index();
    let bundle = rt.bundle();
    let read = index
        .detail_reads
        .get(t)
        .and_then(|i| bundle.operations.get(*i)?.success_output()?.schema.as_ref());
    let declared = [index.record_schemas.get(t), read]
        .into_iter()
        .flatten()
        .find_map(|schema| property_schema(bundle, schema, field));
    let mut all: Vec<Value> = declared.iter().cloned().collect();
    for schema in super::exec::read_schemas(rt, t) {
        if let Some(member) = property_schema(bundle, &schema, field)
            && !all.contains(&member)
        {
            all.push(member);
        }
    }
    let declared = declared.or_else(|| all.first().cloned());
    // A plain text id the contract types as an integer where a route names it is its
    // digits.
    if !all.is_empty() && all.iter().all(|s| plain_text(rt, s)) && named_as_integer(rt, t) {
        let digits = json!({"type": "string", "pattern": "^[1-9][0-9]{0,8}$"});
        return (Some(digits.clone()), Some(digits));
    }
    let held = match all.len() {
        0 | 1 => declared.clone(),
        _ => Some(json!({"allOf": all})),
    };
    (declared, held)
}

/// Whether `schema` is text and says nothing more of it: no `pattern`, `format`,
/// `enum`, `const` or length bound.
fn plain_text(rt: &MockRuntime, schema: &Value) -> bool {
    let parts = super::schema_parts(rt.bundle(), Some(schema)).0;
    let says = |key: &str| parts.iter().any(|p| p.get(key).is_some());
    parts
        .iter()
        .any(|p| p.get("type").and_then(Value::as_str) == Some("string"))
        && ![
            "pattern",
            "format",
            "enum",
            "const",
            "minLength",
            "maxLength",
        ]
        .iter()
        .any(|k| says(k))
}

/// Whether the path and query parameters named for type `t`'s records
/// (`employeeId` for `employees`) are integers wherever the contract declares one: at
/// least one, and none of another type.
fn named_as_integer(rt: &MockRuntime, t: &str) -> bool {
    let bundle = rt.bundle();
    let mut integers = 0;
    for op in &bundle.operations {
        for params in [&op.input.path, &op.input.query].into_iter().flatten() {
            let Some(declared) = params.get("properties").and_then(Value::as_object) else {
                continue;
            };
            for (name, schema) in declared {
                let named = super::reference_prefix(name).is_some_and(|prefix| {
                    super::relations::reference_types(Some(rt.index()), prefix)
                        .iter()
                        .any(|named| named == t)
                });
                if !named {
                    continue;
                }
                let types: Vec<&str> = super::schema_parts(bundle, Some(schema))
                    .0
                    .iter()
                    .filter_map(|p| p.get("type").and_then(Value::as_str))
                    .collect();
                match types.as_slice() {
                    ["integer"] => integers += 1,
                    _ => return false,
                }
            }
        }
    }
    integers > 0
}

/// The checks a minted id passes: as the header serves it, and as the record holds it.
struct IdChecks {
    /// The header's schema, formats asserted; `None` for a `Location`, whose value is
    /// the item's URL and not the id.
    header: Option<SchemaValidator>,
    /// Whether the header is an integer, so its value is read as one.
    header_integer: bool,
    /// The record's identity schema, when declared, formats asserted.
    identity: Option<SchemaValidator>,
}

impl IdChecks {
    /// The checks for an id held as `declared` and, when the header `serves_id` itself
    /// (not a `Location`), served in a header of `header` schema; `None` when a schema
    /// does not compile.
    fn new(
        rt: &MockRuntime,
        declared: Option<&Value>,
        header: &Value,
        serves_id: bool,
    ) -> Option<Self> {
        let compile = |schema: &Value| SchemaValidator::for_output(&rt.bundle().with_defs(schema));
        let header_integer = header.get("type").and_then(Value::as_str) == Some("integer");
        Some(Self {
            header: match serves_id {
                true => Some(compile(header).ok()?),
                false => None,
            },
            header_integer,
            identity: declared.map(compile).transpose().ok()?,
        })
    }

    /// Whether the header holds `text` as its value.
    fn header_holds(&self, text: &str) -> bool {
        let Some(header) = &self.header else {
            return true;
        };
        let value = match self.header_integer {
            true => match text.parse::<i64>() {
                Ok(n) => Value::from(n),
                Err(_) => return false,
            },
            false => Value::String(text.to_string()),
        };
        header.is_valid_output(&value)
    }

    /// Whether the record holds `id` as its identity.
    fn identity_holds(&self, id: &Value) -> bool {
        self.identity
            .as_ref()
            .is_none_or(|identity| identity.is_valid_output(id))
    }
}

/// An id drawn as the record declares its identity, else as the header declares
/// itself, else a plain string: the first both the header and the record hold. A
/// string identity holds a number drawn for an integer header as its digits, so a read
/// by the id the header serves finds the record.
fn draw(
    rt: &MockRuntime,
    (seed, t, field): (MintSeed<'_>, &str, &str),
    (strict, declared): (Option<&Value>, Option<&Value>),
    header: &Value,
    string_identity: bool,
    checks: &IdChecks,
) -> Option<Value> {
    let string = json!({"type": "string"});
    // The strictest first: what every read declares together, when that is more.
    let strict = strict.filter(|s| Some(*s) != declared);
    let schemas = [strict, declared, Some(header), Some(&string)];
    let (mut rng, faker) = seed.draws(rt, (t, field));
    schemas
        .into_iter()
        .flatten()
        .map(|schema| faker.fake(&rt.bundle().with_defs(schema), &mut rng))
        .filter_map(|v| {
            let text = id_string(&v).filter(|s| !s.is_empty())?;
            let held = match (&v, string_identity) {
                (Value::Number(_), true) => Value::String(text.clone()),
                _ => v,
            };
            (checks.header_holds(&text) && checks.identity_holds(&held)).then_some(held)
        })
        .next()
}

/// An id for a record of type `t` that holds none, drawn as [`mint`] draws one with no
/// header to serve it in: as the type's records declare their identity `field` (a
/// plain string where they do not), in the type's identity pattern where it has one.
/// `None` when the draw is not one the records hold (a pattern past the member's
/// `maxLength`). Whether the session holds it already is the caller's to check.
pub(super) fn draw_record_id(
    rt: &MockRuntime,
    seed: MintSeed<'_>,
    t: &str,
    field: &str,
) -> Option<Value> {
    let (declared, held) = record_id_schema(rt, t, field);
    let string = json!({"type": "string"});
    let string_identity = declared
        .as_ref()
        .and_then(|d| d.get("type"))
        .and_then(Value::as_str)
        == Some("string");
    let checks = IdChecks::new(rt, held.as_ref(), &string, false)?;
    let id = draw(
        rt,
        (seed, t, field),
        (held.as_ref(), declared.as_ref()),
        &string,
        string_identity,
        &checks,
    )?;
    match id_string(&id).and_then(|text| records::in_pattern(rt, t, &text)) {
        Some(formed) => {
            let held = Value::String(formed);
            checks.identity_holds(&held).then_some(held)
        }
        None => Some(id),
    }
}

/// An id for a nested item whose identity member's schema is `declared` (a line item's
/// `id`), drawn as [`draw_record_id`] draws one: as declared, else a plain string, and
/// valid as declared. `None` when no draw is. Seeded as type `t`'s member `field`.
pub(super) fn draw_id_as(
    rt: &MockRuntime,
    (seed, t, field): (MintSeed<'_>, &str, &str),
    declared: &Value,
) -> Option<Value> {
    let string = json!({"type": "string"});
    let string_identity = declared.get("type").and_then(Value::as_str) == Some("string");
    let checks = IdChecks::new(rt, Some(declared), &string, false)?;
    draw(
        rt,
        (seed, t, field),
        (None, Some(declared)),
        &string,
        string_identity,
        &checks,
    )
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
