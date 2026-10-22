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

    /// Parse LLM output, enforce the size cap, scrub PII, then validate.
    pub(crate) fn check_output(&self, validator: &SchemaValidator, text: &str) -> Checked {
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
        match validator.validate(&value) {
            Ok(()) => Checked::Valid(value),
            Err(errors) => match unwrap_envelope(validator, &value) {
                Some(inner) => Checked::Valid(inner),
                None => match self.patch(validator, &value) {
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

impl Engine {
    fn patch(&self, validator: &SchemaValidator, value: &Value) -> Option<Value> {
        let faker = crate::Faker::new(self.config.faker_depth);
        let mut rng = rand::rng();
        validator.patch(value, |schema| faker.fake(schema, &mut rng))
    }
}

/// The value inside one or two nested one-key objects, when it is valid and
/// the object is not. JSON-object output mode only allows an object at the
/// top level, so models wrap the array (or scalar) an operation returns:
/// `{"orders": [...]}`; asked for a GraphQL field they answer like a GraphQL
/// server: `{"data": {"user": {...}}}`.
fn unwrap_envelope(validator: &SchemaValidator, value: &Value) -> Option<Value> {
    let mut value = value;
    for _ in 0..2 {
        let Value::Object(map) = value else {
            return None;
        };
        if map.len() != 1 {
            return None;
        }
        value = map.values().next()?;
        if validator.validate(value).is_ok() {
            return Some(value.clone());
        }
    }
    None
}
