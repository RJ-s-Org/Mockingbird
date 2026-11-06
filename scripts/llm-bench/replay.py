#!/usr/bin/env python3
"""Recorded LLM answers for the journeys suites, and the replay gate that runs on them.

A paid run records every LLM call of a (model, journeys suite) and the run's per-check
results. The gate replays those answers, with no network and no key, against the
`mockingbird` binary of a pull request, and fails when a check the recording passed
now fails although every LLM call of its journey was answered from the recording
(exactly, or by its key). Such a failure is the engine's: the model's answers are the
same as when the check passed.

    # Paid (re-)recording: costs real calls, needs OPENROUTER_API_KEY (never printed)
    python3 scripts/llm-bench/replay.py record --model deepseek/deepseek-v4.1-flash-1017 \\
        --budget-usd 1.5 --prices docs/benchmarks/<run>/roster.json [--suite journeys_blind6 ...]

    # Free (re-)recording against the fake LLM (what seeds the committed recordings)
    python3 scripts/llm-bench/replay.py record --fake

    # The gate: free, offline, a few minutes
    python3 scripts/llm-bench/replay.py gate --binary target/release/mockingbird

    # Check that no recording holds a credential
    python3 scripts/llm-bench/replay.py scan

Matching, per call (`proxy.py --replay`), in this order:
  exact    the prompt hash (messages and response_format, with timestamps blanked) of
           a recorded call of the same label;
  key      the same (journey, rep, operation, n-th call of that operation in the journey);
  nearest  the recorded call of the same operation (same journey first) closest in order;
  none     no recorded call of that operation: answered 503 with REPLAY_UNMATCHED.
Every non-exact match is counted in the replay log. The journey comes from the
`x-bench-journey` header that `server_suites.py --tag-journeys` gives each mock's LLM
profile; the rep from the mark it posts to the proxy before each journey.

Standard library only.
"""

import argparse
import datetime
import gzip
import hashlib
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from typing import Any, Dict, Iterable, List, Optional, Tuple

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
RECORDINGS = os.path.join(HERE, "recordings")
FORMAT = 1
JOURNEY_HEADER = "x-bench-journey"
UNMATCHED = "REPLAY_UNMATCHED"
# The model id the free recordings are made with (fake_llm.py answers every call).
FAKE_MODEL = "bench/fake-llm"
# The suites the gate replays by default: every journeys suite with a recording. A
# replay against the fake LLM's recording takes about a second per journey.
JOURNEY_SUITES = ("journeys", "journeys_heldout", "journeys_blind", "journeys_blind2", "journeys_blind3",
                  "journeys_blind4", "journeys_blind5", "journeys_blind6", "journeys_blind7", "journeys_blind8",
                  "journeys_blind9", "journeys_blind10", "journeys_blind11", "journeys_blind12")
# Anything that looks like an OpenRouter key: a recording never holds one.
KEY_PATTERN = re.compile(r"sk-or-[A-Za-z0-9_\-]*")
AUTH_HEADER = re.compile(r"authorization[\\\"']{0,3}\s*[:=]\s*[\\\"']{0,3}\s*(bearer|basic|token)\b", re.I)
NOW_TAG = re.compile(r"<now>([^<]+)</now>")
ISO_TIME = re.compile(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?")
UNIX_SECONDS = re.compile(r"(?<![\d.])1\d{9}(?![\d.])")
# Fields whose integer value is a Unix time, when a replayed answer is moved to the
# replay's clock.
TIME_KEY = re.compile(r"(^|_)(created|updated|modified|deleted|time|timestamp|date|at|expires|starts?|ends?|"
                      r"due|since|until|last|first|issued|paid|canceled|cancelled|completed|closed|sent)($|_)",
                      re.I)
# How far from the recorded clock a timestamp may be and still be moved.
SHIFT_WINDOW_S = 3 * 365 * 86400


# ---------------------------------------------------------------- keys


def scrub(text: str) -> str:
    """Removes anything that looks like an OpenRouter key."""
    return KEY_PATTERN.sub("[redacted]", text)


def system_text(body: Dict[str, Any]) -> str:
    return "\n".join(str(m.get("content") or "") for m in body.get("messages") or []
                     if isinstance(m, dict) and m.get("role") == "system")


def op_of(body: Dict[str, Any]) -> str:
    """The operation a call is for: the prompt's `Operation:` (else `HTTP:`) line, else the
    response_format schema name, else `?`."""
    text = system_text(body) or "\n".join(str(m.get("content") or "") for m in body.get("messages") or []
                                          if isinstance(m, dict))
    for pattern in (r"^Operation: *(.+)$", r"^HTTP: *(.+)$"):
        m = re.search(pattern, text, re.M)
        if m:
            return m.group(1).strip()
    rf = body.get("response_format")
    if isinstance(rf, dict) and isinstance(rf.get("json_schema"), dict) and rf["json_schema"].get("name"):
        return "schema:" + str(rf["json_schema"]["name"])
    return "?"


def prompt_hash(body: Dict[str, Any]) -> str:
    """A hash of what the model was asked, with the clock blanked: the same journey
    asks the same at another time."""
    doc = json.dumps({"messages": body.get("messages"), "response_format": body.get("response_format")},
                     sort_keys=True, ensure_ascii=False)
    doc = UNIX_SECONDS.sub("<unix>", ISO_TIME.sub("<time>", doc))
    return hashlib.sha256(doc.encode()).hexdigest()[:32]


def parse_time(text: str) -> Optional[float]:
    t = text.strip().replace(" ", "T")
    if t.endswith("Z"):
        t = t[:-1] + "+00:00"
    if re.search(r"[+-]\d{4}$", t):
        t = t[:-2] + ":" + t[-2:]
    try:
        dt = datetime.datetime.fromisoformat(t)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=datetime.timezone.utc)
    return dt.timestamp()


