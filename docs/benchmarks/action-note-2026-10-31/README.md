# Action prompts, 2026-10-31

Follow-up to the one bar [simulated-world journeys](../journeys-2026-10-30/README.md) missed: a confirm moved its status on only 3 of 6 times. No context beyond each contract. Spend was $2.26 through `proxy.py`, most of it comparing wordings.

**Result:** the bar is met on main without this change. Action routes, write memory and the later world fixes took Stripe confirm to 115/120 in replays. This change does not add a note for it, because on the final main a note cost more than it gave. What it does is stop prompts telling actions that a status moves. On main, every model told a `validate` succeeded invents `status: "validated"`, which the contract does not allow, in 87 of 87 replays. With this change, 49 of 90 keep the status.

## The change

Three phrases are gone from the prompt:

- "(an action moves a status on)", from every write shown its target.
- "This is a successful `validate`: the call succeeded, so the resource has moved to the state that follows `validate`, whatever it needed before", for an action that does not answer its own resource.
- "must", in what such an action owes its target. That now reads "they include what `validate` changes on it, if it changes anything".

Nothing in these prompts names a status now, so a `preview`, a `validate` or a resource with no status is not told something moved. #170's sentence for an action that answers its own resource is unchanged.

## On the final main

Prompts were captured from the merged code through `fake_llm.py` at no cost, then replayed to the 3 models with main's wording put back, 30 calls per model.

| Route | Main | This change |
| --- | ---: | ---: |
| Invoice `validate` keeps the status | 0/87 | **49/90** |
| Square `complete` moves the status on | 88/89 | 88/90 |
| Zendesk update applies the request | 39/40 | 43/43 |
| Notion update applies the request | 89/90 | 88/88 |

## The note that was dropped

This branch first ended action prompts with a note: "`confirm` succeeded. If a missing member would have stopped it, that member is now there: fill it in." It helped before the later world fixes merged. On prompts captured from the final main, 40 calls per model, it no longer paid:

| Route | Without the note | With it |
| --- | ---: | ---: |
| Stripe confirm | 115/120 | 119/120 |
| Invoice `remind` keeps the status | 90/117 | 76/114 |
| Invoice `preview` keeps the status | 85/120 | 83/120 |
| PayPal confirm | 69/116 | 66/117 |

Its gain was 4 Lightning confirms. It cost the default model 10 of 40 on `remind`, and Luna already confirms 40/40 without it.

## Earlier measurements

These were taken on earlier main, while the note was in, and are kept for what they show about wording:

### Results

Compared with main at `ce95046`, before write memory merged, 3 models. Published journeys and invoicing had 6 repetitions each, held-out journeys 4. Files ending `-before` are main.

| Suite | Main | This change |
| --- | ---: | ---: |
| `stripe.confirm moves the status on` | 16/18 | **17/18** |
| Published journeys | 476/520 (91.5%) | 487/521 (93.5%) |
| Held-out journeys | 637/840 (75.8%) | 631/840 (75.1%) |
| Invoicing contract | 150/162 | 155/162 |

**Held-out journeys did not improve.** This is within what the suite does by itself: main scored 627 and 637 on two runs, and an earlier wording of this note scored 648. The largest moves are not on routes this note reaches. Faker fallbacks rose from 14 to 26, nearly all Notion timeouts and schema failures on Lightning and Luna, on calls that get no note.

**Published journeys vary too.** Main scored 497/517 on code before #170 and 476/520 after, and most of that swing is Lightning failing the schema on `travel`, whose search and create prompts this does not change.

**Actions that should change nothing.** [`invoicing.openapi.yaml`](../contracts/invoicing.openapi.yaml) was written for this: `pay` moves a status, `remind`, `preview` and `validate` do not, and `pin` acts on a resource with no status.

