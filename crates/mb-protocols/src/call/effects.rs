//! Applying an answer's effects to state: typing, merging and stub filling.

use super::*;

/// Counter of the effects an answer listed, by `mock` and `outcome`
/// (`applied` or `rejected`).
pub(super) const EFFECTS_METRIC: &str = "mb_effects_total";

/// Characters of an answer's own text that a log line carries, in one field.
pub(super) const MAX_LOGGED_CHARS: usize = 64;

/// `text`, which an LLM wrote, cut to fit a log line.
pub(super) fn log_capped(text: &str) -> String {
    let mut chars = text.chars();
    let mut capped: String = chars.by_ref().take(MAX_LOGGED_CHARS).collect();
    if chars.next().is_some() {
        capped.push('…');
    }
    capped
}

/// Applies what an answer says it changed in other resources, in this session
/// only, and counts each effect. A rejected effect costs itself: the answer is
/// served and the next effect still applies. One on `own`, the (type, id) an update
/// just stored, keeps only what [`write_memory::own_effect_members`] allows. One on a
/// record not stored on its own that `acted`, the (type, id) the call acted on, holds
/// (a cart's line) goes into that record ([`within_own`]).
pub(super) async fn apply_effects(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    effects: &[ResourceEffect],
    (own, acted): (Option<Record<'_>>, Option<Record<'_>>),
    source: (&CallPlan<'_>, &OperationCall, u16),
) -> Vec<EffectOutcome> {
    let mut outcomes = Vec::with_capacity(effects.len());
    for effect in effects {
        let on_own = own == Some((effect.resource_type.as_str(), effect.id.as_str()));
        let mut result = apply_effect(state, session, rt, effect, on_own.then_some(source)).await;
        // A record not stored on its own may be an item the target holds (a cart's
        // line): the effect goes into the target, in that item.
        if let (Some("no schema to check against" | "no such resource"), Some(acted)) =
            (result.as_ref().err().copied(), acted)
            && acted != (effect.resource_type.as_str(), effect.id.as_str())
            && let Some(within) = within_own(state, session, acted, effect).await
        {
            result = apply_effect(state, session, rt, &within, None)
                .await
                .map(|(merged, _)| (merged, effect.set.clone()));
        }
        tracing::info!(
            mock = %rt.name,
            resource_type = %log_capped(&effect.resource_type),
            id = %log_capped(&effect.id),
            rejected = result.as_ref().err().copied(),
            "effect of an answer"
        );
        let outcome = if result.is_ok() {
            "applied"
        } else {
            "rejected"
        };
        metrics::counter!(EFFECTS_METRIC, "mock" => rt.name.clone(), "outcome" => outcome)
            .increment(1);
        // An applied effect is told by what it set, as it now stands: its dotted
        // keys as the members they went into.
        let (effect, merged, rejected) = match result {
            Ok((merged, set)) => (
                ResourceEffect {
                    set,
                    ..effect.clone()
                },
                Some(merged),
                None,
            ),
            Err(reason) => (effect.clone(), None, Some(reason.to_string())),
        };
        let applied = match rejected {
            None => effect.set.keys().cloned().collect(),
            Some(_) => Vec::new(),
        };
        outcomes.push(EffectOutcome {
            effect,
            rejected,
            merged,
            applied,
        });
    }
    outcomes
}

/// `effect`, on a record the session does not hold on its own, as an effect on the
/// record of type `t` and id `id` the operation acts on, when that holds an object
/// with the effect's id (a cart's line): the top member of `own` holding it, with the
/// effect's members set on that object. `None` when `own` holds no such object.
async fn within_own(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (t, id): (&str, &str),
    effect: &ResourceEffect,
) -> Option<ResourceEffect> {
    let held = state.get(session, t, id).await?;
    let Value::Object(members) = &held.data else {
        return None;
    };
    for (name, value) in members {
        let mut value = value.clone();
        if let Some(item) = holding(&mut value, &effect.id, MAX_ENTITY_DEPTH) {
            for (key, set) in &effect.set {
                item.insert(key.clone(), set.clone());
            }
            let mut set = Map::new();
            set.insert(name.clone(), value);
            return Some(ResourceEffect {
                resource_type: t.to_owned(),
                id: id.to_owned(),
                set,
            });
        }
    }
    None
}

/// The object within `value` whose `id` is `id`, at most `depth` levels down.
fn holding<'v>(value: &'v mut Value, id: &str, depth: usize) -> Option<&'v mut Map<String, Value>> {
    if depth == 0 {
        return None;
    }
    match value {
        Value::Object(fields) => {
            if fields.get("id").and_then(Value::as_str) == Some(id) {
                return Some(fields);
            }
            fields.values_mut().find_map(|v| holding(v, id, depth - 1))
        }
        Value::Array(items) => items.iter_mut().find_map(|v| holding(v, id, depth - 1)),
        _ => None,
    }
}

/// A record's (type, id).
type Record<'a> = (&'a str, &'a str);

