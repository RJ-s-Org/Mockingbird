# LLM benchmark rubric

How the 2026-10-29 model benchmark turns raw records into scores. Inputs live in
`docs/benchmarks/<date>/`: `calls.jsonl` (proxy ledger), `samples.jsonl` (mb-evals
samples, one per engine call, with a `model` field added by `run.py`),
`grades.jsonl` (`grade.py`), `server.jsonl` (`server_suites.py`) and `judge.jsonl`
(this protocol). `aggregate.py` implements every rule below mechanically; where this
document says "report", the value goes into `results.json` under the model's entry.

Conventions: `N_m` is the number of engine samples for model `m` (rows of
`samples.jsonl` with `model == m`). A *sample key* is `contract|target|rep`.
Rates are in [0, 1]. A rate whose denominator is 0 is `null`, never 0. Percentiles
use nearest rank: `pXX(x) = sorted(x)[ceil(XX/100 * n) - 1]`.

## 1. Blinded quality judging

### 1.1 Stratified sample

The judged set is a fixed list of **targets**, identical for every model, so a judge
always compares like with like.

1. Candidate targets are `(contract, target)` pairs present in `samples.jsonl` for
   **every** model that was run (skipped models do not shrink the pool).
2. Strata, in this order: one target per `api_type` present (`rest`, `graphql`,
   `grpc`, `soap`, `asyncapi`), then one target with `domain == "travel"` and one
   with `domain == "payments"`. A domain pick must differ from every target already
   chosen; if no unchosen candidate exists for a stratum, the stratum is skipped and
   recorded in `judge_strata_skipped`.
3. Within a stratum the pick is the candidate with the smallest
   `sha256(seed + "|" + contract + "|" + target)` hex digest. The choice never looks at
   outcomes (source, violations, latency), so it cannot favour any model.
4. Rep: the smallest `rep` present for every model at that target (normally 0).

Default `seed = "mb-bench-2026-10-29"`; it is written to `judge.jsonl` and
`results.json`. With 5 api types and 2 domains this gives up to 7 targets, i.e. 7
batches x 13 models = 91 judged items.

### 1.2 Blinding

Each item gets `opaque_id = sha256(seed + "|" + model + "|" + contract + "|" + target + "|" + rep)[:10]`.
The id-to-model map is written to `judge-key.json`, which the judge never sees.
The judge sees only: `opaque_id`, contract title, `api_type`, `domain`, `target`,
`request` (the faker call or message trigger), `schema` (truncated to 4,000 chars),
and `body` (pretty JSON, truncated to 6,000 chars with a trailing `...(truncated)`).
Stripped: model id/name, provider, `source`, latency, token counts, cost, error text
and any `id`/`generation_id` from the ledger. If a body string contains a model or
vendor name verbatim (e.g. "as an AI model by ..."), it is left in: that is a
realism defect the judge should punish.

### 1.3 Judge batches

One **batch** = one target answered by every model (one item per model). Items in a
batch are ordered by `sha256(seed + "|" + batch_id + "|" + opaque_id)` ascending;
batches are presented in the order of their `batch_id` hash the same way. The judge
scores each item independently on both scales but sees the whole batch, so anchors
are applied relatively consistently. The judge must be a model **not** on the roster
(record it as `judge`); judge calls go through the proxy under label
`judge~judge` and count toward the budget, not toward any model's cost.

Order check (optional, cheap): re-judge 2 batches with the order reversed; report
`judge_order_mad` = mean absolute difference of item scores. Above 0.5 means the
judge scores are noisy and the judge weight in Q (section 4) should be read as such.

`judge.jsonl` line: `{"seed","judge","batch_id","opaque_id","realism":1-5,"coherence":1-5,"rationale":"<=300 chars","auto":false}`.
`aggregate.py` unblinds through `judge-key.json`.

### 1.4 Scales (integers 1-5)

**Realism**: would this pass as a response from the real API in this domain?

