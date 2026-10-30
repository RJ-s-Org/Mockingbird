//! The eval run and its report.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mb_engine::{Engine, EngineConfig, GenerationContext, MessageContext, OperationCall, Source};
use mb_llm::Usage;
use mb_mcm::{Bundle, Direction, Operation};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use mockingbird::file_config::{FileProfile, LimitsConfig};

use crate::corpus::CorpusEntry;

/// The engine settings a served mock gets from a default LLM profile and
/// the default `limits:`: the per-request timeout, completion token limit,
/// temperature and output budget. An eval run uses them unless told
/// otherwise, so it measures what a user gets.
pub fn production_engine_settings() -> EngineSettings {
    let profile = FileProfile::new("", "");
    EngineSettings {
        llm_timeout: profile.timeout,
        max_tokens: profile.max_tokens,
        temperature: profile.temperature,
        output_budget: LimitsConfig::default().output_budget_tokens,
    }
}

/// The engine settings of an eval run (see [`production_engine_settings`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EngineSettings {
    /// The engine's per-request LLM timeout.
    pub llm_timeout: Duration,
    /// Completion token limit of each LLM call.
    pub max_tokens: u32,
    pub temperature: f32,
    /// Completion tokens an answer is shaped to (0: off).
    pub output_budget: u32,
}

/// The outcome of one eval run.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EvalReport {
    pub model: String,
    pub base_url: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub calls: u64,
    /// `(llm + repaired) / calls`.
    pub schema_valid_rate: f64,
    /// `llm / calls`.
    pub first_pass_rate: f64,
    /// `repaired / calls`.
    pub repair_rate: f64,
    /// `fallback / calls`.
    pub fallback_rate: f64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub prompt_tokens_per_call: f64,
    pub completion_tokens_per_call: f64,
    /// Keyed by [`CorpusEntry::name`] (or the manifest entry name).
    pub per_contract: BTreeMap<String, ContractStats>,
    /// Keyed by the manifest's `api_type`; empty for a corpus run.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_api_type: BTreeMap<String, ContractStats>,
    /// Keyed by the manifest's `domain`; empty for a corpus run.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_domain: BTreeMap<String, ContractStats>,
    /// Calls the engine rejected outright (sampled with source `error`),
    /// not counted in `calls`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped_calls: u64,
    /// Streaming operations of the evaluated contracts, which evals do not
    /// call.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped_streaming_operations: u64,
    /// Corpus contracts that could not be read or do not compile.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_contracts: Vec<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The calls of one contract, by source.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ContractStats {
    pub calls: u64,
    pub llm: u64,
    pub repaired: u64,
    pub fallback: u64,
    pub p95_ms: u64,
}

/// The limits an eval run must stay within.
#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    pub min_schema_valid_rate: f64,
    pub max_repair_rate: f64,
    pub max_fallback_rate: f64,
    pub max_p95_ms: u64,
    /// Skipped calls plus skipped contracts at most; `None` (the default)
    /// only reports them.
    pub max_skipped: Option<u64>,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_schema_valid_rate: 0.95,
            max_repair_rate: 0.25,
            max_fallback_rate: 0.05,
            max_p95_ms: 3000,
            max_skipped: None,
        }
    }
}

impl EvalReport {
    /// One line per threshold the report breaks; empty when it passes. A
    /// run that made no calls is a violation too.
    pub fn violations(&self, t: &Thresholds) -> Vec<String> {
        let mut out = Vec::new();
        if self.calls == 0 {
            out.push("no calls were made (empty corpus?)".to_owned());
        }
        if self.schema_valid_rate < t.min_schema_valid_rate {
            out.push(format!(
                "schema_valid_rate {:.3} < {:.3}",
                self.schema_valid_rate, t.min_schema_valid_rate
            ));
        }
        if self.repair_rate > t.max_repair_rate {
            out.push(format!(
                "repair_rate {:.3} > {:.3}",
                self.repair_rate, t.max_repair_rate
            ));
        }
        if self.fallback_rate > t.max_fallback_rate {
            out.push(format!(
                "fallback_rate {:.3} > {:.3}",
                self.fallback_rate, t.max_fallback_rate
            ));
        }
        if self.p95_ms > t.max_p95_ms {
            out.push(format!("p95_ms {} > {}", self.p95_ms, t.max_p95_ms));
        }
        let skipped = self
            .skipped_calls
            .saturating_add(u64::try_from(self.skipped_contracts.len()).unwrap_or(u64::MAX));
        if let Some(max) = t.max_skipped
            && skipped > max
        {
            out.push(format!(
                "skipped {skipped} > {max} ({} calls, {} contracts)",
                self.skipped_calls,
                self.skipped_contracts.len()
            ));
        }
        out
    }

