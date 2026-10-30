# Journey baseline, 2026-10-30

Today's engine on the bench's `journeys` suite, before any simulated-world work: three journeys (Stripe, travel, GitHub), 3 models, 2 repetitions, through `proxy.py`. Spend was $0.03.

**94 of 174 checks passed (54%).** The pilot measured 98 of 174 (56%) with `json_object`, so schema output changes little here. Later work is measured against this run. Mean tokens per LLM call were 1,567 prompt and 597 completion, over 100 calls.

| Model | Checks passed |
| --- | ---: |
| `deepseek/deepseek-v4.1-flash-1017` | 32/58 |
| `openai/gpt-6-luna` | 32/58 |
| `nvidia/nemotron-3.5-lightning` | 30/58 |

Rows are in `server.jsonl`, one per check with `case` = `<journey>.<check>`.

## Checks that fail everywhere

These pass at most 1 of 6:

- **Actions:** `stripe.confirm keeps identity and amount`, `stripe.read shows the confirmed status`, `stripe.a succeeded intent received its amount`.
- **Listed entities:** `github.repo detail keeps the listed repo`, `github.repo keeps its identity`, `travel.offer detail keeps the listed price`, `travel.offer detail keeps the listed segments`.
- **References:** `stripe.refund uses the intent's currency`, `travel.booking costs what the offer did`, `travel.booking flies the offer's segments`, `travel.refund is in the booking's currency`.
- **Effects:** `github.repo counts the new issue`, `travel.cancel shows on the booking`.
