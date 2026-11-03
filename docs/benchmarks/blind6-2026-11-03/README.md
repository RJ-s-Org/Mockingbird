# Sixth blind set, 2026-11-03

This round ran on `main` at c619c0f, after #202–#206: page tokens from stored records, remembered answer envelopes, the API description in the prompt, create refusals on shared values, and fixes to the benchmark's own contracts. A [sixth blind set](../../../scripts/llm-bench/README.md) ran for the first time, alongside the fourth and fifth sets and consistency. Spend through `proxy.py` was $0.79: $0.71 for the suites and $0.08 for single answers. The recordings hold no API key.

**Result:** on five new contracts, journeys passed 55.5% across models and 57.9% on the default model. The fifth set scored 75.8% on its own first run. The drop comes from two contracts: Dropbox (25.6%) and Zoho Books (33.3%). The other three scored 60–80%, in line with earlier first runs.

## Journeys

| Suite | All models | Default | Luna | Lightning | Previous round |
| --- | ---: | ---: | ---: | ---: | ---: |
| Sixth blind (first run) | 466/840 (55.5%) | 162/280 | 161/280 | 143/280 | — |
| Fifth blind | 642/828 (77.5%) | 228/276 | 216/276 | 198/276 | 637/840 (75.8%) |
| Fourth blind | 636/828 (76.8%) | 225/276 | 208/276 | 203/276 | 628/840 (74.8%) |

The fourth and fifth sets have been diagnosed, and their checks changed in #206 (Sentry and Cal.com each lost one check), so their gains are partly tuning. The sixth set is the honest number.

| Sixth blind journey | Passed |
| --- | ---: |
| WooCommerce products and orders | 135/168 |
| Statuspage incidents | 130/168 |
| Klaviyo profiles | 102/168 |
| Zoho Books invoices | 56/168 |
| Dropbox files | 43/168 |

20 checks failed in every run:

- Dropbox: 9 checks, including every read, list, move and delete after the folder is created. 28 of its answers fell back to the faker because the repaired answer still lacked the union tag `.tag`.
- Zoho Books: 8 checks, including every step after the invoice is created.
- Uniqueness refusals in four contracts: Dropbox `a taken path is refused`, Klaviyo `a taken email names the profile that has it`, WooCommerce `a taken sku is refused`, Zoho Books `a taken invoice number is refused`.
- Klaviyo `a removed profile leaves the list`; Statuspage `edited update keeps its new body`.

These are being diagnosed. Once they are, this set stops being blind.

## Single answers

162 calls per model on all 18 contracts, default settings. The default model was skipped: the run's $0.20 cap for this workload projected past its limit while the other two models were in flight, so the default model's first try was not measured this round. Its last five runs gave 157–161.

| Model | First try | Valid overall | Fallback | p50 |
| --- | ---: | ---: | ---: | ---: |
| `openai/gpt-6-luna` | 137/162 (84.6%) | 99.4% | 1 | 2.3 s |
| `nvidia/nemotron-3.5-lightning` | 142/162 (87.7%) | 98.8% | 2 | 1.4 s |

## Other suites

Consistency passed 24/24 on all three models.

## Fallbacks

32 sixth-set answers fell back to the faker, 28 of them on Luna's Dropbox runs, almost all because a required `.tag` was missing after repair. 3 were provider timeouts and 1 was output truncated at the token limit. The fourth and fifth sets had 10 fallbacks, all timeouts.

## Limits

- 12 runs per check per model is small.
- Held-out, tuned, SSE, import and the first three blind sets did not run this round, to fit the budget.
- Blind judging of realism and coherence was not re-run.
- All contracts are hand-written subsets of real APIs.

`server.jsonl` holds each journey and suite check, and `engine/` holds the single-answer reports.
