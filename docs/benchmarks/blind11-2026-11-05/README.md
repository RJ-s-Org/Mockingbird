# Eleventh blind set, 2026-11-05

This round ran on `main` at 725f7852. The changes since [the last round](../blind10-2026-11-04/README.md):

- **Skeletal answers:** a model that writes only a schema's `required` members is now asked for the record's identity, its list member and the members the request sets. A create that still comes back without an id gets one minted, and a list that leaves out its list member is filled from the store (#245).
- **Nested identity and patches:**
  - a record keyed by a member inside shared record metadata (`metadata.name`) is now stored and found by it;
  - a create echo merges request objects into the answer member by member;
  - a `null` in a merge patch removes the member;
  - a compound path parameter (`{app_id_or_name}`) goes to the member whose schema takes its value (#246).
- **Conditional reads and `Range` paging:** a read that declares `304` and `If-None-Match` answers `304` while the tag matches the stored record. A list that declares a `Range` header and a `206` is paged by it, with `Next-Range` (#247).
- **New suite:** an [eleventh blind set](../../../scripts/llm-bench/README.md) was added (#248).

#245, #246 and #247 come from diagnosing the tenth set, so that set is no longer blind.

Every journey suite ran on 3 models, alongside consistency and single answers. Spend through `proxy.py` was $2.42: $0.10 for single answers and $2.32 for the suites. No check carried a provider-failure label, and the recordings hold no API key.

**Result:**

- **Eleventh set (first run):** on five new contracts, journeys passed 46.1% across models. Luna reached 57.9%, and the default model 50.4%.
- **Tenth set, after its diagnosis:** 49.3% across models, up from 29.0%. The default model rose from 19.6% to 51.4%.
- **Default model, the nine earlier blind sets together:** 74.2%, against 74.8% last round. On the second set it reached 93.9%, and on held-out 89.3%.
- **Lightning:** rose from 59.4% to 60.8% on those sets.
- **Single answers:** the default model got 162/162 right first time.

## Journeys

| Suite | All models | Default | Luna | Lightning | Previous (all) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Eleventh blind (first run) | 387/840 (46.1%) | 141/280 | 162/280 | 84/280 | — |
| Tenth (diagnosed) | 414/840 (49.3%) | 144/280 | 163/280 | 107/280 | 244/840 (29.0%) |
| Ninth blind | 331/840 (39.4%) | 116/280 | 137/280 | 78/280 | 330/840 (39.3%) |
| Eighth blind | 378/840 (45.0%) | 122/280 | 152/280 | 104/280 | 391/840 (46.5%) |
| Seventh blind | 541/840 (64.4%) | 187/280 | 204/280 | 150/280 | 546/840 (65.0%) |
| Sixth blind | 607/840 (72.3%) | 223/280 | 211/280 | 173/280 | 586/840 (69.8%) |
| Fifth blind | 668/828 (80.7%) | 247/276 | 230/276 | 191/276 | 667/828 (80.6%) |
| Fourth blind | 694/828 (83.8%) | 237/276 | 239/276 | 218/276 | 697/828 (84.2%) |
| Third blind | 681/840 (81.1%) | 236/280 | 238/280 | 207/280 | 679/840 (80.8%) |
| Second blind | 728/840 (86.7%) | 263/280 | 244/280 | 221/280 | 749/840 (89.2%) |
| First blind | 629/828 (76.0%) | 231/276 | 215/276 | 183/276 | 624/828 (75.4%) |
| Held-out | 767/840 (91.3%) | 250/280 | 275/280 | 242/280 | 761/840 (90.6%) |
| Tuned | 326/344 (94.8%) | 112/114 | 104/114 | 110/116 | 318/347 (91.6%) |

The tenth set's changes were made from its own diagnosis, so its number is a tuned one now, not a blind one.

| Eleventh blind journey | Passed |
| --- | ---: |
| Mux live streams (`disable`/`enable`, stream key reset, playback ids) | 123/168 |
| HCP Terraform workspaces (JSON:API, lock, unique variable keys, safe delete) | 82/168 |
| BigCommerce products and orders (V2 and V3, `status_id`, archiving, `page`/`limit`) | 81/168 |
| Postmark templates and message streams (aliases, `ErrorCode`, archiving) | 77/168 |
| Help Scout conversations (HAL, `Resource-ID`, threads, JSON Patch) | 24/168 |

27 checks failed in every run. They include:
- 12 of the 14 Help Scout checks;
- uniqueness refusals by `ErrorCode` in Postmark;
- templates read and edited by alias;
- BigCommerce archived orders, the empty list's 204, and `page`/`limit` paging;
- Terraform's second page and a sensitive variable's null value;
- deleted records found again in Mux, Postmark and Terraform.

The set is still blind at publication. These failures are diagnosed next, and once they are, this set stops being blind.

## Single answers

162 calls per model on all 18 contracts, default settings.

| Model | First try | Fallback | p50 |
| --- | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 162/162 (100%) | 0 | 2.1 s |
| `openai/gpt-6-luna` | 138/162 (85.2%) | 2 | 3.0 s |
| `nvidia/nemotron-3.5-lightning` | 143/162 (88.3%) | 0 | 1.4 s |

## Other suites

Consistency passed 24/24 on the default model and on Luna, and 21/24 on Lightning.

## Limits

- 4 runs per check per model (12 across models) is small.
- SSE and import did not run, and blind judging was not re-run.
- All contracts are hand-written subsets of real APIs.

`server.jsonl` holds each journey and suite check, and `engine/` holds the single-answer reports.
