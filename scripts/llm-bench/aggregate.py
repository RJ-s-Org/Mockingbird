#!/usr/bin/env python3
"""Aggregate one LLM bench run into results.json, results.csv and tables.md.

    python3 scripts/llm-bench/aggregate.py --out docs/benchmarks/2026-10-29 \\
        [--judge DIR/judge.jsonl] [--judge-key DIR/judge-key.json] [--seed S] [--baseline M]

Implements docs/benchmarks/rubric.md mechanically (its Formulas block, section 1
judging plan and gates, and section 3 cost and latency rules). Inputs are read
from --out:

  roster.json, run.json, calls.jsonl (proxy ledger), samples.jsonl (engine
  samples with `model`), grades.jsonl + grade-summary.json (grade.py),
  server.jsonl (server suites), judge.jsonl + judge-key.json (blinded judging),
  key-before.json / key-after.json, pilot/calls.jsonl (pilot ledger).

Missing inputs never crash the script: the dependent metrics are null (rendered
as an em dash in tables.md) and the gate reasons say why. Nothing here calls the
network or reads any API key. Standard library only, Python 3.9+.
"""

from __future__ import annotations

import argparse
import csv
import glob
import hashlib
import json
import math
import os
import re
import statistics
import sys
from collections import Counter, defaultdict, deque
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional, Tuple

BASELINE = "deepseek/deepseek-v4.1-flash"
SEED = "mb-bench-2026-10-29"
API_ORDER = ["rest", "graphql", "grpc", "soap", "asyncapi"]
DOMAIN_STRATA = ["travel", "payments"]
ENGINE_SOURCES = ("llm", "repaired", "fallback")
ERROR_KINDS = ("http_4xx", "http_5xx", "stream_error", "empty")
DEFAULT_KEYWORDS = {
    "format": ["format"],
    "enum": ["enum", "const"],
    "constraint": ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum",
                   "minLength", "maxLength", "pattern", "minItems", "maxItems"],
}
# (a, b, c): quality, cost and latency exponents of the Value score.
EXPONENT_SETS = {
    "default": (2.0, 0.5, 0.5),
    "1,1,1": (1.0, 1.0, 1.0),
    "2,0.25,1": (2.0, 0.25, 1.0),
}
WEIGHT_S, WEIGHT_K, WEIGHT_JN = 0.35, 0.25, 0.40
GATE_SCHEMA_VALID = 0.90
GATE_ERROR_PLUS_TIMEOUT = 0.10
GATE_JUDGE_N = 5
COST_FLOOR = 0.01          # USD per 1k responses
BEST_Q_BAND = 0.90         # "within 10 % of the best Q"
OPAQUE_LEN = 10


# --------------------------------------------------------------------------
# Small helpers
# --------------------------------------------------------------------------


def num(v: Any) -> Optional[float]:
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        return None
    return float(v) if math.isfinite(v) else None


def rate(n: float, d: float) -> Optional[float]:
    return n / d if d else None


def mean(xs: Any) -> Optional[float]:
    vals = [x for x in xs if x is not None]
    return sum(vals) / len(vals) if vals else None


