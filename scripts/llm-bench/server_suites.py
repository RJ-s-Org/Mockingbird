#!/usr/bin/env python3
"""`mockingbird serve` workloads of the LLM bench: consistency, SSE and smart import.

    python3 scripts/llm-bench/server_suites.py --model deepseek/deepseek-v4.1-flash \\
        --proxy-base http://127.0.0.1:18080 --out docs/benchmarks/2026-10-29 --repeat 3 \\
        [--suite consistency --suite sse --suite import] [--binary target/release/mockingbird]

For each suite it starts `mockingbird serve` with a generated config (fresh
SQLite in a temp directory, free ports, waits for `/readyz` on the admin
port) whose LLM profile points at the bench proxy:
`<proxy-base>/r/<slug>~<suite>/api/v1`, slug = model id with `/` -> `__`.
The proxy injects the real OpenRouter key, so the server itself only gets a
placeholder `OPENROUTER_API_KEY`: the real key never reaches it.

Every check appends one JSON line to `<out>/server.jsonl`:
`{model, suite, case, rep, ok, latency_ms, ttfe_ms?, source, detail}`.
Server logs go to `<out>/server-logs/<slug>~<suite>.log`.

Suites (contracts are in-repo, so the bench is reproducible):
- consistency: per rep a fresh `x-mockingbird-session`; Stripe create/read of a
  PaymentIntent, TMDB popular list then detail of its first id, Square create/read
  of a payment. Field-level agreement in `detail`.
- journeys: multi-step journeys over three contracts, one fresh session each per
  rep: Stripe (customer, payment intent, confirm, refund), travel (search, offer,
  booking, cancel) and GitHub (search, repo, issue). One row per check, `case` =
  `<journey>.<check>`; `source` is the `x-mockingbird-source` of the call the check
  judged. A journey that breaks fails its remaining checks. Not in the default
  suites (it is asked for with `--suite journeys`). Measures remembering entities
  and the effects of writes.
- journeys_heldout: the same kind of journey over five contracts the engine was not
  fixed against: PayPal orders, Jira issues, Zendesk tickets, Notion pages and Square
  payments. Not in the default suites. Measures how `journeys` generalises, so its
  checks are written from each contract and are not edited to suit results.
- sse: `fixtures/openapi/events-3.1.yaml` `GET /ticks`: time to first event,
  events received, each event parsed and checked against the Tick schema.
- import: smart import through the Admin API of every input in
  `docs/benchmarks/import-inputs/` plus `fixtures/smart/petstore.md` and
  `fixtures/smart/tmdb-readme.html`; recall/precision of `METHOD /path` against
  the expected lists. Imports run `--import-repeat` times (default 1:
  extraction is at temperature 0, so repeats mostly add spend).

Standard library only. Makes no LLM calls itself.
"""

import argparse
import base64
import fcntl
import glob
import http.client
import json
import os
import random
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from datetime import datetime

REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
SUITES = ("consistency", "sse", "import", "journeys", "journeys_heldout")
# What a run without --suite does (and what run.py gets). The journey suites
# cost real calls on top of these, so they are asked for by name.
DEFAULT_SUITES = ("consistency", "sse", "import")
PLACEHOLDER_KEY = "bench-proxy-injects-the-real-key"
IMPORT_DIR = "docs/benchmarks/import-inputs"
FIXTURES_SMART_EXPECTED = os.path.join(IMPORT_DIR, "fixtures-smart.expected.json")
FIXTURE_INPUTS = ("fixtures/smart/petstore.md", "fixtures/smart/tmdb-readme.html")
INPUT_KINDS = {".md": "markdown", ".markdown": "markdown", ".html": "html", ".htm": "html",
               ".txt": "text", ".pdf": "pdf", ".har": "har"}

STRIPE_PERSONA = (
    "Stripe API in test mode. Return complete Stripe objects with all top-level fields. Ids use "
    "Stripe prefixes (pi_, cus_, pm_) followed by 24 random alphanumeric chars, unique per object. "
    'A PaymentIntent always has client_secret = "<id>_secret_<24 random chars>", livemode false, '
    "created = a recent unix timestamp, and copies amount (integer), currency and "
    "payment_method_types (from payment_method_types[N] form keys), customer and confirm from the "
    "request body."
)
TMDB_PERSONA = (
    "The Movie Database (TMDB) v3 API. Use real, well-known movies, TV shows and people with "
    "their actual titles, names, years, overviews and plausible TMDB ids. Image paths look like "
    "TMDB's: '/' + 27 alphanumeric chars + '.jpg'. Lists return 10 results per page. Video keys "
    "are 11-char YouTube ids. Dates are YYYY-MM-DD. vote_average is 0-10."
)
SQUARE_PERSONA = (
    "Square sandbox account of a two-location bakery in Oakland, CA. Payment and order ids are 29 "
    "base62 characters, customer ids 26 uppercase alphanumerics. Card payments use sandbox cards "
    "(VISA 1111), entry_method KEYED. amount_money is copied from the request (cents, USD), "
    "total_money adds tip_money; a payment with autocomplete=false is APPROVED, else COMPLETED "
    "with card status CAPTURED. Timestamps are recent RFC 3339."
)

CONSISTENCY_MOCKS = {
    "stripe": ("examples/apis/stripe/stripe.openapi.yaml", "/stripe", STRIPE_PERSONA),
    "tmdb": ("examples/apis/tmdb/tmdb.openapi.yaml", "/tmdb", TMDB_PERSONA),
    "square": ("examples/apis/square/square.openapi.yaml", "/square", SQUARE_PERSONA),
}
# Journeys use the contracts alone: no persona.
JOURNEY_MOCKS = {
    "stripe": ("examples/apis/stripe/stripe.openapi.yaml", "/stripe", None),
    "travel": ("docs/benchmarks/contracts/travel.openapi.yaml", "/travel", None),
    "github": ("examples/apis/github-rest/github-rest.openapi.yaml", "/github", None),
    "invoices": ("docs/benchmarks/contracts/invoicing.openapi.yaml", "/invoices", None),
}
# The held-out journeys, contracts alone as well: no persona.
HELDOUT_MOCKS = {
    "paypal": ("examples/apis/paypal/paypal-orders.openapi.yaml", "/paypal", None),
    "jira": ("examples/apis/jira/jira.openapi.yaml", "/jira", None),
    "zendesk": ("examples/apis/zendesk/zendesk.openapi.yaml", "/zendesk", None),
    "notion": ("examples/apis/notion/notion.openapi.yaml", "/notion", None),
    "square": ("examples/apis/square/square.openapi.yaml", "/square", None),
}
SSE_MOCKS = {
    "events": ("fixtures/openapi/events-3.1.yaml", "/events",
               "A market data feed: ticks of one instrument, seq increasing from 1, prices near 101.25."),
}

SECRET = re.compile(r"^pi_[A-Za-z0-9]+_secret_[A-Za-z0-9]+$")
# The Tick schema of fixtures/openapi/events-3.1.yaml (no YAML parser in the stdlib).
TICK_SCHEMA = {
    "type": "object",
    "required": ["id", "seq", "price"],
    "properties": {
        "id": {"type": "string"},
        "seq": {"type": "integer", "minimum": 0},
        "price": {"type": "number"},
    },
}
TMDB_FIELDS = ("id", "title", "original_title", "original_language", "release_date", "overview",
               "poster_path", "backdrop_path", "adult", "video")
SQUARE_FIELDS = ("id", "amount_money", "status", "source_type", "location_id", "created_at",
                 "order_id", "total_money")


# ---------------------------------------------------------------- plumbing

def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def slug_of(model):
    return model.replace("/", "__")


def session_slug(model):
    """`slug_of(model)` with every character a header value may not carry
    unquoted (such as the `@` of a variant like `x/y@nojson`) as `-`."""
    return re.sub(r"[^A-Za-z0-9_.-]", "-", slug_of(model))


class Sink:
    """Appends rows to <out>/server.jsonl; safe across concurrent processes."""

    def __init__(self, out, model, suite):
        self.path = os.path.join(out, "server.jsonl")
        self.model, self.suite = model, suite
        self.rows = []

    def add(self, case, rep, ok, latency_ms, source, detail, ttfe_ms=None):
        row = {"model": self.model, "suite": self.suite, "case": case, "rep": rep, "ok": bool(ok),
               "latency_ms": None if latency_ms is None else round(latency_ms, 1)}
        if ttfe_ms is not None or self.suite == "sse":
            row["ttfe_ms"] = None if ttfe_ms is None else round(ttfe_ms, 1)
        row["source"] = source
        row["detail"] = detail
        line = (json.dumps(row, ensure_ascii=False) + "\n").encode()
        with open(self.path, "ab") as f:
            fcntl.flock(f, fcntl.LOCK_EX)
            try:
                f.write(line)
                f.flush()
            finally:
                fcntl.flock(f, fcntl.LOCK_UN)
        self.rows.append(row)
        mark = "ok " if ok else "BAD"
        lat = "-" if latency_ms is None else f"{latency_ms / 1000:.1f}s"
        print(f"  [{mark}] {self.suite}/{case} #{rep}: {lat} {source}", file=sys.stderr)


