#!/usr/bin/env python3
"""Recording, budget-guarded OpenRouter proxy for the LLM model benchmark.

One process serves every model. Clients use the base URL

    http://127.0.0.1:P/r/<model-slug>~<workload>/api/v1

(model slug = model id with "/" replaced by "__"). The proxy strips
`/r/<label>`, forwards `POST /api/v1/chat/completions` to OpenRouter with
the real key from the environment (the client's Authorization is replaced),
and appends one JSON line per client call to the ledger.

    source ~/.zshenv >/dev/null 2>&1   # OPENROUTER_API_KEY, never printed
    python3 scripts/llm-bench/proxy.py --port 8787 \\
        --ledger docs/benchmarks/2026-10-29/calls.jsonl --budget-usd 9.0 \\
        --prices docs/benchmarks/2026-10-29/roster.json

It is the bench's budget hard stop: cumulative spend (the ledger's cost, plus
any --prior-ledger, plus in-flight reservations) never crosses --budget-usd;
once it would, every call gets 402 and `<ledger dir>/BUDGET_TRIPPED` exists.

`GET /stats` reports spend; `GET /healthz` answers 200. `--record` and `--replay`
serve replay.py's gate: `--replay` answers from recordings and never calls upstream
or reads the key. Standard library only.
"""

import argparse
import copy
import datetime
import http.client
import gzip
import json
import os
import signal
import socket
import sys
import threading
import time
import urllib.error
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Dict, Iterator, List, Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import replay  # noqa: E402
import re  # noqa: E402

OPENROUTER_URL = "https://openrouter.ai/api/v1/chat/completions"
UPSTREAM_TIMEOUT_S = 120.0
REFERER = "https://github.com/rj-s-org/mockingbird"
TITLE = "Mockingbird bench"
DEFAULT_MAX_TOKENS = 4096
# Per-token prices used for a model the roster does not price: deliberately
# high, so an unknown model reserves too much rather than too little.
FALLBACK_PRICES = (1.0e-5, 4.0e-5)
EXCERPT_CHARS = 300
# The message of the proxy's 402 once the budget is reached. server_suites.py
# imports it (through `budget_exhausted`) to tell a faker answer the bench's own
# budget forced from a model failure; define it nowhere else.
BUDGET_EXHAUSTED_MESSAGE = "bench budget exhausted"
# Mockingbird reports a refused LLM call as `LLM returned HTTP 402: <body>`; the
# body is the proxy's own refusal, so its message is matched exactly.
BUDGET_EXHAUSTED_REASON = re.compile(
    r'LLM returned HTTP 402\b[^"]*\{\s*"error"\s*:\s*\{\s*"message"\s*:\s*"'
    + re.escape(BUDGET_EXHAUSTED_MESSAGE) + '"'
)
# In-flight reservations follow measured cost once a model has this many
# successful calls in the ledger: the MEASURED_PERCENTILE cost of its calls times
# MEASURED_SAFETY (never above the worst case). Before that, the worst case.
MEASURED_MIN_CALLS = 20
MEASURED_PERCENTILE = 0.9
MEASURED_SAFETY = 1.5
REASONING_REJECT_STATUSES = (400, 404, 422)
RESPONSE_FORMAT_REJECT_STATUSES = (400, 422)
RESPONSE_FORMAT_MARKERS = (
    "response_format",
    "response format",
    "json mode",
    "json_mode",
    "json_object",
    "json_schema",
    "structured output",
)


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds")


# Strings never to echo (the upstream key); set by Proxy at start.
SECRETS: List[str] = []


# A model id may carry a bench variant suffix the proxy strips before
# forwarding: `<id>@nojson` drops `response_format` (JSON mode), for models
# whose catalog entry lists no response_format support.
VARIANT_NOJSON = "@nojson"


def base_model(model: str) -> str:
    return model.split("@", 1)[0]


def redact(text: str) -> str:
    for secret in SECRETS:
        if secret:
            text = text.replace(secret, "[redacted]")
    return text


def redact_bytes(data: bytes) -> bytes:
    for secret in SECRETS:
        if secret:
            data = data.replace(secret.encode(), b"[redacted]")
    return data


def excerpt(text: str) -> str:
    text = " ".join(redact(str(text)).split())
    return text[:EXCERPT_CHARS]


def budget_refusal() -> Dict[str, Any]:
    """The body of the proxy's 402 once the budget is reached."""
    return {"error": {"message": BUDGET_EXHAUSTED_MESSAGE, "code": 402}}


def budget_exhausted(reason: Optional[str]) -> bool:
    """Whether a Mockingbird fallback reason (`x-mockingbird-fallback-reason`, or an
    import job's error) is the bench proxy refusing the call on its budget."""
    return bool(reason) and BUDGET_EXHAUSTED_REASON.search(str(reason)) is not None


