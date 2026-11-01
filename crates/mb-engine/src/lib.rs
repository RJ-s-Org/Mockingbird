//! Mock response engine: prompt building, LLM generation with
//! validate → repair → fallback, schema validation and schema-driven faking.
//!
//! This is an internal crate of Mockingbird, published so that
//! [`mockingbird-cli`](https://crates.io/crates/mockingbird-cli) can depend
//! on it; it makes no API stability promise. Extensions use
//! [`mb-ext`](https://crates.io/crates/mb-ext) instead.
//!
//! An [`Engine`] answers an [`OperationCall`] of a contract
//! ([`mb_mcm::Bundle`]) with a [`GeneratedResponse`]: from the LLM when it
//! has a provider and the answer validates (or is repaired), otherwise
//! from the [`Faker`]. The faker and the [`SchemaValidator`] also work on
//! their own:
//!
//! ```
//! # fn demo() -> Result<(), mb_engine::EngineError> {
//! use mb_engine::{Faker, SchemaValidator};
//! use serde_json::json;
//!
//! let schema = json!({
//!     "type": "object",
//!     "required": ["id", "name"],
//!     "properties": {
//!         "id": { "type": "integer", "minimum": 1 },
//!         "name": { "type": "string" }
//!     }
//! });
//! let value = Faker::new(4).fake(&schema, &mut rand::rng());
//! assert!(SchemaValidator::new(&schema)?.is_valid(&value));
//! # Ok(())
//! # }
//! # demo().unwrap();
//! ```
#![warn(missing_debug_implementations, missing_docs)]

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use mb_llm::{ChatMessage, ChatRequest, LlmError, Provider, ResponseFormat, Role, Usage};
use mb_mcm::{
    Binding, Bundle, Effect, HttpMethod, Operation, OperationKind, OutputVariant, StatusSelector,
};
use mb_state::StoredResource;
use serde_json::{Map, Value};
use tokio::time::Instant;
use tracing::Instrument;
use tracing::field::Empty;

use crate::guards::{Checked, Checks, Echoes, Fill};

pub mod budget;
pub mod compact;
pub mod consistency;
mod encoded;
pub mod extract;
pub mod faker;
pub mod guards;
pub mod headers;
mod media;
pub mod message;
pub mod ndjson;
pub mod persona;
pub mod pii;
pub mod prompt;
mod salvage;
pub mod scenario;
pub mod seed;
pub mod stream;
pub mod validate;

pub use extract::extract_json;
pub use faker::{Costs, Faker, FakerHints};
pub use guards::{
    EngineGuards, GenerationLimiter, TokenBudget, without_effects_schema, without_items_schema,
};
pub use message::{GeneratedMessage, MessageContext};
pub use prompt::{build_message_messages, build_messages, build_stream_messages};
pub use scenario::{ScenarioSpec, apply_set};
pub use seed::{DEFAULT_FAKER_SEED, SEED_HEADER};
pub use validate::SchemaValidator;

/// Why the engine could not answer at all (a contract problem, not an LLM
/// failure: those fall back to the faker).
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A schema does not compile; the text is the compiler's message.
    #[error("invalid schema: {0}")]
    Schema(String),
    /// The forced status is not one the operation declares.
    #[error("status {0} is not declared by the operation")]
    UndeclaredStatus(u16),
    /// The operation declares no success output to answer with.
    #[error("operation declares no success output")]
    NoOutput,
}

/// A decoded, protocol-neutral invocation of an operation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OperationCall {
    /// The [`Operation::id`] called.
    pub operation_id: String,
    /// Path parameters by name, typed by their schemas.
    pub path_params: Map<String, Value>,
    /// Query parameters by name (an array for a repeated one).
    pub query: Map<String, Value>,
    /// Only non-sensitive headers; the protocol adapter filters them.
    pub headers: Map<String, Value>,
    /// The decoded request body, if any.
    pub body: Option<Value>,
}

/// One root field of a GraphQL query answered by
/// [`Engine::generate_joint`]: its response key, operation, call and context
/// (a 2xx response; `ctx.response_schema` narrows it).
#[derive(Clone, Copy, Debug)]
pub struct JointPart<'a> {
    /// The root field's response key (its alias, or its name).
    pub key: &'a str,
    /// The operation the root field maps to.
    pub op: &'a Operation,
    /// The field's arguments as a call.
    pub call: &'a OperationCall,
    /// The state the field's answer must stay consistent with.
    pub ctx: GenerationContext<'a>,
}

/// State the response must stay consistent with.
#[derive(Clone, Copy, Debug, Default)]
pub struct GenerationContext<'a> {
    /// Stored resources the answer may refer to.
    pub related: &'a [StoredResource],
    /// Existing resource for query/update/delete.
    pub target: Option<&'a StoredResource>,
    /// Answer with this declared status instead of the success one.
    pub forced_status: Option<u16>,
    /// The active scenario: its instructions go in the prompt and its `set`
    /// patches 2xx object bodies.
    pub scenario: Option<&'a ScenarioSpec>,
    /// A narrower schema for a 2xx response, used instead of the operation's
    /// (in the prompt, for validation and by the faker): the fields a
    /// GraphQL selection set asks for. Self-contained (no `$ref`).
    pub response_schema: Option<&'a Value>,
    /// Successful writes the target does not show yet, oldest first: what the
    /// answer must reflect. Scrubbed by whoever kept them. For the prompt only.
    pub earlier_requests: &'a [Value],
    /// What the latest write said of the target itself, when that was an update's
    /// or a create's own body (only what the contract says the target holds): it
    /// stands over what the answer wrote. Last write wins, so with any later write
    /// there is none, and a child's or an action's body is never one.
    pub earlier_own_body: Option<&'a Value>,
    /// Lets an update's LLM answer settle on the stored target (see [`UpdateGuard`]).
    /// With none it does not.
    pub update_guard: Option<&'a dyn UpdateGuard>,
}

/// What only the runtime knows of an update's stored target, for the answer to settle
/// on it.
pub trait UpdateGuard: std::fmt::Debug + Sync {
    /// Whether an answer of this status has the schema of the resource's read.
    fn settles(&self, status: u16) -> bool;
    /// `candidate` with the identity the target holds, and the keys its path reads by.
    fn keep_identity(&self, candidate: Value) -> Value;
    /// Whether `candidate` is still the target's owner's.
    fn keeps_owner(&self, candidate: &Value) -> bool;
}

/// What a credential's value becomes in a body kept beyond its request (see
/// [`GenerationContext::earlier_requests`]): the request log's text.
pub const REDACTED: &str = "[redacted]";

/// Where a response or item came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// The LLM's answer, valid as it came.
    Llm,
    /// The LLM's first answer after a local fix (a patched value, a
    /// truncated runaway string), with no second LLM call.
    Patched,
    /// The LLM's answer after a repair round trip (a second LLM call).
    Repaired,
    /// The faker's answer (no LLM, or the LLM's answer was unusable).
    Fallback,
    /// Served from the session's stored resource, without the LLM.
    State,
    /// A repeat of an earlier answer in the same session.
    Cache,
    /// The request failed validation: the contract's error shape (or a
    /// problem) answered it, without the LLM.
    Rejected,
}

impl Source {
    /// The less trustworthy of two sources: `Fallback` > `Repaired` >
    /// `Patched` > `Llm`.
    #[must_use]
    pub fn worst(self, other: Source) -> Source {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }

    fn rank(self) -> u8 {
        match self {
            Source::Llm | Source::State | Source::Cache | Source::Rejected => 0,
            Source::Patched => 1,
            Source::Repaired => 2,
            Source::Fallback => 3,
        }
    }

    /// The lowercase name (`llm`, `patched`, `repaired`, `fallback`...), also its
    /// [`Display`](fmt::Display) form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Llm => "llm",
            Source::Patched => "patched",
            Source::Repaired => "repaired",
            Source::Fallback => "fallback",
            Source::State => "state",
            Source::Cache => "cache",
            Source::Rejected => "rejected",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A change an LLM answer asks for on another stored resource: the fields
/// of `set` merged into resource `id` of `resource_type`. The engine only
/// reads it from the answer; the caller checks it against the session's state
/// and applies it.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceEffect {
    /// The resource's type.
    pub resource_type: String,
    /// The resource's id.
    pub id: String,
    /// The members to set on it.
    pub set: Map<String, Value>,
}

/// What became of a [`ResourceEffect`].
#[derive(Clone, Debug, PartialEq)]
pub struct EffectOutcome {
    /// The effect.
    pub effect: ResourceEffect,
    /// Why it was not applied; `None` when it was.
    pub rejected: Option<String>,
    /// The whole resource after it, when it was applied.
    pub merged: Option<Value>,
}

/// A generated answer to one call.
#[derive(Clone, Debug, PartialEq)]
pub struct GeneratedResponse {
    /// The declared status answered.
    pub status: u16,
    /// The variant's media type, when it declares one.
    pub content_type: Option<String>,
    /// The body; `None` for a variant without a schema.
    pub body: Option<Value>,
    /// Where the body came from.
    pub source: Source,
    /// Tokens spent on this response, summed over the primary and repair
    /// calls; zero when no LLM call was made.
    pub usage: Usage,
    /// Why the faker answered instead of the LLM, when it did because an LLM
    /// call failed or was not made (`None` for LLM answers and for
    /// deliberate fallbacks such as a forced status with no LLM). A faker
    /// answer that matched no schema-valid draw carries
    /// [`FAKER_INVALID_REASON`], after the primary reason when there is one.
    pub fallback_reason: Option<String>,
    /// What the LLM's answer changes in other stored resources, as it said
    /// (see [`ResourceEffect`]); empty for anything but an LLM's answer to a
    /// command that holds stored context.
    pub effects: Vec<ResourceEffect>,
    /// What became of [`GeneratedResponse::effects`], for the record: filled
    /// by the caller that applies them.
    pub effect_outcomes: Vec<EffectOutcome>,
    /// Effects the answer listed that never reached
    /// [`GeneratedResponse::effects`]: malformed, over the limit of 10, or
    /// too large for the response-size cap.
    pub dropped_effects: usize,
    /// Whether this answer of an action is a new record of its target's type,
    /// not the target (see `reflect`): it is stored under its own
    /// identity and the target is left as it is.
    pub new_record: bool,
}

/// How an [`Engine`] generates; [`Default`] gives the documented defaults.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Deadline of one generation, LLM calls and repairs included.
    pub timeout: Duration,
    /// Sampling temperature of the LLM calls.
    pub temperature: f32,
    /// Completion-token cap of one LLM call.
    pub max_tokens: u32,
    /// The mock's persona text, added to every prompt; the formats it
    /// declares are kept on generated bodies.
    pub persona: Option<String>,
    /// How deep the faker nests before it stops at required members.
    pub faker_depth: usize,
    /// Limits and pacing of streams.
    pub stream: StreamConfig,
    /// Completion tokens an answer is shaped to (see [`budget`]); 0 turns
    /// shaping off.
    pub output_budget: u32,
}