    /// A Markdown summary: a metric table (with skip counts when there are
    /// any), then one row per contract, then (for a manifest run) one row
    /// per API type and per domain, then the skipped contracts.
    pub fn to_markdown(&self) -> String {
        let pct = |r: f64| format!("{:.1}%", r * 100.0);
        let mut md = String::new();
        let _ = writeln!(md, "# Mockingbird LLM evals\n");
        let _ = writeln!(
            md,
            "Model `{}` at `{}`, started {}.\n",
            self.model,
            self.base_url,
            self.started_at.to_rfc3339()
        );
        let _ = writeln!(md, "| Metric | Value |");
        let _ = writeln!(md, "| --- | --- |");
        let rows = [
            ("Calls", self.calls.to_string()),
            ("Schema-valid rate", pct(self.schema_valid_rate)),
            ("First-pass rate", pct(self.first_pass_rate)),
            ("Repair rate", pct(self.repair_rate)),
            ("Fallback rate", pct(self.fallback_rate)),
            ("p50 latency", format!("{} ms", self.p50_ms)),
            ("p95 latency", format!("{} ms", self.p95_ms)),
            (
                "Prompt tokens per call",
                format!("{:.1}", self.prompt_tokens_per_call),
            ),
            (
                "Completion tokens per call",
                format!("{:.1}", self.completion_tokens_per_call),
            ),
        ];
        for (metric, value) in rows {
            let _ = writeln!(md, "| {metric} | {value} |");
        }
        let skips = [
            ("Skipped calls", self.skipped_calls),
            (
                "Streaming operations not evaluated",
                self.skipped_streaming_operations,
            ),
        ];
        for (metric, n) in skips.into_iter().filter(|(_, n)| *n > 0) {
            let _ = writeln!(md, "| {metric} | {n} |");
        }
        stats_table(&mut md, "Per contract", "Contract", &self.per_contract);
        if !self.per_api_type.is_empty() {
            stats_table(&mut md, "Per API type", "API type", &self.per_api_type);
        }
        if !self.per_domain.is_empty() {
            stats_table(&mut md, "Per domain", "Domain", &self.per_domain);
        }
        if !self.skipped_contracts.is_empty() {
            let _ = writeln!(md, "\n## Skipped contracts\n");
            for name in &self.skipped_contracts {
                let _ = writeln!(md, "- {name}");
            }
        }
        md
    }
}

/// A `## {title}` section with one row per key of `rows`.
fn stats_table(md: &mut String, title: &str, key: &str, rows: &BTreeMap<String, ContractStats>) {
    let _ = writeln!(md, "\n## {title}\n");
    let _ = writeln!(md, "| {key} | Calls | LLM | Repaired | Fallback | p95 ms |");
    let _ = writeln!(md, "| --- | --- | --- | --- | --- | --- |");
    for (name, s) in rows {
        let _ = writeln!(
            md,
            "| {name} | {} | {} | {} | {} | {} |",
            s.calls, s.llm, s.repaired, s.fallback, s.p95_ms
        );
    }
}

