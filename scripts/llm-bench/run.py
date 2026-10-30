#!/usr/bin/env python3
"""Orchestrator for the LLM model benchmark.

    source ~/.zshenv >/dev/null 2>&1   # OPENROUTER_API_KEY, never printed
    python3 scripts/llm-bench/run.py preflight
    python3 scripts/llm-bench/run.py pilot --models deepseek/deepseek-v4.1-flash,openai/gpt-6-luna --calls 10
    python3 scripts/llm-bench/run.py full --repeat 3 --parallel 3

`preflight` writes roster.json (models, per-token prices, capabilities) and
key-before.json (credit numbers only). `pilot` runs a small engine slice per
model behind the proxy and projects the full run's cost. `full` starts the
proxy and, per model, runs the engine workload (`mb-evals run --manifest`)
and then the server suites (`server_suites.py`), cheapest model first, with
the budget checked before every model. Touch `<out>/ABORT` (or Ctrl-C) to
stop: no new model starts and running children are terminated.

Child processes never see the real key: mb-evals gets
MB_EVAL_LLM_API_KEY=bench-dummy and the server suites get
OPENROUTER_API_KEY=bench-dummy; the proxy injects the real one.
Standard library only, Python 3.9+.
"""

import argparse
import concurrent.futures
import datetime
import json
import os
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from typing import Any, Dict, List, Optional, Tuple

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
HERE = os.path.dirname(os.path.abspath(__file__))
OPENROUTER_API = "https://openrouter.ai/api/v1"
DEFAULT_OUT = os.path.join("docs", "benchmarks", "2026-10-29")
DEFAULT_MANIFEST = os.path.join("docs", "benchmarks", "workloads.json")
DEFAULT_MB_EVALS = os.path.join("target", "release", "mb-evals")
DEFAULT_SERVER_SUITES = os.path.join("scripts", "llm-bench", "server_suites.py")
DUMMY_KEY = "bench-dummy"
DEFAULT_MAX_OPS = 10  # mb-evals' --max-ops-per-contract default
# Assumed tokens per call when a model has no pilot measurement yet.
ASSUMED_PROMPT_TOKENS = 1500
ASSUMED_COMPLETION_TOKENS = 500

# User-confirmed roster (spec, 2026-10-29).
ROSTER = [
    "deepseek/deepseek-v4.1-flash",
    "deepseek/deepseek-v4.1-flash-1017",
    "nvidia/nemotron-3.5-super-vl",
    "nvidia/nemotron-3.5-lightning",
    "inception/mercury-3",
    "anthropic/claude-haiku-5.5",
    "qwen/qwen3.8-flash",
    "z-ai/glm-5.3-flash",
    "minimax/minimax-m3.1-flash",
    "openai/gpt-6-luna",
    "google/gemini-3.8-flash",
    "thinkingmachines/inkling-small",
    "thinkingmachines/inkling",
    "reflection/beam-1.0-501b-a23b",
]
LAST_PREFIX = "thinkingmachines/inkling"  # run last, one at a time


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


def slug(model: str) -> str:
    return model.replace("/", "__")


def log(msg: str) -> None:
    print(f"[{datetime.datetime.now().strftime('%H:%M:%S')}] {msg}", flush=True)


def rel(p: str) -> str:
    r = os.path.relpath(p, ROOT)
    return p if r.startswith("..") else r


def abspath(p: str) -> str:
    return p if os.path.isabs(p) else os.path.join(ROOT, p)


def read_json(path: str, default: Any = None) -> Any:
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


def write_json(path: str, obj: Any) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(obj, f, indent=2, sort_keys=False)
        f.write("\n")
    os.replace(tmp, path)


def read_jsonl(path: str) -> List[Dict[str, Any]]:
    out: List[Dict[str, Any]] = []
    if not os.path.exists(path):
        return out
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            if isinstance(rec, dict):
                out.append(rec)
    return out


def model_list(arg: Optional[str]) -> List[str]:
    if not arg:
        return list(ROSTER)
    return [m.strip() for m in arg.split(",") if m.strip()]


# --------------------------------------------------------------------------
# OpenRouter metadata (preflight)