/// Limits and pacing for [`Engine::generate_stream`].
#[derive(Clone, Debug)]
pub struct StreamConfig {
    /// Items generated when the caller asks for no particular count.
    pub default_items: usize,
    /// Upper bound on items per stream.
    pub max_items: usize,
    /// Pause before every item except the first.
    pub item_interval: Duration,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            default_items: 5,
            max_items: 50,
            item_interval: Duration::ZERO,
        }
    }
}

/// Owned state a stream's items must stay consistent with.
#[derive(Clone, Debug, Default)]
pub struct StreamContext {
    /// Stored resources the items may refer to.
    pub related: Vec<StoredResource>,
    /// The existing resource the stream is about, if any.
    pub target: Option<StoredResource>,
    /// The active scenario: its instructions go in the prompt and its `set`
    /// patches object items.
    pub scenario: Option<ScenarioSpec>,
    /// Items the client has sent so far (client and bidi streams).
    pub request_stream: Vec<Value>,
    /// Overrides [`StreamConfig::item_interval`].
    pub item_interval: Option<Duration>,
}

/// One generated stream item.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamItem {
    /// The item.
    pub value: Value,
    /// Where the item came from.
    pub source: Source,
    /// Tokens attributed to this item: its share of the provider stream's
    /// estimate (see [`Engine::generate_stream`]) plus any repair call; zero
    /// for a faker item that read nothing from the provider.
    pub usage: Usage,
}

/// The fallback reason of a mock with no LLM provider. A reason starting
/// with it (see [`FAKER_INVALID_REASON`]) is an offline answer too.
pub const OFFLINE_REASON: &str = "no LLM is configured for this mock (offline)";

/// Added to a faker answer's fallback reason (after `; ` when there is one)
/// when no faker draw validated against the schema, so the answer is not
/// passed off as schema-valid. The faker does not generate for `not` or
/// `if`/`then`/`else`; such schemas rely on its retries and can end here.
pub const FAKER_INVALID_REASON: &str = "faker output does not validate against the schema";

/// The fallback reason of a REST answer in a non-JSON media type (`text/*`,
/// a declared example of any media type, an empty binary body), which the
/// LLM never writes.
pub const NON_JSON_MEDIA_REASON: &str =
    "the response media type is not JSON: answered from the declared example or the faker";

/// Counter of faker answers that matched no schema-valid draw.
pub const FAKER_INVALID_METRIC: &str = "mb_faker_invalid_total";

/// Counter of scenario `set` patches dropped at request time, labelled by
/// `reason`: `invalid` (the patched body would not validate) or
/// `not_an_object` (the body is not a JSON object).
pub const SCENARIO_SET_IGNORED_METRIC: &str = "mb_scenario_set_ignored_total";

/// `reason`, followed by the faker's own note when it has one.
fn with_faker_note(reason: String, faker: Option<String>) -> String {
    match faker {
        Some(note) => format!("{reason}; {note}"),
        None => reason,
    }
}

/// Faker attempts (cycling through top-level `oneOf`/`anyOf` branches and
/// fresh random draws) before an unvalidated fallback body is accepted.
const FALLBACK_ATTEMPTS: usize = 12;

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
            temperature: 0.7,
            max_tokens: 2048,
            persona: None,
            faker_depth: 6,
            stream: StreamConfig::default(),
            output_budget: budget::DEFAULT_OUTPUT_BUDGET,
        }
    }
}

/// Generates schema-valid responses, preferring the LLM and falling back to
/// the faker.
pub struct Engine {
    /// `None` means offline: every response comes from the faker.
    provider: Option<Arc<dyn Provider>>,
    config: EngineConfig,
    /// Keyed by (`<source digest>/<operation id>`, status).
    validators: DashMap<(String, u16), Arc<SchemaValidator>>,
    /// Keyed like `validators`, plus the narrowed schema and the requested
    /// words that name a member.
    shapes: DashMap<ShapeKey, Option<Arc<budget::Shaped>>>,
    /// The member names of each (`<source digest>/<operation id>`, status)
    /// schema, which decide which requested words matter for its shape.
    member_names: DashMap<(String, u16), Arc<MemberNames>>,
    /// Validators of schemas that are not an operation's own (a narrowed
    /// GraphQL selection, a joint answer, an event key, header or address
    /// parameter schema), keyed by (`<source digest>`, schema text); at most
    /// [`SCHEMA_VALIDATORS_MAX`], emptied when full.
    schema_validators: DashMap<(String, String), Arc<SchemaValidator>>,
    /// What the faker needs of a schema (it with the bundle's definitions
    /// it reaches, and its cost table), keyed and bounded like
    /// `schema_validators`.
    fake_setups: DashMap<(String, String), Arc<FakeSetup>>,
    /// The formats the persona declares, kept on every generated body.
    persona_formats: persona::PersonaFormats,
    guards: EngineGuards,
    /// Seeds the faker's draws (see [`seed`]); `None` draws at random.
    faker_seed: Option<u64>,
}

/// The schema a response answers and what it belongs to: the input of
/// [`Engine::finish`].
#[derive(Clone, Copy)]
struct Target<'a> {
    validator: &'a SchemaValidator,
    bundle: &'a Bundle,
    schema: &'a Value,
    op: &'a Operation,
}

/// (`<source digest>/<operation id>`, status, narrowed schema digest,
/// requested member names).
type ShapeKey = (
    String,
    u16,
    Option<u64>,
    Vec<String>,
    Option<budget::PageLimit>,
);

/// A schema's member names, by their stem (see [`budget::stem`]).
type MemberNames = HashMap<String, Vec<String>>;

/// Most validators [`Engine::schema_validator`] keeps.
const SCHEMA_VALIDATORS_MAX: usize = 512;

/// Most faker setups [`Engine::fake_setup`] keeps.
const FAKE_SETUPS_MAX: usize = 512;

/// What the faker draws one answer with: the request values it echoes
/// and the answer's generator.
struct AnswerDraws<'a> {
    hints: FakerHints,
    draws: &'a mut seed::Draws,
}

/// A schema ready for the faker: with the definitions it reaches, and the
/// cost table of that.
struct FakeSetup {
    schema: Value,
    costs: Costs,
}

impl FakeSetup {
    fn new(bundle: &Bundle, schema: &Value) -> Self {
        let schema = bundle.with_reachable_defs(schema);
        let costs = Costs::compute(&schema);
        Self { schema, costs }
    }
}

