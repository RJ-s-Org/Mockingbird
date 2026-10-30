//! Guards around every LLM call: a global generation semaphore, a token
//! budget, the output PII scrubber and a response-size cap. A guard that
//! refuses makes the call fall back to the faker; it never errors.

use std::sync::Arc;

use mb_llm::Usage;
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::Engine;
use crate::validate::SchemaValidator;

/// Gauge of generation permits held.
pub const IN_FLIGHT_METRIC: &str = "mb_generation_in_flight";
/// Counter of calls that gave up waiting for a generation permit.
pub const QUEUE_TIMEOUTS_METRIC: &str = "mb_generation_queue_timeouts_total";
/// Counter of PII replacements, labelled by `pattern`.
pub const PII_REPLACEMENTS_METRIC: &str = "mb_pii_replacements_total";

/// Bounds the LLM generations in flight across every engine sharing it.
#[derive(Debug)]
pub struct GenerationLimiter {
    semaphore: Arc<Semaphore>,
    max: usize,
}

impl GenerationLimiter {
    /// A shared limiter of at most `max` generations in flight.
    #[deprecated(
        since = "0.1.1",
        note = "use `GenerationLimiter::new_shared`; in 0.2.0 `new` returns `Self`"
    )]
    pub fn new(max: usize) -> Arc<Self> {
        Self::new_shared(max)
    }

    /// A shared limiter of at most `max` generations in flight (capped at
    /// the semaphore's maximum).
    #[must_use]
    pub fn new_shared(max: usize) -> Arc<Self> {
        let max = max.min(Semaphore::MAX_PERMITS);
        Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(max)),
            max,
        })
    }

    /// Permits currently held.
    #[must_use]
    pub fn in_use(&self) -> usize {
        self.max.saturating_sub(self.semaphore.available_permits())
    }

    /// The most generations in flight.
    #[must_use]
    pub fn max(&self) -> usize {
        self.max
    }

    /// A permit, or `None` once `deadline` passes first.
    async fn acquire_until(&self, deadline: Instant) -> Option<GenerationPermit> {
        let acquire = Arc::clone(&self.semaphore).acquire_owned();
        match tokio::time::timeout_at(deadline, acquire).await {
            Ok(Ok(permit)) => {
                metrics::gauge!(IN_FLIGHT_METRIC).increment(1.0);
                Some(GenerationPermit { _permit: permit })
            }
            // The semaphore is never closed; treat it like a timeout.
            Ok(Err(_)) | Err(_) => {
                metrics::counter!(QUEUE_TIMEOUTS_METRIC).increment(1);
                None
            }
        }
    }
}

/// A held generation slot; released (and the gauge lowered) on drop.
#[derive(Debug)]
pub(crate) struct GenerationPermit {
    _permit: OwnedSemaphorePermit,
}

impl Drop for GenerationPermit {
    fn drop(&mut self) {
        metrics::gauge!(IN_FLIGHT_METRIC).decrement(1.0);
    }
}

/// A budget of LLM tokens the engine checks and charges.
pub trait TokenBudget: Send + Sync {
    /// Checked before every LLM call; an implementation records its own metric when it returns true.
    fn exhausted(&self) -> bool;
    /// Charges `tokens` (prompt plus completion) after a call.
    fn spend(&self, tokens: u64);
}

/// The guards an [`Engine`] applies to every LLM call. Each is optional.
#[derive(Clone, Default)]
pub struct EngineGuards {
    /// Bounds the generations in flight; a call that waits past its
    /// deadline falls back.
    pub limiter: Option<Arc<GenerationLimiter>>,
    /// Refuses calls once exhausted, and is charged what each call used.
    pub budget: Option<Arc<dyn TokenBudget>>,
    /// Replaces PII in LLM output.
    pub scrubber: Option<Arc<crate::pii::Scrubber>>,
    /// Larger LLM output is rejected (and falls back).
    pub max_response_bytes: Option<usize>,
}

