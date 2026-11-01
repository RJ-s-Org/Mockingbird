# Identity from the contract, 2026-11-01

Measures two import rules on the blind journeys: a member named for its type is the identity of a route with an id-like parameter (`user_id` on `/users/{id}`), and a lookup whose listed items all share a type's schema reads as that type. 3 models, 4 repetitions, blind suite only. Spend was $0.20 through `proxy.py`.

**Result:** blind journeys rose from 55.7% to 65.1%, and on the default model from 61.4% to 70.0%. Almost all of it is Auth0. The tuned and held-out suites were not run, to save budget.

| Journey | Before | Now |
| --- | ---: | ---: |
| Auth0 | 80/168 | 133/168 |
| Slack | 68/168 | 74/168 |
| Spotify | 53/168 | 74/168 |
| HubSpot | 142/168 | 142/168 |
| Discord | 125/168 | 124/168 |
| All | 468/840 | 547/840 |

| Model | Before | Now |
| --- | ---: | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 172/280 | 196/280 |
| `openai/gpt-6-luna` | 170/280 | 197/280 |
| `nvidia/nemotron-3.5-lightning` | 126/280 | 154/280 |

Auth0's gains are the rules at work: `auth0.read matches the created user`, `auth0.search finds the user as updated` and `auth0.lookup by email finds the user` went from 0/12 to 12/12. Slack and Spotify share no route with these rules, so their rises are probably noise, which also shows how far one run can move.

`auth0.deleted user is gone` stays at 0/12 although the read now answers 404, so what fails there is the check's other condition.

## Limits

- **The binary was not the final code.** This run also held a runtime identity learner, removed after review because it could adopt a colliding value such as `count: 2` as an identity. The review found that no example contract reaches it, so it should not have moved these numbers. The type-name and lookup rules were also tightened afterwards, and that was not measured.
- 10 of the 12 routes found without identity still have none: Square 2 and Zendesk 6 (enveloped answers), SendGrid 1 (an array) and Twilio's lookup (no create). A contract can name its identity with `x-mockingbird-resource`.
- These contracts are no longer blind, so this measures progress rather than what a new contract would get.

`server.jsonl` holds each check, and its run recorded no API key.