def now_of(body: Dict[str, Any]) -> Optional[float]:
    """The `<now>` the engine told the model, as Unix seconds."""
    for m in body.get("messages") or []:
        if isinstance(m, dict):
            found = NOW_TAG.search(str(m.get("content") or ""))
            if found:
                return parse_time(found.group(1))
    return None


def format_like(original: str, ts: float) -> str:
    """`ts` written the way `original` is (fraction, zone style)."""
    dt = datetime.datetime.fromtimestamp(ts, datetime.timezone.utc)
    sep = "T" if "T" in original else " "
    frac = re.search(r"\.(\d+)", original)
    out = dt.strftime(f"%Y-%m-%d{sep}%H:%M:%S")
    if frac:
        out += "." + f"{dt.microsecond:06d}"[:len(frac.group(1))].ljust(len(frac.group(1)), "0")
    zone = re.search(r"(Z|[+-]\d{2}:?\d{2})$", original)
    if zone:
        z = zone.group(1)
        if z == "Z":
            out += "Z"
        else:
            sign = 1 if z[0] == "+" else -1
            hours, minutes = int(z[1:3]), int(z[-2:])
            local = dt + sign * datetime.timedelta(hours=hours, minutes=minutes)
            out = local.strftime(f"%Y-%m-%d{sep}%H:%M:%S") + out[19:] + z
    return out


def shift_value(value: Any, delta: float, anchor: float, key: str = "") -> Any:
    if isinstance(value, dict):
        return {k: shift_value(v, delta, anchor, str(k)) for k, v in value.items()}
    if isinstance(value, list):
        return [shift_value(v, delta, anchor, key) for v in value]
    if isinstance(value, str) and ISO_TIME.fullmatch(value):
        ts = parse_time(value)
        if ts is not None and abs(ts - anchor) <= SHIFT_WINDOW_S:
            return format_like(value, ts + delta)
        return value
    if isinstance(value, int) and not isinstance(value, bool) and TIME_KEY.search(key):
        if abs(value - anchor) <= SHIFT_WINDOW_S:
            return int(value + delta)
    return value


def shift_times(content: str, delta: float, anchor: Optional[float]) -> str:
    """A recorded answer moved by `delta` seconds: ISO timestamp strings and Unix-time
    fields within three years of the recorded clock (`anchor`). Content that is not
    JSON (or JSON lines) is left as it is."""
    if not delta or anchor is None:
        return content
    try:
        return json.dumps(shift_value(json.loads(content), delta, anchor), ensure_ascii=False)
    except ValueError:
        pass
    lines = content.split("\n")
    try:
        moved = [json.dumps(shift_value(json.loads(line), delta, anchor), ensure_ascii=False) if line.strip()
                 else line for line in lines]
    except ValueError:
        return content
    return "\n".join(moved)


# ---------------------------------------------------------------- journey tags