impl std::fmt::Debug for EngineGuards {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineGuards")
            .field("limiter", &self.limiter)
            .field("budget", &self.budget.as_ref().map(|_| "TokenBudget"))
            .field("scrubber", &self.scrubber)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

/// LLM output after the guards and validation.
pub(crate) enum Checked {
    Valid(Value),
    /// Valid once the errors a fresh value fixes were patched locally (see
    /// [`SchemaValidator::patch`]).
    Patched(Value),
    /// Repairable: the validation (or parse) errors.
    Invalid(Vec<String>),
    /// Not repairable: the reason to fall back.
    Rejected(String),
}

/// Request values an answer may repeat where they break an asserted
/// `format`: a path id or query value echoed into a `format: uuid` field
/// is kept, not refilled (see [`SchemaValidator::patch_keeping`]).
#[derive(Default)]
pub(crate) struct Echoes<'a>(Vec<&'a Value>);

impl<'a> Echoes<'a> {
    /// The string path and query values of `calls` (and the strings of a
    /// repeated query parameter).
    pub(crate) fn of(calls: impl IntoIterator<Item = &'a crate::OperationCall>) -> Self {
        let mut values = Vec::new();
        for call in calls {
            for value in call.path_params.values().chain(call.query.values()) {
                match value {
                    Value::String(_) => values.push(value),
                    Value::Array(items) => values.extend(items.iter().filter(|v| v.is_string())),
                    _ => {}
                }
            }
        }
        Self(values)
    }

    fn keeps(&self, value: &Value) -> bool {
        self.0.contains(&value)
    }
}

impl Engine {
    /// This engine with `guards` applied to every LLM call.
    #[must_use]
    pub fn with_guards(mut self, guards: EngineGuards) -> Self {
        self.guards = guards;
        self
    }

    /// The guards applied to every LLM call.
    #[must_use]
    pub fn guards(&self) -> &EngineGuards {
        &self.guards
    }

    /// Whether the token budget forbids an LLM call now (logged when so).
    pub(crate) fn budget_exhausted(&self) -> bool {
        let exhausted = self.guards.budget.as_ref().is_some_and(|b| b.exhausted());
        if exhausted {
            tracing::warn!("token budget exhausted");
        }
        exhausted
    }

    /// Admission for an LLM call that must start by `deadline`: `Ok` with the
    /// permit to hold (none without a limiter), `Err` with the reason to fall
    /// back.
    pub(crate) async fn admit(
        &self,
        deadline: Instant,
    ) -> Result<Option<GenerationPermit>, String> {
        if self.budget_exhausted() {
            return Err("token budget exhausted".into());
        }
        match &self.guards.limiter {
            None => Ok(None),
            Some(limiter) => match limiter.acquire_until(deadline).await {
                Some(permit) => Ok(Some(permit)),
                None => Err("timed out waiting for a generation slot".into()),
            },
        }
    }

    /// Charge a completed call's tokens to the budget.
    pub(crate) fn spend(&self, usage: Usage) {
        if let Some(budget) = &self.guards.budget {
            budget.spend(u64::from(usage.prompt_tokens) + u64::from(usage.completion_tokens));
        }
    }

    /// Parse LLM output, unwrap the `{"items": [...]}` envelope when
    /// `checks` asked for one, enforce the size cap, scrub PII, fill what
    /// the output budget left out (see [`crate::budget`]), then validate. A
    /// value breaking an asserted format (see
    /// [`SchemaValidator::for_output`]) is refilled locally unless `echoes`
    /// holds it; a format error alone never makes the answer `Invalid`.
    pub(crate) fn check_output(
        &self,
        validator: &SchemaValidator,
        text: &str,
        checks: &Checks<'_>,
    ) -> Checked {
        let Some(value) = crate::extract_json(text) else {
            return Checked::Invalid(vec![NOT_JSON.to_string()]);
        };
        self.check_parsed(validator, value, text.len(), checks, false)
    }