/// Prints the configuration and cache sizes; never the provider, which
/// holds the LLM credentials.
impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("has_provider", &self.provider.is_some())
            .field("config", &self.config)
            .field("faker_seed", &self.faker_seed)
            .field("guards", &self.guards)
            .field("validators", &self.validators.len())
            .field("schema_validators", &self.schema_validators.len())
            .field("fake_setups", &self.fake_setups.len())
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// An engine generating with `provider`, or offline (faker only) when
    /// it is `None`, with no guards (see [`Engine::with_guards`]). An
    /// offline engine's faker is seeded with [`DEFAULT_FAKER_SEED`]; one
    /// with a provider draws its fallbacks at random until
    /// [`Engine::with_faker_seed`] sets a seed.
    #[must_use]
    pub fn new(provider: Option<Arc<dyn Provider>>, config: EngineConfig) -> Self {
        let persona_formats = config
            .persona
            .as_deref()
            .map(persona::PersonaFormats::parse)
            .unwrap_or_default();
        Self {
            faker_seed: provider.is_none().then_some(DEFAULT_FAKER_SEED),
            persona_formats,
            provider,
            config,
            validators: DashMap::new(),
            shapes: DashMap::new(),
            member_names: DashMap::new(),
            schema_validators: DashMap::new(),
            fake_setups: DashMap::new(),
            guards: EngineGuards::default(),
        }
    }

    /// The configuration the engine was built with.
    #[must_use]
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// This engine with its faker seeded by `seed`: the same seed, contract
    /// and call give the same faker values (see [`seed`]). A call's
    /// [`SEED_HEADER`] overrides it.
    #[must_use]
    pub fn with_faker_seed(mut self, seed: u64) -> Self {
        self.faker_seed = Some(seed);
        self
    }

    /// The seed of the faker's draws when a call names none; `None` when
    /// they are random.
    #[must_use]
    pub fn faker_seed(&self) -> Option<u64> {
        self.faker_seed
    }

    /// The faker draws of one answer: seeded by the call's [`SEED_HEADER`],
    /// else the engine's seed, mixed with what identifies the answer (the
    /// contract, operation, status and request); random without a seed.
    fn call_draws(
        &self,
        bundle: &Bundle,
        op_id: &str,
        status: u16,
        call: &OperationCall,
    ) -> seed::Draws {
        let [path, query, body] = seed::call_parts(call);
        let status = status.to_string();
        seed::Draws::new(
            seed::request_seed(call).or(self.faker_seed),
            &[&bundle.source_digest, op_id, &status],
            &[&path, &query, &body],
        )
    }

    /// The faker draws of a value that answers no call (an event message,
    /// a request value): always random.
    fn unseeded_draws() -> seed::Draws {
        seed::Draws::random()
    }

    /// Whether the engine has an LLM provider; without one every response
    /// comes from the faker, and prompt-only inputs (such as
    /// [`StreamContext::request_stream`]) are never read.
    #[must_use]
    pub fn has_provider(&self) -> bool {
        self.provider.is_some()
    }

    /// Whether `value` is a valid stream item of `op`; `false` when `op`
    /// has no stream item schema.
    #[must_use]
    pub fn conform_item(&self, bundle: &Bundle, op: &Operation, value: &Value) -> bool {
        let Some(schema) = op.stream_item_schema() else {
            return false;
        };
        let Some(status) = op.success_output().map(|o| success_status(o.status)) else {
            return false;
        };
        match self.validator(bundle, op, status, schema) {
            Ok(validator) => validator.is_valid(value),
            Err(e) => {
                tracing::warn!(operation = %op.id, error = %e, "stream item schema cannot be validated");
                false
            }
        }
    }

    /// Answers `call` of `op`: with the success output, or the variant of
    /// `ctx.forced_status`. The LLM answers when there is a provider and its
    /// answer validates (after a local patch or a repair round); otherwise
    /// the faker does, and [`GeneratedResponse::fallback_reason`] says why.
    /// The body is then made consistent with the request, the state and
    /// the scenario.
    ///
    /// # Errors
    ///
    /// [`EngineError::UndeclaredStatus`] when `op` does not declare the
    /// forced status, and [`EngineError::NoOutput`] when it declares no
    /// success output. A failed or invalid LLM answer is not an error.
    pub async fn generate(
        &self,
        bundle: &Bundle,
        op: &Operation,
        call: &OperationCall,
        ctx: GenerationContext<'_>,
    ) -> Result<GeneratedResponse, EngineError> {
        let (variant, status) = match ctx.forced_status {
            Some(code) => (
                op.output_for_status(code)
                    .ok_or(EngineError::UndeclaredStatus(code))?,
                code,
            ),
            None => {
                let variant = op.success_output().ok_or(EngineError::NoOutput)?;
                (variant, success_status(variant.status))
            }
        };

        let narrowed = ctx
            .response_schema
            .filter(|_| (200..300).contains(&status) && variant.schema.is_some());
        let Some(schema) = narrowed.or(variant.schema.as_ref()) else {
            // Nothing to generate: the contract decides the answer. Offline,
            // it is labelled the faker's, like every other offline answer.
            let (source, fallback_reason) = match self.provider {
                Some(_) => (Source::Llm, None),
                None => (Source::Fallback, Some(OFFLINE_REASON.to_string())),
            };
            return Ok(GeneratedResponse {
                status,
                content_type: variant.content_type.clone(),
                body: None,
                source,
                usage: Usage::default(),
                fallback_reason,
                effects: Vec::new(),
                effect_outcomes: Vec::new(),
                dropped_effects: 0,
                new_record: false,
            });
        };
        if let Some(answer) = media::answer(op, variant, schema) {
            return Ok(self.media_response(bundle, op, variant, status, call, answer));
        }

        // A narrowed schema varies per query: its validator is cached by
        // the schema itself.
        let validator = match narrowed {
            Some(schema) => self.schema_validator(bundle, schema)?,
            None => self.validator(bundle, op, status, schema)?,
        };
        let mut usage = Usage::default();
        let mut draws = self.call_draws(bundle, &op.id, status, call);
        let mut response = match &self.provider {
            None => {
                let mut fake = self.fake_response(
                    bundle,
                    variant,
                    status,
                    schema,
                    Some(&validator),
                    &mut AnswerDraws {
                        hints: FakerHints::from_call(op, call),
                        draws: &mut draws,
                    },
                );
                fake.fallback_reason = Some(with_faker_note(
                    OFFLINE_REASON.to_string(),
                    fake.fallback_reason.take(),
                ));
                fake
            }
            Some(provider) => {
                let shaped = self.shaped(bundle, op, status, schema, narrowed.is_some(), call);
                let written = shaped.as_ref().map_or(schema, |s| &s.schema);
                let mut messages = tracing::info_span!("mb.prompt").in_scope(|| {
                    build_messages(
                        bundle,
                        op,
                        status,
                        written,
                        call,
                        &ctx,
                        self.config.persona.as_deref(),
                    )
                });
                let effects_envelope = guards::effects_envelope(op, status, &ctx, &validator);
                // The effects envelope's root is an object already.
                let items_envelope = !effects_envelope
                    && guards::items_envelope(provider.capabilities(), validator.schema());
                if let Some(user) = messages.last_mut() {
                    if effects_envelope {
                        user.content.push_str("\n\n");
                        user.content
                            .push_str(&prompt::effects_note(op, status, &ctx));
                    } else if items_envelope {
                        user.content.push_str("\n\n");
                        user.content.push_str(guards::ITEMS_ENVELOPE_NOTE);
                    }
                }
                let prompt_schema = compact::response_prompt_schema(bundle, written);
                let format = ResponseFormat::JsonSchema {
                    name: format_name(&op.id),
                    schema: if effects_envelope {
                        guards::effects_schema(prompt_schema)
                    } else if items_envelope {
                        guards::items_schema(prompt_schema)
                    } else {
                        prompt_schema
                    },
                };
                let hints = FakerHints::from_call(op, call);
                let fill = shaped.as_deref().map(|shaped| Fill {
                    shaped,
                    hints: &hints,
                });
                match self
                    .generate_with_llm(
                        provider.as_ref(),
                        &validator,
                        messages,
                        &format,
                        &Checks {
                            fill: fill.as_ref(),
                            echoes: Echoes::of([call]),
                            items_envelope,
                            effects_envelope,
                        },
                        &mut usage,
                    )
                    .await
                {
                    Ok(answer) => GeneratedResponse {
                        status,
                        content_type: variant.content_type.clone(),
                        body: Some(answer.value),
                        source: answer.source,
                        usage: Usage::default(),
                        fallback_reason: None,
                        effects: answer.effects.kept,
                        effect_outcomes: Vec::new(),
                        dropped_effects: answer.effects.dropped,
                        new_record: false,
                    },
                    Err(reason) => {
                        tracing::warn!(operation = %op.id, %reason, "LLM generation fell back to faker");
                        let mut fake = self.fake_response(
                            bundle,
                            variant,
                            status,
                            schema,
                            Some(&validator),
                            &mut AnswerDraws {
                                hints: FakerHints::from_call(op, call),
                                draws: &mut draws,
                            },
                        );
                        fake.fallback_reason =
                            Some(with_faker_note(reason, fake.fallback_reason.take()));
                        fake
                    }
                }
            }
        };
        response.usage = usage;
        let target = Target {
            validator: &validator,
            bundle,
            schema,
            op,
        };
        Ok(self.finish(target, call, &ctx, response, &mut draws))
    }

    /// Answers every part with one LLM call: one object keyed by the parts'
    /// keys (a nullable part may be left out and reads as null), validated (and repaired) as a whole, then split and finished per
    /// part. The call's usage is reported on the first part. Without a
    /// provider, for a part with a forced status or no schema, or when the
    /// joint answer stays invalid, every part is generated on its own.
    pub async fn generate_joint(
        &self,
        bundle: &Bundle,
        parts: &[JointPart<'_>],
    ) -> Vec<Result<GeneratedResponse, EngineError>> {
        match self.try_joint(bundle, parts).await {
            Some(responses) => responses.into_iter().map(Ok).collect(),
            None => {
                futures::future::join_all(
                    parts
                        .iter()
                        .map(|p| self.generate(bundle, p.op, p.call, p.ctx)),
                )
                .await
            }
        }
    }

    async fn try_joint(
        &self,
        bundle: &Bundle,
        parts: &[JointPart<'_>],
    ) -> Option<Vec<GeneratedResponse>> {
        let provider = self.provider.as_ref()?;
        let mut shapes = Vec::with_capacity(parts.len());
        let mut properties = Map::new();
        for part in parts {
            if part.ctx.forced_status.is_some() {
                return None;
            }
            let variant = part.op.success_output()?;
            if media::is_non_json(part.op, variant) {
                return None;
            }
            let status = success_status(variant.status);
            let schema = part.ctx.response_schema.or(variant.schema.as_ref())?;
            let validator = match part.ctx.response_schema {
                Some(s) => self.schema_validator(bundle, s).ok()?,
                None => self.validator(bundle, part.op, status, schema).ok()?,
            };
            properties.insert(part.key.to_string(), schema.clone());
            shapes.push((variant, status, validator));
        }
        // A part that may be null may also be left out: models drop the
        // fields they mean as null.
        let keys: Vec<Value> = parts
            .iter()
            .zip(&shapes)
            .filter(|(_, (_, _, validator))| !validator.is_valid(&Value::Null))
            .map(|(p, _)| Value::from(p.key))
            .collect();
        let combined = serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": keys,
        });
        let validator = self.schema_validator(bundle, &combined).ok()?;
        let prompt_schema = compact::response_prompt_schema(bundle, &combined);
        let messages = tracing::info_span!("mb.prompt").in_scope(|| {
            prompt::build_joint_messages(
                bundle,
                parts,
                &prompt_schema,
                self.config.persona.as_deref(),
            )
        });
        let format = ResponseFormat::JsonSchema {
            name: "query".into(),
            schema: prompt_schema,
        };
        let mut usage = Usage::default();
        let (value, source) = match self
            .generate_with_llm(
                provider.as_ref(),
                &validator,
                messages,
                &format,
                &Checks {
                    fill: None,
                    echoes: Echoes::of(parts.iter().map(|p| p.call)),
                    items_envelope: false,
                    effects_envelope: false,
                },
                &mut usage,
            )
            .await
        {
            Ok(answer) => (answer.value, answer.source),
            Err(reason) => {
                tracing::info!(%reason, "joint generation failed; answering each field on its own");
                return None;
            }
        };
        let mut responses = Vec::with_capacity(parts.len());
        for (i, (part, (variant, status, validator))) in parts.iter().zip(shapes).enumerate() {
            let response = GeneratedResponse {
                status,
                content_type: variant.content_type.clone(),
                body: Some(value.get(part.key).cloned().unwrap_or(Value::Null)),
                source,
                usage: if i == 0 { usage } else { Usage::default() },
                fallback_reason: None,
                effects: Vec::new(),
                effect_outcomes: Vec::new(),
                dropped_effects: 0,
                new_record: false,
            };
            let schema = part
                .ctx
                .response_schema
                .or(variant.schema.as_ref())
                .unwrap_or(&Value::Null);
            let target = Target {
                validator: &validator,
                bundle,
                schema,
                op: part.op,
            };
            let mut draws = self.call_draws(bundle, &part.op.id, status, part.call);
            responses.push(self.finish(target, part.call, &part.ctx, response, &mut draws));
        }
        Some(responses)
    }

    /// A faker-generated response for `variant`, tagged `Fallback`. The body
    /// is validated against the variant's schema (retrying other branches and
    /// draws) and, for 2xx responses, made consistent with the request and
    /// state exactly like an LLM response would be.
    pub fn fallback(
        &self,
        bundle: &Bundle,
        op: &Operation,
        variant: &OutputVariant,
        status: u16,
        call: &OperationCall,
        ctx: &GenerationContext<'_>,
    ) -> GeneratedResponse {
        let Some(schema) = &variant.schema else {
            return GeneratedResponse {
                status,
                content_type: variant.content_type.clone(),
                body: None,
                source: Source::Fallback,
                usage: Usage::default(),
                fallback_reason: None,
                effects: Vec::new(),
                effect_outcomes: Vec::new(),
                dropped_effects: 0,
                new_record: false,
            };
        };
        if let Some(answer) = media::answer(op, variant, schema) {
            return self.media_response(bundle, op, variant, status, call, answer);
        }
        let mut draws = self.call_draws(bundle, &op.id, status, call);
        match self.validator(bundle, op, status, schema) {
            Ok(validator) => {
                let response = self.fake_response(
                    bundle,
                    variant,
                    status,
                    schema,
                    Some(&validator),
                    &mut AnswerDraws {
                        hints: FakerHints::from_call(op, call),
                        draws: &mut draws,
                    },
                );
                let target = Target {
                    validator: &validator,
                    bundle,
                    schema,
                    op,
                };
                self.finish(target, call, ctx, response, &mut draws)
            }
            Err(e) => {
                tracing::warn!(operation = %op.id, error = %e, "fallback schema cannot be validated");
                self.fake_response(
                    bundle,
                    variant,
                    status,
                    schema,
                    None,
                    &mut AnswerDraws {
                        hints: FakerHints::from_call(op, call),
                        draws: &mut draws,
                    },
                )
            }
        }
    }

    /// `stored` served as `op`'s success answer, tagged `State`, when the
    /// success variant has a schema (the narrowed `response_schema` when
    /// given) and `stored` validates against it. `None` otherwise: a partial
    /// resource stored by a create is generated and overlaid instead.
    pub fn serve_stored(
        &self,
        bundle: &Bundle,
        op: &Operation,
        stored: &Value,
        response_schema: Option<&Value>,
    ) -> Option<GeneratedResponse> {
        let variant = op.success_output()?;
        let status = success_status(variant.status);
        let schema = variant.schema.as_ref()?;
        let validator = match response_schema {
            Some(narrowed) => self
                .schema_validator(bundle, narrowed)
                .inspect_err(|e| {
                    tracing::warn!(operation = %op.id, error = %e, "narrowed schema cannot be validated");
                })
                .ok()?,
            None => self
                .validator(bundle, op, status, schema)
                .inspect_err(|e| {
                    tracing::warn!(operation = %op.id, error = %e, "response schema cannot be validated");
                })
                .ok()?,
        };
        validator.is_valid(stored).then_some(())?;
        Some(GeneratedResponse {
            status,
            content_type: variant.content_type.clone(),
            body: Some(stored.clone()),
            source: Source::State,
            usage: Usage::default(),
            fallback_reason: None,
            effects: Vec::new(),
            effect_outcomes: Vec::new(),
            dropped_effects: 0,
            new_record: false,
        })
    }

    /// A `404` for a resource the session does not hold: the operation's
    /// declared `404` response, faker-filled, else a problem+json body with
    /// `detail`. Always tagged `State`.
    pub fn not_found(
        &self,
        bundle: &Bundle,
        op: &Operation,
        call: &OperationCall,
        detail: &str,
    ) -> GeneratedResponse {
        let declared = op.output_for_status(404).map(|variant| {
            let ctx = GenerationContext {
                forced_status: Some(404),
                ..GenerationContext::default()
            };
            self.fallback(bundle, op, variant, 404, call, &ctx)
        });
        let mut response = declared.unwrap_or_else(|| GeneratedResponse {
            status: 404,
            content_type: Some("application/problem+json".into()),
            body: Some(serde_json::json!({
                "type": "about:blank",
                "title": "Not Found",
                "status": 404,
                "detail": detail,
            })),
            source: Source::State,
            usage: Usage::default(),
            fallback_reason: None,
            effects: Vec::new(),
            effect_outcomes: Vec::new(),
            dropped_effects: 0,
            new_record: false,
        });
        response.source = Source::State;
        response.fallback_reason = None;
        response
    }

    /// Post-process a generated response: reflect request and state, keep
    /// the persona's formats, give a list read unique item ids, then apply
    /// the scenario's `set` patch.
    fn finish(
        &self,
        target: Target<'_>,
        call: &OperationCall,
        ctx: &GenerationContext<'_>,
        response: GeneratedResponse,
        draws: &mut seed::Draws,
    ) -> GeneratedResponse {
        let Target {
            validator,
            bundle,
            schema,
            op,
        } = target;
        let response = encode_base64(validator, bundle, schema, response);
        let response = reflect(validator, op, call, ctx, response);
        let response = self.keep_persona_formats(validator, call, response, draws);
        let response = if matches!(op.kind, OperationKind::Query)
            && ctx.target.is_none()
            && (200..300).contains(&response.status)
        {
            self.unique_list_ids(bundle, schema, op, validator, response, draws)
        } else {
            response
        };
        apply_scenario(validator, op, call, ctx, response)
    }

    /// A 2xx body whose values break a format the persona declares, patched
    /// (values the path or request body set are kept).
    fn keep_persona_formats(
        &self,
        validator: &SchemaValidator,
        call: &OperationCall,
        mut response: GeneratedResponse,
        draws: &mut seed::Draws,
    ) -> GeneratedResponse {
        if self.persona_formats.is_empty() || !(200..300).contains(&response.status) {
            return response;
        }
        let Some(body) = &response.body else {
            return response;
        };
        let mut protected: Vec<&Value> = call.path_params.values().collect();
        if let Some(Value::Object(request)) = &call.body {
            protected.extend(request.values());
        }
        if let Some(patched) =
            self.persona_formats
                .apply_with(validator, body, &protected, &mut draws.rng)
        {
            tracing::info!(operation = %call.operation_id, "patched values to the persona's formats");
            response.body = Some(patched);
        }
        response
    }

    /// A list body whose items share an identity gets a fresh value for
    /// each later duplicate, kept only if the whole body still validates.
    fn unique_list_ids(
        &self,
        bundle: &Bundle,
        schema: &Value,
        op: &Operation,
        validator: &SchemaValidator,
        mut response: GeneratedResponse,
        draws: &mut seed::Draws,
    ) -> GeneratedResponse {
        let Some(body) = &response.body else {
            return response;
        };
        let field = consistency::item_identity(bundle, op, body);
        let Some(items) = consistency::list_items(body) else {
            return response;
        };
        let mut seen = std::collections::HashSet::new();
        let duplicate = items
            .iter()
            .filter_map(|item| item.get(&field))
            .any(|id| !seen.insert(id.to_string()));
        if !duplicate {
            return response;
        }
        let Some(id_schema) = consistency::item_schema(bundle, schema, body)
            .and_then(|item| consistency::property_schema(bundle, &item, &field))
        else {
            return response;
        };
        let mut fixed = body.clone();
        let Some(items) = consistency::list_items_mut(&mut fixed) else {
            return response;
        };
        let mut used: std::collections::HashSet<String> = items
            .iter()
            .filter_map(|item| item.get(&field))
            .map(Value::to_string)
            .collect();
        let mut kept = std::collections::HashSet::new();
        for item in items.iter_mut() {
            let Some(id) = item.get(&field) else {
                continue;
            };
            if kept.insert(id.to_string()) {
                continue;
            }
            let fresh = (0..IDENTITY_DRAWS)
                .map(|_| self.fake_field(bundle, &field, &id_schema, draws))
                .find(|v| !v.is_null() && !used.contains(&v.to_string()));
            if let (Some(fresh), Value::Object(obj)) = (fresh, &mut *item) {
                used.insert(fresh.to_string());
                kept.insert(fresh.to_string());
                obj.insert(field.clone(), fresh);
            }
        }
        if validator.is_valid(&fixed) {
            response.body = Some(fixed);
        }
        response
    }

    /// `body` (a create's answer) with its identity field (`op`'s, else
    /// `id`) replaced by a different faker value that keeps it valid
    /// against `op`'s success schema; `None` when none is found.
    pub fn fresh_identity(&self, bundle: &Bundle, op: &Operation, body: &Value) -> Option<Value> {
        let status = success_status(op.success_output()?.status);
        self.fresh_identity_at(bundle, op, status, body, 0)
    }

    /// [`Engine::fresh_identity`] for an answer of `status`, drawn anew for
    /// each `attempt`: a fixed seed and the same body otherwise draw the same
    /// identity, however taken it is.
    pub fn fresh_identity_at(
        &self,
        bundle: &Bundle,
        op: &Operation,
        status: u16,
        body: &Value,
        attempt: u32,
    ) -> Option<Value> {
        let schema = op.output_for_status(status)?.schema.as_ref()?;
        let validator = self.validator(bundle, op, status, schema).ok()?;
        let field = op.identity.as_ref().map_or("id", |i| i.field.as_str());
        let current = body.get(field)?;
        let id_schema = consistency::property_schema(bundle, schema, field)?;
        // Seeded by the body whose identity it replaces: the same
        // collision gets the same fresh identity.
        let retry = Value::from(attempt);
        let keys = [body, &retry];
        let keys = if attempt == 0 { &keys[..1] } else { &keys[..] };
        let mut draws = seed::Draws::new(
            self.faker_seed,
            &[&bundle.source_digest, &op.id, "identity"],
            keys,
        );
        (0..IDENTITY_DRAWS).find_map(|_| {
            let fresh = self.fake_field(bundle, field, &id_schema, &mut draws);
            if fresh.is_null() || &fresh == current {
                return None;
            }
            let mut candidate = body.clone();
            candidate.as_object_mut()?.insert(field.to_string(), fresh);
            validator.is_valid(&candidate).then_some(candidate)
        })
    }

    fn fake_response(
        &self,
        bundle: &Bundle,
        variant: &OutputVariant,
        status: u16,
        schema: &Value,
        validator: Option<&SchemaValidator>,
        answer: &mut AnswerDraws<'_>,
    ) -> GeneratedResponse {
        let (body, valid) =
            self.fake_checked(bundle, schema, validator, &answer.hints, answer.draws);
        GeneratedResponse {
            status,
            content_type: variant.content_type.clone(),
            body: Some(body),
            source: Source::Fallback,
            usage: Usage::default(),
            fallback_reason: (!valid).then(|| FAKER_INVALID_REASON.to_string()),
            effects: Vec::new(),
            effect_outcomes: Vec::new(),
            dropped_effects: 0,
            new_record: false,
        }
    }

    /// The answer of a response in a non-JSON media type (see [`media`]):
    /// the declared example, a faker value or no body, never the LLM's.
    fn media_response(
        &self,
        bundle: &Bundle,
        op: &Operation,
        variant: &OutputVariant,
        status: u16,
        call: &OperationCall,
        answer: media::MediaAnswer<'_>,
    ) -> GeneratedResponse {
        let (body, faker_note) = match answer {
            media::MediaAnswer::Example(example) => (Some(example.clone()), None),
            media::MediaAnswer::Empty => (None, None),
            media::MediaAnswer::Fake(schema) => {
                let validator = self.validator(bundle, op, status, schema).ok();
                let mut draws = self.call_draws(bundle, &op.id, status, call);
                let fake = self.fake_response(
                    bundle,
                    variant,
                    status,
                    schema,
                    validator.as_deref(),
                    &mut AnswerDraws {
                        hints: FakerHints::from_call(op, call),
                        draws: &mut draws,
                    },
                );
                (fake.body, fake.fallback_reason)
            }
        };
        GeneratedResponse {
            status,
            content_type: variant.content_type.clone(),
            body,
            source: Source::Fallback,
            usage: Usage::default(),
            fallback_reason: Some(with_faker_note(
                NON_JSON_MEDIA_REASON.to_string(),
                faker_note,
            )),
            effects: Vec::new(),
            effect_outcomes: Vec::new(),
            dropped_effects: 0,
            new_record: false,
        }
    }

    /// A faker value for object member `field` of schema `schema`, so the
    /// faker's name-based rules (id ranges) apply.
    fn fake_field(
        &self,
        bundle: &Bundle,
        field: &str,
        schema: &Value,
        draws: &mut seed::Draws,
    ) -> Value {
        let wrapper = serde_json::json!({
            "type": "object",
            "required": [field],
            "properties": {field: schema},
        });
        self.fake_value(bundle, &wrapper, None, &FakerHints::default(), draws)
            .get(field)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// A faker value for `schema` from `draws`, validated by `validator`
    /// when given: other `oneOf`/`anyOf` branches and fresh draws are tried
    /// before an invalid value is accepted.
    pub(crate) fn fake_value(
        &self,
        bundle: &Bundle,
        schema: &Value,
        validator: Option<&SchemaValidator>,
        hints: &FakerHints,
        draws: &mut seed::Draws,
    ) -> Value {
        self.fake_checked(bundle, schema, validator, hints, draws).0
    }

    /// [`Engine::fake_value`], and whether the value validated (always
    /// `true` without a validator). An invalid value is logged and counted
    /// ([`FAKER_INVALID_METRIC`]).
    fn fake_checked(
        &self,
        bundle: &Bundle,
        schema: &Value,
        validator: Option<&SchemaValidator>,
        hints: &FakerHints,
        draws: &mut seed::Draws,
    ) -> (Value, bool) {
        // One schema and cost table for every attempt (and every call):
        // building them walks the schema.
        let setup = self.fake_setup(bundle, schema);
        let mut faker = Faker::new(self.config.faker_depth).with_hints(hints.clone());
        if draws.seeded {
            // A seeded answer does not move with the clock.
            faker = faker.with_now(seed::SEEDED_NOW);
        }
        let mut first: Option<Value> = None;
        for attempt in 0..FALLBACK_ATTEMPTS {
            let candidate =
                faker.fake_with_costs(&setup.schema, Some(attempt), &setup.costs, &mut draws.rng);
            match validator {
                Some(v) if !v.is_valid(&candidate) => {
                    first.get_or_insert(candidate);
                }
                _ => return (candidate, true),
            }
        }
        tracing::warn!("{FAKER_INVALID_REASON}");
        metrics::counter!(FAKER_INVALID_METRIC).increment(1);
        (first.unwrap_or(Value::Null), false)
    }

    /// `schema` (a response of `op` with `status`) shaped to the output
    /// budget for `call`, or `None` when its answer fits as it is.
    fn shaped(
        &self,
        bundle: &Bundle,
        op: &Operation,
        status: u16,
        schema: &Value,
        selected: bool,
        call: &OperationCall,
    ) -> Option<Arc<budget::Shaped>> {
        if self.config.output_budget == 0 {
            return None;
        }
        use std::hash::{DefaultHasher, Hash, Hasher};
        let id = format!("{}/{}", bundle.source_digest, op.id);
        let names_key = (id.clone(), status);
        let selection = selected.then(|| {
            let mut hasher = DefaultHasher::new();
            schema.to_string().hash(&mut hasher);
            hasher.finish()
        });
        let doc = std::cell::OnceCell::new();
        let doc = || doc.get_or_init(|| bundle.with_reachable_defs(schema));
        let names = match (selection, self.member_names.get(&names_key)) {
            (None, Some(names)) => Arc::clone(&names),
            _ => {
                let mut found = HashSet::new();
                collect_member_names(doc(), &mut found);
                let mut names = MemberNames::new();
                for name in found {
                    names.entry(budget::stem(&name)).or_default().push(name);
                }
                let names = Arc::new(names);
                if selection.is_none() {
                    // Keyed by digest: re-uploads would otherwise pile up.
                    if self.member_names.len() >= MAX_SHAPES {
                        self.member_names.clear();
                    }
                    self.member_names.insert(names_key, Arc::clone(&names));
                }
                names
            }
        };
        let requested: std::collections::BTreeSet<&String> = budget::requested_words(call)
            .iter()
            .filter_map(|w| names.get(&budget::stem(w)))
            .flatten()
            .collect();
        let requested: Vec<String> = requested.into_iter().cloned().collect();
        let limit = budget::requested_limit(op.input.query.as_ref(), doc, call);
        let key = (id, status, selection, requested, limit);
        if let Some(hit) = self.shapes.get(&key) {
            return hit.clone();
        }
        let words: HashSet<String> = key.3.iter().cloned().collect();
        let shaped = tracing::info_span!("mb.shape")
            .in_scope(|| budget::shape(doc(), &words, selected, self.config.output_budget, limit))
            .map(Arc::new);
        // Every key counts against the bound: narrowed schemas vary per
        // query, requested words per call, and digests per upload.
        if self.shapes.len() >= MAX_SHAPES {
            self.shapes.clear();
        }
        self.shapes.insert(key, shaped.clone());
        shaped
    }

    pub(crate) fn validator(
        &self,
        bundle: &Bundle,
        op: &Operation,
        status: u16,
        schema: &Value,
    ) -> Result<Arc<SchemaValidator>, EngineError> {
        let key = (format!("{}/{}", bundle.source_digest, op.id), status);
        if let Some(v) = self.validators.get(&key) {
            return Ok(Arc::clone(&v));
        }
        let v = Arc::new(SchemaValidator::for_output(
            &bundle.with_reachable_defs(schema),
        )?);
        self.validators.insert(key, Arc::clone(&v));
        Ok(v)
    }

    /// The validator of `schema` (with the bundle's definitions it
    /// reaches), cached by the schema's text under the bundle's source
    /// digest. A bundle without a digest (built by hand, not imported) is
    /// never cached: its definitions are not known to be fixed. Compile
    /// errors are not cached.
    pub(crate) fn schema_validator(
        &self,
        bundle: &Bundle,
        schema: &Value,
    ) -> Result<Arc<SchemaValidator>, EngineError> {
        let compile =
            || SchemaValidator::for_output(&bundle.with_reachable_defs(schema)).map(Arc::new);
        if bundle.source_digest.is_empty() {
            return compile();
        }
        let key = (bundle.source_digest.clone(), schema.to_string());
        if let Some(v) = self.schema_validators.get(&key) {
            return Ok(Arc::clone(&v));
        }
        let v = compile()?;
        // Selections are client-controlled: bound the cache.
        if self.schema_validators.len() >= SCHEMA_VALIDATORS_MAX {
            self.schema_validators.clear();
        }
        self.schema_validators.insert(key, Arc::clone(&v));
        Ok(v)
    }

    /// The faker's setup of `schema`, cached like
    /// [`Engine::schema_validator`] (never for a bundle without a digest).
    fn fake_setup(&self, bundle: &Bundle, schema: &Value) -> Arc<FakeSetup> {
        if bundle.source_digest.is_empty() {
            return Arc::new(FakeSetup::new(bundle, schema));
        }
        let key = (bundle.source_digest.clone(), schema.to_string());
        if let Some(hit) = self.fake_setups.get(&key) {
            return Arc::clone(&hit);
        }
        let setup = Arc::new(FakeSetup::new(bundle, schema));
        if self.fake_setups.len() >= FAKE_SETUPS_MAX {
            self.fake_setups.clear();
        }
        self.fake_setups.insert(key, Arc::clone(&setup));
        setup
    }

    /// Ask the LLM, repairing once, under the guards: the budget and a
    /// generation permit (held for both calls) are checked first, waiting
    /// at most the call timeout. `Err` carries the reason to fall back.
    /// Every completed call's tokens are added to `usage` and charged to the
    /// budget, including calls whose output is ultimately discarded.
    ///
    /// An answer the provider cut at `max_tokens` is salvaged when it does
    /// not check as it is (see [`Engine::check_truncated`]): closed and
    /// completed locally, it is `Patched`. Otherwise the repair request
    /// says it was cut, and a fallback reason then starts with the
    /// truncation ("LLM output truncated: …").
    ///
    /// Each provider call runs in an `mb.llm` span, the first output's check
    /// in `mb.validate`, and the repair (only when a repair call is made)
    /// in `mb.repair`.
    async fn generate_with_llm(
        &self,
        provider: &dyn Provider,
        validator: &SchemaValidator,
        mut messages: Vec<ChatMessage>,
        format: &ResponseFormat,
        checks: &Checks<'_>,
        usage: &mut Usage,
    ) -> Result<LlmAnswer, String> {
        // A fallback chain gets every hop's timeout, not just the first's.
        let limit = provider.total_timeout(self.config.timeout);
        let deadline = Instant::now() + limit;
        let _permit = self.admit(deadline).await?;
        let first = self
            .complete(provider, &messages, format, usage, deadline)
            .await?;
        let (checked, cut, effects) = tracing::info_span!("mb.validate")
            .in_scope(|| self.check_reply(validator, &first, checks));
        let truncated = first.truncated.clone().filter(|_| cut);
        let errors = match checked {
            Checked::Valid(value) => {
                return Ok(LlmAnswer {
                    value,
                    source: Source::Llm,
                    effects,
                });
            }
            Checked::Patched(value) => {
                return Ok(LlmAnswer {
                    value,
                    source: Source::Patched,
                    effects,
                });
            }
            Checked::Rejected(reason) => return Err(with_cause(truncated.as_deref(), reason)),
            Checked::Invalid(errors) => errors,
        };
        if self.budget_exhausted() {
            return Err(with_cause(
                truncated.as_deref(),
                "token budget exhausted".into(),
            ));
        }
        tracing::info!(errors = %errors.join("; "), cut, "LLM output invalid; asking for a repair");
        async {
            let request = guards::repair_request(validator, &first.text, &errors, checks, cut);
            if !first.text.trim().is_empty() {
                messages.push(ChatMessage {
                    role: Role::Assistant,
                    content: first.text,
                });
            }
            messages.push(ChatMessage {
                role: Role::User,
                content: request,
            });
            let second = self
                .complete(provider, &messages, format, usage, Instant::now() + limit)
                .await
                .map_err(|reason| with_cause(truncated.as_deref(), reason))?;
            let (checked, second_cut, effects) = tracing::info_span!("mb.validate")
                .in_scope(|| self.check_reply(validator, &second, checks));
            let cause = if second_cut {
                second.truncated
            } else {
                truncated
            };
            match checked {
                Checked::Valid(value) | Checked::Patched(value) => Ok(LlmAnswer {
                    value,
                    source: Source::Repaired,
                    effects,
                }),
                Checked::Rejected(reason) => Err(with_cause(cause.as_deref(), reason)),
                Checked::Invalid(errors) => Err(with_cause(
                    cause.as_deref(),
                    format!("repaired output still invalid: {}", errors.join("; ")),
                )),
            }
        }
        .instrument(tracing::info_span!("mb.repair"))
        .await
    }

    /// [`Engine::check_output`] of `reply`; when the provider cut it at
    /// `max_tokens` and that fails, [`Engine::check_truncated`] (a complete
    /// value before cut prose is accepted as it is). An answer the provider
    /// did not cut is never salvaged: a missing `}` there is an error to
    /// repair, not a cut. The flag says whether the reply was cut; the
    /// effects are those of the checked answer, and a cut reply applies none
    /// (a `set` may stop mid-value): they count as dropped.
    fn check_reply(
        &self,
        validator: &SchemaValidator,
        reply: &Reply,
        checks: &Checks<'_>,
    ) -> (Checked, bool, guards::AnswerEffects) {
        let (checked, effects) = self.check_output(validator, &reply.text, checks);
        if reply.truncated.is_none() {
            return (checked, false, effects);
        }
        let (checked, effects) = match checked {
            Checked::Invalid(errors) => {
                match self.check_truncated(validator, &reply.text, checks) {
                    (salvaged @ Checked::Patched(_), effects) => (salvaged, effects),
                    _ => (Checked::Invalid(errors), guards::AnswerEffects::default()),
                }
            }
            other => (other, effects),
        };
        (checked, true, effects.all_dropped())
    }

    async fn complete(
        &self,
        provider: &dyn Provider,
        messages: &[ChatMessage],
        format: &ResponseFormat,
        usage: &mut Usage,
        deadline: Instant,
    ) -> Result<Reply, String> {
        let request = ChatRequest {
            messages: messages.to_vec(),
            response_format: format.clone(),
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            timeout: self.config.timeout,
        };
        let span = llm_span();
        let call = provider.complete(request).instrument(span.clone());
        let mut charge = |spent: Usage| {
            record_usage(&span, spent);
            *usage = *usage + spent;
            self.spend(spent);
        };
        match tokio::time::timeout_at(deadline, call).await {
            Ok(Ok(response)) => {
                charge(response.usage);
                Ok(Reply {
                    text: response.content,
                    truncated: None,
                })
            }
            Ok(Err(LlmError::Truncated {
                message,
                partial,
                usage: spent,
            })) => {
                charge(spent);
                Ok(Reply {
                    text: partial,
                    truncated: Some(format!("LLM output truncated: {message}")),
                })
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("LLM request timed out".into()),
        }
    }
}

/// An LLM answer that passed the checks.
struct LlmAnswer {
    value: Value,
    source: Source,
    effects: guards::AnswerEffects,
}

/// An LLM reply: its text, and, when the provider stopped it at
/// `max_tokens`, the error saying so (the text is then the part received).
struct Reply {
    text: String,
    truncated: Option<String>,
}

/// `reason` to fall back, after the truncation `cause` when there is one.
fn with_cause(cause: Option<&str>, reason: String) -> String {
    match cause {
        Some(cause) => format!("{cause}; not salvaged: {reason}"),
        None => reason,
    }
}

/// The span of one provider call. The provider records `llm.model` on it
/// (the current span while it runs); the caller records the tokens with
/// [`record_usage`].
pub(crate) fn llm_span() -> tracing::Span {
    tracing::info_span!(
        "mb.llm",
        llm.model = Empty,
        llm.tokens.prompt = Empty,
        llm.tokens.completion = Empty,
        llm.tokens.reasoning = Empty,
    )
}

/// Records a provider call's tokens on its [`llm_span`].
pub(crate) fn record_usage(span: &tracing::Span, usage: Usage) {
    span.record("llm.tokens.prompt", usage.prompt_tokens);
    span.record("llm.tokens.completion", usage.completion_tokens);
    if usage.reasoning_tokens > 0 {
        span.record("llm.tokens.reasoning", usage.reasoning_tokens);
    }
}

/// Shapes (and member-name sets) cached in all; a full cache is emptied.
const MAX_SHAPES: usize = 4096;

/// Every object member name in `value` (a schema).
fn collect_member_names(value: &Value, out: &mut HashSet<String>) {
    match value {
        Value::Object(obj) => {
            if let Some(Value::Object(props)) = obj.get("properties") {
                out.extend(props.keys().cloned());
            }
            obj.values().for_each(|v| collect_member_names(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| collect_member_names(v, out)),
        _ => {}
    }
}

/// The status a success variant is served with.
/// Faker draws tried for an unused identity value.
const IDENTITY_DRAWS: usize = 8;

fn success_status(status: StatusSelector) -> u16 {
    match status {
        StatusSelector::Code(c) => c,
        StatusSelector::Default => 200,
    }
}

/// A `response_format` name matching `^[a-zA-Z0-9_-]{1,64}$`.
pub(crate) fn format_name(id: &str) -> String {
    let name: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if name.is_empty() {
        "response".into()
    } else {
        name
    }
}

/// For a 2xx object body, apply the scenario's `set` merge patch, re-apply
/// the request body (create/update) and the identity so they still win, and
/// validate. Stored state is not re-applied: `set` beats the target. The body
/// of the scenario's own forced status (an error variant) is patched too, but
/// never reflects the request. An invalid result is discarded and the
/// pre-patch body kept.
fn apply_scenario(
    validator: &SchemaValidator,
    op: &Operation,
    call: &OperationCall,
    ctx: &GenerationContext<'_>,
    response: GeneratedResponse,
) -> GeneratedResponse {
    let Some(scenario) = ctx.scenario else {
        return response;
    };
    let Some(set) = &scenario.set else {
        return response;
    };
    let success = (200..300).contains(&response.status);
    if !success && scenario.status != Some(response.status) {
        return response;
    }
    let body = match &response.body {
        Some(body @ Value::Object(_)) => body,
        Some(_) => {
            tracing::warn!(
                operation = %op.id,
                scenario = %scenario.name,
                "scenario set applies to object bodies only; ignoring it"
            );
            metrics::counter!(SCENARIO_SET_IGNORED_METRIC, "reason" => "not_an_object")
                .increment(1);
            return response;
        }
        None => return response,
    };
    let patched = GeneratedResponse {
        body: Some(apply_set(body, set)),
        ..response.clone()
    };
    let patched = if success {
        reassert_request(validator, op, call, ctx, patched)
    } else {
        patched
    };
    // The errors are formatted only for the warning.
    match patched.body.as_ref().map(|b| {
        if validator.is_valid(b) {
            Ok(())
        } else {
            validator.validate(b)
        }
    }) {
        Some(Ok(())) => patched,
        Some(Err(errors)) => {
            tracing::warn!(
                operation = %op.id,
                scenario = %scenario.name,
                errors = %errors.join("; "),
                "scenario set produces an invalid body; ignoring it"
            );
            metrics::counter!(SCENARIO_SET_IGNORED_METRIC, "reason" => "invalid").increment(1);
            response
        }
        None => response,
    }
}

/// A 2xx body whose base64 fields do not decode, with those fields
/// encoded from their text (see [`encoded`]), kept only if it still
/// validates.
fn encode_base64(
    validator: &SchemaValidator,
    bundle: &Bundle,
    schema: &Value,
    mut response: GeneratedResponse,
) -> GeneratedResponse {
    if !(200..300).contains(&response.status) {
        return response;
    }
    let Some(body) = &response.body else {
        return response;
    };
    if let Some(patched) = encoded::patch(bundle, schema, body)
        && validator.is_valid(&patched)
    {
        tracing::info!("encoded base64 fields from their text");
        response.body = Some(patched);
    }
    response
}

/// Make a 2xx body consistent with the request and state (spec §1.2,
/// criterion 4): queries reflect the stored target, creates reflect the
/// request body, updates reflect the target then the request body, and the
/// identity field always equals the path id. An LLM's answer to an update that has
/// the resource's read schema wins where the request named a member
/// ([`settled_update`]), and the request merges into that ([`apply_request`]). An
/// action's LLM answer is the state after it, so it wins over the target instead (see
/// [`Operation::is_action`]), unless it is a new record ([`is_new_record`]),
/// which takes nothing from the target and keeps its own identity. Every merge
/// is validated; an invalid one keeps the best valid value.
fn reflect(
    validator: &SchemaValidator,
    op: &Operation,
    call: &OperationCall,
    ctx: &GenerationContext<'_>,
    mut response: GeneratedResponse,
) -> GeneratedResponse {
    if !(200..300).contains(&response.status) {
        return response;
    }
    let Some(body) = response.body.take() else {
        return response;
    };
    // An answer told of later writes is ahead of the stored copy, so that copy
    // only fills what it left out. A faker's knows of none, and takes the copy.
    let from_llm = !matches!(response.source, Source::Fallback);
    let ahead = from_llm && !ctx.earlier_requests.is_empty();
    let body = match op.kind {
        OperationKind::Query => match ctx.target {
            Some(target) if ahead => caught_up(validator, body, target, ctx.earlier_own_body),
            Some(target) => overlay_target(validator, target, body),
            None => body,
        },
        OperationKind::Command(Effect::Create) => {
            overlay_request(validator, body, call.body.as_ref())
        }
        OperationKind::Command(Effect::Update) => {
            let overlaid = overlay_request(validator, body.clone(), call.body.as_ref());
            if is_new_record(op, call, ctx.target, &response, &overlaid) {
                response.new_record = true;
                overlaid
            } else {
                match (ctx.target, settling_guard(op, ctx, &response)) {
                    (Some(target), Some(guard)) => {
                        let body =
                            settled_update(validator, guard, body, target, call.body.as_ref());
                        apply_request(validator, op, body, call.body.as_ref())
                    }
                    (target, _) => {
                        let body = match target {
                            Some(target) if ahead => {
                                caught_up(validator, body, target, ctx.earlier_own_body)
                            }
                            Some(target) if op.is_action() && from_llm => {
                                fill_omitted(validator, body, target)
                            }
                            target => overlay(validator, body, target.map(|t| &t.data)),
                        };
                        overlay_request(validator, body, call.body.as_ref())
                    }
                }
            }
        }
        OperationKind::Command(_) => body,
        // Streams have no stored target to reflect.
        OperationKind::StreamServer | OperationKind::StreamClient | OperationKind::StreamBidi => {
            body
        }
    };
    response.body = Some(if response.new_record {
        body
    } else {
        force_identity(validator, op, call, body)
    });
    response
}

/// What lets this update's answer settle on its target ([`settled_update`]): an LLM's
/// to a plain update, not told of earlier requests, with the read's schema. Anything
/// else takes the stored copy, then the request's members whole.
fn settling_guard<'a>(
    op: &Operation,
    ctx: &GenerationContext<'a>,
    response: &GeneratedResponse,
) -> Option<&'a dyn UpdateGuard> {
    let plain = op.kind == OperationKind::Command(Effect::Update)
        && !op.is_action()
        && ctx.target.is_some()
        && ctx.earlier_requests.is_empty()
        && !matches!(response.source, Source::Fallback);
    ctx.update_guard
        .filter(|guard| plain && guard.settles(response.status))
}

/// Whether an action's LLM answer `body` is a new record: of its stored
/// target's schema (an update, see [`Operation::is_action`]), carrying a
/// non-empty identity that is not the target's. A reply `c2` under
/// `/comments/c1/replies` is stored as `c2` and `c1` is untouched. An answer
/// with no identity, or the target's, is about the target; so is any answer
/// with no stored target to tell it from, and a faker's invented id says
/// nothing.
fn is_new_record(
    op: &Operation,
    call: &OperationCall,
    target: Option<&StoredResource>,
    response: &GeneratedResponse,
    body: &Value,
) -> bool {
    if op.kind != OperationKind::Command(Effect::Update)
        || !op.is_action()
        || !matches!(
            response.source,
            Source::Llm | Source::Patched | Source::Repaired
        )
        || op
            .output_for_status(response.status)
            .and_then(|o| o.schema.as_ref())
            != op.success_output().and_then(|o| o.schema.as_ref())
    {
        return false;
    }
    let text = |v: &Value| match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let (Some(identity), Some(target)) = (op.identity.as_ref(), target) else {
        return false;
    };
    let member = |name: &str| body.get(name).and_then(text);
    let Some(id) = member(&identity.field) else {
        return false;
    };
    // What the path names: the identity, or the member `path_field` says.
    let Some(named) = member(identity.path_field.as_ref().unwrap_or(&identity.field)) else {
        return false;
    };
    let in_path = identity
        .path_param
        .as_ref()
        .and_then(|p| call.path_params.get(p))
        .and_then(text);
    in_path.as_ref() != Some(&named)
        && target.id != id
        && target.data.get(&identity.field).and_then(text).as_ref() != Some(&id)
}

/// After a scenario `set`: re-apply only the request-body overlay (creates
/// and updates) and the path identity (not on a new record), never the stored
/// target.
fn reassert_request(
    validator: &SchemaValidator,
    op: &Operation,
    call: &OperationCall,
    ctx: &GenerationContext<'_>,
    mut response: GeneratedResponse,
) -> GeneratedResponse {
    let settling = settling_guard(op, ctx, &response).is_some();
    let Some(body) = response.body.take() else {
        return response;
    };
    let body = match op.kind {
        OperationKind::Command(Effect::Update) if settling => {
            apply_request(validator, op, body, call.body.as_ref())
        }
        OperationKind::Command(Effect::Create | Effect::Update) => {
            overlay_request(validator, body, call.body.as_ref())
        }
        _ => body,
    };
    response.body = Some(if response.new_record {
        body
    } else {
        force_identity(validator, op, call, body)
    });
    response
}

/// Overlay the request body's members that the response schema declares
/// (all of them when it declares none): an undeclared member, such as a
/// credential or a field in another casing, is never echoed.
fn overlay_request(validator: &SchemaValidator, base: Value, body: Option<&Value>) -> Value {
    let (Some(Value::Object(fields)), Some(declared)) = (body, validator.declared_property_names())
    else {
        return overlay(validator, base, body);
    };
    let kept: Map<String, Value> = fields
        .iter()
        .filter(|(k, _)| declared.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    overlay(validator, base, Some(&Value::Object(kept)))
}

/// A request body on an answer settled on its target ([`settled_update`]). A `PATCH`'s
/// goes member by member at any depth ([`merge_request`]), so naming part of an object
/// keeps the rest. A `PUT` replaces each member it names.
fn apply_request(
    validator: &SchemaValidator,
    op: &Operation,
    base: Value,
    body: Option<&Value>,
) -> Value {
    let replaces = matches!(
        op.binding,
        Binding::Http {
            method: HttpMethod::Put,
            ..
        }
    );
    if replaces {
        overlay_request(validator, base, body)
    } else {
        merge_request(validator, base, body)
    }
}

/// [`overlay_request`], with each named member merged into the one `base` holds. One
/// that does not fit merged goes whole, as it may start another branch of a union; one
/// that fits neither way leaves `base` as it is.
fn merge_request(validator: &SchemaValidator, base: Value, body: Option<&Value>) -> Value {
    let (Some(Value::Object(fields)), Value::Object(current)) = (body, &base) else {
        return overlay_request(validator, base, body);
    };
    let declared = validator.declared_property_names();
    let root = validator.schema();
    let named = fields
        .iter()
        .filter(|(k, _)| declared.is_none_or(|names| names.contains(k.as_str())));
    let merged: Map<String, Value> = named
        .clone()
        .map(|(k, v)| {
            let inner = schema_level(root, root, Some(k), 0).1;
            let value = match current.get(k) {
                Some(held) => merged_member(root, inner, held, v),
                None => v.clone(),
            };
            (k.clone(), value)
        })
        .collect();
    let applied = overlay(validator, base, Some(&Value::Object(merged.clone())));
    let whole: Map<String, Value> = named
        .filter(|(k, _)| applied.get(k.as_str()) != merged.get(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    overlay(validator, applied, Some(&Value::Object(whole)))
}

/// `over` on `held`, objects member by member. An object under a `oneOf` or `anyOf` is
/// replaced, as its members could mix with another branch's.
fn merged_member(root: &Value, schema: Option<&Value>, held: &Value, over: &Value) -> Value {
    let (Value::Object(held_members), Value::Object(over_members)) = (held, over) else {
        return over.clone();
    };
    if schema.is_some_and(|node| schema_level(root, node, None, 0).0) {
        return over.clone();
    }
    let mut out = held_members.clone();
    for (k, v) in over_members {
        let inner = schema.and_then(|node| schema_level(root, node, Some(k), 0).1);
        let merged = match out.get(k) {
            Some(below) => merged_member(root, inner, below, v),
            None => v.clone(),
        };
        out.insert(k.clone(), merged);
    }
    Value::Object(out)
}

/// An LLM's answer to an update, over the stored copy it replaces: the copy stands. A
/// member of the answer wins only where the request body names the same path, so a
/// value the model drifted on, a faker filled or a budget capped goes, with no list of
/// names or per-API rule. What the copy lacks comes from the answer at the top level.
///
/// A request and its answer may wrap differently. A request's only key, if the response
/// does not declare it, is a wrapper: `{ticket: {status}}` names `status`. And a request
/// naming no member of the response names those of the one member that holds them all,
/// when only one does: `{status}` names `ticket.status`.
///
/// Identity, what the path reads by and `readOnly` members never take the answer's
/// value, nor does an owner: an answer that moves the resource leaves the copy as it
/// was, and what it adds goes, unless the schema needs it. Each top-level member that fits
/// (see [`overlay`]), and one that does not keeps what a faker's answer would.
fn settled_update(
    validator: &SchemaValidator,
    guard: &dyn UpdateGuard,
    answer: Value,
    target: &StoredResource,
    request: Option<&Value>,
) -> Value {
    let root = validator.schema();
    let request = unwrapped_request(validator, request);
    let settled = match response_wrapper(validator, request) {
        Some(wrapper) => {
            let inner = schema_level(root, root, Some(wrapper), 0).1;
            let Some(below) = answer.get(wrapper) else {
                return overlay(validator, answer, Some(&target.data));
            };
            // A wrapped answer is stored as what it wraps.
            let Some(held) = target.data.get(wrapper) else {
                let member = settled_members(root, inner, below, &target.data, request, false);
                let member = guard.keep_identity(member);
                let member = if guard.keeps_owner(&member) {
                    member
                } else {
                    target.data.clone()
                };
                let settled = Value::Object(Map::from_iter([(wrapper.to_string(), member)]));
                return overlay(validator, answer, Some(&settled));
            };
            let member = settled_members(root, inner, below, held, request, false);
            let mut settled = target.data.clone();
            if let Value::Object(members) = &mut settled {
                members.insert(wrapper.to_string(), member);
            }
            settled
        }
        None => settled_members(root, Some(root), &answer, &target.data, request, true),
    };
    let settled = guard.keep_identity(settled);
    if !guard.keeps_owner(&settled) {
        return overlay(
            validator,
            without_other_owners(validator, answer, target),
            Some(&target.data),
        );
    }
    let stored_first = overlay(validator, answer, Some(&target.data));
    overlay(validator, stored_first, Some(&settled))
}

/// `answer` without the members the stored copy lacks, as an answer moving the resource
/// says nothing to go by, unless the schema needs them.
fn without_other_owners(
    validator: &SchemaValidator,
    answer: Value,
    target: &StoredResource,
) -> Value {
    let (Value::Object(members), Value::Object(stored)) = (&answer, &target.data) else {
        return answer;
    };
    let kept: Map<String, Value> = members
        .iter()
        .filter(|(k, _)| stored.contains_key(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let kept = Value::Object(kept);
    if validator.is_valid(&kept) {
        kept
    } else {
        answer
    }
}

/// What a request's only key holds, if that is an object and the response does not
/// declare the key.
fn unwrapped_request<'a>(
    validator: &SchemaValidator,
    request: Option<&'a Value>,
) -> Option<&'a Value> {
    let (Some(Value::Object(members)), Some(declared)) =
        (request, validator.declared_property_names())
    else {
        return request;
    };
    match members.iter().next() {
        Some((key, inner))
            if members.len() == 1 && inner.is_object() && !declared.contains(key.as_str()) =>
        {
            Some(inner)
        }
        _ => request,
    }
}

/// The one member of the response holding all a request names, when the request names
/// none of the response's own.
fn response_wrapper<'a>(
    validator: &'a SchemaValidator,
    request: Option<&Value>,
) -> Option<&'a str> {
    let Value::Object(members) = request? else {
        return None;
    };
    let declared = validator.declared_property_names()?;
    if members.is_empty() || members.keys().any(|k| declared.contains(k.as_str())) {
        return None;
    }
    let root = validator.schema();
    let mut holding = declared.iter().filter(|name| {
        schema_level(root, root, Some(name.as_str()), 0)
            .1
            .is_some_and(|member| {
                members
                    .keys()
                    .all(|k| schema_level(root, member, Some(k), 0).1.is_some())
            })
    });
    let wrapper = holding.next()?;
    holding.next().is_none().then_some(wrapper.as_str())
}

/// [`settled_update`] at one level, `request` being what the request says at this path.
/// Objects go member by member, unless a `oneOf` or `anyOf` says either is a branch,
/// which is taken whole or not at all. A named array takes the request's value, as a cap
/// or an empty answer could have cut the answer's. A null is only the request's to write.
fn settled_members(
    root: &Value,
    schema: Option<&Value>,
    answer: &Value,
    stored: &Value,
    request: Option<&Value>,
    top: bool,
) -> Value {
    let (Value::Object(answer_members), Value::Object(stored_members)) = (answer, stored) else {
        return stored.clone();
    };
    let union = schema.is_some_and(|node| schema_level(root, node, None, 0).0);
    let mut out = stored_members.clone();
    for (k, v) in answer_members {
        let named = request.and_then(|r| r.get(k));
        let answered = match named {
            Some(array @ Value::Array(_)) if v.is_array() => array,
            _ => v,
        };
        let Some(held) = stored_members.get(k) else {
            if top || named.is_some() {
                out.insert(k.clone(), answered.clone());
            }
            continue;
        };
        let inner = schema.and_then(|node| schema_level(root, node, Some(k), 0).1);
        if inner.is_some_and(|member| is_read_only(root, member, 0)) {
            continue;
        }
        let Some(named) = named else {
            continue;
        };
        if named.is_null() {
            out.insert(k.clone(), Value::Null);
        } else if v.is_null() || (held.is_array() && !named.is_array()) {
            continue;
        } else if held.is_object() && v.is_object() && !union && !is_union(root, inner) {
            let member = settled_members(root, inner, v, held, Some(named), false);
            out.insert(k.clone(), member);
        } else {
            out.insert(k.clone(), answered.clone());
        }
    }
    Value::Object(out)
}

/// Whether a member's schema has a `oneOf` or `anyOf`.
fn is_union(root: &Value, schema: Option<&Value>) -> bool {
    schema.is_some_and(|member| schema_level(root, member, None, 0).0)
}

/// Whether the contract says a client does not set this member, by its `$ref`s and
/// `allOf`s.
fn is_read_only(root: &Value, node: &Value, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    if node.get("readOnly").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if let Some(target) = node
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer))
    {
        return is_read_only(root, target, depth + 1);
    }
    node.get("allOf")
        .and_then(Value::as_array)
        .is_some_and(|parts| parts.iter().any(|p| is_read_only(root, p, depth + 1)))
}