/// The nearest-rank percentile `p` (0..=1) of sorted `values`; 0 when
/// empty.
pub(crate) fn percentile<T: Copy + Default>(sorted: &[T], p: f64) -> T {
    if sorted.is_empty() {
        return T::default();
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn rate(n: u64, calls: u64) -> f64 {
    if calls == 0 {
        0.0
    } else {
        n as f64 / calls as f64
    }
}

/// What an eval call exercises: an index into the bundle's `operations` or
/// `channels`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetRef {
    Operation(usize),
    Channel(usize),
}

/// One contract of an eval run, with the targets it calls.
#[derive(Clone, Debug)]
pub struct EvalContract {
    /// Keys `per_contract` in the report.
    pub name: String,
    pub bundle: Arc<Bundle>,
    /// Set by a manifest; keys `per_api_type`.
    pub api_type: Option<String>,
    /// Set by a manifest; keys `per_domain`.
    pub domain: Option<String>,
    pub targets: Vec<TargetRef>,
    /// The narrower response schema an operation target is called with, by
    /// operation id: a GraphQL selection's (see [`crate::selection`]).
    pub response_schemas: BTreeMap<String, Value>,
}

impl EvalContract {
    /// A corpus contract with the targets of [`default_targets`].
    pub fn from_corpus(entry: &CorpusEntry, max_ops: usize) -> Self {
        Self {
            name: entry.name.clone(),
            targets: default_targets(&entry.bundle, max_ops),
            bundle: Arc::new(entry.bundle.clone()),
            api_type: None,
            domain: None,
            response_schemas: BTreeMap::new(),
        }
    }
}

/// How an eval run calls its targets.
#[derive(Clone, Debug, PartialEq)]
pub struct RunOptions {
    /// Calls per target (each with a fresh faker-built request).
    pub repeat: usize,
    /// Calls in flight at most.
    pub concurrency: usize,
    pub engine: EngineSettings,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            repeat: 1,
            concurrency: 1,
            engine: production_engine_settings(),
        }
    }
}

/// One eval call, as written to the samples file.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Sample {
    pub contract: String,
    pub api_type: Option<String>,
    pub domain: Option<String>,
    /// The operation or channel id.
    pub target: String,
    /// `operation` or `channel`.
    pub target_kind: String,
    /// 0-based repetition of this target.
    pub rep: usize,
    /// `llm`, `repaired`, `fallback`, or `error` when the engine rejected
    /// the call (see `error`).
    pub source: String,
    pub latency_ms: u64,
    /// The response body or message payload; null on error.
    pub body: Value,
    /// The schema the engine validated `body` against, self-contained: the
    /// success response (or message payload) schema with its reachable
    /// definitions attached as `$defs`.
    pub schema: Value,
    /// The faker-built call (`path`, `query`, `body`) or the message
    /// `trigger`.
    pub request: Value,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Why the engine rejected the call.
    pub error: Option<String>,
    /// Why the faker answered instead of the LLM, when the engine says.
    pub fallback_reason: Option<String>,
}

/// The unary operations with a success schema, then the publish channels,
/// of `bundle`, at most `max_ops` in all.
pub fn default_targets(bundle: &Bundle, max_ops: usize) -> Vec<TargetRef> {
    let ops = bundle
        .operations
        .iter()
        .enumerate()
        .filter(|(_, op)| eligible_operation(op))
        .map(|(i, _)| TargetRef::Operation(i));
    let channels = bundle
        .channels
        .iter()
        .enumerate()
        .filter(|(_, c)| c.direction == Direction::Publish)
        .map(|(i, _)| TargetRef::Channel(i));
    ops.chain(channels).take(max_ops).collect()
}

/// The targets named by `ids`, in order: each an operation id (unary, with
/// a success schema) or else a publish channel id. `Err` names the first id
/// that is unknown or not eligible.
pub fn select_targets(bundle: &Bundle, ids: &[String]) -> Result<Vec<TargetRef>, String> {
    ids.iter()
        .map(|id| {
            if let Some((i, op)) = bundle
                .operations
                .iter()
                .enumerate()
                .find(|(_, op)| &op.id == id)
            {
                return if eligible_operation(op) {
                    Ok(TargetRef::Operation(i))
                } else {
                    Err(format!(
                        "operation {id} streams or has no success schema, so it cannot be evaluated"
                    ))
                };
            }
            match bundle
                .channels
                .iter()
                .enumerate()
                .find(|(_, c)| &c.id == id)
            {
                Some((i, c)) if c.direction == Direction::Publish => Ok(TargetRef::Channel(i)),
                Some(_) => Err(format!("channel {id} is not a publish channel")),
                None => Err(format!("unknown operation or channel id {id}")),
            }
        })
        .collect()
}

fn eligible_operation(op: &Operation) -> bool {
    !op.kind.streams_request()
        && !op.kind.streams_response()
        && op.success_output().is_some_and(|o| o.schema.is_some())
}

