# Ninth blind set, 2026-11-04

This round ran on `main` at 4c208deb. The changes since the last round:

- **Importer:** parameters inside a path segment (OData keys, `key=`) now get an identity (#224).
- **Concurrency:** a stale version token in the body is refused with the declared 409 or 412, and error bodies the mock builds take codes from the contract (#225).
- **Answers:** corrupted member names such as `"System.Title:"` are repaired, nulls on non-nullable members are handled, and an inline record needs positive evidence to be stored (#226).
- **Tests:** the routing corpus test no longer fails on route sets that matchit refuses (#227).
- **New suite:** a [ninth blind set](../../../scripts/llm-bench/README.md) was added (#228).

Every journey suite ran on 3 models, alongside consistency. Spend through `proxy.py` was $2.73: $0.14 for single answers and $2.59 for the suites. No call was labelled a provider failure, and the recordings hold no API key.

**Result:**

- **Ninth set:** on five new contracts, journeys passed 30.5% across models and 27.5% on the default model. Luna led at 37.5%.
- **Eighth set:** after its diagnosis and fixes, it rose from 37.0% to 39.8%. The gain is smaller than the fixes aimed for.
- **Held-out:** fell from 89.2% to 86.2%, with 35 fallbacks against fewer than 10 before. This is being checked as a possible regression.
- **Other suites:** within run-to-run spread.
- **Single answers:** the default model got 160/162 (98.8%) right first time.

## Journeys

| Suite | All models | Default | Luna | Lightning | Previous |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ninth blind (first run) | 256/840 (30.5%) | 77/280 | 105/280 | 74/280 | — |
| Eighth blind | 334/840 (39.8%) | 101/280 | 145/280 | 88/280 | 311/840 (37.0%) |
| Seventh blind | 462/840 (55.0%) | 152/280 | 167/280 | 143/280 | 486/840 (57.9%) |
| Sixth blind | 560/840 (66.7%) | 202/280 | 199/280 | 159/280 | 559/840 (66.5%) |
| Fifth blind | 663/828 (80.1%) | 232/276 | 231/276 | 200/276 | 654/828 (79.0%) |
| Fourth blind | 633/828 (76.4%) | 221/276 | 221/276 | 191/276 | 640/828 (77.3%) |
| Third blind | 657/840 (78.2%) | 224/280 | 230/280 | 203/280 | 672/840 (80.0%) |
| Second blind | 765/840 (91.1%) | 262/280 | 262/280 | 241/280 | 755/840 (89.9%) |
| First blind | 586/828 (70.8%) | 218/276 | 221/276 | 147/276 | 611/828 (73.8%) |
| Held-out | 724/840 (86.2%) | 244/280 | 258/280 | 222/280 | 749/840 (89.2%) |
| Tuned | 322/347 (92.8%) | 116/116 | 103/115 | 103/116 | 327/347 (94.2%) |

| Ninth blind journey | Passed |
| --- | ---: |
| Cloudinary images (signed multipart, tags, rename, restore) | 94/168 |
| Modern Treasury ledgers (idempotency keys, header cursor) | 73/168 |
| Directus items (bracket filters, relations, `fields`) | 54/168 |
| Cloud Resource Manager folders (operations, soft delete, move) | 22/168 |
| Xero contacts (tenant header, .NET dates, archive) | 13/168 |

31 checks failed in every run. They include:
- every Xero read, update and paging check after the create;
- Cloud Resource Manager's list, soft delete, undelete and move;
- partial responses by `fields` in two contracts;
- uniqueness refusals in Directus, Cloud Resource Manager and Xero, and Modern Treasury's repeated idempotency key.

These are being diagnosed. Once they are, this set stops being blind.

## Single answers

162 calls per model on all 18 contracts, default settings.

| Model | First try | Fallback | p50 |
| --- | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 160/162 (98.8%) | 1 | 2.2 s |
| `openai/gpt-6-luna` | 134/162 (82.7%) | 1 | 2.3 s |
| `nvidia/nemotron-3.5-lightning` | 135/162 (83.3%) | 6 | 2.0 s |

## Other suites

Consistency passed 24/24 on the default model and on Luna, and 21/24 on Lightning.

## Limits

- 12 runs per check per model is small, so differences of 2–3 points between rounds are within spread.
- SSE and import did not run, and blind judging of realism and coherence was not re-run.
- All contracts are hand-written subsets of real APIs.

`server.jsonl` holds each journey and suite check, and `engine/` holds the single-answer reports.
