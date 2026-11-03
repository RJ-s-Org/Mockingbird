# Full benchmark, 2026-11-03

Every suite on `main` after the world fixes (#176–#178, #182–#185), on 3 models: single answers, the consistency, SSE and import suites, and all five journey suites at 4 repetitions. A [third blind set](../../../scripts/llm-bench/README.md) ran here for the first time. Spend was $1.20 through `proxy.py`, and the recordings hold no API key.

**Result:** on five contracts nobody had looked at, journeys passed 64.2%, and 66.4% on the default model. Default first try is 99.4% with #186. Neither meets the 99% goal, and the gap is in multi-step state, not in single answers.

## Journeys

| Suite | All models | Default | Luna | Lightning |
| --- | ---: | ---: | ---: | ---: |
| Third blind (first run) | 539/840 (64.2%) | 186/280 | 181/280 | 172/280 |
| First blind | 607/840 (72.3%) | 226/280 | 213/280 | 168/280 |
| Second blind | 766/840 (91.2%) | 261/280 | 263/280 | 242/280 |
| Held-out | 744/840 (88.6%) | 249/280 | 258/280 | 237/280 |
| Tuned | 319/346 (92.2%) | 112/114 | 104/116 | 103/116 |

Only the third blind set says what a new contract gets. The others have all been tuned against:

- **First blind set:** 44.4% at [its baseline](../blind-baseline-2026-11-01/README.md), 71.8% after the first fixes, now 72.3%. The latest round of fixes barely moved it, though none of them looked at it.
- **Second blind set:** 81.9% when [first run](../blind-final-2026-11-01/README.md). It rose to 91.2% after its failures were diagnosed and fixed, so most of that rise is tuning.

| Third blind journey | Passed |
| --- | ---: |
| GitLab issues | 130/168 |
| Trello boards | 125/168 |
| Freshdesk tickets | 113/168 |
| Mailchimp audiences | 92/168 |
| Asana tasks | 79/168 |

The third set's contracts and checks were written by an author who read no engine code, docs or results. Its failures have not been diagnosed, so that it can measure the next round of work fairly. 7 of its answers fell back to the faker: 5 provider timeouts, 1 truncated answer and 1 answer that stayed invalid.

## Single answers

162 calls per model on all 18 contracts, default settings.

| Model | First try | Valid overall | Fallback | p50 |
| --- | ---: | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 161/162 (99.4%) | 100% | 0 | 1.9 s |
| `openai/gpt-6-luna` | 135/162 (83.3%) | 99.4% | 1 | 2.4 s |
| `nvidia/nemotron-3.5-lightning` | 144/162 (88.9%) | 98.8% | 2 | 1.2 s |

This is the rerun with #186, which moves a member nested one level too deep back to its parent. The run before it, on the same code without #186, had the default at 157/162. A diagnosis found that drop was mostly model noise, with 4 of the 5 failures served by one upstream host. The one real gap it found was a misplaced member, now handled locally.

## Other suites

| Suite | Default | Luna | Lightning |
| --- | ---: | ---: | ---: |
| Consistency | 23/24 | 21/24 | 22/24 |
| SSE | 4/4 | 4/4 | 4/4 |
| Import | 4/4 | 3/4 | 2/4 |

## Blind judging

Two Claude judges scored 7 targets blind, so each model got 14 scores per scale.

| Model | Realism | Coherence |
| --- | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 4.07 | 4.50 |
| `openai/gpt-6-luna` | 4.21 | 4.29 |
| `nvidia/nemotron-3.5-lightning` | 3.50 | 4.36 |

The targets and judges differ from earlier rounds, so this is not a comparison with them. It shows the world work left answer quality where a real API would plausibly be.

## Limits

- Journeys ran on the code before #186, which only changes the patcher. Single answers ran on #186 before its review fixes.
- 12 runs per check per model is small. Run-to-run spread on these suites has been 2–4 points.
- All contracts are hand-written subsets of real APIs.
- Redis and the webhook paths did not run with a real model. #185 added write-memory and delete-marker tests that run against Redis and Postgres when CI provides them, and only the Postgres ones ran locally.

`server.jsonl` holds each journey and suite check. `engine/` holds the single-answer reports, and `judge/` holds the judge scores.
