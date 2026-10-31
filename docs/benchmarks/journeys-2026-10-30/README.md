# Simulated-world journeys, 2026-10-30

Measures the [simulated worlds](../../superpowers/specs/2026-10-30-simulated-world-design.md) work on full-document generation: action routes act on their parent, nested entities are remembered, references reach the prompt, and writes have effects. No context beyond each contract. Spend was $0.38 through `proxy.py`, across the baseline, four journey runs and two single-answer runs.

**Result:** journey checks passed rose from 54% to 96%, with a third fewer LLM calls and no measured cost to single answers. One bar is missed: a confirm moves its status on only 3 of 6 times, all misses on Luna and Lightning.

## The bar

| Measure | Pass | Result | |
| --- | --- | ---: | --- |
| Journey checks passed | At least 75%, from a baseline of 54% | 164/171 (95.9%) | Met |
| "confirm moves the status on" | At most one below the baseline's 6/6 | 3/6 | **Missed** |
| Blind-judged coherence on single answers | No more than 0.2 below round 2's | +0.07 ± 0.34 | Met |
| Blind-judged realism on single answers | No more than 0.2 below round 2's | +0.10 ± 0.23 | Met |
| Mean prompt tokens on the journeys | At most 1.5 times the baseline's | 1,665 vs 1,567 (1.06×) | Met |

The baseline's 6/6 on confirm is a weak comparison. Then, a confirm invented a new resource, so `read shows the confirmed status` was 0/6. Now that check is 6/6: the read always agrees with what the confirm answered.

## Journeys

Three journeys (Stripe payment to refund, travel search to cancellation, GitHub search to issue), 3 models, 2 repetitions. `server.jsonl` holds each check.

| Model | Baseline | Now |
| --- | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 32/58 | **58/58** |
| `openai/gpt-6-luna` | 32/58 | 54/57 |
| `nvidia/nemotron-3.5-lightning` | 30/58 | 52/56 |

LLM calls fell from 100 to 68, and answers served from the store rose from 24 to 54. No journey answer fell back to the faker, against 6 in the baseline. The check count varies because `a succeeded intent received its amount` applies only when the status is `succeeded`.

Checks that changed:

| Check | Baseline | Now |
| --- | ---: | ---: |
| `github.repo counts the new issue` | 0/6 | 4/6 |
| `github.repo detail keeps the listed repo` | 1/6 | 6/6 |
| `github.repo keeps its identity` | 0/6 | 6/6 |
| `stripe.a succeeded intent received its amount` | 0/6 | 3/3 |
| `stripe.confirm keeps identity and amount` | 0/6 | 6/6 |
| `stripe.confirm moves the status on` | 6/6 | 3/6 |
| `stripe.read shows the confirmed status` | 0/6 | 6/6 |
| `stripe.refund uses the intent's currency` | 0/6 | 6/6 |
| `travel.booking costs what the offer did` | 0/6 | 5/6 |
| `travel.booking flies the offer's segments` | 0/6 | 5/6 |
| `travel.cancel shows on the booking` | 0/6 | 6/6 |
| `travel.cancellation names its booking` | 5/6 | 6/6 |
| `travel.offer detail keeps the listed price` | 0/6 | 6/6 |
| `travel.offer detail keeps the listed segments` | 0/6 | 6/6 |
| `travel.offers fly the searched route` | 5/6 | 6/6 |
| `travel.offers use the searched currency` | 5/6 | 6/6 |
| `travel.refund is in the booking's currency` | 0/6 | 6/6 |

Each measured run led to a fix, and this table is the last run:

| Run | Checks passed | Fix that followed |
| --- | ---: | --- |
| First | 123/168 (73%) | Offers were not remembered, because `allOf` resources had no identity. Actions now say they succeeded. Nested routes now see their parent. |
| Second | 164/173 (95%) | Regressions found in review |
| Third | 162/172 (94%) | The default model never wrote effects, because the schema did not require them. They are now required and asked for first. |
| Last | 164/171 (96%) | |

Effects in the last run: 23 of 67 answers had some. These runs are small and vary by a few checks, so treat 94–96% as one result.

## Single answers

162 calls per model on all 18 contracts, default settings, on the code before one last guard against a reply holding effects and no answer. The guard does not touch valid answers. Compare with [the first-try run](../first-try-2026-10-30/README.md).

| Model | First try | REST first try | Overall | p50 | Cost per 1k |
| --- | ---: | ---: | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` | 99.4% | 99% | 100% | 1.6 s | $0.32 |
| `openai/gpt-6-luna` | 82.1% | 73% | 100% | 2.5 s | $0.21 |
| `nvidia/nemotron-3.5-lightning` | 87.7% | 80% | 98.8% | 1.4 s | $0.17 |

Lightning rose from 74.1% and p50 latencies fell, but this work did not aim at either and provider routing varies between runs, so neither is claimed as a gain. 161 of 162 has a 95% interval of about 96.6–99.9%.

Blind judging: two judges, 14 targets, 42 paired answers against round 2. Realism 3.69 against 3.60, coherence 4.12 against 4.05. Single answers do not hold session state, so this checks that the prompt and schema changes do no harm. It does not measure the world.

## What it implies

- **Fallback templates:** journeys had no fallback in the last run, and single answers few. There is no case yet for building them, so step 4 of the spec waits.
- **Learned invariants:** judges still name durations that contradict timestamps, as in the pilot (`LIS-FNC 11:30Z to 14:20Z is 170 min but duration 110`). The case for step 5 stands, and it needs its own test.
- **State moves on weaker models:** Luna and Lightning can still keep a status on a confirm. Moving the note to the system prompt or naming a target state from the contract's enum could help. Neither was tried.

## Limits

- 6 runs per journey and model is small, and 3 contracts is narrow.
- These three journeys drove the fixes, so 96% flatters what a new contract would get.
- `github.repo counts the new issue` needs a model to think of the count, so 4/6 is reasonable for a mock with no rules.
- Known gaps, from review: listed resources with a string id equal to their name can still answer for another owner; a read by number or key can change after a later answer mentions it.
