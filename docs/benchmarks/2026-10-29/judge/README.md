# Blinded judging: model benchmark 2026-10-29

You are a judge for mock API responses. Each file `batch-XX.json` in this folder is
one API operation (or message channel) answered by several anonymous models. Each
item has an `opaque_id`, the `request` the mock was asked for, the response
`schema` (truncated), and the response `body` (truncated).

## Rules

- Score every item on two scales, integers 1 to 5: **realism** and **coherence**.
  Use the anchors below. They are absolute; the batch lets you compare items so
  the scores stay consistent across the batch.
- Score each item on what you see. Do not score schema conformance as such: a
  separate grader measures that. Score realism and coherence as the anchors say.
- Do not try to identify the model or provider behind an item. Do not open
  `judge-key.json`, any file outside this folder's batches, or the other
  judges' outputs.
- Judge batches independently. Do not carry scores from one batch into another.
- Write a `rationale` of at most 300 characters per item: name the specific
  field or value that drove the score.
- Process the batches in this order:

1. `batch-02.json`
2. `batch-04.json`
3. `batch-06.json`
4. `batch-01.json`
5. `batch-03.json`
6. `batch-07.json`
7. `batch-05.json`

## Realism

**Realism: would this pass as a response from the real API in this domain?**

| Score | Anchor |
|---|---|
| 5 | Indistinguishable from production data: plausible, varied values (real-looking names, addresses, IATA codes, ISO currencies, amounts with correct minor units, sensible timestamps), domain-correct ids and formats (`pi_...` for Stripe, PNR-like booking refs), no filler. |
| 4 | Realistic overall; one or two bland or repetitive values (e.g. all prices 100.00, sequential ids) but nothing a developer would flag. |
| 3 | Clearly synthetic in places: generic values ("John Doe", "Item 1"), round or repeated numbers, a domain slip (a flight arriving before departure is coherence, but a 3-letter airport code that does not exist is realism). |
| 2 | Mostly placeholder-like: several fields read "string", "example", "foo", "lorem ipsum", "test", `0`, empty arrays where the request implies data, or schema descriptions echoed as values. |
| 1 | Not usable as mock data: placeholder text throughout, wrong domain entirely, prose or markdown instead of data, a refusal, or an empty/default body. |

## Coherence

**Coherence: is the body internally consistent and consistent with the request?**

| Score | Anchor |
|---|---|
| 5 | Every cross-field relation holds: totals equal sum of line items, `end > start`, currency matches amounts' minor units, status matches timestamps (a `succeeded` charge has `captured: true`), ids requested in the path/query are echoed, filters/pagination in the request are respected, persona or scenario cues (if present in the request or contract) are followed. |
| 4 | One minor inconsistency (a count off by one, a timestamp in a different time zone than the others). |
| 3 | One material inconsistency (total does not match items, wrong id echoed, filter ignored) or two minor ones. |
| 2 | Several contradictions; the body would break a client test that checks relations. |
| 1 | Self-contradictory or unrelated to the request (answers a different operation, wrong resource type). |

## Output

For each batch, write one file `scores/batch-XX.json` (inside this folder) containing a single
JSON object (no code fences, no prose around it):

```json
{
  "batch_id": "batch-01",
  "judge": "<your own model id>",
  "scores": [
    {"opaque_id": "0123456789", "realism": 4, "coherence": 5, "rationale": "..."}
  ]
}
```

`scores` must contain every `opaque_id` of the batch exactly once, with `realism`
and `coherence` as integers 1 to 5. Items with no `opaque_id` in the batch file
are not yours to score.

Validate and merge with: `python3 scripts/llm-bench/judge_prep.py merge --out <DIR> --scores <DIR>/judge/scores/batch-XX.json ...`