class Tags:
    """Journey, rep and per-(journey, rep, operation) order of each call, by label."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.reps: Dict[Tuple[str, str], int] = {}
        self.counts: Dict[Tuple[str, str, int, str], int] = {}

    def mark(self, label: str, journey: str, rep: int) -> None:
        with self.lock:
            self.reps[(label, journey)] = int(rep)

    def tag(self, label: str, headers: Any, body: Dict[str, Any]) -> Dict[str, Any]:
        journey = (headers.get(JOURNEY_HEADER) if headers is not None else None) or "?"
        op = op_of(body)
        with self.lock:
            rep = self.reps.get((label, journey), 0)
            key = (label, journey, rep, op)
            n = self.counts.get(key, 0)
            self.counts[key] = n + 1
        return {"label": label, "journey": journey, "rep": rep, "op": op, "n": n, "hash": prompt_hash(body),
                "now": now_of(body)}


class Recorder(Tags):
    """`proxy.py --record`: appends one line per call to the raw recording. It holds
    the answer, never a header or the key."""

    def __init__(self, path: str) -> None:
        super().__init__()
        self.path = path
        self.write_lock = threading.Lock()

    def write(self, tag: Dict[str, Any], rec: Dict[str, Any], content: str, finish_reason: Optional[str]) -> None:
        line = dict(tag, status=rec.get("status"), finish_reason=finish_reason, content=content,
                    error=rec.get("error_excerpt") if rec.get("error_kind") else None,
                    error_kind=rec.get("error_kind"))
        text = scrub(json.dumps(line, ensure_ascii=False))
        with self.write_lock:
            with open(self.path, "a", encoding="utf-8") as f:
                f.write(text + "\n")


# ---------------------------------------------------------------- recordings


def recording_name(label: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.~@-]", "-", label) + ".json.gz"


def write_recording(path: str, doc: Dict[str, Any]) -> None:
    text = scrub(json.dumps(doc, ensure_ascii=False, sort_keys=True, indent=0))
    # mtime 0: the same recording gives the same bytes.
    with open(path, "wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as f:
            f.write(text.encode())


def read_recording(path: str) -> Dict[str, Any]:
    with gzip.open(path, "rt", encoding="utf-8") as f:
        doc = json.load(f)
    if doc.get("format") != FORMAT:
        raise ValueError(f"{path}: recording format {doc.get('format')}, this replay reads {FORMAT}")
    return doc


def recording_paths(items: Iterable[str]) -> List[str]:
    out: List[str] = []
    for item in items:
        if os.path.isdir(item):
            out += sorted(os.path.join(item, n) for n in os.listdir(item) if n.endswith(".json.gz"))
        else:
            out.append(item)
    return out


def read_jsonl(path: str) -> List[Dict[str, Any]]:
    rows = []
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line:
                try:
                    rows.append(json.loads(line))
                except ValueError:
                    continue
    return rows


# A call's error_kind that is the model's own answer, not a failed call: a recording
# keeps those as the model gave them.
MODEL_ERROR_KINDS = ("refusal", "empty")


class PackRefused(Exception):
    """The run is not one to record: `problems` says why. Nothing was written."""

    def __init__(self, problems: List[str]) -> None:
        super().__init__("; ".join(problems))
        self.problems = problems


def failed_call(c: Dict[str, Any]) -> Optional[str]:
    """Why a raw or ledger call record is not a clean answer, or None."""
    status = c.get("status")
    kind = c.get("error_kind")
    if status == 402 or kind == "budget":
        return "402 (budget)"
    if status != 200:
        return f"status {status}" + (f" ({kind})" if kind else "")
    if kind and kind not in MODEL_ERROR_KINDS:
        return f"error {kind}"  # an in-stream failure keeps its 200 head
    return None


def pack_problems(raw_path: str, server_jsonl: str, ledger: Optional[str] = None) -> List[str]:
    """Why a run must not be packed: the proxy's budget tripped, a call failed (a
    non-200 status, a 402, an infra error kind), or a check is labelled as judged on
    a provider failure (`detail.infra` or `detail.infra_cascade`). Empty when clean."""
    problems: List[str] = []
    dirs = {os.path.dirname(os.path.abspath(raw_path))}
    if ledger:
        dirs.add(os.path.dirname(os.path.abspath(ledger)))
    for d in sorted(dirs):
        if os.path.exists(os.path.join(d, "BUDGET_TRIPPED")):
            problems.append(f"the proxy's budget tripped ({os.path.join(d, 'BUDGET_TRIPPED')})")
    sources = [("raw", raw_path)] + ([("ledger", ledger)] if ledger else [])
    for name, path in sources:
        if not os.path.exists(path):
            if name == "ledger":
                problems.append(f"no ledger at {path}")
            continue
        bad: Dict[str, int] = {}
        for c in read_jsonl(path):
            why = failed_call(c)
            if why:
                bad[why] = bad.get(why, 0) + 1
        for why, n in sorted(bad.items()):
            problems.append(f"{name}: {n} call(s) with {why}")
    labelled: Dict[str, int] = {}
    for r in read_jsonl(server_jsonl):
        detail = r.get("detail") if isinstance(r.get("detail"), dict) else {}
        for label in ("infra", "infra_cascade"):
            if detail.get(label):
                labelled[label] = labelled.get(label, 0) + 1
    for label, n in sorted(labelled.items()):
        problems.append(f"server: {n} check(s) labelled detail.{label} (a provider failure)")
    return problems


def pack(raw_path: str, server_jsonl: str, dest: str, source: str, rep: int = 0,
         ledger: Optional[str] = None) -> List[str]:
    """Writes one recording per label of the raw recording: the calls and the
    per-check results of rep `rep` (`case` -> ok). Returns the files written.
    Raises PackRefused, having written nothing, when `pack_problems` finds any."""
    problems = pack_problems(raw_path, server_jsonl, ledger)
    if problems:
        raise PackRefused(problems)
    calls = [c for c in read_jsonl(raw_path) if c.get("rep") == rep]
    rows = [r for r in read_jsonl(server_jsonl) if r.get("rep") == rep]
    os.makedirs(dest, exist_ok=True)
    written = []
    for label in sorted({c["label"] for c in calls}):
        slug, _, suite = label.partition("~")
        mine = [c for c in calls if c["label"] == label]
        models = {r["model"] for r in rows if r.get("suite") == suite and r.get("model", "").replace("/", "__") == slug}
        model = sorted(models)[0] if models else slug.replace("__", "/")
        results = {r["case"]: bool(r["ok"]) for r in rows if r.get("suite") == suite and r.get("model") == model
                   and r.get("case") != "suite_error"}
        keep = ("journey", "rep", "op", "n", "hash", "now", "status", "finish_reason", "content", "error")
        doc = {"format": FORMAT, "label": label, "model": model, "suite": suite, "source": source, "rep": rep,
               "recorded": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d"),
               "calls": [{k: c.get(k) for k in keep} for c in mine], "results": results}
        path = os.path.join(dest, recording_name(label))
        write_recording(path, doc)
        written.append(path)
    return written


# ---------------------------------------------------------------- replay


class Replayer(Tags):
    """`proxy.py --replay`: answers each call from the recording of its label."""

    def __init__(self, paths: Iterable[str], log_path: Optional[str] = None) -> None:
        super().__init__()
        self.log_path = log_path
        self.log_lock = threading.Lock()
        self.by_label: Dict[str, Dict[str, Any]] = {}
        self.deltas: Dict[Tuple[str, str], float] = {}
        for path in recording_paths(paths):
            doc = read_recording(path)
            calls = doc.get("calls") or []
            exact: Dict[str, List[Dict[str, Any]]] = {}
            key: Dict[Tuple[str, int, str, int], Dict[str, Any]] = {}
            for c in calls:
                exact.setdefault(c["hash"], []).append(c)
                key.setdefault((c["journey"], c["rep"], c["op"], c["n"]), c)
            self.by_label[doc["label"]] = {"calls": calls, "exact": exact, "key": key, "used": set()}

    def find(self, tag: Dict[str, Any]) -> Tuple[str, Optional[Dict[str, Any]]]:
        rec = self.by_label.get(tag["label"])
        if rec is None:
            return "none", None
        with self.lock:
            same = rec["exact"].get(tag["hash"]) or []
            for c in same:
                if id(c) not in rec["used"]:
                    rec["used"].add(id(c))
                    return "exact", c
            hit = rec["key"].get((tag["journey"], tag["rep"], tag["op"], tag["n"]))
            if hit is not None:
                rec["used"].add(id(hit))
                return "key", hit
            if same:
                return "exact", same[-1]
        ops = [c for c in rec["calls"] if c["op"] == tag["op"]]
        if not ops:
            return "none", None
        mine = [c for c in ops if c["journey"] == tag["journey"]] or ops
        return "nearest", min(mine, key=lambda c: (abs(c["n"] - tag["n"]), c["rep"] != tag["rep"], c["n"]))

    def delta(self, tag: Dict[str, Any], call: Dict[str, Any]) -> float:
        """Seconds between the replay's clock and the recording's, fixed per journey at
        its first call, so a timestamp two answers share is moved the same way."""
        if tag.get("now") is None or call.get("now") is None:
            return 0.0
        k = (tag["label"], tag["journey"])
        with self.lock:
            if k not in self.deltas:
                self.deltas[k] = round(tag["now"] - call["now"])
            return self.deltas[k]

    def answer(self, label: str, headers: Any, body: Dict[str, Any]) -> Dict[str, Any]:
        """{status, content, finish_reason, error, match} for one call; logs it."""
        tag = self.tag(label, headers, body)
        match, call = self.find(tag)
        if call is None:
            out = {"status": 503, "content": "", "finish_reason": None, "match": "none",
                   "error": f"{UNMATCHED}: no recorded answer for {tag['journey']} / {tag['op']} #{tag['n']}"}
        else:
            status = int(call.get("status") or 200)
            content = call.get("content") or ""
            if status == 200:
                content = shift_times(content, self.delta(tag, call), call.get("now"))
            out = {"status": status, "content": content, "finish_reason": call.get("finish_reason"),
                   "error": call.get("error"), "match": match}
        if self.log_path:
            line = {k: tag[k] for k in ("label", "journey", "rep", "op", "n")}
            line.update(match=out["match"], status=out["status"])
            with self.log_lock:
                with open(self.log_path, "a", encoding="utf-8") as f:
                    f.write(json.dumps(line, ensure_ascii=False) + "\n")
        return out


# ---------------------------------------------------------------- comparison


def journey_of(case: str) -> str:
    return case.split(".", 1)[0]


def compare(recording: Dict[str, Any], rows: List[Dict[str, Any]], calls: List[Dict[str, Any]]) -> Dict[str, Any]:
    """The gate's verdict on one (model, suite): `regressions` (a check the recording
    passed now fails, and every LLM call of its journey matched exactly or by key),
    `masked` (the same, but some call of the journey matched only by nearness or not
    at all: prompt drift, reported only), `improvements`, `missing` (recorded checks
    the replay did not report) and the call match counts."""
    label = recording["label"]
    now = {r["case"]: bool(r["ok"]) for r in rows if r.get("rep", 0) == 0 and r.get("case") != "suite_error"}
    errors = [r for r in rows if r.get("case") == "suite_error"]
    mine = [c for c in calls if c.get("label") == label]
    counts = {m: sum(1 for c in mine if c.get("match") == m) for m in ("exact", "key", "nearest", "none")}
    dirty = {c.get("journey") for c in mine if c.get("match") in ("nearest", "none")}
    unstable = set(recording.get("unstable") or [])
    out: Dict[str, Any] = {"label": label, "suite": recording["suite"], "model": recording["model"],
                           "checks": len(recording["results"]), "unstable": len(unstable), "flaky": [],
                           "recorded_ok": sum(recording["results"].values()),
                           "replay_ok": sum(1 for c, ok in now.items() if ok and c in recording["results"]),
                           "regressions": [], "masked": [], "improvements": [], "missing": [],
                           "calls": counts, "suite_error": [e.get("detail") for e in errors]}
    for case, was in sorted(recording["results"].items()):
        if case in unstable:
            continue
        if case not in now:
            out["missing"].append(case)
            if was:
                (out["masked"] if journey_of(case) in dirty else out["regressions"]).append(case)
            continue
        if was and not now[case]:
            (out["masked"] if journey_of(case) in dirty else out["regressions"]).append(case)
        elif not was and now[case]:
            out["improvements"].append(case)
    return out


def render(reports: List[Dict[str, Any]]) -> str:
    head = ("suite", "checks", "recorded ok", "replay ok", "regressed", "drift-masked", "improved", "unstable",
            "calls exact/key/nearest/none")
    lines = ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    for r in reports:
        c = r["calls"]
        lines.append("| " + " | ".join(str(x) for x in (
            r["suite"], r["checks"], r["recorded_ok"], r["replay_ok"], len(r["regressions"]), len(r["masked"]),
            len(r["improvements"]), r["unstable"], f"{c['exact']}/{c['key']}/{c['nearest']}/{c['none']}")) + " |")
    for r in reports:
        for name, title in (("regressions", "REGRESSED (passed in the recording, fails now, answers matched)"),
                            ("masked", "failed after prompt drift (not counted)"),
                            ("flaky", "regressed once but not on a re-run (not counted)"),
                            ("improvements", "improved"), ("suite_error", "SUITE ERROR")):
            if r[name]:
                lines.append(f"\n{r['suite']}: {title}:")
                lines += [f"  - {x}" for x in r[name]]
    return "\n".join(lines)


def scan(paths: Iterable[str]) -> List[str]:
    """Problems in recordings: an Authorization header, or anything like an OpenRouter key."""
    problems = []
    for path in recording_paths(paths):
        opener = gzip.open if path.endswith(".gz") else open
        with opener(path, "rt", encoding="utf-8", errors="replace") as f:
            text = f.read()
        # A header, not an API's own `authorizations` field: `Authorization: Bearer ...`,
        # in any quoting, or a `headers` member anywhere in the recording.
        if AUTH_HEADER.search(text) or re.search(r'(?<!\\)"headers"\s*:', text):
            problems.append(f"{path}: holds an Authorization header")
        if "sk-or-" in text:
            problems.append(f"{path}: holds an 'sk-or-' string")
    return problems


# ---------------------------------------------------------------- processes


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_http(url: str, proc: subprocess.Popen, timeout: float = 30.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"{url}: process exited with {proc.returncode}")
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                if r.status == 200:
                    return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"{url} did not answer within {timeout:.0f} s")


def child_env(keep_key: bool = False) -> Dict[str, str]:
    """The environment of a child: without the key (and any variable holding it)
    unless it is the paid recording's proxy."""
    key = os.environ.get("OPENROUTER_API_KEY")
    env = {k: v for k, v in os.environ.items() if not (key and v == key) and k != "OPENROUTER_API_KEY"}
    if keep_key and key:
        env["OPENROUTER_API_KEY"] = key
    return env