def pct(xs: Any, p: int) -> Optional[float]:
    """Nearest rank: sorted(x)[ceil(p/100 * n) - 1]."""
    vals = sorted(x for x in xs if x is not None)
    if not vals:
        return None
    n = len(vals)
    rank = -(-p * n // 100)  # ceil(p * n / 100) with integers
    return vals[max(1, rank) - 1]


def median(xs: Any) -> Optional[float]:
    vals = [x for x in xs if x is not None]
    return statistics.median(vals) if vals else None


def count_nonnull(xs: Any) -> int:
    return sum(1 for x in xs if x is not None)


def model_from_slug(slug: Any) -> Optional[str]:
    return slug.replace("__", "/") if isinstance(slug, str) and slug else None


def row_model(row: Dict[str, Any]) -> Optional[str]:
    for key in ("model", "model_requested"):
        value = row.get(key)
        if isinstance(value, str) and value:
            return value
    m = model_from_slug(row.get("model_slug"))
    if m:
        return m
    label = row.get("label")
    if isinstance(label, str) and "~" in label:
        return model_from_slug(label.split("~", 1)[0])
    return None


def is_judge_ledger_row(row: Dict[str, Any]) -> bool:
    return row.get("workload") == "judge" or str(row.get("label", "")).startswith("judge~")


def is_budget(row: Dict[str, Any]) -> bool:
    return row.get("error_kind") == "budget"


def is_success(row: Dict[str, Any]) -> bool:
    return row.get("status") == 200 and row.get("error_kind") is None


def api_sort_key(api: Any) -> Tuple[int, str]:
    name = str(api)
    return (API_ORDER.index(name) if name in API_ORDER else len(API_ORDER), name)


def norm_op(text: Any) -> Optional[str]:
    """`METHOD /path`: method uppercased, trailing slash dropped, `{param}` -> `{}`."""
    if not isinstance(text, str):
        return None
    parts = text.strip().split(None, 1)
    if len(parts) != 2:
        return None
    method, path = parts[0].upper(), re_sub_params(parts[1].strip())
    if len(path) > 1 and path.endswith("/"):
        path = path.rstrip("/") or "/"
    return f"{method} {path}"


def re_sub_params(path: str) -> str:
    return re.sub(r"\{[^{}]*\}", "{}", path)


# --------------------------------------------------------------------------
# Input loading (every loader returns None when the file is missing)
# --------------------------------------------------------------------------


def read_json(path: str) -> Any:
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return None


def read_jsonl(path: str) -> Tuple[Optional[List[Dict[str, Any]]], int]:
    """(rows, bad_lines); rows is None when the file does not exist."""
    if not os.path.isfile(path):
        return None, 0
    rows: List[Dict[str, Any]] = []
    bad = 0
    with open(path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except ValueError:
                bad += 1
                continue
            if isinstance(obj, dict):
                rows.append(obj)
            else:
                bad += 1
    return rows, bad


class Inputs:
    """Loads everything under --out and records what was present."""

    def __init__(self, out: str, judge_path: str, key_path: str) -> None:
        self.out = out
        self.status: Dict[str, Dict[str, Any]] = {}
        run = self._json("run.json", os.path.join(out, "run.json"))
        self.run = run if isinstance(run, dict) else {}
        self.roster = self._json("roster.json", os.path.join(out, "roster.json"))
        self.key_before = self._json("key-before.json", os.path.join(out, "key-before.json"))
        self.key_after = self._json("key-after.json", os.path.join(out, "key-after.json"))
        self.grade_summary = self._json("grade-summary.json", os.path.join(out, "grade-summary.json"))
        self.calls = self._jsonl("calls.jsonl", os.path.join(out, "calls.jsonl"))
        self.pilot_calls = self._jsonl("pilot/calls.jsonl", os.path.join(out, "pilot", "calls.jsonl"))
        # Calls of runs that were redone (their samples are not in samples.jsonl):
        # real spend on the key, so they count in the spend reconciliation only.
        self.superseded_calls = self._jsonl("superseded/calls.jsonl",
                                            os.path.join(out, "superseded", "calls.jsonl"))
        self.grades = self._jsonl("grades.jsonl", os.path.join(out, "grades.jsonl"))
        self.samples = self._samples()
        self.server = self._server()
        self.judge_rows = self._jsonl("judge.jsonl", judge_path)
        # judge_prep.py merge --recheck writes the reversed-order re-judge here.
        recheck = self._jsonl("judge-recheck.jsonl",
                              os.path.join(os.path.dirname(judge_path) or ".", "judge-recheck.jsonl"))
        if self.judge_rows is not None and recheck:
            self.judge_rows = self.judge_rows + [dict(r, reversed=True) for r in recheck]
        self.judge_key = normalize_judge_key(self._json("judge-key.json", key_path))
        self.judge_key_present = self.judge_key is not None

    def _json(self, name: str, path: str) -> Any:
        value = read_json(path)
        self.status[name] = {"present": value is not None}
        return value

    def _jsonl(self, name: str, path: str) -> Optional[List[Dict[str, Any]]]:
        rows, bad = read_jsonl(path)
        self.status[name] = {"present": rows is not None, "rows": None if rows is None else len(rows),
                             "bad_lines": bad}
        return rows

    def _samples(self) -> Optional[List[Dict[str, Any]]]:
        rows = self._jsonl("samples.jsonl", os.path.join(self.out, "samples.jsonl"))
        if rows is not None:
            return rows
        parts = sorted(glob.glob(os.path.join(self.out, "engine", "*", "samples.jsonl")))
        if not parts:
            return None
        merged: List[Dict[str, Any]] = []
        for part in parts:
            slug = os.path.basename(os.path.dirname(part))
            rows_part, _ = read_jsonl(part)
            for row in rows_part or []:
                if not row.get("model"):
                    row["model"] = model_from_slug(slug)
                merged.append(row)
        self.status["samples.jsonl"] = {"present": True, "rows": len(merged),
                                        "fallback": "engine/*/samples.jsonl (merged per model)"}
        return merged

    def _server(self) -> Optional[List[Dict[str, Any]]]:
        rows = self._jsonl("server.jsonl", os.path.join(self.out, "server.jsonl"))
        if rows is not None:
            return rows
        parts = sorted(glob.glob(os.path.join(self.out, "server", "*", "server.jsonl")))
        if not parts:
            return None
        merged: List[Dict[str, Any]] = []
        for part in parts:
            slug = os.path.basename(os.path.dirname(part))
            rows_part, _ = read_jsonl(part)
            for row in rows_part or []:
                if not row.get("model"):
                    row["model"] = model_from_slug(slug)
                merged.append(row)
        self.status["server.jsonl"] = {"present": True, "rows": len(merged),
                                       "fallback": "server/*/server.jsonl (merged per model)"}
        return merged

    def keyword_categories(self) -> Dict[str, List[str]]:
        cats = (self.grade_summary or {}).get("keyword_categories") if isinstance(self.grade_summary, dict) else None
        if isinstance(cats, dict) and cats:
            return {k: list(v) for k, v in cats.items()}
        return {k: list(v) for k, v in DEFAULT_KEYWORDS.items()}


def normalize_judge_key(obj: Any) -> Optional[List[Dict[str, Any]]]:
    """judge-key.json as a list of items: {opaque_id, model, contract, target, rep, ...}.

    Accepts judge_prep.py's shape ({"seed", "models", "batches", ..., "items":
    {opaque_id: {...}}}), a wrapper holding a list, a bare list, or a bare map of
    opaque_id to item (or to a model id string).
    """
    if obj is None:
        return None
    if isinstance(obj, dict):
        for name in ("items", "entries", "key"):
            if isinstance(obj.get(name), (list, dict)):
                obj = obj[name]
                break
    if isinstance(obj, dict):
        items = []
        for opaque, value in obj.items():
            if isinstance(value, dict):
                item = dict(value)
                item.setdefault("opaque_id", opaque)
                items.append(item)
            elif isinstance(value, str):
                items.append({"opaque_id": opaque, "model": value})
        return items
    if isinstance(obj, list):
        return [dict(x) for x in obj if isinstance(x, dict)]
    return None


def item_model(item: Dict[str, Any]) -> Optional[str]:
    for key in ("model", "model_id", "model_requested"):
        value = item.get(key)
        if isinstance(value, str) and value:
            return value
    return None


# --------------------------------------------------------------------------
# Engine samples and grades
# --------------------------------------------------------------------------


def key_of(model: Any, row: Dict[str, Any]) -> Tuple[str, str, str, str]:
    return (str(model), str(row.get("contract")), str(row.get("target")), str(row.get("rep")))


def attach_grades(samples: List[Dict[str, Any]], grades: Optional[List[Dict[str, Any]]]) -> Dict[str, List[Tuple[Dict[str, Any], Optional[Dict[str, Any]]]]]:
    """Per model, every sample (in file order) with its grade row or None."""
    queues: Dict[Tuple[str, str, str, str], deque] = defaultdict(deque)
    if grades is not None:
        for grade in grades:
            queues[key_of(grade_model(grade), grade)].append(grade)
    per_model: Dict[str, List[Tuple[Dict[str, Any], Optional[Dict[str, Any]]]]] = defaultdict(list)
    for sample in samples:
        model = row_model(sample)
        if model is None:
            continue
        grade = None
        if grades is not None:
            queue = queues.get(key_of(model, sample))
            if queue:
                grade = queue.popleft()
        per_model[model].append((sample, grade))
    return per_model


def grade_model(grade: Dict[str, Any]) -> Optional[str]:
    """grade.py falls back to the samples file's parent directory name, which is the
    model *slug* (`deepseek__deepseek-v4.1-flash`) when run on engine/<slug>/samples.jsonl."""
    model = grade.get("model")
    if isinstance(model, str) and "/" not in model and "__" in model:
        return model_from_slug(model)
    return model if isinstance(model, str) else None


def tally(items: List[Tuple[Dict[str, Any], Optional[Dict[str, Any]]]], has_grades: bool) -> Dict[str, Any]:
    n = len(items)
    sources = Counter(s.get("source") for s, _ in items)
    llm, repaired, fallback = sources.get("llm", 0), sources.get("repaired", 0), sources.get("fallback", 0)
    non_fallback = [(s, g) for s, g in items if s.get("source") != "fallback"]
    # K denominator is N' = N - fallback (rubric 2). A non-fallback sample with no
    # grade row counts as non-conforming; only samples grade.py could not check
    # (no schema: graded == false) leave the denominator.
    ungradable = sum(1 for _, g in non_fallback if g is not None and g.get("graded") is False)
    graded = [(s, g) for s, g in non_fallback if g is None or g.get("graded") is not False]
    conforming = sum(1 for _, g in graded if g is not None and g.get("valid") is True)
    latencies = [num(s.get("latency_ms")) for s, _ in items]
    return {
        "n": n,
        "first_try": rate(llm, n),
        "schema_valid": rate(llm + repaired, n),
        "repair": rate(repaired, n),
        "fallback": rate(fallback, n),
        "conformance": rate(conforming, len(graded)) if has_grades else None,
        "conformance_n": len(graded) if has_grades else None,
        "conformance_ungradable": ungradable if has_grades else None,
        "conformance_ungraded": sum(1 for _, g in non_fallback if g is None) if has_grades else None,
        "p50_latency_ms": pct(latencies, 50),
        "latency_n": count_nonnull(latencies),
    }


FALLBACK_CAUSES = ("truncated", "non_json", "schema", "timeout", "guard", "other")


def fallback_cause(sample: Dict[str, Any]) -> str:
    """One of FALLBACK_CAUSES for a fallback sample, from the engine's
    reason: cut at max_tokens, not JSON (after the repair), invalid against
    the schema after the repair, the LLM timeout, a guard (budget, slots,
    size cap), or anything else (an HTTP error, no reason given)."""
    r = str(sample.get("fallback_reason") or sample.get("error") or "")
    if "stopped at max_tokens" in r or "truncated" in r:
        return "truncated"
    if "not valid JSON" in r:
        return "non_json"
    if "still invalid" in r:
        return "schema"
    if "timed out" in r and "generation slot" not in r:
        return "timeout"
    if any(k in r for k in ("budget exhausted", "generation slot", "-byte cap")):
        return "guard"
    return "other"


def engine_block(items: List[Tuple[Dict[str, Any], Optional[Dict[str, Any]]]], has_grades: bool,
                 kw_categories: Dict[str, List[str]]) -> Dict[str, Any]:
    excluded = sum(1 for s, _ in items if s.get("source") not in ENGINE_SOURCES)
    items = [(s, g) for s, g in items if s.get("source") in ENGINE_SOURCES]
    overall = tally(items, has_grades)

    apis = sorted({s.get("api_type") or "unknown" for s, _ in items}, key=api_sort_key)
    domains = sorted({s.get("domain") or "unknown" for s, _ in items})
    by_api = {a: tally([x for x in items if (x[0].get("api_type") or "unknown") == a], has_grades)
              for a in apis}
    by_domain = {d: tally([x for x in items if (x[0].get("domain") or "unknown") == d], has_grades)
                 for d in domains}

    macro_ft = mean(by_api[a]["first_try"] for a in apis if by_api[a]["n"])
    macro_sv = mean(by_api[a]["schema_valid"] for a in apis if by_api[a]["n"])
    macro_k = mean(by_api[a]["conformance"] for a in apis) if has_grades else None
    s_index = 0.5 * macro_ft + 0.5 * macro_sv if macro_ft is not None and macro_sv is not None else None

    checked: Counter = Counter()
    failed: Counter = Counter()
    for s, g in items:
        if s.get("source") != "fallback" and g is not None:
            checked.update(g.get("checked_keywords") or {})
            failed.update(g.get("failed_keywords") or {})
    keywords = {}
    for category, names in kw_categories.items():
        c = sum(checked.get(name, 0) for name in names)
        f = sum(failed.get(name, 0) for name in names)
        keywords[category] = {"checked": c, "passed": c - f, "pass_rate": rate(c - f, c)}

    # The engine gives up on an LLM call at its own timeout (mb-evals: 30 s) and
    # falls back; the proxy still sees that upstream call finish with 200, so the
    # ledger's timeout_rate cannot count it. Count it from the samples instead.
    engine_timeouts = sum(1 for s, _ in items if s.get("source") == "fallback"
                          and "timed out" in str(s.get("fallback_reason") or ""))
    truncated = sum(1 for s, _ in items if s.get("source") == "fallback"
                    and "stopped at max_tokens" in str(s.get("fallback_reason") or ""))
    causes = Counter(fallback_cause(s) for s, _ in items if s.get("source") == "fallback")
    return {
        **overall,
        "engine_timeouts": engine_timeouts,
        "engine_timeout_rate": rate(engine_timeouts, len(items)),
        "fallback_truncated_max_tokens": truncated,
        "fallback_causes": dict(sorted(causes.items())),
        "excluded_sources": excluded,
        "macro": {"first_try": macro_ft, "schema_valid": macro_sv, "conformance": macro_k,
                  "apis": len(apis)},
        "S": s_index,
        "conformance_keywords": keywords,
        "by_api_type": by_api,
        "by_domain": by_domain,
        "grades_present": has_grades,
    }


# --------------------------------------------------------------------------
# Judge (section 1)
# --------------------------------------------------------------------------


def judge_plan(samples_by_model: Dict[str, List[Dict[str, Any]]], seed: str) -> Dict[str, Any]:
    """Recompute the stratified target list of section 1.1 from the samples."""
    models = sorted(m for m, rows in samples_by_model.items()
                    if any(r.get("source") in ENGINE_SOURCES for r in rows))
    empty = {"seed": seed, "models": models, "targets": [], "strata_skipped": [], "items": 0}
    if not models:
        return empty

    pairs: Dict[Tuple[str, str], Dict[str, Any]] = {}
    for m in models:
        for r in samples_by_model[m]:
            if r.get("source") not in ENGINE_SOURCES:
                continue
            key = (str(r.get("contract")), str(r.get("target")))
            entry = pairs.setdefault(key, {"api_type": r.get("api_type"), "domain": r.get("domain"),
                                           "reps": defaultdict(set)})
            entry["api_type"] = entry["api_type"] or r.get("api_type")
            entry["domain"] = entry["domain"] or r.get("domain")
            entry["reps"][m].add(r.get("rep"))

    pool = {k: v for k, v in pairs.items() if all(m in v["reps"] for m in models)}

    def score(key: Tuple[str, str]) -> str:
        return hashlib.sha256(f"{seed}|{key[0]}|{key[1]}".encode("utf-8")).hexdigest()

    apis_present = [a for a in API_ORDER if any(v["api_type"] == a for v in pairs.values())]
    chosen: List[Tuple[str, str]] = []
    targets: List[Dict[str, Any]] = []
    skipped: List[str] = []

    def pick(stratum: str, candidates: List[Tuple[str, str]]) -> None:
        if not candidates:
            skipped.append(stratum)
            return
        key = min(candidates, key=score)
        chosen.append(key)
        targets.append({"stratum": stratum, "contract": key[0], "target": key[1],
                        "api_type": pool[key]["api_type"], "domain": pool[key]["domain"]})

    for api in apis_present:
        pick(f"api_type:{api}", [k for k, v in pool.items() if v["api_type"] == api and k not in chosen])
    for domain in DOMAIN_STRATA:
        pick(f"domain:{domain}", [k for k, v in pool.items() if v["domain"] == domain and k not in chosen])

    for target in targets:
        key = (target["contract"], target["target"])
        common = set.intersection(*[set(pool[key]["reps"][m]) for m in models])
        target["rep"] = min(common, key=lambda x: (x is None, x)) if common else None

    return {"seed": seed, "models": models, "targets": targets, "strata_skipped": skipped,
            "items": len(targets) * len(models), "candidates": len(pool)}


def empty_body(body: Any) -> bool:
    return body is None or body == {} or body == [] or body == ""


def valid_empty_object(sample: Optional[Dict[str, Any]], grade: Optional[Dict[str, Any]]) -> bool:
    """A model-written `{}` that grade.py found schema-valid.

    Rubric 1.5 auto-scores an "empty body" 1/1 as a failed answer, but for some
    operations `{}` is the correct production response (an OTLP
    `TraceService.Export` full success omits `partialSuccess`). judge_prep.py
    auto-scored those without showing them to the judge, so there is no judge
    score: they are excluded from J (neither penalised nor rewarded) and counted
    in `judge_excluded_valid_empty`.
    """
    return (sample is not None and sample.get("source") in ("llm", "repaired")
            and not sample.get("error") and sample.get("body") == {}
            and grade is not None and grade.get("valid") is True)


def judge_block(model: str, key_items: List[Dict[str, Any]],
                judge_primary: Dict[str, Dict[str, Any]], sample_index: Dict[Tuple[str, str, str, str], Dict[str, Any]],
                judge_present: bool, key_present: bool,
                grade_index: Optional[Dict[Tuple[str, str, str, str], Dict[str, Any]]] = None) -> Dict[str, Any]:
    scored: List[Tuple[float, float, bool]] = []
    auto = missing = invalid = excluded_valid_empty = 0
    for item in key_items:
        if item_model(item) != model:
            continue
        has_identity = all(item.get(k) is not None for k in ("contract", "target", "rep"))
        sample = sample_index.get(key_of(model, item)) if has_identity else None
        grade = (grade_index or {}).get(key_of(model, item)) if has_identity else None
        if valid_empty_object(sample, grade):
            excluded_valid_empty += 1
            continue
        forced_auto = has_identity and (
            sample is None or sample.get("source") == "fallback" or bool(sample.get("error"))
            or empty_body(sample.get("body")))
        if forced_auto:
            scored.append((1.0, 1.0, True))
            auto += 1
            continue
        row = judge_primary.get(item.get("opaque_id"))
        if row is None:
            missing += 1
            continue
        if row.get("auto") is True:
            scored.append((1.0, 1.0, True))
            auto += 1
            continue
        realism, coherence = num(row.get("realism")), num(row.get("coherence"))
        if realism is None or coherence is None or not (1 <= realism <= 5 and 1 <= coherence <= 5):
            invalid += 1
            continue
        scored.append((realism, coherence, False))

    judged = [s for s in scored if not s[2]]
    j_mean = mean((r + c) / 2 for r, c, _ in scored) if scored else None
    return {
        "judge_items": len(scored),
        # judge_n counts the model's items (rubric 1.5: "judge_n (items)", auto items
        # included, as in J). judge_scored_n is the part a judge actually scored.
        "judge_n": len(scored),
        "judge_scored_n": len(judged),
        "judge_auto": auto,
        "judge_missing": missing,
        "judge_invalid": invalid,
        "judge_excluded_valid_empty": excluded_valid_empty,
        "realism": mean(r for r, _, _ in scored) if scored else None,
        "coherence": mean(c for _, c, _ in scored) if scored else None,
        "J": j_mean,
        "Jn": (j_mean - 1) / 4 if j_mean is not None else None,
        "judge_present": judge_present,
        "key_present": key_present,
    }


def judge_primary_rows(judge_rows: Optional[List[Dict[str, Any]]]) -> Tuple[Dict[str, Dict[str, Any]], Optional[float], int]:
    """First row per opaque_id (a reversed re-judge never replaces it), plus the order-check MAD."""
    if judge_rows is None:
        return {}, None, 0
    by_id: Dict[str, List[Dict[str, Any]]] = defaultdict(list)
    for row in judge_rows:
        if row.get("opaque_id"):
            by_id[str(row["opaque_id"])].append(row)
    primary: Dict[str, Dict[str, Any]] = {}
    diffs: List[float] = []
    for opaque, rows in by_id.items():
        plain = [r for r in rows if not r.get("reversed")]
        rev = [r for r in rows if r.get("reversed")]
        first = (plain or rows)[0]
        primary[opaque] = first
        other = rev[0] if rev else (rows[1] if len(rows) > 1 else None)
        if other is None:
            continue
        a, b = score_pair(first), score_pair(other)
        if a is not None and b is not None:
            diffs.append(abs(a - b))
    return primary, mean(diffs), len(diffs)


def score_pair(row: Dict[str, Any]) -> Optional[float]:
    realism, coherence = num(row.get("realism")), num(row.get("coherence"))
    if realism is None or coherence is None:
        return None
    return (realism + coherence) / 2


# --------------------------------------------------------------------------
# Server suites (section 2)
# --------------------------------------------------------------------------


def consistency_block(rows: List[Dict[str, Any]]) -> Dict[str, Any]:
    n = len(rows)
    ok_total = sum(1 for r in rows if r.get("ok") is True)
    by_case: Dict[str, Any] = {}
    for case in sorted({str(r.get("case")) for r in rows}):
        subset = [r for r in rows if str(r.get("case")) == case]
        by_case[case] = {"n": len(subset),
                         "ok_rate": rate(sum(1 for r in subset if r.get("ok") is True), len(subset))}
    agree = checked = 0.0
    field_rows = 0
    for r in rows:
        detail = r.get("detail") if isinstance(r.get("detail"), dict) else {}
        a, c = num(detail.get("fields_agree")), num(detail.get("fields_checked"))
        if a is not None and c is not None:
            agree += a
            checked += c
            field_rows += 1
    return {"n": n, "case_ok_rate": rate(ok_total, n), "field_agreement": rate(agree, checked),
            "fields_checked": int(checked), "field_rows_n": field_rows, "by_case": by_case}


def sse_block(rows: List[Dict[str, Any]]) -> Dict[str, Any]:
    n = len(rows)
    ok_total = sum(1 for r in rows if r.get("ok") is True)
    ttfe = [num(r.get("ttfe_ms")) for r in rows]
    return {"n": n, "ok_rate": rate(ok_total, n), "ttfe_p50_ms": pct(ttfe, 50),
            "ttfe_n": count_nonnull(ttfe)}


def import_block(rows: List[Dict[str, Any]]) -> Dict[str, Any]:
    per_input = []
    for r in rows:
        detail = r.get("detail") if isinstance(r.get("detail"), dict) else {}
        done = detail.get("job_status") == "done"
        has_version = detail.get("result_version") is not None
        fetched = isinstance(detail.get("operations"), list) and "draft_error" not in detail
        valid = bool(done and has_version and fetched)
        expected = None
        if isinstance(detail.get("expected"), list):
            expected = {norm_op(x) for x in detail["expected"] if norm_op(x)}
        draft = set()
        if done and fetched:
            draft = {norm_op(x) for x in detail["operations"] if norm_op(x)}
        inter = len(expected & draft) if expected is not None else None
        recall = rate(inter, len(expected)) if expected else None
        precision = (rate(inter, len(draft)) if draft else 0.0) if expected is not None else None
        per_input.append({
            "case": r.get("case"), "rep": r.get("rep"), "valid": valid,
            "expected_n": len(expected) if expected is not None else None,
            "draft_n": len(draft), "intersection": inter, "recall": recall, "precision": precision,
            "duration_ms": num(r.get("latency_ms")),
            "warnings": num(detail.get("warnings_count")),
        })
    scored = [p for p in per_input if p["recall"] is not None or p["precision"] is not None]
    exp_sum = sum(p["expected_n"] for p in scored if p["expected_n"] is not None)
    draft_sum = sum(p["draft_n"] for p in scored)
    inter_sum = sum(p["intersection"] for p in scored if p["intersection"] is not None)
    return {
        "n": len(per_input),
        "draft_valid_rate": rate(sum(1 for p in per_input if p["valid"]), len(per_input)),
        "recall_macro": mean(p["recall"] for p in scored),
        "recall_micro": rate(inter_sum, exp_sum),
        "precision_macro": mean(p["precision"] for p in scored),
        "precision_micro": rate(inter_sum, draft_sum),
        "scored_n": len(scored),
        "duration_mean_ms": mean(p["duration_ms"] for p in per_input),
        "warnings_mean": mean(p["warnings"] for p in per_input),
        "inputs": per_input,
    }


HARNESS_REJECT_MARKERS = ("X-Mockingbird-Session must be",)


def harness_rejected(row: Dict[str, Any]) -> bool:
    """Mockingbird refused the bench's own request (e.g. the `@nojson` variant
    put `@` in the session header), so no LLM was asked: not a model result."""
    detail = row.get("detail") if isinstance(row.get("detail"), dict) else {}
    error = str(detail.get("error") or "")
    return any(marker in error for marker in HARNESS_REJECT_MARKERS)


def drop_harness_rejected(rows: List[Dict[str, Any]]) -> Tuple[List[Dict[str, Any]], int]:
    """Rows minus harness-rejected ones and the `skipped` rows of the same
    (suite, rep) that only skipped because a rejected step failed."""
    rejected_reps = {(r.get("suite"), r.get("rep")) for r in rows if harness_rejected(r)}
    kept = []
    for r in rows:
        if harness_rejected(r):
            continue
        if (r.get("suite"), r.get("rep")) in rejected_reps and r.get("source") == "skipped":
            continue
        kept.append(r)
    return kept, len(rows) - len(kept)


def server_block(rows: List[Dict[str, Any]]) -> Dict[str, Any]:
    n_all = len(rows)
    rows, rejected = drop_harness_rejected(rows)

    def suite(name: str) -> List[Dict[str, Any]]:
        return [r for r in rows if r.get("suite") == name]

    return {"consistency": consistency_block(suite("consistency")),
            "sse": sse_block(suite("sse")),
            "import": import_block(suite("import")),
            "n": len(rows),
            "n_all": n_all,
            "harness_rejected_n": rejected}


# --------------------------------------------------------------------------
# Ledger: latency, reliability, cost (section 3)
# --------------------------------------------------------------------------


def latency_summary(rows: List[Dict[str, Any]]) -> Dict[str, Any]:
    ttft = [num(r.get("ttft_ms")) for r in rows]
    ttfr = [num(r.get("ttfr_ms")) for r in rows]
    e2e = [num(r.get("total_ms")) for r in rows]
    return {"n": len(rows), "ttft_p50_ms": pct(ttft, 50), "ttft_p95_ms": pct(ttft, 95),
            "ttft_n": count_nonnull(ttft), "ttfr_p50_ms": pct(ttfr, 50), "ttfr_n": count_nonnull(ttfr),
            "e2e_p50_ms": pct(e2e, 50), "e2e_p95_ms": pct(e2e, 95)}


def tok_rates(rows: List[Dict[str, Any]]) -> List[float]:
    out = []
    for r in rows:
        completion = num(r.get("completion_tokens"))
        total = num(r.get("total_ms"))
        if completion is None or total is None or completion < 16:
            continue
        t0 = num(r.get("ttfr_ms"))
        if t0 is None:
            t0 = num(r.get("ttft_ms"))
        if t0 is None or total - t0 < 50:
            continue
        out.append(completion / ((total - t0) / 1000.0))
    return out


def provider_shares(rows: List[Dict[str, Any]]) -> Dict[str, Any]:
    counts = Counter(str(r.get("provider")) for r in rows if r.get("provider"))
    total = sum(counts.values())
    return {p: {"calls": c, "share": rate(c, total)}
            for p, c in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))}


