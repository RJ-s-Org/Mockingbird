# First-try experiments, 2026-10-30

What makes an LLM's first answer valid, and at what cost. Prompts were replayed from round 2 with [`scripts/llm-bench/first_try.py`](../../../scripts/llm-bench/first_try.py) and graded by the engine's own checks; 53 prompts per model across all 18 contracts. The final numbers come from the real engine through `mb-evals`. Spend was $0.75.

**Result:** schema output (`output_mode: json_schema`) is by far the best lever, and is now the default. Reasoning is not worth its latency.

## Real engine, default settings

162 calls per model, against round 2's `json_object`. First try is per call, so REST carries its full weight.

| Model | First try | REST first try | Overall (no fallback) | p50 latency | Cost per 1k answers |
| --- | ---: | ---: | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 66.0% to **97.5%** | 50% to 96% | 99.4% to **99.4%** | 1.9 s to 2.2 s | $0.32 to $0.40 |
| `openai/gpt-6-luna` | 85.2% to 82.1% | 76% to 73% | 100% to 99.4% | 2.7 s to 2.7 s | $0.20 to $0.27 |
| `nvidia/nemotron-3.5-lightning` | 37.0% to 76.5% | 15% to 66% | 88.3% to 90.7% | 2.8 s to 3.6 s | $0.16 to $0.22 |

An earlier engine run without the runaway limit gave the default 98.1%, and the replay pool 98.7% over 159 calls, so about 98% is the fair reading. 158 of 162 has a 95% interval of about 94–99%. **The 99% first-try goal is close on the default model but not shown, and is not met on the other two.** Luna and Lightning results include the race later fixed in the client (Luna's one fallback).

Luna does not benefit because OpenAI treats the schema as advice without `strict`, and strict mode refuses most API contracts. Its misses are exact-length patterns such as `^[0-9A-Za-z]{22}$`, which local patching fixes without a second call.

Blind judging on 54 paired answers across all 18 contracts: realism -0.09 ± 0.26, coherence +0.33 ± 0.29. Schema output has no measured quality cost.

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

With a schema enforced, a model can loop in an unbounded string until `max_tokens`. On gRPC `RouteGuide.GetFeature`, 16 of 24 calls ran away to 16,384 tokens. With `maxLength` on free-text strings, 0 of 24 did, and the median fell from 16,384 tokens to 60, so strings now carry a 500-character limit on the wire. Models can still loop on whitespace or digits, which this does not catch: 1 of 164 calls on the default, 4 of 181 on Lightning. That is the largest remaining source of failures and slow answers.

## Limits

- A 162-call sample cannot separate 98% from 99%.
- The output budget's fill was not applied in the replay, which slightly understates the replay's first try.
- Provider routing varies between runs, and runaways cluster on some providers.