| Score | Anchor |
|---|---|
| 5 | Indistinguishable from production data: plausible, varied values (real-looking names, addresses, IATA codes, ISO currencies, amounts with correct minor units, sensible timestamps), domain-correct ids and formats (`pi_...` for Stripe, PNR-like booking refs), no filler. |
| 4 | Realistic overall; one or two bland or repetitive values (e.g. all prices 100.00, sequential ids) but nothing a developer would flag. |
| 3 | Clearly synthetic in places: generic values ("John Doe", "Item 1"), round or repeated numbers, a domain slip (a flight arriving before departure is coherence, but a 3-letter airport code that does not exist is realism). |
| 2 | Mostly placeholder-like: several fields read "string", "example", "foo", "lorem ipsum", "test", `0`, empty arrays where the request implies data, or schema descriptions echoed as values. |
| 1 | Not usable as mock data: placeholder text throughout, wrong domain entirely, prose or markdown instead of data, a refusal, or an empty/default body. |

**Coherence**: is the body internally consistent and consistent with the request?

| Score | Anchor |
|---|---|
| 5 | Every cross-field relation holds: totals equal sum of line items, `end > start`, currency matches amounts' minor units, status matches timestamps (a `succeeded` charge has `captured: true`), ids requested in the path/query are echoed, filters/pagination in the request are respected, persona or scenario cues (if present in the request or contract) are followed. |
| 4 | One minor inconsistency (a count off by one, a timestamp in a different time zone than the others). |
| 3 | One material inconsistency (total does not match items, wrong id echoed, filter ignored) or two minor ones. |
| 2 | Several contradictions; the body would break a client test that checks relations. |
| 1 | Self-contradictory or unrelated to the request (answers a different operation, wrong resource type). |

Judges score what is shown; schema conformance is measured by `grade.py`, not by the
judge, so an item with a grade violation is judged normally and penalised once, in
the conformance term (no double counting).

### 1.5 Fallback, error and missing items

- `source == "fallback"`: the body is faker output, not the model's. It is **not shown**
  to the judge (it would unblind the batch) and is auto-scored `realism = 1,
  coherence = 1, auto = true`.
- Non-null `error` or empty body: auto-scored 1/1 the same way.
- Model skipped (budget, preflight): no items; the model gets no judge score and no
  Value.
- Model ran but lacks the sample at that key: auto-scored 1/1.

Per-model judge score: `J_m = mean over the model's items of (realism + coherence) / 2`,
auto items included; `Jn_m = (J_m - 1) / 4` in [0, 1]. Also report realism and
coherence means separately and `judge_n` (items).

## 2. Accuracy metrics

Engine samples (from `samples.jsonl`; `state`/`cache`/`rejected` rows are excluded
from `N_m`, they do not occur in evals):

- `first_try_rate = count(source == "llm") / N_m`
- `schema_valid_rate = count(source in {"llm","patched","repaired"}) / N_m` (after
  repair). `patched` is the engine's local fix with no second LLM call; runs before
  #142 (2026-10-29) report it as `repaired`, so compare their sum across rounds
- `patch_rate = count(source == "patched") / N_m`
- `repair_rate = count(source == "repaired") / N_m` (an LLM repair round trip)
- `fallback_rate = count(source == "fallback") / N_m`
- `fallback_causes`: each fallback counted once, by the engine's reason:
  `truncated` (cut at `max_tokens`), `non_json` (still not JSON after the
  repair), `schema` (still invalid after the repair), `timeout` (the LLM
  timeout), `guard` (token budget, generation slot, size cap) or `other`

Conformance (from `grades.jsonl`, joined on model + sample key), over non-fallback
samples only (`N'_m = N_m - fallback`):

- `conformance_sample = count(violations == []) / N'_m`. A `format` violation
  whose value is one of the request's path or query values is an echo (the
  response repeats the id the client asked for), listed under `echoed` and
  not counted as a violation
- `conformance_kw[k] = passed_k / checked_k` for `k in {format, enum, constraint}`
  (constraint = min/max/exclusive*/minLength/maxLength/pattern/minItems/maxItems),
  summed over the model's non-fallback samples.

