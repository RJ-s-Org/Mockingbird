# Blind journeys baseline, 2026-11-01

Measures a mock on five contracts no journey had tested, with journeys written by an author who read no engine code, past results or failure data. This is the first run, on `main` after [write memory](../write-memory-2026-10-31/README.md), before any fix looked at these failures. 3 models, 4 repetitions, no context beyond each contract. Spend was $0.13 through `proxy.py`.

**Result:** 373/840 checks passed (44.4%), and 128/280 (45.7%) on the default model. Tuned journeys are at 98% and the once-blind ones at 84%, so those figures say little about a new contract. This is the number to quote.

| Model | All five | Without Spotify |
| --- | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 128/280 | 128/224 |
| `openai/gpt-6-luna` | 132/280 | 132/224 |
| `nvidia/nemotron-3.5-lightning` | 113/280 | 113/224 |

| Journey | Passed |
| --- | ---: |
| HubSpot CRM | 114/168 |
| Discord | 105/168 |
| Auth0 | 80/168 |
| Slack | 74/168 |
| Spotify | 0/168 |

Spotify's first check stops its journey, so its other 13 checks were skipped on every run: `GET /v1/search?type=track,artist` answered without `tracks` 12 times out of 12. Its 0 says nothing about the later steps, which is why the table also shows the score without it.

## What fails

Read from the answers of the default model, and a first reading rather than a root-cause analysis:

| Pattern | Examples | Likely cause |
| --- | --- | --- |
| A read does not find what was created | `auth0.read matches the created user` 0/12 | `/users/{id}` over a resource with `user_id` and no `id` gets no identity, so the read generates someone else |
| A delete does not take | `auth0.deleted user is gone`, `discord.deleted message is gone`, `hubspot.archived contact is gone`, all 0/12 | Reads still answer 200 after a 204 |
| Lists and searches do not see writes | `auth0.search finds the user as updated`, `discord.list drops the deleted message` 1/12, `hubspot.search by the old stage misses the contact` 3/12 | Generated from the query alone |
| Filters are not applied | `slack.latest filter holds only the earlier message` 0/12, `discord.before filter holds only the earlier message` 4/12 | Same |
| RPC-style routes hold no state | `slack.parent counts the reply`, `slack.reaction shows on the message`, `slack.a repeated reaction is refused`, all 0/12 | `/reactions.add` and `/chat.postMessage` name no resource in their path |
| An optional page is left out | `spotify.search returned tracks` 0/12 | No answer included `tracks`, which the contract does not require |

Checks that passed every time are mostly first answers keeping what the request said: `auth0.user keeps the profile`, `discord.message keeps what was sent`, `slack.post returns a timestamp`.

## Limits

- **The checks have run only this once.** Written without seeing real answers, some may be wrong or stricter than a client needs, such as `discord.read matches the created message` 0/12, which also wants `pinned` and `edited_timestamp`. Nothing has been loosened to raise the score.
- **It stops being blind now.** Any fix that reads this page tunes against these contracts, so a final number needs another set.
- These contracts are Mockingbird's own hand-written examples, subsets of the real APIs. Their subsets leave gaps: Spotify has no create, update or delete, Slack no update or delete, Discord no update.
- 12 runs per check and 3 models is small.

`server.jsonl` holds each check, and its run recorded no API key.