/// Add to an answer the stored members it omits, each only if the answer stays
/// valid. The answer is the base because it is valid, which a stored resource
/// need not be: it may come from a create of another shape, or a fixture.
fn fill_omitted(validator: &SchemaValidator, answer: Value, target: &StoredResource) -> Value {
    let (Value::Object(answer_obj), Value::Object(stored)) = (&answer, &target.data) else {
        return answer;
    };
    let omitted: Map<String, Value> = stored
        .iter()
        .filter(|(k, _)| !answer_obj.contains_key(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    overlay(validator, answer, Some(&Value::Object(omitted)))
}

/// An answer told of earlier requests, over the stored copy it is ahead of: the
/// copy fills what the answer left out, at any depth, and what the latest write said
/// of the target itself (`own`) stands over what the answer wrote, as a request's own
/// body does (see [`overlay_request`]). What was redacted is not a value to keep.
fn caught_up(
    validator: &SchemaValidator,
    answer: Value,
    target: &StoredResource,
    own: Option<&Value>,
) -> Value {
    let root = validator.schema();
    let filled = deep_fill(root, Some(root), &answer, &target.data);
    let body = if validator.is_valid(&filled) {
        filled
    } else {
        fill_omitted(validator, answer, target)
    };
    let Some(Value::Object(fields)) = own.and_then(without_redacted) else {
        return body;
    };
    let Value::Object(current) = &body else {
        return body;
    };
    let over: Map<String, Value> = fields
        .into_iter()
        .map(|(k, v)| {
            let merged = match current.get(&k) {
                Some(held) => deep_over(held, &v),
                None => v,
            };
            (k, merged)
        })
        .collect();
    overlay(validator, body, Some(&Value::Object(over)))
}

/// What one level of a schema in `root` says, by its `$ref`s and `allOf`s: whether it
/// has a `oneOf` or `anyOf`, and member `key`'s schema if asked for.
fn schema_level<'a>(
    root: &'a Value,
    node: &'a Value,
    key: Option<&str>,
    depth: usize,
) -> (bool, Option<&'a Value>) {
    if depth > 8 {
        return (false, None);
    }
    if let Some(target) = node
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer))
    {
        return schema_level(root, target, key, depth + 1);
    }
    let mut union = node.get("oneOf").is_some() || node.get("anyOf").is_some();
    let mut member = key.and_then(|k| node.get("properties")?.get(k));
    for branch in node
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (below, found) = schema_level(root, branch, key, depth + 1);
        union |= below;
        member = member.or(found);
    }
    (union, member)
}

