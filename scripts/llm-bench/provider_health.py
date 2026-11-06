"""Checks a model's pinned OpenRouter endpoints are healthy before a bench run starts.

A pin (providers.json) routes every call of a model to one endpoint with no fallback,
so when that endpoint is degraded the run measures the provider's timeouts and 502s
as the model's failures. Before a run starts (`server_suites.py`, and `run.py full`
before each model), this reads OpenRouter's public endpoint list for the model,
`GET <api>/models/<id>/endpoints`, which needs no key; the key is never sent to it.

- A pinned endpoint is healthy when its `status` is HEALTHY_STATUS (0) and its
  `uptime_last_30m` is at least MIN_UPTIME_30M (99%). A pin entry that matches no
  listed endpoint is degraded too: OpenRouter has nothing to send its calls to.
- Any degraded pinned endpoint: the run refuses to start (EXIT_PROVIDER_DEGRADED),
  naming the model, the endpoint, its status and its uptime, unless
  --allow-degraded-provider is given; then it warns and starts.
- The list cannot be fetched: a warning, and the run goes on (verdict `unknown`).
- An unpinned model never refuses; its endpoints are recorded for information.

Every check is recorded in <out>/providers-status.json (one record per model, the
latest check) and summarised on stderr. Standard library only.
"""

import argparse
import datetime
import json
import os
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Callable, Dict, Iterable, List, Optional

OPENROUTER_API = "https://openrouter.ai/api/v1"
# A healthy endpoint: OpenRouter's status 0, and at least this uptime (percent) over
# the last 30 minutes. A lightning run that got 180 timeouts and 209 502s started
# while Fireworks was at status -2 and 90% uptime; every other endpoint was 0 and 100%.
HEALTHY_STATUS = 0
MIN_UPTIME_30M = 99.0
FETCH_TIMEOUT_S = 10.0
STATUS_FILE = "providers-status.json"
# server_suites.py's exit status when it refuses to start (3 is EXIT_BUDGET).
EXIT_PROVIDER_DEGRADED = 4
# What a record keeps of each listed endpoint.
ENDPOINT_FIELDS = ("tag", "provider_name", "status", "uptime_last_30m", "uptime_last_5m", "quantization",
                   "service_tier")


class EndpointListError(Exception):
    """OpenRouter's endpoint list could not be read."""


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def base_model(model: str) -> str:
    """`<id>@nojson` is the bench's variant of `<id>`; OpenRouter knows only `<id>`."""
    return model.split("@", 1)[0]


def pin_for(pins: Dict[str, Dict[str, Any]], model: str) -> Optional[Dict[str, Any]]:
    return pins.get(model) or pins.get(base_model(model))


def endpoints_url(api: str, model: str) -> str:
    return f"{api.rstrip('/')}/models/{urllib.parse.quote(base_model(model), safe='/')}/endpoints"