def write_config(directory, model, base_url, ports, mocks, suite):
    lines = [
        "llm:\n  default_profile: bench\n  profiles:\n    bench:\n",
        "      api_key: env:OPENROUTER_API_KEY\n",
        f"      base_url: {json.dumps(base_url)}\n",
        f"      model: {json.dumps(model)}\n",
        f"storage:\n  url: sqlite://{directory}/mockingbird.db\n",
        f"server:\n  port: {ports[0]}\n  admin_port: {ports[1]}\n  grpc_port: {ports[2]}\n",
    ]
    if suite == "import":
        # One job at a time per mock is what we measure; the default budget stays.
        lines.append("import:\n  workers: 2\n")
    if mocks:
        lines.append("mocks:\n")
        for name, (contract, prefix, persona) in mocks.items():
            lines.append(f"  {name}:\n")
            lines.append(f"    contract: {json.dumps(os.path.join(REPO, contract))}\n")
            lines.append(f"    route_prefix: {prefix}\n")
            if persona:
                lines.append(f"    persona: {json.dumps(persona)}\n")
    path = os.path.join(directory, "mockingbird.yaml")
    with open(path, "w") as f:
        f.write("".join(lines))
    return path


def wait_ready(admin, proc, timeout):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"server exited with {proc.returncode}")
        try:
            with urllib.request.urlopen(f"{admin}/readyz", timeout=2) as r:
                if r.status == 200:
                    return
        except urllib.error.HTTPError as e:
            last = e.read()[:300].decode(errors="replace")
        except (urllib.error.URLError, OSError):
            pass
        time.sleep(0.3)
    raise RuntimeError(f"server not ready: {last}")


def request(url, method="GET", headers=None, json_body=None, form=None, data=None, timeout=180):
    """(elapsed_ms, status, headers, parsed body or None, raw bytes)."""
    headers = dict(headers or {})
    if json_body is not None:
        data = json.dumps(json_body).encode()
        headers["content-type"] = "application/json"
    elif form is not None:
        data = urllib.parse.urlencode(form).encode()
        headers["content-type"] = "application/x-www-form-urlencoded"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    started = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            status, raw, hdrs = r.status, r.read(), r.headers
    except urllib.error.HTTPError as e:
        status, raw, hdrs = e.code, e.read(), e.headers
    except (urllib.error.URLError, OSError) as e:
        return (time.monotonic() - started) * 1000, 0, {}, None, str(e).encode()
    elapsed = (time.monotonic() - started) * 1000
    try:
        body = json.loads(raw) if raw else None
    except ValueError:
        body = None
    return elapsed, status, hdrs, body, raw


def source_of(hdrs):
    return hdrs.get("x-mockingbird-source", "?") if hdrs else "error"


def fallback_reason(hdrs):
    return hdrs.get("x-mockingbird-fallback-reason") if hdrs else None


def excerpt(raw, n=200):
    if isinstance(raw, bytes):
        raw = raw.decode(errors="replace")
    return raw[:n]


def agreement(a, b, fields):
    """(agreeing, checked, mismatched names) over fields present in both dicts."""
    a = a if isinstance(a, dict) else {}
    b = b if isinstance(b, dict) else {}
    agree, checked, bad = 0, 0, []
    for f in fields:
        if f in a and f in b:
            checked += 1
            if a[f] == b[f]:
                agree += 1
            else:
                bad.append(f)
    return agree, checked, bad


class Server:
    def __init__(self, args, suite, mocks):
        self.args, self.suite = args, suite
        self.dir = tempfile.mkdtemp(prefix=f"mb-bench-{suite}-")
        self.ports = (free_port(), free_port(), free_port())
        self.base_url = f"{args.proxy_base.rstrip('/')}/r/{slug_of(args.model)}~{suite}/api/v1"
        self.config = write_config(self.dir, args.model, self.base_url, self.ports, mocks, suite)
        self.data = f"http://127.0.0.1:{self.ports[0]}"
        self.admin = f"http://127.0.0.1:{self.ports[1]}"
        self.api = f"{self.admin}/api/v1/workspaces/default"
        logs = os.path.join(args.out, "server-logs")
        os.makedirs(logs, exist_ok=True)
        self.log_path = os.path.join(logs, f"{slug_of(args.model)}~{suite}.log")
        self.proc = None

    def __enter__(self):
        # No real key and no MB_* overrides of the generated config: MB_LLM_*
        # adds a `default` profile at openrouter.ai (a way around the proxy),
        # MB_STORAGE_URL / MB_*_PORT would break the fresh SQLite and ports.
        # Also drop any variable that holds the key under another name.
        key = os.environ.get("OPENROUTER_API_KEY")
        env = {k: v for k, v in os.environ.items()
               if not (k.startswith("MB_") or (key and v == key))}
        env["OPENROUTER_API_KEY"] = PLACEHOLDER_KEY
        env.setdefault("RUST_LOG", "info")
        self.log = open(self.log_path, "w")
        self.proc = subprocess.Popen([self.args.binary, "serve", "-c", self.config], cwd=self.dir,
                                     stdout=self.log, stderr=self.log, env=env)
        try:
            wait_ready(self.admin, self.proc, self.args.ready_timeout)
        except Exception:
            self.__exit__(None, None, None)
            raise
        return self

    def __exit__(self, *exc):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.log.close()
        if self.args.keep:
            print(f"kept {self.dir}", file=sys.stderr)
        else:
            shutil.rmtree(self.dir, ignore_errors=True)


# ---------------------------------------------------------------- consistency

def run_consistency(srv, sink, args):
    stamp = int(time.time())
    for rep in range(args.repeat):
        session = f"bench-{session_slug(args.model)}-{stamp}-{rep}"
        h = {"x-mockingbird-session": session}
        consistency_stripe(srv, sink, args, rep, h)
        consistency_tmdb(srv, sink, args, rep, h)
        consistency_square(srv, sink, args, rep, h)


def consistency_stripe(srv, sink, args, rep, h):
    amount = 2000 + 7 * rep
    form = {"amount": str(amount), "currency": "usd", "payment_method_types[0]": "card"}
    ms, status, hdrs, body, raw = request(f"{srv.data}/stripe/v1/payment_intents", "POST", h, form=form,
                                          timeout=args.timeout)
    b = body if isinstance(body, dict) else {}
    secret = b.get("client_secret")
    checks = {
        "status_200": status == 200,
        "amount_copied": b.get("amount") == amount,
        "currency_copied": b.get("currency") == "usd",
        "client_secret_format": bool(isinstance(secret, str) and SECRET.match(secret)),
        "client_secret_prefix": bool(isinstance(secret, str) and b.get("id")
                                     and secret.startswith(f"{b.get('id')}_secret_")),
    }
    detail = {"status": status, "checks": checks, "id": b.get("id"),
              "fallback_reason": fallback_reason(hdrs)}
    if status != 200:
        detail["error"] = excerpt(raw)
    sink.add("stripe.create", rep, all(checks.values()), ms, source_of(hdrs), detail)
    if status != 200 or not b.get("id"):
        sink.add("stripe.read", rep, False, None, "skipped", {"reason": "create failed"})
        return
    pid = urllib.parse.quote(str(b["id"]), safe="")
    ms2, status2, hdrs2, body2, raw2 = request(f"{srv.data}/stripe/v1/payment_intents/{pid}", "GET", h,
                                               timeout=args.timeout)
    fields = ("id", "amount", "currency", "client_secret", "created", "status", "payment_method_types",
              "livemode", "object")
    agree, checked, bad = agreement(b, body2, fields)
    core = all(isinstance(body2, dict) and body2.get(k) == b.get(k) for k in ("id", "amount", "currency"))
    detail2 = {"status": status2, "fields_agree": agree, "fields_checked": checked, "mismatched": bad,
               "core_equal": core, "fallback_reason": fallback_reason(hdrs2)}
    if status2 != 200:
        detail2["error"] = excerpt(raw2)
    sink.add("stripe.read", rep, status2 == 200 and core, ms2, source_of(hdrs2), detail2)


