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
    pub fn new(max: usize) -> Arc<Self> {
        let max = max.min(Semaphore::MAX_PERMITS);
        Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(max)),
            max,
        })
    }

    /// Permits currently held.
    pub fn in_use(&self) -> usize {
        self.max.saturating_sub(self.semaphore.available_permits())
    }

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

pub trait TokenBudget: Send + Sync {
    /// Checked before every LLM call; an implementation records its own metric when it returns true.
    fn exhausted(&self) -> bool;
    fn spend(&self, tokens: u64);
}

/// The guards an [`Engine`] applies to every LLM call. Each is optional.
#[derive(Clone, Default)]
pub struct EngineGuards {
    pub limiter: Option<Arc<GenerationLimiter>>,
    pub budget: Option<Arc<dyn TokenBudget>>,
    pub scrubber: Option<Arc<crate::pii::Scrubber>>,
    pub max_response_bytes: Option<usize>,
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

impl Engine {
    pub fn with_guards(mut self, guards: EngineGuards) -> Self {
        self.guards = guards;
        self
    }

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

    /// Parse LLM output, enforce the size cap, scrub PII, fill what the
    /// output budget left out (see [`crate::budget`]), then validate.
    pub(crate) fn check_output(
        &self,
        validator: &SchemaValidator,
        text: &str,
        fill: Option<&Fill<'_>>,
    ) -> Checked {
        let Some(mut value) = crate::extract_json(text) else {
            return Checked::Invalid(vec!["output is not valid JSON".to_string()]);
        };
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
        match self.check_value(validator, value) {
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
    fn check_value(&self, validator: &SchemaValidator, value: Value) -> Checked {
        match validator.validate(&value) {
            Ok(()) => Checked::Valid(value),
            Err(errors) => match unwrap_envelope(validator, &value)
                .or_else(|| wrap_single_item(validator, &value))
            {
                Some(inner) => Checked::Valid(inner),
                None => match sole_list(&value)
                    .and_then(|list| self.patch(validator, list))
                    .filter(Value::is_array)
                    .or_else(|| self.patch(validator, &value))
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
}

/// What an answer shaped to the output budget needs filled.
pub(crate) struct Fill<'a> {
    pub shaped: &'a crate::budget::Shaped,
    pub hints: &'a crate::FakerHints,
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

    fn patch(&self, validator: &SchemaValidator, value: &Value) -> Option<Value> {
        let faker = crate::Faker::new(self.config.faker_depth);
        let mut rng = rand::rng();
        validator.patch_nodes(value, |node| {
            faker.fake_node(validator.schema(), node, None, validator.costs(), &mut rng)
        })
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