/// Adds what became of `generated`'s effects to a request record's `request`
/// (nothing when it listed none), under `effects`, what the world description
/// leaves open about its operation ([`GeneratedResponse::world_gaps`]), under
/// `world_gap`, each name once, and what the executor noted of the call
/// ([`GeneratedResponse::exec_notes`]) under their own names.
pub(crate) fn record_effects(request: &mut Value, generated: &GeneratedResponse) {
    record_world_gaps(request, &generated.world_gaps);
    if !generated.exec_notes.is_empty() {
        if request.is_null() {
            *request = serde_json::json!({});
        }
        if let Value::Object(request) = request {
            for (name, note) in &generated.exec_notes {
                request.insert(name.clone(), note.clone());
            }
        }
    }
    if generated.effect_outcomes.is_empty() {
        return;
    }
    let recorded = generated.effect_outcomes.iter().map(|o| {
        let mut record = serde_json::json!({
            "type": o.effect.resource_type,
            "id": o.effect.id,
            "set": o.effect.set,
            "outcome": if o.rejected.is_some() { "rejected" } else { "applied" },
        });
        if let Some(reason) = &o.rejected {
            record["reason"] = Value::from(reason.as_str());
        }
        record
    });
    if request.is_null() {
        *request = serde_json::json!({});
    }
    if let Value::Object(request) = request {
        match request.get_mut("effects") {
            Some(Value::Array(list)) => list.extend(recorded),
            _ => {
                request.insert("effects".to_string(), Value::Array(recorded.collect()));
            }
        }
    }
}

/// What a request record's `request` notes of the call beside its body: the world's
/// gaps (`world_gap`) and what the executor noted ([`GeneratedResponse::exec_notes`]).
#[cfg(any(feature = "http-rest", feature = "soap"))]
pub(crate) const RECORD_NOTES: [&str; 4] = [
    "world_gap",
    "exec_fallback",
    "exec_fallback_on",
    "expr_failed",
];

/// Adds `gaps` to a request record's `request` under `world_gap`, each name once.
fn record_world_gaps(request: &mut Value, gaps: &[String]) {
    if gaps.is_empty() {
        return;
    }
    tracing::info!(world_gap = ?gaps, "the world description leaves this operation open");
    if request.is_null() {
        *request = serde_json::json!({});
    }
    let Value::Object(request) = request else {
        return;
    };
    let list = request
        .entry("world_gap")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !list.is_array() {
        *list = Value::Array(Vec::new());
    }
    if let Value::Array(list) = list {
        for gap in gaps {
            if !list.iter().any(|g| g.as_str() == Some(gap)) {
                list.push(Value::from(gap.as_str()));
            }
        }
    }
}

/// Whether resource type `t` has a schema an effect on it is checked against.
pub(super) fn has_schema(index: &BundleIndex, t: &str) -> bool {
    index.detail_reads.contains_key(t) || index.other_shapes.contains_key(t)
}

/// Whether `t` is a resource type of the contract's: one an operation acts on, or a
/// definition is of. Such a name is what it says, schema or not.
pub(super) fn known_type(rt: &MockRuntime, t: &str) -> bool {
    rt.bundle()
        .operations
        .iter()
        .any(|op| op.resource_type.as_deref() == Some(t))
        || rt.index().types_by_def.values().any(|v| v == t)
}

/// The resource types a prompt showed, in the order it showed them, each once, of
/// those an effect can be checked against (see [`mb_engine::prompt`]'s effects note).
pub(super) fn shown_types(
    rt: &MockRuntime,
    target: Option<&StoredResource>,
    related: &[StoredResource],
    created_since: &[StoredResource],
) -> Vec<String> {
    let max = mb_engine::prompt::MAX_RELATED;
    let index = rt.index();
    let mut types: Vec<String> = Vec::new();
    for r in target
        .into_iter()
        .chain(related.iter().take(max))
        .chain(created_since.iter().take(max))
    {
        if has_schema(index, &r.resource_type) && !types.contains(&r.resource_type) {
            types.push(r.resource_type.clone());
        }
    }
    types
}

/// `name` as [`mb_mcm::stem`] has it, its words run together: `meetingRegistrant`,
/// `meeting_registrants` and `Meeting-Registrant` are one.
pub(super) fn type_stem(name: &str) -> String {
    let joined: String = name.chars().filter(|c| c.is_alphanumeric()).collect();
    mb_mcm::stem(&joined)
}

/// The type of `shown` that `name` means, `holding` being those of `shown` that hold a
/// record under the effect's id. The stem decides (see [`type_stem`]): the one shown
/// type of that stem, unless the id is held elsewhere and not by it; of several of
/// that stem, the one holding the id. An id alone names no type.
pub(super) fn stem_type<'a>(shown: &'a [String], name: &str, holding: &[&str]) -> Option<&'a str> {
    let stem = type_stem(name);
    let matching: Vec<&'a str> = shown
        .iter()
        .filter(|t| type_stem(t) == stem)
        .map(String::as_str)
        .collect();
    match matching.as_slice() {
        [] => None,
        [only] => (holding.is_empty() || holding.contains(only)).then_some(*only),
        several => {
            let held: Vec<&'a str> = several
                .iter()
                .copied()
                .filter(|t| holding.contains(t))
                .collect();
            match held.as_slice() {
                [one] => Some(*one),
                _ => None,
            }
        }
    }
}

/// Gives each effect whose type the contract does not know (models write `playlist`
/// for the `playlists` they were shown) the `shown` type it means (see
/// [`stem_type`]). With none, the effect is left as it came, and is rejected.
pub(super) async fn resolve_effect_types(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    shown: &[String],
    effects: &mut [ResourceEffect],
) {
    let index = rt.index();
    for effect in effects.iter_mut() {
        if has_schema(index, &effect.resource_type) || known_type(rt, &effect.resource_type) {
            continue;
        }
        if !shown
            .iter()
            .any(|t| type_stem(t) == type_stem(&effect.resource_type))
        {
            continue;
        }
        let mut holding = Vec::new();
        for t in shown {
            if state.get(session, t, &effect.id).await.is_some() {
                holding.push(t.as_str());
            }
        }
        if let Some(t) = stem_type(shown, &effect.resource_type, &holding) {
            tracing::debug!(
                mock = %rt.name,
                written = %log_capped(&effect.resource_type),
                resource_type = %t,
                "effect type resolved to a shown type"
            );
            effect.resource_type = t.to_string();
        }
    }
}

