# Contract-driven action routes, 2026-11-01

Measures [action-route detection without spelling rules](../../import.md), where a model decides whether an action answering its parent's schema changed the parent or made a new record. 3 models, and spend was about $0.60 across four runs. The last run cost $0.18. It predates two small fixes for id collisions, which do not touch ordinary answers.

**Result:** on the default model and Luna, replies and copies now make new records and closes keep their id. Lightning mostly does not. Journey scores are flat, so this work prevents damage and removes a spelling rule, but does not raise the held-out number.

## Same-schema actions

A small contract, [`probe/shapes.openapi.yaml`](probe/shapes.openapi.yaml), with three routes answering their parent's schema and 201. 8 tries per model. Reply and copy were `Create` under the old spelling rule, so a parent could not be overwritten; a singular tail was an update.

| Check | Default | Luna | Lightning |
| --- | ---: | ---: | ---: |
| Reply gets a new id | 8/8 | 8/8 | 0/8 |
| Parent keeps its body | 8/8 | 8/8 | 0/8 |
| Copy gets a new id | 8/8 | 8/8 | 0/8 |
| Close keeps its id | 8/8 | 8/8 | 8/8 |
| Close moves the state, and a read shows it | 8/8 | 8/8 | 8/8 |

Lightning scored 2/8 on replies in one earlier run. It answers with the parent's id whatever the prompt says, so on such a model a reply overwrites its parent. A contract can correct this with `x-mockingbird-resource` (`action: false` and a `type`).

## Journeys

| Suite | Baseline | This branch, last run | Range over the branch's runs |
| --- | ---: | ---: | ---: |
| Held-out | 627/840 (74.7%) | 632/840 (75.2%) | 73.6–76.7% |
| Tuned | 164/171 at 2 repetitions (95.9%) | 322/344 (93.6%) | 93.6–97.4% |

The spread comes mostly from provider timeouts, so these are one result. `stripe.confirm moves the status on` ranged from 9/12 to 12/12 and `jira.read keeps the summary` stayed at 0/12. `server.jsonl` holds the last run.
