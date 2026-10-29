#!/usr/bin/env python3
"""Live terminal view of a benchmark run, for watching and for recordings.

    python3 scripts/llm-bench/live.py --out docs/benchmarks/2026-10-29          # full run
    python3 scripts/llm-bench/live.py --out docs/benchmarks/2026-10-29 --pilot  # pilot

Reads only files the run writes (the proxy ledger, `run.json`, the
mb-evals samples), never the network or the environment, so it can run in a
second terminal or next to `run.py` under a recorder. Redraws once a second
until `run.json` says the run finished (or Ctrl-C). `--once` prints one
frame and exits. Standard library only.
"""

import argparse
import json
import os
import statistics
import sys
import time
from typing import Any, Dict, List, Optional

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

BAR = 30
RESET, DIM, BOLD = "\033[0m", "\033[2m", "\033[1m"
GREEN, YELLOW, RED, CYAN = "\033[32m", "\033[33m", "\033[31m", "\033[36m"


def read_jsonl(path: str) -> List[Dict[str, Any]]:
    out: List[Dict[str, Any]] = []
    try:
        with open(path) as f:
            for line in f:
                line = line.strip()
                if line:
                    try:
                        out.append(json.loads(line))
                    except ValueError:
                        pass  # a line still being written
    except OSError:
        pass
    return out


def read_json(path: str) -> Optional[Dict[str, Any]]:
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def targets(manifest: Optional[Dict[str, Any]]) -> int:
    if not manifest:
        return 0
    total = 0
    for e in manifest.get("entries", []):
        ops = e.get("operations") or []
        total += len(ops) if ops else int(e.get("max_ops") or 10)
    return total


def p50(values: List[float]) -> Optional[float]:
    return statistics.median(values) if values else None


def bar(frac: float, width: int = BAR, color: str = GREEN) -> str:
    frac = max(0.0, min(1.0, frac))
    n = int(round(frac * width))
    return f"{color}{'█' * n}{RESET}{DIM}{'░' * (width - n)}{RESET}"


def ms(v: Optional[float]) -> str:
    return "—" if v is None else (f"{v / 1000:.1f}s" if v >= 1000 else f"{v:.0f}ms")


def frame(out: str, pilot: bool, budget: float, stop: float) -> (str, bool):
    base = os.path.join(out, "pilot") if pilot else out
    ledger = read_jsonl(os.path.join(base, "calls.jsonl"))
    prior = read_jsonl(os.path.join(out, "calls.jsonl" if pilot else os.path.join("pilot", "calls.jsonl")))
    state = read_json(os.path.join(out, "run.json")) if not pilot else None
    repeat = (state or {}).get("repeat", 1 if pilot else 3)
    manifest = read_json(os.path.join(base, "manifest.slice.json")) if pilot else None
    if manifest is None:
        manifest = read_json((state or {}).get("manifest") or os.path.join("docs", "benchmarks", "workloads.json"))
    expected = targets(manifest) * repeat

    spent = sum(float(r.get("cost_usd") or 0) for r in ledger + prior)
    tripped = os.path.exists(os.path.join(base, "BUDGET_TRIPPED"))
    by_model: Dict[str, List[Dict[str, Any]]] = {}
    for r in ledger:
        by_model.setdefault(r.get("model_requested") or r.get("model_slug") or "?", []).append(r)
    order: List[str] = list((state or {}).get("order") or [])
    order += [m for m in by_model if m not in order]

    lines = []
    title = "Mockingbird model bench — pilot" if pilot else "Mockingbird model bench"
    lines.append(f"{BOLD}{title}{RESET}  {DIM}{time.strftime('%H:%M:%S')}{RESET}")
    color = RED if tripped or spent >= stop else (YELLOW if spent >= 0.7 * stop else GREEN)
    lines.append(
        f"OpenRouter spend {bar(spent / budget, 40, color)} {BOLD}${spent:.3f}{RESET} / ${budget:.0f}"
        f"  {DIM}hard stop ${stop:.2f}{RESET}"
        + (f"  {RED}{BOLD}BUDGET GUARD TRIPPED{RESET}" if tripped else "")
    )
    lines.append("")
    head = f"{'model':34} {'status':8} {'progress':{BAR + 8}} {'valid':>6} {'fallbk':>6} {'TTFT':>6} {'p50':>6} {'err':>4} {'cost':>8}"
    lines.append(f"{DIM}{head}{RESET}")
    for m in order:
        recs = by_model.get(m, [])
        st = ((state or {}).get("models") or {}).get(m, {})
        status = st.get("status") or ("running" if recs else "pending")
        slug = m.replace("/", "__")
        sdir = os.path.join(base, slug) if pilot else os.path.join(out, "engine", slug)
        samples = read_jsonl(os.path.join(sdir, "samples.jsonl"))
        n = len(samples)
        valid = sum(1 for s in samples if s.get("source") in ("llm", "repaired"))
        fallback = sum(1 for s in samples if s.get("source") == "fallback")
        engine = [r for r in recs if not r.get("error_kind")]
        ttft = p50([float(r["ttft_ms"]) for r in engine if r.get("ttft_ms") is not None])
        e2e = p50([float(r["total_ms"]) for r in engine if r.get("total_ms") is not None])
        errs = sum(1 for r in recs if r.get("error_kind"))
        cost = sum(float(r.get("cost_usd") or 0) for r in recs)
        frac = (n / expected) if expected else 0.0
        if status in ("done", "partial") and not pilot:
            frac = 1.0 if status == "done" else frac
        pcolor = {"done": GREEN, "running": CYAN, "partial": YELLOW, "failed": RED, "skipped": DIM}.get(status, DIM)
        vr = f"{100 * valid / n:.0f}%" if n else "—"
        fr = f"{100 * fallback / n:.0f}%" if n else "—"
        prog = f"{bar(frac, BAR, pcolor)} {n:>3}/{expected:<3}"
        lines.append(
            f"{m[:34]:34} {pcolor}{status:8}{RESET} {prog} {vr:>6} {fr:>6} {ms(ttft):>6} {ms(e2e):>6} "
            f"{(RED if errs else '') + str(errs) + RESET:>{4 + len(RESET) + (len(RED) if errs else 0)}} ${cost:>7.4f}"
        )
    lines.append("")
    lines.append(f"{DIM}valid = schema-valid (first try or repaired) · TTFT/p50 from the proxy ledger · "
                 f"{len(ledger)} calls{RESET}")
    # The pilot writes no run state: a recorder passes --until-file instead.
    finished = bool(state and state.get("finished"))
    return "\n".join(lines), finished


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--out", default=os.path.join("docs", "benchmarks", "2026-10-29"))
    p.add_argument("--pilot", action="store_true")
    p.add_argument("--budget-usd", type=float, default=10.0, help="the bar's full scale")
    p.add_argument("--stop-usd", type=float, default=9.0, help="the proxy's hard stop")
    p.add_argument("--once", action="store_true")
    p.add_argument("--until-file", help="exit once this file exists (e.g. a run's exit marker)")
    args = p.parse_args()
    try:
        while True:
            text, finished = frame(args.out, args.pilot, args.budget_usd, args.stop_usd)
            if args.once:
                print(text)
                return 0
            sys.stdout.write("\033[H\033[2J" + text + "\n")
            sys.stdout.flush()
            # With --until-file only that file ends the view: run.json may
            # still hold an earlier invocation's `finished`.
            if (finished and not args.until_file) or (args.until_file and os.path.exists(args.until_file)):
                return 0
            time.sleep(1.0)
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    sys.exit(main())