def consistency_tmdb(srv, sink, args, rep, h):
    ms, status, hdrs, body, raw = request(f"{srv.data}/tmdb/3/movie/popular?page={1 + rep}", "GET", h,
                                          timeout=args.timeout)
    results = body.get("results") if isinstance(body, dict) else None
    first = results[0] if isinstance(results, list) and results and isinstance(results[0], dict) else None
    ok = status == 200 and first is not None and isinstance(first.get("id"), int)
    detail = {"status": status, "results": len(results) if isinstance(results, list) else None,
              "first_id": first.get("id") if first else None, "first_title": first.get("title") if first else None,
              "fallback_reason": fallback_reason(hdrs)}
    if status != 200:
        detail["error"] = excerpt(raw)
    sink.add("tmdb.list", rep, ok, ms, source_of(hdrs), detail)
    if not ok:
        sink.add("tmdb.detail", rep, False, None, "skipped", {"reason": "list failed"})
        return
    ms2, status2, hdrs2, body2, raw2 = request(f"{srv.data}/tmdb/3/movie/{first['id']}", "GET", h,
                                               timeout=args.timeout)
    agree, checked, bad = agreement(first, body2, TMDB_FIELDS)
    title_equal = isinstance(body2, dict) and body2.get("title") == first.get("title")
    detail2 = {"status": status2, "fields_agree": agree, "fields_checked": checked, "mismatched": bad,
               "title_equal": title_equal, "list_title": first.get("title"),
               "detail_title": body2.get("title") if isinstance(body2, dict) else None,
               "fallback_reason": fallback_reason(hdrs2)}
    if status2 != 200:
        detail2["error"] = excerpt(raw2)
    sink.add("tmdb.detail", rep, status2 == 200 and title_equal, ms2, source_of(hdrs2), detail2)


def consistency_square(srv, sink, args, rep, h):
    amount = 1500 + 25 * rep
    req = {"source_id": "cnon:card-nonce-ok", "idempotency_key": str(uuid.uuid4()),
           "amount_money": {"amount": amount, "currency": "USD"}, "autocomplete": True,
           "location_id": "L88917AVBK2S5"}
    ms, status, hdrs, body, raw = request(f"{srv.data}/square/v2/payments", "POST", h, json_body=req,
                                          timeout=args.timeout)
    pay = body.get("payment") if isinstance(body, dict) else None
    pay = pay if isinstance(pay, dict) else {}
    checks = {"status_200": status == 200, "has_id": bool(pay.get("id")),
              "amount_copied": pay.get("amount_money") == req["amount_money"]}
    detail = {"status": status, "checks": checks, "id": pay.get("id"), "fallback_reason": fallback_reason(hdrs)}
    if status != 200:
        detail["error"] = excerpt(raw)
    sink.add("square.create", rep, all(checks.values()), ms, source_of(hdrs), detail)
    if status != 200 or not pay.get("id"):
        sink.add("square.read", rep, False, None, "skipped", {"reason": "create failed"})
        return
    pid = urllib.parse.quote(str(pay["id"]), safe="")
    ms2, status2, hdrs2, body2, raw2 = request(f"{srv.data}/square/v2/payments/{pid}", "GET", h,
                                               timeout=args.timeout)
    pay2 = body2.get("payment") if isinstance(body2, dict) else None
    unwrapped = pay2 is None and isinstance(body2, dict) and "id" in body2
    agree, checked, bad = agreement(pay, pay2, SQUARE_FIELDS)
    core = isinstance(pay2, dict) and all(pay2.get(k) == pay.get(k) for k in ("id", "amount_money"))
    detail2 = {"status": status2, "fields_agree": agree, "fields_checked": checked, "mismatched": bad,
               "core_equal": core, "missing_payment_envelope": unwrapped,
               "fallback_reason": fallback_reason(hdrs2)}
    if status2 != 200:
        detail2["error"] = excerpt(raw2)
    sink.add("square.read", rep, status2 == 200 and core, ms2, source_of(hdrs2), detail2)


# ---------------------------------------------------------------- sse

def type_ok(value, typ):
    if typ == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if typ == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if typ == "string":
        return isinstance(value, str)
    if typ == "boolean":
        return isinstance(value, bool)
    if typ == "object":
        return isinstance(value, dict)
    if typ == "array":
        return isinstance(value, list)
    if typ == "null":
        return value is None
    return True


def basic_violations(value, schema):
    """Type/required/minimum violations of an object schema, one level deep."""
    out = []
    if not type_ok(value, schema.get("type")):
        return [f"type:{schema.get('type')}"]
    for name in schema.get("required", []):
        if name not in value:
            out.append(f"required:{name}")
    for name, sub in schema.get("properties", {}).items():
        if name in value:
            if not type_ok(value[name], sub.get("type")):
                out.append(f"type:{name}")
            elif "minimum" in sub and value[name] < sub["minimum"]:
                out.append(f"minimum:{name}")
    return out


def read_sse(url, headers, want, timeout):
    """Reads `want` events (or until timeout / end); returns (status, ttfe_ms, total_ms, events, error)."""
    parts = urllib.parse.urlsplit(url)
    started = time.monotonic()
    conn = http.client.HTTPConnection(parts.hostname, parts.port, timeout=timeout)
    events, ttfe, error, status = [], None, None, 0
    try:
        conn.request("GET", parts.path + (f"?{parts.query}" if parts.query else ""), headers=headers)
        resp = conn.getresponse()
        status = resp.status
        if status != 200:
            return status, None, (time.monotonic() - started) * 1000, [], excerpt(resp.read())
        deadline = started + timeout
        cur = {"event": "message", "data": []}
        while len(events) < want:
            if time.monotonic() > deadline:
                error = "timeout"
                break
            line = resp.readline()
            if not line:
                error = "stream ended"
                break
            line = line.decode(errors="replace").rstrip("\r\n")
            if line == "":
                if cur["data"]:
                    if ttfe is None:
                        ttfe = (time.monotonic() - started) * 1000
                    events.append({"event": cur["event"], "data": "\n".join(cur["data"])})
                cur = {"event": "message", "data": []}
            elif line.startswith(":"):
                continue
            else:
                field, _, val = line.partition(":")
                val = val[1:] if val.startswith(" ") else val
                if field == "data":
                    cur["data"].append(val)
                elif field == "event":
                    cur["event"] = val
    except (socket.timeout, TimeoutError):
        error = "timeout"
    except (OSError, http.client.HTTPException) as e:
        error = f"{type(e).__name__}: {e}"
    finally:
        conn.close()
    return status, ttfe, (time.monotonic() - started) * 1000, events, error


def logged_source(srv, mock, session, waits=10):
    """Source and tokens of the session's newest request-log entry (SSE reports on disconnect)."""
    q = urllib.parse.urlencode({"session": session, "fields": "summary", "limit": 5})
    for _ in range(waits):
        _, status, _, body, _ = request(f"{srv.api}/mocks/{mock}/requests?{q}", timeout=10)
        if status == 200 and isinstance(body, list) and body:
            e = body[0]
            return e.get("source") or "?", {"prompt_tokens": e.get("prompt_tokens"),
                                            "completion_tokens": e.get("completion_tokens"),
                                            "fallback_reason": e.get("fallback_reason")}
        time.sleep(0.3)
    return "?", {}


def run_sse(srv, sink, args):
    stamp = int(time.time())
    want = args.stream_items
    for rep in range(args.repeat):
        session = f"bench-{session_slug(args.model)}-{stamp}-sse-{rep}"
        headers = {"x-mockingbird-session": session, "accept": "text/event-stream",
                   "x-mockingbird-stream-items": str(want)}
        status, ttfe, total, events, error = read_sse(f"{srv.data}/events/ticks", headers, want, args.timeout)
        valid, bad, names = 0, [], {}
        for i, ev in enumerate(events):
            names[ev["event"]] = names.get(ev["event"], 0) + 1
            try:
                value = json.loads(ev["data"])
            except ValueError:
                bad.append({"index": i, "violations": ["json"]})
                continue
            v = basic_violations(value, TICK_SCHEMA) if ev["event"] == "tick" else [f"event:{ev['event']}"]
            if v:
                bad.append({"index": i, "violations": v})
            else:
                valid += 1
        source, extra = logged_source(srv, "events", session)
        detail = {"status": status, "events": len(events), "wanted": want, "valid": valid,
                  "invalid": bad[:10], "event_names": names, "error": error, **extra}
        ok = status == 200 and len(events) == want and valid == want
        sink.add("ticks", rep, ok, total, source, detail, ttfe_ms=ttfe)


# ---------------------------------------------------------------- import

PARAM = re.compile(r"\{[^}]*\}")


def norm_target(target):
    """'get /a/{x}/' -> ('GET', '/a/{}')."""
    method, _, path = target.strip().partition(" ")
    path = path.strip().split("?")[0]
    path = PARAM.sub("{}", path).rstrip("/") or "/"
    return method.upper(), path


def target_match(a, b):
    """Same method and the same path, or one path a segment-suffix of the other (base paths differ)."""
    if a[0] != b[0]:
        return False
    if a[1] == b[1]:
        return True
    long, short = (a[1], b[1]) if len(a[1]) > len(b[1]) else (b[1], a[1])
    if short == "/" or len(long) == len(short):
        return False
    # Paths start with "/", so a suffix match is at a segment boundary (/v1/pets vs /pets).
    return long.endswith(short)