/// `answer` with every member of `stored` it omits, at any depth, but not within a
/// `oneOf` or `anyOf`: the branch the answer took stands, with no member of another.
fn deep_fill(root: &Value, schema: Option<&Value>, answer: &Value, stored: &Value) -> Value {
    let (Value::Object(answer_members), Value::Object(stored_members)) = (answer, stored) else {
        return answer.clone();
    };
    if schema.is_some_and(|node| schema_level(root, node, None, 0).0) {
        return answer.clone();
    }
    let mut out = answer_members.clone();
    for (k, v) in stored_members {
        let inner = schema.and_then(|node| schema_level(root, node, Some(k), 0).1);
        match out.get_mut(k) {
            Some(held) => *held = deep_fill(root, inner, held, v),
            None => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// `over` on `base`, objects member by member.
fn deep_over(base: &Value, over: &Value) -> Value {
    let (Value::Object(base_members), Value::Object(over_members)) = (base, over) else {
        return over.clone();
    };
    let mut out = base_members.clone();
    for (k, v) in over_members {
        let merged = match out.get(k) {
            Some(held) => deep_over(held, v),
            None => v.clone(),
        };
        out.insert(k.clone(), merged);
    }
    Value::Object(out)
}

/// `value` with what was redacted taken out at any depth: an object loses the
/// member, and an array, which is replaced whole, goes altogether. `None` when
/// nothing is left.
fn without_redacted(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) if text == REDACTED => None,
        Value::Object(members) => {
            let kept: Map<String, Value> = members
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), without_redacted(v)?)))
                .collect();
            (kept.len() == members.len() || !kept.is_empty()).then_some(Value::Object(kept))
        }
        Value::Array(items) => {
            let kept: Option<Vec<Value>> = items
                .iter()
                .map(|item| match item {
                    Value::Object(members) => {
                        let whole = without_redacted(item)?;
                        (whole.as_object().map(Map::len) == Some(members.len())).then_some(whole)
                    }
                    other => without_redacted(other),
                })
                .collect();
            kept.map(Value::Array)
        }
        other => Some(other.clone()),
    }
}

