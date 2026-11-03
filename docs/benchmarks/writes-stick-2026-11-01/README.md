# Writes that stick, 2026-11-01

Measures three engine fixes on all journey suites: the output budget keeps what a request asked for, an update takes the model's answer at the paths the request named, and a deleted record stays deleted. 3 models, 4 repetitions, no context beyond each contract. Three runs cost about $1.20 through `proxy.py`, and this page reports the last one.

**Result:** blind journeys rose from 44.4% to 55.7%, and on the default model from 45.7% to 61.4%. The default model passed every tuned check. The gain of 95 checks is a little over half of the 162–182 that the [root-cause review](../blind-baseline-2026-11-01/README.md) estimated for these causes, and 44% of blind checks still fail.

| Suite | Before | Now |
| --- | ---: | ---: |
| Blind | 373/840 (44.4%) | 468/840 (55.7%) |
| Held-out | 708/840 (84.3%) | 721/840 (85.8%) |
| Tuned | 338/345 (98.0%) | 326/347 (93.9%) |

| Model | Blind before | Blind now | Held-out now | Tuned now |
| --- | ---: | ---: | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 128/280 | 172/280 | 244/280 | 116/116 |
| `openai/gpt-6-luna` | 132/280 | 170/280 | 254/280 | 109/115 |
| `nvidia/nemotron-3.5-lightning` | 113/280 | 126/280 | 223/280 | 101/116 |

Tuned scores vary between runs, mostly on Lightning: 326/346 and 335/346 on earlier versions of this branch. `server.jsonl` holds the last run, which recorded no API key. It ran on the commit before the last, and that last commit was not measured: it removes matching a request through a wrapper, which real Zendesk never reached, and narrows when an effect is refused.

An earlier version let a model's update answer win as a whole. It scored 495/840 on blind and 715/840 on held-out, but review found it overwrote untouched fields, truncated stored arrays and let a non-resource answer corrupt a record. So part of what looks like a loss is that defect going away.

## Largest changes on blind checks

| Check | Before | Now |
| --- | ---: | ---: |
| `discord.deleted message is gone` | 0/12 | 12/12 |
| `hubspot.update changes the contact` | 0/12 | 11/12 |
| `discord.list drops the deleted message` | 1/12 | 10/12 |
| `hubspot.read shows the update and keeps the rest` | 3/12 | 12/12 |
| `spotify.search returned tracks` | 0/12 | 7/12 |
| `hubspot.search finds the contact in its new stage` | 6/12 | 12/12 |
| `discord.list holds both, newest first` | 12/12 | 8/12 |
| `slack.history lists newest first` | 9/12 | 6/12 |

The last two fell, and this run does not show why.

## Still failing

- **Auth0 80/168:** reads, updates, lookups and deletes all fail, because `/users/{id}` over a `user_id` has no identity. The next fix targets this.
- **Slack 68/168:** user info, thread replies, reply counts and reactions stay at 0/12, because writes by reference reach nothing.
- **Spotify 53/168:** its first check still fails 5 of 12 times, which skips the rest. Its item lists are 0/12.

## Limits

- These contracts are no longer blind, so this measures progress rather than what a new contract would get.
- 12 runs per check and 3 models is small.
- Single-answer first try and blind judging were not re-run on this code.
- Redis and the webhook paths were not run. The workspace tests also passed against real Postgres.
