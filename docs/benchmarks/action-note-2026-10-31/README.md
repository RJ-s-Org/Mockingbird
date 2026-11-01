# Action note, 2026-10-31

Follow-up to the one bar [simulated-world journeys](../journeys-2026-10-30/README.md) missed: a confirm moved its status on only 3 of 6 times. No context beyond each contract. Spend was $1.51 through `proxy.py`, most of it comparing wordings.

**Result:** the bar is met, but less of that is this change than it first looked. By the time this was ready, [the contract-driven action routes](https://github.com/RJ-s-Org/Mockingbird/pull/170) had taken confirm to 16/18. This change takes it to 17/18 in the journeys and from 64/71 to 71/71 in replays. Its larger effect is that prompts stop assuming a status: actions that should change nothing rose from 42/54 to 47/54.

## The change

Failing answers showed models reasoning from what was missing: shown `payment_method: null`, Luna kept `requires_payment_method` and invented a `last_payment_error`. When an action route answers with its own resource, the prompt now ends:

> \`confirm\` succeeded. If a missing member would have stopped it, that member is now there: fill it in.

Three things are gone:

- "(an action moves a status on)".
- "the resource has moved to the state that follows `confirm`, whatever it needed before".
- "must" in what an action answering something else owes its target. That now reads "they include what `validate` changes on it, if it changes anything".

Nothing in these prompts names a status, so a `preview`, a `validate` or a resource with no status is not told something moved. An action answering something else gets no note. The verb comes from the route's path, and a scenario's instructions override the note.

## Results

Compared with main at `ce95046`, 3 models. Published journeys and invoicing had 6 repetitions each, held-out journeys 4. Files ending `-before` are main.

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

## What was tried

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

## Limits

- The wording was chosen on Stripe and invoicing, and the invoicing contract was written by the author of the note.
- `paypal.confirm moves the status on` fell from 11/12 to 9/12, all on the default model, which answered `CREATED`. Replaying that prompt 30 times each, the default model kept `CREATED` 16 times without the note and 18 with it, so this is a weakness main shares.
- `paypal.capture completes the order` was 11/12, Luna answering malformed JSON. An earlier wording made Luna's capture answers about 30% longer. This was not re-measured for the shipped wording.
- Effects are not checked against the resource's schema, so a model can store `status: "validated"`, which the contract's enum does not allow.
- Held-out `zendesk.update changes status and priority` and `notion.update changes the properties` are not reached by this note. A separate session is working on them.
- A few Luna calls failed upstream with a 400 saying the prompt was flagged as potentially violating usage policy. The cause was not investigated.