| Check | Main | This change |
| --- | ---: | ---: |
| `pay moves the status on` | 18/18 | 18/18 |
| `remind keeps the status` | 14/18 | 17/18 |
| `preview keeps the status` | 14/18 | 15/18 |
| `validate keeps the status` | 14/18 | 15/18 |
| `pin sets the flag` | 18/18 | 18/18 |

All the misses are Lightning, which is still 3/6 on `preview` and `validate`.

### What was tried

To compare wordings without rebuilding, prompts recorded from the journeys were replayed with their text changed, 24 calls per model at `max_tokens` 2,500, and answers lost upstream left out. All on prompts that already include #170.

| Note at the end of the prompt | Confirm | Remind keeps | Preview keeps | Validate keeps |
| --- | ---: | ---: | ---: | ---: |
| None | 64/71 | 67/71 | 58/71 | 43/72 |
| "it was not refused. If it needs something the resource lacks, that is now there: fill it in" | 72/72 | 62/71 | 65/72 | 31/72 |
| "it was not refused" alone | 68/71 | 65/70 | 62/72 | 34/72 |
| **"If a missing member would have stopped it, that member is now there" (shipped)** | 71/71 | 65/70 | 61/72 | 34/71 |

- **The note is worth having on routes that answer the resource**, for confirm. On `validate`, which answers a report, every wording did harm. The default model went from 12/24 to 0/24 with the second wording, setting `status` to `validated`. So that route gets no note.
- **The shipped wording** matches the best on confirm, and pushes less on the default model's `remind` than the second wording (21/24 against 17/24). The differences between these wordings are small.
- **"must include" was its own push.** With no note, softening that line took `validate` from 48/71 to 63/68, and `cancel`, which does change its target, stayed at no misses (72/72 and 69/69).

Replays before #170, at 20 calls per model on Lightning:

- **A note about status** moved `preview` off its status 17 times in 20, against 3 with no mention of status.
- **Listing the status enum's values** cost confirm (15/20 against 19/20) and gained little on `remind`, so no code reads the schema for them.
- **The system message**, with an earlier wording, did worst on confirm (0/9 on Luna, 3/10 on Lightning).

### Does the note still earn its place?

Checked after [write memory](../write-memory-2026-10-31/README.md) merged, with replays and no new journey run. Prompts recorded from this branch before that merge were sent with and without the note, 40 calls per model and answers lost upstream left out. Write memory can add requests to these prompts, and these recordings do not hold any, so this was not measured on its exact prompts.

| Route | Without the note | With it |
| --- | ---: | ---: |
| Stripe confirm | 104/117 | **119/119** |
| Invoice preview keeps the status | 94/120 | 104/119 |
| PayPal capture | 114/114 | 118/118 |
| Invoice pay | 120/120 | 120/120 |
| Invoice remind keeps the status | 117/120 | 117/120 |
| PayPal confirm | 91/120 | 83/119 |

At that point it stayed, on a narrower case than it first looked; the final main reversed this (see above). The gain is on the weaker models: Luna's Stripe confirm goes from 30/39 to 40/40, and Lightning's `preview` from 14/40 to 25/40. **The default model does not gain overall:** Stripe confirm goes from 37/40 to 40/40, and PayPal confirm falls from 26/40 to 19/40, more often keeping `CREATED`. With the earlier replay (14/30 against 12/30) that fall is 40/70 against 31/70, which could still be chance, but its direction has held twice. If the default model matters most, this is the case for dropping the note and keeping only the wording removals.

## Limits

- The removals were measured by replaying prompts with the old wording put back, not by a journey run on the final main. Captured prompts come from a fake LLM's world, so their resources are less realistic than a real run's.
- `validate` still invents a status about half the time. Effects are not checked against the resource's schema, so `status: "validated"` is stored although the contract's enum does not allow it. Checking effects against the schema would close this, and is not done here.
- The invoicing contract was written by the author of this change.
- Some Luna calls failed upstream with a 400 saying the prompt was flagged as potentially violating usage policy. The cause was not investigated.