def get_json(url: str, auth: bool) -> Any:
    req = urllib.request.Request(url)
    if auth:
        key = os.environ.get("OPENROUTER_API_KEY")
        if not key:
            raise SystemExit("OPENROUTER_API_KEY is not set (source ~/.zshenv)")
        req.add_header("Authorization", "Bearer " + key)
    req.add_header("HTTP-Referer", "https://github.com/rj-s-org/mockingbird")
    req.add_header("X-Title", "Mockingbird bench")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        raise SystemExit(f"GET {url} failed: HTTP {e.code}")
    except urllib.error.URLError as e:
        raise SystemExit(f"GET {url} failed: {e.reason}")


def fnum(v: Any) -> Optional[float]:
    try:
        return None if v is None or v == "" else float(v)
    except (TypeError, ValueError):
        return None


def fetch_key(api: str, path: str) -> None:
    """Writes the key's credit numbers (and nothing else) to `path`."""
    doc = get_json(api + "/key", auth=True)
    data = doc.get("data", doc) if isinstance(doc, dict) else {}
    keep = {k: fnum(data.get(k)) for k in ("usage", "usage_daily", "limit_remaining")}
    keep["fetched_at"] = utc_now()
    write_json(path, keep)
    log(f"wrote {rel(path)}: usage={keep['usage']} usage_daily={keep['usage_daily']} "
        f"limit_remaining={keep['limit_remaining']}")


def preflight(args: argparse.Namespace) -> int:
    out = abspath(args.out)
    models = model_list(args.models)
    doc = get_json(args.openrouter_api + "/models", auth=False)
    by_id = {m.get("id"): m for m in (doc.get("data") or []) if isinstance(m, dict)}
    roster: List[Dict[str, Any]] = []
    missing: List[str] = []
    for mid in models:
        m = by_id.get(mid)
        if m is None:
            missing.append(mid)
            roster.append({"id": mid, "found": False})
            continue
        pricing = m.get("pricing") or {}
        params = m.get("supported_parameters") or []
        top = m.get("top_provider") or {}
        roster.append(
            {
                "id": mid,
                "found": True,
                "name": m.get("name"),
                "pricing": {k: fnum(v) for k, v in pricing.items()},
                "context_length": m.get("context_length"),
                "max_completion_tokens": top.get("max_completion_tokens"),
                "supported_parameters": params,
                "structured_outputs": "structured_outputs" in params,
                "response_format": "response_format" in params,
                "reasoning": "reasoning" in params,
            }
        )
    path = os.path.join(out, "roster.json")
    write_json(path, {"fetched_at": utc_now(), "source": args.openrouter_api + "/models", "models": roster,
                      "missing": missing})
    log(f"wrote {rel(path)}: {len(roster) - len(missing)} found, missing: {missing or 'none'}")
    for r in roster:
        if r.get("found"):
            p = r["pricing"]
            log(f"  {r['id']:<36} in ${(p.get('prompt') or 0) * 1e6:.3f}/M  out ${(p.get('completion') or 0) * 1e6:.3f}/M"
                f"  rf={r['response_format']} so={r['structured_outputs']}")
    if not args.no_key:
        fetch_key(args.openrouter_api, os.path.join(out, "key-before.json"))
    return 1 if missing and args.strict else 0


# --------------------------------------------------------------------------
# Manifest arithmetic


def entry_cap(e: Dict[str, Any]) -> int:
    if e.get("operations"):
        return len(e["operations"])
    return int(e.get("max_ops") or DEFAULT_MAX_OPS)


def manifest_targets(manifest: Dict[str, Any]) -> int:
    """Targets per repeat (an upper bound when an entry relies on max_ops)."""
    return sum(entry_cap(e) for e in manifest.get("entries", []))


def slice_manifest(manifest: Dict[str, Any], calls: int) -> Dict[str, Any]:
    """About `calls` targets, spread round-robin over api types and entries."""
    groups: Dict[str, List[int]] = {}
    entries = manifest.get("entries", [])
    for i, e in enumerate(entries):
        groups.setdefault(e.get("api_type", "?"), []).append(i)
    order: List[int] = []
    queues = [list(v) for v in groups.values()]
    while any(queues):
        for q in queues:
            if q:
                order.append(q.pop(0))
    counts = [0] * len(entries)
    total = 0
    progress = True
    while total < calls and progress:
        progress = False
        for i in order:
            if total >= calls:
                break
            if counts[i] < entry_cap(entries[i]):
                counts[i] += 1
                total += 1
                progress = True
    out = []
    for i in order:
        if counts[i] == 0:
            continue
        e = dict(entries[i])
        if e.get("operations"):
            e["operations"] = e["operations"][: counts[i]]
        e["max_ops"] = counts[i]
        out.append(e)
    return {"entries": out}


