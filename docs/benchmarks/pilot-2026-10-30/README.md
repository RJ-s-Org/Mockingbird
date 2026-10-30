# Simulated-world pilot, 2026-10-30

Tests the [simulated worlds spec](../../superpowers/specs/2026-10-30-simulated-world-design.md) with a throwaway prototype, [`scripts/llm-bench/pilot_world.py`](../../../scripts/llm-bench/pilot_world.py), and no context beyond each contract. Spend was $0.39, through `proxy.py` with a $1 hard stop.

**Result:** values-only generation fails its bar and is dropped. A world grown from traffic, with effects, passes.

## Single calls

Three models (`deepseek/deepseek-v4.1-flash-1017`, `openai/gpt-6-luna`, `nvidia/nemotron-3.5-lightning`), 6 contracts and 216 calls per approach, using the same requests and schemas as round 2. "Engine" is round 2's recorded answers.

| Approach | The LLM writes | Mean completion tokens | p50 ms | Realism change | Coherence change |
| --- | --- | ---: | ---: | ---: | ---: |
| Engine | The whole document | 488 | 3,139 | | |
| Numbers | Flat numbered values | 512 | 3,872 | +0.06 | -0.97 |
| Template | A template with IDs already set | 714 | 3,800 | +0.08 ± 0.33 | -0.58 ± 0.44 |
| Template with LLM IDs | A template, IDs included | 659 | 4,350 | +0.28 ± 0.33 | -0.56 ± 0.43 |

Changes are blind-judged scores on 36 paired answers (1 to 5 scale, 12 targets), against the engine in the same judging round; ± is a 95% interval. Numbers was judged in round 2 of judging, the other two in round 3. A fresh judge re-scored two batches of the first round within 0.17 points on average.

Every assembled answer was schema-valid, except for a few provider failures (at most 2 of 72 per model) that fell to the faker. The engine's share valid on its first answer, with no fix, was 47%, 89% and 29% for those models. The engine's final answers are mostly valid too, through patching and repair, so this is less of a gain than it looks.

The bar was realism and coherence at least level, with lower cost and latency. All three fail on coherence, and none is cheaper or faster. Output did not shrink: the 49% of bytes that are structure did not become token savings, probably because repeating the template costs as much and because the prototype fixes array sizes the output budget would otherwise trim.

The judge's notes show why coherence falls. Engine-drawn IDs lose domain knowledge (no Shopify `gid://`) and break repeats (one user with two IDs). Slots the LLM left out get faker gibberish. A `pattern` also guards meaningful values such as money, airport codes and repository names, so it doesn't mark a value as machine work. Letting the LLM write IDs helped Luna reach near parity (+0.42, -0.08) but not the default model (+0.08, -0.92), so the sample does not separate these causes.

The first run also had two prototype bugs (nullable unions, unechoed request IDs). Its results are in `run1/` and the fixes were re-judged, so the table is post-fix.

## Journeys

Three multi-step journeys (Stripe payment to refund, travel search to cancellation, GitHub search to issue), 3 models, 2 repetitions. The engine ran as today's `mockingbird serve` from current `main`.

| | Engine | World, IDs set | World, LLM IDs |
| --- | ---: | ---: | ---: |
| Checks passed | 98/174 (56%) | 145/169 (86%) | 141/168 (84%) |
| Generated answers, of 114 calls | 90 | 66 | 66 |
| Effects applied / rejected | | 13 / 1 | 15 / 2 |

Gains come from remembering nested and listed entities and from effects: repo detail matching its search result (0/6 to 6/6), read-after-confirm (0/6 to 6/6), offer price (0/6 to 6/6), refund currency (0/6 to 6/6 on Stripe and travel), and a cancel showing on the booking (0/6 to 3/6).

The world did worse in one place: "confirm moves the status on" fell from 6/6 to 1/6, then 0/6. Shown the existing resource, models tend to keep its status. Two segment checks are 0/6 for everything. See `report.md` for every check.

## Limits

- I wrote the journey checks knowing the design, so they favour what it does. They test behaviour a real client relies on, but treat the size of the gain with caution.
- 6 runs per journey and model is small, and 3 contracts is narrow.
- The world ran only on template generation. Both template variants score alike, which suggests the gain comes from the world and not the generation format, but it has not been tested on full-document generation.
- The prototype picks union branches and array sizes itself, where the spec had the LLM pick them. That may account for part of the coherence loss.

## Files

| Path | Contents |
| --- | --- |
| `report.md` | Generated tables, by model and by check |
| `single-*.jsonl`, `baseline.jsonl` | Answers for each approach, and round 2's for the same calls |
| `journeys*.jsonl` | Journey checks per run |
| `judge/`, `judge.jsonl`, `judge-key.json` | Final judging round: engine, template, template with LLM IDs |
| `run1/`, `run2/` | Earlier judging rounds; `run2/` is the numbers comparison |
| `calls.jsonl`, `exchanges.jsonl.gz` | Proxy ledger and full exchanges. A scan for `sk-or-` finds nothing |
