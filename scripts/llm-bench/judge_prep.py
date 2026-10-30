#!/usr/bin/env python3
"""Blinded LLM-judge preparation for the model benchmark (rubric.md 1.1-1.3, 1.5).

Subcommands:

  judge_prep.py build --out DIR [--seed SEED]
      Reads DIR/samples.jsonl (one row per engine call; run.py adds `model`).
      Picks the stratified judged targets (1.1), picks the rep, and writes
      DIR/judge/batch-XX.json (one per target, every model's judged answer under
      an opaque id, blinded and shuffled by seed), DIR/judge/README.md (judge
      instructions and the 1-5 anchors from rubric 1.4) and DIR/judge-key.json
      (opaque id -> model, plus the auto-scored items of 1.5). The key must
      never be shown to a judge. Also writes DIR/judge/recheck/batch-XX.json:
      the first 2 batches in presentation order with their items reversed, for
      the 1.3 order check (give them to a fresh judge after the main pass).

  judge_prep.py merge --out DIR --scores FILE [FILE ...] [--recheck FILE ...]
      Validates judge score files (one per batch) and writes DIR/judge.jsonl,
      adding the auto-scored items from judge-key.json. Every judged opaque id
      of every batch must be scored exactly once, with integer scores 1-5.
      --recheck takes re-judgements of already scored batches (order reversed)
      and writes DIR/judge-recheck.jsonl and DIR/judge-order-check.json with
      judge_order_mad.

Judge score file shape:
  {"batch_id": "batch-01", "judge": "<judge model id>",
   "scores": [{"opaque_id": "...", "realism": 1-5, "coherence": 1-5,
               "rationale": "<=300 chars"}]}

Stdlib only, Python 3.9+. The judges are Claude workflow agents, not OpenRouter
calls, so this script makes no network request.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
from typing import Any, Dict, List, Optional, Set, Tuple

DEFAULT_SEED = "mb-bench-2026-10-29"
API_TYPES = ["rest", "graphql", "grpc", "soap", "asyncapi"]
DOMAINS = ["travel", "payments"]
SCHEMA_CHARS = 4000
BODY_CHARS = 6000
REQUEST_CHARS = 4000
RATIONALE_CHARS = 300
TRUNCATED = "...(truncated)"
AUTO_JUDGE = "auto"
ORDER_MAD_LIMIT = 0.5
RECHECK_BATCHES = 2  # rubric 1.3 order check: re-judge 2 batches with the order reversed

# Rubric 1.4, copied verbatim. tests check these against docs/benchmarks/rubric.md.
REALISM_ANCHORS = [
    (5, "Indistinguishable from production data: plausible, varied values (real-looking names, addresses, IATA codes, ISO currencies, amounts with correct minor units, sensible timestamps), domain-correct ids and formats (`pi_...` for Stripe, PNR-like booking refs), no filler."),
    (4, "Realistic overall; one or two bland or repetitive values (e.g. all prices 100.00, sequential ids) but nothing a developer would flag."),
    (3, "Clearly synthetic in places: generic values (\"John Doe\", \"Item 1\"), round or repeated numbers, a domain slip (a flight arriving before departure is coherence, but a 3-letter airport code that does not exist is realism)."),
    (2, "Mostly placeholder-like: several fields read \"string\", \"example\", \"foo\", \"lorem ipsum\", \"test\", `0`, empty arrays where the request implies data, or schema descriptions echoed as values."),
    (1, "Not usable as mock data: placeholder text throughout, wrong domain entirely, prose or markdown instead of data, a refusal, or an empty/default body."),
]
COHERENCE_ANCHORS = [
    (5, "Every cross-field relation holds: totals equal sum of line items, `end > start`, currency matches amounts' minor units, status matches timestamps (a `succeeded` charge has `captured: true`), ids requested in the path/query are echoed, filters/pagination in the request are respected, persona or scenario cues (if present in the request or contract) are followed."),
    (4, "One minor inconsistency (a count off by one, a timestamp in a different time zone than the others)."),
    (3, "One material inconsistency (total does not match items, wrong id echoed, filter ignored) or two minor ones."),
    (2, "Several contradictions; the body would break a client test that checks relations."),
    (1, "Self-contradictory or unrelated to the request (answers a different operation, wrong resource type)."),
]


def fail(message: str) -> None:
    sys.stderr.write(f"judge_prep: {message}\n")
    sys.exit(2)


def sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def opaque_id(seed: str, model: str, contract: str, target: str, rep: int) -> str:
    return sha256_hex(f"{seed}|{model}|{contract}|{target}|{rep}")[:10]


def truncate(text: str, limit: int) -> str:
    return text if len(text) <= limit else text[:limit] + TRUNCATED


def pretty_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, indent=2)


def compact_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def is_empty_body(body: Any) -> bool:
    return body is None or body == "" or body == {} or body == []


def read_jsonl(path: str) -> List[Any]:
    rows: List[Any] = []
    with open(path, encoding="utf-8") as fh:
        for lineno, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError as exc:
                fail(f"{path}:{lineno}: invalid JSON ({exc})")
    return rows


def write_text_atomic(path: str, text: str) -> None:
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        fh.write(text)
    os.replace(tmp, path)


def write_json_atomic(path: str, value: Any) -> None:
    write_text_atomic(path, json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def write_jsonl_atomic(path: str, rows: List[Dict[str, Any]]) -> None:
    write_text_atomic(path, "".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows))


class Target:
    """One (contract, target) pair and its samples per model and rep."""

    def __init__(self, contract: str, target: str) -> None:
        self.contract = contract
        self.target = target
        self.api_type: Optional[str] = None
        self.domain: Optional[str] = None
        self.target_kind: Optional[str] = None
        self.rep: Optional[int] = None
        # model -> rep -> sample row
        self.rows: Dict[str, Dict[int, Dict[str, Any]]] = {}


def load_index(rows: List[Any]) -> Tuple[List[str], Dict[Tuple[str, str], Target], Dict[str, Any]]:
    models: Set[str] = set()
    targets: Dict[Tuple[str, str], Target] = {}
    stats: Dict[str, Any] = {"rows": len(rows), "duplicates_ignored": 0, "warnings": []}
    for n, row in enumerate(rows, 1):
        if not isinstance(row, dict):
            fail(f"sample {n} is not a JSON object")
        model = row.get("model")
        contract = row.get("contract")
        target = row.get("target")
        rep = row.get("rep")
        if not (isinstance(model, str) and model):
            fail(f"sample {n} has no `model` field (run.py adds it when it merges samples)")
        if not (isinstance(contract, str) and isinstance(target, str)):
            fail(f"sample {n} ({model}) has no string contract/target")
        if not isinstance(rep, int) or isinstance(rep, bool):
            fail(f"sample {n} ({model} {contract} {target}) has no integer rep")
        key = (contract, target)
        t = targets.get(key)
        if t is None:
            t = Target(contract, target)
            targets[key] = t
        for field in ("api_type", "domain", "target_kind"):
            value = row.get(field)
            current = getattr(t, field)
            if current is None:
                setattr(t, field, value)
            elif value != current:
                stats["warnings"].append(
                    f"{contract} {target}: {field} differs between rows "
                    f"({current!r} vs {value!r}); first kept"
                )
        reps = t.rows.setdefault(model, {})
        if rep in reps:
            stats["duplicates_ignored"] += 1
            continue
        reps[rep] = row
        models.add(model)
    return sorted(models), targets, stats


def build_pool(models: List[str], targets: Dict[Tuple[str, str], Target]) -> Tuple[List[Target], List[List[str]]]:
    """Targets present for every model that ran, each with its rep (1.1 step 1 and 4)."""
    wanted = set(models)
    pool: List[Target] = []
    no_common_rep: List[List[str]] = []
    for key in sorted(targets):
        t = targets[key]
        if set(t.rows) != wanted:
            continue
        common: Optional[Set[int]] = None
        for reps in t.rows.values():
            common = set(reps) if common is None else common & set(reps)
        if not common:
            no_common_rep.append([t.contract, t.target])
            continue
        t.rep = min(common)
        pool.append(t)
    return pool, no_common_rep


def pick_stratum(seed: str, candidates: List[Target]) -> Target:
    return min(candidates, key=lambda t: sha256_hex(f"{seed}|{t.contract}|{t.target}"))


def choose_targets(seed: str, pool: List[Target]) -> Tuple[List[Tuple[str, Target, int]], List[str]]:
    """Stratified pick (1.1 step 2 and 3). Returns ([(stratum, target, n_candidates)], skipped)."""
    chosen: List[Tuple[str, Target, int]] = []
    chosen_keys: Set[Tuple[str, str]] = set()
    skipped: List[str] = []

    def stratum(name: str, candidates: List[Target]) -> None:
        free = [t for t in candidates if (t.contract, t.target) not in chosen_keys]
        if not free:
            skipped.append(name)
            return
        pick = pick_stratum(seed, free)
        chosen_keys.add((pick.contract, pick.target))
        chosen.append((name, pick, len(free)))

    for api in API_TYPES:
        stratum(f"api_type:{api}", [t for t in pool if t.api_type == api])
    for domain in DOMAINS:
        stratum(f"domain:{domain}", [t for t in pool if t.domain == domain])
    return chosen, skipped


def build_batch(seed: str, batch_id: str, t: Target, models: List[str]) -> Tuple[Dict[str, Any], List[Dict[str, Any]], Dict[str, Dict[str, Any]], List[str]]:
    """Returns (batch file doc, judged items in presentation order, key items, auto ids)."""
    judged: List[Dict[str, Any]] = []
    key_items: Dict[str, Dict[str, Any]] = {}
    auto_ids: List[str] = []
    for model in models:
        oid = opaque_id(seed, model, t.contract, t.target, t.rep)
        if oid in key_items:
            fail(f"opaque id collision for {oid}")
        row = t.rows.get(model, {}).get(t.rep)
        reason: Optional[str]
        if row is None:
            reason = "missing"
        elif row.get("source") == "fallback":
            reason = "fallback"
        elif row.get("error") is not None:
            reason = "error"
        elif is_empty_body(row.get("body")) and row.get("body") != {}:
            # A model's `{}` can be the correct reply (an OTLP full-success
            # `Export`): it is judged, and aggregate.py leaves a schema-valid
            # one out of J instead of scoring it 1/1.
            reason = "empty_body"
        else:
            reason = None
        key_items[oid] = {
            "model": model,
            "batch_id": batch_id,
            "contract": t.contract,
            "target": t.target,
            "rep": t.rep,
            "auto": reason is not None,
            "auto_reason": reason,
            "auto_scores": {"realism": 1, "coherence": 1} if reason is not None else None,
        }
        if reason is not None:
            auto_ids.append(oid)
            continue
        judged.append({
            "opaque_id": oid,
            "request": truncate(pretty_json(row.get("request")), REQUEST_CHARS),
            "schema": truncate(compact_json(row.get("schema")), SCHEMA_CHARS),
            "body": truncate(pretty_json(row.get("body")), BODY_CHARS),
        })
    judged.sort(key=lambda item: sha256_hex(f"{seed}|{batch_id}|{item['opaque_id']}"))
    doc = {
        "batch_id": batch_id,
        "contract": t.contract,
        "api_type": t.api_type,
        "domain": t.domain,
        "target": t.target,
        "items": judged,
    }
    return doc, judged, key_items, auto_ids


def anchor_table(title: str, anchors: List[Tuple[int, str]]) -> str:
    lines = [f"**{title}**", "", "| Score | Anchor |", "|---|---|"]
    lines += [f"| {score} | {text} |" for score, text in anchors]
    return "\n".join(lines)


def judge_readme(batch_files: List[str]) -> str:
    order = "\n".join(f"{i}. `{name}`" for i, name in enumerate(batch_files, 1))
    return f"""# Blinded judging: model benchmark 2026-10-29

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