/// Overlay `fields`' top-level members onto an object `base`: all at once if
/// that validates, else each member that keeps the object valid.
fn overlay(validator: &SchemaValidator, base: Value, fields: Option<&Value>) -> Value {
    let (Value::Object(base_obj), Some(Value::Object(fields))) = (&base, fields) else {
        return base;
    };
    let mut merged = base_obj.clone();
    for (k, v) in fields {
        merged.insert(k.clone(), v.clone());
    }
    let merged = Value::Object(merged);
    if validator.is_valid(&merged) {
        return merged;
    }
    // Each member is tried in place and taken back out when it breaks the
    // object, instead of validating a fresh copy per member.
    let mut best = base;
    for (k, v) in fields {
        let Value::Object(obj) = &mut best else {
            return best;
        };
        let previous = obj.insert(k.clone(), v.clone());
        if validator.is_valid(&best) {
            continue;
        }
        if let Value::Object(obj) = &mut best {
            match previous {
                // Back in its place: the key keeps its position.
                Some(previous) => {
                    obj.insert(k.clone(), previous);
                }
                // The last key, so removing it moves no other.
                None => {
                    obj.remove(k);
                }
            }
        }
    }
    best
}

/// Set `body[identity.field]` to the path id (else the request body's
/// `identity.input_field`), as a string or (when the schema wants one) a
/// number. The answer of an action that does not return its resource is a
/// record of its own: only a member named for the path parameter names the
/// target, unless that parameter is a bare id (`{id}`, see
/// [`mb_mcm::is_generic_id`]), and any other `id` is the record's.
fn force_identity(
    validator: &SchemaValidator,
    op: &Operation,
    call: &OperationCall,
    body: Value,
) -> Value {
    let Some(identity) = &op.identity else {
        return body;
    };
    if op.kind == OperationKind::Command(Effect::Other)
        && op.is_action()
        && identity
            .path_param
            .as_deref()
            .is_none_or(|p| p != identity.field || mb_mcm::is_generic_id(p))
    {
        return body;
    }
    let from_path = identity
        .path_param
        .as_ref()
        .and_then(|p| call.path_params.get(p));
    let from_body = || {
        let field = identity.input_field.as_ref()?;
        call.body.as_ref()?.get(field)
    };
    let Some(id) = from_path.or_else(from_body) else {
        return body;
    };
    if !body.is_object() || body.get(&identity.field) == Some(id) {
        return body;
    }
    let mut candidates = vec![id.clone()];
    match id {
        Value::String(s) => {
            if let Ok(n) = s.parse::<i64>() {
                candidates.push(Value::from(n));
            } else if let Some(n) = s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
                candidates.push(Value::Number(n));
            }
        }
        Value::Number(n) => candidates.push(Value::String(n.to_string())),
        _ => {}
    }
    for candidate in candidates {
        let mut forced = body.clone();
        if let Value::Object(obj) = &mut forced {
            obj.insert(identity.field.clone(), candidate);
        }
        if validator.is_valid(&forced) {
            return forced;
        }
    }
    body
}