    /// LLM output cut short (see [`crate::salvage`]), closed, then checked
    /// as [`Engine::check_output`] does with every missing required member
    /// filled by the faker. An answer completed that way is `Patched`, never
    /// `Valid`. `Invalid` when nothing complete could be closed.
    pub(crate) fn check_truncated(
        &self,
        validator: &SchemaValidator,
        text: &str,
        checks: &Checks<'_>,
    ) -> Checked {
        let Some(value) = crate::salvage::close_truncated(text) else {
            return Checked::Invalid(vec![CUT_SHORT.to_string()]);
        };
        match self.check_parsed(validator, value, text.len(), checks, true) {
            Checked::Valid(v) | Checked::Patched(v) => {
                tracing::info!("salvaged an answer cut short");
                Checked::Patched(v)
            }
            other => other,
        }
    }

    /// [`Engine::check_output`] of a parsed `value` (of `text_len` bytes);
    /// with `fill_missing`, every missing required member is filled when
    /// the value does not validate without that.
    fn check_parsed(
        &self,
        validator: &SchemaValidator,
        mut value: Value,
        text_len: usize,
        checks: &Checks<'_>,
        fill_missing: bool,
    ) -> Checked {
        let (fill, echoes) = (checks.fill, &checks.echoes);
        if checks.items_envelope {
            value = unwrap_items(validator, value);
        }
        if let Some(cap) = self.guards.max_response_bytes {
            let size = serde_json::to_vec(&value).map_or(text_len, |b| b.len());
            if size > cap {
                return Checked::Rejected(format!(
                    "response of {size} bytes exceeds the {cap}-byte cap"
                ));
            }
        }
        if let Some(scrubber) = &self.guards.scrubber {
            for (pattern, n) in scrubber.scrub_value(&mut value) {
                metrics::counter!(PII_REPLACEMENTS_METRIC, "pattern" => pattern)
                    .increment(n as u64);
            }
        }
        if let Some(fill) = fill
            && let Some(filled) = self.fill(validator, &value, fill)
        {
            value = filled;
        }
        let checked = if fill_missing {
            // Filled only when the value is not valid as it is (or
            // unwrapped), and never around a wrapper: a wrapped answer has
            // its inner value filled, so its own data stays the answer.
            match self.check_value(validator, value.clone(), echoes) {
                Checked::Invalid(errors) => {
                    let target = wrapped(validator, &value).cloned().unwrap_or(value);
                    if !answers_root(validator, &target) {
                        return Checked::Invalid(errors);
                    }
                    match self.fill_missing(validator, &target) {
                        Some(filled) => self.check_value(validator, filled, echoes),
                        None => Checked::Invalid(errors),
                    }
                }
                checked => checked,
            }
        } else {
            self.check_value(validator, value, echoes)
        };
        match checked {
            Checked::Valid(v) => match validator.truncate_runaway(&v) {
                Some(cut) => {
                    tracing::info!("truncated runaway strings");
                    Checked::Patched(cut)
                }
                None => Checked::Valid(v),
            },
            Checked::Patched(v) => Checked::Patched(validator.truncate_runaway(&v).unwrap_or(v)),
            other => other,
        }
    }

    /// Validate, unwrapping an envelope or patching locally when that makes
    /// the value valid.
    fn check_value(
        &self,
        validator: &SchemaValidator,
        value: Value,
        echoes: &Echoes<'_>,
    ) -> Checked {
        match validator.validate(&value) {
            Ok(()) => self.check_formats(validator, value, echoes),
            Err(errors) => match unwrap_envelope(validator, &value)
                .or_else(|| wrap_single_item(validator, &value))
            {
                Some(inner) => self.check_formats(validator, inner, echoes),
                None => match sole_list(&value)
                    .and_then(|list| self.patch(validator, list, echoes))
                    .filter(Value::is_array)
                    .or_else(|| self.patch(validator, &value, echoes))
                {
                    Some(patched) => {
                        tracing::info!(errors = %errors.join("; "), "patched the answer locally");
                        Checked::Patched(patched)
                    }
                    None => Checked::Invalid(errors),
                },
            },
        }
    }

