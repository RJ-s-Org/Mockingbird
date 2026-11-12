//! What a query asks of a single-record read.
//!
//! A read by path of one record (the route its type is read by, ending at the type's
//! identity parameter, see [`single_record_read`]) names its record by the path alone:
//! whatever its query sends, it reads, stores and is answered from the record under the
//! path's id. Its query only selects how that record is represented, each parameter by
//! the role the contract gives it ([`role`]):
//! - [`Role::Language`]: the language the record's text is written in. A read in another
//!   language than the record was stored in is its variant ([`super::variants`]).
//! - [`Role::Embed`]: response members (sub-resources) the read asks to be included
//!   (`append_to_response=credits,videos`, `expand[]=customer`, `embed=author`). They
//!   are built once, by the first read that asks for them, and kept with the record. A
//!   read that asks for them is answered the kept ones; a read that does not leave out
//!   those the contract offers ([`Embeds::offered`]).
//! - [`Role::Projection`]: anything else (`fields`, `include_image_language`). It is
//!   applied to the stored record where it is described (a fields parameter) and is
//!   otherwise ignored. It never makes another record.
//!
//! Collection reads, searches and reads whose query names the record (`?id=`, with no
//! identity in the path) are not single-record reads: their query is theirs.

use std::collections::HashSet;

use mb_engine::consistency::{declared_members, property_schema, required_members};
use mb_mcm::{Binding, Operation, OperationKind};
use mb_state::StoredResource;
use serde_json::Value;

use super::{MockRuntime, OperationCall, SessionKey, StateAccess, targets_by_path};

/// The role a single-record read's query parameter has (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Role {
    /// It selects the language of the record's text.
    Language,
    /// It asks for response members to be included: those it names, of `offered` (the
    /// members the contract says it can name; empty when it says none, as `expand`
    /// says none: the members are then those a request names).
    Embed {
        /// Optional response members the contract says the parameter names.
        offered: Vec<String>,
    },
    /// It narrows or filters how the record is shown, or does nothing to it.
    Projection,
}

/// Words saying a parameter selects a language.
const LANGUAGE_WORDS: &[&str] = &["language", "lang", "locale"];

/// Words saying a parameter adds members to the answer.
const EMBED_WORDS: &[&str] = &[
    "append", "appends", "appended", "expand", "expands", "expanded", "embed", "embeds", "embedded",
];

/// Words saying a parameter includes or filters something; with one in its name, a
/// parameter whose name says language selects what is included
/// (`include_image_language`), not the answer's language.
const INCLUDE_WORDS: &[&str] = &[
    "include",
    "includes",
    "including",
    "filter",
    "filters",
    "only",
    "with",
];

/// What [`INCLUDE_WORDS`] are in a description ("only" and "with" are not: "Language
/// with which to display" selects a language).
const DESCRIBED_INCLUDE_WORDS: &[&str] = &["include", "includes", "including", "filter", "filters"];

/// Whether `op` reads one record by its path: a query of its type's read by path
/// (the type's detail read), whose identity parameter selects that record. A list, a
/// search, and a read naming its record by a query value (`?id=`) are none.
pub(super) fn single_record_read(rt: &MockRuntime, op: &Operation) -> bool {
    if !matches!(op.kind, OperationKind::Query) || !matches!(op.binding, Binding::Http { .. }) {
        return false;
    }
    let Some(t) = op.resource_type.as_deref() else {
        return false;
    };
    let index = rt.index();
    let is_detail = match (index.detail_reads.get(t), index.operation(&op.id)) {
        (Some(detail), Some(own)) => *detail == own,
        _ => false,
    };
    is_detail && op.identity.as_ref().is_some_and(|i| i.record.is_none()) && targets_by_path(rt, op)
}

