# Write memory, 2026-10-31

Measures [write memory](../../concepts.md#sessions-and-state), where a mock keeps what a write's answer did not show and shows it to the next generation of that resource. 3 models, 4 repetitions, no context beyond each contract. Five runs cost about $1 through `proxy.py`, and this page reports the last one.

**Result:** journey checks on the five contracts first written as a blind test rose from 74.7% to 84.3%, and the default model from 76.4% to 86.8%. The tuned journeys stayed at 98%. Lists and searches are still the largest gap, so this is not yet the 99% we want.

## Journeys

| Suite | Baseline | Now |
| --- | ---: | ---: |
| Held-out, all models | 627/840 (74.7%) | 708/840 (84.3%) |
| Tuned, all models | 164/171 at 2 repetitions (95.9%) | 338/345 (98.0%) |

| Model | Held-out baseline | Held-out now | Tuned now |
| --- | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 214/280 | 243/280 | 114/115 |
| `openai/gpt-6-luna` | 215/280 | 243/280 | 114/115 |
| `nvidia/nemotron-3.5-lightning` | 198/280 | 222/280 | 110/115 |

Across the branch's runs on the reviewed code, held-out ranged from 81.7% to 84.9% and tuned from 94.8% to 98.0%, mostly from Lightning falling back after timeouts or invalid answers. Treat them as one result. `server.jsonl` holds the last run, and its recordings hold no API key.

Largest gains on held-out checks, baseline to now:

| Check | Baseline | Now |
| --- | ---: | ---: |
| `jira.read keeps the summary` | 0/12 | 12/12 |
| `jira.read keeps the description` | 0/12 | 12/12 |
| `jira.read keeps the labels` | 0/12 | 12/12 |
| `jira.read keeps the issue after the transition` | 0/12 | 12/12 |
| `zendesk.update changes status and priority` | 0/12 | 12/12 |
| `jira.read keeps project, type and priority` | 4/12 | 12/12 |
| `jira.read shows the new status` | 1/12 | 8/12 |
| `jira.search finds the issue in its new status` | 2/12 | 7/12 |

Checks that fell by more than one:

| Check | Baseline | Now | Note |
| --- | ---: | ---: | --- |
| `zendesk.comment list holds the reply` | 12/12 | 8/12 | 12/12 and 10/12 in earlier runs on this branch; 3 of the 4 misses are Lightning |
| `square.delayed payment waits for completion` | 10/12 | 7/12 | About the same in every run on this branch, mostly the default model |
| `square.read keeps the original amount` | 12/12 | 10/12 | |
| `zendesk.reply keeps the ticket` | 12/12 | 10/12 | One provider timeout and one prompt the provider refused, both on fallback |

`square.delayed payment waits for completion` falls on the first generated answer, before any memory exists, so the prompt's new block cannot have caused it. It stays open.

## Still failing

| Check | Now | What it needs |
| --- | ---: | --- |
| `notion.query finds the page in its new status` | 0/12 | A list or search that sees stored records |
| `notion.search finds the page by title` | 0/12 | Same |
| `square.list shows the completed payment` | 0/12 | Same |
| `notion.update changes the properties` | 0/12 | An update answer shaped by a free-form `properties` map |
| `square.read shows the refund` | 0/12 | An effect on a payment that a refund names |
| `zendesk.comment list holds both comments in order` | 3/12 | A list that sees stored records |
| `paypal.read shows the completed capture` | 5/12 | An effect on an order that a capture names |

## Calls

LLM calls per step for create, read, update, read, delete, read, cumulative, from a scripted model against `origin/main`:

| Contract | Main | Now |
| --- | ---: | ---: |
| `PUT` answers 200 with the read's schema | 1, 1, 2, 2, 2, 3 | 1, 1, 2, 2, 2, 3 |
| `PUT` 204, valid body | 1, 1, 1, 1, 1, 2 | 1, 1, 1, 1, 1, 2 |
| `PUT` 204, valid body with `password` | 1, 1, 1, 1, 1, 2 | 1, 1, 1, 1, 1, 2 |
| `PUT` 204, partial body | 1, 1, 1, 2, 2, 3 | 1, 1, 1, 1, 1, 2 |
| `PUT` 204, valid body with an undeclared member | 1, 1, 1, 1, 1, 2 | 1, 1, 1, 2, 2, 3 |

## Limits

- **These contracts are no longer blind.** PayPal, Jira, Zendesk, Notion and Square were written before any fix, but this work looked at their failures, so 84.3% flatters what a new contract would get. A fresh blind set has to give the headline number.
- 12 runs per check and 3 models is small.
- Single-answer first try and blind judging were not re-run on this code.
- The Redis and Postgres paths were not run with a real model.