    /// A valid `value` whose asserted formats hold (or are echoes), else
    /// the value with those formats refilled. When no refill validates, the
    /// value is kept as it is: a format error is never sent for repair.
    fn check_formats(
        &self,
        validator: &SchemaValidator,
        value: Value,
        echoes: &Echoes<'_>,
    ) -> Checked {
        if validator.formats_hold(&value, &|_, v| echoes.keeps(v)) {
            return Checked::Valid(value);
        }
        match self.patch(validator, &value, echoes) {
            Some(patched) => {
                tracing::info!("patched values breaking their format");
                Checked::Patched(patched)
            }
            None => Checked::Valid(value),
        }
    }
}

/// What an answer shaped to the output budget needs filled.
pub(crate) struct Fill<'a> {
    pub shaped: &'a crate::budget::Shaped,
    pub hints: &'a crate::FakerHints,
}

/// What [`Engine::check_output`] needs besides the text: the output-budget
/// fill, if any, the request values the answer may echo, and whether the
/// answer was asked for as an [`ITEMS_KEY`] envelope.
#[derive(Default)]
pub(crate) struct Checks<'a> {
    pub fill: Option<&'a Fill<'a>>,
    pub echoes: Echoes<'a>,
    /// The array the schema describes was asked for as
    /// `{"items": [...]}` (see [`items_envelope`]).
    pub items_envelope: bool,
}

/// The member an array answer is asked for under (see [`items_envelope`]).
pub(crate) const ITEMS_KEY: &str = "items";

/// The instruction added to the prompt when an array answer is asked for
/// as an [`ITEMS_KEY`] envelope.
pub(crate) const ITEMS_ENVELOPE_NOTE: &str = "The response schema describes a JSON array, but \
only an object is allowed at the top level. Output {\"items\": [...]}: one object whose only \
member, \"items\", is the array the schema describes. Add no other members.";

/// The error of output that holds no JSON value.
pub(crate) const NOT_JSON: &str = "output is not valid JSON";

/// The error of output cut short with nothing complete in it.
pub(crate) const CUT_SHORT: &str = "output was cut short before any complete member";

/// The repair request for `answer`, the LLM output that failed with
/// `errors` (shallowest first, at most 10; see
/// [`SchemaValidator::validate`]): what is wrong with its shape, if
/// anything (see [`shape_hints`]), then the errors. `cut` marks output cut
/// short (at `max_tokens`, or inside the value).
pub(crate) fn repair_request(
    validator: &SchemaValidator,
    answer: &str,
    errors: &[String],
    checks: &Checks<'_>,
    cut: bool,
) -> String {
    let mut hints = Vec::new();
    if cut {
        hints.push(
            "Your answer was cut off at the output token limit before the JSON ended. Return a \
complete answer that is shorter: fewer array items, short strings, no optional members."
                .to_string(),
        );
    } else if let Some(value) = crate::extract_json(answer) {
        hints.extend(shape_hints(validator, &value, checks.items_envelope));
    }
    let mut request = String::new();
    for hint in &hints {
        request.push_str(hint);
        request.push('\n');
    }
    if !hints.is_empty() {
        request.push('\n');
    }
    request.push_str("The JSON above is invalid against the schema. Errors:\n");
    for e in errors {
        request.push_str("- ");
        request.push_str(e);
        request.push('\n');
    }
    request.push_str("Return only corrected JSON.");
    request
}