def fetch_endpoints(api: str, model: str, timeout: float = FETCH_TIMEOUT_S) -> List[Dict[str, Any]]:
    """The model's endpoint list. A public endpoint: no Authorization header is sent."""
    req = urllib.request.Request(endpoints_url(api, model), headers={"Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            doc = json.load(r)
    except urllib.error.HTTPError as e:
        raise EndpointListError(f"HTTP {e.code}") from None
    except (urllib.error.URLError, OSError, ValueError) as e:
        raise EndpointListError(f"{type(e).__name__}: {getattr(e, 'reason', e)}") from None
    data = doc.get("data") if isinstance(doc, dict) else None
    endpoints = data.get("endpoints") if isinstance(data, dict) else None
    if not isinstance(endpoints, list):
        raise EndpointListError("the answer has no data.endpoints list")
    return [e for e in endpoints if isinstance(e, dict)]


def pin_entry_matches(entry: str, endpoint: Dict[str, Any]) -> bool:
    """Whether a pin entry names this endpoint, as providers.json documents it: a full
    tag (`wafer/us`) matches only that tag; a base slug (`fireworks`) matches each of
    that provider's endpoints except service tiers (`openai/flex`, `openai/fast`)."""
    entry = str(entry).lower()
    tag = str(endpoint.get("tag") or "").lower()
    if "/" in entry:
        return tag == entry
    return tag.split("/", 1)[0] == entry and not endpoint.get("service_tier")


def quantization_allowed(pin: Dict[str, Any], endpoint: Dict[str, Any]) -> bool:
    allowed = pin.get("quantizations")
    return not allowed or endpoint.get("quantization") in allowed


def endpoint_healthy(endpoint: Dict[str, Any]) -> bool:
    uptime = endpoint.get("uptime_last_30m")
    return (endpoint.get("status") == HEALTHY_STATUS and isinstance(uptime, (int, float))
            and uptime >= MIN_UPTIME_30M)


def fmt_uptime(uptime: Any) -> str:
    return f"{uptime:.1f}%" if isinstance(uptime, (int, float)) else "unknown"


def assess(model: str, pin: Optional[Dict[str, Any]], endpoints: List[Dict[str, Any]]) -> Dict[str, Any]:
    """A record of the model's endpoints and, when it is pinned, whether the pin is healthy."""
    order = list((pin or {}).get("order") or [])
    rows = []
    for e in endpoints:
        row = {k: e.get(k) for k in ENDPOINT_FIELDS}
        row["pinned"] = bool(order) and quantization_allowed(pin, e) and any(pin_entry_matches(p, e) for p in order)
        row["healthy"] = endpoint_healthy(e)
        rows.append(row)
    problems = []
    for entry in order:
        if not any(r["pinned"] and pin_entry_matches(entry, r) for r in rows):
            problems.append(f"{model}: pinned endpoint `{entry}` is not in OpenRouter's endpoint list"
                            + (f" with quantization {pin['quantizations']}" if pin.get("quantizations") else ""))
    for r in rows:
        if r["pinned"] and not r["healthy"]:
            problems.append(f"{model}: pinned endpoint `{r['tag']}` ({r['provider_name']}) is degraded: "
                            f"status {r['status']}, uptime {fmt_uptime(r['uptime_last_30m'])} over the last 30 min "
                            f"(healthy: status {HEALTHY_STATUS} and uptime >= {MIN_UPTIME_30M:g}%)")
    verdict = "unpinned" if not order else ("degraded" if problems else "healthy")
    return {"model": model, "pin": order or None, "verdict": verdict, "problems": problems,
            "fetch_error": None, "endpoints": rows}


def check_model(model: str, pin: Optional[Dict[str, Any]], api: str = OPENROUTER_API,
                timeout: float = FETCH_TIMEOUT_S,
                fetch: Callable[[str, str, float], List[Dict[str, Any]]] = fetch_endpoints) -> Dict[str, Any]:
    checked_at = utc_now()
    try:
        rec = assess(model, pin, fetch(api, model, timeout))
    except EndpointListError as e:
        rec = {"model": model, "pin": list((pin or {}).get("order") or []) or None, "verdict": "unknown",
               "problems": [], "fetch_error": str(e), "endpoints": []}
    rec["checked_at"] = checked_at
    rec["source"] = endpoints_url(api, model)
    rec["allowed_degraded"] = False
    return rec


def summary_lines(rec: Dict[str, Any]) -> List[str]:
    model = rec["model"]
    if rec["fetch_error"]:
        return [f"warning: {model}: cannot read OpenRouter's endpoint list ({rec['fetch_error']}); "
                f"provider health unknown, starting anyway"]
    def describe(rows: List[Dict[str, Any]]) -> str:
        return ", ".join(f"{r['tag']} status {r['status']} uptime {fmt_uptime(r['uptime_last_30m'])}"
                         for r in rows) or "none"

    rows = rec["endpoints"]
    if rec["pin"]:
        return [f"{model}: pin {', '.join(rec['pin'])} {rec['verdict']}: "
                f"{describe([r for r in rows if r['pinned']])}"]
    # Unpinned: for information only; the endpoints below the bar are named.
    healthy = sum(1 for r in rows if r["healthy"])
    return [f"{model}: unpinned, {healthy} of {len(rows)} endpoints healthy; others: "
            f"{describe([r for r in rows if not r['healthy']])}"]


def write_status(out: str, records: Iterable[Dict[str, Any]]) -> str:
    """Merges the records into <out>/providers-status.json (one per model, the latest)."""
    path = os.path.join(out, STATUS_FILE)
    try:
        with open(path, encoding="utf-8") as f:
            doc = json.load(f)
    except (OSError, ValueError):
        doc = {}
    if not isinstance(doc, dict) or not isinstance(doc.get("models"), dict):
        doc = {"models": {}}
    doc["healthy_when"] = {"status": HEALTHY_STATUS, "min_uptime_last_30m": MIN_UPTIME_30M}
    for rec in records:
        doc["models"][rec["model"]] = rec
    os.makedirs(out, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".providers-status.", dir=out)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(doc, f, indent=2, sort_keys=True)
        f.write("\n")
    os.replace(tmp, path)
    return path


def gate(model: str, pins: Dict[str, Dict[str, Any]], out: str, api: str, allow_degraded: bool,
         log: Callable[[str], None], timeout: float = FETCH_TIMEOUT_S) -> Optional[str]:
    """Checks one model, records it and logs the summary. Returns why the model must not
    start (a degraded pin without allow_degraded), else None."""
    rec = check_model(model, pin_for(pins, model), api, timeout)
    degraded = rec["verdict"] == "degraded"
    rec["allowed_degraded"] = degraded and allow_degraded
    write_status(out, [rec])
    for line in summary_lines(rec):
        log(line)
    if not degraded:
        return None
    if allow_degraded:
        for p in rec["problems"]:
            log(f"warning: {p}; starting anyway (--allow-degraded-provider)")
        return None
    for p in rec["problems"]:
        log(f"refusing to start: {p}")
    log(f"{model}: its calls would measure the provider's failures as the model's. Wait for the endpoint "
        f"to recover, change the pin in providers.json, or pass --allow-degraded-provider to run anyway.")
    return "; ".join(rec["problems"])


def add_args(p: argparse.ArgumentParser) -> None:
    p.add_argument("--allow-degraded-provider", action="store_true",
                   help="start even when a pinned OpenRouter endpoint is degraded (status not "
                        f"{HEALTHY_STATUS} or 30-min uptime under {MIN_UPTIME_30M:g}%%); it is recorded")
    p.add_argument("--no-provider-check", action="store_true",
                   help="do not read OpenRouter's endpoint list before starting (offline runs, replay)")


def stderr_log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)
