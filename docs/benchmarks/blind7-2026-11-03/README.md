# Seventh blind set, 2026-11-03

This round ran on `main` at 9aa97278, after #208–#212:
- answers wrapped in a shared envelope now give the record inside it an identity;
- page-number and offset pages are served from stored records;
- a bench crash and unquoted YAML commas were fixed, and the faker no longer draws Unicode for `.` in a pattern;
- create refusals now work on every state store, the default `sqlite::memory:` included;
- a [seventh blind set](../../../scripts/llm-bench/README.md) was added.

The seventh set ran for the first time, alongside the first, fourth, fifth and sixth sets, the held-out set and consistency. Spend through `proxy.py` was $1.61: $0.14 for single answers and $1.47 for the suites. The proxy was restarted twice with a higher cap, carrying the earlier spend; a call in flight during a restart could have fallen back. The recordings hold no API key.

**Result:**
- On five new contracts in styles the earlier sets covered less, journeys passed 51.2% across models and 50.4% on the default model.
- The sixth set, diagnosed last round, rose from 55.5% to 66.1%. The fifth rose from 77.5% to 81.4%.
- The first blind set fell 3.4 points. Most of the fall is on the Lightning model, and is being checked as a possible regression.
- Default first try was 161/162 (99.4%), meeting the 99% goal on this run.

## Journeys

| Suite | All models | Default | Luna | Lightning | Previous |
| --- | ---: | ---: | ---: | ---: | ---: |
| Seventh blind (first run) | 430/840 (51.2%) | 141/280 | 154/280 | 135/280 | — |
| Sixth blind | 555/840 (66.1%) | 204/280 | 196/280 | 155/280 | 466/840 (55.5%) |
| Fifth blind | 674/828 (81.4%) | 246/276 | 224/276 | 204/276 | 642/828 (77.5%) |
| Fourth blind | 637/828 (76.9%) | 219/276 | 218/276 | 200/276 | 636/828 (76.8%) |
| First blind | 597/840 (71.1%) | 215/280 | 231/280 | 151/280 | 626/840 (74.5%) |
| Held-out | 740/840 (88.1%) | 247/280 | 269/280 | 224/280 | 739/840 (88.0%) |

The sixth set's failures fed this round's fixes, so its rise is partly tuning. The first blind set was not looked at; its fall is being diagnosed. Lightning had 10 fallbacks there, and 17 on held-out.

| Seventh blind journey | Passed |
| --- | ---: |
| Mollie payments (HAL links, money strings) | 126/168 |
| Bitbucket pull requests (three-level nesting, soft-deleted comments) | 110/168 |
| Chargebee customers (form-encoded, minor units, epoch seconds) | 93/168 |
| Linear issues (GraphQL) | 59/168 |
| Cloudflare DNS records (success/result envelope, batch endpoint) | 42/168 |

23 checks failed in every run. They include:
- every Cloudflare read after the create;
- most Chargebee reads after the create;
- Linear's update, comments and archive handling;
- uniqueness refusals in Chargebee, Bitbucket and Cloudflare, and Mollie's declared 422 for a malformed amount.

These are being diagnosed. Once they are, this set stops being blind.

## Single answers

162 calls per model on all 18 contracts, default settings.

| Model | First try | Valid overall | Fallback | p50 |
| --- | ---: | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 161/162 (99.4%) | 100% | 0 | 1.9 s |
| `openai/gpt-6-luna` | 137/162 (84.6%) | 98.8% | 2 | 2.0 s |
| `nvidia/nemotron-3.5-lightning` | 140/162 (86.4%) | 96.3% | 6 | 1.3 s |

The last five default runs gave 157, 161, 159, 160 and 161.

## Other suites

Consistency passed 24/24 on the default model, 23/24 on Luna and 21/24 on Lightning.

## Limits

- 12 runs per check per model is small.
- The tuned set, the second and third blind sets, SSE and import did not run, to fit the budget.
- Blind judging of realism and coherence was not re-run.
- All contracts are hand-written subsets of real APIs.

`server.jsonl` holds each journey and suite check, and `engine/` holds the single-answer reports.