/// What is wrong with the shape of `value` as an answer of `validator`'s
/// schema, in words a model acts on: an empty object, the schema itself, a
/// wrapper around the answer, an object where an array belongs (or the
/// reverse). `items_envelope`: the array was asked for as
/// `{"items": [...]}` (see [`items_envelope`]).
pub(crate) fn shape_hints(
    validator: &SchemaValidator,
    value: &Value,
    items_envelope: bool,
) -> Vec<String> {
    let schema = validator.schema();
    let wants_array = is_array_root(schema);
    let wants_object = !wants_array && root_admits(schema, "object");
    let declared = validator.declared_property_names();
    let declares = |name: &str| declared.is_some_and(|d| d.contains(name));
    let mut hints = Vec::new();
    match value {
        Value::Object(map) if map.is_empty() => hints.push(
            "You returned `{}`, an empty object. Return the whole answer the schema describes, \
with its required members."
                .to_string(),
        ),
        Value::Object(map) => {
            let mut present = map.iter().filter(|(_, v)| !v.is_null());
            let sole = match (present.next(), present.next()) {
                (Some((key, inner)), None) if inner.is_object() || inner.is_array() => {
                    Some((key, inner))
                }
                _ => None,
            };
            let schema_itself = (map.contains_key("$schema")
                || (map.get("properties").is_some_and(Value::is_object)
                    && map.get("type").is_some_and(|t| t == "object")))
                && !declares("$schema")
                && !declares("properties");
            if wants_array && items_envelope {
                if !matches!(sole, Some((key, inner)) if key == ITEMS_KEY && inner.is_array()) {
                    hints.push(format!(
                        "An array was expected. Return it under \"{ITEMS_KEY}\": \
{{\"{ITEMS_KEY}\": [...]}}, with no other members."
                    ));
                }
            } else if wants_array {
                match sole {
                    Some((key, inner)) if inner.is_array() => hints.push(format!(
                        "You wrapped the answer in `{key}`. An array was expected at the root: \
return the array itself."
                    )),
                    _ => hints.push(
                        "An array was expected at the root, but you returned an object."
                            .to_string(),
                    ),
                }
            } else if schema_itself {
                hints.push(
                    "You returned the JSON Schema itself (`$schema`/`properties` at the root). \
Return data that matches the schema, not the schema."
                        .to_string(),
                );
            } else if let Some((key, _)) = sole
                && wants_object
                && !declares(key)
            {
                hints.push(format!(
                    "You wrapped the answer in `{key}`. Return its contents at the top level, \
without the wrapper."
                ));
            }
        }
        Value::Array(_) if wants_object => {
            hints.push("An object was expected at the root, but you returned an array.".to_string())
        }
        _ => {}
    }
    hints
}

/// The array schema inside its `{"items": ...}` wrapping, else `schema`:
/// recordings stay comparable whichever way an array answer was asked for.
#[must_use]
pub fn without_items_schema(schema: &Value) -> Value {
    let Some(root) = schema.as_object() else {
        return schema.clone();
    };
    let keys_match = root
        .keys()
        .all(|k| matches!(k.as_str(), "type" | "required" | "properties" | "$defs"));
    let sole_item = root.get("required") == Some(&serde_json::json!([ITEMS_KEY]))
        && root
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|p| p.len() == 1);
    let Some(mut inner) = root
        .get("properties")
        .and_then(|p| p.get(ITEMS_KEY))
        .filter(|_| keys_match && sole_item)
        .cloned()
    else {
        return schema.clone();
    };
    if let (Some(defs), Some(inner)) = (root.get("$defs"), inner.as_object_mut()) {
        inner.insert("$defs".to_string(), defs.clone());
    }
    inner
}

/// Whether an answer of `schema` (with its definitions) is asked for as an
/// [`ITEMS_KEY`] envelope: the provider can be asked for JSON, which some
/// endpoints only allow as an object at the top level, and the schema's root
/// is an array. Models otherwise invent an envelope of their own
/// (`{"messages": [...]}`), or answer one item.
///
/// A fallback chain reports JSON mode only when every hop uses it (see
/// `FallbackProvider::capabilities`), so a chain mixing `none` and
/// `json_object` hops asks for the bare array. That is deliberate: the
/// prompt cannot depend on which hop answers, and a JSON-mode hop's own
/// envelope (`{"orders": [...]}`) is still unwrapped by the envelope checks
/// in `check_value`.
pub(crate) fn items_envelope(caps: mb_llm::Capabilities, schema: &Value) -> bool {
    caps.json_mode && is_array_root(schema)
}

