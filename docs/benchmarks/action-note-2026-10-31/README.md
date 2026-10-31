# Action note, 2026-10-31

Follow-up to the one bar [simulated-world journeys](../journeys-2026-10-30/README.md) missed: a confirm moved its status on only 3 of 6 times. No context beyond each contract. Spend was $0.13 through `proxy.py`.

**Result:** the bar is met, 6/6 against a bar of 5/6. The same note gave 12/12 captures on a second contract and 12/12 pays on a third. An action that should leave the status alone is no better on the default model: `remind` is 0/4 before and after.

## Why models missed

The failing answers showed two habits:

- **Reasoning from what was missing.** Luna saw `payment_method: null`, kept `requires_payment_method` and invented a `last_payment_error`.
- **Taking the next value in the enum.** Models answered `requires_confirmation` after a confirm, and `awaiting_payment` after paying a draft invoice.

The prompt said the resource had moved to "the state that follows `confirm`, whatever it needed before". It now ends with this, for any 2xx on an action route:

> \`confirm\` succeeded, so whatever the resource lacked for \`confirm\` is now there (fill it in, with no error), and the answer shows the state a finished \`confirm\` leaves it in, not a status that still waits for \`confirm\` or for something before it.

The words "(an action moves a status on)" are gone from the prompt. A scenario's instructions override the note. The verb comes from the route's path, so nothing here names a vendor or adds config.

## The bar

Three published journeys, 3 models, 2 repetitions, as in the earlier run. `server.jsonl` holds each check.

| Measure | Pass | Before | Now | |
| --- | --- | ---: | ---: | --- |
| `stripe.confirm moves the status on` | At least 5/6 | 3/6 | **6/6** | Met |
| Journey checks passed | No fall from 96% | 164/171 | 169/174 (97%) | Met |

| Model | Now |
| --- | ---: |
| `deepseek/deepseek-v4.1-flash-1017` (default) | 58/58 |
| `openai/gpt-6-luna` | 56/58 |
| `nvidia/nemotron-3.5-lightning` | 55/58 |

The only other checks that changed are `stripe.a succeeded intent received its amount` (3/3 to 6/6, which only applies to a succeeded intent) and `github.repo counts the new issue` (4/6 to 3/6, which this note does not reach).

## Overfitting

The note was tuned on Stripe's confirm, so it was run on two more contracts: PayPal's order capture, and a new contract written for this check ([`invoicing.openapi.yaml`](../contracts/invoicing.openapi.yaml)) with a `pay` route that moves the status and a `remind` route whose summary says the invoice stays as it is. `actions-before.jsonl` is the code before this change and `actions-after.jsonl` the code with it.

| Check | Luna and Lightning, before | Luna and Lightning, now | Default, before | Default, now |
| --- | ---: | ---: | ---: | ---: |
| `stripe.confirm moves the status on` | 7/10 | 8/8 | not run | 4/4 |
| `paypal.capture moves the status on` | 10/10 | 8/8 | not run | 4/4 |
| `invoices.pay moves the status on` | 9/10 | 8/8 | 4/4 | 4/4 |
| `invoices.remind keeps the status` | 7/10 | 6/8 | 0/4 | 0/4 |

Before, confirm passed 7/10 here against 3/6 on the published journeys. So part of the published gap was noise, and the real starting point is probably between the two.

**`remind` is not solved.** Every miss starts from `draft` and answers `awaiting_payment`, on both versions. Reminding someone about a draft invoice is odd, so the model's answer is defensible, but the contract's summary says otherwise. The new note neither causes this nor fixes it.

## What was tried

To compare wordings without a rebuild, the prompts recorded for each action were replayed with their text changed, 10 calls per cell at `max_tokens` 2,500. Each count leaves out answers that could not be read, such as upstream errors. Each cell draws on a few recorded prompts, so it does not replace the journeys.

| Variant | Confirm, Luna | Confirm, Lightning | Remind, Luna | Remind, Lightning |
| --- | ---: | ---: | ---: | ---: |
| Prompt before this change | 4/9 | 9/9 | 9/9 | 3/9 |
| Rule out waiting statuses, in the first paragraph | 2/8 | 7/10 | 10/10 | 6/10 |
| The same, at the end of the prompt | 5/10 | 8/10 | 10/10 | 9/10 |
| The same, in the system message | 0/9 | 3/10 | 10/10 | 7/10 |
| Also supply what it lacked, in the first paragraph | 10/10 | 10/10 | 10/10 | 3/10 |
| **Also supply what it lacked, at the end (shipped)** | 10/10 | 10/10 | 10/10 | 7/10 |
| The same, naming the status's values and its value before | 10/10 | 10/10 | 10/10 | 7/10 |

- **Supplying what was lacking is what fixed it.** Ruling out waiting statuses alone did not help Luna.
- **The system message did worst** for confirm, so the note was not moved there. Only the earlier wording was tried in that position.
- **Naming the enum's values added nothing**, so no code reads the schema for them.
- **The end of the prompt beat the first paragraph** on `remind`, which is why the note goes last.

## Limits

- 6 runs on the bar is small, and 12/12 and 10/10 still leave room for misses on a contract not tried here.
- The note was chosen on these contracts, and the invoicing contract was written by the author of the note.
- A 2xx from an action that should report a failure in its body now needs a scenario to say so. Without one, the mock answers the action as done.
- Some Luna calls failed upstream with a 400 saying the prompt was flagged as potentially violating usage policy (6 in the first replay, none in the second). The cause was not investigated.