# --------------------------------------------------------------------------
# Proxy


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class ProxyProc:
    def __init__(self, args: argparse.Namespace, ledger: str, prior: List[str], logfile: str):
        self.port = args.port or free_port()
        cmd = [
            sys.executable, os.path.join(HERE, "proxy.py"),
            "--port", str(self.port),
            "--ledger", ledger,
            "--budget-usd", str(args.budget_usd),
            "--dump", os.path.join(os.path.dirname(ledger), "exchanges.jsonl"),
        ]
        roster = os.path.join(abspath(args.out), "roster.json")
        if os.path.exists(roster):
            cmd += ["--prices", roster]
        for p in list(prior) + list(args.prior_ledger or []):
            if os.path.exists(p):
                cmd += ["--prior-ledger", p]
        if args.no_stream_upgrade:
            cmd.append("--no-stream-upgrade")
        if args.upstream:
            cmd += ["--upstream", args.upstream]
        os.makedirs(os.path.dirname(logfile), exist_ok=True)
        self.log = open(logfile, "a", encoding="utf-8")
        # Own session: Ctrl-C must not kill the proxy before its in-flight
        # calls reach the ledger; stop() terminates it after the children.
        self.proc = subprocess.Popen(cmd, cwd=ROOT, stdout=self.log, stderr=subprocess.STDOUT,
                                     start_new_session=True)
        self.base = f"http://127.0.0.1:{self.port}"
        deadline = time.time() + 15
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise SystemExit(f"proxy exited with {self.proc.returncode}; see {logfile}")
            try:
                self.stats()
                return
            except (OSError, ValueError):
                time.sleep(0.1)
        self.stop()
        raise SystemExit(f"proxy did not come up; see {logfile}")

    def stats(self) -> Dict[str, Any]:
        with urllib.request.urlopen(self.base + "/stats", timeout=5) as r:
            return json.load(r)

    def label_url(self, model: str, workload: str) -> str:
        return f"{self.base}/r/{slug(model)}~{workload}/api/v1"

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                # The proxy drains in-flight calls (up to its 120 s upstream
                # timeout) before it exits.
                self.proc.wait(140)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.log.close()


# --------------------------------------------------------------------------
# Children