/// `schema` (an array root) as the object [`items_envelope`] asks for: some
/// endpoints refuse a response schema whose root is not an object.
pub(crate) fn items_schema(mut schema: Value) -> Value {
    let defs = schema.as_object_mut().and_then(|root| root.remove("$defs"));
    let mut wrapped = serde_json::json!({
        "type": "object",
        "required": [ITEMS_KEY],
        "properties": {ITEMS_KEY: schema},
    });
    if let (Some(defs), Some(root)) = (defs, wrapped.as_object_mut()) {
        root.insert("$defs".to_string(), defs);
    }
    wrapped
}

/// `doc`'s root schema object, following local `$ref`s within `doc`.
fn root_schema(doc: &Value) -> Option<&serde_json::Map<String, Value>> {
    let mut schema = doc;
    // Bounded: a `$ref` cycle has no root.
    for _ in 0..8 {
        let obj = schema.as_object()?;
        match obj.get("$ref").and_then(Value::as_str) {
            Some(r) => schema = r.strip_prefix('#').and_then(|p| doc.pointer(p))?,
            None => return Some(obj),
        }
    }
    None
}

/// Whether `doc`'s root admits only arrays (or `null`), following local
/// `$ref`s within `doc`.
fn is_array_root(doc: &Value) -> bool {
    let Some(obj) = root_schema(doc) else {
        return false;
    };
    match obj.get("type") {
        Some(Value::String(t)) => t == "array",
        Some(Value::Array(types)) => {
            types.iter().any(|t| t == "array") && types.iter().all(|t| t == "array" || t == "null")
        }
        Some(_) => false,
        None => obj.contains_key("items") || obj.contains_key("prefixItems"),
    }
}

/// Whether `doc`'s root declares `type` (alone or among others), or, for
/// an object, declares `properties`.
fn root_admits(doc: &Value, type_name: &str) -> bool {
    let Some(obj) = root_schema(doc) else {
        return false;
    };
    match obj.get("type") {
        Some(Value::String(t)) => t == type_name,
        Some(Value::Array(types)) => types.iter().any(|t| t == type_name),
        _ => type_name == "object" && obj.contains_key("properties"),
    }
}

/// The array under [`ITEMS_KEY`] when `value` is the envelope asked for,
/// else `value` as it is (a model may answer the bare array, or one item).
/// It is the envelope when `items` is an array and its only member that is
/// not `null`, or, with other members beside it, when the array validates
/// and the object does not. A single item whose own `items` member is a
/// list (an order with line items) is therefore kept, for
/// `wrap_single_item` to turn into a one-item list.
fn unwrap_items(validator: &SchemaValidator, value: Value) -> Value {
    let Value::Object(map) = &value else {
        return value;
    };
    if map.len() == 1
        && map.get(ITEMS_KEY).is_some_and(Value::is_null)
        && validator.is_valid(&Value::Null)
    {
        return Value::Null;
    }
    let Some(items @ Value::Array(_)) = map.get(ITEMS_KEY) else {
        return value;
    };
    let sole = map.iter().all(|(k, v)| k == ITEMS_KEY || v.is_null());
    if sole || (validator.is_valid(items) && !validator.is_valid(&value)) {
        items.clone()
    } else {
        value
    }
}

impl Engine {
    /// `value` with the required members the output budget left out filled
    /// by the faker from the full schema; `None` when nothing was missing.
    fn fill(&self, validator: &SchemaValidator, value: &Value, fill: &Fill<'_>) -> Option<Value> {
        if !fill.shaped.fills_any() {
            return None;
        }
        let faker = crate::Faker::new(self.config.faker_depth).with_hints(fill.hints.clone());
        let mut rng = rand::rng();
        validator.fill_required_nodes(
            value,
            |location, property| fill.shaped.fills(location, property),
            |node| faker.fake_node(validator.schema(), node, None, validator.costs(), &mut rng),
        )
    }

