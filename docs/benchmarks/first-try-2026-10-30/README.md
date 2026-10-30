# First-try experiments, 2026-10-30

What makes an LLM's first answer valid, and at what cost. Prompts were replayed from round 2 with [`scripts/llm-bench/first_try.py`](../../../scripts/llm-bench/first_try.py) and graded by the engine's own checks; 53 prompts per model across all 18 contracts. The final numbers come from the real engine through `mb-evals`. Spend was $1.39.

**Result:** schema output (`output_mode: json_schema`) is by far the best lever, and is now the default. On the default model first try is 99.0% and no answer fell back, in 486 calls. Reasoning is not worth its latency.

## Real engine, default settings

Round 2's `json_object` against the final code, per call, so REST carries its full weight. Luna and Lightning had 162 calls each (`engine-run3`); the default had 486 (`engine-run3` and `engine-run4`).

| Model | First try | REST first try | Overall (no fallback) | p50 latency | Cost per 1k answers |
| --- | ---: | ---: | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 66.0% to **99.0%** | 50% to 98.7% | 99.4% to **100%** | 1.9 s to 2.2 s | $0.32 to $0.34 |
| `openai/gpt-6-luna` | 85.2% to 82.1% | 76% to 73% | 100% to 99.4% | 2.7 s to 2.9 s | $0.20 to $0.27 |
| `nvidia/nemotron-3.5-lightning` | 37.0% to 74.1% | 15% to 62% | 88.3% to 95.1% | 2.8 s to 2.8 s | $0.16 to $0.20 |

On the default, 481 of 486 first answers were valid, with a 95% interval of 97.6–99.6%. The other 5 were patched locally, with no second LLM call and no repair. The first 162 calls scored 162 and the next 324 scored 319 (98.5%), so 100% was luck. **On the default model, 99% first try is met at the point estimate, though the data cannot rule out 98%, and 99% overall is met. Luna and Lightning miss 99% first try; Luna meets 99% overall and Lightning does not.**

Luna does not benefit because OpenAI treats the schema as advice without `strict`, and strict mode refuses most API contracts. Its misses are exact-length patterns such as `^[0-9A-Za-z]{22}$`, which local patching fixes without a second call. Its 14 HTTP rejections are schemas OpenAI refuses, which step down to JSON mode. Lightning's fallbacks are 3 timeouts and 5 answers still invalid after repair.

Blind judging on 54 paired answers across all 18 contracts: realism -0.09 ± 0.26, coherence +0.33 ± 0.29. Schema output has no measured quality cost. Those answers (`engine-final`) predate the 2,000-character limit and the last client fixes.

## Variants, on replayed prompts

First try, p50 latency and cost per 1k answers, 53 prompts each.

| Variant | Default | Luna | Lightning |
| --- | ---: | ---: | ---: |
| Control (`json_object`, no reasoning) | 66%, 1.8 s, $0.30 | 85%, 3.0 s, $0.25 | 36%, 3.6 s, $0.13 |
| Minimal reasoning | 79%, 2.9 s, $0.58 | 89%, 4.6 s, $0.32 | 55%, 34 s, $0.88 |
| Low reasoning | 81%, 2.8 s, $0.52 | 79%, 4.2 s, $0.24 | 66%, 39 s, $0.92 |
| Valid-value hints | 77%, 1.6 s, $0.35 | 81%, 2.6 s, $0.27 | 49%, 3.2 s, $0.15 |
| Hints and low reasoning | 89%, 4.8 s, $0.64 | 94%, 3.9 s, $0.33 | 64%, 44 s, $1.02 |
| Schema | 98%, 1.8 s, $0.50* | 83%, 2.5 s, $0.20† | 70%, 2.6 s, $0.24* |
| Schema and hints | 98%, 2.5 s | 83%, 2.2 s† | 85%, 3.5 s |

\* Includes rare runaways to 16,384 tokens; without them the default costs $0.25–0.33. † The bench proxy was stripping the schema after a rejection, so these mostly ran without it.

**Reasoning:** it helps first try on every model except low effort on Luna, but schema output beats it on the default at no latency cost. Reasoning doubles latency on the default and costs 10 times as long on Lightning, while multiplying cost by up to 7. It stays off. It is worth considering only for a model like Luna, where hints and low reasoning gave the best result seen (94%) at 3.9 s, and that combination is not built.

## Runaways

With a schema enforced, a model can loop in an unbounded string until `max_tokens`. On gRPC `RouteGuide.GetFeature`, 16 of 24 calls ran away to 16,384 tokens. With `maxLength` on free-text strings, 0 of 24 did, and the median fell from 16,384 tokens to 60, so free-text strings now carry a 2,000-character limit on the wire. Models can still loop on whitespace or digits, which this does not catch. In the final runs that was 0 of 486 calls on the default (longest answer 3,420 tokens), 1 of 179 on Luna and 3 of 184 on Lightning. It is the largest remaining source of failures and slow answers on the weaker models.

## Limits

- 486 calls cannot separate 98.5% from 99.5%.
- The output budget's fill was not applied in the replay, which slightly understates the replay's first try.
- Provider routing varies between runs, and runaways cluster on some providers.
- `engine-schema` and `engine-final` are earlier runs on code before the final fixes, kept for the judging. They gave the default 98.1% and 97.5%.