def ledger_block(rows: List[Dict[str, Any]]) -> Dict[str, Any]:
    """rows: this model's ledger rows, judge and budget-refused rows already removed."""
    n = len(rows)
    error_n = sum(1 for r in rows if r.get("error_kind") in ERROR_KINDS)
    timeout_n = sum(1 for r in rows if r.get("error_kind") == "timeout")
    refusal_n = sum(1 for r in rows if r.get("error_kind") == "refusal")
    retry_n = sum(1 for r in rows if (num(r.get("attempts")) or 1) > 1)
    success = [r for r in rows if is_success(r)]
    engine_success = [r for r in success if r.get("workload") == "engine"]
    engine = [r for r in rows if r.get("workload") == "engine"]

    reliability = {
        "n": n,
        "errors": error_n, "error_rate": rate(error_n, n),
        "timeouts": timeout_n, "timeout_rate": rate(timeout_n, n),
        "refusals": refusal_n, "refusal_rate": rate(refusal_n, n),
        "retries": retry_n, "retry_rate": rate(retry_n, n),
        "error_kinds": {str(k): v for k, v in Counter(r.get("error_kind") for r in rows
                                                       if r.get("error_kind")).items()},
        "reasoning_rejected": any(r.get("reasoning_rejected") is True for r in rows),
        "reasoning_rejected_calls": sum(1 for r in rows if r.get("reasoning_rejected") is True),
        "response_format_rejected": any(r.get("response_format_rejected") is True for r in rows),
        "response_format_rejected_calls": sum(1 for r in rows if r.get("response_format_rejected") is True),
        "response_format_types": {str(k): v for k, v in Counter(
            r.get("response_format_type") for r in rows if r.get("response_format_type")).items()},
        "providers": provider_shares(rows),
        "providers_n": sum(1 for r in rows if r.get("provider")),
    }

    cost_engine = sum(num(r.get("cost_usd")) or 0.0 for r in engine)
    cost_all = sum(num(r.get("cost_usd")) or 0.0 for r in rows)
    estimate_engine = sum(num(r.get("cost_usd")) or 0.0 for r in engine if r.get("cost_source") == "estimate")
    estimate_all = sum(num(r.get("cost_usd")) or 0.0 for r in rows if r.get("cost_source") == "estimate")
    actual_all = sum(num(r.get("cost_usd")) or 0.0 for r in rows if r.get("cost_source") == "usage")
    cost = {
        "n": n,
        "actual_usd_usage": actual_all,
        "total_usd_all_workloads": cost_all,
        "engine_usd": cost_engine if engine else None,
        "engine_calls": len(engine),
        "estimate_share_engine": rate(estimate_engine, cost_engine) if engine else None,
        "estimate_share_all": rate(estimate_all, cost_all),
        "by_workload_usd": {str(k): round(v, 8) for k, v in _sum_by(rows, "workload").items()},
        "prompt_tokens_per_call": mean(num(r.get("prompt_tokens")) for r in engine),
        "completion_tokens_per_call": mean(num(r.get("completion_tokens")) for r in engine),
        "reasoning_tokens_per_call": mean(num(r.get("reasoning_tokens")) for r in engine),
        "tokens_n": count_nonnull(num(r.get("prompt_tokens")) for r in engine),
    }
    return {
        "reliability": reliability,
        "latency": {
            "engine": latency_summary(engine_success),
            "all": latency_summary(success),
            "tok_s_engine_median": median(tok_rates(engine_success)),
            "tok_s_engine_n": len(tok_rates(engine_success)),
            "tok_s_all_median": median(tok_rates(success)),
            "tok_s_all_n": len(tok_rates(success)),
        },
        "cost": cost,
    }


