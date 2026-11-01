# Blind journeys on final code, 2026-11-01

Measures `main` after write memory and the [blind baseline](../blind-baseline-2026-11-01/README.md)'s fixes, on five contracts no fix looked at, with journeys written by an author who read no engine code, docs or results. 3 models, 4 repetitions, no context beyond each contract. Spend was $0.33 through `proxy.py`, of which single answers were $0.06.

**Result:** 688/840 checks passed (81.9%), and 241/280 (86.1%) on the default model. First try held at 99.4%. This is the number to quote for a new contract, and it is still short of 99%.

| Model | Passed |
| --- | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 241/280 (86.1%) |
| `openai/gpt-6-luna` | 235/280 (83.9%) |
| `nvidia/nemotron-3.5-lightning` | 212/280 (75.7%) |

| Journey | Passed |
| --- | ---: |
| Twilio | 149/168 |
| PayPal Payments | 143/168 |
| OpenWeatherMap | 136/168 |
| SendGrid | 132/168 |
| Plaid | 128/168 |

## This is not a jump from 44% to 82%

The first blind suite stands at 603/840 (71.8%), last measured before the final secret fixes and not re-run. The second suite is easier in ways that have nothing to do with the fixes:

- Its author was told to avoid gates that skip a journey and to assert only what a contract promises, so fewer checks fail for the suite's own reasons.
- Its contracts offer fewer writes. OpenWeatherMap has none, so 12 of its 14 checks need little memory.
- No baseline exists for it, so its gain was not measured.

The fairest reading is that a new contract now scores somewhere around 70–85%, depending on how much of it is writes and state.

## Other suites, default model only

| Suite | Passed |
| --- | ---: |
| Tuned | 110/116 |
| Held-out | 258/280 (92.1%) |

All 6 tuned failures are one travel repetition, in which an offer search timed out.

## Single answers

162 calls on the default model across all 18 contracts: first try 161/162 (99.4%), 1 repaired, none fell back, p50 1.9 s, p95 6.5 s. These match the earlier [journeys run](../journeys-2026-10-30/README.md), so the world work did not cost first try. Luna and Lightning were not re-run.

## What still fails

| Check | Passed | |
| --- | ---: | --- |
| `plaid.item keeps its webhook` | 0/12 | |
| `plaid.accounts name their item` | 0/12 | |
| `paypalpayments.a fully refunded capture refuses another` | 0/12 | A rule the contract implies rather than states |
| `plaid.item reads back as exchanged` | 1/12 | |
| `sendgrid.removed address has no bounce` | 3/12 | |
| `sendgrid.bounce read keeps the listed bounce` | 4/12 | |
| `openweathermap.unknown place matches nothing` | 4/12 | |
| `paypalpayments.capture shows the full refund` | 5/12 | |
| `paypalpayments.capture shows the partial refund` | 6/12 | |
| `twilio.message is queued` | 6/12 | |

These have not been diagnosed, so that no fix tunes against this set by accident. Some checks may be stricter than a client needs.

## Limits

- The suite has run once. 12 runs per check and 3 models is small.
- Blind judging of realism and coherence was not re-run on this code.
- The tuned and held-out suites ran on the default model only, to stay within budget.
- These contracts are Mockingbird's own example files. PayPal Payments is a sibling of a contract the held-out journeys used, and Twilio and SendGrid each had a route named in a review of identity gaps, though no fix targeted them.
- Redis, the webhook paths and protocols other than HTTP were not run with a real model.

`server.jsonl` holds each check, and its run recorded no API key.