/// A faker-built object for an input schema (path, query or body part).
fn fake_object(engine: &Engine, bundle: &Bundle, schema: Option<&Value>) -> Map<String, Value> {
    match schema.map(|s| engine.fake(bundle, s)) {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// A faker-built call of `op`.
fn fake_call(engine: &Engine, bundle: &Bundle, op: &Operation) -> OperationCall {
    OperationCall {
        operation_id: op.id.clone(),
        path_params: fake_object(engine, bundle, op.input.path.as_ref()),
        query: fake_object(engine, bundle, op.input.query.as_ref()),
        headers: Map::new(),
        body: op.input.body.as_ref().map(|s| engine.fake(bundle, s)),
    }
}

/// One call to make.
#[derive(Clone, Copy, Debug)]
struct Job {
    contract: usize,
    target: TargetRef,
    rep: usize,
}

/// What one call produced.
struct Done {
    source: Source,
    usage: Usage,
    body: Option<Value>,
    fallback_reason: Option<String>,
}

/// One finished call.
struct Finished {
    job: Job,
    latency_ms: u64,
    target_id: String,
    request: Value,
    outcome: Result<Done, String>,
}

/// Makes the call of `job`, an operation's answering `narrowed` when set (a
/// GraphQL selection's schema); `None` when its target index is out of
/// range.
async fn call(
    engine: &Engine,
    bundle: &Bundle,
    job: Job,
    narrowed: Option<&Value>,
) -> Option<Finished> {
    let start = Instant::now();
    let (target_id, request, outcome) = match job.target {
        TargetRef::Operation(i) => {
            let op = bundle.operations.get(i)?;
            let call = fake_call(engine, bundle, op);
            let request = json!({
                "path": call.path_params,
                "query": call.query,
                "body": call.body,
            });
            let ctx = GenerationContext {
                response_schema: narrowed,
                ..GenerationContext::default()
            };
            let outcome = engine
                .generate(bundle, op, &call, ctx)
                .await
                .map(|r| Done {
                    source: r.source,
                    usage: r.usage,
                    body: r.body,
                    fallback_reason: r.fallback_reason,
                })
                .map_err(|e| e.to_string());
            (op.id.clone(), request, outcome)
        }
        TargetRef::Channel(i) => {
            let channel = bundle.channels.get(i)?;
            let ctx = MessageContext {
                trigger: json!({"kind": "eval"}),
                ..MessageContext::default()
            };
            let request = json!({ "trigger": ctx.trigger });
            let outcome = engine
                .generate_message(bundle, channel, &ctx)
                .await
                .map(|m| Done {
                    source: m.source,
                    usage: m.usage,
                    body: Some(m.payload),
                    fallback_reason: None,
                })
                .map_err(|e| e.to_string());
            (channel.id.clone(), request, outcome)
        }
    };
    Some(Finished {
        job,
        latency_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        target_id,
        request,
        outcome,
    })
}

/// The narrower schema `contract` calls operation `target` with, if any.
fn narrowed_schema(contract: &EvalContract, target: TargetRef) -> Option<Value> {
    let TargetRef::Operation(i) = target else {
        return None;
    };
    let op = contract.bundle.operations.get(i)?;
    contract.response_schemas.get(&op.id).cloned()
}

/// The schema the engine validates `target`'s answers against: the
/// narrower one `contract` calls it with, else the target's own with its
/// reachable definitions attached; null when there is none.
fn validated_schema(contract: &EvalContract, target: TargetRef) -> Value {
    if let Some(narrowed) = narrowed_schema(contract, target) {
        return narrowed;
    }
    let bundle = &contract.bundle;
    let schema = match target {
        TargetRef::Operation(i) => bundle
            .operations
            .get(i)
            .and_then(|op| op.success_output())
            .and_then(|o| o.schema.as_ref()),
        TargetRef::Channel(i) => bundle.channels.get(i).map(|c| &c.message_schema),
    };
    schema.map_or(Value::Null, |s| bundle.with_reachable_defs(s))
}

/// Stats and latencies being gathered for one group of calls.
#[derive(Default)]
struct Tally {
    stats: ContractStats,
    latencies: Vec<u64>,
}

impl Tally {
    fn add(&mut self, source: Source, latency_ms: u64) {
        self.stats.calls += 1;
        match source {
            Source::Llm => self.stats.llm += 1,
            // A local patch counts with repairs until W1 reports it apart.
            Source::Patched | Source::Repaired => self.stats.repaired += 1,
            Source::Fallback => self.stats.fallback += 1,
            // `Engine::generate` never answers from state or the cache.
            Source::State | Source::Cache | Source::Rejected => {}
        }
        self.latencies.push(latency_ms);
    }

    fn finish(self) -> ContractStats {
        self.finish_with_p50().0
    }

    /// The stats, with their p95, and the p50 latency.
    fn finish_with_p50(mut self) -> (ContractStats, u64) {
        self.latencies.sort_unstable();
        self.stats.p95_ms = percentile(&self.latencies, 0.95);
        let p50 = percentile(&self.latencies, 0.50);
        (self.stats, p50)
    }
}

fn finish_all(tallies: BTreeMap<String, Tally>) -> BTreeMap<String, ContractStats> {
    tallies.into_iter().map(|(k, t)| (k, t.finish())).collect()
}

/// Runs every eval call against `provider`, one at a time, and summarises
/// them. Each contract contributes at most `max_ops` calls (its unary
/// operations with a success schema, then its publish channels); a call the
/// engine rejects outright (an uncompilable schema) is logged and not
/// counted. `base_url` is left empty for the caller to fill in.
pub async fn run_evals(
    entries: &[CorpusEntry],
    provider: Arc<dyn mb_llm::Provider>,
    model: &str,
    max_ops: usize,
) -> EvalReport {
    let contracts: Vec<EvalContract> = entries
        .iter()
        .map(|e| EvalContract::from_corpus(e, max_ops))
        .collect();
    let mut no_samples = |_: &Sample| Ok(());
    match run_contracts(
        &contracts,
        provider,
        model,
        &RunOptions::default(),
        &mut no_samples,
    )
    .await
    {
        Ok(report) => report,
        // The sink above never fails.
        Err((report, _)) => report,
    }
}

/// Runs `opts.repeat` calls of every target of `contracts` against
/// `provider`, at most `opts.concurrency` in flight, hands each call to
/// `on_sample` as it finishes, and summarises them. A call the engine
/// rejects outright is logged, sampled with source `error`, not counted in
/// `calls` but in `skipped_calls`; streaming operations are counted in
/// `skipped_streaming_operations`. `skipped_contracts` is left empty for the
/// caller (see [`crate::corpus::load_with_skipped`]). `per_api_type`/`per_domain` cover the contracts that set them.
/// `base_url` is left empty for the caller to fill in.
///
/// After `on_sample` first fails it is not called again, the run still
/// finishes, and `Err` carries the report with that error.
pub async fn run_contracts(
    contracts: &[EvalContract],
    provider: Arc<dyn mb_llm::Provider>,
    model: &str,
    opts: &RunOptions,
    on_sample: &mut (dyn FnMut(&Sample) -> std::io::Result<()> + Send),
) -> Result<EvalReport, (EvalReport, std::io::Error)> {
    let started_at = chrono::Utc::now();
    let engine = Arc::new(Engine::new(
        Some(provider),
        EngineConfig {
            timeout: opts.engine.llm_timeout,
            max_tokens: opts.engine.max_tokens,
            temperature: opts.engine.temperature,
            output_budget: opts.engine.output_budget,
            ..EngineConfig::default()
        },
    ));
    let repeat = opts.repeat.max(1);
    let concurrency = opts.concurrency.max(1);
    let mut jobs = contracts
        .iter()
        .enumerate()
        .flat_map(|(contract, c)| {
            c.targets.iter().flat_map(move |&target| {
                (0..repeat).map(move |rep| Job {
                    contract,
                    target,
                    rep,
                })
            })
        })
        .peekable();

    let mut usage = Usage::default();
    let mut totals = Tally::default();
    let mut per_contract: BTreeMap<String, Tally> = contracts
        .iter()
        .map(|c| (c.name.clone(), Tally::default()))
        .collect();
    let mut per_api_type: BTreeMap<String, Tally> = BTreeMap::new();
    let mut per_domain: BTreeMap<String, Tally> = BTreeMap::new();
    let mut sample_error: Option<std::io::Error> = None;
    let mut skipped_calls = 0u64;
    let skipped_streaming_operations = contracts
        .iter()
        .flat_map(|c| &c.bundle.operations)
        .filter(|op| op.kind.streams_request() || op.kind.streams_response())
        .count();

    let mut in_flight = tokio::task::JoinSet::new();
    loop {
        while in_flight.len() < concurrency {
            let Some(job) = jobs.next() else { break };
            let Some(contract) = contracts.get(job.contract) else {
                continue;
            };
            let engine = Arc::clone(&engine);
            let bundle = Arc::clone(&contract.bundle);
            let narrowed = narrowed_schema(contract, job.target);
            in_flight.spawn(async move { call(&engine, &bundle, job, narrowed.as_ref()).await });
        }
        let Some(joined) = in_flight.join_next().await else {
            if jobs.peek().is_none() {
                break;
            }
            continue;
        };
        let finished = match joined {
            Ok(Some(finished)) => finished,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "eval call task failed");
                continue;
            }
        };
        let Some(contract) = contracts.get(finished.job.contract) else {
            continue;
        };
        let target_kind = match finished.job.target {
            TargetRef::Operation(_) => "operation",
            TargetRef::Channel(_) => "channel",
        };
        let mut sample = Sample {
            contract: contract.name.clone(),
            api_type: contract.api_type.clone(),
            domain: contract.domain.clone(),
            target: finished.target_id,
            target_kind: target_kind.to_owned(),
            rep: finished.job.rep,
            source: "error".to_owned(),
            latency_ms: finished.latency_ms,
            body: Value::Null,
            schema: validated_schema(contract, finished.job.target),
            request: finished.request,
            prompt_tokens: 0,
            completion_tokens: 0,
            error: None,
            fallback_reason: None,
        };
        match finished.outcome {
            Ok(done) => {
                sample.source = done.source.as_str().to_owned();
                sample.body = done.body.unwrap_or(Value::Null);
                sample.prompt_tokens = done.usage.prompt_tokens;
                sample.completion_tokens = done.usage.completion_tokens;
                sample.fallback_reason = done.fallback_reason;
                usage = usage + done.usage;
                let latency = finished.latency_ms;
                totals.add(done.source, latency);
                per_contract
                    .entry(contract.name.clone())
                    .or_default()
                    .add(done.source, latency);
                if let Some(t) = &contract.api_type {
                    per_api_type
                        .entry(t.clone())
                        .or_default()
                        .add(done.source, latency);
                }
                if let Some(d) = &contract.domain {
                    per_domain
                        .entry(d.clone())
                        .or_default()
                        .add(done.source, latency);
                }
            }
            Err(e) => {
                tracing::warn!(contract = %contract.name, target = %sample.target, error = %e, "eval call skipped");
                skipped_calls += 1;
                sample.error = Some(e);
            }
        }
        if sample_error.is_none()
            && let Err(e) = on_sample(&sample)
        {
            sample_error = Some(e);
        }
    }

    let per_contract = finish_all(per_contract);
    for (name, stats) in &per_contract {
        tracing::info!(contract = %name, ?stats, "contract evaluated");
    }
    let (totals, p50_ms) = totals.finish_with_p50();
    let calls = totals.calls;
    let per_call = |tokens: u32| {
        if calls == 0 {
            0.0
        } else {
            f64::from(tokens) / calls as f64
        }
    };
    let report = EvalReport {
        model: model.to_owned(),
        base_url: String::new(),
        started_at,
        calls,
        schema_valid_rate: rate(totals.llm + totals.repaired, calls),
        first_pass_rate: rate(totals.llm, calls),
        repair_rate: rate(totals.repaired, calls),
        fallback_rate: rate(totals.fallback, calls),
        p50_ms,
        p95_ms: totals.p95_ms,
        prompt_tokens_per_call: per_call(usage.prompt_tokens),
        completion_tokens_per_call: per_call(usage.completion_tokens),
        per_contract,
        per_api_type: finish_all(per_api_type),
        per_domain: finish_all(per_domain),
        skipped_calls,
        skipped_streaming_operations: u64::try_from(skipped_streaming_operations)
            .unwrap_or(u64::MAX),
        skipped_contracts: Vec::new(),
    };
    match sample_error {
        None => Ok(report),
        Some(e) => Err((report, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn nearest_rank_percentiles() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 0.50), 50);
        assert_eq!(percentile(&v, 0.95), 95);
        assert_eq!(percentile(&[7u64], 0.95), 7);
        assert_eq!(percentile::<u64>(&[], 0.95), 0);
        assert_eq!(percentile(&[1u64, 2], 0.0), 1);
    }
}