/// The parts of `text`, lower case: split at anything but letters and digits, and at
/// camelCase boundaries (`appendToResponse` → `append`, `to`, `response`).
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for c in text.chars() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        previous_lower = c.is_lowercase() || c.is_ascii_digit();
        current.extend(c.to_lowercase());
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Whether `value` is shaped as a language tag (`en`, `fr-FR`, `pt_BR`, `zh-Hant`).
fn language_tag(value: &str) -> bool {
    let mut parts = value.split(['-', '_']);
    let lang = parts.next().unwrap_or_default();
    (2..=3).contains(&lang.len())
        && lang.chars().all(|c| c.is_ascii_lowercase())
        && parts.all(|p| (2..=4).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// The values `schema` enumerates for a parameter, or for each item of an array one.
fn enumerated(schema: &Value) -> Vec<&str> {
    schema
        .get("enum")
        .or_else(|| schema.get("items").and_then(|i| i.get("enum")))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Bounds [`sub_resource`]'s walk through `$ref`s.
const MAX_REF_DEPTH: usize = 8;

/// Whether `schema` holds a sub-resource (an object or an array, or a choice that may be
/// one, as an expandable reference is), not a scalar, looking through `$ref`s.
fn sub_resource(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
    let mut schema = schema;
    for _ in 0..MAX_REF_DEPTH {
        let Some(name) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.rsplit('/').next())
        else {
            break;
        };
        match bundle.defs.get(name) {
            Some(found) => schema = found,
            None => return false,
        }
    }
    let typed = |t: &Value| matches!(t.as_str(), Some("object" | "array"));
    schema.get("type").is_some_and(|t| match t {
        Value::Array(types) => types.iter().any(typed),
        t => typed(t),
    }) || [
        "properties",
        "items",
        "additionalProperties",
        "allOf",
        "anyOf",
        "oneOf",
    ]
    .iter()
    .any(|key| schema.get(*key).is_some())
}

/// The response members a read of `op` may embed: the sub-resources ([`sub_resource`])
/// its success answer declares and does not require.
fn embeddable_members(rt: &MockRuntime, op: &Operation) -> HashSet<String> {
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return HashSet::new();
    };
    let bundle = rt.bundle();
    let mut members = declared_members(bundle, schema).unwrap_or_default();
    for required in required_members(bundle, schema) {
        members.remove(&required);
    }
    members.retain(|member| {
        property_schema(bundle, schema, member).is_some_and(|s| sub_resource(bundle, &s))
    });
    members
}

/// Whether any of `words` is one of `set`.
fn says(set: &[&str], words: &[String]) -> bool {
    words.iter().any(|w| set.contains(&w.as_str()))
}

/// Whether query parameter `name`, declared with `schema`, selects the language of the
/// answer's text: its name ends in a language word (`language`, `lang`, `locale`), its
/// `format` names a language, its `enum` lists language tags, or its description says
/// language or locale; and it is no list, nor says it includes or filters something
/// (`include_image_language` selects which images are included, by its name). An enum
/// of language tags holds a two-letter one or one with a subtag (`en`, `pt-BR`), so
/// `[asc, dsc]` is none.
fn selects_language(name: &str, schema: &Value) -> bool {
    let named = words(name);
    let described = words(
        schema
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let values = enumerated(schema);
    let format = schema
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    schema.get("type").and_then(Value::as_str) != Some("array")
        && !says(INCLUDE_WORDS, &named)
        && (named
            .last()
            .is_some_and(|w| LANGUAGE_WORDS.contains(&w.as_str()))
            || format.contains("lang")
            || format.contains("locale")
            || format.contains("bcp")
            || (values.len() >= 2
                && values.iter().all(|v| language_tag(v))
                && values
                    .iter()
                    .any(|v| v.len() == 2 || v.contains(['-', '_'])))
            || (says(LANGUAGE_WORDS, &described) && !says(DESCRIBED_INCLUDE_WORDS, &described)))
}

/// The role query parameter `name`, declared with `schema`, has on single-record read
/// `op`, whose answer may embed `members` ([`embeddable_members`]); see the module docs.
/// A parameter the read does not declare does nothing to it.
pub(super) fn role(
    rt: &MockRuntime,
    op: &Operation,
    (name, schema): (&str, &Value),
    members: &HashSet<String>,
) -> Role {
    // A described fields parameter narrows the record (`exec::fields`).
    let fields = rt
        .described()
        .op(&op.id)
        .and_then(|d| d.fields.as_ref())
        .is_some_and(|f| f.param == name);
    if fields {
        return Role::Projection;
    }
    if selects_language(name, schema) {
        return Role::Language;
    }
    let description = schema
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let says_embed = says(EMBED_WORDS, &words(name)) || says(EMBED_WORDS, &words(description));
    // What the contract says the parameter names: its enumerated values, else, where its
    // name or description says it adds members, the members its description names.
    let mut offered: Vec<String> = enumerated(schema)
        .into_iter()
        .filter(|v| members.contains(*v))
        .map(str::to_string)
        .collect();
    if offered.is_empty() && says_embed {
        let mut seen = HashSet::new();
        offered = description
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| members.contains(*w) && seen.insert(*w))
            .map(str::to_string)
            .collect();
    }
    if !offered.is_empty() || says_embed {
        return Role::Embed { offered };
    }
    Role::Projection
}

/// The query parameters `op` declares, each with its schema.
fn declared(op: &Operation) -> impl Iterator<Item = (&String, &Value)> {
    op.input
        .query
        .as_ref()
        .and_then(|q| q.get("properties"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
}

/// The query parameter that selects the language of `op`'s answer, as `op` declares it.
pub(super) fn language_param(op: &Operation) -> Option<&String> {
    declared(op)
        .find(|(name, schema)| selects_language(name, schema))
        .map(|(name, _)| name)
}

/// What a single-record read's query asks to be embedded.
#[derive(Debug, Default)]
pub(super) struct Embeds {
    /// The members the read asks for that its answer may embed.
    pub(super) asked: Vec<String>,
    /// The members any of `op`'s embedding parameters is said to name.
    pub(super) offered: Vec<String>,
}

impl Embeds {
    /// The asked members `data` (the stored record) does not hold.
    pub(super) fn missing<'e>(&'e self, data: &Value) -> Vec<&'e str> {
        self.asked
            .iter()
            .filter(|m| data.get(m.as_str()).is_none())
            .map(String::as_str)
            .collect()
    }

    /// Adds to `variant`, a language's variant of a record, the asked members it lacks
    /// that `record` holds: what was embedded is kept with the record, not its variants.
    pub(super) fn fill_from(&self, variant: &mut Value, record: &Value) {
        let absent: Vec<String> = self
            .missing(variant)
            .into_iter()
            .map(str::to_string)
            .collect();
        if let Value::Object(members) = variant {
            for name in absent {
                if let Some(value) = record.get(&name) {
                    members.insert(name, value.clone());
                }
            }
        }
    }

    /// Leaves out of `body` (a record) the offered members the read did not ask for.
    pub(super) fn leave_out_unasked(&self, body: Option<&mut Value>) {
        let Some(Value::Object(members)) = body else {
            return;
        };
        for offered in &self.offered {
            if !self.asked.contains(offered) {
                members.remove(offered);
            }
        }
    }
}

/// The members a value of an embedding parameter names: a comma (or space) separated
/// list, or an array of them; a dotted name (`data.customer`) by its first step.
fn named_members(value: &Value) -> Vec<String> {
    let texts: Vec<&str> = match value {
        Value::String(text) => vec![text.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    texts
        .into_iter()
        .flat_map(|t| t.split([',', ' ']))
        .map(|name| {
            name.trim()
                .split('.')
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// The members any embedding parameter of `op`, a single-record read, is said to name
/// ([`Embeds::offered`]); none for any other read.
pub(super) fn offered(rt: &MockRuntime, op: &Operation) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if !single_record_read(rt, op) {
        return out;
    }
    let members = embeddable_members(rt, op);
    for (name, schema) in declared(op) {
        if let Role::Embed { offered } = role(rt, op, (name, schema), &members) {
            for member in offered {
                if !out.contains(&member) {
                    out.push(member);
                }
            }
        }
    }
    out
}

/// What `call`, a single-record read of `op`, asks to be embedded; empty for any other
/// read.
pub(super) fn embeds(rt: &MockRuntime, op: &Operation, call: &OperationCall) -> Embeds {
    let mut out = Embeds::default();
    if !single_record_read(rt, op) {
        return out;
    }
    let members = embeddable_members(rt, op);
    for (name, schema) in declared(op) {
        let Role::Embed { offered } = role(rt, op, (name, schema), &members) else {
            continue;
        };
        for member in offered {
            if !out.offered.contains(&member) {
                out.offered.push(member);
            }
        }
        for member in call.query.get(name).map(named_members).unwrap_or_default() {
            if members.contains(&member) && !out.asked.contains(&member) {
                out.asked.push(member);
            }
        }
    }
    out
}

/// `record`, what a read of `op` in another language than `held` was stored in generated
/// over it, made the language's kept variant with what `record` embeds, where that
/// language has one: a read generated only for what it embeds keeps the text its
/// language was answered with.
pub(super) fn onto_kept(
    rt: &MockRuntime,
    session: &SessionKey,
    (op, call): (&Operation, &OperationCall),
    held: &StoredResource,
    record: &mut Value,
) {
    if let Some(super::variants::Variant::Kept(kept)) = rt.variants().of(session, op, call, held) {
        let mut kept = kept.data;
        embeds(rt, op, call).fill_from(&mut kept, record);
        *record = kept;
    }
}

/// Keeps with `held`, the record a single-record read of `op` was generated over, the
/// members `call` asked to be embedded that it lacks and `body`, the answer, holds. The
/// record's own members stay as stored. Its language and kept variants stay too (see
/// [`super::variants::Variants::grew`]). Then, where the read's language has a kept
/// variant, `body` is that variant with what was embedded: the read answers in its own
/// language, as its repeats will.
pub(super) async fn keep_embedded(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, call): (&Operation, &OperationCall),
    held: &StoredResource,
    body: &mut Value,
) {
    let rt = state.rt;
    let embeds = embeds(rt, op, call);
    if embeds.asked.is_empty() {
        return;
    }
    let Some(current) = state.get(session, &held.resource_type, &held.id).await else {
        return;
    };
    let Value::Object(mut members) = current.data.clone() else {
        return;
    };
    let mut grew = false;
    for name in embeds.missing(&current.data) {
        if let Some(value) = body.get(name) {
            members.insert(name.to_string(), value.clone());
            grew = true;
        }
    }
    if !grew {
        return;
    }
    let grown = StoredResource {
        data: Value::Object(members),
        ..current.clone()
    };
    let own = rt.own_reads().holds(session, &current);
    if !state.upsert(session, grown.clone()).await {
        return;
    }
    if own {
        rt.own_reads().remember(session, &grown);
    }
    rt.variants().grew(session, &current, &grown.data);
    if let Some(super::variants::Variant::Kept(mut kept)) =
        rt.variants().of(session, op, call, &grown)
    {
        embeds.fill_from(&mut kept.data, &grown.data);
        *body = kept.data;
    }
}

#[cfg(test)]
mod tests {
    use super::{language_tag, named_members, words};
    use serde_json::json;

    #[test]
    fn words_split_names_and_text() {
        assert_eq!(words("append_to_response"), ["append", "to", "response"]);
        assert_eq!(words("appendToResponse"), ["append", "to", "response"]);
        assert_eq!(
            words("include_image_language"),
            ["include", "image", "language"]
        );
        assert_eq!(words("expand[]"), ["expand"]);
    }

    #[test]
    fn languages_are_told_by_the_contract() {
        for (name, schema) in [
            // TMDB.
            (
                "language",
                json!({"type": "string", "pattern": "^[a-z]{2}(-[A-Z]{2})?$"}),
            ),
            // Stripe's Checkout `locale`, with `auto` among its values.
            (
                "locale",
                json!({"type": "string", "enum": ["auto", "de", "fr"]}),
            ),
            (
                "hl",
                json!({"type": "string", "description": "Display language."}),
            ),
            ("ui", json!({"type": "string", "enum": ["en", "pt-BR"]})),
            ("tag", json!({"type": "string", "format": "bcp47"})),
        ] {
            assert!(super::selects_language(name, &schema), "{name}");
        }
        for (name, schema) in [
            // TMDB: which images are included.
            ("include_image_language", json!({"type": "string"})),
            // Stripe.
            (
                "expand",
                json!({"type": "array", "items": {"type": "string"},
                       "description": "Specifies which fields in the response should be expanded."}),
            ),
            // GitHub.
            (
                "per_page",
                json!({"type": "integer", "description": "The number of results per page."}),
            ),
            (
                "languages",
                json!({"type": "string", "description": "Only include repositories in these languages."}),
            ),
        ] {
            assert!(!super::selects_language(name, &schema), "{name}");
        }
        for values in [json!(["asc", "dsc"]), json!(["top", "new", "hot"])] {
            let schema = json!({"type": "string", "enum": values});
            assert!(!super::selects_language("order", &schema), "{schema}");
        }
    }

    #[test]
    fn language_tags() {
        for tag in ["en", "fr-FR", "pt_BR", "zh-Hant", "es-419"] {
            assert!(language_tag(tag), "{tag}");
        }
        for not in ["EN", "credits", "a", "en-", "english"] {
            assert!(!language_tag(not), "{not}");
        }
    }

    #[test]
    fn embedding_values_name_members() {
        assert_eq!(
            named_members(&json!("credits, videos,images")),
            ["credits", "videos", "images"]
        );
        assert_eq!(
            named_members(&json!(["customer", "data.invoice"])),
            ["customer", "data"]
        );
    }
}
