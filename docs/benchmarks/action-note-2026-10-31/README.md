# Action note, 2026-10-31

Follow-up to the one bar [simulated-world journeys](../journeys-2026-10-30/README.md) missed: a confirm moved its status on only 3 of 6 times. No context beyond each contract. Spend was $0.55 through `proxy.py`.

**Result:** the bar is met. A confirm moves its status on 17 of 18 times, up from 13 of 18 with the old prompt at the same repetitions, and the bar was 5 of 6. Held-out journeys rose from 627 to 635 of 840. The published journeys fell from 497/517 to 488/521, on routes whose prompts did not change.

## The change

Failing answers showed models reasoning from what was missing: shown `payment_method: null`, Luna kept `requires_payment_method` and invented a `last_payment_error`. For a 2xx on an action route, the prompt now ends:

> \`confirm\` succeeded; it was not refused. If \`confirm\` needs something the resource lacks, that is now there: fill it in.

This replaces "the resource has moved to the state that follows `confirm`, whatever it needed before", and the words "(an action moves a status on)" are gone. The prompt no longer says anything about a status, so it assumes none: a `preview` or `validate` is not told something moved, and a resource with no status is not told it has one. The verb comes from the route's path. A scenario's instructions override the note.

## Results

Each comparison ran 3 models with the old and new prompts. The published journeys and the invoicing contract had 6 repetitions each. The held-out journeys had 4, and their baseline is [its own run](../heldout-baseline-2026-10-31/README.md).

| Suite | Before | Now |
| --- | ---: | ---: |
| `stripe.confirm moves the status on` | 13/18 | **17/18** |
| Published journeys | 497/517 (96.1%) | 488/521 (93.7%) |
| Held-out journeys | 627/840 (74.7%) | 635/840 (75.6%) |
| Invoicing contract | 142/162 | 150/162 |

**Published journeys.** Most of the fall is in `travel`, and in Lightning most of all. `searchFlights` and `createBooking` send the same prompt template with both versions, so this note cannot have caused it. Lightning's answers failed the schema more often on those routes (8 travel checks judged a faker fallback, up from 2), and its provider mix differed between runs. Treat this as noise, but it was not re-measured.

**Held-out journeys.** Action checks that were strong stayed so, except one: `paypal.capture completes the order` fell from 12/12 to 10/12. Both misses are Luna writing malformed JSON in a long answer. Replaying that prompt 40 times each gave 0 malformed before and 1 now, so the note is not clearly the cause, but it did make these answers about 30% longer (609 to 796 tokens).

**Actions that should change nothing.** [`invoicing.openapi.yaml`](../contracts/invoicing.openapi.yaml) was written for this: `pay` moves a status, `remind`, `preview` and `validate` do not, and `pin` acts on a resource with no status.

| Check | Before | Now |
| --- | ---: | ---: |
| `pay moves the status on` | 18/18 | 18/18 |
| `preview keeps the status` | 11/18 | 14/18 |
| `validate keeps the status` | 11/18 | 14/18 |
| `remind keeps the status` | 12/18 | 14/18 |
| `pin sets the flag` | 18/18 | 18/18 |
| `read shows the pinned note` | 18/18 | 18/18 |

The default model is 3/6 on `preview`, `validate` and `remind` both before and after, so this is not solved.

## What was tried

To compare wordings without a rebuild, prompts recorded from the journeys were replayed with their text changed at `max_tokens` 2,500. The `preview` prompts were made from recorded `remind` prompts. Counts are out of 20, and answers lost upstream count as misses.

| Variant, Lightning | Confirm | Pay | Preview keeps | Remind keeps |
| --- | ---: | ---: | ---: | ---: |
| An earlier note ("the state a finished `confirm` leaves it in, not a status that still waits") | 20 | 20 | **3** | 14 |
| **Says nothing of state (shipped)** | 19 | 20 | 17 | 13 |
| The same, listing enum values and their value before | 15 | 20 | 16 | 15 |
| The same, with "with no error" | 16 | 19 | 15 | 15 |

Luna and the default model passed at least 18 of 20 in every cell here, and their only misses were upstream errors.

- **The earlier note pushed status.** It scored best on confirm, yet moved `preview` off its status 17 times in 20.
- **Listing the enum's values did not help.** It cost Lightning on confirm and gained little on `remind`, so no code reads the schema for them.

An earlier, smaller replay with 10 calls per cell also tried other positions. In the system message, an earlier wording did worst on confirm (0/9 on Luna, 3/10 on Lightning). In the prompt's first paragraph, a note like the earlier one above matched the end of the prompt on confirm and did worse on `remind` (3/10 against 7/10), which is why the note goes last.

## Limits

- The wording was chosen on Stripe and invoicing, and the invoicing contract was written by the author of the note.
- The held-out journeys have now been used once to check this, and did not drive the wording.
- Held-out `zendesk.update changes status and priority` and `notion.update changes the properties` are 1/12 and 0/12, and this note does not reach them. Models answer the new values, then `reflect` in `crates/mb-engine/src/lib.rs` looks to overlay the stored ones on `PUT` and `PATCH`.
- A few Luna calls failed upstream with a 400 saying the prompt was flagged as potentially violating usage policy. The cause was not investigated.
