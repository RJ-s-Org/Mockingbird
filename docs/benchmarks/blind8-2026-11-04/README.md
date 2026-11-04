# Eighth blind set, 2026-11-04

This round ran on `main` at 058a3981. The changes since the last round:

- **Bench:** a provider failure is now only a label and never re-run (#216). The Discord gate was dropped from the first blind set, so it now has 13 checks.
- **Creates:** a repeated create no longer overwrites the stored record or copies its request onto a sibling (#217).
- **Paging:** next-page links given as URLs are filled in, and only a page cursor the mock itself served reaches the prompt (#218).
- **GraphQL:** noun-first mutations such as `commentDelete` now have kinds, and a mutation updates a record by id only when its name names that record (#219).
- **WSDL and prompts:** WSDL documentation reaches the prompt, and every hint, summary and name in the prompt is escaped and redacted (#221).
- **Routing:** a new path router handles OData-style keys and other parameters inside a segment, and skips a route that can't be registered instead of failing the whole mock (#222).
- **New suite:** a [eighth blind set](../../../scripts/llm-bench/README.md) was added (#220).

Every journey suite ran on 3 models, alongside consistency. It is the first full run on the parallel runner (#213, #214): the suites took about 9 minutes, against several hours before. Spend through `proxy.py` was $2.15: $0.12 for single answers and $2.03 for the suites. No call was labelled a provider failure, and the recordings hold no API key.

**Result:**

- **Eighth set:** on five new contracts, journeys passed 37.0% across models and 30.7% on the default model. Luna led at 46.8%.
- **Seventh set:** diagnosed last round, it rose from 51.2% to 57.9%.
- **Other suites:** within run-to-run spread of the last measurement.
- **Single answers:** the default model got 161/162 (99.4%) right first time.

## Journeys

| Suite | All models | Default | Luna | Lightning | Previous |
| --- | ---: | ---: | ---: | ---: | ---: |
| Eighth blind (first run) | 311/840 (37.0%) | 86/280 | 131/280 | 94/280 | — |
| Seventh blind | 486/840 (57.9%) | 162/280 | 169/280 | 155/280 | 430/840 (51.2%) |
| Sixth blind | 559/840 (66.5%) | 205/280 | 197/280 | 157/280 | 555/840 (66.1%) |
| Fifth blind | 654/828 (79.0%) | 232/276 | 218/276 | 204/276 | 674/828 (81.4%) |
| Fourth blind | 640/828 (77.3%) | 222/276 | 216/276 | 202/276 | 637/828 (76.9%) |
| Third blind | 672/840 (80.0%) | 226/280 | 236/280 | 210/280 | 650/840 (77.4%) |
| Second blind | 755/840 (89.9%) | 258/280 | 263/280 | 234/280 | 767/840 (91.3%) |
| First blind | 611/828 (73.8%) | 222/276 | 210/276 | 179/276 | 597/840 (71.1%) |
| Held-out | 749/840 (89.2%) | 249/280 | 269/280 | 231/280 | 740/840 (88.1%) |
| Tuned | 327/347 (94.2%) | 114/115 | 101/116 | 112/116 | 327/347 (94.2%) |

The second and third sets last ran in an earlier round (blind5's), so their deltas cover several rounds of fixes. The first set now has one check fewer on Discord, so its number is not comparable check for check.

| Eighth blind journey | Passed |
| --- | ---: |
| Azure DevOps work items (JSON Patch, WIQL) | 92/168 |
| Business Central customers and invoices (OData) | 79/168 |
| commercetools carts (update actions, versions, cent amounts) | 75/168 |
| eBay Trading listings (SOAP) | 42/168 |
| Meilisearch documents (async tasks, search) | 23/168 |

These are being diagnosed. Once they are, this set stops being blind.

## Single answers

162 calls per model on all 18 contracts, default settings.

| Model | First try | Fallback | p50 |
| --- | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 161/162 (99.4%) | 0 | 2.1 s |
| `openai/gpt-6-luna` | 136/162 (84.0%) | 0 | 2.1 s |
| `nvidia/nemotron-3.5-lightning` | 137/162 (84.6%) | 0 | 1.3 s |

## Other suites

Consistency passed 24/24 on the default model and on Luna, and 23/24 on Lightning.

## Fallbacks

- **Sixth set:** 29 answers fell back to the faker, all from Luna, which writes Dropbox's `.tag` key with a leading space.
- **Everywhere else:** fewer than 7 per suite.
- **Provider failures:** none.

## Limits

- 12 runs per check per model is small.
- SSE and import did not run, and blind judging of realism and coherence was not re-run.
- All contracts are hand-written subsets of real APIs.

`server.jsonl` holds each journey and suite check, and `engine/` holds the single-answer reports.