Every engine rate is reported overall (micro), per `api_type` and per `domain`. The
headline value used in section 4 is the **macro average over api_types** (each api
type weighs equally, so the REST-heavy manifest does not dominate).

Server suites (from `server.jsonl`):

- `consistency_case_rate = count(ok) / rows` for `suite == "consistency"`.
- `consistency_field_agreement = sum(agreed fields) / sum(compared fields)` (from `detail`).
- `sse_ok_rate = count(ok) / rows`, `sse_ttfe_p50 = p50(ttfe_ms)` for `suite == "sse"`.
- Smart import, per input: `draft_valid = 1` iff the job status is `done`, it has a
  `result_version`, and the draft was fetched. `draft_valid_rate = mean(draft_valid)`.
  Ops are normalised as `METHOD /path` (method uppercased, trailing `/` dropped, every
  `{param}` replaced by `{}`). With `E` = expected set and `D` = draft set:
  `op_recall = |E ∩ D| / |E|`, `op_precision = |E ∩ D| / |D|` (0 when `D` is empty or
  the job failed). Report the macro mean over inputs and the micro value (sums of
  intersections over sums of sizes), plus mean `duration_ms` and warnings.

## 3. Latency, reliability, cost

Source is `calls.jsonl`. Rows with `error_kind == "budget"` are excluded from every
denominator (they never reached the model).

- **Successful call**: `status == 200 and error_kind is null`.
- `TTFT p50/p95` = percentiles of `ttft_ms` over successful calls with non-null `ttft_ms`.
  Also report `ttfr_ms` p50 when present (reasoning token arrived first).
- `e2e p50/p95` = percentiles of `total_ms` over successful calls. These are LLM call
  times only (proxy-measured), not Mockingbird server wall time. `L_m` (section 4) is
  e2e p50 over the **engine workload** (`workload == "engine"`), where every model
  answers the same prompts; the all-workload figures are reported alongside.
- `output tok/s` per call = `completion_tokens / ((total_ms - t0) / 1000)` with
  `t0 = ttfr_ms if ttfr_ms is not null else ttft_ms`, only for calls with
  `completion_tokens >= 16` and `total_ms - t0 >= 50`. Report the **median** of
  per-call rates. (Deviation from the plain `ttft_ms` form: OpenRouter's
  `completion_tokens` includes reasoning tokens, which stream before content, so the
  generation window starts at the first token of either kind.)
- `error_rate = count(error_kind in {http_4xx, http_5xx, stream_error, empty}) / n`
- `timeout_rate = count(error_kind == "timeout") / n`
- `refusal_rate = count(error_kind == "refusal") / n`
- `retry_rate = count(attempts > 1) / n`; report `reasoning_rejected` and
  `response_format_rejected` as booleans.
- **Cost per 1,000 mock responses**:
  `C_m = 1000 * sum(cost_usd over engine-workload rows of m) / N_m`. All rows count,
  including failed calls and repair calls: they are the real price of producing a
  response. Report `cost_estimated_share` = share of that cost with
  `cost_source == "estimate"`. Also report total spend per model across all workloads.

## 4. Composite Value score

Quality index (each term in [0, 1]):

- `S = 0.5 * first_try_rate + 0.5 * schema_valid_rate`. Half weight on first try,
  because a repair doubles the response time for that request and `L_m` (per LLM call)
  does not see it.
- `K = conformance_sample` (non-fallback samples): depth of correctness beyond the
  engine's own validator.
- `Jn` from section 1.5.
- `Q = 0.35 * S + 0.25 * K + 0.40 * Jn`

Judge quality gets the largest single weight because schema validity is table
stakes that most models clear; what separates them is whether the data looks real.
`S` and `K` together still weigh 0.60, so a model cannot buy a good Value with
pretty but invalid output.

Value relative to the baseline `b = deepseek/deepseek-v4.1-flash`, the shipped default when rounds 1 and 2 ran. It stays the baseline so their figures remain comparable:

`Value = 100 * (Q / Q_b)^2 / ( (C / C_b)^0.5 * (L / L_b)^0.5 )`

Exponents: `a = 2` on quality, `b = c = 0.5` on cost and latency. Cost across the
roster spans one to two orders of magnitude while quality spans maybe 30 %; with
linear cost (`b = 1`) the cheapest model wins regardless of output. With these
exponents, doubling cost is offset by `2^(0.25) = +19 %` quality, and halving
latency is worth the same as a 19 % quality gain. Latency and cost weigh equally
because both scale per request for a mock server. `aggregate.py` also reports the
ranking under `(a,b,c) = (1,1,1)` and `(2,0.25,1)` as `value_sensitivity` so readers
can see whether the order is stable.

Gates (Value is `null`, `gated: "<reason>"`, still listed with its metrics):

- `schema_valid_rate < 0.90` (`schema_valid`)
- `error_rate + timeout_rate > 0.10` (`unreliable`)
- `judge_n < 5` (`unjudged`)

Floors: `C` is clamped to `max(C, 0.01)` (USD per 1k) so a free endpoint does not
divide by zero. The baseline's raw `Q_b, C_b, L_b` are used even if the baseline is
gated. If the baseline did not run, use the model with median `Q` among ungated
models and record `baseline_used`.

### Worked example (made-up numbers)

| Model | first_try | schema_valid | K | J | Q | C ($/1k) | L (ms) | Value |
|---|---|---|---|---|---|---|---|---|
| Baseline B | 0.80 | 0.96 | 0.90 | 3.8 | 0.8130 | 0.40 | 1800 | 100.0 |
| X | 0.92 | 0.99 | 0.95 | 4.4 | 0.9118 | 1.60 | 1200 | 77.0 |
| Z | 0.70 | 0.93 | 0.85 | 3.2 | 0.7178 | 0.10 | 900 | 220.5 |
| Y | 0.60 | 0.85 | - | - | - | - | - | gated: schema_valid |

B: `S = 0.88`, `Jn = 0.70`, `Q = 0.35*0.88 + 0.25*0.90 + 0.40*0.70 = 0.813`.
X: `Q/Q_b = 1.1215`, squared 1.2577; cost ratio 4 -> 2.0; latency ratio 0.667 ->
0.8165; `Value = 100 * 1.2577 / (2.0 * 0.8165) = 77.0`. X is the best model, but at
4x the price it is worse value. Z: `(0.8829)^2 = 0.7795`; `0.25^0.5 * 0.5^0.5 =
0.3536`; `Value = 220.5`. Value is a price/performance lens: `tables.md` always
shows the Q ranking next to it, and a model is only recommended as the default if it
is within 10 % of the best Q.

## Formulas

```
N      = engine samples of the model (source in llm|patched|repaired|fallback)
FT     = n(llm)/N                 SV = n(llm|patched|repaired)/N         FB = n(fallback)/N
K      = n(non-fallback with 0 violations) / (N - n(fallback))
J      = mean((realism + coherence)/2) over judged items, auto items = 1
Jn     = (J - 1)/4
S      = 0.5*FT + 0.5*SV          (FT, SV, K: macro mean over api_types)
Q      = 0.35*S + 0.25*K + 0.40*Jn
C      = max(0.01, 1000 * sum(cost_usd | workload=="engine") / N)
L      = p50(total_ms | workload=="engine", status==200, error_kind null)
tok/s  = median( completion_tokens / ((total_ms - coalesce(ttfr_ms, ttft_ms))/1000) )
err    = n(error_kind in http_4xx|http_5xx|stream_error|empty) / n(rows, excl. budget)
Value  = 100 * (Q/Q_b)^2 / ((C/C_b)^0.5 * (L/L_b)^0.5)
gate   = SV < 0.90  or  err + timeout_rate > 0.10  or  judge_n < 5  -> Value = null
pXX(x) = sorted(x)[ceil(XX/100*len(x)) - 1]
```