def _sum_by(rows: List[Dict[str, Any]], field: str) -> Dict[str, float]:
    totals: Dict[str, float] = defaultdict(float)
    for r in rows:
        totals[str(r.get(field) or "unknown")] += num(r.get("cost_usd")) or 0.0
    return dict(totals)


# --------------------------------------------------------------------------
# Spend
# --------------------------------------------------------------------------


def spend_scope(rows: Optional[List[Dict[str, Any]]]) -> Dict[str, Any]:
    if rows is None:
        return {"present": False, "calls": None, "usd": None, "by_workload": {}}
    return {"present": True, "calls": len(rows),
            "usd": sum(num(r.get("cost_usd")) or 0.0 for r in rows),
            "by_workload": {k: round(v, 8) for k, v in _sum_by(rows, "workload").items()},
            "budget_refused_calls": sum(1 for r in rows if is_budget(r))}


def spend_summary(inputs: Inputs) -> Dict[str, Any]:
    pilot = spend_scope(inputs.pilot_calls)
    full = spend_scope(inputs.calls)
    combined_usd = (pilot["usd"] or 0.0) + (full["usd"] or 0.0) \
        if pilot["present"] or full["present"] else None
    combined_by: Dict[str, float] = defaultdict(float)
    for scope in (pilot, full):
        for wl, usd in scope["by_workload"].items():
            combined_by[wl] += usd
    superseded = spend_scope(inputs.superseded_calls)
    all_usd = (combined_usd or 0.0) + (superseded["usd"] or 0.0) \
        if combined_usd is not None or superseded["present"] else None
    all_by: Dict[str, float] = defaultdict(float, combined_by)
    for wl, usd in superseded["by_workload"].items():
        all_by[wl] += usd
    before = num((inputs.key_before or {}).get("usage")) if isinstance(inputs.key_before, dict) else None
    after = num((inputs.key_after or {}).get("usage")) if isinstance(inputs.key_after, dict) else None
    delta = after - before if before is not None and after is not None else None
    return {
        "pilot": pilot,
        "full": full,
        "superseded": superseded,
        "all_incl_superseded": {
            "present": all_usd is not None,
            "calls": (pilot["calls"] or 0) + (full["calls"] or 0) + (superseded["calls"] or 0)
            if all_usd is not None else None,
            "usd": all_usd, "by_workload": {k: round(v, 8) for k, v in all_by.items()}},
        "combined": {"present": pilot["present"] or full["present"],
                     "calls": (pilot["calls"] or 0) + (full["calls"] or 0)
                     if pilot["present"] or full["present"] else None,
                     "usd": combined_usd, "by_workload": {k: round(v, 8) for k, v in combined_by.items()}},
        "key": {
            "usage_before_usd": before,
            "usage_after_usd": after,
            "usage_delta_usd": delta,
            "limit_remaining_before": num((inputs.key_before or {}).get("limit_remaining"))
            if isinstance(inputs.key_before, dict) else None,
            "limit_remaining_after": num((inputs.key_after or {}).get("limit_remaining"))
            if isinstance(inputs.key_after, dict) else None,
            "ledger_combined_minus_key_delta_usd": (combined_usd - delta)
            if combined_usd is not None and delta is not None else None,
            "ledger_all_incl_superseded_minus_key_delta_usd": (all_usd - delta)
            if all_usd is not None and delta is not None else None,
        },
    }


