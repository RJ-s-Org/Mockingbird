# Tenth blind set, 2026-11-04

This round ran on `main` at 641ff6e6. The changes since [the last round](../round-2026-11-04/README.md):

- **Repetition guard:** a streamed answer that keeps repeating text for 32 KiB is stopped and retried once. Every LLM call now streams when the guard is on, and each LLM profile can turn the guard off (#241).
- **Bench:**
  - a rate-limited call is labelled a provider failure;
  - a failure that comes after a provider failure earlier in its journey gets its own label, but still counts against the model;
  - the default model's pin names its endpoint precision (#240);
  - the proxy stops reading from a provider once Mockingbird hangs up (#242).
- **New suite:** a [tenth blind set](../../../scripts/llm-bench/README.md) was added (#243). The same PR fixed a double count in the ninth set's paging checks.

Every journey suite ran on 3 models, alongside consistency and single answers. Spend through `proxy.py` was $2.13: $0.12 for single answers and $2.01 for the suites. No check carried a provider-failure label, and the recordings hold no API key.

**Result:**

- **Tenth set (first run):** on five new contracts, journeys passed 29.0% across models. Luna reached 41.8%, and the default model 19.6%.
- **Default model, the nine earlier blind sets together:** 74.8%, up from 72.9%. On the second set it reached 95.4%, and on held-out 91.1%.
- **Lightning:** rose from 51.3% to 59.4% on those sets. 14 of its answers hit the token limit, against 258 in the last run, and the guard cut 77 loops short.
- **Single answers:** the default model got 162/162 right first time.

## Journeys

| Suite | All models | Default | Luna | Lightning | Previous (all) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Tenth blind (first run) | 244/840 (29.0%) | 55/280 | 117/280 | 72/280 | — |
| Ninth blind | 330/840 (39.3%) | 114/280 | 136/280 | 80/280 | 303/840 (36.1%) |
| Eighth blind | 391/840 (46.5%) | 129/280 | 157/280 | 105/280 | 346/840 (41.2%) |
| Seventh blind | 546/840 (65.0%) | 193/280 | 206/280 | 147/280 | 508/840 (60.5%) |
| Sixth blind | 586/840 (69.8%) | 217/280 | 213/280 | 156/280 | 569/840 (67.7%) |
| Fifth blind | 667/828 (80.6%) | 243/276 | 229/276 | 195/276 | 622/828 (75.1%) |
| Fourth blind | 697/828 (84.2%) | 243/276 | 236/276 | 218/276 | 664/828 (80.2%) |
| Third blind | 679/840 (80.8%) | 231/280 | 243/280 | 205/280 | 652/840 (77.6%) |
| Second blind | 749/840 (89.2%) | 267/280 | 253/280 | 229/280 | 723/840 (86.1%) |
| First blind | 624/828 (75.4%) | 238/276 | 231/276 | 155/276 | 628/828 (75.8%) |
| Held-out | 761/840 (90.6%) | 255/280 | 272/280 | 234/280 | 713/840 (84.9%) |
| Tuned | 318/347 (91.6%) | 114/115 | 108/115 | 96/117 | 315/347 (90.8%) |

The ninth set's paging checks were reworked in #243, so its number is not comparable check for check.

| Tenth blind journey | Passed |
| --- | ---: |
| Heroku apps (`Range` paging, releases, rollback) | 70/168 |
| Google Calendar events (conditional requests, sync tokens) | 63/168 |
| PagerDuty incidents (`From` header, state machine, `include[]`) | 50/168 |
| Databricks SCIM users and groups (PATCH operations) | 48/168 |
| Kubernetes config maps (label selectors, `continue`, `resourceVersion`) | 13/168 |

35 checks failed in every run. They include:
- nearly every Kubernetes check after the create;
- uniqueness refusals in all five contracts;
- `Range` and `startIndex` paging;
- conditional `If-None-Match` reads;
- deleted records found again.

These are being diagnosed. Once they are, this set stops being blind.

## Single answers

162 calls per model on all 18 contracts, default settings.

| Model | First try | Fallback | p50 |
| --- | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 162/162 (100%) | 0 | 1.8 s |
| `openai/gpt-6-luna` | 135/162 (83.3%) | 3 | 2.7 s |
| `nvidia/nemotron-3.5-lightning` | 126/162 (77.8%) | 11 | 1.3 s |

## Other suites

Consistency passed 24/24 on the default model and on Luna, and 23/24 on Lightning.

## Limits

- 12 runs per check per model is small.
- SSE and import did not run, and blind judging was not re-run.
- All contracts are hand-written subsets of real APIs.

`server.jsonl` holds each journey and suite check, and `engine/` holds the single-answer reports.