    /// `value` with every missing required member filled by the faker;
    /// `None` when nothing was missing.
    fn fill_missing(&self, validator: &SchemaValidator, value: &Value) -> Option<Value> {
        let faker = crate::Faker::new(self.config.faker_depth);
        let mut rng = rand::rng();
        validator.fill_required_nodes(
            value,
            |_, _| true,
            |node| faker.fake_node(validator.schema(), node, None, validator.costs(), &mut rng),
        )
    }

    fn patch(
        &self,
        validator: &SchemaValidator,
        value: &Value,
        echoes: &Echoes<'_>,
    ) -> Option<Value> {
        let faker = crate::Faker::new(self.config.faker_depth);
        let mut rng = rand::rng();
        validator.patch_nodes(
            value,
            |_, v| echoes.keeps(v),
            |node| faker.fake_node(validator.schema(), node, None, validator.costs(), &mut rng),
        )
    }
}

/// Whether `value` is an attempt at the root schema worth completing: an
/// object for a root that declares properties holds at least one of them
/// (`{"order": {...}, "others": [...]}` is no order to fill in).
fn answers_root(validator: &SchemaValidator, value: &Value) -> bool {
    match (value, validator.declared_property_names()) {
        (Value::Object(map), Some(declared)) => map.keys().any(|k| declared.contains(k)),
        _ => true,
    }
}

/// The value inside one or two nested one-member objects (`{"payment_intent":
/// {...}}`) whose keys the root schema does not declare, when it has the
/// root's kind (an object for an object root, an array for an array root).
fn wrapped<'v>(validator: &SchemaValidator, value: &'v Value) -> Option<&'v Value> {
    let wants_array = is_array_root(validator.schema());
    let declared = validator.declared_property_names();
    let mut value = value;
    let mut inner = None;
    for _ in 0..2 {
        let Value::Object(map) = value else {
            break;
        };
        let mut present = map.iter().filter(|(_, v)| !v.is_null());
        let (Some((key, next)), None) = (present.next(), present.next()) else {
            break;
        };
        if declared.is_some_and(|d| d.contains(key)) || !(next.is_object() || next.is_array()) {
            break;
        }
        value = next;
        if next.is_array() == wants_array {
            inner = Some(next);
        }
    }
    inner
}

/// The list a one-member object wraps (`{"orders": [...]}`), which a
/// patch may make valid where the wrapper never is.
fn sole_list(value: &Value) -> Option<&Value> {
    let Value::Object(map) = value else {
        return None;
    };
    let mut present = map.values().filter(|v| !v.is_null());
    match (present.next(), present.next()) {
        (Some(list @ Value::Array(_)), None) => Some(list),
        _ => None,
    }
}

/// `[value]` when that is valid: asked for a list (a top-level array),
/// models sometimes answer with its one item.
fn wrap_single_item(validator: &SchemaValidator, value: &Value) -> Option<Value> {
    if !value.is_object() {
        return None;
    }
    let wrapped = Value::Array(vec![value.clone()]);
    validator.is_valid(&wrapped).then_some(wrapped)
}

/// The value inside one or two nested one-key objects, when it is valid and
/// the object is not. JSON-object output mode only allows an object at the
/// top level, so models wrap the array (or scalar) an operation returns:
/// `{"orders": [...]}`; asked for a GraphQL field they answer like a GraphQL
/// server: `{"data": {"user": {...}}}`, sometimes with `"errors": null`
/// beside it. Members that are `null` do not count as keys.
fn unwrap_envelope(validator: &SchemaValidator, value: &Value) -> Option<Value> {
    let mut value = value;
    for _ in 0..2 {
        let Value::Object(map) = value else {
            return None;
        };
        let mut present = map.values().filter(|v| !v.is_null());
        let (Some(inner), None) = (present.next(), present.next()) else {
            return None;
        };
        value = inner;
        if validator.is_valid(value) {
            return Some(value.clone());
        }
    }
    None
}