/// What one place in a record's schema says, by its `$ref`s and compositions (see
/// [`schema_parts`]).
pub(super) struct Place<'a> {
    pub(super) parts: Vec<&'a Value>,
    pub(super) defs: Vec<&'a str>,
}

impl<'a> Place<'a> {
    pub(super) fn of(rt: &'a MockRuntime, schema: Option<&'a Value>) -> Self {
        let (parts, defs) = schema_parts(rt.bundle(), schema);
        Self { parts, defs }
    }

    /// The schema the place's `properties` give member `key`.
    pub(super) fn property(&self, key: &str) -> Option<&'a Value> {
        self.parts
            .iter()
            .find_map(|p| p.get("properties")?.get(key))
    }

    /// Whether a part of the place lists member `key` as `required`.
    pub(super) fn requires(&self, key: &str) -> bool {
        self.parts.iter().any(|p| {
            p.get("required")
                .and_then(Value::as_array)
                .is_some_and(|r| r.iter().any(|k| k.as_str() == Some(key)))
        })
    }

    /// [`Place::of`], where a `oneOf` or `anyOf` counts only the branches `held`, what
    /// the place holds now, validates against (all of them when it validates against
    /// none, or nothing is held): what one variant requires or lets be null says
    /// nothing of another.
    pub(super) fn of_held(
        rt: &'a MockRuntime,
        schema: Option<&'a Value>,
        held: Option<&Value>,
    ) -> Self {
        fn visit<'a>(
            bundle: &'a mb_mcm::Bundle,
            schema: &'a Value,
            held: Option<&Value>,
            depth: usize,
            place: &mut Place<'a>,
        ) {
            if depth > 8 || place.parts.len() >= MAX_SCHEMA_PARTS {
                return;
            }
            if let Some(def) = def_name(schema) {
                place.defs.push(def);
                if let Some(target) = bundle.defs.get(def) {
                    visit(bundle, target, held, depth + 1, place);
                }
                return;
            }
            place.parts.push(schema);
            for key in ["anyOf", "oneOf", "allOf"] {
                let branches: Vec<&Value> = schema
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .collect();
                let matching: Vec<&Value> = match held.filter(|_| key != "allOf") {
                    Some(held) => branches
                        .iter()
                        .copied()
                        .filter(|b| {
                            mb_engine::SchemaValidator::new(&bundle.with_reachable_defs(b))
                                .is_ok_and(|v| v.is_valid(held))
                        })
                        .collect(),
                    None => Vec::new(),
                };
                let chosen = if matching.is_empty() {
                    branches
                } else {
                    matching
                };
                for branch in chosen {
                    visit(bundle, branch, held, depth + 1, place);
                }
            }
        }
        let mut place = Self {
            parts: Vec::new(),
            defs: Vec::new(),
        };
        if let Some(schema) = schema {
            visit(rt.bundle(), schema, held, 0, &mut place);
        }
        place
    }

    /// What a `null` for member `key` is at this place: kept where the place does
    /// not declare the member or its schema admits null (see [`admits_null`]),
    /// otherwise the member's removal when it is optional and refused when it is
    /// required. For a union, build the place with [`Place::of_held`].
    pub(super) fn null_member(&self, rt: &MockRuntime, key: &str) -> NullMember {
        match self.property(key) {
            Some(schema) if !admits_null(rt, schema) => {
                if self.requires(key) {
                    NullMember::Refused
                } else {
                    NullMember::Removed
                }
            }
            _ => NullMember::Kept,
        }
    }

    /// Whether the place declares any `properties`.
    pub(super) fn declares(&self) -> bool {
        self.parts
            .iter()
            .any(|p| p.get("properties").and_then(Value::as_object).is_some())
    }

    /// Whether the place is a map: `additionalProperties` and nothing declared.
    pub(super) fn is_map(&self) -> bool {
        !self.declares()
            && self.parts.iter().any(|p| {
                matches!(
                    p.get("additionalProperties"),
                    Some(Value::Object(_) | Value::Bool(true))
                )
            })
    }

    /// Whether the place holds an object.
    pub(super) fn is_object(&self) -> bool {
        self.declares()
            || self.is_map()
            || self
                .parts
                .iter()
                .any(|p| p.get("type").and_then(Value::as_str) == Some("object"))
    }

    /// Whether `new` is of the variant `held` is, at a place that is a union of two or
    /// more object branches (`oneOf`, `anyOf`): no two values of its discriminator,
    /// `discriminator.propertyName` or else a member a part of every object branch pins
    /// (`const`, a one-value `enum`), tell them apart. Anywhere else there is one variant.
    pub(super) fn same_variant(
        &self,
        rt: &MockRuntime,
        held: &Map<String, Value>,
        new: &Map<String, Value>,
    ) -> bool {
        let Some((part, branches)) = self.parts.iter().find_map(|p| {
            let branches = p
                .get("oneOf")
                .or_else(|| p.get("anyOf"))
                .and_then(Value::as_array)?;
            let objects: Vec<Place<'_>> = branches
                .iter()
                .map(|b| Place::of(rt, Some(b)))
                .filter(Place::is_object)
                .collect();
            (objects.len() >= 2).then_some((*p, objects))
        }) else {
            return true;
        };
        // Any part of a branch may pin it (`allOf: [$ref Base, {...}]`), whatever
        // the others declare.
        let pins = |key: &str| {
            branches.iter().all(|b| {
                b.parts.iter().any(|p| {
                    p.get("properties")
                        .and_then(|members| members.get(key))
                        .is_some_and(|s| {
                            s.get("const").is_some()
                                || s.get("enum")
                                    .and_then(Value::as_array)
                                    .is_some_and(|e| e.len() == 1)
                        })
                })
            })
        };
        let key = part
            .get("discriminator")
            .and_then(|d| d.get("propertyName"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                let first = branches.first()?;
                first
                    .parts
                    .iter()
                    .filter_map(|p| p.get("properties").and_then(Value::as_object))
                    .flat_map(|members| members.keys())
                    .find(|k| pins(k))
                    .cloned()
            });
        // Another variant only when both say which, and differ: a value that leaves
        // it out says nothing of its variant. With no discriminator to tell, the
        // variants cannot be told apart, and the value is taken for another one.
        key.is_some_and(|k| !matches!((held.get(&k), new.get(&k)), (Some(a), Some(b)) if a != b))
    }

    /// The resource type the place's `$ref` is a definition of.
    pub(super) fn record_type(&self, index: &BundleIndex) -> Option<String> {
        self.defs
            .iter()
            .find_map(|d| index.types_by_def.get(*d))
            .cloned()
    }
}

