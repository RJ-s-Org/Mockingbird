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

`GET /stats` reports spend; `GET /healthz` answers 200. Standard library only.
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

OPENROUTER_URL = "https://openrouter.ai/api/v1/chat/completions"
UPSTREAM_TIMEOUT_S = 120.0
REFERER = "https://github.com/rj-s-org/mockingbird"
TITLE = "Mockingbird bench"
DEFAULT_MAX_TOKENS = 4096
# Per-token prices used for a model the roster does not price: deliberately
# high, so an unknown model reserves too much rather than too little.
FALLBACK_PRICES = (1.0e-5, 4.0e-5)
EXCERPT_CHARS = 300
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
            self._tally(rec)
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

    def _tally(self, rec: Dict[str, Any]) -> None:
        cost = as_float(rec.get("cost_usd")) or 0.0
        self.spent += cost
        self.calls += 1
        label = rec.get("label") or "?"
        s = self.per_label.setdefault(
            label, {"calls": 0, "cost_usd": 0.0, "errors": 0, "prompt_tokens": 0, "completion_tokens": 0}
        )
        s["calls"] += 1
        s["cost_usd"] += cost
        if rec.get("error_kind"):
            s["errors"] += 1
        s["prompt_tokens"] += as_int(rec.get("prompt_tokens")) or 0
        s["completion_tokens"] += as_int(rec.get("completion_tokens")) or 0

    def total_spent(self) -> float:
        return self.spent + self.prior_spent

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
                    k: dict(v, cost_usd=round(v["cost_usd"], 6)) for k, v in sorted(self.per_label.items())
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
        self.ledger = Ledger(args.ledger, args.budget_usd, args.prior_ledger or [])
        self.prices = load_prices(args.prices)
        if self.prices:
            self.fallback = (
                max(p for p, _ in self.prices.values()),
                max(c for _, c in self.prices.values()),
            )
            self.fallback = (max(self.fallback[0], FALLBACK_PRICES[0]), max(self.fallback[1], FALLBACK_PRICES[1]))
        else:
            self.fallback = FALLBACK_PRICES
        self.key = os.environ.get("OPENROUTER_API_KEY", "")
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

    def price(self, model: str) -> Tuple[float, float]:
        return self.prices.get(base_model(model), self.fallback)

    def estimate(self, model: str, body: Dict[str, Any]) -> float:
        p_in, p_out = self.price(model)
        chars = len(json.dumps(body.get("messages", []), ensure_ascii=False))
        max_tokens = as_int(body.get("max_tokens")) or as_int(body.get("max_completion_tokens")) or DEFAULT_MAX_TOKENS
        return chars / 3.0 * p_in + max_tokens * p_out

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
            return conn.getresponse()
        except BaseException:
            conn.close()
            raise


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "mb-llm-bench-proxy"
    proxy: Proxy  # set on the class at start

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
        elif path in ("/healthz", "/readyz"):
            self.send_json(200, {"ok": True})
        else:
            self.send_json(404, {"error": {"message": f"no route for GET {path}"}})

    def do_POST(self) -> None:
        received = time.monotonic()
        path = urllib.parse.urlsplit(self.path).path
        raw = self.read_body()
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
        self.chat(label, body, received)

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
        }
        estimate = px.estimate(model, body)
        if not px.ledger.reserve(estimate):
            rec.update(status=402, error_kind="budget", error_excerpt="bench budget exhausted", total_ms=0.0)
            px.ledger.settle(estimate, rec, reserved=False)
            self.send_json(402, {"error": {"message": "bench budget exhausted", "code": 402}})
            return
        try:
            self.forward(px, model, body, client_stream, rec, received)
        except Exception as e:  # never lose a call's cost to a proxy bug
            # The upstream may have generated: charge the full reservation.
            rec.update(status=502, error_kind="stream_error", cost_usd=estimate, cost_source="estimate",
                       error_excerpt=excerpt(f"proxy error: {type(e).__name__}: {e}"))
            rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)
            self.send_json(502, {"error": {"message": rec["error_excerpt"]}})
        finally:
            px.ledger.settle(estimate, rec, reserved=True)

    def forward(
        self,
        px: Proxy,
        model: str,
        body: Dict[str, Any],
        client_stream: bool,
        rec: Dict[str, Any],
        received: float,
    ) -> None:
        up = copy.deepcopy(body)
        if model.endswith(VARIANT_NOJSON):
            up["model"] = base_model(model)
            if up.pop("response_format", None) is not None:
                rec["response_format_stripped"] = True
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
                "response_format" in up
                and status in RESPONSE_FORMAT_REJECT_STATUSES
                and any(m in low for m in RESPONSE_FORMAT_MARKERS)
            ):
                up.pop("response_format", None)
                rec["response_format_rejected"] = True
                with px.memo_lock:
                    px.response_format_rejected.add(model)
                continue
            kind = "http_4xx" if status < 500 else "http_5xx"
            err_body = redact_bytes(err_body)
            rec.update(status=status, error_kind=kind, error_excerpt=excerpt(err_body.decode("utf-8", "replace")))
            rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)
            self.send_bytes(status, err_body or json.dumps({"error": {"message": f"upstream {status}"}}).encode(),
                            "application/json")
            return
        else:
            # Three rejections in a row: answer with the last one.
            rec.update(status=502, error_kind="http_4xx", error_excerpt="upstream kept rejecting the request")
            rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)
            self.send_json(502, {"error": {"message": "upstream kept rejecting the request"}})
            return

        assert resp is not None
        try:
            if client_stream:
                self.pass_through(resp, out)
            elif upgraded:
                self.collect_stream(resp, out)
            else:
                doc = json.loads(resp.read() or b"{}")
                out.ttft_ms = out.since()
                out.take_message(doc if isinstance(doc, dict) else {})
        except (socket.timeout, TimeoutError) as e:
            out.error_kind = out.error_kind or "timeout"
            out.error_excerpt = out.error_excerpt or excerpt(f"upstream timeout mid-response: {e}")
        except (ValueError, OSError, http.client.HTTPException) as e:
            out.error_kind = out.error_kind or "stream_error"
            out.error_excerpt = out.error_excerpt or excerpt(f"bad upstream response: {e}")
        finally:
            resp.close()
        p_in, _ = px.price(model)
        prompt_est = len(json.dumps(body.get("messages", []), ensure_ascii=False)) / 3.0 * p_in
        self.complete(px, model, rec, out, client_stream, received, prompt_est)
        px.dump(rec, body, out)

    def collect_stream(self, resp: Any, out: Outcome) -> None:
        for data in sse_events(resp, deadline=out.started + UPSTREAM_TIMEOUT_S):
            if data.strip() == "[DONE]":
                break
            try:
                chunk = json.loads(data)
            except ValueError:
                continue
            if isinstance(chunk, dict):
                out.take_chunk(chunk)

    def pass_through(self, resp: Any, out: Outcome) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        client_gone = [False]

        def relay(raw: bytes) -> None:
            if client_gone[0]:
                return
            try:
                self.wfile.write(raw)
                if raw in (b"\n", b"\r\n"):
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                client_gone[0] = True

        for data in sse_events(resp, on_line=relay, deadline=out.started + UPSTREAM_TIMEOUT_S):
            if data.strip() == "[DONE]":
                continue
            try:
                chunk = json.loads(data)
            except ValueError:
                continue
            if isinstance(chunk, dict):
                out.take_chunk(chunk)
        try:
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

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
        if out.error_kind:
            rec.update(status=502, error_kind=out.error_kind, error_excerpt=out.error_excerpt)
        elif out.finish_reason == "content_filter" or out.refusal:
            rec.update(status=200, error_kind="refusal", error_excerpt=excerpt(out.refusal or "content_filter"))
        elif not out.text().strip() and not out.reasoning_text().strip():
            rec.update(status=200, error_kind="empty")
        else:
            rec["status"] = 200
        rec["total_ms"] = round((time.monotonic() - received) * 1000.0, 1)
        if client_stream:
            if out.error_kind:
                rec["status"] = 200  # the 200 head is already sent; the error went through in-stream
            return
        if out.error_kind:
            self.send_bytes(502, redact_bytes(out.error_body or b"") or json.dumps({"error": {"message": out.error_excerpt}}).encode(),
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


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--ledger", required=True, help="calls.jsonl to append to")
    ap.add_argument("--budget-usd", type=float, default=9.0)
    ap.add_argument("--prices", help="roster.json (per-token prices)")
    ap.add_argument(
        "--prior-ledger",
        action="append",
        help="another ledger whose cost counts toward the budget (repeatable; read only)",
    )
    ap.add_argument("--no-stream-upgrade", action="store_true")
    ap.add_argument("--dump", help="exchanges.jsonl: append each call's messages and answer")
    ap.add_argument("--upstream", default=OPENROUTER_URL, help=argparse.SUPPRESS)
    args = ap.parse_args()

    if args.upstream == OPENROUTER_URL and not os.environ.get("OPENROUTER_API_KEY"):
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
        f"{len(proxy.prices)} priced models)",
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