/// Overlay the stored resource's top-level fields onto a generated object.
/// If the merge is invalid, prefer the stored data when it validates, else
/// keep the generated body.
fn overlay_target(validator: &SchemaValidator, target: &StoredResource, generated: Value) -> Value {
    let (Value::Object(mut merged), Value::Object(stored)) = (generated.clone(), &target.data)
    else {
        return generated;
    };
    for (k, v) in stored {
        merged.insert(k.clone(), v.clone());
    }
    let merged = Value::Object(merged);
    if validator.is_valid(&merged) {
        merged
    } else if validator.is_valid(&target.data) {
        target.data.clone()
    } else {
        generated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn orders() -> Bundle {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/openapi/orders-3.0.yaml"
        );
        mb_import::compile(&std::fs::read(path).unwrap()).unwrap()
    }

    /// Every contract upload has a new digest: the shape caches stay
    /// bounded however many there were (MB-ENGINE-PERF-1).
    #[test]
    fn shape_caches_stay_bounded_across_digests() {
        let engine = Engine::new(None, EngineConfig::default());
        let mut bundle = orders();
        let op = bundle.operation("getOrder").unwrap().clone();
        let schema = op.success_output().unwrap().schema.clone().unwrap();
        let call = OperationCall::default();
        for i in 0..MAX_SHAPES + 100 {
            bundle.source_digest = format!("digest-{i}");
            engine.shaped(&bundle, &op, 200, &schema, false, &call);
        }
        assert!(engine.shapes.len() <= MAX_SHAPES, "{}", engine.shapes.len());
        assert!(
            engine.member_names.len() <= MAX_SHAPES,
            "{}",
            engine.member_names.len()
        );
        // Still a memo: the same key is answered from it.
        let before = engine.shapes.len();
        engine.shaped(&bundle, &op, 200, &schema, false, &call);
        assert_eq!(engine.shapes.len(), before);
    }

    /// The per-member fallback keeps each member that leaves the object
    /// valid, puts back a member it replaced when the new value breaks it,
    /// and leaves the other keys in place (MB-ENGINE-PERF-10).
    #[test]
    fn overlay_falls_back_to_each_member_that_stays_valid() {
        let v = SchemaValidator::new(&json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "integer"},
                "c": {"type": "string"}
            },
            "additionalProperties": {"type": "integer"}
        }))
        .unwrap();
        // The fallback as it was: a validated copy per member.
        let by_copies = |base: Value, fields: &Value| {
            let mut best = base;
            for (k, value) in fields.as_object().unwrap() {
                let mut candidate = best.clone();
                candidate
                    .as_object_mut()
                    .unwrap()
                    .insert(k.clone(), value.clone());
                if v.is_valid(&candidate) {
                    best = candidate;
                }
            }
            best
        };
        let base = json!({"b": 1, "a": "x"});
        let fields = json!({"a": "y", "b": "bad", "c": "z", "d": 2, "e": "bad"});
        let out = overlay(&v, base.clone(), Some(&fields));
        assert_eq!(out, json!({"a": "y", "b": 1, "c": "z", "d": 2}));
        let old = by_copies(base, &fields);
        let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&out), keys(&old));
        // All at once when that validates.
        let out = overlay(&v, json!({"a": "x"}), Some(&json!({"b": 3})));
        assert_eq!(out, json!({"a": "x", "b": 3}));
    }
}
