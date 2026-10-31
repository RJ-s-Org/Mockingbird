# Held-out journey baseline, 2026-10-31

How the simulated world generalises. The `journeys` suite drove the fixes in [its run](../journeys-2026-10-30/README.md), which scored 96%, so its score flatters the engine. These five journeys were written from their contracts by an author who read no engine code, before any fix aimed at them: PayPal orders, Jira, Zendesk, Notion and Square. 14 checks each, 3 models, 4 repetitions. Spend was $0.15, plus $0.07 on a run discarded after its proxy was killed by mistake.

**627 of 840 checks passed (74.7%), against 96% on the suite that drove the fixes.**

| Journey | Passed |
| --- | ---: |
| PayPal orders | 150/168 (89%) |
| Zendesk | 139/168 (83%) |
| Square | 131/168 (78%) |
| Notion | 116/168 (69%) |
| Jira | 91/168 (54%) |

| Model | Passed |
| --- | ---: |
| `openai/gpt-6-luna` | 215/280 |
| `deepseek/deepseek-v4.1-flash-1017` | 214/280 |
| `nvidia/nemotron-3.5-lightning` | 198/280 |

Rows are in `server.jsonl`. 16 answers fell back to the faker.

## What fails

- **A create that answers little:** Jira's create answers `id`, `key` and `self`, so a later read invents the summary, description and labels (0/12 each).
- **A write that answers nothing:** Jira's transition answers 204, so its read never shows the new status (1/12).
- **Updates that do not apply:** `zendesk.update changes status and priority` and `notion.update changes the properties` are 0/12.
- **Lists and searches:** `square.list shows the completed payment`, `notion.search finds the page by title` and `notion.query finds the page in its new status` are 0/12.
- **Consequences on another resource:** `square.read shows the refund` is 0/12.

Once fixes target these journeys, they stop being held out. A fresh set should give the final number.