# --------------------------------------------------------------------------
# Value score (section 4)
# --------------------------------------------------------------------------


def value_of(q: Optional[float], c: Optional[float], l_ms: Optional[float],
             qb: Optional[float], cb: Optional[float], lb: Optional[float],
             exps: Tuple[float, float, float]) -> Optional[float]:
    if None in (q, c, l_ms, qb, cb, lb):
        return None
    if qb <= 0 or l_ms <= 0 or lb <= 0:
        return None
    a, b, cc = exps
    c = max(c, COST_FLOOR)
    cb = max(cb, COST_FLOOR)
    return 100.0 * (q / qb) ** a / (((c / cb) ** b) * ((l_ms / lb) ** cc))


def rank_desc(values: Dict[str, Optional[float]]) -> Dict[str, Optional[int]]:
    ordered = sorted(((v, m) for m, v in values.items() if v is not None), key=lambda x: (-x[0], x[1]))
    ranks: Dict[str, Optional[int]] = {m: None for m in values}
    for i, (_, m) in enumerate(ordered, start=1):
        ranks[m] = i
    return ranks


def error_plus_timeout(block: Dict[str, Any]) -> Optional[float]:
    """Ledger error rate plus the larger of the ledger timeout rate and the
    engine-side timeout rate (engine timeouts never show in the ledger)."""
    err = block["reliability"]["error_rate"]
    to = block["reliability"]["timeout_rate"]
    if err is None or to is None:
        return None
    eng_to = block["engine"].get("engine_timeout_rate")
    return err + max(to, eng_to or 0.0)


