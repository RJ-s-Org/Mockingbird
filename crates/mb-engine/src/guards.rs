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
        let (fill, echoes) = (checks.fill, &checks.echoes);
        let Some(mut value) = crate::extract_json(text) else {
            return Checked::Invalid(vec!["output is not valid JSON".to_string()]);
        };
        if checks.items_envelope {
            value = unwrap_items(value);
        }
        if let Some(cap) = self.guards.max_response_bytes {
            let size = serde_json::to_vec(&value).map_or(text.len(), |b| b.len());
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
        match self.check_value(validator, value, echoes) {
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

/// The member an array answer is asked for under in JSON-object mode.
pub(crate) const ITEMS_KEY: &str = "items";

/// The instruction added to the prompt when an array answer is asked for
/// as an [`ITEMS_KEY`] envelope.
pub(crate) const ITEMS_ENVELOPE_NOTE: &str = "The response schema describes a JSON array, but \
JSON mode only allows an object at the top level. Output {\"items\": [...]}: one object whose only \
member, \"items\", is the array the schema describes. Add no other members.";

/// Whether an answer of `schema` (with its definitions) is asked for as an
/// [`ITEMS_KEY`] envelope: the provider is in JSON-object mode (JSON mode
/// without structured outputs), which forces an object at the top level,
/// and the schema's root is an array. Models otherwise invent an envelope
/// of their own (`{"messages": [...]}`), or answer one item.
pub(crate) fn items_envelope(caps: mb_llm::Capabilities, schema: &Value) -> bool {
    caps.json_mode && !caps.json_schema && is_array_root(schema)
}

/// Whether `doc`'s root admits only arrays (or `null`), following local
/// `$ref`s within `doc`.
fn is_array_root(doc: &Value) -> bool {
    let mut schema = doc;
    // Bounded: a `$ref` cycle is not an array.
    for _ in 0..8 {
        let Some(obj) = schema.as_object() else {
            return false;
        };
        if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
            match r.strip_prefix('#').and_then(|p| doc.pointer(p)) {
                Some(target) => {
                    schema = target;
                    continue;
                }
                None => return false,
            }
        }
        return match obj.get("type") {
            Some(Value::String(t)) => t == "array",
            Some(Value::Array(types)) => {
                types.iter().any(|t| t == "array")
                    && types.iter().all(|t| t == "array" || t == "null")
            }
            Some(_) => false,
            None => obj.contains_key("items") || obj.contains_key("prefixItems"),
        };
    }
    false
}

/// The array under [`ITEMS_KEY`] when `value` is an object holding one
/// there, else `value` as it is (a model may answer the bare array).
fn unwrap_items(value: Value) -> Value {
    match value {
        Value::Object(mut map) if map.get(ITEMS_KEY).is_some_and(Value::is_array) => {
            map.remove(ITEMS_KEY).unwrap_or_default()
        }
        other => other,
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
