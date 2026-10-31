# Model benchmarks

Mockingbird can use any OpenAI-compatible model. This page records two benchmark rounds on Mockingbird's own generation code. Each round measures schema validity, conformance, blinded realism and coherence scores, latency, reliability and cost per 1,000 mock responses, and combines them into one Value score against a baseline, `deepseek/deepseek-v4.1-flash`, the shipped default when both rounds ran. The default is now `deepseek/deepseek-v4.1-flash-1017`; the numbers below stay as measured, still relative to that baseline.

- **Round 2 (2026-10-30):** 12 configurations, at production settings (16,384 output tokens, 60 s timeout), after the engine's output-validity fixes.
- **Round 1 (2026-10-29):** 16 configurations, at the bench's earlier 2,048-token cap and 30 s timeout.

The short version:

- Every round-2 configuration clears the 0.90 schema-valid gate; in round 1, 5 of 16 did. On the same 12 models and the same 1,944 calls, fallbacks to the faker fell from 239 to 59. See [round 2](#round-2-2026-10-30).
- `openai/gpt-6-luna` has the best Value in both rounds (160.5 in round 2). `deepseek/deepseek-v4.1-flash-1017` is second (144.0). `anthropic/claude-haiku-5.5` has the best quality score (Q 0.934). The top quality scores are close to each other given the sample size.
- `thinkingmachines/inkling` now runs, with `output_mode: none`: SV 0.992, J 4.00. Its Value (32.6) is the lowest, because it costs $2.21 per 1,000 responses.
- After these results, the shipped default moved from `deepseek/deepseek-v4.1-flash` to `deepseek/deepseek-v4.1-flash-1017`. No figure here was re-based: `deepseek/deepseek-v4.1-flash` is still Value 100. C is what OpenRouter charged under default routing, which spreads calls across many providers at different prices: `deepseek/deepseek-v4.1-flash-1017`'s $0.324 per 1,000 responses is below what its listed price implies. That listed price was $0.15/$0.60 per million tokens in round 1, $0.30/$1.20 when round 2 started, and $0.15/$0.60 again later that day, so expect its cost to move.

- Since these rounds, `output_mode` defaults to `json_schema`. On the default model, first try per call rose from 66% to 99% with no measured quality cost; other models gain less. See [the experiments](benchmarks/first-try-2026-10-30/README.md).
- Mocks now keep a simulated world in a session, from a contract alone. Multi-step journey checks rose from 54% to 96% across three models, with a third fewer LLM calls and no measured cost to single answers. See [the journeys](benchmarks/journeys-2026-10-30/README.md).

![The live bench view during part 2 of the full run: a table of 16 models with schema-valid, fallback, latency and cost columns filling in as Gemini, Beam and the Inkling variants run, followed by the server suites](benchmarks/media/05-full-run-part2-short.gif)

## Round 2 (2026-10-30)

Round 2 re-ran 12 configurations after the output-validity work. Raw data and full tables are in [`2026-10-30/`](https://github.com/RJ-s-Org/Mockingbird/tree/main/docs/benchmarks/2026-10-30), with [`tables.md`](benchmarks/2026-10-30/tables.md) for every breakdown.

### What changed since round 1

Engine (what a Mockingbird user gets):

- JSON extraction takes the first complete value. Two objects back to back, or trailing prose with braces, no longer fail. A broken or cut answer is never reduced to an inner fragment ([#142](https://github.com/RJ-s-Org/Mockingbird/pull/142)).
- `output_mode: none | json_object | json_schema` replaces `json_schema: bool`. With `none`, no `response_format` is sent ([#142](https://github.com/RJ-s-Org/Mockingbird/pull/142)).
- In JSON mode, an array-root schema is asked for as `{"items": [...]}` and unwrapped ([#142](https://github.com/RJ-s-Org/Mockingbird/pull/142)).
- A local fix with no second call is reported as `patched`, separate from `repaired`, which now means only an LLM repair round trip ([#142](https://github.com/RJ-s-Org/Mockingbird/pull/142)).
- The faker's fallback bodies are schema-valid for every example contract ([#139](https://github.com/RJ-s-Org/Mockingbird/pull/139), [#144](https://github.com/RJ-s-Org/Mockingbird/pull/144)).
- Repair prompts say what is wrong with the shape before listing errors ([#158](https://github.com/RJ-s-Org/Mockingbird/pull/158)).
- An answer cut at the token limit is closed and refilled instead of discarded ([#158](https://github.com/RJ-s-Org/Mockingbird/pull/158)).
- A provider that rejects the reasoning-off switch is retried once with minimal effort ([#158](https://github.com/RJ-s-Org/Mockingbird/pull/158)).

Bench (how it measures):

- mb-evals runs with the production profile settings: `max_tokens` 16,384, a 60 s timeout and the 1,500-token output budget ([#104](https://github.com/RJ-s-Org/Mockingbird/pull/104)).
- GraphQL targets are called with a selection set ([#104](https://github.com/RJ-s-Org/Mockingbird/pull/104)).
- A format violation that echoes a request value (a path id like `garnet` in a `uuid` field) is not counted against the model ([#104](https://github.com/RJ-s-Org/Mockingbird/pull/104)).
- Fallbacks are classified by cause ([#104](https://github.com/RJ-s-Org/Mockingbird/pull/104)).
- `@nojson` configurations run the engine with `output_mode: none` ([#157](https://github.com/RJ-s-Org/Mockingbird/pull/157)).

Both kinds of change move the numbers, and this run cannot separate them. Truncation was mostly the bench's 2,048-token cap. Doubled JSON objects were an engine defect.

### Roster

The same 12 configurations as round 1, minus four. The models were checked against the live OpenRouter catalog on 2026-10-30, and nothing newer had shipped in these families.

| Dropped | Why |
| --- | --- |
| `google/gemini-3.8-flash` | Cost: $1.16 of round 1's ledger, the second largest. Reasoning-heavy: 53 calls cut at the token limit. |
| `reflection/beam-1.0-501b-a23b` | Cost and reasoning: 78 calls cut at the token limit, the most of any model. |
| `thinkingmachines/inkling` (JSON mode) | It does not work with JSON mode (round 1 SV 0.000). The `@nojson` configuration covers the model. |
| `thinkingmachines/inkling-small` (JSON mode) | The same; the `@nojson` configuration covers it. |

Round 2 had to fit in about $4 so that the two rounds together stay under $10. Their round-1 figures are below, labelled round 1.

### Leaderboard

Sorted by Value, the default-exponent formula against the baseline `deepseek/deepseek-v4.1-flash` = 100. All 12 pass every gate. Q is quality, SV the macro schema-valid rate, First try the share answered validly with no fix, averaged over API types, J the judge score, C the cost per 1,000 responses, L the p50 end-to-end latency. "Round 1 Value" is the ungated figure in parentheses where round 1 gated the model.

| # | Model | Value | Q | SV | First try | J (1-5) | C $/1k | L p50 ms | Round 1 Value |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | `openai/gpt-6-luna` | 160.5 | 0.925 | 1.000 | 0.953 | 4.33 | 0.198 | 2,660 | 125.1 |
| 2 | `deepseek/deepseek-v4.1-flash-1017` (default now) | 144.0 | 0.909 | 0.998 | 0.852 | 4.36 | 0.324 | 1,882 | 87.4 |
| 3 | `nvidia/nemotron-3.5-lightning` | 134.6 | 0.797 | 0.931 | 0.664 | 3.71 | 0.160 | 2,579 | (94.5) |
| 4 | `inception/mercury-3` | 128.6 | 0.788 | 0.975 | 0.710 | 3.43 | 0.435 | 989 | (137.1) |
| 5 | `anthropic/claude-haiku-5.5` | 120.4 | 0.934 | 0.998 | 0.869 | 4.57 | 0.433 | 2,240 | 84.1 |
| 6 | `nvidia/nemotron-3.5-super-vl` | 102.1 | 0.887 | 0.992 | 0.811 | 4.21 | 0.439 | 2,498 | (64.9) |
| 7 | `deepseek/deepseek-v4.1-flash` (baseline, default then) | 100.0 | 0.911 | 0.996 | 0.864 | 4.36 | 0.365 | 3,491 | 100.0 |
| 8 | `minimax/minimax-m3.1-flash` | 88.1 | 0.908 | 0.959 | 0.762 | 4.57 | 0.607 | 2,671 | (101.3) |
| 9 | `z-ai/glm-5.3-flash` | 79.2 | 0.884 | 0.998 | 0.815 | 4.17 | 0.409 | 4,411 | (61.1) |
| 10 | `qwen/qwen3.8-flash` | 64.8 | 0.864 | 1.000 | 0.796 | 4.00 | 0.311 | 7,887 | (52.7) |
| 11 | `thinkingmachines/inkling-small@nojson` | 63.6 | 0.842 | 0.975 | 0.735 | 3.93 | 0.887 | 2,596 | (61.8) |
| 12 | `thinkingmachines/inkling@nojson` | 32.6 | 0.855 | 0.992 | 0.750 | 4.00 | 2.211 | 4,202 | 31.9 |

Round 1 only, not re-run (2,048 tokens, 30 s):

| Model | Round 1 Value (ungated) | Round 1 SV | Round 1 J |
| --- | ---: | ---: | ---: |
| `google/gemini-3.8-flash` | (11.6) | 0.826 | 4.14 |
| `reflection/beam-1.0-501b-a23b` | (21.7) | 0.722 | 3.43 |
| `thinkingmachines/inkling-small` | (41.4) | 0.803 | 3.93 |
| `thinkingmachines/inkling` | — | 0.000 | 1.00 |

First try weighs each API type equally, and gRPC is 100% for every model. Per call, across all 1,944 answers in `samples.jsonl.gz`, it's lower:

| Model | First try per call | REST only | No second call |
| --- | ---: | ---: | ---: |
| `openai/gpt-6-luna` | 0.852 | 0.765 | 0.994 |
| `deepseek/deepseek-v4.1-flash-1017` | 0.660 | 0.500 | 0.975 |
| `nvidia/nemotron-3.5-lightning` | 0.370 | 0.147 | 0.679 |
| `inception/mercury-3` | 0.469 | 0.275 | 0.926 |
| `anthropic/claude-haiku-5.5` | 0.735 | 0.627 | 0.932 |
| `nvidia/nemotron-3.5-super-vl` | 0.549 | 0.324 | 0.926 |
| `deepseek/deepseek-v4.1-flash` | 0.679 | 0.520 | 0.988 |
| `minimax/minimax-m3.1-flash` | 0.586 | 0.461 | 0.815 |
| `z-ai/glm-5.3-flash` | 0.636 | 0.490 | 0.883 |
| `qwen/qwen3.8-flash` | 0.630 | 0.490 | 0.994 |
| `thinkingmachines/inkling-small@nojson` | 0.494 | 0.294 | 0.852 |
| `thinkingmachines/inkling@nojson` | 0.500 | 0.284 | 0.907 |

The baseline's own p50 latency rose from 2,204 to 3,491 ms between rounds. Value is relative to the baseline, so that rise lifts every other model's Value; compare Q, C and L directly as well.

Value sensitivity, rank in parentheses:

| Model | (2, 0.5, 0.5) default | (1, 1, 1) | (2, 0.25, 1) |
| --- | ---: | ---: | ---: |
| `openai/gpt-6-luna` | 160.5 (1) | 246.1 (3) | 157.7 (3) |
| `deepseek/deepseek-v4.1-flash-1017` | 144.0 (2) | 208.6 (4) | 190.3 (2) |
| `nvidia/nemotron-3.5-lightning` | 134.6 (3) | 270.5 (1) | 127.4 (5) |
| `inception/mercury-3` | 128.6 (4) | 256.0 (2) | 252.4 (1) |
| `anthropic/claude-haiku-5.5` | 120.4 (5) | 134.6 (5) | 156.8 (4) |
| `deepseek/deepseek-v4.1-flash` | 100.0 (7) | 100.0 (7) | 100.0 (8) |

### Validity, round 1 against round 2

Same 12 configurations, same workload, 1,944 engine calls per round. SV is the macro schema-valid rate (1 minus fallback, averaged over API types). Patched answers were counted as repaired in round 1.

| Model | SV round 1 | SV round 2 | Fallback round 1 | Fallback round 2 | Patched round 2 | Repaired round 2 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `openai/gpt-6-luna` | 0.949 | 1.000 | 6% | 0% | 14% | 1% |
| `qwen/qwen3.8-flash` | 0.898 | 1.000 | 8% | 0% | 36% | 1% |
| `deepseek/deepseek-v4.1-flash-1017` | 0.936 | 0.998 | 6% | 1% | 31% | 2% |
| `z-ai/glm-5.3-flash` | 0.853 | 0.998 | 17% | 1% | 25% | 11% |
| `anthropic/claude-haiku-5.5` | 0.930 | 0.998 | 8% | 1% | 20% | 6% |
| `deepseek/deepseek-v4.1-flash` | 0.929 | 0.996 | 6% | 1% | 31% | 0% |
| `nvidia/nemotron-3.5-super-vl` | 0.872 | 0.992 | 14% | 2% | 38% | 5% |
| `thinkingmachines/inkling@nojson` | 0.909 | 0.992 | 9% | 2% | 41% | 7% |
| `inception/mercury-3` | 0.837 | 0.975 | 14% | 4% | 46% | 4% |
| `thinkingmachines/inkling-small@nojson` | 0.846 | 0.975 | 17% | 8% | 36% | 7% |
| `minimax/minimax-m3.1-flash` | 0.889 | 0.959 | 13% | 5% | 23% | 14% |
| `nvidia/nemotron-3.5-lightning` | 0.746 | 0.931 | 30% | 12% | 31% | 20% |

Fallbacks by cause, same 12 configurations:

| Cause | Round 1 | Round 2 |
| --- | ---: | ---: |
| Truncated at the token limit | 87 | 2 |
| Not JSON after repair | 26 | 5 |
| Schema-invalid after repair | 108 | 51 |
| Timeout | 12 | 1 |
| Other | 6 | 0 |
| **Total** | **239** | **59** |

A patched answer is the model's own answer fixed locally, with no second call. Of the 549 patched answers that could be matched to their raw output, 504 had a value replaced for failing a contract `pattern`, mostly exact-length identifiers such as `^pi_[A-Za-z0-9]{24}$`, and 445 had nothing else changed. The replacement is independent fake data, so related fields can stop matching: `client_secret` starts with the `id` in all 11 first-try Stripe payment intents and in none of the 54 patched ones. Patch rates of 14% to 46% mean the engine's local fixes do much of the work. First-try rate is the stricter view of what each model gets right unaided.

Schema-invalid answers are the largest remaining cause. `nvidia/nemotron-3.5-lightning` accounts for 18 of the 51 (14 of its 19 fallbacks are REST targets).

Two cached calls, replayed side by side at their measured latency (no new LLM calls):

![Discord list_messages, an array-root response: Nemotron 3.5 Super VL and GPT-6 Luna fell back to the faker in round 1 (placeholder values like "garnet" and "kestrel") and answered with realistic messages in round 2, one after a local patch and one on the first try](benchmarks/media/08-discord-list-messages-r1-vs-r2.gif)

![Shopify Query.products: DeepSeek v4.1 Flash and Inkling without JSON mode fell back in round 1, with about 5,300 lines of recursive faker output and amounts like "ember". In round 2 both were valid on the first try, answering the selection set a real client sends](benchmarks/media/09-shopify-products-r1-vs-r2.gif)

### Judging

The same 7 targets as round 1 (same seed), one Claude Opus 5.5 judge per batch, 84 items, with 1 fallback auto-scored. A model's `{}` is now judged instead of auto-scored 1/1, and a schema-valid `{}` is left out of J, as round 1's write-up intended. Order check: fresh judges re-scored 2 batches in reverse order. The mean absolute difference was 0.06 over 24 items, against a limit of 0.5.

### Spend

| Item | Amount |
| --- | ---: |
| Round 2, key usage delta | $1.33 |
| Round 2 pilot ledger | $0.04 |
| Round 2 full-run ledger | $1.34 |
| Round 1 ledgers (full, pilot, superseded) | $5.39 |
| Both rounds | $6.77 |

The proxy enforced a hard stop at $9.50 across both rounds and refused no calls. The judges did not go through OpenRouter.

### Caveats

- Sample sizes are small: 162 engine samples per model, 6 or 7 judge items, 18 consistency rows and 4 import inputs. One judge item moving one point shifts Q by about 0.014.
- Measurement and engine changes landed together, so this run cannot say how much of the improvement each one caused.
- One judge model, from the same vendor as one roster model (Claude Haiku 5.5).
- K, keyword conformance, is 1.000 for 11 of 12 models, partly because request-id echoes no longer count against them.

### Recommendations

| Use case | Suggestion | Evidence |
| --- | --- | --- |
| Value | `openai/gpt-6-luna` | Best Value in both rounds; $0.198/1k; SV 1.000, first try 0.953 |
| Quality | `anthropic/claude-haiku-5.5` | Q 0.934, J 4.57; $0.433/1k |
| Balance | `deepseek/deepseek-v4.1-flash-1017` | Value 144.0, Q 0.909, consistency and import 100% |
| Lowest latency | `inception/mercury-3` | L 989 ms, but J 3.43 and consistency 78% |
| Inkling | `thinkingmachines/inkling-small@nojson` over `thinkingmachines/inkling@nojson` | Similar Q (0.842 against 0.855) at 40% of the cost; full Inkling's smart import produced 1 valid draft of 4 |

To run any `@nojson` configuration, set `llm.profiles.<name>.output_mode: none` ([configuration](configuration.md)).

When this round ran, the shipped default was `deepseek/deepseek-v4.1-flash`, the baseline above. Several models beat it on Value, and Luna and Haiku beat it on Q, and DeepSeek 1017 is level (0.909 against 0.911). The default is now `deepseek/deepseek-v4.1-flash-1017`.

# Round 1 (2026-10-29)

Everything from here to [Re-running](#re-running) is round 1. It ran at a 2,048-token cap and a 30 s engine timeout, with no GraphQL selections and no echo-aware grading. The method sections still describe how both rounds score. In this round, "the default" means `deepseek/deepseek-v4.1-flash`, the default at the time, and the recommendations are the ones made then.

## Method

Every number comes from Mockingbird's real code paths, pointed at OpenRouter through a recording proxy. The harness is in [`scripts/llm-bench`](../scripts/llm-bench/README.md); the scoring rules are in [`docs/benchmarks/rubric.md`](benchmarks/rubric.md), and `aggregate.py` applies them mechanically.

### Engine workload: `mb-evals run`

`mb-evals run` (in [`crates/mb-evals`](../crates/mb-evals)) builds the generation engine for each contract in [`workloads.json`](benchmarks/workloads.json) and calls it the way the server does: the same prompt, the same response-format request, schema validation, deterministic patching, a repair call and the faker fallback. Requests are built by the faker from the contract, so each model sees its own request values (an `offerId` or a customer id differs between models for the same operation). Each sample records its source (`llm`, `repaired` or `fallback`), the body, the validated schema, latency and tokens.

- 18 contracts, 54 operations, 3 repetitions: N = 162 samples per model, 2,592 in total.
- `--concurrency 4` calls in flight per model, 3 models at a time (`--parallel 3`).
- mb-evals' engine settings: `max_tokens` 2,048 (the engine's built-in default) and a 30 s LLM timeout. Production profiles default to 16,384 tokens and 60 s; this difference matters, see [harness limits](#harness-limits-found-in-this-run).
- Grading: [`grade.py`](../scripts/llm-bench/grade.py) checks every non-fallback body against its schema again, keyword by keyword (format, enum, constraints), with no model involved.

### Server suites: `mockingbird serve`

[`server_suites.py`](../scripts/llm-bench/server_suites.py) starts `mockingbird serve` with a fresh SQLite store and an LLM profile pointing at the proxy, using the profile defaults (16,384 tokens, 60 s):

- Consistency: per rep, a fresh session; Stripe create then read of a PaymentIntent, TMDB popular list then detail of its first id, Square create then read of a payment. 18 rows per model, with field-level agreement between the write and the read.
- SSE: `GET /ticks` on `fixtures/openapi/events-3.1.yaml`, time to first event and each event checked against its schema. 3 rows per model.
- Smart import through the Admin API: 4 inputs (`petstore.md`, `tmdb-readme.html`, `shipping-api.pdf`, `travel-api-notes.md`), with recall and precision of the drafted `METHOD /path` operations against expected lists.

### Proxy, latency and cost

[`proxy.py`](../scripts/llm-bench/proxy.py) sits between every client and OpenRouter. It injects the real key (children only get a placeholder), writes one ledger row per call to `calls.jsonl` and enforces a hard budget ($9.00; it never tripped).

- Non-streaming calls are upgraded to streaming upstream, so the proxy can record time to first content token (TTFT) and time to first reasoning token (TTFR). Both are measured from when the upstream connection was up. End-to-end time (`e2e`) is measured from when the client's request arrived. These are LLM call times, not Mockingbird server wall time.
- Cost is OpenRouter's own `usage.cost` from the final stream chunk; no engine call had to fall back to an estimate.
- If the client does not send `reasoning`, the proxy adds `reasoning: {"enabled": false}`, which is what Mockingbird sends to `openrouter.ai` in production. A model that rejects it is retried once without it, and that is remembered for the model.

### Blinded judging

A fixed, outcome-blind set of 7 targets is judged for every model, one per API type plus one travel and one payments operation, picked by a seeded hash (`mb-bench-2026-10-29`), rep 0:

| Stratum | Contract | Target |
| --- | --- | --- |
| rest | `rest/travel-booking` | `createBooking` |
| graphql | `graphql/ecommerce-shopify-admin` | `Query.shop` |
| grpc | `grpc/otlp-trace-service` | `TraceService.Export` |
| soap | `soap/accounts-rpc` | `GetAccount` |
| asyncapi | `asyncapi/orders-amqp-2.6` | `orderCreated` |
| travel | `rest/travel-booking` | `getBooking` |
| payments | `rest/payments-stripe` | `PostRefunds` |

One batch is one target answered by every model, under opaque ids in a seeded order, with model, provider, source, latency and cost stripped. The judge was Claude Opus 5.5 (`claude-opus-5-5`), one judge per batch. It scored realism and coherence from 1 to 5 against the anchors in the rubric. A fallback body is faker output, so it is not shown and is scored 1/1 automatically. A schema-valid `{}` written by the model (the correct OTLP full-success reply) is left out of J rather than scored. Of 112 planned items, 110 count toward J. Order check: a fresh judge re-scored 2 batches in reverse order. The mean absolute difference was 0.017 over 30 pairs, and 29 scores were identical.

### Scores

All rates are over the 162 engine samples. FT, SV and K are macro averages over the five API types, so each type weighs the same.

```
FT     = n(llm) / N                       first try: valid without repair
SV     = n(llm | repaired) / N            schema-valid after patch or repair
K      = non-fallback samples with 0 grade.py violations / non-fallback samples
J      = mean((realism + coherence) / 2) over judged items, auto items = 1
Jn     = (J - 1) / 4
S      = 0.5 * FT + 0.5 * SV
Q      = 0.35 * S + 0.25 * K + 0.40 * Jn
C      = 1000 * engine-workload cost_usd / N         USD per 1,000 mock responses
L      = p50(e2e ms) over successful engine calls
Value  = 100 * (Q / Q_b)^2 / ((C / C_b)^0.5 * (L / L_b)^0.5)
```

`b` is the baseline `deepseek/deepseek-v4.1-flash` (Q_b 0.890, C_b $0.432, L_b 2,204 ms). C includes failed and repair calls, because they are the real price of producing a response. With these exponents, doubling cost is offset by a 19% gain in quality, and so is a doubling of latency.

Gates set Value to null: SV below 0.90 (`schema_valid`), ledger error rate plus timeout rate above 0.10 (`unreliable`), or fewer than 5 judge items (`unjudged`). A model is "recommendable" when it is ungated and its Q is within 10% of the best Q.

## Roster

Prices are OpenRouter's per-million-token list prices on the run day (`roster.json`). Every request asked for JSON mode (`response_format: {"type": "json_object"}`), Mockingbird's default (`json_schema: false`). "Providers" are the upstreams OpenRouter's default routing actually served, with the largest shares.

| Model id | Input $/M | Output $/M | Catalog lists `response_format` | Reasoning off | Providers served |
| --- | ---: | ---: | --- | --- | --- |
| `nvidia/nemotron-3.5-lightning` | 0.070 | 0.200 | yes | accepted | 4: DeepInfra 49%, CoreWeave 33%, Io Net 17%, ... |
| `deepseek/deepseek-v4.1-flash` | 0.045 | 0.500 | yes | accepted | 14: Wafer 40%, CoreWeave 14%, Decart 14%, ... |
| `inception/mercury-3` | 0.100 | 0.350 | yes | accepted | 1: Inception |
| `anthropic/claude-haiku-5.5` | 0.100 | 0.500 | yes | accepted | 1: Claude Platform on AWS |
| `openai/gpt-6-luna` | 0.100 | 0.500 | yes | accepted | 1: OpenAI |
| `qwen/qwen3.8-flash` | 0.150 | 0.470 | yes | accepted | 1: Alibaba |
| `nvidia/nemotron-3.5-super-vl` | 0.150 | 0.500 | yes | accepted | 7: Darkbloom 25%, Crusoe 24%, Together 14%, ... |
| `z-ai/glm-5.3-flash` | 0.150 | 0.500 | yes | rejected | 8: Wafer 43%, OpenInference 42%, InferenceNet 5%, ... |
| `deepseek/deepseek-v4.1-flash-1017` | 0.150 | 0.600 | yes | accepted | 23: Wafer 17%, Cohere 15%, InferenceNet 13%, ... |
| `minimax/minimax-m3.1-flash` | 0.150 | 0.600 | yes | rejected | 7: Parasail 37%, DigitalOcean 32%, Together 19%, ... |
| `reflection/beam-1.0-501b-a23b` | 0.250 | 1.000 | yes | rejected | 5: Nebius 37%, CoreWeave 25%, Together 18%, ... |
| `google/gemini-3.8-flash` | 0.825 | 4.125 | yes | rejected | 2: Google 98%, Google AI Studio 2% |
| `thinkingmachines/inkling-small` | 0.450 | 1.200 | no | accepted | 1: DeepInfra |
| `thinkingmachines/inkling-small@nojson` | 0.450 | 1.200 | no | accepted | 1: DeepInfra |
| `thinkingmachines/inkling` | 1.000 | 4.050 | no | accepted | 1: Together |
| `thinkingmachines/inkling@nojson` | 1.000 | 4.050 | no | accepted | 1: Together |

How each model was driven:

- JSON mode was sent to every model. No upstream rejected it, including the two Inkling models, whose catalog entries list no `response_format` support.
- The four models that rejected the reasoning-off switch (GLM, MiniMax, Beam, Gemini) were retried without it on their first call and ran with reasoning on for the rest of the run. They averaged 403, 107, 1,348 and 930 reasoning tokens per engine call.
- `<id>@nojson` is a bench-only variant: the proxy strips `response_format` before forwarding to `<id>`, so the model answers without JSON mode and the engine's own parsing and repair take over. It exists because the Inkling models' catalog entries do not list JSON mode, and `thinkingmachines/inkling` breaks with it (below). Mockingbird itself cannot run a model this way, so the `@nojson` rows show what such a setting would give. They are not a configuration you can choose.

Run order and load: the first invocation ran 10 models from 18:07 to 18:30 UTC, 3 at a time, cheapest first. The second ran Gemini and Beam together from 19:34, then the four Inkling configurations one at a time until 20:09.

## Workloads

From [`workloads.json`](benchmarks/workloads.json). n is engine samples per model (operations x 3 reps).

| API type | Contracts | Operations | n |
| --- | --- | ---: | ---: |
| REST | Stripe, PayPal Orders, Square, `orders-3.1`, TMDB, Spotify, Discord, GitHub REST, travel booking, travel hotels | 34 | 102 |
| GraphQL | Shopify Admin (`Query.shop`, `Query.order`, `Query.products`, `Mutation.productCreate`), GitHub (`Query.viewer`, `Query.repository`, `Query.user`, `Mutation.createIssue`) | 8 | 24 |
| gRPC | `route_guide.proto` `GetFeature`, OTLP `TraceService.Export` | 2 | 6 |
| SOAP | `calculator.wsdl` (Add, Subtract, Divide), `accounts-rpc.wsdl` (GetAccount, CreateAccount) | 5 | 15 |
| AsyncAPI | commerce events on Kafka (3 messages), orders on AMQP 2.6 (2 messages) | 5 | 15 |

| Domain | n |
| --- | ---: |
| ecommerce | 48 |
| social-media | 30 |
| devtools | 27 |
| payments | 21 |
| generic | 18 |
| travel | 18 |

The travel contract is [`docs/benchmarks/contracts/travel.openapi.yaml`](benchmarks/contracts/travel.openapi.yaml); the import inputs are in [`docs/benchmarks/import-inputs`](https://github.com/RJ-s-Org/Mockingbird/tree/main/docs/benchmarks/import-inputs).

## Results

The full tables, including per-domain rates, keyword conformance and per-case consistency, are in [`tables.md`](benchmarks/2026-10-29/tables.md).

### Leaderboard

Sorted by Value; gated models follow, sorted by Q. "Value ungated" applies the formula without the gates. It is a diagnostic, not the rubric's Value. Err+TO is the ledger error rate plus the larger of the ledger and engine timeout rates.

| # | Model | Value | Gate | Q | Q rank | SV | J (1-5) | C $/1k | L p50 ms | Err+TO | Value ungated |
| ---: | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | `openai/gpt-6-luna` | 125.1 | | 0.882 | 5 | 0.949 | 4.17 | 0.238 | 2,469 | 0.0% | 125.1 |
| 2 | `deepseek/deepseek-v4.1-flash` | 100.0 | | 0.890 | 3 | 0.929 | 4.36 | 0.432 | 2,204 | 0.0% | 100.0 |
| 3 | `deepseek/deepseek-v4.1-flash-1017` | 87.4 | | 0.901 | 1 | 0.936 | 4.50 | 0.495 | 2,649 | 0.0% | 87.4 |
| 4 | `anthropic/claude-haiku-5.5` | 84.1 | | 0.899 | 2 | 0.930 | 4.36 | 0.560 | 2,502 | 0.0% | 84.1 |
| 5 | `thinkingmachines/inkling@nojson` | 31.9 | | 0.828 | 9 | 0.909 | 4.07 | 2.922 | 2,403 | 0.0% | 31.9 |
| — | `minimax/minimax-m3.1-flash` | — | SV < 0.90 | 0.886 | 4 | 0.889 | 4.67 | 0.543 | 1,681 | 0.6% | 101.3 |
| — | `google/gemini-3.8-flash` | — | SV < 0.90 | 0.850 | 6 | 0.826 | 4.14 | 6.188 | 9,588 | 0.6% | 11.6 |
| — | `z-ai/glm-5.3-flash` | — | SV < 0.90 | 0.843 | 7 | 0.853 | 4.21 | 0.291 | 7,044 | 4.9% | 61.1 |
| — | `qwen/qwen3.8-flash` | — | SV < 0.90 | 0.842 | 8 | 0.898 | 4.21 | 0.378 | 7,270 | 2.4% | 52.7 |
| — | `thinkingmachines/inkling-small@nojson` | — | SV < 0.90 | 0.825 | 10 | 0.846 | 4.21 | 1.175 | 1,568 | 0.0% | 61.8 |
| — | `inception/mercury-3` | — | SV < 0.90 | 0.819 | 11 | 0.837 | 4.29 | 0.332 | 1,096 | 0.0% | 137.1 |
| — | `nvidia/nemotron-3.5-super-vl` | — | SV < 0.90 | 0.810 | 12 | 0.872 | 3.93 | 0.539 | 2,882 | 0.0% | 64.9 |
| — | `thinkingmachines/inkling-small` | — | SV < 0.90 | 0.778 | 13 | 0.803 | 3.93 | 1.320 | 2,458 | 0.0% | 41.4 |
| — | `reflection/beam-1.0-501b-a23b` | — | SV < 0.90 | 0.739 | 14 | 0.722 | 3.43 | 1.963 | 4,883 | 0.5% | 21.7 |
| — | `nvidia/nemotron-3.5-lightning` | — | SV < 0.90 | 0.719 | 15 | 0.746 | 3.50 | 0.263 | 1,728 | 0.0% | 94.5 |
| — | `thinkingmachines/inkling` | — | SV < 0.90 | — | — | 0.000 | 1.00 | 6.459 | 3,917 | 0.0% | — |

`thinkingmachines/inkling@nojson` passes every gate and is marked recommendable in `results.json`, but it is not a configuration Mockingbird can run (see [roster](#roster)).

Value sensitivity, for the ungated models (rank in parentheses):

| Model | (2, 0.5, 0.5) default | (1, 1, 1) | (2, 0.25, 1) |
| --- | ---: | ---: | ---: |
| `openai/gpt-6-luna` | 125.1 (1) | 160.7 (1) | 101.8 (1) |
| `deepseek/deepseek-v4.1-flash` | 100.0 (2) | 100.0 (2) | 100.0 (2) |
| `deepseek/deepseek-v4.1-flash-1017` | 87.4 (3) | 73.6 (3) | 82.4 (4) |
| `anthropic/claude-haiku-5.5` | 84.1 (4) | 68.7 (4) | 84.2 (3) |
| `thinkingmachines/inkling@nojson` | 31.9 (5) | 12.6 (5) | 49.2 (5) |

With a gate of 0.85 instead of 0.90, the default-exponent order would be: luna 125.1, minimax 101.3, deepseek 100.0, 1017 87.4, haiku 84.1, nemotron-super 64.9, GLM 61.1, qwen 52.7, inkling@nojson 31.9. At 0.80, `inception/mercury-3` (137.1) moves to the top, and inkling-small@nojson (61.8), inkling-small (41.4) and Gemini (11.6) join.

### Quality components

FT, SV and K are macro over API types. Fallback is the share of all 162 samples. The last two columns count fallback samples caused by output truncation at `max_tokens` and by the engine's timeout.

| Model | FT | SV | Fallback | S | K | J | Realism | Coherence | judge n | FB: max_tokens | FB: timeout | Q |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `openai/gpt-6-luna` | 0.863 | 0.949 | 6.2% | 0.906 | 0.994 | 4.17 | 4.17 | 4.17 | 6 | 0 | 0 | 0.882 |
| `deepseek/deepseek-v4.1-flash` | 0.819 | 0.929 | 6.2% | 0.874 | 0.994 | 4.36 | 3.86 | 4.86 | 7 | 4 | 0 | 0.890 |
| `deepseek/deepseek-v4.1-flash-1017` | 0.794 | 0.936 | 6.2% | 0.865 | 0.994 | 4.50 | 4.14 | 4.86 | 7 | 3 | 0 | 0.901 |
| `anthropic/claude-haiku-5.5` | 0.869 | 0.930 | 8.0% | 0.900 | 0.994 | 4.36 | 4.00 | 4.71 | 7 | 3 | 0 | 0.899 |
| `thinkingmachines/inkling@nojson` | 0.655 | 0.909 | 8.6% | 0.782 | 0.990 | 4.07 | 3.57 | 4.57 | 7 | 1 | 0 | 0.828 |
| `minimax/minimax-m3.1-flash` | 0.662 | 0.889 | 13.0% | 0.775 | 0.993 | 4.67 | 4.33 | 5.00 | 6 | 18 | 1 | 0.886 |
| `google/gemini-3.8-flash` | 0.822 | 0.826 | 32.7% | 0.824 | 0.990 | 4.14 | 3.86 | 4.43 | 7 | 52 | 1 | 0.850 |
| `z-ai/glm-5.3-flash` | 0.716 | 0.853 | 16.7% | 0.784 | 0.987 | 4.21 | 4.00 | 4.43 | 7 | 12 | 8 | 0.843 |
| `qwen/qwen3.8-flash` | 0.691 | 0.898 | 8.0% | 0.795 | 0.971 | 4.21 | 3.71 | 4.71 | 7 | 8 | 3 | 0.842 |
| `thinkingmachines/inkling-small@nojson` | 0.622 | 0.846 | 16.7% | 0.734 | 0.987 | 4.21 | 3.86 | 4.57 | 7 | 7 | 0 | 0.825 |
| `inception/mercury-3` | 0.589 | 0.837 | 14.2% | 0.713 | 0.965 | 4.29 | 4.14 | 4.43 | 7 | 15 | 0 | 0.819 |
| `nvidia/nemotron-3.5-super-vl` | 0.671 | 0.872 | 14.2% | 0.771 | 0.989 | 3.93 | 3.57 | 4.29 | 7 | 13 | 0 | 0.810 |
| `thinkingmachines/inkling-small` | 0.556 | 0.803 | 16.7% | 0.680 | 0.988 | 3.93 | 3.57 | 4.29 | 7 | 16 | 0 | 0.778 |
| `reflection/beam-1.0-501b-a23b` | 0.697 | 0.722 | 48.8% | 0.709 | 0.990 | 3.43 | 3.00 | 3.86 | 7 | 78 | 0 | 0.739 |
| `nvidia/nemotron-3.5-lightning` | 0.547 | 0.746 | 29.6% | 0.646 | 0.971 | 3.50 | 2.86 | 4.14 | 7 | 3 | 0 | 0.719 |
| `thinkingmachines/inkling` | 0.000 | 0.000 | 100.0% | 0.000 | — | 1.00 | 1.00 | 1.00 | 7 | 14 | 0 | — |

### Schema-valid rate by API type

| Model | REST (n=102) | GraphQL (n=24) | gRPC (n=6) | SOAP (n=15) | AsyncAPI (n=15) | Macro SV |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `openai/gpt-6-luna` | 0.951 | 0.792 | 1.000 | 1.000 | 1.000 | 0.949 |
| `deepseek/deepseek-v4.1-flash` | 0.980 | 0.667 | 1.000 | 1.000 | 1.000 | 0.929 |
| `deepseek/deepseek-v4.1-flash-1017` | 0.971 | 0.708 | 1.000 | 1.000 | 1.000 | 0.936 |
| `anthropic/claude-haiku-5.5` | 0.941 | 0.708 | 1.000 | 1.000 | 1.000 | 0.930 |
| `thinkingmachines/inkling@nojson` | 0.961 | 0.583 | 1.000 | 1.000 | 1.000 | 0.909 |
| `minimax/minimax-m3.1-flash` | 0.902 | 0.542 | 1.000 | 1.000 | 1.000 | 0.889 |
| `google/gemini-3.8-flash` | 0.588 | 0.542 | 1.000 | 1.000 | 1.000 | 0.826 |
| `z-ai/glm-5.3-flash` | 0.873 | 0.458 | 1.000 | 1.000 | 0.933 | 0.853 |
| `qwen/qwen3.8-flash` | 0.990 | 0.500 | 1.000 | 1.000 | 1.000 | 0.898 |
| `thinkingmachines/inkling-small@nojson` | 0.882 | 0.417 | 1.000 | 1.000 | 0.933 | 0.846 |
| `inception/mercury-3` | 0.941 | 0.375 | 1.000 | 0.867 | 1.000 | 0.837 |
| `nvidia/nemotron-3.5-super-vl` | 0.902 | 0.458 | 1.000 | 1.000 | 1.000 | 0.872 |
| `thinkingmachines/inkling-small` | 0.941 | 0.208 | 1.000 | 1.000 | 0.867 | 0.803 |
| `reflection/beam-1.0-501b-a23b` | 0.392 | 0.417 | 1.000 | 0.800 | 1.000 | 0.722 |
| `nvidia/nemotron-3.5-lightning` | 0.745 | 0.250 | 1.000 | 1.000 | 0.733 | 0.746 |
| `thinkingmachines/inkling` | 0.000 | 0.000 | 0.000 | 0.000 | 0.000 | 0.000 |

GraphQL is the weakest type for every model, and gRPC and SOAP are near 1.0 for all working models. Conformance K on non-fallback bodies is 0.965 to 0.994 for every working model. Among the keyword checks, enum and constraint keywords pass 100% for every model, and `format` passes 0.973 to 0.993.

### Latency

Proxy-measured LLM call times over successful engine calls, in ms. n counts calls, including repair calls, so it can exceed 162. tok/s is the median per-call output rate, with reasoning tokens counted.

| Model | n | TTFT p50 | TTFT p95 | TTFR p50 | e2e p50 | e2e p95 | tok/s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `openai/gpt-6-luna` | 175 | 906 | 1,496 | — | 2,469 | 7,137 | 125 |
| `deepseek/deepseek-v4.1-flash` | 172 | 483 | 1,046 | — | 2,204 | 8,229 | 224 |
| `deepseek/deepseek-v4.1-flash-1017` | 169 | 535 | 1,733 | — | 2,649 | 9,752 | 190 |
| `anthropic/claude-haiku-5.5` | 181 | 531 | 737 | — | 2,502 | 6,473 | 240 |
| `thinkingmachines/inkling@nojson` | 184 | 291 | 669 | — | 2,403 | 9,961 | 108 |
| `minimax/minimax-m3.1-flash` | 188 | 531 | 5,736 | 929 | 1,681 | 11,700 | 279 |
| `google/gemini-3.8-flash` | 166 | 5,952 | 14,852 | 2,091 | 9,588 | 15,848 | 211 |
| `z-ai/glm-5.3-flash` | 191 | 2,964 | 15,255 | 428 | 7,044 | 27,446 | 124 |
| `qwen/qwen3.8-flash` | 163 | 1,057 | 6,301 | — | 7,270 | 25,199 | 80 |
| `thinkingmachines/inkling-small@nojson` | 193 | 338 | 741 | — | 1,568 | 4,482 | 269 |
| `inception/mercury-3` | 177 | 509 | 841 | — | 1,096 | 3,336 | 773 |
| `nvidia/nemotron-3.5-super-vl` | 182 | 835 | 11,874 | — | 2,882 | 14,375 | 510 |
| `thinkingmachines/inkling-small` | 207 | 340 | 775 | — | 2,458 | 7,510 | 240 |
| `reflection/beam-1.0-501b-a23b` | 162 | 3,854 | 8,625 | 511 | 4,883 | 11,543 | 416 |
| `nvidia/nemotron-3.5-lightning` | 227 | 294 | 695 | — | 1,728 | 4,796 | 269 |
| `thinkingmachines/inkling` | 312 | 281 | 573 | — | 3,917 | 17,775 | 100 |

### Reliability

n is ledger rows across all workloads. Engine timeouts are counted from the samples, because the engine gives up at 30 s while the upstream call still finishes with status 200, so the ledger never sees them.

| Model | n | Errors | Engine timeouts (of 162) | Retry rate | Reasoning off rejected | Providers |
| --- | ---: | ---: | ---: | ---: | --- | ---: |
| `openai/gpt-6-luna` | 196 | 0 | 0 | 0.0% | no | 1 |
| `deepseek/deepseek-v4.1-flash` | 193 | 0 | 0 | 0.0% | no | 14 |
| `deepseek/deepseek-v4.1-flash-1017` | 190 | 0 | 0 | 0.0% | no | 23 |
| `anthropic/claude-haiku-5.5` | 202 | 0 | 0 | 0.0% | no | 1 |
| `thinkingmachines/inkling@nojson` | 187 | 0 | 0 | 0.0% | no | 1 |
| `minimax/minimax-m3.1-flash` | 210 | 0 | 1 | 1.9% | yes | 7 |
| `google/gemini-3.8-flash` | 187 | 0 | 1 | 2.1% | yes | 2 |
| `z-ai/glm-5.3-flash` | 213 | 0 | 8 | 1.9% | yes | 8 |
| `qwen/qwen3.8-flash` | 185 | 1 | 3 | 0.0% | no | 1 |
| `thinkingmachines/inkling-small@nojson` | 196 | 0 | 0 | 0.0% | no | 1 |
| `inception/mercury-3` | 201 | 0 | 0 | 0.0% | no | 1 |
| `nvidia/nemotron-3.5-super-vl` | 204 | 0 | 0 | 0.0% | no | 7 |
| `thinkingmachines/inkling-small` | 231 | 0 | 0 | 0.0% | no | 1 |
| `reflection/beam-1.0-501b-a23b` | 184 | 1 | 0 | 2.2% | yes | 5 |
| `nvidia/nemotron-3.5-lightning` | 248 | 0 | 0 | 0.0% | no | 4 |
| `thinkingmachines/inkling` | 348 | 0 | 0 | 0.0% | no | 1 |

There were no refusals. Retries (calls with more than one upstream attempt) occur only for the four models that rejected the reasoning-off switch.

### Cost

C is engine cost per 1,000 mock responses: total engine spend, including repair and failed calls, divided by the 162 samples. Token counts are per engine call.

| Model | Engine calls | Engine $ | All workloads $ | C $/1k responses | Prompt tok/call | Completion tok/call | Reasoning tok/call |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `openai/gpt-6-luna` | 175 | 0.0385 | 0.0477 | 0.238 | 1,233 | 277 | 0 |
| `deepseek/deepseek-v4.1-flash` | 172 | 0.0700 | 0.0819 | 0.432 | 1,400 | 515 | 0 |
| `deepseek/deepseek-v4.1-flash-1017` | 169 | 0.0801 | 0.0940 | 0.495 | 1,347 | 491 | 0 |
| `anthropic/claude-haiku-5.5` | 181 | 0.0907 | 0.1038 | 0.560 | 2,157 | 571 | 0 |
| `thinkingmachines/inkling@nojson` | 184 | 0.4734 | 0.4951 | 2.922 | 1,370 | 350 | 0 |
| `minimax/minimax-m3.1-flash` | 188 | 0.0879 | 0.1009 | 0.543 | 1,430 | 538 | 107 |
| `google/gemini-3.8-flash` | 166 | 1.0025 | 1.1581 | 6.188 | 1,325 | 1,345 | 930 |
| `z-ai/glm-5.3-flash` | 191 | 0.0471 | 0.0540 | 0.291 | 1,379 | 964 | 403 |
| `qwen/qwen3.8-flash` | 164 | 0.0613 | 0.0743 | 0.378 | 1,218 | 602 | 0 |
| `thinkingmachines/inkling-small@nojson` | 193 | 0.1903 | 0.1981 | 1.175 | 1,392 | 483 | 0 |
| `inception/mercury-3` | 177 | 0.0537 | 0.0789 | 0.332 | 1,457 | 592 | 0 |
| `nvidia/nemotron-3.5-super-vl` | 182 | 0.0873 | 0.1080 | 0.539 | 1,449 | 590 | 0 |
| `thinkingmachines/inkling-small` | 207 | 0.2138 | 0.2694 | 1.320 | 1,420 | 641 | 0 |
| `reflection/beam-1.0-501b-a23b` | 163 | 0.3181 | 0.3873 | 1.963 | 1,214 | 1,493 | 1,348 |
| `nvidia/nemotron-3.5-lightning` | 227 | 0.0425 | 0.0476 | 0.263 | 1,730 | 513 | 0 |
| `thinkingmachines/inkling` | 312 | 1.0464 | 1.2834 | 6.459 | 1,405 | 577 | 0 |

### Server suites

Consistency is 18 rows per model, SSE 3 and import 4 inputs. The two `@nojson` rows have no consistency or SSE results. The bench put the `@` of the variant id into the `x-mockingbird-session` header, Mockingbird rejected it with 400 before any LLM call, and those 21 rows per variant are excluded rather than counted as failures. Import does not use the session header, so the `@nojson` import results stand.

| Model | Consistency ok (n) | Field agreement | SSE ok (n) | SSE first event p50 ms | Import drafts valid (n) | Import op recall (macro) | Import mean ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `openai/gpt-6-luna` | 88.9% (18) | 82.7% | 100.0% (3) | 1,183 | 100.0% (4) | 1.000 | 8,349 |
| `deepseek/deepseek-v4.1-flash` | 100.0% (18) | 92.6% | 100.0% (3) | 520 | 100.0% (4) | 1.000 | 5,621 |
| `deepseek/deepseek-v4.1-flash-1017` | 100.0% (18) | 91.4% | 100.0% (3) | 1,077 | 100.0% (4) | 1.000 | 7,925 |
| `anthropic/claude-haiku-5.5` | 100.0% (18) | 92.6% | 100.0% (3) | 721 | 100.0% (4) | 1.000 | 5,476 |
| `thinkingmachines/inkling@nojson` | — (0) | — | — (0) | — | 25.0% (4) | 0.250 | 9,213 |
| `minimax/minimax-m3.1-flash` | 88.9% (18) | 85.2% | 100.0% (3) | 805 | 75.0% (4) | 0.750 | 4,174 |
| `google/gemini-3.8-flash` | 100.0% (18) | 92.6% | 100.0% (3) | 4,066 | 100.0% (4) | 1.000 | 10,282 |
| `z-ai/glm-5.3-flash` | 94.4% (18) | 86.4% | 100.0% (3) | 1,161 | 75.0% (4) | 0.750 | 28,115 |
| `qwen/qwen3.8-flash` | 100.0% (18) | 91.4% | 100.0% (3) | 2,310 | 100.0% (4) | 1.000 | 29,852 |
| `thinkingmachines/inkling-small@nojson` | — (0) | — | — (0) | — | 75.0% (4) | 0.750 | 2,731 |
| `inception/mercury-3` | 83.3% (18) | 74.1% | 100.0% (3) | 429 | 50.0% (4) | 0.500 | 1,922 |
| `nvidia/nemotron-3.5-super-vl` | 83.3% (18) | 82.7% | 100.0% (3) | 452 | 100.0% (4) | 1.000 | 10,879 |
| `thinkingmachines/inkling-small` | 77.8% (18) | 72.6% | 100.0% (3) | 376 | 100.0% (4) | 0.800 | 5,665 |
| `reflection/beam-1.0-501b-a23b` | 100.0% (18) | 88.9% | 100.0% (3) | 2,966 | 100.0% (4) | 1.000 | 17,026 |
| `nvidia/nemotron-3.5-lightning` | 83.3% (18) | 81.5% | 100.0% (3) | 474 | 100.0% (4) | 1.000 | 9,281 |
| `thinkingmachines/inkling` | 50.0% (18) | 49.4% | 100.0% (3) | 511 | 25.0% (4) | 0.250 | 27,130 |

Most consistency misses are the TMDB detail read (popular list, then the detail of its first id): `inception/mercury-3` 0 of 3, `openai/gpt-6-luna` and `minimax/minimax-m3.1-flash` 1 of 3.

## Spend

From the proxy ledgers (OpenRouter `usage.cost`) and the key's usage before and after the run:

| Scope | Calls | USD |
| --- | ---: | ---: |
| Pilot (10 engine calls per model for 7 configurations) | 88 | 0.1474 |
| Full run: engine | | 3.9038 |
| Full run: consistency | | 0.4724 |
| Full run: SSE | | 0.0170 |
| Full run: import | | 0.1894 |
| Full run, total | 3,375 | 4.5826 |
| Superseded attempts (Gemini, Beam, inkling-small@nojson; redone, not scored) | 421 | 0.6578 |
| All ledgers | 3,884 | 5.3878 |
| Key usage delta (`key-after.json` minus `key-before.json`) | | 5.3536 |

The ledgers come to $0.034 (0.6%) more than the key delta. Gemini and Inkling with JSON mode account for $2.44, 53% of the full run. The judge did not go through OpenRouter and is not included.

## Harness limits found in this run

A review of the raw data found that most of the schema-valid gating measures limits of the bench rather than model output:

- Output cap. mb-evals runs the engine with its built-in `max_tokens` of 2,048 (`crates/mb-engine/src/lib.rs`), while production profiles default to 16,384. Models that kept reasoning on spend most of that budget before writing the answer. For Gemini, 52 of 53 fallbacks are truncation, and 53 of 166 calls ended at `length` with about 1,600 reasoning tokens on average. For Beam, 78 of 79 fallbacks are truncation, and 78 of 163 calls ended at `length` with about 1,940 of their 2,048 tokens spent on reasoning. The same reasoning inflates their cost and latency.
- GraphQL schemas. The engine request carries variables but no selection set, so the response schema is the full type graph (12.8 KB, with cycles and deep required fields). Every model falls back on GitHub `Query.repository` (48 of 48 samples), and Shopify `Query.products` fails 45 of 48. Dropping those two targets lets 8 models pass 0.90 instead of 5. Also leaving out truncation and timeout fallbacks, 15 pass, with Gemini and Beam at 1.000.
- Engine timeout. The engine gives up at 30 s, but the upstream call still finishes with status 200, so ledger-only reliability showed 0% for GLM. It had 8 engine timeouts in 162 samples (4.9%). Err+TO now counts both.
- Macro weighting. gRPC (6 samples, 2 targets, every working model at 1.0) weighs as much as REST (102 samples). K is near saturation (0.965 to 0.994) and barely separates models.
- A fallback is penalised three times: in first-try rate, in SV, and as a 1/1 judge score when it lands on a judged item.
- Two scoring fixes were applied before publication. A schema-valid `{}` on `TraceService.Export` had been auto-scored 1/1 for luna and minimax (it is now excluded from J: luna Q 0.837 to 0.882, minimax 0.834 to 0.886). The `@nojson` server rows rejected by the session-header bug had been counted as failures. The underlying bugs in `judge_prep.py` and `server_suites.py` are not fixed yet, and the two excluded judge items should be re-judged on the next run.

## Caveats

- Sample sizes are small. N = 162 engine samples per model, 6 or 7 judge items per model (rep 0 only, two of the seven from the same travel contract), 18 consistency rows, 3 SSE rows and 4 import inputs. One judge item moving one point shifts Q by about 0.014. A bootstrap over samples and judge items (cost and latency held fixed) gives overlapping 90% Q-rank intervals, rank 1 to 5 or 6, for deepseek, deepseek-1017, haiku, luna and minimax.
- One day, one route. Everything ran on 2026-10-29 with OpenRouter's default provider routing. Routing varies a lot: within `deepseek/deepseek-v4.1-flash`'s 14 providers, p50 latency ran from 1.2 s (DigitalOcean) to 4.3 s (Morph) and cost per call varied 4x. `deepseek/deepseek-v4.1-flash-1017` was served by 23 providers. `minimax/minimax-m3.1-flash` on MiniMax's own endpoint took 6.1 s with 7 of 18 calls truncated, against 0.86 s on Parasail.
- Parallel load. Three models ran at a time with 4 calls in flight each, in two invocations about 1.5 hours apart (Gemini, Beam and the Inkling models ran later), so latency across invocations is not controlled.
- One judge. The judge is Claude Opus 5.5, and the roster includes Anthropic's Claude Haiku 5.5, so same-vendor bias is possible. The order check measures one judge's self-consistency, not agreement between judges. Coherence sits near the ceiling for most models.
- Requests differ per model. The faker builds each request, so two models answering the same operation see different ids and parameters.
- The `@nojson` rows are a bench variant only (see [roster](#roster)).

## Surprises

- `thinkingmachines/inkling` with JSON mode produced no usable response: 0% schema-valid, all 162 samples on the faker fallback, at $6.46 per 1,000 responses. 309 of its 312 engine responses contain `}{`: two JSON objects back to back, which no JSON parser accepts. Without JSON mode the same model reached 0.909 SV, the only Inkling configuration to pass the gate.
- `inception/mercury-3` is the fastest model (L 1,096 ms, 773 tok/s) and has the highest ungated Value (137.1), first under all five exponent sets tried. It is gated at SV 0.837, and weak elsewhere: GraphQL SV 0.375, import 2 of 4 valid, TMDB detail consistency 0 of 3, first-try rate 0.589.
- `openai/gpt-6-luna` writes about half the completion tokens of the default (277 against 515 per call), which is most of its cost advantage at the same list price per token.
- `minimax/minimax-m3.1-flash` had the best judge score (J 4.67, coherence 5.00 on 6 items) but misses the gate at SV 0.889.
- `google/gemini-3.8-flash` cost 14x the default per response. It rejected the reasoning-off switch, and reasoning made up 930 of its 1,345 completion tokens per call.

![Side-by-side replay of cached samples for Stripe GetPaymentIntentsIntent: Inkling with JSON mode falls back to the faker on all three reps because its output is not valid JSON, while the same model without JSON mode, DeepSeek V4.1 Flash and Claude Haiku 5.5 return valid bodies](benchmarks/media/06-compare-payments-json-vs-nojson.gif)

## Recommendations

Keep `deepseek/deepseek-v4.1-flash` as the default. `openai/gpt-6-luna` is 45% cheaper per response, has the best SV (0.949) and first-try rate (0.863), and came out ahead on Value in every bootstrap resample. But it shows no quality gain (Q 0.882 against 0.890, J 4.17 against 4.36), weaker consistency (88.9% against 100%), and a slower first SSE event (1,183 against 520 ms). The saving is about $0.19 per 1,000 responses. Re-run after the harness fixes below and revisit.

By use case:

| Use case | Suggestion | Evidence | Alternative |
| --- | --- | --- | --- |
| Fast and cheap | `openai/gpt-6-luna` | $0.238/1k, SV 0.949, one provider | `inception/mercury-3` for raw speed (L 1,096 ms, $0.33/1k), only where GraphQL and smart import do not matter |
| Highest quality | `deepseek/deepseek-v4.1-flash-1017` or `anthropic/claude-haiku-5.5` | Q 0.901 / 0.899, consistency 100%, import 4 of 4 | $0.49 / $0.56 per 1k; not distinguishable from the default within the noise |
| Smart import | `deepseek/deepseek-v4.1-flash` (default) or `anthropic/claude-haiku-5.5` | Both 4 of 4 drafts valid, recall 1.0, about 5.5 s mean | Gemini and Beam also pass at about 10x the cost; mercury, GLM, minimax and every Inkling configuration produced invalid or truncated drafts (n = 4 inputs) |

To switch, set `llm.profiles.<name>.model` ([configuration](configuration.md)). Keep the production `max_tokens` default (16,384) for models that reject the reasoning-off switch: the 2,048 this bench used is what cut them off.

Before the next run:

- mb-evals: run the engine with the production `max_tokens` (16,384) and timeout, so reasoning models are not cut off.
- GraphQL: build response schemas from a selection set, or drop `Query.repository` and `Query.products`.
- `server_suites.py`: replace `@` in the session id slug.
- `judge_prep.py`: stop auto-scoring a schema-valid `{}`, and re-judge the two excluded items.

## Known gap: JSON mode cannot be turned off

In this run, `thinkingmachines/inkling` only worked without JSON mode, and Mockingbird had no switch for that: `llm.profiles.<name>.json_schema` chose between `json_schema` structured output and JSON mode (`json_object`), and `extra_body` rejects the `response_format` key. The `@nojson` rows approximate a profile that sends no `response_format`. Since #142, `output_mode: none` is that setting.

## Raw data

Everything is under [`docs/benchmarks/2026-10-29/`](https://github.com/RJ-s-Org/Mockingbird/tree/main/docs/benchmarks/2026-10-29):

| File | Contents |
| --- | --- |
| [`results.json`](benchmarks/2026-10-29/results.json), [`results.csv`](benchmarks/2026-10-29/results.csv) | Every metric per model, the leaderboard, sensitivity, spend |
| [`tables.md`](benchmarks/2026-10-29/tables.md) | All tables, including per-domain rates and per-case consistency |
| [`roster.json`](benchmarks/2026-10-29/roster.json) | Model metadata and prices from OpenRouter |
| [`run.json`](benchmarks/2026-10-29/run.json) | Run order, per-model status, timings and spend |
| [`key-before.json`](benchmarks/2026-10-29/key-before.json), [`key-after.json`](benchmarks/2026-10-29/key-after.json) | Key usage before and after |
| `calls.jsonl.gz`, `exchanges.jsonl.gz` | Proxy ledger (one row per LLM call) and the request and response of every call |
| `samples.jsonl.gz`, `engine/<model>/` | Engine samples and per-model mb-evals reports |
| `grades.jsonl.gz`, `grade-summary.json` | Conformance grades |
| `server.jsonl`, `server/<model>/` | Server suite rows |
| `judge/`, `judge.jsonl`, `judge-key.json`, `judge-recheck.jsonl`, `judge-order-check.json` | Judge batches and instructions, scores, the blinding key and the order check |
| `pilot/`, `superseded/` | The pilot and the redone attempts (counted in spend, not scored) |

The large JSONL files are gzipped; `gunzip -k docs/benchmarks/2026-10-29/*.gz` restores the names the scripts read. Per-model duplicate samples and the proxy and server logs are not committed.

# Re-running

The run needs `OPENROUTER_API_KEY` in the environment. Only the proxy and the preflight metadata calls read it, and children get placeholders. See [`scripts/llm-bench/README.md`](../scripts/llm-bench/README.md) for the budget, stopping and testing without spend. Round 2 was run like this:

```sh
cargo build --release -p mb-evals -p mockingbird-cli
OUT=docs/benchmarks/2026-10-30
R1=docs/benchmarks/2026-10-29
MODELS=deepseek/deepseek-v4.1-flash,...,thinkingmachines/inkling@nojson   # the 12 above
# Earlier spend that counts toward --budget-usd
PRIOR="--prior-ledger $R1/calls.jsonl.gz --prior-ledger $R1/pilot/calls.jsonl.gz --prior-ledger $R1/superseded/calls.jsonl.gz"

# 1. Model metadata and prices -> roster.json; key usage -> key-before.json
python3 scripts/llm-bench/run.py preflight --out $OUT --models $MODELS

# 2. Pilot: about 10 engine calls per model, then a cost projection
python3 scripts/llm-bench/run.py pilot --out $OUT --models $MODELS --calls 10 --repeat 3 --budget-usd 9.5 $PRIOR

# 3. Full run: cheapest first, 3 models at a time, Inkling models last
python3 scripts/llm-bench/run.py full --out $OUT --models $MODELS --repeat 3 --parallel 3 --concurrency 4 --budget-usd 9.5 $PRIOR

# 4. Grade, judge and aggregate
python3 scripts/llm-bench/grade.py --samples $OUT/samples.jsonl --out $OUT/grades.jsonl --summary $OUT/grade-summary.json
python3 scripts/llm-bench/judge_prep.py build --out $OUT
#    (a judge scores judge/batch-XX.json into judge/scores/, and recheck/ in reverse order; see judge/README.md)
python3 scripts/llm-bench/judge_prep.py merge --out $OUT --scores $OUT/judge/scores/*.json \
    --recheck $OUT/judge/recheck/scores-*.json
python3 scripts/llm-bench/aggregate.py --out $OUT
```

`python3 scripts/llm-bench/live.py --out $OUT` shows the run live in a second terminal; it reads only the files the run writes. Compare models only within one run: provider routing and load change from day to day.