/// What a `null` set on a declared member becomes (see [`Place::null_member`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NullMember {
    /// Stored as null: the member's schema admits it, or the place does not declare it.
    Kept,
    /// The member is removed: it is optional, and its schema does not admit null.
    Removed,
    /// Not applied: the member is required, and its schema does not admit null.
    Refused,
}

/// Whether `schema` admits `null`: a part of it (by its `$ref`s and compositions)
/// has type `null` (or `nullable: true`), a `const` null or an `enum` holding null,
/// or none of its parts says what type it is.
pub(super) fn admits_null(rt: &MockRuntime, schema: &Value) -> bool {
    let parts = Place::of(rt, Some(schema)).parts;
    let null = Value::Null;
    let says_null = |p: &&Value| {
        p.get("nullable").and_then(Value::as_bool) == Some(true)
            || match p.get("type") {
                Some(Value::String(t)) => t == "null",
                Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("null")),
                _ => false,
            }
            || p.get("const") == Some(&null)
            || p.get("enum")
                .and_then(Value::as_array)
                .is_some_and(|e| e.contains(&null))
    };
    let constrains = |p: &&Value| {
        [
            "type",
            "const",
            "enum",
            "properties",
            "items",
            "additionalProperties",
        ]
        .iter()
        .any(|k| p.get(*k).is_some())
    };
    parts.iter().any(says_null) || !parts.iter().any(constrains)
}

/// Whether `value` is what a prompt shows in place of a redacted one.
pub(super) fn is_redacted(value: &Value) -> bool {
    value.as_str() == Some(mb_engine::REDACTED)
}

/// `new` where nothing is kept: what was redacted in it is left out (see
/// [`mb_engine::without_redacted`]); none when nothing is left.
pub(super) fn unredacted(new: &Value) -> Option<Value> {
    mb_engine::without_redacted(new)
}

/// `new` replacing `held` at the same place, what was redacted in it as `held` has it
/// there (left out where `held` has nothing). An array holding anything redacted
/// stays as held: it is replaced whole, and a prompt showed it redacted.
pub(super) fn restored(held: Option<&Value>, new: &Value) -> Option<Value> {
    if is_redacted(new) {
        return held.cloned();
    }
    match new {
        Value::Object(members) => {
            let held = held.and_then(Value::as_object);
            Some(Value::Object(
                members
                    .iter()
                    .filter_map(|(k, v)| {
                        Some((k.clone(), restored(held.and_then(|h| h.get(k)), v)?))
                    })
                    .collect(),
            ))
        }
        Value::Array(_) if unredacted(new).as_ref() != Some(new) => held.cloned(),
        other => Some(other.clone()),
    }
}

