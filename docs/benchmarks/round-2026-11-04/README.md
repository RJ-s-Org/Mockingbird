# Cross-suite fixes, 2026-11-04

This round ran on `main` at cc4ffa1b. The changes since [the ninth set's first run](../blind9-2026-11-04/README.md):

- **Lists served from stored records:** a query value that isn't a filter no longer hides every stored record. A record's create scope decides whether it belongs to a scoped list. Stored records come before the model's own next page, and nothing is served twice (#231).
- **Sibling-route and bodyless writes:** a write one level under a parent reaches that parent's lists. A held child's delete tells its parent. A 2xx write with no response body gets one effects-only call, and a retry doesn't repeat it (#232).
- **Refusal bodies:** a refusal's error body takes the code the contract states. A repeated value is enforced only on a declared 409 that names the member, and a 2xx answer that is the stored record itself is never refused (#234).
- **Envelopes and order:**
  - an envelope answer to a write stores the member that is the record;
  - `allOf` merges in declaration order;
  - one write's records read back in answer order;
  - a request array no longer wipes the model's ids;
  - a record held as the one item of an envelope array gets the item's identity.

  All of these are in #236.
- **Header concurrency:** an `If-Match` entity tag tied to a server-managed member, or a body version the client is seen to track, is refused when stale, with the declared 412 or 409 (#237).
- **Correctness batch:**
  - member values are indexed on every state store;
  - compound owner parameters (`{x_id_or_slug}`) are matched;
  - a model's id that its schema keys apart is kept;
  - GraphQL deletes leave a marker;
  - a missing record answers a declared 410 when the read declares no 404.

  All of these are in #238.
- **Bench:**
  - checks that tested behaviour their contracts didn't state now test only what the contracts say (#229);
  - each model is pinned to one upstream provider, so runs are comparable (#233).

Every journey suite ran on 3 models, alongside consistency and single answers. Spend through `proxy.py` was $3.00: $0.14 for single answers and $2.87 for the suites. The recordings hold no API key.

**Result:**

- **Default model, all nine blind sets together:** 72.9%, up from 67.3%. Luna rose from 71.0% to 75.8%.
- **Single answers:** the default model got 162/162 right first time.
- **The ninth set is no longer blind.** Its failures were diagnosed for this round, so its rise from 30.5% to 36.1% is partly tuning.
- **Lightning fell from 57.7% to 51.3%.** That is not the code: pinned to Fireworks, its answers looped on repeated text until the 16,384-token limit in 258 calls, against 78 in the last run. See Limits.

## Journeys

| Suite | All models | Default | Luna | Lightning | Previous (all) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ninth blind | 303/840 (36.1%) | 101/280 | 137/280 | 65/280 | 256/840 (30.5%) |
| Eighth blind | 346/840 (41.2%) | 110/280 | 154/280 | 82/280 | 334/840 (39.8%) |
| Seventh blind | 508/840 (60.5%) | 178/280 | 205/280 | 125/280 | 462/840 (55.0%) |
| Sixth blind | 569/840 (67.7%) | 218/280 | 211/280 | 140/280 | 560/840 (66.7%) |
| Fifth blind | 622/828 (75.1%) | 248/276 | 219/276 | 155/276 | 663/828 (80.1%) |
| Fourth blind | 664/828 (80.2%) | 236/276 | 244/276 | 184/276 | 633/828 (76.4%) |
| Third blind | 652/840 (77.6%) | 238/280 | 238/280 | 176/280 | 657/840 (78.2%) |
| Second blind | 723/840 (86.1%) | 264/280 | 268/280 | 191/280 | 765/840 (91.1%) |
| First blind | 628/828 (75.8%) | 235/276 | 225/276 | 168/276 | 586/828 (70.8%) |
| Held-out | 713/840 (84.9%) | 249/280 | 269/280 | 195/280 | 724/840 (86.2%) |
| Tuned | 315/347 (90.8%) | 116/116 | 102/116 | 97/115 | 322/347 (92.8%) |

Default model by set: 84–94% on the first five sets, 78% on the sixth, 64% on the seventh, and 36–39% on the eighth and ninth. Luna leads on the last three sets. The falls on the second and fifth sets are all on Lightning: 191 against 241, and 155 against 200.

## Single answers

162 calls per model on all 18 contracts, default settings.

| Model | First try | Fallback | p50 |
| --- | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 162/162 (100%) | 0 | 1.6 s |
| `openai/gpt-6-luna` | 140/162 (86.4%) | 0 | 2.8 s |
| `nvidia/nemotron-3.5-lightning` | 121/162 (74.7%) | 3 | 1.6 s |

## Other suites

Consistency passed 24/24 on the default model and on Luna, and 22/24 on Lightning.

## Limits

- **Lightning on Fireworks:** 253 of its 258 cut-off answers repeat one token or line, such as `0_0_0_…`. The pin sent every call to Fireworks, where the last run spread them over five providers. The next fix stops such loops early and retries.
- **The default model's fallbacks rose from 9 to 44.** 35 of them are 429 rate limits from its single pinned provider, which the bench did not label as provider failures.
- **Sample size:** 12 runs per check per model is small. SSE and import did not run, and blind judging was not re-run.
- **Contracts:** all are hand-written subsets of real APIs.

`server.jsonl` holds each journey and suite check, and `engine/` holds the single-answer reports.
