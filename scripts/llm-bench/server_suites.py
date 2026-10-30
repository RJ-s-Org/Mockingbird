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
SUITES = ("consistency", "sse", "import")
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


# ---------------------------------------------------------------- main

RUNNERS = {
    "consistency": (CONSISTENCY_MOCKS, run_consistency),
    "sse": (SSE_MOCKS, run_sse),
    "import": ({}, run_import),
}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--model", required=True, help="model id, e.g. deepseek/deepseek-v4.1-flash")
    p.add_argument("--proxy-base", required=True, help="bench proxy, e.g. http://127.0.0.1:18080")
    p.add_argument("--out", required=True, help="results directory (server.jsonl is appended)")
    p.add_argument("--repeat", type=int, default=3, help="reps (fresh session each) per suite")
    p.add_argument("--suite", action="append", choices=SUITES, help="default: all")
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
    for suite in args.suite or SUITES:
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