def stop(proc: Optional[subprocess.Popen]) -> None:
    if proc and proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()


def run_suites(model: str, suites: List[str], proxy_base: str, out: str, args: argparse.Namespace,
               repeat: int) -> int:
    cmd = [sys.executable, os.path.join(HERE, "server_suites.py"), "--model", model, "--proxy-base", proxy_base,
           "--out", out, "--repeat", str(repeat), "--binary", args.binary, "--tag-journeys",
           "--jobs", str(args.jobs), "--suites-parallel"]
    for s in suites:
        cmd += ["--suite", s]
    log = open(os.path.join(out, "server_suites.log"), "w")
    try:
        return subprocess.call(cmd, cwd=REPO, env=child_env(), stdout=log, stderr=subprocess.STDOUT)
    finally:
        log.close()


def binary_path(path: str) -> str:
    if not os.path.isabs(path):
        path = os.path.join(REPO, path)
    return os.path.abspath(path)


def replay_once(docs: List[Dict[str, Any]], paths: List[str], args: argparse.Namespace
                ) -> Tuple[List[Dict[str, Any]], List[Dict[str, Any]]]:
    """Runs each recording's suite once (repeat 1) against a replaying proxy: the
    server.jsonl rows and the replay log's calls."""
    work = tempfile.mkdtemp(prefix="mb-replay-gate-")
    try:
        log_path = os.path.join(work, "replay-calls.jsonl")
        port = free_port()
        proxy_cmd = [sys.executable, os.path.join(HERE, "proxy.py"), "--port", str(port),
                     "--ledger", os.path.join(work, "calls.jsonl"), "--replay-log", log_path] + [x for p in paths for x in ("--replay", p)]
        with open(os.path.join(work, "proxy.log"), "w") as plog:
            proxy = subprocess.Popen(proxy_cmd, env=child_env(), stdout=plog, stderr=subprocess.STDOUT,
                                     start_new_session=True)
        try:
            base = f"http://127.0.0.1:{port}"
            wait_http(f"{base}/healthz", proxy)
            for model in sorted({d["model"] for d in docs}):
                run_suites(model, [d["suite"] for d in docs if d["model"] == model], base, work, args, repeat=1)
        finally:
            stop(proxy)
        server = os.path.join(work, "server.jsonl")
        rows = read_jsonl(server) if os.path.exists(server) else []
        calls = read_jsonl(log_path) if os.path.exists(log_path) else []
        return rows, calls
    finally:
        if getattr(args, "keep", False):
            print(f"kept {work}", file=sys.stderr)
        else:
            shutil.rmtree(work, ignore_errors=True)