/// `new` put where `held` is (none: nothing is there), at a place of schema `schema`;
/// none when nothing is to be there. Objects merge member by member where the place
/// declares members (a value object), as the members' own places say. These are
/// replaced whole: anything that is not an object; a map (see [`Place::is_map`]) or
/// an object the schema says nothing of, what was redacted in it kept as it was at
/// its own key (see [`restored`]); and an object of another variant of a union (see
/// [`Place::same_variant`]) or naming another identity (see [`reassigns`]), which
/// keeps nothing of `held`, redacted or not. In a value object, a `null` its
/// optional member's schema does not admit removes the member (see
/// [`Place::null_member`]); on a required member it stays, a violation the record
/// did not have, for which what sets it is refused whole (see [`added_violation`]).
pub(super) fn merge_at(
    rt: &MockRuntime,
    held: Option<&Value>,
    new: &Value,
    schema: Option<&Value>,
) -> Option<Value> {
    let Some(held) = held else {
        // A new value object's nulls go as its members' places say.
        if new
            .as_object()
            .is_some_and(|m| m.values().any(Value::is_null))
            && Place::of(rt, schema).declares()
        {
            return merge_at(rt, Some(&Value::Object(Map::new())), new, schema);
        }
        return unredacted(new);
    };
    let (Value::Object(held_members), Value::Object(new_members)) = (held, new) else {
        return restored(Some(held), new);
    };
    let place = Place::of(rt, schema);
    if !place.same_variant(rt, held_members, new_members)
        || reassigns(rt, &place, held_members, new_members)
    {
        return unredacted(new);
    }
    if !place.declares() {
        return restored(Some(held), new);
    }
    // Nulls go by the variant of a union the object is now.
    let nulls = new_members
        .values()
        .any(Value::is_null)
        .then(|| Place::of_held(rt, schema, Some(held)));
    let mut out = held_members.clone();
    for (key, value) in new_members {
        // A null the member's schema does not admit removes an optional member (see
        // [`Place::null_member`]). On a required one it stays, a violation the record
        // did not have: what sets it is refused whole (see [`added_violation`]).
        if value.is_null()
            && nulls.as_ref().map(|p| p.null_member(rt, key)) == Some(NullMember::Removed)
        {
            out.remove(key);
            continue;
        }
        if let Some(merged) = merge_at(rt, out.get(key), value, place.property(key)) {
            out.insert(key.clone(), merged);
        }
    }
    Some(Value::Object(out))
}

/// Whether `new` names another object than `held` at `place`. For a place of a
/// resource type, an identity member of that type (see [`identity_members`]) the place
/// declares holds another value. For an inline object, a scalar member that is a
/// generic id (`id`, `_id`, see [`mb_mcm::is_generic_id`]) does, of any type, or a
/// string member the place declares `readOnly`, by its `$ref`s and `allOf`s too.
pub(super) fn reassigns(
    rt: &MockRuntime,
    place: &Place<'_>,
    held: &Map<String, Value>,
    new: &Map<String, Value>,
) -> bool {
    let changed = |k: &str| matches!((held.get(k), new.get(k)), (Some(a), Some(b)) if a != b);
    match place.record_type(rt.index()) {
        Some(t) => identity_members(rt, &t)
            .iter()
            .filter(|k| place.property(k).is_some())
            .any(|k| changed(k)),
        None => new.iter().any(|(k, v)| {
            let scalar = |v: &Value| !v.is_object() && !v.is_array();
            let read_only = || {
                Place::of(rt, place.property(k))
                    .parts
                    .iter()
                    .any(|p| p.get("readOnly").and_then(Value::as_bool) == Some(true))
            };
            changed(k)
                && if mb_mcm::is_generic_id(k) {
                    scalar(v) && held.get(k).is_some_and(scalar)
                } else {
                    v.is_string() && held.get(k).is_some_and(Value::is_string) && read_only()
                }
        }),
    }
}

/// Puts `set` on `data`, the record of type `t`, each member as its place says (see
/// [`merge_at`]).
pub(super) fn merge_members(
    rt: &MockRuntime,
    t: &str,
    data: &mut Map<String, Value>,
    set: Map<String, Value>,
) {
    let place = Place::of(rt, write_memory::read_schema(rt, t));
    for (key, value) in set {
        if let Some(merged) = merge_at(rt, data.get(&key), &value, place.property(&key)) {
            data.insert(key, merged);
        }
    }
}

/// The keys a dotted key of a record of type `t` is a path of, and the schema of the
/// place it ends at; none when it is no path. It is one when the schema does not
/// declare the dotted name and declares its first segment as an object. It goes
/// on through declared `properties` only: past a map, or a segment not declared,
/// the rest is one key (`annotations.example.com/owner`).
pub(super) fn dotted_path<'a>(
    rt: &'a MockRuntime,
    record: &Place<'a>,
    key: &str,
) -> Option<(Vec<String>, Option<&'a Value>)> {
    let segments: Vec<&str> = key.split('.').collect();
    let (first, rest) = segments.split_first()?;
    if rest.is_empty() || segments.iter().any(|s| s.is_empty()) || record.property(key).is_some() {
        return None;
    }
    let mut schema = record.property(first)?;
    if !Place::of(rt, Some(schema)).is_object() {
        return None;
    }
    let mut path = vec![(*first).to_string()];
    for (i, segment) in rest.iter().enumerate() {
        let place = Place::of(rt, Some(schema));
        match place.property(segment).filter(|_| !place.is_map()) {
            Some(inner) => {
                path.push((*segment).to_string());
                schema = inner;
            }
            None => {
                path.push(rest[i..].join("."));
                return Some((path, None));
            }
        }
    }
    Some((path, Some(schema)))
}

/// `value` put at `path` below `base`, the last key as its place says (see
/// [`merge_at`]); what is not an object on the way becomes one.
pub(super) fn put_at(
    rt: &MockRuntime,
    base: &mut Value,
    path: &[String],
    value: &Value,
    schema: Option<&Value>,
) {
    let Some((key, rest)) = path.split_first() else {
        return;
    };
    if !base.is_object() {
        *base = Value::Object(Map::new());
    }
    let Value::Object(members) = base else {
        return;
    };
    if rest.is_empty() {
        if let Some(merged) = merge_at(rt, members.get(key), value, schema) {
            members.insert(key.clone(), merged);
        }
        return;
    }
    let entry = members.entry(key.clone()).or_insert(Value::Null);
    put_at(rt, entry, rest, value, schema);
}