def score_models(models: Dict[str, Dict[str, Any]], baseline: str) -> Dict[str, Any]:
    # Quality index, cost and latency for every model (inputs to the gates and Value).
    for block in models.values():
        engine, judge = block["engine"], block["judge"]
        s_idx = engine["S"]
        k = engine["macro"]["conformance"]
        jn = judge["Jn"]
        q = (WEIGHT_S * s_idx + WEIGHT_K * k + WEIGHT_JN * jn) \
            if None not in (s_idx, k, jn) else None
        block["quality"] = {"S": s_idx, "K": k, "Jn": jn, "Q": q}
        block["cost"]["C"] = max(COST_FLOOR, block["cost"]["engine_usd"] * 1000.0 / engine["n"]) \
            if block["cost"]["engine_usd"] is not None and engine["n"] else None
        block["latency"]["L"] = block["latency"]["engine"]["e2e_p50_ms"]

    # Gates (Value is null when any applies).
    for block in models.values():
        engine, judge = block["engine"], block["judge"]
        reasons: List[str] = []
        if block.get("run_status") == "skipped":
            reasons.append("skipped")
        elif engine["n"] == 0:
            reasons.append("no_data")
        sv = engine["macro"]["schema_valid"]
        if sv is not None and sv < GATE_SCHEMA_VALID:
            reasons.append("schema_valid")
        err_to = error_plus_timeout(block)
        if err_to is not None and err_to > GATE_ERROR_PLUS_TIMEOUT:
            reasons.append("unreliable")
        if (judge["judge_n"] or 0) < GATE_JUDGE_N:
            reasons.append("unjudged")
        if not reasons and any(p is None for p in (block["quality"]["Q"], block["cost"]["C"],
                                                   block["latency"]["L"])):
            reasons.append("missing_inputs")
        block["gate_reasons"] = reasons
        block["gated"] = reasons[0] if reasons else None

    # Baseline (raw Q, C, L). Fall back to the lower-median Q among ungated models.
    def usable(block: Dict[str, Any]) -> bool:
        return block["quality"]["Q"] is not None and block["cost"]["C"] is not None \
            and block["latency"]["L"] is not None

    base_used: Optional[str] = None
    baseline_note = None
    if baseline in models and usable(models[baseline]):
        base_used = baseline
    else:
        ungated = sorted((m for m, b in models.items() if not b["gate_reasons"] and usable(b)),
                         key=lambda m: (models[m]["quality"]["Q"], m))
        if ungated:
            base_used = ungated[(len(ungated) - 1) // 2]
            baseline_note = (f"{baseline} not usable (no run or missing inputs); "
                             f"used the lower-median Q model among ungated models")
        else:
            baseline_note = f"{baseline} not usable and no ungated model has complete inputs"
    qb = models[base_used]["quality"]["Q"] if base_used else None
    cb = models[base_used]["cost"]["C"] if base_used else None
    lb = models[base_used]["latency"]["L"] if base_used else None

    sensitivity: Dict[str, Dict[str, Optional[float]]] = {}
    for set_name, exps in EXPONENT_SETS.items():
        vals: Dict[str, Optional[float]] = {}
        for name, block in models.items():
            if block["gate_reasons"]:
                vals[name] = None
            else:
                vals[name] = value_of(block["quality"]["Q"], block["cost"]["C"], block["latency"]["L"],
                                      qb, cb, lb, exps)
        sensitivity[set_name] = vals

    for name, block in models.items():
        block["value"] = sensitivity["default"][name]
        block["value_sensitivity"] = {k: sensitivity[k][name] for k in EXPONENT_SETS}
        # Diagnostic only (not the rubric's Value): the default formula with the
        # gates ignored, so readers can see where a gated model would land.
        block["value_ungated"] = value_of(block["quality"]["Q"], block["cost"]["C"], block["latency"]["L"],
                                          qb, cb, lb, EXPONENT_SETS["default"])

    q_values = {m: b["quality"]["Q"] for m, b in models.items()}
    q_ranks = rank_desc(q_values)
    q_best = max((q for q in q_values.values() if q is not None), default=None)
    for name, block in models.items():
        block["rank_quality"] = q_ranks[name]
        q = block["quality"]["Q"]
        block["within_10pct_best_q"] = (q is not None and q_best is not None and q >= BEST_Q_BAND * q_best)
        block["recommendable"] = bool(block["value"] is not None and block["within_10pct_best_q"])

    value_ranks = {k: rank_desc(sensitivity[k]) for k in EXPONENT_SETS}
    for name, block in models.items():
        block["rank_value"] = value_ranks["default"][name]
        block["value_rank_sensitivity"] = {k: value_ranks[k][name] for k in EXPONENT_SETS}

    return {
        "baseline_requested": baseline,
        "baseline_used": base_used,
        "baseline_note": baseline_note,
        "baseline_raw": {"Q": qb, "C": cb, "L": lb},
        "exponents": {k: list(v) for k, v in EXPONENT_SETS.items()},
        "weights": {"S": WEIGHT_S, "K": WEIGHT_K, "Jn": WEIGHT_JN},
        "rankings": {k: [m for _, m in sorted(((v, m) for m, v in sensitivity[k].items() if v is not None),
                                              key=lambda x: (-x[0], x[1]))]
                     for k in EXPONENT_SETS},
        "values": sensitivity,
        "gates": {"schema_valid_below": GATE_SCHEMA_VALID,
                  "error_plus_timeout_above": GATE_ERROR_PLUS_TIMEOUT,
                  "judge_n_below": GATE_JUDGE_N},
    }


# --------------------------------------------------------------------------
# Assembly
# --------------------------------------------------------------------------


def analyse(out: str, judge_path: str, key_path: str, seed_arg: Optional[str], baseline: str) -> Dict[str, Any]:
    inputs = Inputs(out, judge_path, key_path)
    kw_categories = inputs.keyword_categories()
    judge_model = Counter(str(r.get("judge")) for r in (inputs.judge_rows or [])
                          if r.get("judge") and r.get("auto") is not True
                          and r.get("judge") != "auto").most_common(1)
    seed = seed_arg or next((str(r.get("seed")) for r in (inputs.judge_rows or []) if r.get("seed")), None) or SEED

    # Universe of models: run order, roster, and everything the data mentions.
    universe: List[str] = []
    seen = set()

    def add(m: Optional[str]) -> None:
        if m and m not in seen:
            seen.add(m)
            universe.append(m)

    run_models = inputs.run.get("models") if isinstance(inputs.run.get("models"), dict) else {}
    for m in inputs.run.get("order") or []:
        add(m if isinstance(m, str) else None)
    for m in run_models:
        add(m)
    if isinstance(inputs.roster, dict) and isinstance(inputs.roster.get("models"), list):
        for entry in inputs.roster["models"]:
            if isinstance(entry, dict):
                add(entry.get("id"))
    elif isinstance(inputs.roster, list):
        for entry in inputs.roster:
            if isinstance(entry, dict):
                add(entry.get("id"))

    samples = inputs.samples or []
    server_rows = inputs.server or []
    ledger = [r for r in (inputs.calls or []) if not is_judge_ledger_row(r)]
    samples_by_model: Dict[str, List[Dict[str, Any]]] = defaultdict(list)
    unattributed_samples = 0
    for r in samples:
        m = row_model(r)
        if m is None:
            unattributed_samples += 1
            continue
        samples_by_model[m].append(r)
    ledger_by_model: Dict[str, List[Dict[str, Any]]] = defaultdict(list)
    for r in ledger:
        m = row_model(r)
        if m is not None and not is_budget(r):
            ledger_by_model[m].append(r)
    server_by_model: Dict[str, List[Dict[str, Any]]] = defaultdict(list)
    for r in server_rows:
        m = row_model(r)
        if m is not None:
            server_by_model[m].append(r)
    for source in (samples_by_model, ledger_by_model, server_by_model):
        for m in source:
            add(m)
    key_items = inputs.judge_key or []
    for item in key_items:
        add(item_model(item))

    graded = attach_grades(samples, inputs.grades)
    has_grades = inputs.grades is not None
    sample_index: Dict[Tuple[str, str, str, str], Dict[str, Any]] = {}
    for m, rows in samples_by_model.items():
        for r in rows:
            sample_index.setdefault(key_of(m, r), r)
    grade_index: Dict[Tuple[str, str, str, str], Dict[str, Any]] = {}
    for m, pairs_ in graded.items():
        for s, g in pairs_:
            if g is not None:
                grade_index.setdefault(key_of(m, s), g)
    primary, order_mad, order_pairs = judge_primary_rows(inputs.judge_rows)
    judge_present = inputs.judge_rows is not None

    models: Dict[str, Dict[str, Any]] = {}
    for m in universe:
        items = graded.get(m, [])
        engine = engine_block(items, has_grades, kw_categories)
        ledger_rows = ledger_by_model.get(m, [])
        block = {
            "model": m,
            "run_status": (run_models.get(m) or {}).get("status") if isinstance(run_models.get(m), dict) else None,
            "engine": engine,
            "judge": judge_block(m, key_items, primary, sample_index,
                                 judge_present, inputs.judge_key_present, grade_index),
            "server": server_block(server_by_model.get(m, [])),
        }
        block.update(ledger_block(ledger_rows))
        block["ledger_rows"] = len(ledger_rows)
        models[m] = block

    scoring = score_models(models, baseline)
    leaderboard = []
    for m, block in models.items():
        leaderboard.append({
            "model": m,
            "rank_value": block["rank_value"],
            "value": block["value"],
            "gated": block["gated"],
            "rank_quality": block["rank_quality"],
            "Q": block["quality"]["Q"],
            "S": block["quality"]["S"],
            "K": block["quality"]["K"],
            "Jn": block["quality"]["Jn"],
            "C_usd_per_1k": block["cost"]["C"],
            "L_p50_ms": block["latency"]["L"],
            "schema_valid": block["engine"]["macro"]["schema_valid"],
            "error_plus_timeout": error_plus_timeout(block),
            "value_ungated": block["value_ungated"],
            "judge_n": block["judge"]["judge_n"],
            "engine_n": block["engine"]["n"],
            "within_10pct_best_q": block["within_10pct_best_q"],
            "recommendable": block["recommendable"],
        })
    leaderboard.sort(key=lambda row: (row["value"] is None,
                                      -(row["value"] or 0.0),
                                      -(row["Q"] if row["Q"] is not None else -1.0),
                                      row["model"]))

    plan = judge_plan(samples_by_model, seed)
    key_check = None
    if inputs.judge_key is not None:
        planned = {(m, t["contract"], t["target"], str(t["rep"])) for t in plan["targets"] for m in plan["models"]}
        keyed = {(item_model(i), str(i.get("contract")), str(i.get("target")), str(i.get("rep")))
                 for i in inputs.judge_key}
        id_mismatch = 0
        for item in inputs.judge_key:
            m = item_model(item)
            if m is None or item.get("opaque_id") is None:
                continue
            expect = hashlib.sha256(
                f"{seed}|{m}|{item.get('contract')}|{item.get('target')}|{item.get('rep')}".encode("utf-8")
            ).hexdigest()[:OPAQUE_LEN]
            if expect != str(item.get("opaque_id")):
                id_mismatch += 1
        key_check = {"key_items": len(inputs.judge_key), "planned_items": len(planned),
                     "missing_from_key": len(planned - keyed), "extra_in_key": len(keyed - planned),
                     "opaque_id_mismatch": id_mismatch}

    judge_summary = {
        "judge_model": judge_model[0][0] if judge_model else None,
        "seed": seed,
        "order_check_mad": order_mad,
        "order_check_pairs": order_pairs,
        "order_check_noisy": (order_mad > 0.5) if order_mad is not None else None,
        "plan": plan,
        "key_check": key_check,
        "items_judged": sum(b["judge"]["judge_items"] for b in models.values()),
    }

    spend = spend_summary(inputs)
    return {
        "meta": {
            "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "out": out,
            "inputs": inputs.status,
            "models_universe": universe,
            "samples_unattributed": unattributed_samples,
            "run": {k: inputs.run.get(k) for k in ("started", "finished", "repeat", "parallel", "concurrency",
                                                   "budget_usd", "manifest", "aborted")} if inputs.run else None,
            "skipped": inputs.run.get("skipped") if inputs.run else None,
            "seed": seed,
            "judge_model": judge_summary["judge_model"],
        },
        "baseline": {k: v for k, v in scoring.items() if k in ("baseline_requested", "baseline_used",
                                                                 "baseline_note", "baseline_raw")},
        "scoring": {k: v for k, v in scoring.items() if k not in ("baseline_requested", "baseline_used",
                                                                  "baseline_note", "baseline_raw", "values")},
        "value_sensitivity": {"exponents": scoring["exponents"], "rankings": scoring["rankings"],
                              "values": scoring["values"]},
        "judge": judge_summary,
        "leaderboard": leaderboard,
        "models": models,
        "spend": spend,
        "keyword_categories": kw_categories,
    }


# --------------------------------------------------------------------------
# Output: results.json, results.csv, tables.md
# --------------------------------------------------------------------------


def flatten(prefix: str, obj: Any, sink: Dict[str, Any]) -> None:
    if isinstance(obj, dict):
        for key, value in obj.items():
            flatten(f"{prefix}.{key}" if prefix else str(key), value, sink)
    elif isinstance(obj, list):
        sink[prefix] = json.dumps(obj, ensure_ascii=False)
    else:
        sink[prefix] = "" if obj is None else obj


def write_csv(path: str, results: Dict[str, Any]) -> None:
    rows = []
    columns: List[str] = ["model"]
    for m, block in results["models"].items():
        flat: Dict[str, Any] = {"model": m}
        flatten("", {k: v for k, v in block.items() if k != "model"}, flat)
        rows.append(flat)
        for key in flat:
            if key not in columns:
                columns.append(key)
    with open(path, "w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=columns, restval="", extrasaction="ignore")
        writer.writeheader()
        for row in rows:
            writer.writerow(row)


def fmt(value: Any, kind: str = "num", nd: int = 3) -> str:
    if value is None:
        return "—"
    if isinstance(value, bool):
        return "yes" if value else "no"
    if isinstance(value, str):
        return value
    if kind == "pct":
        return f"{100.0 * value:.1f}%"
    if kind == "ms":
        return f"{value:,.0f}"
    if kind == "usd":
        return f"{value:.4f}"
    if kind == "int":
        return f"{int(value):,}"
    return f"{value:.{nd}f}"


def md_table(headers: List[str], rows: List[List[Any]]) -> List[str]:
    lines = ["| " + " | ".join(headers) + " |", "|" + "|".join("---" for _ in headers) + "|"]
    for row in rows:
        lines.append("| " + " | ".join(str(c).replace("|", "\\|") for c in row) + " |")
    return lines


def write_tables(path: str, results: Dict[str, Any]) -> None:
    models = results["models"]
    meta = results["meta"]
    lines: List[str] = []
    lines.append("# LLM model benchmark: tables")
    lines.append("")
    lines.append(f"Generated {meta['generated_at']} from `{meta['out']}`. Seed `{meta['seed']}`, "
                 f"judge `{meta.get('judge_model') or '—'}`, baseline used "
                 f"`{results['baseline']['baseline_used'] or '—'}`"
                 f" (requested `{results['baseline']['baseline_requested']}`).")
    if results["baseline"]["baseline_note"]:
        lines.append("")
        lines.append(f"Baseline note: {results['baseline']['baseline_note']}")
    lines.append("")
    lines.append("Conventions: `—` means null (missing input or zero denominator). Every table has an n "
                 "column or states its n. N = engine samples per model; n in the latency table = "
                 "successful calls; judge_n = judge items incl. auto-scored ones (rubric 1.5).")
    lines.append("")

    lines.append("## 1. Leaderboard (sorted by Value; quality rank alongside)")
    lines.append("")
    lines.append(f"n = engine samples N per model (column `N`); judge_n = judge items per model (auto included).")
    lines.append("")
    rows = []
    for row in results["leaderboard"]:
        rows.append([
            fmt(row["rank_value"], "int") if row["rank_value"] else "—", row["model"],
            fmt(row["value"], nd=1), row["gated"] or "", fmt(row["Q"]), fmt(row["rank_quality"], "int")
            if row["rank_quality"] else "—", fmt(row["S"]), fmt(row["K"]), fmt(row["Jn"]),
            fmt(row["C_usd_per_1k"], "usd"), fmt(row["L_p50_ms"], "ms"), fmt(row["schema_valid"], "pct"),
            fmt(row["error_plus_timeout"], "pct"), fmt(row["judge_n"], "int"), fmt(row["engine_n"], "int"),
            "yes" if row["within_10pct_best_q"] else "no", "yes" if row["recommendable"] else "no",
            fmt(row.get("value_ungated"), nd=1),
        ])
    lines += md_table(["Value rank", "Model", "Value", "Gate", "Q", "Q rank", "S", "K", "Jn", "C $/1k",
                       "L p50 ms", "Schema valid", "Err+TO", "judge_n", "N", "Within 10% of best Q",
                       "Recommendable", "Value if ungated (diagnostic)"], rows)
    lines.append("")
    lines.append("Err+TO = ledger error rate + max(ledger timeout rate, engine-side timeout rate from "
                 "samples). `Value if ungated` applies the default formula ignoring the gates; it is not "
                 "the rubric's Value.")
    lines.append("")

    lines.append("## 2. Quality components")
    lines.append("")
    lines.append("n = engine samples N (macro rates over api types); judge items = blinded judge items "
                 "(auto items included in J).")
    lines.append("")
    rows = []
    for m, b in models.items():
        e, j = b["engine"], b["judge"]
        rows.append([m, fmt(e["n"], "int"), fmt(e["macro"]["first_try"]), fmt(e["macro"]["schema_valid"]),
                     fmt(e["fallback"], "pct"), fmt(e["S"]), fmt(e["macro"]["conformance"]),
                     fmt(j["J"], nd=2), fmt(j["Jn"]), fmt(j["realism"], nd=2), fmt(j["coherence"], nd=2),
                     fmt(j["judge_n"], "int"), fmt(j["judge_auto"], "int"), fmt(j["judge_missing"], "int"),
                     fmt(j.get("judge_excluded_valid_empty"), "int"),
                     fmt(e.get("fallback_truncated_max_tokens"), "int"), fmt(e.get("engine_timeouts"), "int"),
                     fmt(b["quality"]["Q"])])
    lines += md_table(["Model", "N", "First try (FT)", "Schema valid (SV)", "Fallback", "S", "K (conformance)",
                       "J", "Jn", "Realism", "Coherence", "judge_n", "auto", "missing", "excl. valid {}",
                       "fallback: max_tokens", "fallback: engine timeout", "Q"], rows)
    lines.append("")
    lines.append("excl. valid {} = judge items whose body was a schema-valid `{}` written by the model; "
                 "they were not shown to the judge and are left out of J instead of being auto-scored 1. "
                 "fallback: max_tokens / engine timeout = fallback samples caused by output truncation at "
                 "the engine's max_tokens or by the engine's LLM timeout (sample counts).")
    lines.append("")

    for label, key in (("api type", "by_api_type"), ("domain", "by_domain")):
        lines.append(f"## 3{'a' if key == 'by_api_type' else 'b'}. Per model x {label}")
        lines.append("")
        lines.append("n = engine samples in that cell; conformance is over its non-fallback samples "
                     "(conf n in the last column); p50 is the mb-evals latency of those samples.")
        lines.append("")
        rows = []
        for m, b in models.items():
            for cell in sorted(b["engine"][key]):
                c = b["engine"][key][cell]
                rows.append([m, cell, fmt(c["n"], "int"), fmt(c["first_try"]), fmt(c["schema_valid"]),
                             fmt(c["fallback"], "pct"), fmt(c["conformance"]), fmt(c["conformance_n"], "int"),
                             fmt(c["p50_latency_ms"], "ms")])
        lines += md_table(["Model", label, "n", "First try", "Schema valid", "Fallback", "Conformance",
                           "conf n", "p50 ms"], rows)
        lines.append("")

    lines.append("## 4. Conformance by keyword category (non-fallback samples)")
    lines.append("")
    lines.append("n = keyword checks in each cell (`checked`); pass rate = passed / checked.")
    lines.append("")
    cats = list(results["keyword_categories"].keys())
    rows = []
    for m, b in models.items():
        kw = b["engine"]["conformance_keywords"]
        rows.append([m] + [f"{fmt(kw.get(c, {}).get('pass_rate'))} (n={fmt(kw.get(c, {}).get('checked', 0), 'int')})"
                           for c in cats])
    lines += md_table(["Model"] + cats, rows)
    lines.append("")

    lines.append("## 5. Latency (proxy-measured LLM call times)")
    lines.append("")
    lines.append("n (engine) = successful engine calls; n (all) = successful calls, all workloads; "
                 "n tok = calls used for the tok/s median. L = e2e p50 over engine calls.")
    lines.append("")
    rows = []
    for m, b in models.items():
        eng, al, lat = b["latency"]["engine"], b["latency"]["all"], b["latency"]
        rows.append([m, fmt(eng["n"], "int"), fmt(eng["ttft_p50_ms"], "ms"), fmt(eng["ttft_p95_ms"], "ms"),
                     fmt(eng["ttfr_p50_ms"], "ms"), fmt(eng["e2e_p50_ms"], "ms"), fmt(eng["e2e_p95_ms"], "ms"),
                     fmt(al["n"], "int"), fmt(al["e2e_p50_ms"], "ms"), fmt(al["e2e_p95_ms"], "ms"),
                     fmt(lat["tok_s_engine_median"], nd=1), fmt(lat["tok_s_all_median"], nd=1),
                     fmt(lat["tok_s_all_n"], "int")])
    lines += md_table(["Model", "n (engine)", "TTFT p50", "TTFT p95", "TTFR p50", "e2e p50 (engine)",
                       "e2e p95 (engine)", "n (all)", "e2e p50 (all)", "e2e p95 (all)", "tok/s (engine)",
                       "tok/s (all)", "n tok"], rows)
    lines.append("")

    lines.append("## 6. Reliability")
    lines.append("")
    lines.append("n = ledger rows for the model, budget-refused rows excluded. Provider shares are over "
                 "calls with a served provider.")
    lines.append("")
    rows = []
    for m, b in models.items():
        r = b["reliability"]
        providers = ", ".join(f"{p} {fmt(v['share'], 'pct')}" for p, v in r["providers"].items()) or "—"
        rf_types = ", ".join(f"{k}:{v}" for k, v in r["response_format_types"].items()) or "—"
        rows.append([m, fmt(r["n"], "int"), fmt(r["errors"], "int"), fmt(r["error_rate"], "pct"),
                     fmt(r["timeouts"], "int"), fmt(r["timeout_rate"], "pct"),
                     f"{fmt(b['engine'].get('engine_timeouts'), 'int')} ({fmt(b['engine'].get('engine_timeout_rate'), 'pct')})",
                     fmt(r["refusal_rate"], "pct"),
                     fmt(r["retry_rate"], "pct"), fmt(r["reasoning_rejected"]), fmt(r["response_format_rejected"]),
                     rf_types, providers, fmt(r["providers_n"], "int")])
    lines += md_table(["Model", "n", "errors", "error rate", "timeouts", "timeout rate",
                       "engine timeouts (of N)", "refusal rate",
                       "retry rate", "reasoning toggle rejected", "JSON mode rejected", "response_format sent",
                       "served providers (share)", "n providers"], rows)
    lines.append("")

    lines.append("## 7. Cost")
    lines.append("")
    lines.append("n = engine calls (C, tokens per call); total and actual spend cover all ledger rows of the model.")
    lines.append("")
    rows = []
    for m, b in models.items():
        c = b["cost"]
        rows.append([m, fmt(c["engine_calls"], "int"), fmt(c["actual_usd_usage"], "usd"),
                     fmt(c["total_usd_all_workloads"], "usd"), fmt(c["engine_usd"], "usd"),
                     fmt(c["C"], "usd"), fmt(c["estimate_share_engine"], "pct"),
                     fmt(c["prompt_tokens_per_call"], "ms"), fmt(c["completion_tokens_per_call"], "ms"),
                     fmt(c["reasoning_tokens_per_call"], "ms"), fmt(c["tokens_n"], "int")])
    lines += md_table(["Model", "engine calls", "actual $ (usage)", "total $ (all workloads)",
                       "engine $", "C $/1k responses", "estimate share (engine)", "prompt tok/call",
                       "completion tok/call", "reasoning tok/call", "n tok"], rows)
    lines.append("")

    lines.append("## 8. Server suites")
    lines.append("")
    lines.append("n = server checks per model (consistency rows, SSE rows, import inputs); import "
                 "recall/precision are over the import inputs with an expected list.")
    lines.append("")
    rows = []
    for m, b in models.items():
        s = b["server"]
        cons, sse, imp = s["consistency"], s["sse"], s["import"]
        rows.append([m, fmt(cons["n"], "int"), fmt(cons["case_ok_rate"], "pct"),
                     f"{fmt(cons['field_agreement'], 'pct')} (fields {fmt(cons['fields_checked'], 'int')})",
                     fmt(sse["n"], "int"), fmt(sse["ok_rate"], "pct"), fmt(sse["ttfe_p50_ms"], "ms"),
                     fmt(imp["n"], "int"), fmt(imp["draft_valid_rate"], "pct"),
                     f"{fmt(imp['recall_macro'])} / {fmt(imp['recall_micro'])}",
                     f"{fmt(imp['precision_macro'])} / {fmt(imp['precision_micro'])}",
                     fmt(imp["duration_mean_ms"], "ms"), fmt(imp["warnings_mean"], nd=1),
                     fmt(s.get("harness_rejected_n"), "int")])
    lines += md_table(["Model", "consistency n", "case ok", "field agreement", "SSE n", "SSE ok",
                       "SSE TTFE p50", "import n", "draft valid", "recall macro / micro",
                       "precision macro / micro", "import mean ms", "warnings mean", "harness-rejected rows"], rows)
    lines.append("")
    lines.append("harness-rejected rows = server rows Mockingbird refused before any LLM call (the bench's "
                 "own request was invalid, e.g. `@` from a variant id in the session header) plus the rows "
                 "skipped because of them; they are left out of every rate above.")
    lines.append("")
    lines.append("Consistency cases by name (n, ok):")
    lines.append("")
    rows = []
    for m, b in models.items():
        for case, c in b["server"]["consistency"]["by_case"].items():
            rows.append([m, case, fmt(c["n"], "int"), fmt(c["ok_rate"], "pct")])
    lines += md_table(["Model", "case", "n", "ok rate"], rows or [["—", "—", "0", "—"]])
    lines.append("")

    lines.append("## 9. Value sensitivity")
    lines.append("")
    lines.append("Value = 100 (Q/Qb)^a / ((C/Cb)^b (L/Lb)^c). n = models with a Value under each set "
                 "(gated models show —). Default (a,b,c) = (2, 0.5, 0.5).")
    lines.append("")
    sens = results["value_sensitivity"]
    rows = []
    for m in models:
        vals = sens["values"]
        rows.append([m] + [f"{fmt(vals[k].get(m), nd=1)} (rank {fmt(models[m]['value_rank_sensitivity'][k], 'int') if models[m]['value_rank_sensitivity'][k] else '—'})"
                           for k in EXPONENT_SETS])
    lines += md_table(["Model"] + [f"({k})" for k in EXPONENT_SETS], rows)
    lines.append("")

    lines.append("## 10. Spend")
    lines.append("")
    spend = results["spend"]
    rows = []
    for label, scope in (("pilot ledger", spend["pilot"]), ("full ledger", spend["full"]),
                         ("combined (pilot + full)", spend["combined"]),
                         ("superseded ledger (redone runs)", spend.get("superseded", {})),
                         ("all incl. superseded", spend.get("all_incl_superseded", {}))):
        rows.append([label, fmt(scope.get("calls"), "int"), fmt(scope.get("usd"), "usd")])
    lines += md_table(["Scope", "n (calls)", "USD (ledger cost_usd sum)"], rows)
    lines.append("")
    wl_rows = []
    for label, scope in (("pilot", spend["pilot"]), ("full", spend["full"]), ("combined", spend["combined"]),
                         ("superseded", spend.get("superseded", {}))):
        for wl, usd in sorted(scope.get("by_workload", {}).items()):
            wl_rows.append([label, wl, fmt(usd, "usd")])
    if wl_rows:
        lines.append("By workload (n = ledger rows per scope is in the table above):")
        lines.append("")
        lines += md_table(["Scope", "workload", "USD"], wl_rows)
        lines.append("")
    key = spend["key"]
    lines.append(f"Key usage delta (key-after.usage - key-before.usage): {fmt(key['usage_delta_usd'], 'usd')} USD "
                 f"(before {fmt(key['usage_before_usd'], 'usd')}, after {fmt(key['usage_after_usd'], 'usd')}; "
                 f"ledger combined minus key delta {fmt(key['ledger_combined_minus_key_delta_usd'], 'usd')}; "
                 f"ledger incl. superseded minus key delta "
                 f"{fmt(key.get('ledger_all_incl_superseded_minus_key_delta_usd'), 'usd')}).")
    lines.append("")

    lines.append("## 11. Judge plan and checks")
    lines.append("")
    jd = results["judge"]
    plan = jd["plan"]
    lines.append(f"n = judge items {jd['items_judged']}; planned targets {len(plan['targets'])}; judge model "
                 f"`{jd['judge_model'] or '—'}`; seed `{jd['seed']}`; order-check MAD "
                 f"{fmt(jd['order_check_mad'])} over {jd['order_check_pairs']} pairs"
                 f" (noisy: {fmt(jd['order_check_noisy'])}).")
    lines.append("")
    rows = [[t["stratum"], t["contract"], t["target"], t["api_type"] or "—", t["domain"] or "—",
             fmt(t["rep"], "int") if t["rep"] is not None else "—"] for t in plan["targets"]]
    lines += md_table(["stratum", "contract", "target", "api_type", "domain", "rep"], rows or [["—"] * 6])
    lines.append("")
    if plan["strata_skipped"]:
        lines.append(f"Skipped strata: {', '.join(plan['strata_skipped'])}.")
        lines.append("")
    if jd["key_check"]:
        kc = jd["key_check"]
        lines.append(f"judge-key check: {kc['key_items']} key items vs {kc['planned_items']} planned, "
                     f"missing {kc['missing_from_key']}, extra {kc['extra_in_key']}, "
                     f"opaque_id mismatches {kc['opaque_id_mismatch']}.")
        lines.append("")
    else:
        lines.append("judge-key.json missing: no judge scores can be attributed to models.")
        lines.append("")

    lines.append("## Gates")
    lines.append("")
    lines.append("A model's Value is null when gated: skipped, no_data, schema_valid (SV < 0.90), unreliable "
                 "(error + timeout > 0.10), unjudged (judge_n < 5), missing_inputs. n = engine samples N.")
    lines.append("")
    rows = [[m, fmt(b["engine"]["n"], "int"), ", ".join(b["gate_reasons"]) or "none"]
            for m, b in models.items()]
    lines += md_table(["Model", "N", "gate reasons"], rows)
    lines.append("")

    with open(path, "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines) + "\n")


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description="Aggregate an LLM bench run into results.json, "
                                                 "results.csv and tables.md.")
    parser.add_argument("--out", required=True, help="run directory (e.g. docs/benchmarks/2026-10-29)")
    parser.add_argument("--judge", help="judge.jsonl (default: <out>/judge.jsonl)")
    parser.add_argument("--judge-key", dest="judge_key", help="judge-key.json (default: <out>/judge-key.json)")
    parser.add_argument("--seed", help="judge seed (default: from judge.jsonl, else %s)" % SEED)
    parser.add_argument("--baseline", default=BASELINE, help="baseline model id (default %(default)s)")
    args = parser.parse_args(argv)

    out = os.path.abspath(args.out)
    os.makedirs(out, exist_ok=True)
    judge_path = os.path.abspath(args.judge) if args.judge else os.path.join(out, "judge.jsonl")
    key_path = os.path.abspath(args.judge_key) if args.judge_key else os.path.join(out, "judge-key.json")

    results = analyse(out, judge_path, key_path, args.seed, args.baseline)

    with open(os.path.join(out, "results.json"), "w", encoding="utf-8") as handle:
        json.dump(results, handle, indent=2, ensure_ascii=False, default=str)
        handle.write("\n")
    write_csv(os.path.join(out, "results.csv"), results)
    write_tables(os.path.join(out, "tables.md"), results)

    print(f"aggregated {len(results['models'])} models -> {out}/results.json, results.csv, tables.md",
          file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