def percentile(sorted_xs: List[float], p: float) -> float:
    """The nearest-rank `p` percentile (0..1] of a sorted, non-empty list."""
    k = max(0, min(len(sorted_xs) - 1, int(-(-p * len(sorted_xs) // 1)) - 1))
    return sorted_xs[k]


def as_float(v: Any) -> Optional[float]:
    try:
        if v is None or v == "":
            return None
        return float(v)
    except (TypeError, ValueError):
        return None


def as_int(v: Any) -> Optional[int]:
    try:
        if v is None or isinstance(v, bool):
            return None
        return int(v)
    except (TypeError, ValueError):
        return None


# --------------------------------------------------------------------------
# Prices


def load_prices(path: Optional[str]) -> Dict[str, Tuple[float, float]]:
    """Per-token (prompt, completion) prices by model id.

    Accepts the roster `run.py preflight` writes (`{"models": [...]}` with
    `pricing: {prompt, completion}` per token), a bare list of such entries,
    or OpenRouter's raw `/models` answer (`{"data": [...]}`).
    """
    prices: Dict[str, Tuple[float, float]] = {}
    if not path:
        return prices
    with open(path, encoding="utf-8") as f:
        doc = json.load(f)
    if isinstance(doc, dict):
        entries = doc.get("models") or doc.get("data") or []
        if isinstance(entries, dict):
            entries = [dict(v, id=k) if isinstance(v, dict) else {} for k, v in entries.items()]
    else:
        entries = doc
    for e in entries:
        if not isinstance(e, dict) or not e.get("id"):
            continue
        pricing = e.get("pricing") or {}
        p = as_float(pricing.get("prompt", e.get("prompt_price")))
        c = as_float(pricing.get("completion", e.get("completion_price")))
        if p is None or c is None:
            continue
        prices[e["id"]] = (p, c)
    return prices


# --------------------------------------------------------------------------
# Provider pins

DEFAULT_PROVIDER_PINS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "providers.json")
# OpenRouter's own words when no provider the routing allows can serve the call. Only
# these phrases count: other 4xx bodies that mention a provider or an endpoint ("Provider
# returned error", "This endpoint's maximum context length ...", "No endpoints found that
# can handle the requested parameters") are the request's own errors, and mb-llm steps
# down from them (schema, reasoning), so they must stay 4xx. OpenRouter documents the
# routing failure as a 503, which passes through as a 5xx anyway; the 404 text is the
# one it sends when allowed providers rule out every endpoint.
PIN_UNAVAILABLE_STATUSES = (400, 404)
PIN_UNAVAILABLE_MARKERS = (
    "no allowed providers are available",
    "no available model provider that meets your routing requirements",
)


class PinRefused(Exception):
    """The first streamed chunk came from a provider the pin does not allow."""


class ClientClosed(Exception):
    """The client hung up on a stream mid-generation: stop reading from the provider."""


# The error_kind of a call the client hung up on mid-generation (status 499). It is
# neither a success nor an error: every count of errors leaves it out.
CLIENT_CLOSED = "client_closed"


def is_error(rec: Dict[str, Any]) -> bool:
    """Whether a ledger record is an error: any error_kind except a client hang-up."""
    return bool(rec.get("error_kind")) and rec.get("error_kind") != CLIENT_CLOSED


def load_provider_pins(path: Optional[str]) -> Dict[str, Dict[str, Any]]:
    """OpenRouter `provider` objects by model id, from `providers.json`.

    The file is `{"pins": {"<model id>": {"provider": {...}, "basis": "..."}}}`;
    a pin without a non-empty `provider.order` is ignored. No path: no pins.
    """
    pins: Dict[str, Dict[str, Any]] = {}
    if not path:
        return pins
    with open(path, encoding="utf-8") as f:
        doc = json.load(f)
    entries = doc.get("pins") if isinstance(doc, dict) else None
    for model, entry in (entries or {}).items():
        prov = entry.get("provider") if isinstance(entry, dict) else None
        if isinstance(prov, dict) and isinstance(prov.get("order"), list) and prov["order"]:
            pins[str(model)] = copy.deepcopy(prov)
    return pins


def provider_key(name: Any) -> str:
    """`Io Net`, `io-net` and `io-net/fp8` all give `ionet`."""
    return "".join(ch for ch in str(name).split("/", 1)[0].lower() if ch.isalnum())


def pin_allows(pin: Dict[str, Any], served: Optional[str]) -> bool:
    """Whether the provider that served a call is one the pin allows (unknown: yes)."""
    if not served:
        return True
    return provider_key(served) in {provider_key(p) for p in pin.get("order") or []}


def apply_provider_pin(up: Dict[str, Any], pin: Optional[Dict[str, Any]]) -> Optional[str]:
    """Adds `pin` as the upstream body's `provider` unless the client sent one.

    Returns who set the routing: "proxy", "client" or None.
    """
    if "provider" in up:
        return "client"
    if pin:
        up["provider"] = copy.deepcopy(pin)
        return "proxy"
    return None


# --------------------------------------------------------------------------
# Ledger and budget


class Ledger:
    """Thread-safe call ledger and budget guard."""

    def __init__(self, path: str, budget_usd: float, prior: List[str]):
        self.path = path
        self.dir = os.path.dirname(os.path.abspath(path))
        self.trip_file = os.path.join(self.dir, "BUDGET_TRIPPED")
        self.budget = budget_usd
        self.lock = threading.Lock()
        self.spent = 0.0  # this ledger's recorded cost
        self.prior_spent = 0.0  # cost in --prior-ledger files
        self.reserved = 0.0  # in-flight reservations
        self.in_flight = 0
        self.calls = 0
        self.per_label: Dict[str, Dict[str, Any]] = {}
        self.model_costs: Dict[str, List[float]] = {}  # successful calls' cost, by base model id
        os.makedirs(self.dir, exist_ok=True)
        prior_tripped = False
        for p in prior:
            if os.path.abspath(p) == os.path.abspath(path):
                continue
            for rec in self._read(p):
                self.prior_spent += as_float(rec.get("cost_usd")) or 0.0
            # A trip in another phase (the pilot's ledger dir) stays a trip.
            prior_tripped = prior_tripped or os.path.exists(
                os.path.join(os.path.dirname(os.path.abspath(p)), "BUDGET_TRIPPED")
            )
        for rec in self._read(path):
            self._tally(rec, live=False)
        self.tripped = prior_tripped or os.path.exists(self.trip_file) or self.total_spent() >= self.budget
        if self.tripped and not os.path.exists(self.trip_file):
            self._write_trip("spend loaded at start already reaches the budget")
        self.fh = open(path, "a", encoding="utf-8")

    @staticmethod
    def _read(path: str) -> Iterator[Dict[str, Any]]:
        if not os.path.exists(path):
            return
        opener = gzip.open if path.endswith(".gz") else open
        with opener(path, "rt", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    rec = json.loads(line)
                except ValueError:
                    continue
                if isinstance(rec, dict):
                    yield rec

    def _tally(self, rec: Dict[str, Any], live: bool = True) -> None:
        """Counts a record. `providers` counts only calls made since this proxy
        started (`live`), so a resumed ledger's earlier rows stay out of this
        run's provider mix; spend and call counts include them."""
        cost = as_float(rec.get("cost_usd")) or 0.0
        self.spent += cost
        self.calls += 1
        if rec.get("status") == 200 and not is_error(rec) and cost > 0:
            model = rec.get("model_requested") or str(rec.get("model_slug") or "").replace("__", "/")
            if model:
                self.model_costs.setdefault(base_model(str(model)), []).append(cost)
        label = rec.get("label") or "?"
        s = self.per_label.setdefault(
            label, {"calls": 0, "cost_usd": 0.0, "errors": 0, "client_closed": 0, "budget_refused": 0,
                    "prompt_tokens": 0, "completion_tokens": 0, "providers": {}}
        )
        s["calls"] += 1
        if rec.get("error_kind") == "budget":
            # server_suites.py compares this before and after a suite: a refusal it could
            # not see in an answer (an SSE stream carries no fallback reason) still counts.
            s["budget_refused"] += 1
        prov = rec.get("provider")
        if prov and live:
            s["providers"][prov] = s["providers"].get(prov, 0) + 1
        s["cost_usd"] += cost
        if is_error(rec):
            s["errors"] += 1
        if rec.get("error_kind") == CLIENT_CLOSED:
            s["client_closed"] += 1
        s["prompt_tokens"] += as_int(rec.get("prompt_tokens")) or 0
        s["completion_tokens"] += as_int(rec.get("completion_tokens")) or 0

    def total_spent(self) -> float:
        return self.spent + self.prior_spent

    def measured_reserve(self, model: str) -> Optional[Tuple[float, int]]:
        """(reservation, calls measured) from this ledger's successful calls of `model`
        (any variant), or None while it has fewer than MEASURED_MIN_CALLS."""
        with self.lock:
            costs = sorted(self.model_costs.get(base_model(model), ()))
        if len(costs) < MEASURED_MIN_CALLS:
            return None
        return percentile(costs, MEASURED_PERCENTILE) * MEASURED_SAFETY, len(costs)

    def measured_mean(self, model: str) -> Optional[float]:
        with self.lock:
            costs = self.model_costs.get(base_model(model)) or []
            return sum(costs) / len(costs) if costs else None

    def _write_trip(self, why: str) -> None:
        try:
            with open(self.trip_file, "w", encoding="utf-8") as f:
                json.dump(
                    {
                        "ts": utc_now(),
                        "reason": why,
                        "spent_usd": round(self.total_spent(), 6),
                        "budget_usd": self.budget,
                    },
                    f,
                )
                f.write("\n")
        except OSError as e:
            print(f"proxy: cannot write {self.trip_file}: {e}", file=sys.stderr)

    def reserve(self, estimate: float) -> bool:
        """Reserves `estimate` USD for a call, or trips the budget."""
        with self.lock:
            if self.tripped:
                return False
            if self.total_spent() + self.reserved + estimate >= self.budget:
                self.tripped = True
                self._write_trip(
                    f"spent {self.total_spent():.4f} + reserved {self.reserved:.4f} "
                    f"+ next {estimate:.4f} >= budget {self.budget:.2f}"
                )
                print(f"proxy: BUDGET TRIPPED at ${self.total_spent():.4f}", file=sys.stderr, flush=True)
                return False
            self.reserved += estimate
            self.in_flight += 1
            return True

    def settle(self, estimate: float, rec: Dict[str, Any], reserved: bool) -> None:
        """Releases a reservation and appends the call's record."""
        line = json.dumps(rec, separators=(",", ":"), ensure_ascii=False)
        with self.lock:
            if reserved:
                self.reserved = max(0.0, self.reserved - estimate)
                self.in_flight = max(0, self.in_flight - 1)
            self._tally(rec)
            self.fh.write(line + "\n")
            self.fh.flush()
            if not self.tripped and self.total_spent() >= self.budget:
                self.tripped = True
                self._write_trip(f"spent {self.total_spent():.4f} >= budget {self.budget:.2f}")

    def stats(self) -> Dict[str, Any]:
        with self.lock:
            return {
                "spent_usd": round(self.total_spent(), 6),
                "ledger_spent_usd": round(self.spent, 6),
                "prior_spent_usd": round(self.prior_spent, 6),
                "reserved_usd": round(self.reserved, 6),
                "in_flight": self.in_flight,
                "budget_usd": self.budget,
                "calls": self.calls,
                "tripped": self.tripped,
                "per_label": {
                    k: dict(v, cost_usd=round(v["cost_usd"], 6), providers=dict(v["providers"]))
                    for k, v in sorted(self.per_label.items())
                },
            }


# --------------------------------------------------------------------------
# Per-call accumulation


class Outcome:
    """What one upstream exchange produced."""

    def __init__(self) -> None:
        self.status = 0
        self.error_kind: Optional[str] = None
        self.error_excerpt: Optional[str] = None
        self.error_body: Optional[bytes] = None
        self.content: List[str] = []
        self.reasoning: List[str] = []
        self.refusal: Optional[str] = None
        self.finish_reason: Optional[str] = None
        self.client_gone_after_end = False  # the client hung up after the answer ended
        self.usage: Optional[Dict[str, Any]] = None
        self.provider: Optional[str] = None
        self.model: Optional[str] = None
        self.gen_id: Optional[str] = None
        self.created: Optional[int] = None
        self.ttft_ms: Optional[float] = None
        self.ttfr_ms: Optional[float] = None
        self.started = time.monotonic()

    def since(self) -> float:
        return round((time.monotonic() - self.started) * 1000.0, 1)

    def take_meta(self, obj: Dict[str, Any]) -> None:
        if obj.get("provider") and not self.provider:
            self.provider = obj.get("provider")
        if obj.get("model") and not self.model:
            self.model = obj.get("model")
        if obj.get("id") and not self.gen_id:
            self.gen_id = obj.get("id")
        if obj.get("created") and not self.created:
            self.created = as_int(obj.get("created"))
        if isinstance(obj.get("usage"), dict):
            self.usage = obj["usage"]

    def take_chunk(self, chunk: Dict[str, Any]) -> None:
        """Folds one streamed `chat.completion.chunk` in."""
        self.take_meta(chunk)
        if chunk.get("error") is not None:
            self.set_stream_error(chunk)
        for choice in chunk.get("choices") or []:
            if not isinstance(choice, dict):
                continue
            delta = choice.get("delta") or {}
            text = delta.get("content")
            if isinstance(text, str) and text:
                if self.ttft_ms is None:
                    self.ttft_ms = self.since()
                self.content.append(text)
            reasoning = delta.get("reasoning")
            if reasoning is None:
                reasoning = delta.get("reasoning_content")
            if isinstance(reasoning, str) and reasoning:
                if self.ttfr_ms is None:
                    self.ttfr_ms = self.since()
                self.reasoning.append(reasoning)
            if delta.get("refusal"):
                self.refusal = (self.refusal or "") + str(delta["refusal"])
            if choice.get("finish_reason"):
                self.finish_reason = choice["finish_reason"]
                if choice["finish_reason"] == "error" and self.error_kind is None:
                    self.set_stream_error(choice)

    def take_message(self, doc: Dict[str, Any]) -> None:
        """Folds a non-streamed `chat.completion` in."""
        self.take_meta(doc)
        if doc.get("error") is not None:
            self.set_stream_error(doc)
        choices = doc.get("choices") or []
        if choices and isinstance(choices[0], dict):
            msg = choices[0].get("message") or {}
            if isinstance(msg.get("content"), str):
                self.content.append(msg["content"])
            r = msg.get("reasoning") or msg.get("reasoning_content")
            if isinstance(r, str):
                self.reasoning.append(r)
            if msg.get("refusal"):
                self.refusal = str(msg["refusal"])
            self.finish_reason = choices[0].get("finish_reason")

    def set_stream_error(self, obj: Dict[str, Any]) -> None:
        err = obj.get("error")
        self.error_kind = "stream_error"
        body = {"error": err if err is not None else {"message": "stream finished with an error"}}
        self.error_body = json.dumps(body).encode()
        self.error_excerpt = excerpt(json.dumps(body["error"]))

    def text(self) -> str:
        return "".join(self.content)

    def reasoning_text(self) -> str:
        return "".join(self.reasoning)


def sse_events(resp: Any, on_line: Any = None, deadline: Optional[float] = None) -> Iterator[str]:
    """Yields each SSE event's data from an upstream response.

    Comment lines (OpenRouter's `: OPENROUTER PROCESSING` keep-alives) and
    non-data fields are skipped; multi-line data is joined per the SSE spec.
    `on_line(raw_bytes)` sees every raw line (for pass-through). Past the
    monotonic `deadline` (checked on every line, keep-alives included) it
    raises `socket.timeout`.
    """
    data: List[str] = []
    while True:
        raw = resp.readline()
        if not raw:
            break
        if deadline is not None and time.monotonic() > deadline:
            raise socket.timeout("stream exceeded the upstream timeout")
        if on_line is not None:
            on_line(raw)
        line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
        if line == "":
            if data:
                yield "\n".join(data)
                data = []
            continue
        if line.startswith(":"):
            continue
        field, _, value = line.partition(":")
        if value.startswith(" "):
            value = value[1:]
        if field == "data":
            data.append(value)
    if data:
        yield "\n".join(data)


# --------------------------------------------------------------------------
# Proxy


class Proxy:
    def dump(self, rec: Dict[str, Any], body: Dict[str, Any], out: "Outcome") -> None:
        """Appends the call's prompt and answer to `--dump` (no headers, so no key)."""
        if not self.dump_path:
            return
        line = json.dumps({
            "ts": rec.get("ts"),
            "label": rec.get("label"),
            "model_requested": rec.get("model_requested"),
            "provider": rec.get("provider"),
            "generation_id": rec.get("generation_id"),
            "total_ms": rec.get("total_ms"),
            "ttft_ms": rec.get("ttft_ms"),
            "messages": body.get("messages"),
            "response_format": body.get("response_format"),
            "content": "".join(out.content),
            "reasoning": "".join(out.reasoning)[:4000],
            "finish_reason": out.finish_reason,
        }, ensure_ascii=False)
        with self.dump_lock:
            with open(self.dump_path, "a") as f:
                f.write(redact(line) + "\n")

    def __init__(self, args: argparse.Namespace):
        self.upstream = args.upstream
        self.stream_upgrade = not args.no_stream_upgrade
        self.dump_path = args.dump
        self.dump_lock = threading.Lock()
        # --replay answers every call from recordings: no upstream, no key, no ledger.
        replays = getattr(args, "replay", None) or []
        self.replayer = replay.Replayer(replays, getattr(args, "replay_log", None)) if replays else None
        record = getattr(args, "record", None)
        self.recorder = replay.Recorder(record) if record and not self.replayer else None
        ledger = args.ledger
        if self.replayer and not ledger:
            import tempfile
            ledger = os.path.join(tempfile.mkdtemp(prefix="mb-replay-ledger-"), "calls.jsonl")
        self.ledger = Ledger(ledger, args.budget_usd, [] if self.replayer else (args.prior_ledger or []))
        self.prices = load_prices(args.prices)
        self.pins = load_provider_pins(getattr(args, "provider_pins", None))
        if self.prices:
            self.fallback = (
                max(p for p, _ in self.prices.values()),
                max(c for _, c in self.prices.values()),
            )
            self.fallback = (max(self.fallback[0], FALLBACK_PRICES[0]), max(self.fallback[1], FALLBACK_PRICES[1]))
        else:
            self.fallback = FALLBACK_PRICES
        self.key = "" if self.replayer else os.environ.get("OPENROUTER_API_KEY", "")
        if self.key:
            SECRETS.append(self.key)
        split = urllib.parse.urlsplit(self.upstream)
        self.up_https = split.scheme == "https"
        self.up_host = split.hostname or ""
        self.up_port = split.port
        self.up_path = (split.path or "/") + (f"?{split.query}" if split.query else "")
        self.memo_lock = threading.Lock()
        self.reasoning_rejected: set = set()
        self.response_format_rejected: set = set()
        self.format_fallback = not getattr(args, "no_format_fallback", False)

    def price(self, model: str) -> Tuple[float, float]:
        return self.prices.get(base_model(model), self.fallback)

    def pin(self, model: str) -> Optional[Dict[str, Any]]:
        return self.pins.get(model) or self.pins.get(base_model(model))

    def worst_case(self, model: str, body: Dict[str, Any]) -> Tuple[float, float]:
        """(prompt part, whole worst case): prompt characters / 3 x input price, plus
        `max_tokens` x output price."""
        p_in, p_out = self.price(model)
        chars = len(json.dumps(body.get("messages", []), ensure_ascii=False))
        max_tokens = as_int(body.get("max_tokens")) or as_int(body.get("max_completion_tokens")) or DEFAULT_MAX_TOKENS
        prompt = chars / 3.0 * p_in
        return prompt, prompt + max_tokens * p_out

    def estimate(self, model: str, body: Dict[str, Any]) -> float:
        """What a call reserves while in flight: the worst case until the ledger has
        MEASURED_MIN_CALLS successful calls of the model, then its measured
        high-percentile cost times a safety factor (at least this call's prompt part,
        never more than the worst case)."""
        return self.reservation(model, body)[0]

    def reservation(self, model: str, body: Dict[str, Any]) -> Tuple[float, str, int]:
        """(reservation, basis `worst_case` or `measured`, calls measured)."""
        prompt, worst = self.worst_case(model, body)
        measured = self.ledger.measured_reserve(model)
        if measured is None:
            return worst, "worst_case", 0
        reserve, n = measured
        if reserve >= worst:
            return worst, "worst_case", n
        return max(prompt, reserve), "measured", n

    def post(self, body: Dict[str, Any], out: "Outcome") -> Any:
        """Opens an upstream request; returns the response (any status).

        The connection (TCP + TLS) is set up first and `out.started` is reset
        once it is up: a real client reuses pooled connections, so a fresh
        handshake per call must not count toward TTFT or TTFR.
        """
        data = json.dumps(body).encode()
        headers = {
            "Content-Type": "application/json",
            "Authorization": "Bearer " + self.key,
            "HTTP-Referer": REFERER,
            "X-Title": TITLE,
            "Content-Length": str(len(data)),
        }
        if body.get("stream"):
            headers["Accept"] = "text/event-stream"
        cls = http.client.HTTPSConnection if self.up_https else http.client.HTTPConnection
        conn = cls(self.up_host, self.up_port, timeout=UPSTREAM_TIMEOUT_S)
        try:
            conn.connect()
            out.started = time.monotonic()
            conn.request("POST", self.up_path, body=data, headers=headers)
            resp = conn.getresponse()
            resp.bench_conn = conn  # closed with the response, so a cut stream drops the socket at once
            return resp
        except BaseException:
            conn.close()
            raise


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "mb-llm-bench-proxy"
    proxy: Proxy  # set on the class at start
    stream_started = False

    def log_message(self, fmt: str, *args: Any) -> None:  # quiet by default
        if os.environ.get("MB_BENCH_PROXY_VERBOSE"):
            sys.stderr.write("proxy: " + (fmt % args) + "\n")

    # ---- plumbing

    def send_json(self, status: int, obj: Any) -> None:
        body = json.dumps(obj).encode()
        self.send_bytes(status, body, "application/json")

    def send_bytes(self, status: int, body: bytes, ctype: str) -> None:
        try:
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            self.close_connection = True

    def read_body(self) -> bytes:
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n > 0 else b""

    def do_GET(self) -> None:
        path = urllib.parse.urlsplit(self.path).path
        if path == "/stats":
            self.send_json(200, self.proxy.ledger.stats())
        elif path == "/reserve":
            self.reserve_info()
        elif path in ("/healthz", "/readyz"):
            self.send_json(200, {"ok": True})
        else:
            self.send_json(404, {"error": {"message": f"no route for GET {path}"}})

    def reserve_info(self) -> None:
        """`GET /reserve?model=<id>&max_tokens=<n>`: what a call of that model with an
        empty prompt reserves now, with the budget, spend and other reservations, so a
        client that knows its concurrency can warn before it starts."""
        q = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
        model = (q.get("model") or [""])[0]
        if not model:
            self.send_json(400, {"error": {"message": "model is required"}})
            return
        body: Dict[str, Any] = {"messages": []}
        max_tokens = as_int((q.get("max_tokens") or [None])[0])
        if max_tokens:
            body["max_tokens"] = max_tokens
        reserve, basis, n = self.proxy.reservation(model, body)
        st = self.proxy.ledger.stats()
        self.send_json(200, {"model": model, "reserve_usd": reserve, "basis": basis, "measured_calls": n,
                             "measured_mean_usd": self.proxy.ledger.measured_mean(model),
                             "budget_usd": st["budget_usd"], "spent_usd": st["spent_usd"],
                             "reserved_usd": st["reserved_usd"], "in_flight": st["in_flight"],
                             "tripped": st["tripped"]})

    def do_POST(self) -> None:
        received = time.monotonic()
        path = urllib.parse.urlsplit(self.path).path
        raw = self.read_body()
        if path == "/bench/mark":
            self.mark(raw)
            return
        parts = path.split("/", 3)  # "", "r", label, rest
        if len(parts) < 4 or parts[1] != "r" or not parts[2]:
            self.send_json(404, {"error": {"message": f"use /r/<label>/api/v1/...; got {path}"}})
            return
        label = urllib.parse.unquote(parts[2])
        rest = "/" + parts[3]
        if rest.rstrip("/") != "/api/v1/chat/completions":
            self.send_json(404, {"error": {"message": f"only /api/v1/chat/completions is proxied; got {rest}"}})
            return
        try:
            body = json.loads(raw or b"{}")
            if not isinstance(body, dict):
                raise ValueError("body is not an object")
        except ValueError as e:
            self.send_json(400, {"error": {"message": f"bad JSON body: {e}"}})
            return
        if self.proxy.replayer:
            self.replay(label, body)
            return
        self.chat(label, body, received)

    # ---- record and replay (replay.py)

    def mark(self, raw: bytes) -> None:
        """`{label, journey, rep}`: the journey's calls from now on belong to that rep."""
        tags = self.proxy.replayer or self.proxy.recorder
        try:
            doc = json.loads(raw or b"{}")
            if tags:
                tags.mark(str(doc["label"]), str(doc["journey"]), int(doc["rep"]))
        except (ValueError, KeyError, TypeError) as e:
            self.send_json(400, {"error": {"message": f"bad mark: {e}"}})
            return
        self.send_json(200, {"ok": True, "recording": tags is not None})

    def replay(self, label: str, body: Dict[str, Any]) -> None:
        """Answers from the recording: no upstream call, no key."""
        ans = self.proxy.replayer.answer(label, self.headers, body)
        if ans["status"] != 200:
            self.send_json(ans["status"], {"error": {"message": ans["error"] or f"replayed {ans['status']}",
                                                     "code": ans["status"]}})
            return
        model = body.get("model")
        content = ans["content"]
        finish = ans["finish_reason"] or "stop"
        usage = {"prompt_tokens": 0, "completion_tokens": len(content) // 4, "total_tokens": len(content) // 4}
        if not body.get("stream"):
            self.send_json(200, {"id": "replay", "object": "chat.completion", "model": model, "provider": "replay",
                                 "choices": [{"index": 0, "message": {"role": "assistant", "content": content},
                                              "finish_reason": finish}], "usage": usage})
            return
        self.close_connection = True
        try:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()
            base = {"id": "replay", "object": "chat.completion.chunk", "model": model, "provider": "replay"}
            for i in range(0, len(content), 400):
                chunk = dict(base, choices=[{"index": 0, "delta": {"content": content[i:i + 400]},
                                             "finish_reason": None}])
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            end = dict(base, choices=[{"index": 0, "delta": {}, "finish_reason": finish}], usage=usage)
            self.wfile.write(f"data: {json.dumps(end)}\n\ndata: [DONE]\n\n".encode())
            self.wfile.flush()
        except OSError:
            pass

    # ---- the proxied call

    def chat(self, label: str, body: Dict[str, Any], received: float) -> None:
        px = self.proxy
        slug, _, workload = label.partition("~")
        model = str(body.get("model") or slug.replace("__", "/"))
        client_stream = bool(body.get("stream"))
        rf = body.get("response_format")
        rec: Dict[str, Any] = {
            "ts": utc_now(),
            "label": label,
            "model_slug": slug,
            "workload": workload or None,
            "model_requested": model,
            "model_served": None,
            "provider": None,
            "generation_id": None,
            "status": None,
            "error_kind": None,
            "error_excerpt": None,
            "stream_upgraded": False,
            "ttft_ms": None,
            "ttfr_ms": None,
            "total_ms": None,
            "prompt_tokens": None,
            "completion_tokens": None,
            "reasoning_tokens": None,
            "cached_tokens": None,
            "cost_usd": 0.0,
            "cost_source": "estimate",
            "reasoning_sent": False,
            "reasoning_rejected": False,
            "response_format_type": rf.get("type") if isinstance(rf, dict) else None,
            "response_format_rejected": False,
            "response_format_stripped": False,
            "finish_reason": None,
            "attempts": 0,
            "provider_pin": None,
            "provider_pin_by": None,
        }
        tag = px.recorder.tag(label, self.headers, body) if px.recorder else None
        self.last_outcome = None
        estimate = px.estimate(model, body)
        if not px.ledger.reserve(estimate):
            rec.update(status=402, error_kind="budget", error_excerpt=BUDGET_EXHAUSTED_MESSAGE, total_ms=0.0)
            px.ledger.settle(estimate, rec, reserved=False)
            self.send_json(402, budget_refusal())
            return
        try:
            self.forward(px, model, body, client_stream, rec, received)
        except Exception as e:  # never lose a call's cost to a proxy bug
            # The upstream may have generated all of `max_tokens`: charge the worst case,
            # not the reservation, which once measured is a percentile and can undercount.
            rec.update(status=502, error_kind="stream_error", cost_usd=px.worst_case(model, body)[1],
                       cost_source="estimate",
                       error_excerpt=excerpt(f"proxy error: {type(e).__name__}: {e}"))
            rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)
            self.send_json(502, {"error": {"message": rec["error_excerpt"]}})
        finally:
            px.ledger.settle(estimate, rec, reserved=True)
            if tag is not None:
                out = self.last_outcome
                px.recorder.write(tag, rec, redact("".join(out.content)) if out else "",
                                  out.finish_reason if out else None)

    def forward(
        self,
        px: Proxy,
        model: str,
        body: Dict[str, Any],
        client_stream: bool,
        rec: Dict[str, Any],
        received: float,
    ) -> None:
        self.stream_started = False  # set once a client stream's 200 head has gone out
        up = copy.deepcopy(body)
        if model.endswith(VARIANT_NOJSON):
            up["model"] = base_model(model)
            if up.pop("response_format", None) is not None:
                rec["response_format_stripped"] = True
        # The proxy's own pin is enforced; a client's `provider` is passed on as sent.
        routing = apply_provider_pin(up, px.pin(model))
        rec["provider_pin_by"] = routing
        sent = up.get("provider") if isinstance(up.get("provider"), dict) else {}
        if routing and isinstance(sent.get("order"), list):
            rec["provider_pin"] = list(sent["order"])
        pinned = sent if routing == "proxy" else None
        with px.memo_lock:
            no_reasoning = model in px.reasoning_rejected
            no_rf = model in px.response_format_rejected
        if "reasoning" not in up and not no_reasoning:
            up["reasoning"] = {"enabled": False}
            rec["reasoning_sent"] = True
        elif no_reasoning and "reasoning" not in body:
            rec["reasoning_rejected"] = True
        if no_rf and "response_format" in up:
            up.pop("response_format", None)
            rec["response_format_rejected"] = True
        usage_opt = up.get("usage") if isinstance(up.get("usage"), dict) else {}
        up["usage"] = dict(usage_opt, include=True)
        upgraded = (not client_stream) and px.stream_upgrade
        if upgraded or client_stream:
            up["stream"] = True
            so = up.get("stream_options") if isinstance(up.get("stream_options"), dict) else {}
            up["stream_options"] = dict(so, include_usage=True)
        rec["stream_upgraded"] = upgraded

        out = Outcome()
        resp = None
        for _ in range(3):
            rec["attempts"] += 1
            out = Outcome()
            self.last_outcome = out
            try:
                resp = px.post(up, out)
            except (socket.timeout, TimeoutError) as e:
                self.fail(rec, out, "timeout", 504, f"upstream timeout: {e}", received)
                return
            except (urllib.error.URLError, http.client.HTTPException, OSError) as e:
                reason = getattr(e, "reason", e)
                kind = "timeout" if isinstance(reason, (socket.timeout, TimeoutError)) else "http_5xx"
                self.fail(rec, out, kind, 504 if kind == "timeout" else 502, f"upstream unreachable: {reason}", received)
                return
            status = int(resp.status or 0)
            if status < 400:
                break
            try:
                err_body = resp.read()
            except (socket.timeout, TimeoutError, OSError):
                err_body = b""
            finally:
                resp.close()
            low = err_body.decode("utf-8", errors="replace").lower()
            if rec["reasoning_sent"] and status in REASONING_REJECT_STATUSES and "reasoning" in low:
                up.pop("reasoning", None)
                rec["reasoning_sent"] = False
                rec["reasoning_rejected"] = True
                with px.memo_lock:
                    px.reasoning_rejected.add(model)
                continue
            if (
                px.format_fallback
                and "response_format" in up
                and status in RESPONSE_FORMAT_REJECT_STATUSES
                and any(m in low for m in RESPONSE_FORMAT_MARKERS)
            ):
                up.pop("response_format", None)
                rec["response_format_rejected"] = True
                with px.memo_lock:
                    px.response_format_rejected.add(model)
                continue
            if (
                pinned
                and status in PIN_UNAVAILABLE_STATUSES
                and any(m in low for m in PIN_UNAVAILABLE_MARKERS)
            ):
                # No pinned provider can take the call. That is the provider's failure,
                # so the client gets a 503 (an infra fallback), not a request error.
                msg = excerpt(f"pinned provider {','.join(map(str, pinned['order']))} unavailable: "
                              + redact_bytes(err_body).decode("utf-8", "replace"))
                rec.update(status=503, error_kind="provider_unavailable", error_excerpt=msg)
                rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)
                self.send_json(503, {"error": {"message": msg, "code": 503}})
                return
            kind = "http_4xx" if status < 500 else "http_5xx"
            err_body = redact_bytes(err_body)
            rec.update(status=status, error_kind=kind, error_excerpt=excerpt(err_body.decode("utf-8", "replace")))
            rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)
            self.send_bytes(status, err_body or json.dumps({"error": {"message": f"upstream {status}"}}).encode(),
                            "application/json")
            return
        else:
            # Three rejections in a row: the request's failure, not the provider's, so a
            # 4xx (424) rather than a 502 that the bench would read as a provider error.
            rec.update(status=424, error_kind="http_4xx", error_excerpt="upstream kept rejecting the request")
            rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)
            self.send_json(424, {"error": {"message": "upstream kept rejecting the request"}})
            return

        assert resp is not None
        client_closed = False
        try:
            if client_stream:
                self.pass_through(resp, out, pinned)
            elif upgraded:
                self.collect_stream(resp, out, pinned)
            else:
                doc = json.loads(resp.read() or b"{}")
                out.ttft_ms = out.since()
                out.take_message(doc if isinstance(doc, dict) else {})
        except PinRefused:
            pass  # refused below, before anything reached the client
        except ClientClosed:
            # The client hung up mid-stream (Mockingbird aborts a looping answer). Stop
            # reading: closing the upstream connection below stops the generation, so
            # the provider bills no more tokens than it had sent.
            client_closed = True
        except (socket.timeout, TimeoutError) as e:
            out.error_kind = out.error_kind or "timeout"
            out.error_excerpt = out.error_excerpt or excerpt(f"upstream timeout mid-response: {e}")
        except (ValueError, OSError, http.client.HTTPException) as e:
            out.error_kind = out.error_kind or "stream_error"
            out.error_excerpt = out.error_excerpt or excerpt(f"bad upstream response: {e}")
        finally:
            resp.close()
            conn = getattr(resp, "bench_conn", None)
            if conn is not None:
                conn.close()
        if pinned and not out.error_kind and not pin_allows(pinned, out.provider):
            # allow_fallbacks false should make this impossible. The provider is checked
            # on the first chunk, before anything is relayed, so the answer is refused
            # there. Only a stream whose first chunk named no provider has already gone
            # out by the time a later chunk names one: that call is flagged, not refused.
            rec["provider_pin_mismatch"] = True
            if not self.stream_started:
                msg = f"served by {out.provider}, not the pinned {','.join(map(str, pinned['order']))}"
                out.error_kind = "provider_unavailable"
                out.error_excerpt = excerpt(msg)
                out.error_body = json.dumps({"error": {"message": msg, "code": 503}}).encode()
        p_in, _ = px.price(model)
        prompt_est = len(json.dumps(body.get("messages", []), ensure_ascii=False)) / 3.0 * p_in
        if client_closed:
            self.close_connection = True
            self.client_closed(px, model, rec, out, received, prompt_est)
        else:
            self.complete(px, model, rec, out, client_stream, received, prompt_est)
        px.dump(rec, body, out)

    @staticmethod
    def check_first_chunk(out: Outcome, pinned: Optional[Dict[str, Any]]) -> None:
        """Raises PinRefused when the first chunk names a provider the pin does not allow."""
        if pinned and not pin_allows(pinned, out.provider):
            raise PinRefused(out.provider)

    def collect_stream(self, resp: Any, out: Outcome, pinned: Optional[Dict[str, Any]] = None) -> None:
        first = True
        for data in sse_events(resp, deadline=out.started + UPSTREAM_TIMEOUT_S):
            if data.strip() == "[DONE]":
                break
            try:
                chunk = json.loads(data)
            except ValueError:
                continue
            if isinstance(chunk, dict):
                out.take_chunk(chunk)
                if first:
                    first = False
                    self.check_first_chunk(out, pinned)  # stop reading a refused answer

    def pass_through(self, resp: Any, out: Outcome, pinned: Optional[Dict[str, Any]] = None) -> None:
        """Relays the stream. Nothing (not even the 200 head) goes out before the
        first chunk has been checked against the pin; keep-alives before it wait."""
        held: List[bytes] = []
        client_gone = [False]
        done = [False]

        def write(raw: bytes) -> None:
            # This touches only the client socket, so any OSError (broken pipe, reset,
            # macOS EPROTOTYPE) means the client is gone. Relaying stops there; whether
            # reading stops too is decided once the event being relayed has been read.
            if client_gone[0]:
                return
            try:
                self.wfile.write(raw)
                if raw in (b"\n", b"\r\n"):
                    self.wfile.flush()
            except OSError:
                client_gone[0] = True

        def start() -> None:
            self.close_connection = True
            self.stream_started = True
            try:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache")
                self.send_header("Connection", "close")
                self.end_headers()
            except OSError:
                client_gone[0] = True
            for raw in held:
                write(raw)
            held.clear()

        def relay(raw: bytes) -> None:
            if self.stream_started:
                write(raw)
            else:
                held.append(raw)

        def check_gone() -> None:
            # A client gone mid-generation: stop reading (ClientClosed), so the provider
            # generates and bills no more. A client gone after the answer ended (a
            # finish_reason, an in-band error, [DONE]; Mockingbird drops a stream right
            # after a `length` chunk) costs nothing more: keep reading, unrelayed, so the
            # usage that comes last still arrives, and record the call as usual.
            if client_gone[0] and not (done[0] or out.finish_reason or out.error_kind):
                raise ClientClosed("client closed the stream mid-generation")

        for data in sse_events(resp, on_line=relay, deadline=out.started + UPSTREAM_TIMEOUT_S):
            if data.strip() == "[DONE]":
                done[0] = True
                if not self.stream_started:
                    start()
                continue
            try:
                chunk = json.loads(data)
            except ValueError:
                check_gone()
                continue
            if isinstance(chunk, dict):
                out.take_chunk(chunk)
                if not self.stream_started:
                    self.check_first_chunk(out, pinned)
                    start()
            check_gone()
        if not self.stream_started:
            start()
        if not client_gone[0]:
            try:
                self.wfile.flush()
            except OSError:
                client_gone[0] = True
        out.client_gone_after_end = client_gone[0]

    def fail(self, rec: Dict[str, Any], out: Outcome, kind: str, status: int, msg: str, received: float) -> None:
        rec.update(status=status, error_kind=kind, error_excerpt=excerpt(msg))
        rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)
        self.send_json(status, {"error": {"message": msg}})

    def complete(
        self,
        px: Proxy,
        model: str,
        rec: Dict[str, Any],
        out: Outcome,
        client_stream: bool,
        received: float,
        prompt_est: float,
    ) -> None:
        usage = self.fill_record(px, model, rec, out, prompt_est)
        err_status = 503 if out.error_kind == "provider_unavailable" else 502
        if out.error_kind:
            rec.update(status=err_status, error_kind=out.error_kind, error_excerpt=out.error_excerpt)
        elif out.finish_reason == "content_filter" or out.refusal:
            rec.update(status=200, error_kind="refusal", error_excerpt=excerpt(out.refusal or "content_filter"))
        elif not out.text().strip() and not out.reasoning_text().strip():
            rec.update(status=200, error_kind="empty")
        else:
            rec["status"] = 200
        rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)
        if out.client_gone_after_end:
            rec["client_gone_after_end"] = True  # recorded as usual: the answer had ended
        if client_stream and self.stream_started:
            if out.error_kind:
                rec["status"] = 200  # the 200 head is already sent; the error went through in-stream
            return
        # A client stream that failed before its first chunk (refused, timed out) gets
        # the error as a plain answer, as a non-streamed call would.
        if out.error_kind:
            self.send_bytes(err_status, redact_bytes(out.error_body or b"") or json.dumps({"error": {"message": out.error_excerpt}}).encode(),
                            "application/json")
            return
        message: Dict[str, Any] = {"role": "assistant", "content": out.text()}
        if out.reasoning:
            message["reasoning"] = out.reasoning_text()
        if out.refusal:
            message["refusal"] = out.refusal
        doc = {
            "id": out.gen_id or "gen-bench",
            "object": "chat.completion",
            "created": out.created or int(time.time()),
            "model": out.model or model,
            "provider": out.provider,
            "choices": [{"index": 0, "message": message, "finish_reason": out.finish_reason}],
            "usage": usage or None,
        }
        self.send_json(200, doc)

    def client_closed(
        self,
        px: Proxy,
        model: str,
        rec: Dict[str, Any],
        out: Outcome,
        received: float,
        prompt_est: float,
    ) -> None:
        """Records a stream the client hung up on mid-generation: what was received so
        far (generation_id included), charged as usage when the provider had sent it,
        else as the prompt plus the output seen. That cost is a lower bound: a provider
        that does not support cancellation keeps generating, and billing. Status 499
        (client closed request), `error_kind: client_closed`: neither a success nor an
        error. Nothing is sent: the client is gone."""
        self.fill_record(px, model, rec, out, prompt_est)
        rec.update(status=499, error_kind=CLIENT_CLOSED, client_closed=True,
                   error_excerpt="client closed the stream; upstream read stopped",
                   received_chars=len(out.text()) + len(out.reasoning_text()))
        rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)

    def fill_record(
        self,
        px: Proxy,
        model: str,
        rec: Dict[str, Any],
        out: Outcome,
        prompt_est: float,
    ) -> Dict[str, Any]:
        """Fills `rec` with the answer's provider, timings, tokens and cost; returns its usage."""
        usage = out.usage or {}
        pt = as_int(usage.get("prompt_tokens"))
        ct = as_int(usage.get("completion_tokens"))
        details = usage.get("completion_tokens_details") or {}
        pdetails = usage.get("prompt_tokens_details") or {}
        cost = as_float(usage.get("cost"))
        if cost is not None:
            rec["cost_usd"], rec["cost_source"] = cost, "usage"
        else:
            p_in, p_out = px.price(model)
            if pt is not None or ct is not None:
                rec["cost_usd"] = (pt or 0) * p_in + (ct or 0) * p_out
            else:
                # No usage (stream cut, timeout): charge what was probably
                # used, the prompt plus the output seen so far.
                seen = len(out.text()) + len(out.reasoning_text())
                rec["cost_usd"] = prompt_est + seen / 3.0 * p_out
            rec["cost_source"] = "estimate"
        rec.update(
            model_served=out.model,
            provider=out.provider,
            generation_id=out.gen_id,
            ttft_ms=out.ttft_ms,
            ttfr_ms=out.ttfr_ms,
            prompt_tokens=pt,
            completion_tokens=ct,
            reasoning_tokens=as_int(details.get("reasoning_tokens")),
            cached_tokens=as_int(pdetails.get("cached_tokens")),
            finish_reason=out.finish_reason,
        )
        return usage


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--ledger", help="calls.jsonl to append to (required unless --replay)")
    ap.add_argument("--budget-usd", type=float, default=9.0)
    ap.add_argument("--prices", help="roster.json (per-token prices)")
    ap.add_argument("--provider-pins", default=DEFAULT_PROVIDER_PINS,
                    help="providers.json: per-model OpenRouter provider pins (default: the bench's; '' for none)")
    ap.add_argument(
        "--prior-ledger",
        action="append",
        help="another ledger whose cost counts toward the budget (repeatable; read only)",
    )
    ap.add_argument("--no-stream-upgrade", action="store_true")
    ap.add_argument("--no-format-fallback", action="store_true",
                    help="pass a rejected response_format's error to the client instead of retrying without it")
    ap.add_argument("--dump", help="exchanges.jsonl: append each call's messages and answer")
    ap.add_argument("--record", help="raw recording (replay.py): append each call's journey tag and answer")
    ap.add_argument("--replay", action="append",
                    help="answer every call from these recordings (files or directories; repeatable): "
                         "no upstream, no key, no network")
    ap.add_argument("--replay-log", help="with --replay: append each call's match (exact/key/reused/nearest/none) and the recorded call it used")
    ap.add_argument("--upstream", default=OPENROUTER_URL, help=argparse.SUPPRESS)
    args = ap.parse_args()
    if not args.ledger and not args.replay:
        ap.error("--ledger is required unless --replay")

    if not args.replay and args.upstream == OPENROUTER_URL and not os.environ.get("OPENROUTER_API_KEY"):
        print("proxy: OPENROUTER_API_KEY is not set (source ~/.zshenv)", file=sys.stderr)
        return 2
    proxy = Proxy(args)
    Handler.proxy = proxy
    ThreadingHTTPServer.request_queue_size = 128  # bursts of parallel clients must not wait on SYN retries
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    server.daemon_threads = True

    def on_term(*_: Any) -> None:
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, on_term)
    st = proxy.ledger.stats()
    print(
        f"proxy: listening on http://127.0.0.1:{server.server_address[1]} "
        f"(spent ${st['spent_usd']:.4f} of ${args.budget_usd:.2f}, tripped={st['tripped']}, "
        f"{len(proxy.prices)} priced models, {len(proxy.pins)} provider pins)",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.serve_forever(poll_interval=0.2)
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        # Let in-flight calls finish and reach the ledger: a call dropped
        # here would be spend the next start does not count.
        deadline = time.monotonic() + UPSTREAM_TIMEOUT_S + 10
        while proxy.ledger.stats()["in_flight"] > 0 and time.monotonic() < deadline:
            try:
                time.sleep(0.2)
            except KeyboardInterrupt:
                break
        left = proxy.ledger.stats()["in_flight"]
        if left:
            print(f"proxy: exiting with {left} call(s) still in flight (not in the ledger)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