def score(found, expected):
    found_n = [norm_target(t) for t in found]
    exp_n = [norm_target(t) for t in expected]
    hit_exp = [any(target_match(e, f) for f in found_n) for e in exp_n]
    hit_found = [any(target_match(f, e) for e in exp_n) for f in found_n]
    recall = sum(hit_exp) / len(exp_n) if exp_n else None
    precision = sum(hit_found) / len(found_n) if found_n else (None if not exp_n else 0.0)
    missing = [t for t, hit in zip(expected, hit_exp) if not hit]
    extra = [t for t, hit in zip(found, hit_found) if not hit]
    return recall, precision, missing, extra


def load_json(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def import_inputs():
    """[(display name, path, expected list or None)] of every import input."""
    out = []
    smart = load_json(os.path.join(REPO, FIXTURES_SMART_EXPECTED)) or {}
    for rel in FIXTURE_INPUTS:
        name = os.path.basename(rel)
        out.append((name, os.path.join(REPO, rel), smart.get(name)))
    for path in sorted(glob.glob(os.path.join(REPO, IMPORT_DIR, "*"))):
        base = os.path.basename(path)
        if base.endswith(".json") or base.startswith(".") or base.lower() == "readme.md" or os.path.isdir(path):
            continue
        stem = os.path.splitext(path)[0]
        expected = None
        for cand in (f"{path}.expected.json", f"{stem}.expected.json"):
            data = load_json(cand)
            if data is not None:
                expected = data.get("operations", data) if isinstance(data, dict) else data
                break
        out.append((base, path, expected))
    return out


def import_body(name, path):
    ext = os.path.splitext(path)[1].lower()
    kind = INPUT_KINDS.get(ext)
    with open(path, "rb") as f:
        raw = f.read()
    if kind in ("markdown", "html", "text", "har"):
        return {"kind": kind, "text": raw.decode("utf-8", errors="replace"), "filename": name, "mode": "replace"}
    body = {"content_base64": base64.b64encode(raw).decode(), "filename": name, "mode": "replace"}
    if kind:
        body["kind"] = kind
    return body


def run_import(srv, sink, args):
    inputs = import_inputs()
    if args.only_input:
        inputs = [i for i in inputs if re.search(args.only_input, i[0])]
    reps = min(args.repeat, args.import_repeat)
    for rep in range(reps):
        for n, (name, path, expected) in enumerate(inputs):
            mock = f"imp-{rep}-{n}"
            _, status, _, body, raw = request(f"{srv.api}/mocks", "POST", json_body={
                "name": mock, "route_prefix": f"/{mock}", "llm_profile": "bench"}, timeout=30)
            if status not in (200, 201):
                sink.add(name, rep, False, None, "import", {"phase": "create_mock", "status": status,
                                                            "error": excerpt(raw, 300)})
                continue
            sink.add(name, rep, *import_one(srv, args, mock, name, path, expected))


def job_duration_ms(job):
    """created_at -> finished_at of a finished job, in ms (None when unknown)."""
    try:
        start = datetime.fromisoformat(job["created_at"].replace("Z", "+00:00"))
        end = datetime.fromisoformat(job["finished_at"].replace("Z", "+00:00"))
    except (KeyError, TypeError, AttributeError, ValueError):
        return None
    return round((end - start).total_seconds() * 1000, 1)


def import_one(srv, args, mock, name, path, expected):
    """(ok, latency_ms, source, detail) of one import job."""
    started = time.monotonic()
    _, status, _, job, raw = request(f"{srv.api}/mocks/{mock}/imports", "POST",
                                     json_body=import_body(name, path), timeout=60)
    if status != 202 or not isinstance(job, dict):
        return False, None, "import", {"phase": "create_job", "status": status, "error": excerpt(raw, 300)}
    job_id = job.get("id")
    deadline = started + args.import_timeout
    while job.get("status") in ("queued", "running") and time.monotonic() < deadline:
        time.sleep(args.poll_interval)
        _, st, _, polled, _ = request(f"{srv.api}/mocks/{mock}/imports/{job_id}", timeout=30)
        if st == 200 and isinstance(polled, dict):
            job = polled
    elapsed = (time.monotonic() - started) * 1000
    detail = {"job_status": job.get("status"), "error": job.get("error"), "progress": job.get("progress"),
              "result_version": job.get("result_version"), "expected": expected}
    if job.get("status") in ("queued", "running"):
        request(f"{srv.api}/mocks/{mock}/imports/{job_id}", "DELETE",
                headers={"content-type": "application/json"}, timeout=30)
        detail["error"] = "bench timeout; job canceled"
    job_ms = job_duration_ms(job)
    detail["wall_ms"] = round(elapsed, 1)
    detail["job_ms"] = job_ms
    if job_ms is not None:
        elapsed = job_ms  # exact, not rounded up to the poll interval
    version = job.get("result_version")
    found = []
    if job.get("status") == "done" and version is not None:
        _, st, _, draft, raw = request(f"{srv.api}/mocks/{mock}/contracts/{version}/draft", timeout=60)
        if st == 200 and isinstance(draft, dict):
            ops = draft.get("operations") or []
            found = [f"{o.get('method', '').upper()} {o.get('path', '')}" for o in ops]
            meta = draft.get("meta") or {}
            warnings = meta.get("warnings") or []
            codes = {}
            for w in warnings:
                codes[w.get("code")] = codes.get(w.get("code"), 0) + 1
            detail.update({
                "operations": found,
                "operation_count": len(found),
                "confidences": [o.get("confidence") for o in ops],
                "warnings_count": len(warnings),
                "warning_codes": codes,
                "chunk_count": meta.get("chunk_count"),
                "chunks_processed": meta.get("chunks_processed"),
                "prompt_tokens": (meta.get("usage") or {}).get("prompt_tokens"),
                "completion_tokens": (meta.get("usage") or {}).get("completion_tokens"),
            })
        else:
            detail["draft_error"] = f"{st}: {excerpt(raw, 300)}"
    compiles = job.get("status") == "done" and version is not None and "draft_error" not in detail
    detail["compiles"] = compiles
    if expected is not None:
        recall, precision, missing, extra = score(found, expected)
        detail.update({"expected_count": len(expected), "recall": recall, "precision": precision,
                       "missing": missing, "extra": extra})
    usage = (detail.get("prompt_tokens") or 0) + (detail.get("completion_tokens") or 0)
    source = "llm" if usage else ("deterministic" if compiles else "import")
    return compiles, elapsed, source, detail


# ---------------------------------------------------------------- journeys

# The checks of each journey in step order, the same names as the pilot
# (docs/benchmarks/pilot-2026-10-30/). A check that only applies in some runs
# (a succeeded intent, a numeric open_issues_count) is not listed here.
JOURNEY_CHECKS = {
    "stripe": [
        "intent keeps the customer", "intent keeps the amount", "read matches the created intent",
        "confirm keeps identity and amount", "confirm moves the status on",
        "read shows the confirmed status", "refund names its intent", "refund keeps the amount",
        "refund uses the intent's currency", "customer reads back as created",
    ],
    "travel": [
        "offers fly the searched route", "offers use the searched currency",
        "offer detail keeps the listed price", "offer detail keeps the listed segments",
        "booking names its offer", "booking keeps the passenger", "booking costs what the offer did",
        "booking flies the offer's segments", "read matches the created booking",
        "cancellation names its booking", "refund is in the booking's currency",
        "cancel shows on the booking",
    ],
    "github": [
        "repo detail keeps the listed repo", "issue keeps its title", "issue reads back as created",
        "issue list holds the new issue", "repo keeps its identity",
    ],
    "invoices": [
        "remind keeps the status", "preview keeps the status", "validate answers a report",
        "validate keeps the status", "pay moves the status on", "read shows the paid status",
        "pin sets the flag", "pin keeps the text", "read shows the pinned note",
    ],
}
# The check recorded when a journey cannot go on past its first call.
JOURNEY_GATE = {"travel": "search returned offers", "github": "search returned repositories"}


def journey_check(sink, journey, rep, name, ok, detail, source="?", latency_ms=None):
    """One row `<journey>.<name>`; `source` and `latency_ms` are those of the
    call the check judged."""
    sink.add(f"{journey}.{name}", rep, ok, latency_ms, source, detail)


class JourneyStop(Exception):
    """The journey cannot go on; its remaining checks fail."""


def eq(a, b):
    return a is not None and a == b


class Journey:
    """One journey run: a fresh session, calls through `request()`, and checks
    recorded as rows against the latest call."""

    def __init__(self, name, srv, sink, args, rep):
        self.name, self.srv, self.sink, self.args, self.rep = name, srv, sink, args, rep
        session = f"bench-{session_slug(args.model)}-{int(time.time())}-{name}-{rep}"
        self.headers = {"x-mockingbird-session": session}
        self.rng = random.Random(f"{args.model}|{name}|{rep}")
        self.done = set()
        self.last = {"status": None, "source": "?", "ms": None, "fallback_reason": None}

    def call(self, method, path, query=None, body=None, form=None):
        """The JSON body of the response, or {} when it is not a JSON object."""
        url = f"{self.srv.data}/{self.name}{path}"
        if query:
            url += "?" + urllib.parse.urlencode(query)
        ms, status, hdrs, parsed, raw = request(url, method, self.headers, json_body=body, form=form,
                                                timeout=self.args.timeout)
        self.last = {"status": status, "source": source_of(hdrs), "ms": ms,
                     "fallback_reason": fallback_reason(hdrs)}
        if status >= 400 or status == 0:
            self.last["error"] = excerpt(raw)
        return parsed if isinstance(parsed, (dict, list)) else {}

    def check(self, name, ok, **detail):
        self.done.add(name)
        detail = dict(detail, **{k: v for k, v in self.last.items() if k not in ("source", "ms") and v is not None})
        journey_check(self.sink, self.name, self.rep, name, bool(ok), detail,
                      self.last["source"], self.last["ms"])

    def stop(self, gate):
        self.check(gate, False)
        raise JourneyStop(gate)


def run_one_journey(name, steps, srv, sink, args, rep):
    j = Journey(name, srv, sink, args, rep)
    try:
        steps(j)
        reason = None
    except JourneyStop as e:
        reason = f"stopped at {e}"
    except Exception as e:  # noqa: BLE001 - a journey that breaks is recorded, not raised
        reason = f"{type(e).__name__}: {e}"[:300]
    for check in JOURNEY_CHECKS[name]:
        if check not in j.done:
            j.last = {"status": None, "source": "skipped", "ms": None, "fallback_reason": None}
            j.check(check, False, reason=reason or "journey ended early")


def stripe_steps(j):
    name, email = "Ana Silva", f"ana{j.rng.randint(100, 999)}@example.org"
    cust = j.call("POST", "/v1/customers", form={"name": name, "email": email})
    amount = 1000 + j.rng.randint(1, 900) * 10
    pi = j.call("POST", "/v1/payment_intents",
                form={"amount": amount, "currency": "eur", "customer": cust.get("id")})
    pid = pi.get("id")
    j.check("intent keeps the customer", eq(pi.get("customer"), cust.get("id")))
    j.check("intent keeps the amount", eq(pi.get("amount"), amount))
    read = j.call("GET", f"/v1/payment_intents/{pid}")
    j.check("read matches the created intent",
            all(eq(read.get(k), pi.get(k)) for k in ("id", "amount", "customer", "status")))
    conf = j.call("POST", f"/v1/payment_intents/{pid}/confirm", form={})
    j.check("confirm keeps identity and amount", eq(conf.get("id"), pid) and eq(conf.get("amount"), amount))
    j.check("confirm moves the status on",
            conf.get("status") in ("succeeded", "processing", "requires_action", "requires_capture"),
            status_value=conf.get("status"))
    if conf.get("status") == "succeeded":
        j.check("a succeeded intent received its amount", eq(conf.get("amount_received"), amount))
    read2 = j.call("GET", f"/v1/payment_intents/{pid}")
    j.check("read shows the confirmed status", eq(read2.get("status"), conf.get("status")),
            confirmed=conf.get("status"), read=read2.get("status"))
    part = amount // 2
    refund = j.call("POST", "/v1/refunds", form={"payment_intent": pid, "amount": part})
    j.check("refund names its intent", eq(refund.get("payment_intent"), pid))
    j.check("refund keeps the amount", eq(refund.get("amount"), part))
    j.check("refund uses the intent's currency", eq(str(refund.get("currency")).lower(), "eur"))
    cust2 = j.call("GET", f"/v1/customers/{cust.get('id')}")
    j.check("customer reads back as created", eq(cust2.get("name"), name) and eq(cust2.get("email"), email))


def travel_steps(j):
    search = {"origin": "LIS", "destination": "OSL", "departureDate": "2026-12-14",
              "passengers": [{"type": "adult"}], "currency": "EUR"}
    found = j.call("POST", "/flights/search", body=search)
    offers = found.get("offers") if isinstance(found, dict) else None
    if not offers or not isinstance(offers[0], dict):
        j.stop(JOURNEY_GATE["travel"])
    first = offers[0]
    segs = first.get("segments") or [{}]
    seg0 = segs[0] if isinstance(segs[0], dict) else {}
    origin = seg0.get("origin")
    j.check("offers fly the searched route",
            eq(origin.get("iata") if isinstance(origin, dict) else origin, "LIS") or "LIS" in json.dumps(seg0))
    price = first.get("price")
    j.check("offers use the searched currency", eq(price.get("currency") if isinstance(price, dict) else None, "EUR"))
    offer = j.call("GET", f"/offers/{first.get('id')}")
    j.check("offer detail keeps the listed price", eq(offer.get("price"), first.get("price")))
    j.check("offer detail keeps the listed segments", eq(offer.get("segments"), first.get("segments")))
    passenger = {"givenName": "Ana", "familyName": "Silva", "dateOfBirth": "1990-04-02", "type": "adult"}
    body = {"offerId": first.get("id"), "passengers": [passenger],
            "contact": {"email": "ana@example.org", "phone": "+351912345678"},
            "payment": {"type": "card", "brand": "visa", "last4": "4242", "expMonth": 8, "expYear": 2030,
                        "holderName": "Ana Silva"}}
    booking = j.call("POST", "/bookings", body=body)
    bid = booking.get("id")
    j.check("booking names its offer", eq(booking.get("offerId"), first.get("id")))
    passengers = booking.get("passengers")
    p0 = passengers[0] if isinstance(passengers, list) and passengers and isinstance(passengers[0], dict) else {}
    j.check("booking keeps the passenger", eq(p0.get("familyName"), "Silva"))
    bprice = booking.get("price")
    j.check("booking costs what the offer did",
            eq(bprice.get("total") if isinstance(bprice, dict) else None, first.get("price")))
    j.check("booking flies the offer's segments", eq(booking.get("segments"), first.get("segments")))
    read = j.call("GET", f"/bookings/{bid}")
    j.check("read matches the created booking",
            all(eq(read.get(k), booking.get(k)) for k in ("id", "reference", "status", "price")))
    cancel = j.call("POST", f"/bookings/{bid}/cancel", body={"reason": "customer_request"})
    j.check("cancellation names its booking", eq(cancel.get("bookingId"), bid))
    refund = cancel.get("refund")
    amount = refund.get("amount") if isinstance(refund, dict) else None
    total = bprice.get("total") if isinstance(bprice, dict) else None
    j.check("refund is in the booking's currency",
            eq(amount.get("currency") if isinstance(amount, dict) else None,
               total.get("currency") if isinstance(total, dict) else None))
    read2 = j.call("GET", f"/bookings/{bid}")
    j.check("cancel shows on the booking", read2.get("status") == "cancelled", status_value=read2.get("status"))


def github_steps(j):
    found = j.call("GET", "/search/repositories", query={"q": "language:rust", "per_page": 3})
    items = found.get("items") if isinstance(found, dict) else None
    first = items[0] if isinstance(items, list) and items and isinstance(items[0], dict) else None
    full = str(first.get("full_name") or "") if first else ""
    if "/" not in full:
        j.stop(JOURNEY_GATE["github"])
    owner, repo = full.split("/", 1)
    got = j.call("GET", f"/repos/{owner}/{repo}")
    j.check("repo detail keeps the listed repo",
            all(eq(got.get(k), first.get(k)) for k in ("id", "full_name", "stargazers_count", "description")))
    before = got.get("open_issues_count")
    title = "Crash when the cache is empty"
    issue = j.call("POST", f"/repos/{owner}/{repo}/issues",
                   body={"title": title, "body": "Steps to reproduce follow."})
    j.check("issue keeps its title", eq(issue.get("title"), title))
    number = issue.get("number")
    read = j.call("GET", f"/repos/{owner}/{repo}/issues/{number}")
    j.check("issue reads back as created", eq(read.get("title"), title) and eq(read.get("id"), issue.get("id")))
    again = j.call("GET", f"/repos/{owner}/{repo}/issues")
    listed = again if isinstance(again, list) else []
    j.check("issue list holds the new issue", any(isinstance(i, dict) and i.get("number") == number for i in listed))
    after = j.call("GET", f"/repos/{owner}/{repo}")
    j.check("repo keeps its identity", eq(after.get("id"), got.get("id")))
    if isinstance(before, int) and isinstance(after.get("open_issues_count"), int):
        j.check("repo counts the new issue", after["open_issues_count"] == before + 1,
                before=before, after=after["open_issues_count"])


def invoices_steps(j):
    inv = j.call("POST", "/invoices", body={"customer": "Ana Silva", "amount": 4180, "currency": "eur"})
    iid = inv.get("id")
    # An action that moves nothing on is the check against a prompt that pushes every status.
    rem = j.call("POST", f"/invoices/{iid}/remind")
    j.check("remind keeps the status", eq(rem.get("status"), inv.get("status")),
            before=inv.get("status"), status_value=rem.get("status"))
    seen = j.call("POST", f"/invoices/{iid}/preview")
    j.check("preview keeps the status", eq(seen.get("status"), inv.get("status")),
            before=inv.get("status"), status_value=seen.get("status"))
    report = j.call("POST", f"/invoices/{iid}/validate")
    j.check("validate answers a report", isinstance(report.get("valid"), bool), report=report)
    read = j.call("GET", f"/invoices/{iid}")
    j.check("validate keeps the status", eq(read.get("status"), inv.get("status")),
            before=inv.get("status"), status_value=read.get("status"))
    paid = j.call("POST", f"/invoices/{iid}/pay")
    j.check("pay moves the status on", paid.get("status") == "paid",
            before=rem.get("status"), status_value=paid.get("status"))
    read = j.call("GET", f"/invoices/{iid}")
    j.check("read shows the paid status", eq(read.get("status"), paid.get("status")),
            paid=paid.get("status"), read=read.get("status"))
    # No enum, so nothing in the prompt may talk about a status.
    note = j.call("POST", "/notes", body={"text": "Call the bank"})
    nid = note.get("id")
    pinned = j.call("POST", f"/notes/{nid}/pin")
    j.check("pin sets the flag", pinned.get("pinned") is True, pinned=pinned.get("pinned"))
    j.check("pin keeps the text", eq(pinned.get("text"), "Call the bank"))
    read = j.call("GET", f"/notes/{nid}")
    j.check("read shows the pinned note", read.get("pinned") is True, pinned=read.get("pinned"))


def journey_stripe(srv, sink, args, rep):
    run_one_journey("stripe", stripe_steps, srv, sink, args, rep)


def journey_travel(srv, sink, args, rep):
    run_one_journey("travel", travel_steps, srv, sink, args, rep)


def journey_github(srv, sink, args, rep):
    run_one_journey("github", github_steps, srv, sink, args, rep)


JOURNEYS = {
    "stripe": journey_stripe, "travel": journey_travel, "github": journey_github,
    "invoices": lambda srv, sink, args, rep: run_one_journey("invoices", invoices_steps, srv, sink, args, rep),
}
# What a journeys run without --journey does: the set the published results compare.
DEFAULT_JOURNEYS = ("stripe", "travel", "github")


def run_journeys(srv, sink, args):
    for rep in range(args.repeat):
        for name in args.journey or DEFAULT_JOURNEYS:
            JOURNEYS[name](srv, sink, args, rep)


# ---------------------------------------------------------------- held-out journeys

# These journeys measure how the engine generalises beyond the `journeys` suite,
# which drove its fixes. They are written from the contracts alone, as a client of
# each API would expect it to behave. Do not edit a check to suit results.
HELDOUT_CHECKS = {
    "paypal": [
        "order has an id", "order keeps the intent", "order keeps the amount", "order keeps the reference id",
        "read matches the created order", "confirm keeps the order id", "confirm names the payer",
        "confirm moves the status on", "read shows the confirmed status", "capture keeps the order id",
        "capture completes the order", "capture takes the order's amount", "read shows the completed capture",
        "read keeps the order's details",
    ],
    "jira": [
        "issue has an id and key", "issue lands in the project asked for", "read keeps the identity",
        "read keeps the summary", "read keeps the description", "read keeps project, type and priority",
        "read keeps the labels", "comment keeps its text", "comment list holds the new comment",
        "comment list counts one comment", "transitions offer another status", "read shows the new status",
        "read keeps the issue after the transition", "search finds the issue in its new status",
    ],
    "zendesk": [
        "ticket has an id", "ticket keeps its subject", "ticket keeps priority and type",
        "first comment becomes the description", "audit names the ticket", "read matches the created ticket",
        "read keeps the tags", "reply keeps the ticket", "comment list holds the reply",
        "comment list holds both comments in order", "update changes status and priority",
        "read shows the update", "read keeps the subject and description", "list shows the ticket as updated",
    ],
    "notion": [
        "page has an id", "page keeps its parent", "page keeps its title", "page keeps its properties",
        "read matches the created page", "update keeps the page's identity", "update changes the properties",
        "read shows the update", "read keeps what the update left alone", "append returns the new blocks in order",
        "new blocks name their page", "block list holds the appended blocks",
        "query finds the page in its new status", "search finds the page by title",
    ],
    "square": [
        "payment has an id", "payment keeps the amount", "total adds the tip", "payment keeps the client's details",
        "delayed payment waits for completion", "read matches the created payment",
        "complete keeps identity and amount", "complete moves the status on", "read shows the completed status",
        "list shows the completed payment", "refund names its payment", "refund keeps the amount and reason",
        "read shows the refund", "read keeps the original amount",
    ],
}
JOURNEY_CHECKS.update(HELDOUT_CHECKS)
# The contracts require a credential; this one only makes the client well formed.
HELDOUT_AUTH = {"authorization": "Bearer bench-placeholder"}


def need(j, check, value):
    """A check the rest of the journey depends on: when it fails, so do the others."""
    j.check(check, bool(value))
    if not value:
        raise JourneyStop(check)


def obj(value):
    return value if isinstance(value, dict) else {}


def dicts(value):
    return [v for v in value if isinstance(v, dict)] if isinstance(value, list) else []


def head(value):
    items = dicts(value)
    return items[0] if items else {}


def by_id(items, wanted):
    return next((i for i in dicts(items) if eq(i.get("id"), wanted)), {})


def same(a, b, *fields):
    return all(eq(a.get(f), b.get(f)) for f in fields)


def paypal_steps(j):
    # Capture, not authorize: an order has one intent. Refunds are in another
    # contract (paypal-payments). PATCH is left out: the lifecycle does not need it.
    j.headers.update(HELDOUT_AUTH)
    j.headers["prefer"] = "return=representation"  # the full order, not only its id and status
    amount = {"currency_code": "EUR", "value": f"{j.rng.randint(20, 900)}.{j.rng.choice(['25', '50', '75'])}"}
    reference = f"basket-{j.rng.randint(1000, 9999)}"
    order = j.call("POST", "/v2/checkout/orders", body={
        "intent": "CAPTURE",
        "purchase_units": [{"reference_id": reference, "description": "Two ceramic mugs", "amount": amount}],
    })
    oid = order.get("id")
    need(j, "order has an id", oid)
    unit = head(order.get("purchase_units"))
    j.check("order keeps the intent", eq(order.get("intent"), "CAPTURE"))
    j.check("order keeps the amount", same(obj(unit.get("amount")), amount, "currency_code", "value"))
    j.check("order keeps the reference id", eq(unit.get("reference_id"), reference))
    read = j.call("GET", f"/v2/checkout/orders/{oid}")
    read_unit = head(read.get("purchase_units"))
    j.check("read matches the created order",
            same(read, order, "id", "intent", "status") and same(read_unit, unit, "reference_id", "amount"))
    email = f"ana{j.rng.randint(100, 999)}@example.org"
    conf = j.call("POST", f"/v2/checkout/orders/{oid}/confirm-payment-source",
                  body={"payment_source": {"paypal": {"email_address": email}}})
    j.check("confirm keeps the order id", eq(conf.get("id"), oid))
    j.check("confirm names the payer", eq(obj(obj(conf.get("payment_source")).get("paypal")).get("email_address"), email))
    j.check("confirm moves the status on", conf.get("status") in ("APPROVED", "PAYER_ACTION_REQUIRED"),
            status_value=conf.get("status"))
    read2 = j.call("GET", f"/v2/checkout/orders/{oid}")
    j.check("read shows the confirmed status", eq(read2.get("status"), conf.get("status")),
            confirmed=conf.get("status"), read=read2.get("status"))
    cap = j.call("POST", f"/v2/checkout/orders/{oid}/capture", body={})
    j.check("capture keeps the order id", eq(cap.get("id"), oid))
    j.check("capture completes the order", eq(cap.get("status"), "COMPLETED"), status_value=cap.get("status"))
    capture = head(obj(head(cap.get("purchase_units")).get("payments")).get("captures"))
    j.check("capture takes the order's amount", same(obj(capture.get("amount")), amount, "currency_code", "value"))
    read3 = j.call("GET", f"/v2/checkout/orders/{oid}")
    read3_unit = head(read3.get("purchase_units"))
    read3_capture = head(obj(read3_unit.get("payments")).get("captures"))
    j.check("read shows the completed capture",
            eq(read3.get("status"), "COMPLETED") and eq(read3_capture.get("id"), capture.get("id")),
            status_value=read3.get("status"))
    j.check("read keeps the order's details",
            eq(read3.get("intent"), "CAPTURE") and eq(read3_unit.get("reference_id"), reference)
            and same(obj(read3_unit.get("amount")), amount, "currency_code", "value")
            and eq(obj(obj(read3.get("payment_source")).get("paypal")).get("email_address"), email))


def adf(text):
    return {"type": "doc", "version": 1,
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": text}]}]}


def adf_text(doc):
    return "".join(str(node.get("text")) for para in dicts(obj(doc).get("content"))
                   for node in dicts(para.get("content")))


def jira_steps(j):
    j.headers.update(HELDOUT_AUTH)
    issue = "/rest/api/3/issue"
    summary = f"Checkout fails for basket {j.rng.randint(1000, 9999)}"
    description = "Paying with a saved card returns an error page."
    labels = ["checkout", "regression"]
    created = j.call("POST", issue, body={"fields": {
        "project": {"key": "PLNT"}, "issuetype": {"name": "Bug"}, "summary": summary,
        "description": adf(description), "priority": {"name": "High"}, "labels": labels,
    }})
    key = created.get("key")
    need(j, "issue has an id and key", key and created.get("id"))
    j.check("issue lands in the project asked for", str(key).startswith("PLNT-"), key=key)
    read = j.call("GET", f"{issue}/{key}")
    fields = obj(read.get("fields"))
    j.check("read keeps the identity", same(read, created, "id", "key"))
    j.check("read keeps the summary", eq(fields.get("summary"), summary))
    j.check("read keeps the description", adf_text(fields.get("description")) == description)
    j.check("read keeps project, type and priority",
            eq(obj(fields.get("project")).get("key"), "PLNT") and eq(obj(fields.get("issuetype")).get("name"), "Bug")
            and eq(obj(fields.get("priority")).get("name"), "High"))
    j.check("read keeps the labels", isinstance(fields.get("labels"), list) and sorted(fields["labels"]) == labels)
    text = "Reproduced on the staging shop as well."
    comment = j.call("POST", f"{issue}/{key}/comment", body={"body": adf(text)})
    j.check("comment keeps its text", adf_text(comment.get("body")) == text)
    page = j.call("GET", f"{issue}/{key}/comment")
    comments = dicts(page.get("comments"))
    j.check("comment list holds the new comment", comment.get("id") is not None
            and adf_text(by_id(comments, comment.get("id")).get("body")) == text)
    j.check("comment list counts one comment", eq(page.get("total"), 1) and len(comments) == 1,
            total=page.get("total"), listed=len(comments))
    before = obj(fields.get("status")).get("name")
    transitions = j.call("GET", f"{issue}/{key}/transitions")
    options = [t for t in dicts(transitions.get("transitions"))
               if t.get("id") and obj(t.get("to")).get("name") not in (None, before)]
    target = next((t for t in options if t["to"]["name"] == "In Progress"), options[0] if options else None)
    need(j, "transitions offer another status", target)
    to_status = target["to"]["name"]
    j.call("POST", f"{issue}/{key}/transitions", body={"transition": {"id": target["id"]}})
    read2 = j.call("GET", f"{issue}/{key}")
    fields2 = obj(read2.get("fields"))
    j.check("read shows the new status", eq(obj(fields2.get("status")).get("name"), to_status),
            wanted=to_status, read=obj(fields2.get("status")).get("name"))
    j.check("read keeps the issue after the transition",
            same(read2, created, "id", "key") and eq(fields2.get("summary"), summary))
    found = j.call("GET", "/rest/api/3/search/jql", query={"jql": f"key = {key}"})
    hit = next((i for i in dicts(found.get("issues")) if eq(i.get("key"), key)), {})
    j.check("search finds the issue in its new status",
            eq(obj(obj(hit.get("fields")).get("status")).get("name"), to_status))


def zendesk_steps(j):
    # The contract has no route to post a comment: a comment goes in through
    # PUT /api/v2/tickets/{ticket_id}, as its summary says.
    j.headers.update(HELDOUT_AUTH)
    subject = f"Invoice {j.rng.randint(1000, 9999)} shows the wrong address"
    opening, reply = "The invoice has our old address on it.", "Checking with the billing team."
    tags = ["billing", "invoice"]
    made = j.call("POST", "/api/v2/tickets", body={"ticket": {
        "subject": subject, "comment": {"body": opening}, "priority": "high", "type": "incident", "tags": tags,
        "requester": {"name": "Ana Silva", "email": "ana@example.org"},
    }})
    ticket = obj(made.get("ticket"))
    tid = ticket.get("id")
    need(j, "ticket has an id", tid)
    path = f"/api/v2/tickets/{tid}"
    j.check("ticket keeps its subject", eq(ticket.get("subject"), subject))
    j.check("ticket keeps priority and type", eq(ticket.get("priority"), "high") and eq(ticket.get("type"), "incident"))
    j.check("first comment becomes the description", eq(ticket.get("description"), opening))
    j.check("audit names the ticket", eq(obj(made.get("audit")).get("ticket_id"), tid))
    read = obj(j.call("GET", path).get("ticket"))
    j.check("read matches the created ticket",
            same(read, ticket, "id", "subject", "description", "status", "priority", "type", "requester_id"))
    j.check("read keeps the tags", isinstance(read.get("tags"), list) and sorted(read["tags"]) == tags)
    replied = obj(j.call("PUT", path, body={"ticket": {"comment": {"body": reply, "public": False}}}).get("ticket"))
    j.check("reply keeps the ticket", eq(replied.get("id"), tid) and eq(replied.get("subject"), subject))
    page = j.call("GET", f"{path}/comments")
    comments = dicts(page.get("comments"))
    j.check("comment list holds the reply",
            any(eq(c.get("body"), reply) and c.get("public") is False for c in comments))
    j.check("comment list holds both comments in order",
            [c.get("body") for c in comments] == [opening, reply] and eq(page.get("count"), 2),
            listed=len(comments), count=page.get("count"))
    updated = obj(j.call("PUT", path, body={"ticket": {"status": "pending", "priority": "urgent"}}).get("ticket"))
    j.check("update changes status and priority",
            eq(updated.get("id"), tid) and eq(updated.get("status"), "pending") and eq(updated.get("priority"), "urgent"))
    read2 = obj(j.call("GET", path).get("ticket"))
    j.check("read shows the update", eq(read2.get("status"), "pending") and eq(read2.get("priority"), "urgent"),
            status_value=read2.get("status"), priority=read2.get("priority"))
    j.check("read keeps the subject and description",
            eq(read2.get("subject"), subject) and eq(read2.get("description"), opening))
    listed = by_id(j.call("GET", "/api/v2/tickets", query={"sort_by": "created_at", "sort_order": "desc"}).get("tickets"),
                   tid)
    j.check("list shows the ticket as updated", eq(listed.get("status"), "pending") and eq(listed.get("subject"), subject))


def notion_text(rich_text):
    return "".join(str(t.get("plain_text")) for t in dicts(rich_text))


def notion_title(page):
    return notion_text(obj(obj(page.get("properties")).get("Name")).get("title"))


def notion_values(page):
    """(status, priority, due date) of a page in the contract's data source."""
    props = obj(page.get("properties"))
    return (obj(obj(props.get("Status")).get("status")).get("name"),
            obj(obj(props.get("Priority")).get("select")).get("name"),
            obj(obj(props.get("Due")).get("date")).get("start"))


def notion_block_text(block):
    return notion_text(obj(block.get(block.get("type"))).get("rich_text"))


def notion_block(kind, text, **extra):
    return {"object": "block", "type": kind,
            kind: {"rich_text": [{"type": "text", "text": {"content": text}}], **extra}}


def notion_steps(j):
    # A page in a data source the client was given: the contract has no route to
    # create a database or a data source. Trashing is left out: its Page schema pins
    # in_trash and archived to false, so a read-back could not show it.
    j.headers.update(HELDOUT_AUTH)
    j.headers["notion-version"] = "2025-09-03"
    source = str(uuid.UUID(int=j.rng.getrandbits(128), version=4))
    title = f"Oslo launch checklist {j.rng.randint(100, 999)}"
    due = "2026-12-14"
    page = j.call("POST", "/v1/pages", body={
        "parent": {"type": "data_source_id", "data_source_id": source},
        "properties": {
            "Name": {"title": [{"type": "text", "text": {"content": title}}]},
            "Status": {"status": {"name": "Not started"}},
            "Priority": {"select": {"name": "P1"}},
            "Due": {"date": {"start": due}},
        },
    })
    pid = page.get("id")
    need(j, "page has an id", pid)
    j.check("page keeps its parent", eq(obj(page.get("parent")).get("data_source_id"), source))
    j.check("page keeps its title", notion_title(page) == title)
    j.check("page keeps its properties", notion_values(page) == ("Not started", "P1", due), values=notion_values(page))
    read = j.call("GET", f"/v1/pages/{pid}")
    j.check("read matches the created page",
            same(read, page, "id", "parent", "created_time") and notion_title(read) == title
            and notion_values(read) == notion_values(page))
    updated = j.call("PATCH", f"/v1/pages/{pid}", body={"properties": {
        "Status": {"status": {"name": "In progress"}}, "Priority": {"select": {"name": "P0"}},
    }})
    j.check("update keeps the page's identity", eq(updated.get("id"), pid) and notion_title(updated) == title)
    j.check("update changes the properties", notion_values(updated)[:2] == ("In progress", "P0"),
            values=notion_values(updated))
    read2 = j.call("GET", f"/v1/pages/{pid}")
    j.check("read shows the update", notion_values(read2)[:2] == ("In progress", "P0"), values=notion_values(read2))
    j.check("read keeps what the update left alone",
            notion_title(read2) == title and notion_values(read2)[2] == due and same(read2, page, "id", "created_time"))
    texts = ["Book the venue in Oslo.", "Send the invitations."]
    appended = j.call("PATCH", f"/v1/blocks/{pid}/children", body={"children": [
        notion_block("paragraph", texts[0]), notion_block("to_do", texts[1], checked=False),
    ]})
    blocks = dicts(appended.get("results"))
    j.check("append returns the new blocks in order", [notion_block_text(b) for b in blocks] == texts)
    j.check("new blocks name their page",
            len(blocks) == 2 and all(eq(obj(b.get("parent")).get("page_id"), pid) for b in blocks))
    children = dicts(j.call("GET", f"/v1/blocks/{pid}/children").get("results"))
    j.check("block list holds the appended blocks",
            [notion_block_text(b) for b in children] == texts
            and [b.get("id") for b in children] == [b.get("id") for b in blocks])
    query = j.call("POST", f"/v1/data_sources/{source}/query",
                   body={"filter": {"property": "Status", "status": {"equals": "In progress"}}})
    j.check("query finds the page in its new status",
            notion_values(by_id(query.get("results"), pid))[0] == "In progress")
    found = j.call("POST", "/v1/search", body={"query": title, "filter": {"property": "object", "value": "page"}})
    j.check("search finds the page by title", notion_title(by_id(found.get("results"), pid)) == title)


def square_steps(j):
    # The contract has no route to read a refund: its effect is read on the payment.
    j.headers.update(HELDOUT_AUTH)
    location = "L88917AVBK2S5"
    amount = {"amount": 1000 + j.rng.randint(1, 900) * 10, "currency": "USD"}
    tip = {"amount": 100 + j.rng.randint(1, 90) * 5, "currency": "USD"}
    reference, note = f"table-{j.rng.randint(100, 999)}", "Dinner for two"
    made = j.call("POST", "/v2/payments", body={
        "source_id": "cnon:card-nonce-ok", "idempotency_key": str(uuid.UUID(int=j.rng.getrandbits(128), version=4)),
        "amount_money": amount, "tip_money": tip, "autocomplete": False, "location_id": location,
        "reference_id": reference, "note": note,
    })
    pay = obj(made.get("payment"))
    pid = pay.get("id")
    need(j, "payment has an id", pid)
    j.check("payment keeps the amount", eq(pay.get("amount_money"), amount))
    j.check("total adds the tip", eq(obj(pay.get("total_money")).get("amount"), amount["amount"] + tip["amount"]))
    j.check("payment keeps the client's details",
            eq(pay.get("reference_id"), reference) and eq(pay.get("note"), note) and eq(pay.get("location_id"), location))
    j.check("delayed payment waits for completion", eq(pay.get("status"), "APPROVED"), status_value=pay.get("status"))
    read = obj(j.call("GET", f"/v2/payments/{pid}").get("payment"))
    j.check("read matches the created payment",
            same(read, pay, "id", "amount_money", "total_money", "status", "location_id", "order_id", "reference_id"))
    done = obj(j.call("POST", f"/v2/payments/{pid}/complete", body={}).get("payment"))
    j.check("complete keeps identity and amount", eq(done.get("id"), pid) and eq(done.get("amount_money"), amount))
    j.check("complete moves the status on", eq(done.get("status"), "COMPLETED"), status_value=done.get("status"))
    read2 = obj(j.call("GET", f"/v2/payments/{pid}").get("payment"))
    j.check("read shows the completed status", eq(read2.get("status"), "COMPLETED"), status_value=read2.get("status"))
    listed = by_id(j.call("GET", "/v2/payments", query={"location_id": location}).get("payments"), pid)
    j.check("list shows the completed payment", eq(listed.get("status"), "COMPLETED") and eq(listed.get("amount_money"), amount))
    part = {"amount": amount["amount"] // 2, "currency": "USD"}
    reason = "One dish was returned"
    refund = obj(j.call("POST", "/v2/refunds", body={
        "idempotency_key": str(uuid.UUID(int=j.rng.getrandbits(128), version=4)),
        "amount_money": part, "payment_id": pid, "reason": reason,
    }).get("refund"))
    j.check("refund names its payment", eq(refund.get("payment_id"), pid))
    j.check("refund keeps the amount and reason", eq(refund.get("amount_money"), part) and eq(refund.get("reason"), reason))
    read3 = obj(j.call("GET", f"/v2/payments/{pid}").get("payment"))
    ids = read3.get("refund_ids")
    j.check("read shows the refund",
            isinstance(ids, list) and refund.get("id") in ids and eq(read3.get("refunded_money"), part),
            refund_ids=ids, refunded=read3.get("refunded_money"))
    j.check("read keeps the original amount", eq(read3.get("amount_money"), amount))


HELDOUT_JOURNEYS = {"paypal": paypal_steps, "jira": jira_steps, "zendesk": zendesk_steps,
                    "notion": notion_steps, "square": square_steps}


def run_journeys_heldout(srv, sink, args):
    for rep in range(args.repeat):
        for name, steps in HELDOUT_JOURNEYS.items():
            run_one_journey(name, steps, srv, sink, args, rep)


# ---------------------------------------------------------------- main

RUNNERS = {
    "consistency": (CONSISTENCY_MOCKS, run_consistency),
    "sse": (SSE_MOCKS, run_sse),
    "import": ({}, run_import),
    "journeys": (JOURNEY_MOCKS, run_journeys),
    "journeys_heldout": (HELDOUT_MOCKS, run_journeys_heldout),
}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--model", required=True, help="model id, e.g. deepseek/deepseek-v4.1-flash")
    p.add_argument("--proxy-base", required=True, help="bench proxy, e.g. http://127.0.0.1:18080")
    p.add_argument("--out", required=True, help="results directory (server.jsonl is appended)")
    p.add_argument("--repeat", type=int, default=3, help="reps (fresh session each) per suite")
    p.add_argument("--suite", action="append", choices=SUITES, help="default: consistency, sse, import")
    p.add_argument("--journey", action="append", choices=tuple(JOURNEYS),
                   help="journeys to run; default: stripe, travel, github")
    p.add_argument("--binary", default="target/release/mockingbird")
    p.add_argument("--timeout", type=float, default=180, help="client timeout per call, seconds")
    p.add_argument("--stream-items", type=int, default=5, help="SSE events to read per rep")
    p.add_argument("--import-repeat", type=int, default=1, help="cap on import reps (min with --repeat)")
    p.add_argument("--import-timeout", type=float, default=600, help="per import job, seconds")
    p.add_argument("--poll-interval", type=float, default=0.25, help="import job poll, seconds")
    p.add_argument("--only-input", help="regex over import input names")
    p.add_argument("--ready-timeout", type=float, default=120)
    p.add_argument("--keep", action="store_true", help="keep each suite's temp directory")
    args = p.parse_args()
    if not os.path.exists(args.binary) and not os.path.isabs(args.binary):
        args.binary = os.path.join(REPO, args.binary)
    args.binary = os.path.abspath(args.binary)
    if not os.path.exists(args.binary):
        sys.exit(f"{args.binary} not found: cargo build --release -p mockingbird-cli")
    os.makedirs(args.out, exist_ok=True)
    failed = False
    for suite in args.suite or DEFAULT_SUITES:
        mocks, runner = RUNNERS[suite]
        sink = Sink(args.out, args.model, suite)
        print(f"{args.model} / {suite}", file=sys.stderr)
        try:
            with Server(args, suite, mocks) as srv:
                runner(srv, sink, args)
        except Exception as e:  # one broken suite must not stop the others
            failed = True
            sink.add("suite_error", -1, False, None, "error", {"error": f"{type(e).__name__}: {e}"[:500]})
        n_ok = sum(1 for r in sink.rows if r["ok"])
        print(f"  {suite}: {n_ok}/{len(sink.rows)} ok", file=sys.stderr)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