def verdicts(docs: List[Dict[str, Any]], rows: List[Dict[str, Any]], calls: List[Dict[str, Any]]
             ) -> List[Dict[str, Any]]:
    out = []
    for d in sorted(docs, key=lambda d: (d["model"], JOURNEY_SUITES.index(d["suite"])
                                         if d["suite"] in JOURNEY_SUITES else 99, d["suite"])):
        mine = [r for r in rows if r.get("model") == d["model"] and r.get("suite") == d["suite"]]
        out.append(compare(d, mine, calls))
    return out


def cmd_gate(args: argparse.Namespace) -> int:
    started = time.monotonic()
    args.binary = binary_path(args.binary)
    if not os.path.exists(args.binary):
        print(f"gate: {args.binary} not found: cargo build --release -p mockingbird-cli", file=sys.stderr)
        return 2
    paths = recording_paths(args.recordings)
    docs = [read_recording(p) for p in paths]
    if args.suite:
        docs = [d for d in docs if d["suite"] in args.suite]
    if args.model:
        docs = [d for d in docs if d["model"] in args.model]
    if not docs:
        print("gate: no recording to replay", file=sys.stderr)
        return 2
    reports = verdicts(docs, *replay_once(docs, paths, args))
    # A regression must reproduce: the engine's own randomness (an id it makes up) can
    # flip a check once. The suites with one run again, and a check counts only when
    # it regressed every time.
    for _ in range(args.confirm):
        suspect = [r for r in reports if r["regressions"]]
        if not suspect:
            break
        again_docs = [d for d in docs if any(d["label"] == r["label"] for r in suspect)]
        again = {r["label"]: r for r in verdicts(again_docs, *replay_once(again_docs, paths, args))}
        for r in suspect:
            still = set(again[r["label"]]["regressions"])
            r["flaky"] += [c for c in r["regressions"] if c not in still]
            r["regressions"] = [c for c in r["regressions"] if c in still]
    print(render(reports))
    failed = [r for r in reports if r["regressions"] or r["suite_error"]]
    took = time.monotonic() - started
    if args.report:
        with open(args.report, "w") as f:
            json.dump({"reports": reports, "seconds": round(took, 1)}, f, indent=1)
    print(f"\nreplay gate: {sum(len(r['regressions']) for r in reports)} regression(s) in "
          f"{len(reports)} suite(s), {took:.0f} s")
    return 1 if failed else 0


