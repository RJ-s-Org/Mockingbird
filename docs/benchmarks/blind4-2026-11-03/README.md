# Fourth blind set and full rerun, 2026-11-03

This round ran every suite on `main` after #186 and #189–#191 (the patcher, related writes, record keys from parameter names), on 3 models. A [fourth blind set](../../../scripts/llm-bench/README.md) ran for the first time. Spend was $1.25 through `proxy.py`: $0.15 for single answers and $1.10 for the suites. The recordings hold no API key.

**Result:** on five new contracts, journeys passed 68.9% across models and 70.0% on the default model. The third blind set scored 64.2% on its own first run. Default first try was 98.1%. Neither number meets the 99% goal, and multi-step state is still where most checks fail.

## Journeys

| Suite | All models | Default | Luna | Lightning | Previous round |
| --- | ---: | ---: | ---: | ---: | ---: |
| Fourth blind (first run) | 579/840 (68.9%) | 196/280 | 198/280 | 185/280 | — |
| Third blind | 634/840 (75.5%) | 216/280 | 217/280 | 201/280 | 539/840 (64.2%) |
| First blind | 597/840 (71.1%) | 215/280 | 205/280 | 177/280 | 607/840 (72.3%) |
| Second blind | 765/840 (91.1%) | 268/280 | 259/280 | 238/280 | 766/840 (91.2%) |
| Held-out | 749/840 (89.2%) | 247/280 | 268/280 | 234/280 | 744/840 (88.6%) |
| Tuned | 326/346 (94.2%) | 113/115 | 103/115 | 110/116 | 319/346 (92.2%) |

Each suite has a different status:

- **Fourth blind set:** the honest number for this round. No fix looked at its contracts or results. A reviewer compiled its contracts to check that they load, and so saw which APIs it uses, but saw no results.
- **Third blind set:** stopped being blind this round. Its failures were diagnosed and fed the fixes in #190 and #191, so most of its 11-point rise is tuning.
- **First and second blind sets, held-out and tuned:** no fix this round targeted them. They held within run-to-run spread of 2–4 points, so the fixes did not regress suites they weren't designed on.

Each fix this round had to fail the same way in at least 3 contracts across at least 2 suites, and use only contract structure or what the mock had seen.

| Fourth blind journey | Passed |
| --- | ---: |
| DigitalOcean droplets | 146/168 |
| Pipedrive deals | 134/168 |
| Zoom meetings | 130/168 |
| Sentry projects | 97/168 |
| Contentful entries | 72/168 |

10 checks failed in every run:

- Zoom: `page_size 1 returns one meeting and a token`, `next page holds another meeting`, `registration names the meeting`
- Sentry: `read by slug matches the created project`, `a taken slug is refused`, `new slug reads the same project`, `key belongs to the project`, `unknown issue is not found`
- Contentful: `content type takes the id asked for`, `read matches the created entry`

These failures have not been diagnosed. Once they are, this set stops being blind, like the third, and the next round needs a fifth set. 2 of its answers fell back to the faker: 1 provider timeout and 1 truncated answer.

## Single answers

162 calls per model on all 18 contracts, default settings.

| Model | First try | Valid overall | Fallback | p50 |
| --- | ---: | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 159/162 (98.1%) | 100% | 0 | 2.4 s |
| `openai/gpt-6-luna` | 133/162 (82.1%) | 99.4% | 1 | 2.7 s |
| `nvidia/nemotron-3.5-lightning` | 142/162 (87.7%) | 98.8% | 2 | 1.3 s |

The default model's 3 misses were all fixed locally by the patcher: 2 GitHub pull requests and 1 Spotify playlist. None needed a second model call. The last three runs on near-identical patcher code gave 157, 161 and 159, so 98–99% is the honest range. The swing is model noise, not a change.

## Other suites

| Suite | Default | Luna | Lightning |
| --- | ---: | ---: | ---: |
| Consistency | 23/24 | 21/24 | 21/24 |
| SSE | 4/4 | 4/4 | 4/4 |
| Import | 4/4 | 4/4 | 3/4 |

## Limits

- 12 runs per check per model is small.
- Blind judging of realism and coherence was not re-run. The [previous round's](../full-2026-11-03/README.md) scores stand: 4.07 realism and 4.50 coherence on the default model.
- All contracts are hand-written subsets of real APIs.
- Redis and the webhook paths did not run with a real model.
- #190 costs extra calls: a create-heavy flow took 11 model calls, against 6 before it.

`server.jsonl` holds each journey and suite check, and `engine/` holds the single-answer reports.
