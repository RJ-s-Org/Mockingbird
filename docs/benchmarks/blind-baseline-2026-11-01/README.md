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

Of 467 failed checks, a review of the code and recorded prompts puts 405 on the engine, 50 on the models (filters, ordering, invalid output) and 12 on one wrong check. These counts are one reviewer's sort, and no fix has confirmed them. The checks each cause covers:

| Cause | Failed | What happens |
| --- | ---: | --- |
| The output budget drops `tracks` | 168, of which 156 skipped | `budget.rs` removes optional members the request does not name, heaviest first, and `type=track` does not match `tracks`. All 12 prompts offered `artists` alone, so the models never left it out |
| An update discards the answer | 48 | The stored copy overrides the answer, then a nested request object fails validation and is dropped. The model answered `opportunity`; the client got `lead` |
| Writes by reference reach nothing | 48 | `references_in` goes by field name, so `channel` and `thread_ts` link to nothing. Slack's messages are stored; what they refer to is not found |
| Deletes do not stick | 47 | The record is removed, but with `unknown_ids: generate` the next read generates a replacement and stores it |
| No record of the caller | 46 | Earlier answers such as `/users/@me` are not shown to later generations |
| Identity is missing | 36 | `/users/{id}` over a resource with `user_id` and no `id` gets none, so nothing is stored. 12 routes across `examples/apis` have the same gap |
| A lookup is its own type | 12 | `users-by-email` sees no stored users, though its items share their schema |
| Model weakness | 50 | `related()` already shows stored records, and models do not always filter them. So lists do see writes |
| Wrong check | 12 | `slack.reply names its thread and stays out of the channel`: the contract never says replies leave history |

Three more checks look stricter than a client needs: `slack.parent counts the reply`, `auth0.user holds the assigned role` and the Spotify gate wanting at least 3 tracks. Spotify's other 13 checks have never run.

Checks that passed every time are mostly first answers keeping what the request said: `auth0.user keeps the profile`, `discord.message keeps what was sent`, `slack.post returns a timestamp`.

## Limits

- **The checks have run only this once.** Written without seeing real answers, some may be wrong or stricter than a client needs, such as `discord.read matches the created message` 0/12, which also wants `pinned` and `edited_timestamp`. Nothing has been loosened to raise the score.
- **It stops being blind now.** Any fix that reads this page tunes against these contracts, so a final number needs another set.
- These contracts are Mockingbird's own hand-written examples, subsets of the real APIs. Their subsets leave gaps: Spotify has no create, update or delete, Slack no update or delete, Discord no update.
- 12 runs per check and 3 models is small.

`server.jsonl` holds each check, and its run recorded no API key.