/// `set` with each dotted key that is a path (see [`dotted_path`]) put into the member
/// its first segment names, as `held` (the stored record of type `t`) has it: a whole
/// new value for that member, with the rest of it as stored. A dotted name the schema
/// declares, or one into no declared object, stays as it is.
pub(super) fn dotted_as_paths(
    rt: &MockRuntime,
    t: &str,
    held: &Map<String, Value>,
    set: &Map<String, Value>,
) -> Map<String, Value> {
    if !set.keys().any(|k| k.contains('.')) {
        return set.clone();
    }
    let record = Place::of(rt, write_memory::read_schema(rt, t));
    let mut out = Map::new();
    for (key, value) in set {
        match dotted_path(rt, &record, key) {
            Some((path, schema)) => {
                let Some((first, rest)) = path.split_first() else {
                    continue;
                };
                let mut member = out
                    .get(first)
                    .or_else(|| held.get(first))
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new()));
                put_at(rt, &mut member, rest, value, schema);
                out.insert(first.clone(), member);
            }
            None => {
                // After a path into it, a member is merged as its place says.
                let merged = match out.get(key) {
                    Some(was) => merge_at(rt, Some(was), value, record.property(key)),
                    None => Some(value.clone()),
                };
                if let Some(merged) = merged {
                    out.insert(key.clone(), merged);
                }
            }
        }
    }
    out
}

/// The members a record of type `t` is identified by: its read's key, `id`,
/// `number` and the fields its operations identify it by.
pub(super) fn identity_members<'a>(rt: &'a MockRuntime, t: &str) -> Vec<&'a str> {
    let index = rt.index();
    let own = index
        .detail_reads
        .get(t)
        .and_then(|i| rt.bundle().operations.get(*i)?.identity.as_ref())
        .map(|identity| identity.field.as_str());
    own.into_iter()
        .chain(["id", "number"])
        .chain(index.identity_fields(t).iter().map(String::as_str))
        .collect()
}

/// Whether `object` names `stored` and says nothing more: each of its members is one
/// of `identity`, or a string `stored` holds as well (a `uri`, an `href`), and every
/// one is as `stored` has it. It holds less than `stored`, or there is nothing to fill.
pub(super) fn is_stub(object: &Map<String, Value>, stored: &Value, identity: &[&str]) -> bool {
    let Value::Object(held) = stored else {
        return false;
    };
    object.len() < held.len()
        && object.iter().any(|(k, _)| identity.contains(&k.as_str()))
        && object.iter().all(|(k, v)| {
            (identity.contains(&k.as_str()) || v.is_string()) && held.get(k) == Some(v)
        })
}

