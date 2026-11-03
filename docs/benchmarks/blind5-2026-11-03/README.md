# Fifth blind set and full rerun, 2026-11-03

This round ran every suite on 3 models. The code was `main` after #194, #195 and #197–#199, plus #196, which was merged locally for the run. Those PRs cover references found by value, list overlays and filters, cursor pagination, effect merging, update settling, and declared refusals and headers in the prompt. A [fifth blind set](../../../scripts/llm-bench/README.md) ran for the first time. Spend was $1.51 through `proxy.py`: $0.15 for single answers and $1.36 for the suites. The recordings hold no API key.

**Result:** on five new contracts, journeys passed 75.8% across models and 78.9% on the default model. The previous blind set scored 68.9% on its own first run. Default first try was 98.8%. Neither number meets the 99% goal yet. Multi-step state is still where most checks fail, mainly uniqueness refusals, cross-record links and pagination tokens.

## Journeys

| Suite | All models | Default | Luna | Lightning | Previous round |
| --- | ---: | ---: | ---: | ---: | ---: |
| Fifth blind (first run) | 637/840 (75.8%) | 221/280 | 218/280 | 198/280 | — |
| Fourth blind | 628/840 (74.8%) | 217/280 | 202/280 | 209/280 | 579/840 (68.9%) |
| Third blind | 650/840 (77.4%) | 222/280 | 226/280 | 202/280 | 634/840 (75.5%) |
| First blind | 626/840 (74.5%) | 224/280 | 230/280 | 172/280 | 597/840 (71.1%) |
| Second blind | 767/840 (91.3%) | 262/280 | 260/280 | 245/280 | 765/840 (91.1%) |
| Held-out | 739/840 (88.0%) | 250/280 | 262/280 | 227/280 | 749/840 (89.2%) |
| Tuned | 327/347 (94.2%) | 114/115 | 108/116 | 105/116 | 326/346 (94.2%) |

Each suite has a different status:

- **Fifth blind set:** the honest number for this round. No fix looked at its contracts or results.
- **Fourth blind set:** stopped being blind this round. Its failures were diagnosed and fed this round's fixes, so its 6-point rise is partly tuning.
- **First blind set:** up 3.4 points with no fix aimed at it. This is the clearest sign that the fixes generalize.
- **Third and second blind sets, held-out and tuned:** within run-to-run spread of 2–4 points. Held-out fell 1.2 points, still inside that spread.

Each fix this round had to fail the same way in at least 3 contracts across at least 2 suites, and use only contract structure or what the mock had seen.

| Fifth blind journey | Passed |
| --- | ---: |
| Okta users | 144/168 |
| Airtable records | 138/168 |
| Cal.com bookings | 135/168 |
| Intercom conversations | 118/168 |
| Box folders | 102/168 |

6 checks failed in every run:

- Okta: `read by login matches the user`
- Box: `a taken name is refused`
- Intercom: `a second contact with the same email is refused`, `conversation lists its contact`, `admin reply shows as a conversation part`
- Cal.com: `unknown booking is not found`

These failures have not been diagnosed yet. Once they are, this set stops being blind and the next round needs a sixth set.

## Single answers

162 calls per model on all 18 contracts, default settings.

| Model | First try | Valid overall | Fallback | p50 |
| --- | ---: | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 160/162 (98.8%) | 100% | 0 | 2.4 s |
| `openai/gpt-6-luna` | 143/162 (88.3%) | 100% | 0 | 2.6 s |
| `nvidia/nemotron-3.5-lightning` | 137/162 (84.6%) | 98.8% | 2 | 1.5 s |

The last four default runs gave 157, 161, 159 and 160, so 98–99% remains the honest range.

## Other suites

| Suite | Default | Luna | Lightning |
| --- | ---: | ---: | ---: |
| Consistency | 24/24 | 23/24 | 23/24 |
| SSE | 4/4 | 4/4 | 4/4 |
| Import | 4/4 | 4/4 | 3/4 |

## Fallbacks

Across all suites and models, 30 journey answers fell back to the faker on a provider timeout, 3 of them in the fifth blind set. Another 19 fell back because the repaired answer was still invalid, plus 1 consistency answer. Most were in the large Notion, Spotify and HubSpot schemas of the older suites, and none were in the fifth blind set.

## Limits

- 12 runs per check per model is small.
- Blind judging of realism and coherence was not re-run. The [earlier round's](../full-2026-11-03/README.md) scores stand: 4.07 realism and 4.50 coherence on the default model.
- All contracts are hand-written subsets of real APIs.
- Redis and the webhook paths did not run with a real model.

`server.jsonl` holds each journey and suite check, and `engine/` holds the single-answer reports.