class Children:
    """Tracks child processes so an abort can terminate them."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.procs: List[subprocess.Popen] = []
        self.aborted = False

    def run(self, cmd: List[str], env: Dict[str, str], logfile: str) -> int:
        os.makedirs(os.path.dirname(logfile), exist_ok=True)
        with open(logfile, "a", encoding="utf-8") as lf:
            lf.write(f"# {utc_now()} {' '.join(cmd)}\n")
            lf.flush()
            with self.lock:
                if self.aborted:
                    return -1
                p = subprocess.Popen(cmd, cwd=ROOT, env=env, stdout=lf, stderr=subprocess.STDOUT,
                                     start_new_session=True)
                self.procs.append(p)
            try:
                return p.wait()
            finally:
                with self.lock:
                    if p in self.procs:
                        self.procs.remove(p)

    def abort(self) -> None:
        with self.lock:
            self.aborted = True
            procs = list(self.procs)
        for p in procs:
            try:
                os.killpg(p.pid, signal.SIGTERM)
            except OSError:
                pass


def child_env(extra: Dict[str, str]) -> Dict[str, str]:
    """The environment of a child: no real key, no way around the proxy.

    Drops the key, any other variable holding the same value, and the
    `MB_LLM_*` / `MB_EVAL_LLM_*` variables (mockingbird's zero-config
    profile defaults to openrouter.ai, which would bypass the proxy).
    """
    key = os.environ.get("OPENROUTER_API_KEY")
    env = {
        k: v for k, v in os.environ.items()
        if not (k == "OPENROUTER_API_KEY" or k.startswith(("MB_LLM_", "MB_EVAL_LLM_")) or (key and v == key))
    }
    env.update(extra)
    return env


def run_engine(
    args: argparse.Namespace,
    kids: Children,
    proxy: ProxyProc,
    model: str,
    workload: str,
    manifest: str,
    repeat: int,
    outdir: str,
) -> int:
    cmd = [
        abspath(args.mb_evals), "run",
        "--manifest", manifest,
        "--repeat", str(repeat),
        "--concurrency", str(args.concurrency),
        "--samples", os.path.join(outdir, "samples.jsonl"),
        "--out", outdir,
    ]
    # Unset: mb-evals uses a default LLM profile's settings, as the server does.
    if args.llm_timeout_secs:
        cmd += ["--llm-timeout-secs", str(args.llm_timeout_secs)]
    if args.max_tokens:
        cmd += ["--max-tokens", str(args.max_tokens)]
    if args.output_budget is not None:
        cmd += ["--output-budget", str(args.output_budget)]
    env = child_env({
        "MB_EVAL_LLM_BASE_URL": proxy.label_url(model, workload),
        "MB_EVAL_LLM_MODEL": model,
        "MB_EVAL_LLM_API_KEY": DUMMY_KEY,
    })
    return kids.run(cmd, env, os.path.join(outdir, "mb-evals.log"))


def run_server_suites(
    args: argparse.Namespace, kids: Children, proxy: ProxyProc, model: str, repeat: int, outdir: str
) -> int:
    cmd = [
        sys.executable, abspath(args.server_suites),
        "--model", model,
        "--proxy-base", proxy.base,
        "--out", outdir,
        "--repeat", str(repeat),
    ]
    rcs = []
    suites = [s.strip() for s in (args.suites or "").split(",") if s.strip()] or [None]
    for suite in suites:
        c = cmd + (["--suite", suite] if suite else [])
        rcs.append(kids.run(c, child_env({"OPENROUTER_API_KEY": DUMMY_KEY}),
                            os.path.join(outdir, "server_suites.log")))
    return next((rc for rc in rcs if rc != 0), 0)


def engine_ok(rc: int) -> bool:
    # 0 = thresholds met, 1 = a threshold broken (still a complete run).
    return rc in (0, 1)


# --------------------------------------------------------------------------
# Pilot


def summarize(records: List[Dict[str, Any]]) -> Dict[str, Any]:
    n = len(records)

    def mean(key: str) -> Optional[float]:
        vals = [r[key] for r in records if isinstance(r.get(key), (int, float))]
        return round(sum(vals) / n, 2) if vals and n else None

    def median(key: str) -> Optional[float]:
        vals = [r[key] for r in records if isinstance(r.get(key), (int, float))]
        return round(statistics.median(vals), 1) if vals else None

    cost = sum(float(r.get("cost_usd") or 0.0) for r in records)
    return {
        "calls": n,
        "errors": sum(1 for r in records if r.get("error_kind")),
        "error_kinds": sorted({r["error_kind"] for r in records if r.get("error_kind")}),
        "cost_usd": round(cost, 6),
        "cost_per_call": (cost / n) if n else None,
        "prompt_tokens_per_call": mean("prompt_tokens"),
        "completion_tokens_per_call": mean("completion_tokens"),
        "reasoning_tokens_per_call": mean("reasoning_tokens"),
        "ttft_ms_p50": median("ttft_ms"),
        "total_ms_p50": median("total_ms"),
        "reasoning_rejected": any(r.get("reasoning_rejected") for r in records),
        "response_format_rejected": any(r.get("response_format_rejected") for r in records),
        "providers": sorted({r["provider"] for r in records if r.get("provider")}),
    }


def pilot(args: argparse.Namespace) -> int:
    out = abspath(args.out)
    pdir = os.path.join(out, "pilot")
    manifest = read_json(abspath(args.manifest))
    if not manifest:
        raise SystemExit(f"cannot read the manifest {args.manifest}")
    if not os.path.exists(os.path.join(out, "roster.json")):
        log("warning: no roster.json (run preflight); the proxy reserves budget at fallback prices")
    sl = slice_manifest(manifest, args.calls)
    slice_targets = manifest_targets(sl)
    full_targets = manifest_targets(manifest)
    slice_path = os.path.join(pdir, "manifest.slice.json")
    write_json(slice_path, sl)
    ledger = os.path.join(pdir, "calls.jsonl")
    proxy = ProxyProc(args, ledger, [os.path.join(out, "calls.jsonl")], os.path.join(pdir, "proxy.log"))
    kids = Children()
    results_path = os.path.join(pdir, "pilot.json")
    results = read_json(results_path, {}) or {}
    try:
        for model in model_list(args.models):
            if proxy.stats().get("tripped"):
                log("budget tripped; stopping the pilot")
                break
            mdir = os.path.join(pdir, slug(model))
            since = utc_now()
            label = f"{slug(model)}~pilot"
            before = sum(1 for r in read_jsonl(ledger) if r.get("label") == label)
            log(f"pilot {model}: {slice_targets} targets x 1")
            t0 = time.time()
            rc = run_engine(args, kids, proxy, model, "pilot", slice_path, 1, mdir)
            recs = [r for r in read_jsonl(ledger) if r.get("label") == label][before:]
            s = summarize(recs)
            calls_per_target = (len(recs) / slice_targets) if slice_targets and recs else 1.0
            full_engine_calls = full_targets * args.repeat * max(1.0, calls_per_target)
            full_calls = full_engine_calls + args.server_calls
            s.update(
                model=model,
                exit_code=rc,
                wall_s=round(time.time() - t0, 1),
                slice_targets=slice_targets,
                calls_per_target=round(calls_per_target, 3),
                full_targets=full_targets,
                full_repeat=args.repeat,
                server_calls=args.server_calls,
                full_calls=round(full_calls),
                projected_usd=(s["cost_per_call"] * full_calls) if s["cost_per_call"] is not None else None,
                measured_at=since,
            )
            results[model] = s
            write_json(results_path, results)
            proj = s["projected_usd"]
            log(f"  rc={rc} calls={s['calls']} errors={s['errors']} {s['error_kinds'] or ''} "
                f"cost/call=${(s['cost_per_call'] or 0):.6f} tokens/call in={s['prompt_tokens_per_call']} "
                f"out={s['completion_tokens_per_call']} reason={s['reasoning_tokens_per_call']} "
                f"ttft_p50={s['ttft_ms_p50']}ms -> projected ${proj if proj is None else round(proj, 4)}")
    except KeyboardInterrupt:
        kids.abort()
        log("interrupted")
    finally:
        proxy.stop()
    print_projection(results, model_list(args.models))
    return 0


def print_projection(results: Dict[str, Any], models: List[str]) -> None:
    print()
    print("| model | calls | err | $/call | in tok/call | out tok/call | reason tok/call | ttft p50 ms | "
          "full calls | projected $ |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    total = 0.0
    for m in models:
        s = results.get(m)
        if not s:
            print(f"| {m} | - | - | - | - | - | - | - | - | - |")
            continue
        proj = s.get("projected_usd")
        total += proj or 0.0
        cpc = s.get("cost_per_call")
        print(f"| {m} | {s['calls']} | {s['errors']} | {'-' if cpc is None else f'{cpc:.6f}'} | "
              f"{s['prompt_tokens_per_call']} | {s['completion_tokens_per_call']} | "
              f"{s['reasoning_tokens_per_call']} | {s['ttft_ms_p50']} | {s['full_calls']} | "
              f"{'-' if proj is None else f'{proj:.4f}'} |")
    print(f"\nprojected full run: ${total:.4f} for the models above")


# --------------------------------------------------------------------------
# Full run


def projections(args: argparse.Namespace, models: List[str]) -> Dict[str, Tuple[float, str]]:
    """Projected full-run cost per model: from the pilot, else roster prices."""
    out = abspath(args.out)
    pilot_res = read_json(os.path.join(out, "pilot", "pilot.json"), {}) or {}
    roster = read_json(os.path.join(out, "roster.json"), {}) or {}
    prices = {m.get("id"): m.get("pricing") or {} for m in roster.get("models", []) if isinstance(m, dict)}
    manifest = read_json(abspath(args.manifest), {}) or {}
    full_calls = manifest_targets(manifest) * args.repeat + args.server_calls
    proj: Dict[str, Tuple[float, str]] = {}
    for m in models:
        p = pilot_res.get(m) or {}
        if p.get("cost_per_call") is not None:
            cpt = max(1.0, float(p.get("calls_per_target") or 1.0))
            calls = manifest_targets(manifest) * args.repeat * cpt + args.server_calls
            proj[m] = (float(p["cost_per_call"]) * calls, "pilot")
            continue
        pr = prices.get(m.split("@", 1)[0]) or {}
        pin, pout = fnum(pr.get("prompt")), fnum(pr.get("completion"))
        if pin is not None and pout is not None:
            per = ASSUMED_PROMPT_TOKENS * pin + ASSUMED_COMPLETION_TOKENS * pout
            proj[m] = (per * full_calls, "roster")
        else:
            proj[m] = (float("inf"), "unknown")
    return proj


def model_spent(stats: Dict[str, Any], model: str) -> float:
    pre = slug(model) + "~"
    return sum(float(v.get("cost_usd") or 0.0) for k, v in (stats.get("per_label") or {}).items()
               if k.startswith(pre))


def merge_outputs(out: str, models: List[str]) -> None:
    """Joins per-model samples and server checks into the run's top-level files.

    Every model with results under engine/ or server/ is included (not only
    this invocation's), so a resumed run keeps the earlier models.
    """
    seen = list(models)
    for sub in ("engine", "server"):
        d = os.path.join(out, sub)
        if os.path.isdir(d):
            for name in sorted(os.listdir(d)):
                m = name.replace("__", "/")
                if os.path.isdir(os.path.join(d, name)) and m not in seen:
                    seen.append(m)
    models = seen
    with open(os.path.join(out, "samples.jsonl"), "w", encoding="utf-8") as f:
        for m in models:
            for rec in read_jsonl(os.path.join(out, "engine", slug(m), "samples.jsonl")):
                rec = dict(rec, model=m, model_slug=slug(m))
                f.write(json.dumps(rec, ensure_ascii=False) + "\n")
    with open(os.path.join(out, "server.jsonl"), "w", encoding="utf-8") as f:
        for m in models:
            for rec in read_jsonl(os.path.join(out, "server", slug(m), "server.jsonl")):
                rec.setdefault("model", m)
                f.write(json.dumps(rec, ensure_ascii=False) + "\n")


def full(args: argparse.Namespace) -> int:
    out = abspath(args.out)
    os.makedirs(out, exist_ok=True)
    manifest_path = abspath(args.manifest)
    if not read_json(manifest_path):
        raise SystemExit(f"cannot read the manifest {args.manifest}")
    if not os.path.exists(abspath(args.mb_evals)):
        raise SystemExit(f"{args.mb_evals} is missing; build it: cargo build --release -p mb-evals")
    models = model_list(args.models)
    proj = projections(args, models)
    normal = sorted([m for m in models if not m.startswith(LAST_PREFIX)], key=lambda m: proj[m][0])
    last = sorted([m for m in models if m.startswith(LAST_PREFIX)], key=lambda m: proj[m][0])
    trip_file = os.path.join(out, "BUDGET_TRIPPED")
    abort_file = os.path.join(out, "ABORT")
    if os.path.exists(abort_file):
        raise SystemExit(f"{abort_file} exists; remove it to run")

    state_path = os.path.join(out, "run.json")
    state: Dict[str, Any] = {
        "started": utc_now(),
        "finished": None,
        "repeat": args.repeat,
        "parallel": args.parallel,
        "concurrency": args.concurrency,
        "budget_usd": args.budget_usd,
        "manifest": rel(manifest_path),
        "order": normal + last,
        "models": {m: {"status": "pending", "projected_usd": None if proj[m][0] == float("inf") else round(proj[m][0], 4),
                       "projection_source": proj[m][1]} for m in models},
        "skipped": [],
        "aborted": False,
    }
    # A later invocation (an added model) keeps the earlier models' state.
    prev = read_json(state_path, None)
    if isinstance(prev, dict) and isinstance(prev.get("models"), dict):
        kept = {m: v for m, v in prev["models"].items() if m not in state["models"]}
        state["models"] = dict(kept, **state["models"])
        state["order"] = [m for m in prev.get("order", []) if m in kept] + state["order"]
        state["started"] = prev.get("started") or state["started"]
        state["invocations"] = list(prev.get("invocations") or [{"started": prev.get("started"),
                                                                 "finished": prev.get("finished")}])
        state["invocations"].append({"started": utc_now(), "models": models})
    lock = threading.Lock()

    def save() -> None:
        with lock:
            write_json(state_path, state)

    def setm(m: str, **kw: Any) -> None:
        with lock:
            state["models"][m].update(kw)
        save()

    if not args.no_key and not os.path.exists(os.path.join(out, "key-before.json")):
        fetch_key(args.openrouter_api, os.path.join(out, "key-before.json"))
    log("order: " + ", ".join(f"{m} (${proj[m][0]:.3f} {proj[m][1]})" for m in normal + last))
    proxy = ProxyProc(args, os.path.join(out, "calls.jsonl"), [os.path.join(out, "pilot", "calls.jsonl"),
                                                     os.path.join(out, "superseded", "calls.jsonl")],
                      os.path.join(out, "proxy.log"))
    kids = Children()
    running: Dict[str, float] = {}  # model -> projection, while it runs
    save()

    def stopped() -> Optional[str]:
        if kids.aborted or os.path.exists(abort_file):
            return "aborted"
        if os.path.exists(trip_file):
            return "budget tripped"
        return None

    def admit(m: str) -> Optional[str]:
        """None if `m` may start now, else why not."""
        why = stopped()
        if why:
            return why
        st = proxy.stats()
        if st.get("tripped"):
            return "budget tripped"
        with lock:
            outstanding = sum(max(0.0, p - model_spent(st, r)) for r, p in running.items())
        need = float(st.get("spent_usd") or 0.0) + outstanding + proj[m][0]
        if need >= args.budget_usd:
            return (f"projected ${proj[m][0]:.3f} on top of spent ${st.get('spent_usd', 0):.3f} "
                    f"+ in-flight ${outstanding:.3f} would cross ${args.budget_usd:.2f}")
        return None

    def work(m: str) -> None:
        t0 = time.time()
        setm(m, status="running", started=utc_now())
        engine_dir = os.path.join(out, "engine", slug(m))
        log(f"{m}: engine workload")
        rc = run_engine(args, kids, proxy, m, "engine", manifest_path, args.repeat, engine_dir)
        setm(m, engine_exit=rc)
        if not engine_ok(rc):
            log(f"{m}: mb-evals exited {rc}; see {rel(engine_dir)}/mb-evals.log")
        srv_rc = None
        if args.skip_server:
            pass
        elif stopped():
            log(f"{m}: skipping server suites ({stopped()})")
        else:
            log(f"{m}: server suites")
            srv_rc = run_server_suites(args, kids, proxy, m, args.repeat, os.path.join(out, "server", slug(m)))
            if srv_rc != 0:
                log(f"{m}: server_suites.py exited {srv_rc}")
        st = proxy.stats()
        spent = model_spent(st, m)
        why = stopped()
        status = "done" if engine_ok(rc) and srv_rc in (None, 0) and not why else ("partial" if why else "failed")
        setm(m, status=status, server_exit=srv_rc, spent_usd=round(spent, 6), wall_s=round(time.time() - t0, 1),
             finished=utc_now(), stop_reason=why)
        log(f"{m}: {status}, spent ${spent:.4f} (total ${st.get('spent_usd', 0):.4f})")

    def skip_rest(queue: List[str], why: str) -> None:
        for m in queue:
            setm(m, status="skipped", skip_reason=why)
            with lock:
                state["skipped"].append(m)
            log(f"skip {m}: {why}")
        queue.clear()

    def interrupt(*_: Any) -> None:
        kids.abort()

    prev = signal.signal(signal.SIGINT, interrupt)
    try:
        queue = list(normal)
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.parallel)) as ex:
            futures: Dict[concurrent.futures.Future, str] = {}
            while queue:
                while len(futures) >= max(1, args.parallel):
                    done, _ = concurrent.futures.wait(futures, timeout=1.0,
                                                      return_when=concurrent.futures.FIRST_COMPLETED)
                    for f in done:
                        with lock:
                            running.pop(futures.pop(f), None)
                        f.result()
                    if os.path.exists(abort_file):
                        kids.abort()
                m = queue[0]
                why = admit(m)
                if why:
                    skip_rest(queue, why)
                    break
                queue.pop(0)
                with lock:
                    running[m] = proj[m][0]
                futures[ex.submit(work, m)] = m
            while futures:
                done, _ = concurrent.futures.wait(futures, timeout=1.0,
                                                  return_when=concurrent.futures.FIRST_COMPLETED)
                for f in done:
                    with lock:
                        running.pop(futures.pop(f), None)
                    f.result()
                if os.path.exists(abort_file):
                    kids.abort()
        queue = list(last)
        if state["skipped"]:
            skip_rest(queue, "an earlier model was skipped (budget or abort)")
        while queue:
            m = queue[0]
            why = admit(m)
            if why:
                skip_rest(queue, why)
                break
            queue.pop(0)
            with lock:
                running[m] = proj[m][0]
            work(m)
            with lock:
                running.pop(m, None)
    finally:
        signal.signal(signal.SIGINT, prev)
        with lock:
            state["aborted"] = kids.aborted or os.path.exists(abort_file)
        try:
            final = proxy.stats()
            with lock:
                state["spent_usd"] = final.get("spent_usd")
                state["calls"] = final.get("calls")
                state["tripped"] = final.get("tripped")
        except (OSError, ValueError):
            pass
        proxy.stop()
        merge_outputs(out, models)
        with lock:
            state["finished"] = utc_now()
        save()
    if not args.no_key:
        try:
            fetch_key(args.openrouter_api, os.path.join(out, "key-after.json"))
        except SystemExit as e:
            log(f"key-after failed: {e}")
    log(f"done: spent ${state.get('spent_usd') or 0:.4f}, skipped {state['skipped'] or 'none'}; "
        f"state in {rel(state_path)}")
    return 0


# --------------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser(description="LLM model benchmark orchestrator")
    sub = ap.add_subparsers(dest="cmd", required=True)

    def common(p: argparse.ArgumentParser) -> None:
        p.add_argument("--out", default=DEFAULT_OUT, help="results directory (default %(default)s)")
        p.add_argument("--models", help="comma-separated model ids (default: the bench roster)")
        p.add_argument("--no-key", action="store_true", help="do not fetch the key's credit numbers")
        p.add_argument("--openrouter-api", default=OPENROUTER_API, help=argparse.SUPPRESS)

    def runner(p: argparse.ArgumentParser) -> None:
        p.add_argument("--manifest", default=DEFAULT_MANIFEST)
        p.add_argument("--mb-evals", default=DEFAULT_MB_EVALS)
        p.add_argument("--server-suites", default=DEFAULT_SERVER_SUITES, help=argparse.SUPPRESS)
        p.add_argument("--budget-usd", type=float, default=9.0)
        p.add_argument("--port", type=int, default=0, help="proxy port (default: a free one)")
        p.add_argument("--concurrency", type=int, default=4, help="mb-evals calls in flight per model")
        p.add_argument("--llm-timeout-secs", type=int, help="passed to mb-evals (default: the profile default)")
        p.add_argument("--max-tokens", type=int, help="passed to mb-evals (default: the profile default)")
        p.add_argument("--output-budget", type=int, help="passed to mb-evals (default: the limits default)")
        p.add_argument("--prior-ledger", action="append",
                       help="an earlier run's calls.jsonl(.gz) whose spend counts toward --budget-usd (repeatable)")
        p.add_argument("--repeat", type=int, default=3, help="calls per target in the full run")
        p.add_argument("--server-calls", type=int, default=40, help="estimated server-suite LLM calls per model")
        p.add_argument("--no-stream-upgrade", action="store_true")
        p.add_argument("--upstream", help=argparse.SUPPRESS)  # tests: a fake OpenRouter

    p = sub.add_parser("preflight", help="write roster.json and key-before.json")
    common(p)
    p.add_argument("--strict", action="store_true", help="exit 1 when a roster model is missing")

    p = sub.add_parser("pilot", help="small engine slice per model; project the full run's cost")
    common(p)
    runner(p)
    p.add_argument("--calls", type=int, default=10, help="targets in the pilot slice (about)")

    p = sub.add_parser("full", help="the full benchmark run")
    common(p)
    runner(p)
    p.add_argument("--parallel", type=int, default=3, help="models run concurrently (Inkling models: one at a time, last)")
    p.add_argument("--suites", help="comma-separated server suites (default: all)")
    p.add_argument("--skip-server", action="store_true", help="engine workload only")

    args = ap.parse_args()
    if args.cmd == "preflight":
        return preflight(args)
    if args.upstream:
        args.no_key = True
    if args.cmd == "pilot":
        return pilot(args)
    return full(args)


if __name__ == "__main__":
    sys.exit(main())
