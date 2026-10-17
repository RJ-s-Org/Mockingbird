//! The eval run and its report.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mb_engine::{Engine, EngineConfig, GenerationContext, MessageContext, OperationCall, Source};
use mb_llm::Usage;
use mb_mcm::{Bundle, Channel, Direction, Operation};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::corpus::CorpusEntry;

/// The per-request LLM timeout of an eval run. Generous on purpose, so a
/// slow model shows up in `p95_ms` instead of as fallbacks.
pub const EVAL_LLM_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Keyed by [`CorpusEntry::name`].
    pub per_contract: BTreeMap<String, ContractStats>,
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
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_schema_valid_rate: 0.95,
            max_repair_rate: 0.25,
            max_fallback_rate: 0.05,
            max_p95_ms: 3000,
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
        out
    }

    /// A Markdown summary: a metric table, then one row per contract.
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
        let _ = writeln!(md, "\n## Per contract\n");
        let _ = writeln!(
            md,
            "| Contract | Calls | LLM | Repaired | Fallback | p95 ms |"
        );
        let _ = writeln!(md, "| --- | --- | --- | --- | --- | --- |");
        for (name, s) in &self.per_contract {
            let _ = writeln!(
                md,
                "| {name} | {} | {} | {} | {} | {} |",
                s.calls, s.llm, s.repaired, s.fallback, s.p95_ms
            );
        }
        md
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

/// What an eval call exercises.
enum Target<'a> {
    Operation(&'a Operation),
    Channel(&'a Channel),
}

/// The unary operations with a success schema, then the publish channels,
/// of `bundle`, at most `max_ops` in all.
fn targets(bundle: &Bundle, max_ops: usize) -> Vec<Target<'_>> {
    let ops = bundle
        .operations
        .iter()
        .filter(|op| !op.kind.streams_request() && !op.kind.streams_response())
        .filter(|op| op.success_output().is_some_and(|o| o.schema.is_some()))
        .map(Target::Operation);
    let channels = bundle
        .channels
        .iter()
        .filter(|c| c.direction == Direction::Publish)
        .map(Target::Channel);
    ops.chain(channels).take(max_ops).collect()
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
    let started_at = chrono::Utc::now();
    let engine = Engine::new(
        Some(provider),
        EngineConfig {
            timeout: EVAL_LLM_TIMEOUT,
            ..EngineConfig::default()
        },
    );
    let mut latencies: Vec<u64> = Vec::new();
    let mut usage = Usage::default();
    let mut per_contract = BTreeMap::new();
    let mut totals = ContractStats::default();

    for entry in entries {
        let bundle = &entry.bundle;
        let mut stats = ContractStats::default();
        let mut contract_latencies = Vec::new();
        for target in targets(bundle, max_ops) {
            let start = Instant::now();
            let outcome = match target {
                Target::Operation(op) => {
                    let call = fake_call(&engine, bundle, op);
                    engine
                        .generate(bundle, op, &call, GenerationContext::default())
                        .await
                        .map(|r| (r.source, r.usage))
                        .map_err(|e| (op.id.clone(), e))
                }
                Target::Channel(channel) => {
                    let ctx = MessageContext {
                        trigger: json!({"kind": "eval"}),
                        ..MessageContext::default()
                    };
                    engine
                        .generate_message(bundle, channel, &ctx)
                        .await
                        .map(|m| (m.source, m.usage))
                        .map_err(|e| (channel.id.clone(), e))
                }
            };
            let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            let (source, call_usage) = match outcome {
                Ok(done) => done,
                Err((id, e)) => {
                    tracing::warn!(contract = %entry.name, target = %id, error = %e, "eval call skipped");
                    continue;
                }
            };
            stats.calls += 1;
            match source {
                Source::Llm => stats.llm += 1,
                Source::Repaired => stats.repaired += 1,
                Source::Fallback => stats.fallback += 1,
            }
            usage = usage + call_usage;
            contract_latencies.push(elapsed);
        }
        contract_latencies.sort_unstable();
        stats.p95_ms = percentile(&contract_latencies, 0.95);
        totals.calls += stats.calls;
        totals.llm += stats.llm;
        totals.repaired += stats.repaired;
        totals.fallback += stats.fallback;
        latencies.extend(contract_latencies);
        tracing::info!(contract = %entry.name, ?stats, "contract evaluated");
        per_contract.insert(entry.name.clone(), stats);
    }

    latencies.sort_unstable();
    let calls = totals.calls;
    let per_call = |tokens: u32| {
        if calls == 0 {
            0.0
        } else {
            f64::from(tokens) / calls as f64
        }
    };
    EvalReport {
        model: model.to_owned(),
        base_url: String::new(),
        started_at,
        calls,
        schema_valid_rate: rate(totals.llm + totals.repaired, calls),
        first_pass_rate: rate(totals.llm, calls),
        repair_rate: rate(totals.repaired, calls),
        fallback_rate: rate(totals.fallback, calls),
        p50_ms: percentile(&latencies, 0.50),
        p95_ms: percentile(&latencies, 0.95),
        prompt_tokens_per_call: per_call(usage.prompt_tokens),
        completion_tokens_per_call: per_call(usage.completion_tokens),
        per_contract,
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