{order}

## Realism

{anchor_table("Realism: would this pass as a response from the real API in this domain?", REALISM_ANCHORS)}

## Coherence

{anchor_table("Coherence: is the body internally consistent and consistent with the request?", COHERENCE_ANCHORS)}

## Output

For each batch, write one file `scores/batch-XX.json` (inside this folder) containing a single
JSON object (no code fences, no prose around it):

```json
{{
  "batch_id": "batch-01",
  "judge": "<your own model id>",
  "scores": [
    {{"opaque_id": "0123456789", "realism": 4, "coherence": 5, "rationale": "..."}}
  ]
}}
```

`scores` must contain every `opaque_id` of the batch exactly once, with `realism`
and `coherence` as integers 1 to 5. Items with no `opaque_id` in the batch file
are not yours to score.

Validate and merge with: `python3 scripts/llm-bench/judge_prep.py merge --out <DIR> --scores <DIR>/judge/scores/batch-XX.json ...`
"""


def cmd_build(args: argparse.Namespace) -> None:
    out = args.out
    samples_path = os.path.join(out, "samples.jsonl")
    if not os.path.isfile(samples_path):
        fail(f"missing {samples_path}")
    seed = args.seed
    models, targets, stats = load_index(read_jsonl(samples_path))
    if not models:
        fail("no samples")
    pool, no_common_rep = build_pool(models, targets)
    if not pool:
        fail(f"no (contract, target) present for every one of the {len(models)} models that ran")
    chosen, skipped = choose_targets(seed, pool)
    if not chosen:
        fail("no judged target could be chosen")

    judge_dir = os.path.join(out, "judge")
    os.makedirs(judge_dir, exist_ok=True)
    for name in os.listdir(judge_dir):
        if name.startswith("batch-") and name.endswith(".json"):
            os.remove(os.path.join(judge_dir, name))

    recheck_dir = os.path.join(judge_dir, "recheck")
    if os.path.isdir(recheck_dir):
        for name in os.listdir(recheck_dir):
            if name.startswith("batch-") and name.endswith(".json"):
                os.remove(os.path.join(recheck_dir, name))

    items: Dict[str, Dict[str, Any]] = {}
    docs: Dict[str, Dict[str, Any]] = {}
    batches: List[Dict[str, Any]] = []
    strata: List[Dict[str, Any]] = []
    batch_ids: List[str] = []
    judged_total = 0
    auto_total = 0
    for index, (stratum, t, n_candidates) in enumerate(chosen, 1):
        batch_id = f"batch-{index:02d}"
        batch_ids.append(batch_id)
        doc, judged, key_items, auto_ids = build_batch(seed, batch_id, t, models)
        for oid, entry in key_items.items():
            if oid in items:
                fail(f"opaque id collision for {oid}")
            items[oid] = entry
        file_name = f"{batch_id}.json"
        write_json_atomic(os.path.join(judge_dir, file_name), doc)
        docs[batch_id] = doc
        judged_ids = [item["opaque_id"] for item in judged]
        judged_total += len(judged_ids)
        auto_total += len(auto_ids)
        batches.append({
            "batch_id": batch_id,
            "file": f"judge/{file_name}",
            "stratum": stratum,
            "contract": t.contract,
            "target": t.target,
            "api_type": t.api_type,
            "domain": t.domain,
            "target_kind": t.target_kind,
            "rep": t.rep,
            "pool_candidates": n_candidates,
            "judged": judged_ids,
            "auto": auto_ids,
        })
        strata.append({"stratum": stratum, "batch_id": batch_id, "contract": t.contract,
                       "target": t.target, "rep": t.rep, "candidates": n_candidates})

    presentation = sorted(batch_ids, key=lambda b: sha256_hex(f"{seed}|{b}"))
    presentation_files = [f"{b}.json" for b in presentation]
    write_text_atomic(os.path.join(judge_dir, "README.md"), judge_readme(presentation_files))

    # Order check (rubric 1.3): the first batches in presentation order, items reversed.
    # Give these to a fresh judge context after the main pass; merge them with --recheck.
    recheck_ids = [b for b in presentation if len(docs[b]["items"]) > 1][:RECHECK_BATCHES]
    if recheck_ids:
        os.makedirs(recheck_dir, exist_ok=True)
    for b in recheck_ids:
        doc = dict(docs[b], items=list(reversed(docs[b]["items"])))
        write_json_atomic(os.path.join(recheck_dir, f"{b}.json"), doc)

    key = {
        "seed": seed,
        "models": models,
        "pool": {
            "candidates": len(pool),
            "no_common_rep": no_common_rep,
            "duplicates_ignored": stats["duplicates_ignored"],
            "warnings": stats["warnings"],
        },
        "strata": strata,
        "strata_skipped": skipped,
        "presentation_order": presentation,
        "recheck_batches": recheck_ids,
        "batches": batches,
        "items": items,
    }
    write_json_atomic(os.path.join(out, "judge-key.json"), key)
    summary = {
        "seed": seed,
        "models": len(models),
        "batches": len(batches),
        "judged_items": judged_total,
        "auto_items": auto_total,
        "strata_skipped": skipped,
        "recheck_batches": [f"judge/recheck/{b}.json" for b in recheck_ids],
        "targets": [[b["batch_id"], b["contract"], b["target"], b["api_type"], b["domain"], b["rep"]] for b in batches],
    }
    print(json.dumps(summary, indent=2))


def load_score_file(path: str, batches: Dict[str, Dict[str, Any]], model_names: Set[str]) -> Dict[str, Any]:
    try:
        with open(path, encoding="utf-8") as fh:
            doc = json.load(fh)
    except (OSError, json.JSONDecodeError) as exc:
        fail(f"{path}: cannot read score file ({exc})")
    if not isinstance(doc, dict):
        fail(f"{path}: top level must be a JSON object")
    batch_id = doc.get("batch_id")
    if batch_id not in batches:
        fail(f"{path}: unknown batch_id {batch_id!r}")
    judge = doc.get("judge")
    if not isinstance(judge, str) or not judge.strip():
        fail(f"{path}: `judge` must be a non-empty string")
    normalised = judge.strip().lower().replace(" ", "-")
    if normalised in model_names or normalised.split("/")[-1] in model_names:
        fail(f"{path}: judge {judge!r} is a roster model; the judge must not be on the roster")
    scores = doc.get("scores")
    if not isinstance(scores, list):
        fail(f"{path}: `scores` must be a list")
    expected = set(batches[batch_id]["judged"])
    seen: Dict[str, Dict[str, Any]] = {}
    for entry in scores:
        if not isinstance(entry, dict):
            fail(f"{path}: every score must be an object")
        oid = entry.get("opaque_id")
        if oid not in expected:
            fail(f"{path}: {oid!r} is not a judged item of {batch_id} (unknown, or an auto item)")
        if oid in seen:
            fail(f"{path}: {oid} scored more than once")
        for scale in ("realism", "coherence"):
            value = entry.get(scale)
            if not isinstance(value, int) or isinstance(value, bool) or not 1 <= value <= 5:
                fail(f"{path}: {oid} {scale} must be an integer 1-5, got {value!r}")
        rationale = entry.get("rationale")
        if not isinstance(rationale, str):
            fail(f"{path}: {oid} rationale must be a string")
        seen[oid] = {
            "realism": entry["realism"],
            "coherence": entry["coherence"],
            "rationale": rationale[:RATIONALE_CHARS],
        }
    missing = sorted(expected - set(seen))
    if missing:
        fail(f"{path}: {batch_id} is missing {len(missing)} judged item(s), e.g. {missing[0]}")
    return {"batch_id": batch_id, "judge": judge.strip(), "scores": seen}


def cmd_merge(args: argparse.Namespace) -> None:
    out = args.out
    key_path = os.path.join(out, "judge-key.json")
    if not os.path.isfile(key_path):
        fail(f"missing {key_path}; run build first")
    with open(key_path, encoding="utf-8") as fh:
        key = json.load(fh)
    seed = key["seed"]
    batches = {b["batch_id"]: b for b in key["batches"]}
    items = key["items"]
    model_names: Set[str] = set()
    for model in key["models"]:
        model_names.add(model.lower())
        model_names.add(model.lower().replace("/", "__"))
        model_names.add(model.lower().split("/")[-1])

    scored: Dict[str, Dict[str, Any]] = {}
    for path in args.scores:
        doc = load_score_file(path, batches, model_names)
        if doc["batch_id"] in scored:
            fail(f"{doc['batch_id']} is scored by more than one file (use --recheck for re-judgements)")
        scored[doc["batch_id"]] = doc
    missing_batches = [b for b in batches if b not in scored]
    if missing_batches:
        fail(f"no score file for {', '.join(missing_batches)}")

    rows: List[Dict[str, Any]] = []
    for batch_id in sorted(batches):
        batch = batches[batch_id]
        doc = scored[batch_id]
        for oid in sorted(batch["judged"], key=lambda o: sha256_hex(f"{seed}|{batch_id}|{o}")):
            s = doc["scores"][oid]
            rows.append({"seed": seed, "judge": doc["judge"], "batch_id": batch_id, "opaque_id": oid,
                         "realism": s["realism"], "coherence": s["coherence"],
                         "rationale": s["rationale"], "auto": False})
        for oid in batch["auto"]:
            reason = items[oid]["auto_reason"]
            rows.append({"seed": seed, "judge": AUTO_JUDGE, "batch_id": batch_id, "opaque_id": oid,
                         "realism": 1, "coherence": 1,
                         "rationale": f"auto: {reason}; body not shown to judge", "auto": True})
    write_jsonl_atomic(os.path.join(out, "judge.jsonl"), rows)
    summary: Dict[str, Any] = {
        "seed": seed,
        "batches": len(batches),
        "judged_rows": sum(1 for r in rows if not r["auto"]),
        "auto_rows": sum(1 for r in rows if r["auto"]),
        "judges": sorted({doc["judge"] for doc in scored.values()}),
        "wrote": os.path.join(out, "judge.jsonl"),
    }

    if args.recheck:
        rechecks: Dict[str, Dict[str, Any]] = {}
        for path in args.recheck:
            doc = load_score_file(path, batches, model_names)
            if doc["batch_id"] in rechecks:
                fail(f"{doc['batch_id']} has more than one recheck file")
            if doc["batch_id"] not in scored:
                fail(f"recheck for {doc['batch_id']}, which has no main score file")
            rechecks[doc["batch_id"]] = doc
        diffs: List[float] = []
        recheck_rows: List[Dict[str, Any]] = []
        for batch_id in sorted(rechecks):
            for oid in batches[batch_id]["judged"]:
                a = scored[batch_id]["scores"][oid]
                b = rechecks[batch_id]["scores"][oid]
                diffs.append(abs((a["realism"] + a["coherence"]) / 2 - (b["realism"] + b["coherence"]) / 2))
                recheck_rows.append({"seed": seed, "judge": rechecks[batch_id]["judge"], "batch_id": batch_id,
                                     "opaque_id": oid, "realism": b["realism"], "coherence": b["coherence"],
                                     "rationale": b["rationale"], "auto": False})
        mad = round(sum(diffs) / len(diffs), 4) if diffs else None
        write_jsonl_atomic(os.path.join(out, "judge-recheck.jsonl"), recheck_rows)
        write_json_atomic(os.path.join(out, "judge-order-check.json"), {
            "seed": seed,
            "batches": sorted(rechecks),
            "items": len(diffs),
            "judge_order_mad": mad,
            "limit": ORDER_MAD_LIMIT,
            "noisy": (mad is not None and mad > ORDER_MAD_LIMIT),
        })
        summary["judge_order_mad"] = mad
        summary["judge_order_items"] = len(diffs)
    print(json.dumps(summary, indent=2))


def main(argv: Optional[List[str]] = None) -> None:
    parser = argparse.ArgumentParser(description="Blinded judge preparation (rubric 1.1-1.3, 1.5).")
    sub = parser.add_subparsers(dest="cmd", required=True)

    b = sub.add_parser("build", help="write judge batches, README and judge-key.json")
    b.add_argument("--out", required=True, help="run directory holding samples.jsonl")
    b.add_argument("--seed", default=DEFAULT_SEED)
    b.set_defaults(func=cmd_build)

    m = sub.add_parser("merge", help="validate judge scores and write judge.jsonl")
    m.add_argument("--out", required=True, help="run directory holding judge-key.json")
    m.add_argument("--scores", nargs="+", required=True, help="judge score files, one per batch")
    m.add_argument("--recheck", nargs="+", default=None, help="re-judged batches (order reversed)")
    m.set_defaults(func=cmd_merge)

    args = parser.parse_args(argv)
    args.func(args)


if __name__ == "__main__":
    main()