/// `stored` with only the members `place` declares (all of it when it declares none).
pub(super) fn as_declared(stored: &Value, place: &Place<'_>) -> Value {
    match stored {
        Value::Object(members) if place.declares() => Value::Object(
            members
                .iter()
                .filter(|(k, _)| place.property(k).is_some())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Records one effect's stub fill reads from the store at most, all its members
/// together.
pub(super) const MAX_STUB_READS: usize = 20;

/// Fills, in the members `keys` of record `value` of type `t`, each object that only
/// names a record (see [`is_stub`]) at a place whose `$ref` is a definition of that
/// record's type, with that record as the session stores it and as the place
/// declares it (see [`as_declared`]).
pub(super) async fn fill_stubs(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    t: &str,
    value: &mut Value,
    keys: &[String],
) {
    let record = Place::of(rt, write_memory::read_schema(rt, t));
    // First what the stubs name, then the records under it.
    let mut named: Vec<(String, String)> = Vec::new();
    if let Value::Object(members) = &*value {
        let mut walk = StubWalk {
            rt,
            fill: &mut |rtype: &str, id: &str, _: &Map<String, Value>, _: &Place<'_>| {
                let pair = (rtype.to_string(), id.to_string());
                if named.len() < MAX_STUB_READS && !named.contains(&pair) {
                    named.push(pair);
                }
                None
            },
        };
        for key in keys {
            if let Some(member) = members.get(key) {
                let mut probe = member.clone();
                walk.visit(&mut probe, record.property(key), 1);
            }
        }
    }
    let mut held: HashMap<(String, String), Value> = HashMap::new();
    for (rtype, id) in named {
        if let Some(found) = state.get(session, &rtype, &id).await {
            held.insert((rtype, id), found.data);
        }
    }
    if held.is_empty() {
        return;
    }
    if let Value::Object(members) = value {
        let mut walk = StubWalk {
            rt,
            fill: &mut |rtype: &str, id: &str, object: &Map<String, Value>, place: &Place<'_>| {
                let stored = as_declared(held.get(&(rtype.to_string(), id.to_string()))?, place);
                let identity = identity_members(rt, rtype);
                is_stub(object, &stored, &identity).then_some(stored)
            },
        };
        for key in keys {
            if let Some(member) = members.get_mut(key) {
                walk.visit(member, record.property(key), 1);
            }
        }
    }
}

/// What [`StubWalk`] asks of each object at a place holding a resource type: given
/// the type, the id the object names, the object and its place, the value to put
/// in its place, if any.
pub(super) type StubFill<'f> =
    dyn FnMut(&str, &str, &Map<String, Value>, &Place<'_>) -> Option<Value> + 'f;

/// The objects an effect's member holds at places whose `$ref` is a definition of a
/// resource type, with the id each names; `fill` may put another value in its place.
pub(super) struct StubWalk<'a, 'f> {
    pub(super) rt: &'a MockRuntime,
    pub(super) fill: &'f mut StubFill<'f>,
}

impl<'a> StubWalk<'a, '_> {
    pub(super) fn visit(&mut self, value: &mut Value, schema: Option<&'a Value>, depth: usize) {
        if depth > MAX_ENTITY_DEPTH {
            return;
        }
        let place = Place::of(self.rt, schema);
        match value {
            Value::Array(items) => {
                let item = place.parts.iter().find_map(|p| p.get("items"));
                for value in items {
                    self.visit(value, item, depth + 1);
                }
            }
            Value::Object(fields) => {
                if let Some(rtype) = place.record_type(self.rt.index())
                    && let Some(id) = identity_members(self.rt, &rtype)
                        .into_iter()
                        .find_map(|f| fields.get(f).and_then(id_string))
                    && let Some(record) = (self.fill)(&rtype, &id, fields, &place)
                {
                    *value = record;
                    return;
                }
                for (field, value) in fields.iter_mut() {
                    if value.is_object() || value.is_array() {
                        self.visit(value, place.property(field), depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Merges `effect` into the stored resource it names and returns the result and the
/// members it set, as they now are; `Err` says why not. The resource must exist (an
/// effect never creates one, and never deletes one), keep its identity and owner and,
/// merged, have no violation of its type's schema it did not have (see
/// [`added_violation`]): a record that fit still fits, and one held in an earlier,
/// incomplete shape gets nothing new wrong. The type must have a schema to check
/// against. Each member goes as its place in the schema says (see [`merge_at`]), a
/// dotted key may be a path (see [`dotted_as_paths`]), and objects that only name
/// stored records are filled with them (see [`fill_stubs`]). Member names written with
/// JSON around them are the declared ones first (see [`mended_set`]). A `null` a
/// member's schema does not admit removes the member when it is optional and rejects
/// the effect when it is required (see [`Place::null_member`]), in a value object too.
/// It applies whole or not at all.
pub(super) async fn apply_effect(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    rt: &MockRuntime,
    effect: &ResourceEffect,
    own: Option<(&CallPlan<'_>, &OperationCall, u16)>,
) -> Result<(Value, Map<String, Value>), &'static str> {
    let t = effect.resource_type.as_str();
    let index = rt.index();
    let set = write_memory::rehomed_members(rt, t, mended_set(rt, t, &effect.set));
    let identity = index.identity_fields(t);
    // A key held in an object member (`metadata.name`) is set by naming it there.
    let sets_nested = |k: &str, v: &Value| {
        identity
            .iter()
            .filter(|f| !f.contains('.') || !set.contains_key(f.as_str()))
            .any(|f| {
                f.strip_prefix(k)
                    .and_then(|rest| rest.strip_prefix('.'))
                    .is_some_and(|rest| mb_mcm::member_at(v, rest).is_some())
            })
    };
    if set.iter().any(|(k, v)| {
        matches!(k.as_str(), "id" | "number") || identity.contains(k) || sets_nested(k, v)
    }) {
        return Err("it sets an identity field");
    }
    if !has_schema(index, t) {
        return Err("no schema to check against");
    }
    let Some(mut stored) = state.get(session, t, &effect.id).await else {
        return Err("no such resource");
    };
    let held = stored.clone();
    let Value::Object(data) = &mut stored.data else {
        return Err("the stored resource is not an object");
    };
    let requested = dotted_as_paths(rt, t, data, &set);
    // What was redacted is restored, or left out, where it merges (see [`merge_at`]).
    if !set.is_empty() && set.values().all(|v| unredacted(v).is_none()) {
        return Err("it sets only what was redacted");
    }
    let requested = if let Some((plan, call, status)) = own {
        let own = write_memory::own_effect_members(rt, plan, call, &held, &requested, status);
        if own.set.is_empty() {
            return Err("it sets no member an effect on this update's target may");
        }
        // A member a `PUT` replaces whole goes where nothing is held.
        for key in &own.whole {
            data.remove(key);
        }
        own.set
    } else {
        requested
    };
    // A null the member's schema does not admit (in the variant of a union the record
    // is) removes an optional member; on a required one, the effect is refused.
    let record = Place::of_held(rt, write_memory::read_schema(rt, t), Some(&held.data));
    let mut requested = requested;
    let mut removed = Vec::new();
    let mut refused = false;
    requested.retain(|key, value| {
        if !value.is_null() {
            return true;
        }
        match record.null_member(rt, key) {
            NullMember::Kept => true,
            NullMember::Removed => {
                removed.push(key.clone());
                false
            }
            NullMember::Refused => {
                refused = true;
                true
            }
        }
    });
    if refused {
        return Err("it sets a required member to null");
    }
    for key in &removed {
        data.remove(key);
    }
    let keys: Vec<String> = requested.keys().cloned().collect();
    merge_members(rt, t, data, requested);
    // Objects the effect names only by their identity are the records stored under it.
    fill_stubs(state, session, rt, t, &mut stored.data, &keys).await;
    // Each member it sets fits its own schema, in the variant the record is: under a
    // union read, a record that already fits no variant shows only the union's own
    // violation, the same with a wrong-typed member as without.
    if let Some(member) = wrong_member(rt, t, &held.data, &stored.data, &keys, &set) {
        tracing::info!(
            mock = %rt.name,
            resource_type = %t,
            member = %member,
            "an effect sets a member to a value its schema does not take"
        );
        return Err("the merged resource breaks its schema");
    }
    // Refused when it adds a violation: it breaks a record that fit, or gives one held
    // in an earlier, incomplete shape a violation it did not have.
    if let Some((path, keyword)) = added_violation(rt, t, &held.data, &stored.data) {
        tracing::info!(
            mock = %rt.name,
            resource_type = %t,
            path = %path,
            keyword = %keyword,
            "an effect leaves its resource with a schema violation it did not have"
        );
        return Err("the merged resource breaks its schema");
    }
    if let Some((plan, call, _)) = own
        && !write_memory::keeps_owner(rt, plan, call, &held, &stored.data)
    {
        return Err("the merged resource is no longer its owner's");
    }
    let applied: Map<String, Value> = keys
        .iter()
        .filter_map(|k| Some((k.clone(), stored.data.get(k)?.clone())))
        .collect();
    let merged = stored.data.clone();
    let carried = stored.clone();
    if !state.upsert(session, stored).await {
        return Err("the store took no write");
    }
    rt.collections().carry(session, &held, &carried);
    Ok((merged, applied))
}

/// The first member of `keys` (the top-level members an effect merged into `after`,
/// the record of type `t` that was `before`) given a violation of its own schema it
/// did not have, as the record's variant declares it (see [`Place::of_held`]): of a
/// dotted key of `set` that is a path (see [`dotted_path`]), the leaf it ends at, else
/// the whole member (see [`adds_to_member`]). Where several parts of the variant
/// declare the member, one it adds nothing to is enough; a member with no schema to go
/// by takes anything.
fn wrong_member(
    rt: &MockRuntime,
    t: &str,
    before: &Value,
    after: &Value,
    keys: &[String],
    set: &Map<String, Value>,
) -> Option<String> {
    let record = Place::of_held(rt, write_memory::read_schema(rt, t), Some(before));
    let at = |value: &'_ Value, path: &[String]| -> Option<Value> {
        path.iter()
            .try_fold(value, |v, k| v.get(k.as_str()))
            .cloned()
    };
    for key in keys {
        // A member set by name is checked whole, and so is one set through no path.
        let mut by_path = false;
        for dotted in set.keys().filter(|k| k.contains('.')) {
            let Some((path, schema)) = dotted_path(rt, &record, dotted) else {
                continue;
            };
            if path.first() != Some(key) {
                continue;
            }
            by_path = true;
            let (Some(schema), Some(leaf)) = (schema, at(after, &path)) else {
                continue;
            };
            if adds_to_member(rt, schema, at(before, &path).as_ref(), &leaf) {
                return Some(dotted.clone());
            }
        }
        if by_path && !set.contains_key(key) {
            continue;
        }
        let Some(value) = after.get(key.as_str()) else {
            continue;
        };
        let mut declared = record
            .parts
            .iter()
            .filter_map(|p| p.get("properties")?.get(key.as_str()))
            .peekable();
        if declared.peek().is_some()
            && declared.all(|schema| adds_to_member(rt, schema, before.get(key.as_str()), value))
        {
            return Some(key.clone());
        }
    }
    None
}

/// Whether `after`, a member's value of `schema` that was `before`, has a violation of
/// it `before` has not: it breaks a value that fit, or one that did not (held in an
/// earlier, incomplete shape) gets something new wrong. A value left as it was, or a
/// schema that does not compile, adds none.
fn adds_to_member(rt: &MockRuntime, schema: &Value, before: Option<&Value>, after: &Value) -> bool {
    if before == Some(after) {
        return false;
    }
    let Ok(validator) = mb_engine::SchemaValidator::new(&rt.bundle().with_reachable_defs(schema))
    else {
        return false;
    };
    if validator.is_valid(after) {
        return false;
    }
    match before {
        Some(before) if !validator.is_valid(before) => {
            let held: HashSet<(String, String)> =
                validator.violations(before).into_iter().collect();
            validator
                .violations(after)
                .iter()
                .any(|v| !held.contains(v))
        }
        _ => true,
    }
}

/// `set`, an effect's members for a record of type `t`, with each name written with
/// JSON around it (`"status:"`) the member the type's schema declares (see
/// [`mb_engine::mend_member_names`]); as it is when the type has no schema.
pub(super) fn mended_set(
    rt: &MockRuntime,
    t: &str,
    set: &Map<String, Value>,
) -> Map<String, Value> {
    let Some(schema) = write_memory::read_schema(rt, t) else {
        return set.clone();
    };
    let bundle = rt.bundle();
    let resolve = |r: &str| r.strip_prefix("#/$defs/").and_then(|n| bundle.defs.get(n));
    let mut value = Value::Object(set.clone());
    if mb_engine::mend_member_names(schema, &resolve, &mut value) > 0 {
        tracing::debug!(mock = %rt.name, resource_type = %t, "mended member names in an effect");
    }
    match value {
        Value::Object(mended) => mended,
        _ => set.clone(),
    }
}

/// A faker answer standing in for a failed LLM call (an offline mock's
/// answers are its real ones, and a faker answer that only notes it found no
/// valid draw stands in for nothing).
pub(crate) fn transient_fallback(generated: &GeneratedResponse) -> bool {
    generated.source == Source::Fallback
        && generated
            .fallback_reason
            .as_deref()
            .is_some_and(|r| !r.starts_with(OFFLINE_REASON) && r != FAKER_INVALID_REASON)
}