def calibrate(paths: List[str], args: argparse.Namespace, runs: int) -> None:
    """Replays fresh recordings `runs` times on the binary that made them, and marks as
    `unstable` every check whose result is not the recorded one in each run: the
    engine decides it by chance (an id it makes up), so the gate cannot hold it."""
    docs = [read_recording(p) for p in paths]
    unstable: Dict[str, set] = {d["label"]: set() for d in docs}
    for _ in range(runs):
        for r in verdicts(docs, *replay_once(docs, paths, args)):
            d = next(x for x in docs if x["label"] == r["label"])
            now = set(r["regressions"]) | set(r["masked"]) | set(r["improvements"]) | set(r["missing"])
            unstable[r["label"]] |= {c for c in now if c in d["results"]}
    for path, d in zip(paths, docs):
        d["unstable"] = sorted(unstable[d["label"]])
        write_recording(path, d)


def cmd_record(args: argparse.Namespace) -> int:
    """Runs the suites once through a recording proxy, packs the recordings, then
    calibrates them with free replays."""
    args.binary = binary_path(args.binary)
    suites = args.suite or list(JOURNEY_SUITES)
    model = FAKE_MODEL if args.fake else args.model
    if not model:
        print("record: --model (a paid run) or --fake", file=sys.stderr)
        return 2
    if not args.fake and not os.environ.get("OPENROUTER_API_KEY"):
        print("record: OPENROUTER_API_KEY is not set (source ~/.zshenv)", file=sys.stderr)
        return 2
    work = tempfile.mkdtemp(prefix="mb-replay-record-")
    fake = None
    proxy = None
    try:
        raw = os.path.join(work, "raw.jsonl")
        port = free_port()
        proxy_cmd = [sys.executable, os.path.join(HERE, "proxy.py"), "--port", str(port),
                     "--ledger", os.path.join(work, "calls.jsonl"), "--record", raw,
                     # The fake LLM costs nothing, but the proxy prices it at its high fallback rates.
                     "--budget-usd", str(1e9 if args.fake else args.budget_usd)]
        if args.prices:
            proxy_cmd += ["--prices", args.prices]
        if args.fake:
            fport = free_port()
            fake = subprocess.Popen([sys.executable, os.path.join(HERE, "fake_llm.py"), "--port", str(fport)],
                                    env=child_env(), start_new_session=True)
            proxy_cmd += ["--upstream", f"http://127.0.0.1:{fport}/api/v1/chat/completions", "--provider-pins", ""]
        with open(os.path.join(work, "proxy.log"), "w") as plog:
            proxy = subprocess.Popen(proxy_cmd, env=child_env(keep_key=not args.fake), stdout=plog,
                                     stderr=subprocess.STDOUT, start_new_session=True)
        base = f"http://127.0.0.1:{port}"
        wait_http(f"{base}/healthz", proxy)
        code = run_suites(model, suites, base, work, args, repeat=1)
        stop(proxy)
        proxy = None
        try:
            written = pack(raw, os.path.join(work, "server.jsonl"), args.dest,
                           source="fake_llm.py" if args.fake else f"paid run via OpenRouter ({model})",
                           ledger=os.path.join(work, "calls.jsonl"))
        except PackRefused as e:
            print("record: refusing to pack this run, nothing written (--keep keeps the ledger):", file=sys.stderr)
            for p in e.problems:
                print(f"  - {p}", file=sys.stderr)
            return 1
        if args.calibrate:
            calibrate(written, args, args.calibrate)
        for p in written:
            doc = read_recording(p)
            print(f"{os.path.relpath(p, REPO)}: {len(doc['calls'])} calls, "
                  f"{sum(doc['results'].values())}/{len(doc['results'])} checks ok, "
                  f"{len(doc.get('unstable') or [])} unstable")
        problems = scan(written)
        for p in problems:
            print(p, file=sys.stderr)
        return 1 if problems or code else 0
    finally:
        stop(proxy)
        stop(fake)
        if args.keep:
            print(f"kept {work}", file=sys.stderr)
        else:
            shutil.rmtree(work, ignore_errors=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("gate", help="replay the recordings and compare per check (free, offline)")
    g.add_argument("--recordings", action="append", default=None, help=f"files or directories (default {RECORDINGS})")
    g.add_argument("--suite", action="append", help="only these suites")
    g.add_argument("--model", action="append", help="only these recorded models")
    g.add_argument("--binary", default="target/release/mockingbird")
    g.add_argument("--jobs", type=int, default=6, help="journeys at once per suite")
    g.add_argument("--confirm", type=int, default=2,
                   help="re-runs of a suite with a regression; it counts only if it regresses every time")
    g.add_argument("--report", help="write the verdict as JSON here")
    g.add_argument("--keep", action="store_true", help="keep the work directory")
    r = sub.add_parser("record", help="record a run's LLM answers and results (paid unless --fake)")
    r.add_argument("--model", help="the model to record (a paid run through OpenRouter)")
    r.add_argument("--fake", action="store_true", help=f"record fake_llm.py's answers as {FAKE_MODEL} (free)")
    r.add_argument("--suite", action="append", help="default: every journeys suite")
    r.add_argument("--dest", default=RECORDINGS)
    r.add_argument("--binary", default="target/release/mockingbird")
    r.add_argument("--jobs", type=int, default=6)
    r.add_argument("--budget-usd", type=float, default=1.5,
                   help="the recording proxy's hard budget (one rep of all 14 suites on one model is about $0.2)")
    r.add_argument("--prices", help="roster.json (per-token prices) for the budget")
    r.add_argument("--calibrate", type=int, default=4,
                   help="free replays after recording; a check that changes in any is marked unstable")
    r.add_argument("--keep", action="store_true")
    p = sub.add_parser("pack", help="raw recording + server.jsonl -> recordings")
    p.add_argument("--raw", required=True)
    p.add_argument("--server", required=True)
    p.add_argument("--dest", default=RECORDINGS)
    p.add_argument("--source", required=True, help="what answered the calls, kept in the recording")
    p.add_argument("--rep", type=int, default=0)
    p.add_argument("--ledger", help="the proxy's calls.jsonl: its failed calls and a BUDGET_TRIPPED next to it "
                   "refuse the pack too")
    s = sub.add_parser("scan", help="fail when a recording holds an Authorization header or an sk-or- string")
    s.add_argument("paths", nargs="*")
    args = ap.parse_args()
    if args.cmd == "gate":
        args.recordings = args.recordings or [RECORDINGS]
        return cmd_gate(args)
    if args.cmd == "record":
        return cmd_record(args)
    if args.cmd == "pack":
        try:
            written = pack(args.raw, args.server, args.dest, args.source, args.rep, ledger=args.ledger)
        except PackRefused as e:
            print("pack: refusing to pack this run, nothing written:", file=sys.stderr)
            for p in e.problems:
                print(f"  - {p}", file=sys.stderr)
            return 1
        for path in written:
            print(path)
        return 0
    problems = scan(args.paths or [RECORDINGS])
    for line in problems:
        print(line, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
