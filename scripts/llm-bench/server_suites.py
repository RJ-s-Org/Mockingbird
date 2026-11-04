#!/usr/bin/env python3
"""`mockingbird serve` workloads of the LLM bench: consistency, SSE and smart import.

    python3 scripts/llm-bench/server_suites.py --model deepseek/deepseek-v4.1-flash \\
        --proxy-base http://127.0.0.1:18080 --out docs/benchmarks/2026-10-29 --repeat 3 \\
        [--suite consistency --suite sse --suite import] [--binary target/release/mockingbird] \\
        [--jobs 4] [--suites-parallel]

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
  judged. A journey that breaks fails its remaining checks. A failure the provider
  caused (a timeout or 5xx fallback) carries `detail.infra`, and a journey it ends
  before any check passed is re-run once (`run_one_journey`). Not in the default
  suites (it is asked for with `--suite journeys`). Measures remembering entities
  and the effects of writes.
- journeys_heldout: the same kind of journey over five contracts the engine was not
  fixed against: PayPal orders, Jira issues, Zendesk tickets, Notion pages and Square
  payments. Not in the default suites. Measures how `journeys` generalises, so its
  checks are written from each contract and are not edited to suit results.
- journeys_blind: the same kind of journey over five more contracts: Spotify playlists,
  Discord messages, Auth0 users, HubSpot CRM and Slack messages. Not in the default
  suites. Its checks were written from the contracts and from what each real API
  promises a client, without looking at the engine or at any past result, so it
  measures behaviour on contracts nobody tuned against. Do not edit a check to suit results.
- journeys_blind2: the same again over five more contracts: Twilio messages and calls,
  SendGrid templates and bounces, Plaid items, PayPal refunds and OpenWeatherMap. Not in
  the default suites. Written under the same rule, and no check gates the rest of its journey.
- journeys_blind3: the same again over five contracts written for it, in
  `scripts/llm-bench/contracts/blind3/`: Trello boards, Asana tasks, GitLab issues,
  Freshdesk tickets and Mailchimp audiences. Not in the default suites. Same rule; no
  check gates the rest.
- journeys_blind4: the same again over five more contracts written for it, in
  `scripts/llm-bench/contracts/blind4/`: Pipedrive deals, Zoom meetings, Sentry
  projects, Contentful entries and DigitalOcean droplets. Not in the default suites.
  Same rule; no check gates the rest.
- journeys_blind5: the same again over five more contracts written for it, in
  `scripts/llm-bench/contracts/blind5/`: Airtable records, Okta users, Box folders,
  Intercom conversations and Cal.com bookings. Not in the default suites. Same rule;
  no check gates the rest.
- journeys_blind6: the same again over five more contracts written for it, in
  `scripts/llm-bench/contracts/blind6/`: Dropbox folders, Klaviyo profiles and lists,
  WooCommerce products and orders, Statuspage incidents and Zoho Books invoices. Not
  in the default suites. Same rule; no check gates the rest.
- journeys_blind7: the same again over five more contracts written for it, in
  `scripts/llm-bench/contracts/blind7/`: Linear issues (GraphQL), Mollie payments,
  Chargebee customers and credits, Bitbucket pull requests and Cloudflare DNS
  records. Not in the default suites. Same rule; no check gates the rest.
- sse: `fixtures/openapi/events-3.1.yaml` `GET /ticks`: time to first event,
  events received, each event parsed and checked against the Tick schema.
- import: smart import through the Admin API of every input in
  `docs/benchmarks/import-inputs/` plus `fixtures/smart/petstore.md` and
  `fixtures/smart/tmdb-readme.html`; recall/precision of `METHOD /path` against
  the expected lists. Imports run `--import-repeat` times (default 1:
  extraction is at temperature 0, so repeats mostly add spend).

Concurrency: `--jobs N` runs independent units on N threads against each suite's
server: every (rep, journey) of a journeys suite, and every rep of consistency and
SSE (each has its own session). Rows are the same as with the default `--jobs 1`
except for their order and latencies. Import always runs in order.
`--suites-parallel` runs the suites at once, each on its own server, temp
directory and free ports; so does one process per (model, suite), which is safe
too: ports are picked under a lock file in the temp directory and held until the
server is ready.

Standard library only. Makes no LLM calls itself.
"""

import argparse
import base64
import contextlib
import fcntl
import functools
import glob
import hashlib
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
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from concurrent.futures import FIRST_EXCEPTION, ThreadPoolExecutor, wait
from datetime import datetime, timedelta, timezone

REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
SUITES = ("consistency", "sse", "import", "journeys", "journeys_heldout", "journeys_blind", "journeys_blind2",
          "journeys_blind3", "journeys_blind4", "journeys_blind5", "journeys_blind6", "journeys_blind7")
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
# The blind journeys, contracts alone as well: no persona.
BLIND_MOCKS = {
    "spotify": ("examples/apis/spotify/spotify.openapi.yaml", "/spotify", None),
    "discord": ("examples/apis/discord/discord.openapi.yaml", "/discord", None),
    "auth0": ("examples/apis/auth0-management/auth0-management.openapi.yaml", "/auth0", None),
    "hubspot": ("examples/apis/hubspot/hubspot.openapi.yaml", "/hubspot", None),
    "slack": ("examples/apis/slack/slack.openapi.yaml", "/slack", None),
}
# The second blind journeys, contracts alone as well: no persona.
BLIND2_MOCKS = {
    "twilio": ("examples/apis/twilio/twilio.openapi.yaml", "/twilio", None),
    "sendgrid": ("examples/apis/sendgrid/sendgrid.openapi.yaml", "/sendgrid", None),
    "plaid": ("examples/apis/plaid/plaid.openapi.yaml", "/plaid", None),
    "paypalpayments": ("examples/apis/paypal/paypal-payments.openapi.yaml", "/paypalpayments", None),
    "openweathermap": ("examples/apis/openweathermap/openweathermap.openapi.yaml", "/openweathermap", None),
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
    return free_ports(1)[0]


def free_ports(n):
    """`n` distinct free ports. Every socket stays bound until all are picked,
    so one call never hands out the same port twice."""
    socks = []
    try:
        for _ in range(n):
            s = socket.socket()
            socks.append(s)
            s.bind(("127.0.0.1", 0))
        return tuple(s.getsockname()[1] for s in socks)
    finally:
        for s in socks:
            s.close()


# Held from picking a server's ports until it answers /readyz, so another bench
# process or thread (`run.py full --parallel`, one process per suite,
# --suites-parallel) cannot pick a port that is free only because our server has
# not bound it yet: once it is bound, the OS no longer hands it out. Processes
# coordinate only when they share a temp directory (TMPDIR).
PORT_LOCK_PATH = os.path.join(tempfile.gettempdir(), "mb-bench-ports.lock")
_PORT_THREAD_LOCK = threading.Lock()


@contextlib.contextmanager
def port_reservation(timeout):
    """Holds the port lock, waiting at most `timeout` seconds for it. A holder
    that is stopped (SIGSTOP, a debugger) then delays the others but cannot block
    them forever: past the timeout we go on without the lock, and a port taken
    under us shows up as a server that exits, which `Server` retries."""
    deadline = time.monotonic() + timeout
    got_thread = _PORT_THREAD_LOCK.acquire(timeout=max(0.0, timeout))
    f = None
    try:
        f = open(PORT_LOCK_PATH, "a")
        locked = False
        while True:
            try:
                fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
                locked = True
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    break
                time.sleep(0.05)
        if not (got_thread and locked):
            print(f"  port lock {PORT_LOCK_PATH} still held after {timeout:.0f}s; going on without it",
                  file=sys.stderr)
        try:
            yield
        finally:
            if locked:
                fcntl.flock(f, fcntl.LOCK_UN)
    finally:
        if f:
            f.close()
        if got_thread:
            _PORT_THREAD_LOCK.release()


class Stopped(BaseException):
    """Raised by calls made after a parallel run was interrupted. A
    BaseException, so the journeys' `except Exception` records no row for it."""


# Set when a parallel run is interrupted: units still running stop at their next call.
STOP = threading.Event()


def run_units(units, args, jobs=None):
    """Runs independent zero-argument callables and returns their results in
    unit order: in order when --jobs is 1 (the default), else on a pool of
    --jobs threads against the same server. Each unit owns its session and
    writes its own rows, so the rows are the same in either mode up to order
    and latency. After a unit raises, units not started yet are dropped and the
    first error (in unit order) is raised once the running ones finish, as
    sequential mode stops at it. On Ctrl-C (or any BaseException in the
    waiting thread) queued units are cancelled, running ones stop at their next
    call (`STOP`), and the interrupt is raised at once."""
    if jobs is None:
        jobs = max(1, int(getattr(args, "jobs", 1) or 1))
    if jobs == 1 or len(units) <= 1:
        return [unit() for unit in units]
    pool = ThreadPoolExecutor(max_workers=min(jobs, len(units)), thread_name_prefix="bench-unit")
    futures = [pool.submit(unit) for unit in units]
    try:
        # A timed wait, so a signal reaches this thread even where a blocking wait would not wake for it.
        pending = futures
        while pending:
            done, pending = wait(pending, timeout=0.5, return_when=FIRST_EXCEPTION)
            if any(not f.cancelled() and f.exception() is not None for f in done):
                break
    except BaseException:
        STOP.set()
        pool.shutdown(wait=False, cancel_futures=True)
        raise
    pool.shutdown(wait=True, cancel_futures=True)
    for f in futures:
        if not f.cancelled() and f.exception() is not None:
            raise f.exception()
    return [f.result() for f in futures]


def slug_of(model):
    return model.replace("/", "__")


def session_slug(model):
    """`slug_of(model)` with every character a header value may not carry
    unquoted (such as the `@` of a variant like `x/y@nojson`) as `-`."""
    return re.sub(r"[^A-Za-z0-9_.-]", "-", slug_of(model))


class Sink:
    """Appends rows to <out>/server.jsonl; safe across concurrent processes
    (flock) and across the threads of --jobs (a lock per sink)."""

    def __init__(self, out, model, suite):
        self.path = os.path.join(out, "server.jsonl")
        self.model, self.suite = model, suite
        self.rows = []
        self.lock = threading.Lock()

    def add(self, case, rep, ok, latency_ms, source, detail, ttfe_ms=None):
        row = {"model": self.model, "suite": self.suite, "case": case, "rep": rep, "ok": bool(ok),
               "latency_ms": None if latency_ms is None else round(latency_ms, 1)}
        if ttfe_ms is not None or self.suite == "sse":
            row["ttfe_ms"] = None if ttfe_ms is None else round(ttfe_ms, 1)
        row["source"] = source
        row["detail"] = detail
        line = (json.dumps(row, ensure_ascii=False) + "\n").encode()
        mark = "ok " if ok else "BAD"
        lat = "-" if latency_ms is None else f"{latency_ms / 1000:.1f}s"
        with self.lock:
            with open(self.path, "ab") as f:
                fcntl.flock(f, fcntl.LOCK_EX)
                try:
                    f.write(line)
                    f.flush()
                finally:
                    fcntl.flock(f, fcntl.LOCK_UN)
            self.rows.append(row)
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


class ServerExited(RuntimeError):
    """The server process ended before it answered /readyz."""


def wait_ready(admin, proc, timeout):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        if proc.poll() is not None:
            raise ServerExited(f"server exited with {proc.returncode}")
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
    if STOP.is_set():
        raise Stopped(url)
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


# A fallback answer the provider caused, not the model: the LLM call timed out (but not
# a wait for a generation slot, which is the bench's own concurrency), the provider
# answered 5xx or 429 ("no capacity", rate limited), or the connection to it failed.
INFRA_REASON = re.compile(r"LLM request timed out|LLM returned HTTP (5\d\d|429)\b|LLM transport error")


def infra_reason(last):
    """The fallback reason of `last` (a `Journey.last`) when the mock's answer was a
    fallback forced by a provider error or timeout, else None."""
    reason = str(last.get("fallback_reason") or "")
    if last.get("source") == "fallback" and INFRA_REASON.search(reason) and "generation slot" not in reason:
        return reason
    return None


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
    # Starts tried when the server exits before it is ready (such as a port taken
    # by a process outside the bench between picking and binding it).
    START_ATTEMPTS = 3

    def __init__(self, args, suite, mocks):
        self.args, self.suite, self.mocks = args, suite, mocks
        self.dir = tempfile.mkdtemp(prefix=f"mb-bench-{suite}-")
        self.base_url = f"{args.proxy_base.rstrip('/')}/r/{slug_of(args.model)}~{suite}/api/v1"
        logs = os.path.join(args.out, "server-logs")
        os.makedirs(logs, exist_ok=True)
        self.log_path = os.path.join(logs, f"{slug_of(args.model)}~{suite}.log")
        self.proc = None
        self.log = None

    def use_ports(self, ports):
        self.ports = ports
        self.config = write_config(self.dir, self.args.model, self.base_url, ports, self.mocks, self.suite)
        self.data = f"http://127.0.0.1:{ports[0]}"
        self.admin = f"http://127.0.0.1:{ports[1]}"
        self.api = f"{self.admin}/api/v1/workspaces/default"

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
        for attempt in range(self.START_ATTEMPTS):
            try:
                # The lock waits as long as its holder may take to get ready, and a margin.
                with port_reservation(self.args.ready_timeout + 30):
                    self.use_ports(free_ports(3))
                    self.proc = subprocess.Popen([self.args.binary, "serve", "-c", self.config], cwd=self.dir,
                                                 stdout=self.log, stderr=self.log, env=env)
                    wait_ready(self.admin, self.proc, self.args.ready_timeout)
                return self
            except ServerExited:
                if attempt + 1 == self.START_ATTEMPTS:
                    self.__exit__(None, None, None)
                    raise
                self.log.write("\n--- bench: the server exited before it was ready; retrying on new ports\n")
                self.log.flush()
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
        if self.log:
            self.log.close()
        if self.args.keep:
            print(f"kept {self.dir}", file=sys.stderr)
        else:
            shutil.rmtree(self.dir, ignore_errors=True)


# ---------------------------------------------------------------- consistency

def run_consistency(srv, sink, args):
    stamp = int(time.time())
    run_units([functools.partial(consistency_rep, srv, sink, args, stamp, rep) for rep in range(args.repeat)], args)


def consistency_rep(srv, sink, args, stamp, rep):
    """One rep: its own session; its three checks share it, so they run in order."""
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
    if STOP.is_set():
        raise Stopped(url)
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
    # Each rep has its own session, and its source is looked up by that session.
    run_units([functools.partial(sse_rep, srv, sink, args, stamp, want, rep) for rep in range(args.repeat)], args)


def sse_rep(srv, sink, args, stamp, want, rep):
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
    # Sequential whatever --jobs says: the server runs import jobs on a small
    # worker pool, so concurrent jobs would queue there, and the queue wait would
    # count toward --import-timeout and the measured job time.
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


def any_order(values):
    """`values` sorted so that any mix of types (a missing id is None) sorts
    without raising; equal lists of values sort the same way."""
    return sorted(values, key=lambda v: (type(v).__name__, v if isinstance(v, str) else repr(v)))


def same_items(got, want):
    """Whether two lists hold the same values in any order. Unlike comparing
    `sorted()` lists, a None or mixed-type entry fails the check instead of raising."""
    return isinstance(got, list) and any_order(got) == any_order(want)


class Journey:
    """One journey run: a fresh session, calls through `request()`, and checks
    recorded as rows against the latest call.

    A failed check judged on a fallback the provider forced (`infra_reason`) carries
    `detail.infra` true. Rows are held back until the first check passes, so that a
    run which fails on the provider before any check passed can be dropped and run
    again (`run_one_journey`); after that they are written as they come."""

    def __init__(self, name, srv, sink, args, rep, retry_of=None):
        self.name, self.srv, self.sink, self.args, self.rep = name, srv, sink, args, rep
        session = f"bench-{session_slug(args.model)}-{int(time.time())}-{name}-{rep}"
        if retry_of:
            session += "-retry"
        self.headers = {"x-mockingbird-session": session}
        self.rng = random.Random(f"{args.model}|{name}|{rep}")
        self.done = set()
        self.last = {"status": None, "source": "?", "ms": None, "fallback_reason": None}
        self.retry_of = retry_of  # the infra reason that dropped the first run
        self.passed = False  # a check has passed: rows are no longer held back
        self.held = []
        self.infra = None  # the first infra reason a failed check was judged on
        self.last_infra = False  # the latest check failed on infra

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

    def check(self, name, ok, infra=None, **detail):
        self.done.add(name)
        detail = dict(detail, **{k: v for k, v in self.last.items() if k not in ("source", "ms") and v is not None})
        ok = bool(ok)
        if infra is None:
            infra = infra_reason(self.last)
        self.last_infra = bool(infra) and not ok
        if self.last_infra:
            detail["infra"] = True
            self.infra = self.infra or infra
        if self.retry_of:
            detail["retried_after"] = self.retry_of
        row = (self.sink, self.name, self.rep, name, ok, detail, self.last["source"], self.last["ms"])
        if self.passed:
            journey_check(*row)
            return
        self.held.append(row)
        if ok:
            self.passed = True
            self.flush()

    def flush(self):
        held, self.held = self.held, []
        for row in held:
            journey_check(*row)

    def stop(self, gate):
        self.check(gate, False)
        raise JourneyStop(gate)


def run_one_journey(name, steps, srv, sink, args, rep):
    """Runs a journey and records every one of its checks. A journey that ends early
    on a provider error or timeout (`infra_reason`) before any check passed is run once
    more from the start, in a fresh session; only the second run's rows are kept, and
    they name the first run's reason in `detail.retried_after`. Checks left out because
    the journey ended on such an error carry `detail.infra` too."""
    retry_of = None
    while True:
        j = Journey(name, srv, sink, args, rep, retry_of=retry_of)
        try:
            steps(j)
            reason = None
        except JourneyStop as e:
            reason = f"stopped at {e}"
        except Exception as e:  # noqa: BLE001 - a journey that breaks is recorded, not raised
            reason = f"{type(e).__name__}: {e}"[:300]
            if infra_reason(j.last):
                j.infra, j.last_infra = j.infra or infra_reason(j.last), True
        except BaseException:
            j.flush()  # an interrupt keeps the rows judged so far and pads none
            raise
        if reason and j.infra and not j.passed and retry_of is None:
            retry_of = j.infra
            print(f"  [re-run] {sink.suite}/{name} #{rep}: {j.infra[:120]}", file=sys.stderr)
            continue
        break
    ended_on_infra = bool(reason) and j.last_infra
    for check in JOURNEY_CHECKS[name]:
        if check not in j.done:
            j.last = {"status": None, "source": "skipped", "ms": None, "fallback_reason": None}
            j.check(check, False, infra=j.infra if ended_on_infra else False,
                    reason=reason or "journey ended early")
    j.flush()


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
    run_units([functools.partial(JOURNEYS[name], srv, sink, args, rep)
               for rep in range(args.repeat) for name in args.journey or DEFAULT_JOURNEYS], args)


def run_journey_table(table, srv, sink, args):
    """Every (rep, journey) of `table` as its own unit. Each `Journey` has its
    own session (`<model>-<stamp>-<journey>-<rep>`, unique within the suite's
    server), its own seeded rng and its own record of checks; the step
    functions keep no module state."""
    run_units([functools.partial(run_one_journey, name, steps, srv, sink, args, rep)
               for rep in range(args.repeat) for name, steps in table.items()], args)


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
    j.check("read keeps the labels", isinstance(fields.get("labels"), list) and same_items(fields["labels"], labels))
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
    j.check("read keeps the tags", isinstance(read.get("tags"), list) and same_items(read["tags"], tags))
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
    run_journey_table(HELDOUT_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- blind journeys

# These journeys measure the engine on contracts nobody tuned against. They were
# written from the contracts and from what each real API promises a client, with no
# look at the engine or at past results. A check that is hard for a mock stays; a
# check is left out only when its contract has no route for it or its schema rules the
# answer out, and a comment says so. Do not edit a check to suit results.
BLIND_CHECKS = {
    "spotify": [
        "search returned tracks", "search answers what was asked", "track detail keeps the listed track",
        "artist detail keeps the track's artist", "top tracks credit the artist",
        "playlist read names the playlist asked for", "add returns a new snapshot",
        "item list shows the new tracks first", "items carry the full track", "items name who added them",
        "insert lands at its position", "older path adds to the same playlist",
        "read shows the latest snapshot and count", "read keeps the playlist's details",
    ],
    "discord": [
        "guild read names the guild asked for", "channels name their guild", "message has an id",
        "message keeps what was sent", "author is the bot", "read matches the created message",
        "reply names the message it answers", "read keeps the reference", "list holds both, newest first",
        "after filter holds only the later message", "before filter holds only the earlier message",
        "deleted message is gone", "list drops the deleted message",
    ],
    "auth0": [
        "user has an id", "user keeps the profile", "user belongs to the connection", "read matches the created user",
        "password never comes back", "update changes the fields", "update merges the metadata", "read shows the update",
        "search finds the user as updated", "search misses another email", "lookup by email finds the user",
        "tenant offers a role", "user holds the assigned role", "deleted user is gone",
    ],
    "hubspot": [
        "company has an id", "company keeps its properties", "read matches the created company", "contact has an id",
        "contact keeps its properties", "read returns the properties asked for", "deal has an id",
        "read matches the created deal", "deal list holds the new deal", "update changes the contact",
        "read shows the update and keeps the rest", "search finds the contact in its new stage",
        "search by the old stage misses the contact", "archived contact is gone",
    ],
    "slack": [
        "auth names the bot", "channel list offers a channel", "channel list answers the filter",
        "user info keeps the listed bot", "post returns a timestamp", "message keeps text and author",
        "history holds the new message", "history lists newest first", "oldest filter holds only the later message",
        "latest filter holds only the earlier message", "reply names its thread and stays out of the channel",
        "parent counts the reply", "reaction shows on the message", "a repeated reaction is refused",
    ],
}
JOURNEY_CHECKS.update(BLIND_CHECKS)


def spotify_uris(page):
    return [obj(item.get("track")).get("uri") for item in dicts(page.get("items"))]


def spotify_steps(j):
    # A playlist the client was given: the contract has no route to create one, and
    # none to update its details, remove an item or delete anything. Item lists are read
    # from their start and inserts go there too, because its Paging schema pins offset to 0.
    j.headers.update(HELDOUT_AUTH)
    term = "love"
    found = obj(j.call("GET", "/v1/search", query={"q": term, "type": "track,artist", "market": "DE", "limit": 5}))
    track_page, artist_page = obj(found.get("tracks")), obj(found.get("artists"))
    tracks = dicts(track_page.get("items"))
    need(j, "search returned tracks", len(tracks) >= 3 and all(t.get("id") and t.get("uri") for t in tracks))
    listed_artists = artist_page.get("items")
    j.check("search answers what was asked",
            eq(track_page.get("limit"), 5) and eq(artist_page.get("limit"), 5) and isinstance(listed_artists, list)
            and "albums" not in found and term in json.dumps([tracks, listed_artists]).lower())
    first = tracks[0]
    track = obj(j.call("GET", f"/v1/tracks/{first['id']}", query={"market": "DE"}))
    j.check("track detail keeps the listed track",
            same(track, first, "id", "name", "uri", "duration_ms") and same(obj(track.get("album")), obj(first.get("album")), "id", "name"))
    credit = head(first.get("artists"))
    artist = obj(j.call("GET", f"/v1/artists/{credit.get('id')}"))
    j.check("artist detail keeps the track's artist", same(artist, credit, "id", "name", "uri"))
    top = dicts(obj(j.call("GET", f"/v1/artists/{credit.get('id')}/top-tracks", query={"market": "DE"})).get("tracks"))
    j.check("top tracks credit the artist",
            bool(top) and credit.get("id") is not None
            and all(by_id(t.get("artists"), credit.get("id")) for t in top), listed=len(top))
    me = obj(j.call("GET", "/v1/me"))
    alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
    pid = "".join(j.rng.choice(alphabet) for _ in range(22))
    path = f"/v1/playlists/{pid}"
    before = obj(j.call("GET", path))
    need(j, "playlist read names the playlist asked for",
         eq(before.get("id"), pid) and eq(before.get("uri"), f"spotify:playlist:{pid}") and before.get("snapshot_id"))
    count = obj(before.get("tracks")).get("total")

    def grown(page, by):
        return isinstance(count, int) and eq(page.get("total"), count + by)

    pair = [tracks[0]["uri"], tracks[1]["uri"]]
    added = obj(j.call("POST", f"{path}/items", body={"uris": pair, "position": 0}))
    j.check("add returns a new snapshot",
            j.last["status"] == 201 and added.get("snapshot_id") and added["snapshot_id"] != before.get("snapshot_id"),
            snapshot=added.get("snapshot_id"))
    page = obj(j.call("GET", f"{path}/items", query={"limit": 5}))
    j.check("item list shows the new tracks first", spotify_uris(page)[:2] == pair and grown(page, 2),
            uris=spotify_uris(page), total=page.get("total"), before=count)
    items = dicts(page.get("items"))
    j.check("items carry the full track",
            same(obj(head(items).get("track")), first, "id", "name", "uri", "duration_ms"))
    j.check("items name who added them",
            len(items) >= 2 and all(eq(obj(i.get("added_by")).get("id"), me.get("id")) and i.get("added_at") for i in items[:2]))
    inserted = tracks[2]["uri"]
    j.call("POST", f"{path}/items", body={"uris": [inserted], "position": 1})
    page = obj(j.call("GET", f"{path}/items", query={"limit": 5}))
    j.check("insert lands at its position", spotify_uris(page)[:3] == [pair[0], inserted, pair[1]], uris=spotify_uris(page))
    older = head(top).get("uri") or pair[0]  # the same track twice is allowed in a playlist
    last = obj(j.call("POST", f"{path}/tracks", body={"uris": [older], "position": 0}))
    page = obj(j.call("GET", f"{path}/items", query={"limit": 5}))
    j.check("older path adds to the same playlist",
            spotify_uris(page)[:4] == [older, pair[0], inserted, pair[1]], uris=spotify_uris(page))
    read = obj(j.call("GET", path))
    j.check("read shows the latest snapshot and count",
            eq(read.get("snapshot_id"), last.get("snapshot_id")) and grown(obj(read.get("tracks")), 4),
            total=obj(read.get("tracks")).get("total"), before=count)
    j.check("read keeps the playlist's details",
            same(read, before, "id", "uri", "name", "owner", "public", "collaborative"))


def discord_steps(j):
    # A guild the client was given: the contract has no route to create one or to list the
    # bot's guilds. It has no route to edit or pin a message either. Its interaction callback
    # is left out: it needs an interaction from the gateway and answers 204, so nothing
    # shows its effect. Usernames are not compared: UserResponse's pattern rules out the
    # const username of UserPIIResponse.
    j.headers["authorization"] = "Bot bench-placeholder"
    me = obj(j.call("GET", "/users/@me"))
    gid = str(1200000000000000000 + j.rng.randrange(10 ** 17))
    guild = obj(j.call("GET", f"/guilds/{gid}", query={"with_counts": "true"}))
    j.check("guild read names the guild asked for",
            eq(guild.get("id"), gid) and isinstance(guild.get("approximate_member_count"), int))
    channels = dicts(j.call("GET", f"/guilds/{gid}/channels"))
    j.check("channels name their guild", bool(channels) and all(eq(c.get("guild_id"), gid) for c in channels))
    # The contract does not promise the guild a text channel (types are an enum and it has
    # no route to make one), so a listed text channel is used when there is one, and else a
    # DM channel the client was given: `create_message` posts to either.
    channel = next((c for c in channels if c.get("type") == 0), {})
    cid = channel.get("id") or str(1100000000000000000 + j.rng.randrange(10 ** 17))
    messages = f"/channels/{cid}/messages"
    content, title = f"Doors open at {j.rng.randint(17, 20)}:30 tonight.", "Meetup reminder"
    nonce = f"bench-{j.rng.randint(100000, 999999)}"
    first = obj(j.call("POST", messages, body={
        "content": content, "nonce": nonce, "embeds": [{"title": title, "description": "Bring a laptop."}],
    }))
    mid = first.get("id")
    need(j, "message has an id", mid)
    embed = head(first.get("embeds"))
    j.check("message keeps what was sent",
            eq(first.get("content"), content) and eq(first.get("channel_id"), cid) and eq(first.get("nonce"), nonce)
            and eq(embed.get("title"), title) and eq(embed.get("description"), "Bring a laptop."))
    j.check("author is the bot", eq(obj(first.get("author")).get("id"), me.get("id")))
    read = obj(j.call("GET", f"{messages}/{mid}"))
    j.check("read matches the created message",
            same(read, first, "id", "content", "channel_id", "timestamp", "embeds")
            and eq(obj(read.get("author")).get("id"), me.get("id")) and read.get("pinned") is False
            and "edited_timestamp" in read and read["edited_timestamp"] is None)
    answer = "Will the talks be recorded?"
    reply = obj(j.call("POST", messages, body={"content": answer, "message_reference": {"message_id": mid}}))
    rid = reply.get("id")
    j.check("reply names the message it answers",
            eq(reply.get("type"), 19) and eq(reply.get("content"), answer)
            and eq(obj(reply.get("message_reference")).get("message_id"), mid))
    read2 = obj(j.call("GET", f"{messages}/{rid}"))
    j.check("read keeps the reference",
            eq(read2.get("id"), rid) and eq(obj(read2.get("message_reference")).get("message_id"), mid))

    def ids(**query):
        return [m.get("id") for m in dicts(j.call("GET", messages, query=query))]

    listed = ids(limit=50)
    j.check("list holds both, newest first",
            rid in listed and mid in listed and listed.index(rid) < listed.index(mid), listed=listed)
    later = ids(after=mid)
    j.check("after filter holds only the later message", rid in later and mid not in later, listed=later)
    earlier = ids(before=rid)
    j.check("before filter holds only the earlier message", mid in earlier and rid not in earlier, listed=earlier)
    j.call("DELETE", f"{messages}/{mid}")
    deleted = j.last["status"]
    j.call("GET", f"{messages}/{mid}")
    j.check("deleted message is gone", deleted == 204 and j.last["status"] == 404, delete_status=deleted)
    left = ids(limit=50)
    j.check("list drops the deleted message", rid in left and mid not in left, listed=left)


def auth0_user(users, wanted):
    return next((u for u in dicts(users) if eq(u.get("user_id"), wanted)), {})


def auth0_steps(j):
    # The contract has no route to create a role, so the role comes from the tenant's list.
    j.headers.update(HELDOUT_AUTH)
    connection = "Username-Password-Authentication"
    tag = j.rng.randint(1000, 9999)
    email, name = f"ana.silva{tag}@example.org", "Ana Silva"
    user_meta = {"plan": "team"}
    app_meta = {"region": "eu"}
    created = obj(j.call("POST", "/api/v2/users", body={
        "connection": connection, "email": email, "password": f"Bench-Placeholder-{tag}!", "name": name,
        "given_name": "Ana", "family_name": "Silva", "user_metadata": user_meta, "app_metadata": app_meta,
    }))
    uid = created.get("user_id")
    need(j, "user has an id", uid)
    path = f"/api/v2/users/{urllib.parse.quote(str(uid), safe='')}"  # the id holds a `|`
    j.check("user keeps the profile",
            eq(created.get("email"), email) and eq(created.get("name"), name)
            and eq(created.get("given_name"), "Ana") and eq(created.get("family_name"), "Silva"))
    identity = head(created.get("identities"))
    j.check("user belongs to the connection",
            eq(identity.get("connection"), connection) and eq(identity.get("provider"), "auth0")
            and eq(f"auth0|{identity.get('user_id')}", uid), identity=identity)
    read = obj(j.call("GET", path))
    j.check("read matches the created user",
            same(read, created, "user_id", "email", "name", "given_name", "family_name", "created_at", "identities")
            and eq(read.get("user_metadata"), user_meta) and eq(read.get("app_metadata"), app_meta))
    j.check("password never comes back", bool(read) and "password" not in created and "password" not in read)
    updated = obj(j.call("PATCH", path, body={"blocked": True, "given_name": "Anabela", "user_metadata": {"locale": "de"}}))
    merged = dict(user_meta, locale="de")
    j.check("update changes the fields",
            eq(updated.get("user_id"), uid) and updated.get("blocked") is True and eq(updated.get("given_name"), "Anabela"))
    j.check("update merges the metadata",
            eq(updated.get("user_metadata"), merged) and eq(updated.get("app_metadata"), app_meta),
            user_metadata=updated.get("user_metadata"))
    read2 = obj(j.call("GET", path))
    j.check("read shows the update",
            read2.get("blocked") is True and eq(read2.get("given_name"), "Anabela") and eq(read2.get("family_name"), "Silva")
            and eq(read2.get("user_metadata"), merged) and eq(read2.get("created_at"), created.get("created_at"))
            and read2.get("updated_at") is not None and read2["updated_at"] != created.get("updated_at"),
            updated_at=read2.get("updated_at"))
    found = obj(j.call("GET", "/api/v2/users", query={"q": f'email:"{email}"', "search_engine": "v3", "include_totals": "true"}))
    hit = auth0_user(found.get("users"), uid)
    j.check("search finds the user as updated",
            eq(hit.get("email"), email) and hit.get("blocked") is True and eq(hit.get("given_name"), "Anabela")
            and eq(found.get("total"), 1), total=found.get("total"))
    missed = obj(j.call("GET", "/api/v2/users",
                        query={"q": f'email:"nobody{tag}@example.org"', "search_engine": "v3", "include_totals": "true"}))
    j.check("search misses another email", missed.get("users") == [] and eq(missed.get("total"), 0), total=missed.get("total"))
    by_email = j.call("GET", "/api/v2/users-by-email", query={"email": email})
    j.check("lookup by email finds the user", eq(auth0_user(by_email, uid).get("email"), email))
    role = head(obj(j.call("GET", "/api/v2/roles", query={"include_totals": "true"})).get("roles"))
    need(j, "tenant offers a role", role.get("id"))
    held = obj(j.call("GET", f"{path}/roles", query={"include_totals": "true"}))
    j.call("POST", f"{path}/roles", body={"roles": [role["id"]]})
    assigned = j.last["status"]
    after = obj(j.call("GET", f"{path}/roles", query={"include_totals": "true"}))
    j.check("user holds the assigned role",
            eq(held.get("total"), 0) and assigned == 204 and eq(after.get("total"), 1)
            and same(by_id(after.get("roles"), role["id"]), role, "id", "name"),
            before=held.get("total"), after=after.get("total"), assign_status=assigned)
    j.call("DELETE", path)
    deleted = j.last["status"]
    j.call("GET", path)
    gone = j.last["status"]
    left = j.call("GET", "/api/v2/users-by-email", query={"email": email})
    j.check("deleted user is gone", deleted == 204 and gone == 404 and left == [],
            delete_status=deleted, read_status=gone)


def hubspot_props(thing):
    return obj(thing.get("properties"))


def hubspot_search(j, *filters):
    query = {"filterGroups": [{"filters": [{"propertyName": p, "operator": "EQ", "value": v} for p, v in filters]}],
             "properties": ["email", "firstname", "lastname", "lifecyclestage"], "limit": 10}
    return obj(j.call("POST", "/crm/v3/objects/contacts/search", body=query))


def hubspot_steps(j):
    # The contract has no route or parameter to read associations back, so a deal's links
    # are sent as a client would send them and the references checked are its properties.
    # It has no route to update or delete a company or a deal. An archived contact is not
    # read with archived=true: the Contact schema pins archived to false.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randint(1000, 9999)
    made = {"name": f"Fjord Logistics {tag}", "domain": f"fjord{tag}.example.org",
            "industry": "LOGISTICS_AND_SUPPLY_CHAIN", "city": "Oslo"}
    company = obj(j.call("POST", "/crm/v3/objects/companies", body={"properties": made}))
    company_id = company.get("id")
    need(j, "company has an id", company_id)
    j.check("company keeps its properties", all(eq(hubspot_props(company).get(k), v) for k, v in made.items()))
    got = obj(j.call("GET", f"/crm/v3/objects/companies/{company_id}", query={"properties": ",".join(made)}))
    j.check("read matches the created company",
            eq(got.get("id"), company_id) and all(eq(hubspot_props(got).get(k), v) for k, v in made.items()))
    owner = "52550153"
    email = f"ana.silva{tag}@example.org"
    person = {"email": email, "firstname": "Ana", "lastname": "Silva", "phone": f"+1 617 555 0{tag % 1000:03d}",
              "company": made["name"], "jobtitle": "Buyer", "lifecyclestage": "lead", "hubspot_owner_id": owner}

    def link(to, kind):
        return {"to": {"id": to}, "types": [{"associationCategory": "HUBSPOT_DEFINED", "associationTypeId": kind}]}

    contact = obj(j.call("POST", "/crm/v3/objects/contacts", body={"properties": person, "associations": [link(company_id, 1)]}))
    contact_id = contact.get("id")
    need(j, "contact has an id", contact_id)
    path = f"/crm/v3/objects/contacts/{contact_id}"
    j.check("contact keeps its properties", all(eq(hubspot_props(contact).get(k), v) for k, v in person.items()))
    # HubSpot returns only a default set unless the client names the properties it wants.
    asked = {"properties": ",".join(person)}
    read = obj(j.call("GET", path, query=asked))
    j.check("read returns the properties asked for",
            eq(read.get("id"), contact_id) and all(eq(hubspot_props(read).get(k), v) for k, v in person.items()))
    sale = {"dealname": f"Fjord renewal {tag}", "amount": f"{j.rng.randint(2, 90)}000", "dealstage": "qualifiedtobuy",
            "pipeline": "default", "closedate": "2026-12-31T00:00:00.000Z", "hubspot_owner_id": owner}
    deal = obj(j.call("POST", "/crm/v3/objects/deals",
                      body={"properties": sale, "associations": [link(contact_id, 3), link(company_id, 5)]}))
    deal_id = deal.get("id")
    need(j, "deal has an id", deal_id)
    got_deal = obj(j.call("GET", f"/crm/v3/objects/deals/{deal_id}", query={"properties": ",".join(sale)}))
    j.check("read matches the created deal",
            eq(got_deal.get("id"), deal_id) and all(eq(hubspot_props(got_deal).get(k), v) for k, v in sale.items()))
    deals = obj(j.call("GET", "/crm/v3/objects/deals", query={"limit": 100, "properties": ",".join(sale)}))
    j.check("deal list holds the new deal",
            all(eq(hubspot_props(by_id(deals.get("results"), deal_id)).get(k), sale[k]) for k in ("dealname", "amount")))
    updated = obj(j.call("PATCH", path, body={"properties": {"lifecyclestage": "opportunity", "jobtitle": "Head of buying"}}))
    j.check("update changes the contact",
            eq(updated.get("id"), contact_id) and eq(hubspot_props(updated).get("lifecyclestage"), "opportunity")
            and eq(hubspot_props(updated).get("jobtitle"), "Head of buying"))
    read2 = obj(j.call("GET", path, query=asked))
    props2 = hubspot_props(read2)
    j.check("read shows the update and keeps the rest",
            eq(props2.get("lifecyclestage"), "opportunity") and eq(props2.get("jobtitle"), "Head of buying")
            and all(eq(props2.get(k), person[k]) for k in ("email", "firstname", "lastname", "phone", "company"))
            and same(read2, read, "id", "createdAt"), stage=props2.get("lifecyclestage"))
    found = hubspot_search(j, ("email", email), ("lifecyclestage", "opportunity"))
    j.check("search finds the contact in its new stage",
            eq(hubspot_props(by_id(found.get("results"), contact_id)).get("lastname"), "Silva") and eq(found.get("total"), 1),
            total=found.get("total"))
    stale = hubspot_search(j, ("email", email), ("lifecyclestage", "lead"))
    j.check("search by the old stage misses the contact",
            stale.get("results") == [] and eq(stale.get("total"), 0), total=stale.get("total"))
    j.call("DELETE", path)
    archived = j.last["status"]
    j.call("GET", path)
    gone = j.last["status"]
    after = hubspot_search(j, ("email", email))
    j.check("archived contact is gone",
            archived == 204 and gone == 404 and after.get("results") == [] and eq(after.get("total"), 0),
            archive_status=archived, read_status=gone, total=after.get("total"))


def slack_history(j, channel, **query):
    return dicts(obj(j.call("GET", "/conversations.history", query=dict(query, channel=channel))).get("messages"))


def slack_message(messages, ts):
    return next((m for m in messages if eq(m.get("ts"), ts)), {})


def slack_stamps(messages):
    return [m.get("ts") for m in messages]


def slack_steps(j):
    # The contract has no route to edit or delete a message, to read a thread or to create
    # a channel, so a reply's effect is read on its parent and the channel comes from the list.
    j.headers["authorization"] = "Bearer xoxb-bench-placeholder"
    who = obj(j.call("GET", "/auth.test"))
    bot = who.get("user_id")
    need(j, "auth names the bot", who.get("ok") is True and bot)
    listed = obj(j.call("GET", "/conversations.list",
                        query={"exclude_archived": "true", "types": "public_channel", "limit": 100}))
    channels = dicts(listed.get("channels"))
    channel = next((c for c in channels if c.get("is_member") is True), head(channels))
    cid = channel.get("id")
    need(j, "channel list offers a channel", cid)
    j.check("channel list answers the filter",
            all(c.get("is_private") is False and c.get("is_archived") is False for c in channels))
    members = obj(j.call("GET", "/users.list", query={"limit": 100}))
    member = by_id(members.get("members"), bot)
    info = obj(obj(j.call("GET", "/users.info", query={"user": bot})).get("user"))
    j.check("user info keeps the listed bot",
            same(info, member, "id", "name", "real_name", "team_id") and info.get("is_bot") is True)

    def post(**fields):
        return obj(j.call("POST", "/chat.postMessage", body=dict(fields, channel=cid)))

    text = f"Deploy {j.rng.randint(1000, 9999)} is rolling out."
    made = post(text=text)
    ts = made.get("ts")
    need(j, "post returns a timestamp", made.get("ok") is True and ts)
    message = obj(made.get("message"))
    j.check("message keeps text and author",
            eq(made.get("channel"), cid) and eq(message.get("text"), text) and eq(message.get("ts"), ts)
            and eq(message.get("user"), bot))
    j.check("history holds the new message", eq(slack_message(slack_history(j, cid), ts).get("text"), text))
    later = post(text="Deploy finished.")
    ts2 = later.get("ts")
    stamps = slack_stamps(slack_history(j, cid))
    j.check("history lists newest first",
            ts2 is not None and ts2 in stamps and ts in stamps and stamps.index(ts2) < stamps.index(ts), stamps=stamps)
    # Both bounds are exclusive unless inclusive is set.
    newer = slack_stamps(slack_history(j, cid, oldest=ts))
    j.check("oldest filter holds only the later message", ts2 in newer and ts not in newer, stamps=newer)
    older = slack_stamps(slack_history(j, cid, latest=ts2))
    j.check("latest filter holds only the earlier message", ts in older and ts2 not in older, stamps=older)
    reply = post(text="Rollback plan is ready.", thread_ts=ts)
    channel_view = slack_stamps(slack_history(j, cid))
    j.check("reply names its thread and stays out of the channel",
            eq(obj(reply.get("message")).get("thread_ts"), ts) and reply.get("ts") is not None
            and reply["ts"] not in channel_view, stamps=channel_view)
    # A client reads one message by bounding history to it.
    parent = slack_message(slack_history(j, cid, latest=ts, inclusive="true", limit=1), ts)
    j.check("parent counts the reply", eq(parent.get("reply_count"), 1) and eq(parent.get("thread_ts"), ts),
            reply_count=parent.get("reply_count"))
    react = {"channel": cid, "name": "eyes", "timestamp": ts}
    reacted = obj(j.call("POST", "/reactions.add", body=react))
    shown = slack_message(slack_history(j, cid, latest=ts, inclusive="true", limit=1), ts)
    reaction = next((r for r in dicts(shown.get("reactions")) if r.get("name") == "eyes"), {})
    j.check("reaction shows on the message",
            reacted.get("ok") is True and eq(reaction.get("count"), 1) and bot in (reaction.get("users") or [])
            and eq(shown.get("text"), text), reaction=reaction)
    again = obj(j.call("POST", "/reactions.add", body=react))
    j.check("a repeated reaction is refused", again.get("ok") is False and eq(again.get("error"), "already_reacted"),
            slack_ok=again.get("ok"), slack_error=again.get("error"))


BLIND_JOURNEYS = {"spotify": spotify_steps, "discord": discord_steps, "auth0": auth0_steps,
                  "hubspot": hubspot_steps, "slack": slack_steps}


def run_journeys_blind(srv, sink, args):
    run_journey_table(BLIND_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- second blind journeys

# The same rule as the blind journeys, over five more contracts: written from each
# contract and from what its API promises a client, with no look at the engine or at
# past results. No check gates the rest: a journey goes on with what it has, using an
# id of its own when a call gave none. A check is left out only when its contract has
# no route for it or its schema rules the answer out, and a comment says so. Do not
# edit a check to suit results.
BLIND2_CHECKS = {
    "twilio": [
        "message has a sid", "message keeps what was sent", "message is queued", "read matches the created message",
        "list holds both, newest first", "To filter holds only that recipient", "From filter holds only that sender",
        "page size caps the list", "redaction empties the body", "read shows the redaction and keeps the rest",
        "deleted message is gone", "list drops the deleted message", "read matches the created call",
        "call list answers the filter",
    ],
    "sendgrid": [
        "template has an id", "template keeps its name and generation", "read matches the created template",
        "generation defaults to legacy", "template list holds both", "generation filter holds only that generation",
        "page size caps the template list", "mail using the template is accepted", "stats cover each day asked for",
        "bounce read keeps the listed bounce", "email filter holds only that address", "removing a bounce answers 204",
        "removed address has no bounce", "bounce list drops the address",
    ],
    "plaid": [
        "public token is issued", "exchange returns an access token", "item reads back as exchanged",
        "item keeps its webhook", "accounts name their item", "balances cover the same accounts",
        "account filter holds only the asked account", "auth accounts belong to the item",
        "auth numbers cover checking and savings", "identity accounts belong to the item", "count caps the sync",
        "transactions name the item's accounts", "next sync repeats nothing", "institution answers the id asked for",
    ],
    "paypalpayments": [
        "capture read names the capture asked for", "a second read repeats the capture", "refund has an id",
        "refund keeps the amount asked for", "refund keeps the invoice and note", "read matches the created refund",
        "capture shows the partial refund", "capture keeps its amount", "second refund has its own id",
        "second refund keeps its amount", "capture shows the full refund", "first refund still reads as it was",
        "a fully refunded capture refuses another", "authorization read names the authorization asked for",
    ],
    "openweathermap": [
        "geocode finds the city asked for", "limit caps the geocode matches", "unknown place matches nothing",
        "weather keeps the coordinates asked for", "city id read names the same city",
        "zip lookup keeps the zip asked for", "zip lands near the city", "reverse lookup stays in the country",
        "forecast holds the steps asked for", "forecast steps three hours apart",
        "forecast names the same city as the weather", "one call keeps the coordinates",
        "one call leaves out what was excluded", "air pollution keeps the coordinates",
    ],
}
JOURNEY_CHECKS.update(BLIND2_CHECKS)


def matches(pattern, value):
    return isinstance(value, str) and re.fullmatch(pattern, value) is not None


def is_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def near(a, b, tolerance):
    return is_number(a) and is_number(b) and abs(a - b) <= tolerance


def hex_id(j, prefix):
    return prefix + "".join(j.rng.choice("0123456789abcdef") for _ in range(32))


def by_key(items, key, wanted):
    return next((i for i in dicts(items) if eq(i.get(key), wanted)), {})


def twilio_steps(j):
    # List schemas pin page_size to 50 and next_page_uri to null, so a page's own paging
    # fields are not checked, only the rows PageSize asked for. Scheduling a message needs
    # a messaging service nothing here creates, so canceling one is left out, and the
    # contract has no route to change a call.
    account = hex_id(j, "AC")
    j.headers["authorization"] = "Basic " + base64.b64encode(f"{account}:bench-placeholder".encode()).decode()
    messages, calls = f"/2010-04-01/Accounts/{account}/Messages", f"/2010-04-01/Accounts/{account}/Calls"
    to_a, to_b, from_a, from_b = "+14155550101", "+14155550102", "+14155550103", "+14155550104"
    text = "Your table for two is ready."

    def page_of(**query):
        return dicts(obj(j.call("GET", f"{messages}.json", query=query)).get("messages"))

    def sids(rows):
        return [m.get("sid") for m in rows]

    first = obj(j.call("POST", f"{messages}.json", form={"To": to_a, "From": from_a, "Body": text}))
    j.check("message has a sid", j.last["status"] == 201 and matches(r"SM[0-9a-f]{32}", first.get("sid")),
            sid=first.get("sid"))
    j.check("message keeps what was sent",
            eq(first.get("to"), to_a) and eq(first.get("from"), from_a) and eq(first.get("body"), text))
    j.check("message is queued", eq(first.get("status"), "queued"), message_status=first.get("status"))
    sid_a = first.get("sid") or hex_id(j, "SM")
    read = obj(j.call("GET", f"{messages}/{sid_a}.json"))
    j.check("read matches the created message", same(read, first, "sid", "account_sid", "to", "from", "body"))
    time.sleep(1.1)  # dates have whole seconds: "newest first" needs two different ones
    second = obj(j.call("POST", f"{messages}.json", form={"To": to_b, "From": from_b, "Body": "A second message."}))
    sid_b = second.get("sid") or hex_id(j, "SM")
    listed = sids(page_of())
    j.check("list holds both, newest first",
            sid_a in listed and sid_b in listed and listed.index(sid_b) < listed.index(sid_a), listed=listed)
    rows = page_of(To=to_a)
    j.check("To filter holds only that recipient", sid_a in sids(rows) and all(eq(m.get("to"), to_a) for m in rows),
            listed=sids(rows))
    rows = page_of(From=from_b)
    j.check("From filter holds only that sender", sid_b in sids(rows) and all(eq(m.get("from"), from_b) for m in rows),
            listed=sids(rows))
    rows = page_of(PageSize=1)
    j.check("page size caps the list", len(rows) == 1, listed=sids(rows))
    redacted = obj(j.call("POST", f"{messages}/{sid_a}.json", form={"Body": ""}))
    j.check("redaction empties the body", j.last["status"] == 200 and redacted.get("body") == "")
    read = obj(j.call("GET", f"{messages}/{sid_a}.json"))
    j.check("read shows the redaction and keeps the rest",
            read.get("body") == "" and same(read, first, "sid", "to", "from"))
    j.call("DELETE", f"{messages}/{sid_a}.json")
    deleted = j.last["status"]
    j.call("GET", f"{messages}/{sid_a}.json")
    j.check("deleted message is gone", j.last["status"] == 404, delete_status=deleted)
    listed = sids(page_of())
    j.check("list drops the deleted message", sid_b in listed and sid_a not in listed, listed=listed)
    dialled = "+14155550105"
    call = obj(j.call("POST", f"{calls}.json", form={"To": dialled, "From": from_a, "Url": "https://example.com/twiml"}))
    call_sid = call.get("sid") or hex_id(j, "CA")
    read = obj(j.call("GET", f"{calls}/{call_sid}.json"))
    j.check("read matches the created call", eq(read.get("to"), dialled) and same(read, call, "sid", "to", "from"))
    rows = dicts(obj(j.call("GET", f"{calls}.json", query={"To": dialled})).get("calls"))
    j.check("call list answers the filter",
            bool(by_key(rows, "sid", call_sid)) and all(eq(c.get("to"), dialled) for c in rows), listed=len(rows))


def sendgrid_steps(j):
    # No route updates or deletes a template, and none adds a bounce: the journey removes
    # a bounce it finds listed. `_metadata.self` is pinned to one page size, so it is not
    # checked. Stats are not tied to the mail sent: nothing says when a send is counted.
    j.headers.update(HELDOUT_AUTH)
    both = "legacy,dynamic"
    tag = j.rng.randrange(10**6, 10**7)

    def templates(**query):
        return dicts(obj(j.call("GET", "/v3/templates", query=query)).get("result"))

    name = f"Order receipt {tag}"
    made = obj(j.call("POST", "/v3/templates", body={"name": name, "generation": "dynamic"}))
    j.check("template has an id", j.last["status"] == 201 and matches(r"d-[0-9a-f]{32}", made.get("id")), id=made.get("id"))
    j.check("template keeps its name and generation", eq(made.get("name"), name) and eq(made.get("generation"), "dynamic"))
    tid = made.get("id") or hex_id(j, "d-")
    read = obj(j.call("GET", f"/v3/templates/{tid}"))
    j.check("read matches the created template", same(read, made, "id", "name", "generation"))
    plain = obj(j.call("POST", "/v3/templates", body={"name": f"Password reset {tag}"}))
    j.check("generation defaults to legacy", eq(plain.get("generation"), "legacy"), generation=plain.get("generation"))
    plain_id = plain.get("id") or hex_id(j, "d-")
    rows = templates(page_size=200, generations=both)  # the default generation is not stated, so both are asked for
    j.check("template list holds both", bool(by_id(rows, tid)) and bool(by_id(rows, plain_id)), listed=len(rows))
    rows = templates(page_size=200, generations="dynamic")
    j.check("generation filter holds only that generation",
            bool(by_id(rows, tid)) and all(eq(t.get("generation"), "dynamic") for t in rows), listed=len(rows))
    rows = templates(page_size=1, generations=both)
    j.check("page size caps the template list", len(rows) == 1, listed=len(rows))
    j.call("POST", "/v3/mail/send", body={
        "personalizations": [{"to": [{"email": "ada@example.com"}], "dynamic_template_data": {"order": tag}}],
        "from": {"email": "shop@example.com"}, "template_id": tid})
    j.check("mail using the template is accepted", j.last["status"] == 202)
    days = [time.strftime("%Y-%m-%d", time.gmtime(time.time() - 86400 * back)) for back in (3, 2, 1)]
    stats = dicts(j.call("GET", "/v3/stats", query={"start_date": days[0], "end_date": days[-1], "aggregated_by": "day"}))
    answered = sorted(str(s.get("date")) for s in stats)
    j.check("stats cover each day asked for", answered == days, dates=answered, asked=days)
    listed = head(j.call("GET", "/v3/suppression/bounces"))
    email = listed.get("email") or "nobody@example.com"
    bounce_path = "/v3/suppression/bounces/" + urllib.parse.quote(str(email), safe="@")
    one = dicts(j.call("GET", bounce_path))
    j.check("bounce read keeps the listed bounce",
            len(one) == 1 and same(one[0], listed, "email", "reason", "status", "created"))
    rows = dicts(j.call("GET", "/v3/suppression/bounces", query={"email": email}))
    j.check("email filter holds only that address", bool(rows) and all(eq(b.get("email"), email) for b in rows),
            listed=len(rows))
    j.call("DELETE", bounce_path)
    j.check("removing a bounce answers 204", j.last["status"] == 204)
    after = j.call("GET", bounce_path)
    j.check("removed address has no bounce", j.last["status"] == 200 and after == [], answered=after)
    after = j.call("GET", "/v3/suppression/bounces")
    j.check("bounce list drops the address",
            j.last["status"] == 200 and isinstance(after, list) and not by_key(after, "email", email))


def plaid_ids(accounts):
    return {a["account_id"] for a in dicts(accounts) if isinstance(a.get("account_id"), str)}


def plaid_token(j, kind):
    return f"{kind}-sandbox-{uuid.UUID(int=j.rng.getrandbits(128), version=4)}"


def plaid_steps(j):
    # Sandbox item to data. The contract has no route to update or remove an item. An
    # item's institution id and name are optional, so they are not compared; sync does not
    # promise any transaction exists yet, so its checks hold over what it did answer.
    j.headers.update({"plaid-client-id": "bench-placeholder", "plaid-secret": "bench-placeholder"})
    institution, hook = "ins_109508", "https://example.com/plaid-hook"
    uuid4 = r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}"
    public = obj(j.call("POST", "/sandbox/public_token/create", body={
        "institution_id": institution, "initial_products": ["auth", "identity", "transactions"],
        "options": {"webhook": hook}}))
    j.check("public token is issued", matches(f"public-sandbox-{uuid4}", public.get("public_token")))
    token = public.get("public_token") or plaid_token(j, "public")
    exchanged = obj(j.call("POST", "/item/public_token/exchange", body={"public_token": token}))
    j.check("exchange returns an access token",
            matches(f"access-sandbox-{uuid4}", exchanged.get("access_token")) and exchanged.get("item_id"))
    item_id = exchanged.get("item_id")
    with_token = {"access_token": exchanged.get("access_token") or plaid_token(j, "access")}
    item = obj(obj(j.call("POST", "/item/get", body=with_token)).get("item"))
    j.check("item reads back as exchanged", eq(item.get("item_id"), item_id), item=item.get("item_id"), asked=item_id)
    j.check("item keeps its webhook", eq(item.get("webhook"), hook), webhook=item.get("webhook"))
    answer = obj(j.call("POST", "/accounts/get", body=with_token))
    known = plaid_ids(answer.get("accounts"))
    j.check("accounts name their item", eq(obj(answer.get("item")).get("item_id"), item_id))
    balances = plaid_ids(obj(j.call("POST", "/accounts/balance/get", body=with_token)).get("accounts"))
    j.check("balances cover the same accounts", bool(known) and balances == known,
            accounts=sorted(known), balances=sorted(balances))
    wanted = min(known) if known else "no-account-listed"
    only = obj(j.call("POST", "/accounts/get", body=dict(with_token, options={"account_ids": [wanted]})))
    only = plaid_ids(only.get("accounts"))
    j.check("account filter holds only the asked account", bool(known) and only == {wanted}, answered=sorted(only))
    auth = obj(j.call("POST", "/auth/get", body=with_token))
    auth_accounts = dicts(auth.get("accounts"))
    auth_ids = plaid_ids(auth_accounts)
    j.check("auth accounts belong to the item", bool(auth_ids) and auth_ids <= known)
    ach = {n.get("account_id") for n in dicts(obj(auth.get("numbers")).get("ach"))}
    bank = {a.get("account_id") for a in auth_accounts if a.get("subtype") in ("checking", "savings")}
    j.check("auth numbers cover checking and savings", bool(auth_ids) and bank <= ach <= auth_ids,
            ach=sorted(map(str, ach)), bank=sorted(map(str, bank)))
    owners = plaid_ids(obj(j.call("POST", "/identity/get", body=with_token)).get("accounts"))
    j.check("identity accounts belong to the item", bool(owners) and owners <= known)
    synced = obj(j.call("POST", "/transactions/sync", body=dict(with_token, count=2)))
    added = dicts(synced.get("added"))
    answered = j.last["status"] == 200 and isinstance(synced.get("added"), list)
    j.check("count caps the sync", answered and len(added) <= 2, added=len(added))
    j.check("transactions name the item's accounts", answered and all(t.get("account_id") in known for t in added))
    cursor = synced.get("next_cursor")
    later = obj(j.call("POST", "/transactions/sync", body=dict(with_token, **({"cursor": cursor} if cursor else {}))))
    seen = {t.get("transaction_id") for t in added}
    again = [t.get("transaction_id") for t in dicts(later.get("added")) if t.get("transaction_id") in seen]
    j.check("next sync repeats nothing", j.last["status"] == 200 and bool(cursor) and not again, repeated=again)
    found = obj(j.call("POST", "/institutions/get_by_id", body={"institution_id": institution, "country_codes": ["US"]}))
    j.check("institution answers the id asked for", eq(obj(found.get("institution")).get("institution_id"), institution))


def paypal_cents(money):
    value = obj(money).get("value")
    return int(value.replace(".", "")) if matches(r"[0-9]+\.[0-9]{2}", value) else None


def paypalpayments_steps(j):
    # Captures the client was given: the contract has no route to create one, or to list
    # or change anything else. Only a completed capture of at least 2.00 can be refunded
    # twice (the money schema starts at 1.00), and a mock may answer with any status for
    # an id it was not told about, so the journey reads up to three ids to find one, and
    # goes on with the last when none fits. No check asks about links: the schema names
    # the relations but does not say which a refund carries.
    j.headers.update(HELDOUT_AUTH)
    j.headers["prefer"] = "return=representation"  # the journey reads the refund's fields from the answer

    def paypal_id():
        rest = "".join(j.rng.choice("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ") for _ in range(16))
        return j.rng.choice("0123456789") + rest

    for _ in range(3):
        cid = paypal_id()
        capture = obj(j.call("GET", f"/captures/{cid}"))
        if capture.get("status") == "COMPLETED" and (paypal_cents(capture.get("amount")) or 0) >= 200:
            break
    j.check("capture read names the capture asked for", eq(capture.get("id"), cid),
            capture_status=capture.get("status"), amount=capture.get("amount"))
    again = obj(j.call("GET", f"/captures/{cid}"))
    j.check("a second read repeats the capture",
            same(again, capture, "id", "status", "amount", "final_capture", "create_time"))
    total = paypal_cents(capture.get("amount")) or 10000
    currency = obj(capture.get("amount")).get("currency_code") or "USD"
    part, remainder = max(total // 2, 100), max(total - max(total // 2, 100), 100)

    def money(cents):
        return {"currency_code": currency, "value": f"{cents // 100}.{cents % 100:02d}"}

    asked = {"amount": money(part), "invoice_id": f"INV-{j.rng.randrange(10**6, 10**7)}", "note_to_payer": "Half of it back."}
    refund = obj(j.call("POST", f"/captures/{cid}/refund", body=asked))
    j.check("refund has an id", j.last["status"] == 201 and matches(r"[0-9][A-Z0-9]{16}", refund.get("id")),
            id=refund.get("id"))
    j.check("refund keeps the amount asked for", eq(refund.get("amount"), asked["amount"]), amount=refund.get("amount"))
    j.check("refund keeps the invoice and note", same(refund, asked, "invoice_id", "note_to_payer"))
    rid = refund.get("id") or paypal_id()
    read = obj(j.call("GET", f"/refunds/{rid}"))
    j.check("read matches the created refund", same(read, refund, "id", "amount", "invoice_id", "note_to_payer"))
    after = obj(j.call("GET", f"/captures/{cid}"))
    j.check("capture shows the partial refund", eq(after.get("status"), "PARTIALLY_REFUNDED"),
            capture_status=after.get("status"))
    j.check("capture keeps its amount", same(after, capture, "amount"))
    rest = obj(j.call("POST", f"/captures/{cid}/refund", body={"amount": money(remainder)}))
    j.check("second refund has its own id", rest.get("id") and rest.get("id") != refund.get("id"), id=rest.get("id"))
    j.check("second refund keeps its amount", eq(rest.get("amount"), money(remainder)), amount=rest.get("amount"))
    after = obj(j.call("GET", f"/captures/{cid}"))
    j.check("capture shows the full refund", eq(after.get("status"), "REFUNDED"), capture_status=after.get("status"))
    read = obj(j.call("GET", f"/refunds/{rid}"))
    j.check("first refund still reads as it was", same(read, refund, "id", "amount", "invoice_id"))
    j.call("POST", f"/captures/{cid}/refund", body={"amount": money(100)})
    j.check("a fully refunded capture refuses another", j.last["status"] in (400, 409, 422))
    aid = paypal_id()
    authorization = obj(j.call("GET", f"/authorizations/{aid}"))
    j.check("authorization read names the authorization asked for", eq(authorization.get("id"), aid))


def openweathermap_steps(j):
    # Reads only: the contract has no route that writes. Its schemas bound temperatures to
    # the Kelvin range whatever `units` says, so a Celsius answer to units=metric would break
    # them and unit conversion is left out. Its coordinates are bound to Germany and Austria,
    # so the city is Munich. Coordinates are compared with room for rounding.
    def get(path, **query):
        return j.call("GET", path, query=dict(query, appid="bench-placeholder"))

    def good():
        return j.last["status"] == 200

    city = "Munich,DE"
    place = head(get("/geo/1.0/direct", q=city, limit=2))
    j.check("geocode finds the city asked for",
            any(n in str(place.get("name")).lower() for n in ("munich", "münchen")) and eq(place.get("country"), "DE"),
            place=place.get("name"), country=place.get("country"))
    capped = get("/geo/1.0/direct", q=city, limit=1)
    j.check("limit caps the geocode matches", good() and isinstance(capped, list) and len(capped) <= 1, listed=len(capped))
    nowhere = get("/geo/1.0/direct", q="Qqzxvyplk Wrtbnmg,DE")
    j.check("unknown place matches nothing", good() and nowhere == [], listed=len(nowhere))
    lat, lon = place.get("lat"), place.get("lon")
    if not (is_number(lat) and is_number(lon)):
        lat, lon = 48.1374, 11.5755
    coord = obj(obj(get("/data/2.5/weather", lat=lat, lon=lon)).get("coord"))
    j.check("weather keeps the coordinates asked for",
            near(coord.get("lat"), lat, 0.05) and near(coord.get("lon"), lon, 0.05), coord=coord, asked=[lat, lon])
    weather = obj(get("/data/2.5/weather", q=city))
    by_city = obj(get("/data/2.5/weather", id=weather.get("id") or 2867714))
    j.check("city id read names the same city", same(by_city, weather, "id", "name"),
            answered=[by_city.get("id"), by_city.get("name")], asked=[weather.get("id"), weather.get("name")])
    area = obj(get("/geo/1.0/zip", zip="80331,DE"))
    j.check("zip lookup keeps the zip asked for", eq(area.get("zip"), "80331") and eq(area.get("country"), "DE"))
    j.check("zip lands near the city", near(area.get("lat"), lat, 0.3) and near(area.get("lon"), lon, 0.3),
            area=[area.get("lat"), area.get("lon")], city=[lat, lon])
    nearby = dicts(get("/geo/1.0/reverse", lat=lat, lon=lon, limit=1))
    j.check("reverse lookup stays in the country", len(nearby) == 1 and eq(nearby[0].get("country"), "DE"),
            listed=len(nearby))
    forecast = obj(get("/data/2.5/forecast", q=city, cnt=3))
    steps = dicts(forecast.get("list"))
    j.check("forecast holds the steps asked for", len(steps) == 3 and eq(forecast.get("cnt"), 3),
            listed=len(steps), cnt=forecast.get("cnt"))
    stamps = [s.get("dt") for s in steps]
    j.check("forecast steps three hours apart",
            len(stamps) >= 2 and all(is_number(a) and is_number(b) and b - a == 10800 for a, b in zip(stamps, stamps[1:])),
            stamps=stamps)
    j.check("forecast names the same city as the weather", eq(obj(forecast.get("city")).get("id"), weather.get("id")),
            forecast_city=obj(forecast.get("city")).get("id"), weather_city=weather.get("id"))
    onecall = obj(get("/data/3.0/onecall", lat=lat, lon=lon, exclude="minutely,hourly,alerts"))
    j.check("one call keeps the coordinates", near(onecall.get("lat"), lat, 0.05) and near(onecall.get("lon"), lon, 0.05))
    j.check("one call leaves out what was excluded",
            good() and bool(onecall) and not {"minutely", "hourly", "alerts"} & set(onecall),
            parts=sorted(onecall))
    air = obj(obj(get("/data/2.5/air_pollution", lat=lat, lon=lon)).get("coord"))
    j.check("air pollution keeps the coordinates", near(air.get("lat"), lat, 0.05) and near(air.get("lon"), lon, 0.05),
            coord=air, asked=[lat, lon])


BLIND2_JOURNEYS = {"twilio": twilio_steps, "sendgrid": sendgrid_steps, "plaid": plaid_steps,
                   "paypalpayments": paypalpayments_steps, "openweathermap": openweathermap_steps}


def run_journeys_blind2(srv, sink, args):
    run_journey_table(BLIND2_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- third blind journeys

# The same rule again, over five contracts written for this suite from each real API's
# public documentation (scripts/llm-bench/contracts/blind3/), so nobody working on the
# engine has seen them. No check gates the rest: a journey goes on with an id of its own
# when a call gave none. A check asserts only what its contract promises a client. Do
# not edit a check to suit results.
BLIND3_CHECKS = {
    "trello": [
        "board has an id", "read matches the created board", "list belongs to the board", "board lists hold both",
        "card sits in its list and board", "card keeps its due date", "list cards hold the card",
        "moving a card changes its list", "old list no longer holds the card", "comment keeps its text",
        "card actions list the comment", "closed filter holds the archived card", "renamed board reads back renamed",
        "deleted card is gone",
    ],
    "asana": [
        "project has a gid", "section belongs to the project", "task keeps its project", "read matches what was sent",
        "project filter holds both tasks", "limit pages the task list", "section lists the added task",
        "subtask names its parent", "subtasks list the subtask", "completion reads back",
        "completed_since=now leaves out the completed task", "comment keeps its text", "task stories hold the comment",
        "deleted task is gone",
    ],
    "gitlab": [
        "milestone has an id", "issue has an iid in its project", "issue keeps its labels", "issue names its milestone",
        "read matches the created issue", "labels filter holds only labelled issues", "search finds the issue by title",
        "per_page caps the list", "milestone issues hold the issue", "closing an issue closes it",
        "state filter leaves out the closed issue", "note keeps its body", "issue notes hold the note",
        "deleted issue is gone",
    ],
    "freshdesk": [
        "contact names its company", "email filter finds only the contact", "ticket names the contact as requester",
        "ticket keeps what was sent", "read matches the created ticket", "requester filter holds the ticket",
        "reply names its ticket", "a note is private by default", "conversations hold the reply and the note",
        "update resolves the ticket", "stats show when it was resolved", "search finds the ticket",
        "deleted filter holds the deleted ticket", "default list leaves out the deleted ticket",
    ],
    "mailchimp": [
        "list has an id", "read matches the created list", "member id is the email's hash",
        "member keeps its status and merge fields", "read by hash matches the member",
        "status filter holds only subscribed members", "count caps the members", "member tags hold the added tag",
        "update changes the merge field", "update keeps the other merge field",
        "upsert adds a new address with status_if_new", "search finds the member by email",
        "archived member reads as archived", "erased member is gone",
    ],
}
JOURNEY_CHECKS.update(BLIND3_CHECKS)
BLIND3_DIR = "scripts/llm-bench/contracts/blind3"
# Contracts alone: no persona.
BLIND3_MOCKS = {name: (f"{BLIND3_DIR}/{name}.openapi.yaml", f"/{name}", None) for name in BLIND3_CHECKS}


def instant(value):
    """The moment an ISO 8601 date-time names, or None."""
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def ids_of(items, key="id"):
    return [i.get(key) for i in dicts(items)]


def trello_steps(j):
    # Fields go in the query string, as the contract has them. The contract keeps a deleted
    # board out of the journey (a delete answers only `_value`), so the delete is a card's.
    auth = {"key": "bench-placeholder", "token": "bench-placeholder"}

    def call(method, path, **query):
        return j.call(method, path, query=dict(query, **auth))

    def trello_id():
        return "".join(j.rng.choice("0123456789abcdef") for _ in range(24))

    tag = j.rng.randrange(10**5, 10**6)
    name, desc = f"Kitchen remodel {tag}", "Everything for the new kitchen."
    board = obj(call("POST", "/1/boards", name=name, desc=desc, defaultLists="false"))
    j.check("board has an id", j.last["status"] == 200 and matches(r"[0-9a-fA-F]{24}", board.get("id")),
            id=board.get("id"))
    bid = board.get("id") or trello_id()
    read = obj(call("GET", f"/1/boards/{bid}"))
    j.check("read matches the created board",
            eq(read.get("id"), bid) and eq(read.get("name"), name) and eq(read.get("desc"), desc))
    todo = obj(call("POST", "/1/lists", name="To do", idBoard=bid))
    j.check("list belongs to the board", eq(todo.get("idBoard"), bid), idBoard=todo.get("idBoard"))
    done = obj(call("POST", "/1/lists", name="Done", idBoard=bid))
    todo_id, done_id = todo.get("id") or trello_id(), done.get("id") or trello_id()
    lists = call("GET", f"/1/boards/{bid}/lists")
    j.check("board lists hold both", bool(by_id(lists, todo_id)) and bool(by_id(lists, done_id)), listed=ids_of(lists))
    due = "2026-12-01T17:00:00.000Z"
    card = obj(call("POST", "/1/cards", idList=todo_id, name="Order the tiles", desc="Grey, 30 by 60.", due=due))
    j.check("card sits in its list and board", eq(card.get("idList"), todo_id) and eq(card.get("idBoard"), bid))
    j.check("card keeps its due date", eq(instant(card.get("due")), instant(due)), due=card.get("due"))
    cid = card.get("id") or trello_id()
    rows = call("GET", f"/1/lists/{todo_id}/cards")
    j.check("list cards hold the card", bool(by_id(rows, cid)) and all(eq(c.get("idList"), todo_id) for c in dicts(rows)),
            listed=ids_of(rows))
    moved = obj(call("PUT", f"/1/cards/{cid}", idList=done_id))
    j.check("moving a card changes its list", eq(moved.get("id"), cid) and eq(moved.get("idList"), done_id),
            idList=moved.get("idList"))
    rows = call("GET", f"/1/lists/{todo_id}/cards")
    j.check("old list no longer holds the card", j.last["status"] == 200 and isinstance(rows, list) and not by_id(rows, cid),
            listed=ids_of(rows))
    text = "Tiles ordered, delivery on Friday."
    comment = obj(call("POST", f"/1/cards/{cid}/actions/comments", text=text))
    j.check("comment keeps its text", eq(obj(comment.get("data")).get("text"), text))
    aid = comment.get("id") or trello_id()
    actions = call("GET", f"/1/cards/{cid}/actions", filter="commentCard")
    j.check("card actions list the comment",
            bool(by_id(actions, aid)) and all(eq(a.get("type"), "commentCard") for a in dicts(actions)),
            listed=ids_of(actions))
    call("PUT", f"/1/cards/{cid}", closed="true")
    closed = call("GET", f"/1/boards/{bid}/cards/closed")
    j.check("closed filter holds the archived card",
            bool(by_id(closed, cid)) and all(c.get("closed") is True for c in dicts(closed)), listed=ids_of(closed))
    renamed = f"Kitchen and pantry {tag}"
    call("PUT", f"/1/boards/{bid}", name=renamed)
    read = obj(call("GET", f"/1/boards/{bid}"))
    j.check("renamed board reads back renamed", eq(read.get("name"), renamed), board_name=read.get("name"))
    call("DELETE", f"/1/cards/{cid}")
    deleted = j.last["status"]
    call("GET", f"/1/cards/{cid}")
    j.check("deleted card is gone", j.last["status"] == 404, delete_status=deleted)


def asana_steps(j):
    # Lists answer compact records, so list checks look only at gids. Where a task lands
    # when created in a project without a section is the project's first section, which
    # may not be the one created here, so the section check asks only for the task added.
    j.headers.update(HELDOUT_AUTH)

    def gid():
        return "1" + "".join(j.rng.choice("0123456789") for _ in range(15))

    def data(value):
        return obj(obj(value).get("data"))

    def gids(value):
        return ids_of(obj(value).get("data"), "gid")

    tag = j.rng.randrange(10**5, 10**6)
    project = data(j.call("POST", "/api/1.0/projects",
                          body={"data": {"name": f"Offsite {tag}", "notes": "Team offsite in March.", "workspace": gid()}}))
    j.check("project has a gid", j.last["status"] == 201 and matches(r"[0-9]+", project.get("gid")), gid=project.get("gid"))
    pid = project.get("gid") or gid()
    section = data(j.call("POST", f"/api/1.0/projects/{pid}/sections", body={"data": {"name": "Shortlist"}}))
    j.check("section belongs to the project", eq(obj(section.get("project")).get("gid"), pid))
    sid = section.get("gid") or gid()
    sent = {"name": f"Book the venue {tag}", "notes": "Room for forty people.", "due_on": "2026-12-04"}
    task = data(j.call("POST", "/api/1.0/tasks", body={"data": dict(sent, projects=[pid])}))
    j.check("task keeps its project", pid in ids_of(task.get("projects"), "gid"))
    t1 = task.get("gid") or gid()
    read = data(j.call("GET", f"/api/1.0/tasks/{t1}"))
    j.check("read matches what was sent", eq(read.get("gid"), t1) and same(read, sent, "name", "notes", "due_on"))
    t2 = data(j.call("POST", "/api/1.0/tasks",
                     body={"data": {"name": f"Send the invitations {tag}", "projects": [pid]}})).get("gid") or gid()
    listed = gids(j.call("GET", "/api/1.0/tasks", query={"project": pid}))
    j.check("project filter holds both tasks", t1 in listed and t2 in listed, listed=listed)
    page = obj(j.call("GET", "/api/1.0/tasks", query={"project": pid, "limit": 1}))
    offset = obj(page.get("next_page")).get("offset")
    j.check("limit pages the task list", len(dicts(page.get("data"))) == 1 and isinstance(offset, str) and bool(offset),
            listed=gids(page), next_page=page.get("next_page"))
    j.call("POST", f"/api/1.0/sections/{sid}/addTask", body={"data": {"task": t1}})
    listed = gids(j.call("GET", f"/api/1.0/sections/{sid}/tasks"))
    j.check("section lists the added task", t1 in listed, listed=listed)
    sub = data(j.call("POST", f"/api/1.0/tasks/{t1}/subtasks", body={"data": {"name": "Confirm the catering"}}))
    j.check("subtask names its parent", eq(obj(sub.get("parent")).get("gid"), t1))
    sub_id = sub.get("gid") or gid()
    listed = gids(j.call("GET", f"/api/1.0/tasks/{t1}/subtasks"))
    j.check("subtasks list the subtask", sub_id in listed, listed=listed)
    j.call("PUT", f"/api/1.0/tasks/{t2}", body={"data": {"completed": True}})
    read = data(j.call("GET", f"/api/1.0/tasks/{t2}"))
    j.check("completion reads back", read.get("completed") is True, completed=read.get("completed"))
    listed = gids(j.call("GET", "/api/1.0/tasks", query={"project": pid, "completed_since": "now"}))
    j.check("completed_since=now leaves out the completed task",
            j.last["status"] == 200 and t1 in listed and t2 not in listed, listed=listed)
    text = "The north wing is free on the 12th."
    story = data(j.call("POST", f"/api/1.0/tasks/{t1}/stories", body={"data": {"text": text}}))
    j.check("comment keeps its text", eq(story.get("text"), text))
    story_id = story.get("gid") or gid()
    listed = gids(j.call("GET", f"/api/1.0/tasks/{t1}/stories"))
    j.check("task stories hold the comment", story_id in listed, listed=listed)
    j.call("DELETE", f"/api/1.0/tasks/{t2}")
    deleted = j.last["status"]
    j.call("GET", f"/api/1.0/tasks/{t2}")
    j.check("deleted task is gone", j.last["status"] == 404, delete_status=deleted)


def gitlab_steps(j):
    # Issue routes take the iid; the milestone route takes the milestone's global id.
    # Closing writes a system note, so the note check asks only that its note is listed.
    j.headers["private-token"] = "bench-placeholder"
    project = j.rng.randrange(10**6, 10**7)
    base = f"/api/v4/projects/{project}"
    tag = j.rng.randrange(10**5, 10**6)

    def iids(query):
        return ids_of(j.call("GET", f"{base}/issues", query=query), "iid")

    milestone = obj(j.call("POST", f"{base}/milestones", body={"title": f"Release {tag}", "due_date": "2026-12-18"}))
    j.check("milestone has an id", j.last["status"] == 201 and is_int(milestone.get("id")), id=milestone.get("id"))
    mid = milestone.get("id") if is_int(milestone.get("id")) else j.rng.randrange(10**6, 10**7)
    word = f"flaky{tag}"
    sent = {"title": f"Checkout {word} on Safari", "description": "The pay button does nothing after a coupon.",
            "labels": "bug,backend", "milestone_id": mid, "due_date": "2026-12-11"}
    first = obj(j.call("POST", f"{base}/issues", body=sent))
    j.check("issue has an iid in its project",
            j.last["status"] == 201 and is_int(first.get("iid")) and eq(first.get("project_id"), project),
            iid=first.get("iid"), project_id=first.get("project_id"))
    labels = first.get("labels")
    j.check("issue keeps its labels", isinstance(labels, list) and sorted(map(str, labels)) == ["backend", "bug"],
            labels=labels)
    j.check("issue names its milestone", eq(obj(first.get("milestone")).get("id"), mid))
    iid = first.get("iid") if is_int(first.get("iid")) else 1
    read = obj(j.call("GET", f"{base}/issues/{iid}"))
    j.check("read matches the created issue", same(read, first, "id", "iid", "title", "description", "due_date"))
    second = obj(j.call("POST", f"{base}/issues", body={"title": "Update the API docs", "labels": "docs"}))
    iid2 = second.get("iid") if is_int(second.get("iid")) else iid + 1
    rows = j.call("GET", f"{base}/issues", query={"labels": "bug"})
    listed = ids_of(rows, "iid")
    j.check("labels filter holds only labelled issues",
            iid in listed and iid2 not in listed and all(isinstance(r.get("labels"), list) and "bug" in r["labels"] for r in dicts(rows)),
            listed=listed)
    listed = iids({"search": word})
    j.check("search finds the issue by title", iid in listed and iid2 not in listed, listed=listed)
    rows = j.call("GET", f"{base}/issues", query={"per_page": 1})
    j.check("per_page caps the list", isinstance(rows, list) and len(rows) == 1, listed=ids_of(rows, "iid"))
    rows = j.call("GET", f"{base}/milestones/{mid}/issues")
    j.check("milestone issues hold the issue",
            iid in ids_of(rows, "iid") and all(eq(obj(r.get("milestone")).get("id"), mid) for r in dicts(rows)),
            listed=ids_of(rows, "iid"))
    closed = obj(j.call("PUT", f"{base}/issues/{iid}", body={"state_event": "close"}))
    j.check("closing an issue closes it", eq(closed.get("state"), "closed"), state=closed.get("state"))
    listed = iids({"state": "opened"})
    j.check("state filter leaves out the closed issue", iid2 in listed and iid not in listed, listed=listed)
    text = "Reproduced on Safari 18 with the SPRING coupon."
    note = obj(j.call("POST", f"{base}/issues/{iid}/notes", body={"body": text}))
    j.check("note keeps its body", eq(note.get("body"), text))
    note_id = note.get("id") if is_int(note.get("id")) else j.rng.randrange(10**6, 10**7)
    notes = j.call("GET", f"{base}/issues/{iid}/notes")
    j.check("issue notes hold the note", bool(by_id(notes, note_id)), listed=ids_of(notes))
    j.call("DELETE", f"{base}/issues/{iid2}")
    deleted = j.last["status"]
    j.call("GET", f"{base}/issues/{iid2}")
    j.check("deleted issue is gone", j.last["status"] == 404, delete_status=deleted)


def freshdesk_steps(j):
    # A deleted ticket goes to the trash, so it is looked for under filter=deleted rather
    # than expected to answer 404. Search terms are those the ticket was last given.
    j.headers["authorization"] = "Basic " + base64.b64encode(b"bench-placeholder:X").decode()
    tag = j.rng.randrange(10**5, 10**6)

    def number():
        return j.rng.randrange(10**8, 10**9)

    company = obj(j.call("POST", "/api/v2/companies", body={"name": f"Northwind {tag}"}))
    company_id = company.get("id") if is_int(company.get("id")) else number()
    email = f"ana.{tag}@northwind.example"
    contact = obj(j.call("POST", "/api/v2/contacts", body={"name": "Ana Silva", "email": email, "company_id": company_id}))
    j.check("contact names its company", j.last["status"] == 201 and eq(contact.get("company_id"), company_id),
            company_id=contact.get("company_id"))
    contact_id = contact.get("id") if is_int(contact.get("id")) else number()
    rows = j.call("GET", "/api/v2/contacts", query={"email": email})
    j.check("email filter finds only the contact",
            bool(by_id(rows, contact_id)) and all(eq(r.get("email"), email) for r in dicts(rows)), listed=ids_of(rows))
    sent = {"subject": f"Invoice {tag} shows the wrong VAT", "description": "<p>The VAT rate should be 23%.</p>",
            "email": email, "priority": 3, "status": 2, "source": 2, "tags": ["billing", "vat"]}
    ticket = obj(j.call("POST", "/api/v2/tickets", body=sent))
    j.check("ticket names the contact as requester", eq(ticket.get("requester_id"), contact_id),
            requester_id=ticket.get("requester_id"))
    j.check("ticket keeps what was sent",
            same(ticket, sent, "subject", "priority", "status", "source") and sorted(map(str, ticket.get("tags") or [])) == sent["tags"])
    tid = ticket.get("id") if is_int(ticket.get("id")) else number()
    read = obj(j.call("GET", f"/api/v2/tickets/{tid}"))
    j.check("read matches the created ticket", same(read, ticket, "id", "subject", "priority", "status", "requester_id"))
    rows = j.call("GET", "/api/v2/tickets", query={"requester_id": contact_id})
    j.check("requester filter holds the ticket",
            bool(by_id(rows, tid)) and all(eq(r.get("requester_id"), contact_id) for r in dicts(rows)), listed=ids_of(rows))
    reply = obj(j.call("POST", f"/api/v2/tickets/{tid}/reply", body={"body": "<p>Thanks, we are on it.</p>"}))
    j.check("reply names its ticket", eq(reply.get("ticket_id"), tid))
    note = obj(j.call("POST", f"/api/v2/tickets/{tid}/notes", body={"body": "Finance confirmed the 23% rate."}))
    j.check("a note is private by default", note.get("private") is True, private=note.get("private"))
    convs = j.call("GET", f"/api/v2/tickets/{tid}/conversations")
    j.check("conversations hold the reply and the note",
            bool(by_id(convs, reply.get("id"))) and bool(by_id(convs, note.get("id"))), listed=ids_of(convs))
    resolved = obj(j.call("PUT", f"/api/v2/tickets/{tid}", body={"status": 4}))
    j.check("update resolves the ticket", eq(resolved.get("status"), 4), status_value=resolved.get("status"))
    stats = obj(obj(j.call("GET", f"/api/v2/tickets/{tid}", query={"include": "stats"})).get("stats"))
    j.check("stats show when it was resolved", instant(stats.get("resolved_at")) is not None,
            resolved_at=stats.get("resolved_at"))
    found = dicts(obj(j.call("GET", "/api/v2/search/tickets", query={"query": '"priority:3 AND status:4"'})).get("results"))
    j.check("search finds the ticket",
            bool(by_id(found, tid)) and all(eq(t.get("priority"), 3) and eq(t.get("status"), 4) for t in found),
            listed=ids_of(found))
    j.call("DELETE", f"/api/v2/tickets/{tid}")
    deleted = j.last["status"]
    rows = j.call("GET", "/api/v2/tickets", query={"filter": "deleted"})
    j.check("deleted filter holds the deleted ticket", bool(by_id(rows, tid)), delete_status=deleted, listed=ids_of(rows))
    rows = j.call("GET", "/api/v2/tickets")
    j.check("default list leaves out the deleted ticket",
            j.last["status"] == 200 and isinstance(rows, list) and not by_id(rows, tid), listed=ids_of(rows))


def mailchimp_steps(j):
    # A member's path is the MD5 of its lowercase email, which the client computes itself,
    # so every member call uses that hash whatever the create answered.
    j.headers["authorization"] = "Basic " + base64.b64encode(b"bench:bench-placeholder").decode()
    tag = j.rng.randrange(10**5, 10**6)

    def md5(email):
        return hashlib.md5(email.lower().encode()).hexdigest()

    sent = {
        "name": f"Bakery news {tag}",
        "contact": {"company": "Crumb & Co", "address1": "12 Market St", "city": "Oakland", "state": "CA",
                    "zip": "94607", "country": "US"},
        "permission_reminder": "You signed up at our counter.",
        "campaign_defaults": {"from_name": "Crumb & Co", "from_email": "news@crumb.example",
                              "subject": "Fresh this week", "language": "en"},
        "email_type_option": False,
    }
    made = obj(j.call("POST", "/3.0/lists", body=sent))
    j.check("list has an id", j.last["status"] == 200 and isinstance(made.get("id"), str) and bool(made.get("id")),
            id=made.get("id"))
    lid = made.get("id") or "".join(j.rng.choice("0123456789abcdef") for _ in range(10))
    members = f"/3.0/lists/{lid}/members"
    read = obj(j.call("GET", f"/3.0/lists/{lid}"))
    j.check("read matches the created list",
            eq(read.get("id"), lid) and same(read, sent, "name", "permission_reminder")
            and eq(obj(read.get("campaign_defaults")).get("from_email"), "news@crumb.example"))
    email = f"Ada.Lovelace.{tag}@example.com"
    h = md5(email)
    member = obj(j.call("POST", members, body={"email_address": email, "status": "subscribed",
                                               "merge_fields": {"FNAME": "Ada", "LNAME": "Lovelace"}}))
    j.check("member id is the email's hash", eq(member.get("id"), h), id=member.get("id"), hash=h)
    fields = obj(member.get("merge_fields"))
    j.check("member keeps its status and merge fields",
            eq(member.get("status"), "subscribed") and eq(fields.get("FNAME"), "Ada") and eq(fields.get("LNAME"), "Lovelace"))
    read = obj(j.call("GET", f"{members}/{h}"))
    j.check("read by hash matches the member",
            eq(read.get("id"), h) and str(read.get("email_address")).lower() == email.lower() and eq(read.get("list_id"), lid))
    other = f"grace.hopper.{tag}@example.com"
    h2 = md5(other)
    j.call("POST", members, body={"email_address": other, "status": "pending"})
    rows = dicts(obj(j.call("GET", members, query={"status": "subscribed"})).get("members"))
    listed = ids_of(rows)
    j.check("status filter holds only subscribed members",
            h in listed and h2 not in listed and all(eq(m.get("status"), "subscribed") for m in rows), listed=listed)
    rows = dicts(obj(j.call("GET", members, query={"count": 1})).get("members"))
    j.check("count caps the members", len(rows) == 1, listed=ids_of(rows))
    label = f"vip-{tag}"
    j.call("POST", f"{members}/{h}/tags", body={"tags": [{"name": label, "status": "active"}]})
    names = ids_of(obj(j.call("GET", f"{members}/{h}/tags")).get("tags"), "name")
    j.check("member tags hold the added tag", label in names, tags=names)
    patched = obj(j.call("PATCH", f"{members}/{h}", body={"merge_fields": {"FNAME": "Augusta"}}))
    j.check("update changes the merge field", eq(obj(patched.get("merge_fields")).get("FNAME"), "Augusta"))
    read = obj(j.call("GET", f"{members}/{h}"))
    j.check("update keeps the other merge field", eq(obj(read.get("merge_fields")).get("LNAME"), "Lovelace"),
            merge_fields=read.get("merge_fields"))
    third = f"katherine.johnson.{tag}@example.com"
    upserted = obj(j.call("PUT", f"{members}/{md5(third)}", body={"email_address": third, "status_if_new": "subscribed"}))
    j.check("upsert adds a new address with status_if_new",
            eq(upserted.get("id"), md5(third)) and eq(upserted.get("status"), "subscribed"), status_value=upserted.get("status"))
    found = obj(j.call("GET", "/3.0/search-members", query={"query": email, "list_id": lid}))
    exact = ids_of(obj(found.get("exact_matches")).get("members"))
    j.check("search finds the member by email", h in exact, listed=exact)
    j.call("DELETE", f"{members}/{h}")
    deleted = j.last["status"]
    read = obj(j.call("GET", f"{members}/{h}"))
    j.check("archived member reads as archived", j.last["status"] == 200 and eq(read.get("status"), "archived"),
            delete_status=deleted, status_value=read.get("status"))
    j.call("POST", f"{members}/{h2}/actions/delete-permanent")
    erased = j.last["status"]
    j.call("GET", f"{members}/{h2}")
    j.check("erased member is gone", j.last["status"] == 404, erase_status=erased)


BLIND3_JOURNEYS = {"trello": trello_steps, "asana": asana_steps, "gitlab": gitlab_steps,
                   "freshdesk": freshdesk_steps, "mailchimp": mailchimp_steps}


def run_journeys_blind3(srv, sink, args):
    run_journey_table(BLIND3_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- fourth blind journeys

# The same rule again, over five more contracts written for this suite from each real
# API's public documentation (scripts/llm-bench/contracts/blind4/). Between them they
# wrap answers in envelopes (Pipedrive `data`, DigitalOcean `droplet`), page with
# cursors (Pipedrive, Zoom), nest resources two levels deep (Contentful environments in
# spaces, Sentry projects in organizations), update with partial PATCH bodies (Pipedrive,
# Zoom) and address records by slug or name (Sentry, DigitalOcean tags). No check gates
# the rest: a journey goes on with an id of its own when a call gave none. A check
# asserts only what its contract promises a client. Do not edit a check to suit results.
BLIND4_CHECKS = {
    "pipedrive": [
        "organization has an id", "person names its organization", "deal links its person and organization",
        "deal keeps value and currency", "read matches the created deal", "patch changes the value sent",
        "patch keeps what it was not sent", "person filter holds only the person's deals",
        "limit returns one deal and a cursor", "cursor reaches the other deal", "search finds the deal by title",
        "deal product keeps what was sent", "deal products list the product",
        "default list leaves out the deleted deal",
    ],
    "zoom": [
        "meeting has a numeric id", "meeting is hosted by the caller", "meeting keeps topic, start and duration",
        "meeting keeps its settings", "read matches the created meeting", "patch answers no content",
        "patch changes the topic", "patch keeps what it was not sent", "scheduled list holds both meetings",
        "page_size 1 returns one meeting and a token", "next page holds another meeting",
        "registration names the meeting", "approved registrants list the registrant", "deleted meeting is gone",
    ],
    "sentry": [
        "team keeps its slug and name", "organization teams list the team", "project takes the slug asked for",
        "project is owned by its team", "read by slug matches the created project",
        "organization projects list the project", "a taken slug is refused", "update answers the new name and slug",
        "new slug reads the same project", "key keeps its name", "key belongs to the project",
        "project keys list the key", "deleted project leaves the organization's list",
    ],
    "contentful": [
        "content type takes the id asked for", "content type keeps its fields",
        "activation records the version published", "entry names its content type", "entry keeps its localized fields",
        "a new entry is not published", "read matches the created entry", "publish records the version published",
        "publish raises the version by one", "a stale version is refused", "the current version updates the entry",
        "content_type filter holds both entries", "full-text query finds the entry", "deleted entry is gone",
    ],
    "digitalocean": [
        "tag keeps its name", "droplet is accepted with an id", "droplet keeps its name, region and size",
        "droplet carries its tag", "create links its create action", "read matches the created droplet",
        "tag filter holds only tagged droplets", "per_page pages the droplets", "tag counts both droplets",
        "tag filter now holds the second droplet", "action keeps its type and droplet",
        "droplet actions list the action", "action reads back by id", "deleted droplet is gone",
    ],
}
JOURNEY_CHECKS.update(BLIND4_CHECKS)
BLIND4_DIR = "scripts/llm-bench/contracts/blind4"
# Contracts alone: no persona.
BLIND4_MOCKS = {name: (f"{BLIND4_DIR}/{name}.openapi.yaml", f"/{name}", None) for name in BLIND4_CHECKS}


def pipedrive_steps(j):
    # Every answer is wrapped as {success, data}; lists add additional_data.next_cursor.
    # A deleted deal stays readable for 30 days, so its check is the default list, which
    # leaves deleted deals out. The organization is new, so its deals are the two made here.
    j.headers["x-api-token"] = "bench-placeholder"
    tag = j.rng.randrange(10**5, 10**6)

    def data(value):
        return obj(obj(value).get("data"))

    def rows(value):
        return dicts(obj(value).get("data"))

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**6, 10**7)

    org = data(j.call("POST", "/api/v2/organizations", body={"name": f"Northwind {tag}"}))
    j.check("organization has an id", j.last["status"] == 200 and is_int(org.get("id")), id=org.get("id"))
    org_id = own(org.get("id"))
    email = f"ana.{tag}@northwind.example"
    person = data(j.call("POST", "/api/v2/persons", body={
        "name": "Ana Silva", "org_id": org_id, "emails": [{"value": email, "primary": True, "label": "work"}]}))
    j.check("person names its organization", eq(person.get("org_id"), org_id), org_id=person.get("org_id"))
    person_id = own(person.get("id"))
    word = f"harbor{tag}"
    sent = {"title": f"Fleet renewal {word}", "value": 48000, "currency": "EUR", "person_id": person_id,
            "org_id": org_id, "expected_close_date": "2026-12-15"}
    deal = data(j.call("POST", "/api/v2/deals", body=sent))
    j.check("deal links its person and organization", same(deal, sent, "person_id", "org_id"))
    j.check("deal keeps value and currency", same(deal, sent, "value", "currency"))
    d1 = own(deal.get("id"))
    read = data(j.call("GET", f"/api/v2/deals/{d1}"))
    j.check("read matches the created deal", eq(read.get("id"), d1) and same(read, sent, "title", "expected_close_date"))
    patched = data(j.call("PATCH", f"/api/v2/deals/{d1}", body={"value": 52500}))
    j.check("patch changes the value sent", eq(patched.get("value"), 52500), value=patched.get("value"))
    read = data(j.call("GET", f"/api/v2/deals/{d1}"))
    j.check("patch keeps what it was not sent", same(read, sent, "title", "currency", "person_id", "org_id"))
    second = data(j.call("POST", "/api/v2/deals",
                         body={"title": f"Spare parts {tag}", "value": 3200, "currency": "EUR", "org_id": org_id}))
    d2 = own(second.get("id"))
    listed = rows(j.call("GET", "/api/v2/deals", query={"person_id": person_id}))
    j.check("person filter holds only the person's deals",
            bool(by_id(listed, d1)) and not by_id(listed, d2) and all(eq(r.get("person_id"), person_id) for r in listed),
            listed=ids_of(listed))
    page = obj(j.call("GET", "/api/v2/deals", query={"org_id": org_id, "limit": 1}))
    first = rows(page)
    cursor = obj(page.get("additional_data")).get("next_cursor")
    j.check("limit returns one deal and a cursor", len(first) == 1 and isinstance(cursor, str) and bool(cursor),
            listed=ids_of(first), next_cursor=cursor)
    query = {"org_id": org_id, "limit": 1}
    if isinstance(cursor, str) and cursor:
        query["cursor"] = cursor
    nxt = rows(j.call("GET", "/api/v2/deals", query=query))
    j.check("cursor reaches the other deal", len(nxt) == 1 and same_items(ids_of(first + nxt), [d1, d2]),
            first=ids_of(first), listed=ids_of(nxt))
    found = dicts(obj(data(j.call("GET", "/api/v2/deals/search", query={"term": word, "fields": "title"}))).get("items"))
    hits = [obj(i.get("item")).get("id") for i in found]
    j.check("search finds the deal by title", d1 in hits and d2 not in hits, listed=hits)
    product = data(j.call("POST", "/api/v2/products", body={
        "name": f"Service plan {tag}", "code": f"SP-{tag}", "prices": [{"price": 1200, "currency": "EUR"}]}))
    product_id = own(product.get("id"))
    line = data(j.call("POST", f"/api/v2/deals/{d1}/products",
                       body={"product_id": product_id, "item_price": 1200, "quantity": 3}))
    j.check("deal product keeps what was sent",
            j.last["status"] == 201 and eq(line.get("product_id"), product_id) and eq(line.get("item_price"), 1200)
            and eq(line.get("quantity"), 3))
    lines = rows(j.call("GET", f"/api/v2/deals/{d1}/products"))
    j.check("deal products list the product",
            product_id in ids_of(lines, "product_id") and all(eq(r.get("deal_id"), d1) for r in lines),
            listed=ids_of(lines, "product_id"))
    j.call("DELETE", f"/api/v2/deals/{d2}")
    deleted = j.last["status"]
    listed = rows(j.call("GET", "/api/v2/deals", query={"org_id": org_id}))
    j.check("default list leaves out the deleted deal",
            j.last["status"] == 200 and bool(by_id(listed, d1)) and not by_id(listed, d2),
            delete_status=deleted, listed=ids_of(listed))


def zoom_steps(j):
    # A meeting update answers 204, so what it changed is read back. The meeting asks for
    # approval_type 0, which registers and approves a registrant at once. Meetings are
    # scheduled for the user the token belongs to, `me`.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**10, 10**11)

    host = obj(j.call("GET", "/v2/users/me")).get("id")
    sent = {"topic": f"Quarterly planning {tag}", "type": 2, "start_time": "2026-12-08T15:00:00Z", "duration": 45,
            "timezone": "UTC", "agenda": "Budget, hiring and the roadmap.",
            "settings": {"approval_type": 0, "mute_upon_entry": True}}
    made = obj(j.call("POST", "/v2/users/me/meetings", body=sent))
    j.check("meeting has a numeric id", j.last["status"] == 201 and is_int(made.get("id")), id=made.get("id"))
    j.check("meeting is hosted by the caller", isinstance(host, str) and bool(host) and eq(made.get("host_id"), host),
            host_id=made.get("host_id"), me=host)
    j.check("meeting keeps topic, start and duration",
            eq(made.get("topic"), sent["topic"]) and eq(instant(made.get("start_time")), instant(sent["start_time"]))
            and eq(made.get("duration"), sent["duration"]), start_time=made.get("start_time"))
    settings = obj(made.get("settings"))
    j.check("meeting keeps its settings", eq(settings.get("approval_type"), 0) and settings.get("mute_upon_entry") is True,
            settings=settings)
    mid = own(made.get("id"))
    read = obj(j.call("GET", f"/v2/meetings/{mid}"))
    j.check("read matches the created meeting", eq(read.get("id"), mid) and same(read, made, "uuid", "topic", "join_url"))
    topic = f"Quarterly planning and budget {tag}"
    j.call("PATCH", f"/v2/meetings/{mid}", body={"topic": topic})
    j.check("patch answers no content", j.last["status"] == 204)
    read = obj(j.call("GET", f"/v2/meetings/{mid}"))
    j.check("patch changes the topic", eq(read.get("topic"), topic), topic=read.get("topic"))
    j.check("patch keeps what it was not sent", same(read, sent, "duration", "agenda") and
            eq(instant(read.get("start_time")), instant(sent["start_time"])))
    other = obj(j.call("POST", "/v2/users/me/meetings", body={
        "topic": f"Design review {tag}", "type": 2, "start_time": "2026-12-09T10:00:00Z", "duration": 30}))
    mid2 = own(other.get("id"))
    listed = ids_of(obj(j.call("GET", "/v2/users/me/meetings", query={"type": "scheduled", "page_size": 300})).get("meetings"))
    j.check("scheduled list holds both meetings", mid in listed and mid2 in listed, listed=listed)
    page = obj(j.call("GET", "/v2/users/me/meetings", query={"type": "scheduled", "page_size": 1}))
    first = ids_of(page.get("meetings"))
    token = page.get("next_page_token")
    j.check("page_size 1 returns one meeting and a token", len(first) == 1 and isinstance(token, str) and bool(token),
            listed=first, next_page_token=token)
    query = {"type": "scheduled", "page_size": 1}
    if isinstance(token, str) and token:
        query["next_page_token"] = token
    nxt = ids_of(obj(j.call("GET", "/v2/users/me/meetings", query=query)).get("meetings"))
    j.check("next page holds another meeting", len(nxt) == 1 and nxt[0] not in first, first=first, listed=nxt)
    reg = obj(j.call("POST", f"/v2/meetings/{mid}/registrants",
                     body={"email": f"ana.{tag}@example.org", "first_name": "Ana", "last_name": "Silva"}))
    j.check("registration names the meeting", j.last["status"] == 201 and eq(reg.get("id"), mid) and eq(reg.get("topic"), topic),
            id=reg.get("id"), topic=reg.get("topic"))
    rid = reg.get("registrant_id") if isinstance(reg.get("registrant_id"), str) and reg.get("registrant_id") \
        else "".join(j.rng.choice("abcdefghijklmnopqrstuvwxyz0123456789") for _ in range(22))
    rows = dicts(obj(j.call("GET", f"/v2/meetings/{mid}/registrants")).get("registrants"))
    j.check("approved registrants list the registrant",
            bool(by_id(rows, rid)) and all(eq(r.get("status"), "approved") for r in rows), listed=ids_of(rows))
    j.call("DELETE", f"/v2/meetings/{mid2}")
    deleted = j.last["status"]
    j.call("GET", f"/v2/meetings/{mid2}")
    j.check("deleted meeting is gone", j.last["status"] == 404, delete_status=deleted)


def sentry_steps(j):
    # Projects are addressed by organization and project slug. Issues are made by events,
    # which nothing here sends, so the issue check is only that an unknown one is not found.
    # An old slug may redirect after a rename, so only the new one is read.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    org, team_slug, slug, renamed = f"acme-{tag}", f"payments-{tag}", f"checkout-api-{tag}", f"checkout-{tag}"
    team = obj(j.call("POST", f"/api/0/organizations/{org}/teams/", body={"slug": team_slug, "name": "Payments"}))
    j.check("team keeps its slug and name",
            j.last["status"] == 201 and eq(team.get("slug"), team_slug) and eq(team.get("name"), "Payments"))
    listed = ids_of(j.call("GET", f"/api/0/organizations/{org}/teams/"), "slug")
    j.check("organization teams list the team", team_slug in listed, listed=listed)
    project = obj(j.call("POST", f"/api/0/teams/{org}/{team_slug}/projects/",
                         body={"name": "Checkout API", "slug": slug, "platform": "python-django"}))
    j.check("project takes the slug asked for", j.last["status"] == 201 and eq(project.get("slug"), slug),
            slug=project.get("slug"))
    j.check("project is owned by its team", team_slug in ids_of(project.get("teams"), "slug"),
            teams=ids_of(project.get("teams"), "slug"))
    pid = project.get("id") if isinstance(project.get("id"), str) and project.get("id") \
        else str(j.rng.randrange(10**6, 10**7))
    read = obj(j.call("GET", f"/api/0/projects/{org}/{slug}/"))
    j.check("read by slug matches the created project",
            eq(read.get("id"), pid) and eq(read.get("slug"), slug) and eq(read.get("name"), "Checkout API")
            and eq(read.get("platform"), "python-django"))
    listed = ids_of(j.call("GET", f"/api/0/organizations/{org}/projects/"), "slug")
    j.check("organization projects list the project", slug in listed, listed=listed)
    j.call("POST", f"/api/0/teams/{org}/{team_slug}/projects/", body={"name": "Checkout API again", "slug": slug})
    j.check("a taken slug is refused", j.last["status"] == 409)
    updated = obj(j.call("PUT", f"/api/0/projects/{org}/{slug}/", body={"name": "Checkout", "slug": renamed}))
    j.check("update answers the new name and slug", eq(updated.get("name"), "Checkout") and eq(updated.get("slug"), renamed))
    read = obj(j.call("GET", f"/api/0/projects/{org}/{renamed}/"))
    j.check("new slug reads the same project", j.last["status"] == 200 and eq(read.get("id"), pid), id=read.get("id"))
    key = obj(j.call("POST", f"/api/0/projects/{org}/{renamed}/keys/", body={"name": "Backend"}))
    j.check("key keeps its name", j.last["status"] == 201 and eq(key.get("name"), "Backend"))
    j.check("key belongs to the project", str(key.get("projectId")) == pid, projectId=key.get("projectId"), project=pid)
    kid = key.get("id") if isinstance(key.get("id"), str) and key.get("id") \
        else "".join(j.rng.choice("0123456789abcdef") for _ in range(32))
    keys = j.call("GET", f"/api/0/projects/{org}/{renamed}/keys/")
    j.check("project keys list the key", bool(by_id(keys, kid)), listed=ids_of(keys))
    j.call("DELETE", f"/api/0/projects/{org}/{renamed}/")
    deleted = j.last["status"]
    rows = j.call("GET", f"/api/0/organizations/{org}/projects/")
    j.check("deleted project leaves the organization's list",
            j.last["status"] == 200 and isinstance(rows, list) and not by_id(rows, pid)
            and renamed not in ids_of(rows, "slug"), delete_status=deleted, listed=ids_of(rows, "slug"))


def contentful_steps(j):
    # Writes send the current sys.version in X-Contentful-Version, taken from the last
    # answer; when an answer gave none the journey goes on with the version the contract
    # says it must be. The deleted entry is a draft, since a published one must be
    # unpublished first.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    base = f"/spaces/space{tag}/environments/master"

    def call(method, path, version=None, content_type=None, **kw):
        extra = {}
        if version is not None:
            extra["X-Contentful-Version"] = str(version)
        if content_type:
            extra["X-Contentful-Content-Type"] = content_type
        j.headers.update(extra)
        try:
            return obj(j.call(method, base + path, **kw))
        finally:
            for k in extra:
                j.headers.pop(k, None)

    def sys_of(value):
        return obj(obj(value).get("sys"))

    def version(value, fallback):
        v = sys_of(value).get("version")
        return v if is_int(v) else fallback

    def link_id(value):
        return obj(obj(value).get("sys")).get("id")

    def entry_ids(rows):
        return [sys_of(r).get("id") for r in rows]

    ct = f"recipe{tag}"
    fields = [{"id": "title", "name": "Title", "type": "Symbol", "required": True},
              {"id": "body", "name": "Body", "type": "Text"},
              {"id": "servings", "name": "Servings", "type": "Integer"}]
    made = call("PUT", f"/content_types/{ct}", body={"name": "Recipe", "displayField": "title", "fields": fields})
    j.check("content type takes the id asked for", j.last["status"] == 201 and eq(sys_of(made).get("id"), ct),
            id=sys_of(made).get("id"))
    j.check("content type keeps its fields", ids_of(made.get("fields")) == ["title", "body", "servings"],
            fields=ids_of(made.get("fields")))
    v = version(made, 1)
    active = call("PUT", f"/content_types/{ct}/published", version=v)
    j.check("activation records the version published", eq(sys_of(active).get("publishedVersion"), v),
            sent_version=v, publishedVersion=sys_of(active).get("publishedVersion"))
    word = f"saffron{tag}"
    sent = {"title": {"en-US": f"Paella with {word}"}, "body": {"en-US": "Toast the rice first."},
            "servings": {"en-US": 4}}
    entry = call("POST", "/entries", content_type=ct, body={"fields": sent})
    j.check("entry names its content type",
            j.last["status"] == 201 and eq(link_id(sys_of(entry).get("contentType")), ct),
            contentType=link_id(sys_of(entry).get("contentType")))
    j.check("entry keeps its localized fields", eq(entry.get("fields"), sent), fields=entry.get("fields"))
    j.check("a new entry is not published",
            j.last["status"] == 201 and bool(sys_of(entry).get("id")) and sys_of(entry).get("publishedVersion") is None,
            publishedVersion=sys_of(entry).get("publishedVersion"))
    eid = sys_of(entry).get("id") or f"entry{tag}"
    read = call("GET", f"/entries/{eid}")
    j.check("read matches the created entry", eq(sys_of(read).get("id"), eid) and eq(read.get("fields"), sent))
    v = version(read, 1)
    published = call("PUT", f"/entries/{eid}/published", version=v)
    j.check("publish records the version published", eq(sys_of(published).get("publishedVersion"), v),
            sent_version=v, publishedVersion=sys_of(published).get("publishedVersion"))
    j.check("publish raises the version by one", eq(sys_of(published).get("version"), v + 1),
            sent_version=v, version=sys_of(published).get("version"))
    current = version(published, v + 1)
    changed = dict(sent, title={"en-US": f"Paella valenciana with {word}"})
    call("PUT", f"/entries/{eid}", version=current - 1, body={"fields": changed})
    j.check("a stale version is refused", j.last["status"] == 409, sent_version=current - 1)
    updated = call("PUT", f"/entries/{eid}", version=current, body={"fields": changed})
    j.check("the current version updates the entry",
            j.last["status"] == 200 and eq(obj(obj(updated.get("fields")).get("title")).get("en-US"),
                                           changed["title"]["en-US"]), sent_version=current)
    draft = call("POST", "/entries", content_type=ct, body={"fields": {"title": {"en-US": f"Gazpacho {tag}"}}})
    e2 = sys_of(draft).get("id") or f"draft{tag}"
    rows = dicts(call("GET", "/entries", query={"content_type": ct}).get("items"))
    listed = entry_ids(rows)
    j.check("content_type filter holds both entries",
            eid in listed and e2 in listed and all(eq(link_id(sys_of(r).get("contentType")), ct) for r in rows),
            listed=listed)
    listed = entry_ids(dicts(call("GET", "/entries", query={"query": word}).get("items")))
    j.check("full-text query finds the entry", eid in listed and e2 not in listed, listed=listed)
    call("DELETE", f"/entries/{e2}", version=version(draft, 1))
    deleted = j.last["status"]
    call("GET", f"/entries/{e2}")
    j.check("deleted entry is gone", j.last["status"] == 404, delete_status=deleted)


def digitalocean_steps(j):
    # Answers wrap their record under its own name. Creating a droplet is asynchronous,
    # so nothing here waits for it to become active. A tag is addressed by its name.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**8, 10**9)

    def droplets(query):
        return dicts(obj(j.call("GET", "/v2/droplets", query=query)).get("droplets"))

    label = f"web-{tag}"
    made = obj(obj(j.call("POST", "/v2/tags", body={"name": label})).get("tag"))
    j.check("tag keeps its name", j.last["status"] == 201 and eq(made.get("name"), label), tag_name=made.get("name"))
    sent = {"name": f"web-{tag}-1", "region": "nyc3", "size": "s-1vcpu-1gb", "image": "ubuntu-24-04-x64",
            "tags": [label], "monitoring": True}
    answer = obj(j.call("POST", "/v2/droplets", body=sent))
    droplet = obj(answer.get("droplet"))
    j.check("droplet is accepted with an id", j.last["status"] == 202 and is_int(droplet.get("id")), id=droplet.get("id"))
    j.check("droplet keeps its name, region and size",
            eq(droplet.get("name"), sent["name"]) and eq(obj(droplet.get("region")).get("slug"), "nyc3")
            and eq(droplet.get("size_slug"), "s-1vcpu-1gb"))
    j.check("droplet carries its tag", isinstance(droplet.get("tags"), list) and label in droplet["tags"],
            tags=droplet.get("tags"))
    links = dicts(obj(answer.get("links")).get("actions"))
    j.check("create links its create action", any(eq(a.get("rel"), "create") and is_int(a.get("id")) for a in links),
            links=links)
    d1 = own(droplet.get("id"))
    read = obj(obj(j.call("GET", f"/v2/droplets/{d1}")).get("droplet"))
    j.check("read matches the created droplet",
            eq(read.get("id"), d1) and eq(read.get("name"), sent["name"]) and eq(read.get("size_slug"), sent["size"]))
    second = obj(obj(j.call("POST", "/v2/droplets", body={
        "name": f"worker-{tag}", "region": "nyc3", "size": "s-1vcpu-1gb", "image": "ubuntu-24-04-x64"})).get("droplet"))
    d2 = own(second.get("id"))
    rows = droplets({"tag_name": label})
    j.check("tag filter holds only tagged droplets",
            bool(by_id(rows, d1)) and not by_id(rows, d2) and all(label in (r.get("tags") or []) for r in rows),
            listed=ids_of(rows))
    page = obj(j.call("GET", "/v2/droplets", query={"per_page": 1}))
    following = obj(obj(page.get("links")).get("pages")).get("next")
    j.check("per_page pages the droplets",
            len(dicts(page.get("droplets"))) == 1 and isinstance(following, str) and bool(following),
            listed=ids_of(page.get("droplets")), next=following)
    j.call("POST", f"/v2/tags/{label}/resources",
           body={"resources": [{"resource_id": str(d2), "resource_type": "droplet"}]})
    tagged = j.last["status"]
    got = obj(obj(j.call("GET", f"/v2/tags/{label}")).get("tag"))
    j.check("tag counts both droplets", eq(obj(obj(got.get("resources")).get("droplets")).get("count"), 2),
            tag_status=tagged, resources=got.get("resources"))
    rows = droplets({"tag_name": label})
    j.check("tag filter now holds the second droplet", bool(by_id(rows, d2)), listed=ids_of(rows))
    action = obj(obj(j.call("POST", f"/v2/droplets/{d1}/actions", body={"type": "power_off"})).get("action"))
    j.check("action keeps its type and droplet",
            j.last["status"] == 201 and eq(action.get("type"), "power_off") and eq(action.get("resource_id"), d1)
            and eq(action.get("resource_type"), "droplet"))
    aid = own(action.get("id"))
    rows = dicts(obj(j.call("GET", f"/v2/droplets/{d1}/actions")).get("actions"))
    j.check("droplet actions list the action", bool(by_id(rows, aid)), listed=ids_of(rows))
    got = obj(obj(j.call("GET", f"/v2/actions/{aid}")).get("action"))
    j.check("action reads back by id",
            eq(got.get("id"), aid) and eq(got.get("type"), "power_off") and eq(got.get("resource_id"), d1))
    j.call("DELETE", f"/v2/droplets/{d2}")
    deleted = j.last["status"]
    j.call("GET", f"/v2/droplets/{d2}")
    j.check("deleted droplet is gone", j.last["status"] == 404, delete_status=deleted)


BLIND4_JOURNEYS = {"pipedrive": pipedrive_steps, "zoom": zoom_steps, "sentry": sentry_steps,
                   "contentful": contentful_steps, "digitalocean": digitalocean_steps}


def run_journeys_blind4(srv, sink, args):
    run_journey_table(BLIND4_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- fifth blind journeys

# The same rule again, over five more contracts written for this suite from each real
# API's public documentation (scripts/llm-bench/contracts/blind5/). Between them they
# keep typed `fields` maps that are filtered by formula and sorted (Airtable), move a
# user through a lifecycle state machine with action endpoints (Okta), keep a folder
# tree with parents, moves, a trash and If-Match etags, and page with offset, limit and
# total_count (Box), assign, close and reopen conversations and search them with
# structured queries (Intercom), and book, reschedule and cancel against slots in a
# date range, with a version header per endpoint (Cal.com). No check gates the rest: a
# journey goes on with an id of its own when a call gave none. A check asserts only
# what its contract promises a client. Do not edit a check to suit results.
BLIND5_CHECKS = {
    "airtable": [
        "table keeps its fields", "create answers a record per record sent", "created records keep their fields",
        "read matches the created record", "patch changes the field sent", "patch keeps the fields it was not sent",
        "formula filter holds only matching records", "sort orders the records by the field",
        "pageSize 1 returns one record and an offset", "offset reaches the other record",
        "an unknown field is refused", "put clears the fields it was not sent", "comments list the comment",
        "deleted record is gone",
    ],
    "okta": [
        "a user created without activation is staged", "user keeps its profile", "read by login matches the user",
        "activation answers a token", "activated user is active", "partial update changes the field sent",
        "partial update keeps the rest of the profile", "search finds the user by department",
        "suspended user reads as suspended", "unsuspended user reads as active", "group members list the user",
        "user's groups list the group", "deactivated user is deprovisioned", "deleted user is not found",
    ],
    "box": [
        "folder lives in its parent", "child folder's path ends at its parent", "a taken name is refused",
        "web link keeps its url and parent", "read matches the created web link",
        "limit 1 returns one item and the total", "offset 1 reaches the other item",
        "a matching etag renames the folder", "a stale etag is refused", "a refused update changes nothing",
        "moved web link names its new parent", "trashed web link reads from the trash",
        "trash lists the trashed web link", "restored web link returns to its folder",
    ],
    "intercom": [
        "contact keeps its email and external id", "a second contact with the same email is refused",
        "contact search finds the contact by email", "message names the conversation it opened",
        "conversation lists its contact", "conversation source is the first message", "a new conversation is open",
        "assignment sets the admin assignee", "admin reply shows as a conversation part",
        "close sets the state to closed", "search by state and contact finds the conversation",
        "reopen sets the state to open", "tag shows on the conversation", "deleted contact is gone",
    ],
    "calcom": [
        "event type keeps its slug and length", "event type is owned by the caller", "patch changes the length",
        "patch keeps the title and slug", "slots fall inside the asked range",
        "booking keeps its start and event type", "booking lasts the event type's new length",
        "booking lists its attendee", "read by uid matches the booking", "date-range list holds the booking",
        "reschedule names the original booking", "original booking points to its reschedule",
        "cancelled booking reads as cancelled",
    ],
}
JOURNEY_CHECKS.update(BLIND5_CHECKS)
BLIND5_DIR = "scripts/llm-bench/contracts/blind5"
# Contracts alone: no persona.
BLIND5_MOCKS = {name: (f"{BLIND5_DIR}/{name}.openapi.yaml", f"/{name}", None) for name in BLIND5_CHECKS}
ALNUM = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"


def made_up(j, prefix, n, alphabet=ALNUM):
    return prefix + "".join(j.rng.choice(alphabet) for _ in range(n))


def given(value, fallback):
    """`value` when it is a non-empty string, else `fallback`."""
    return value if isinstance(value, str) and value else fallback


def airtable_steps(j):
    # Records are addressed by table id; the table is new, so its records are the two
    # made here. Empty fields are left out of `fields`, which is what PUT is checked by.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    base = made_up(j, "app", 14)
    fields = [{"name": "Name", "type": "singleLineText"},
              {"name": "Status", "type": "singleSelect",
               "options": {"choices": [{"name": "Todo"}, {"name": "In progress"}, {"name": "Done"}]}},
              {"name": "Quantity", "type": "number", "options": {"precision": 0}},
              {"name": "Due date", "type": "date", "options": {"dateFormat": {"name": "iso"}}}]
    table = obj(j.call("POST", f"/v0/meta/bases/{base}/tables", body={"name": f"Orders {tag}", "fields": fields}))
    j.check("table keeps its fields", j.last["status"] == 200 and ids_of(table.get("fields"), "name") == ids_of(fields, "name"),
            fields=ids_of(table.get("fields"), "name"))
    path = f"/v0/{base}/{given(table.get('id'), made_up(j, 'tbl', 14))}"
    word = f"walnut{tag}"
    a = {"Name": f"Desk {word}", "Status": "Todo", "Quantity": 3, "Due date": "2026-12-01"}
    b = {"Name": f"Chair {tag}", "Status": "Done", "Quantity": 1}
    made = dicts(obj(j.call("POST", path, body={"records": [{"fields": a}, {"fields": b}]})).get("records"))
    j.check("create answers a record per record sent",
            j.last["status"] == 200 and len(made) == 2 and all(str(r.get("id", "")).startswith("rec") for r in made),
            listed=ids_of(made))
    j.check("created records keep their fields",
            len(made) == 2 and eq(made[0].get("fields"), a) and eq(made[1].get("fields"), b),
            fields=[r.get("fields") for r in made])
    r1 = given(made[0].get("id") if made else None, made_up(j, "rec", 14))
    r2 = given(made[1].get("id") if len(made) > 1 else None, made_up(j, "rec", 14))
    read = obj(j.call("GET", f"{path}/{r1}"))
    j.check("read matches the created record", eq(read.get("id"), r1) and eq(read.get("fields"), a),
            fields=read.get("fields"))
    patched = obj(j.call("PATCH", f"{path}/{r1}", body={"fields": {"Status": "In progress"}}))
    j.check("patch changes the field sent", eq(obj(patched.get("fields")).get("Status"), "In progress"),
            fields=patched.get("fields"))
    read = obj(obj(j.call("GET", f"{path}/{r1}")).get("fields"))
    j.check("patch keeps the fields it was not sent", same(read, a, "Name", "Quantity", "Due date"), fields=read)
    rows = dicts(obj(j.call("GET", path, query={"filterByFormula": "{Status} = 'Done'"})).get("records"))
    j.check("formula filter holds only matching records",
            bool(by_id(rows, r2)) and not by_id(rows, r1)
            and all(eq(obj(r.get("fields")).get("Status"), "Done") for r in rows), listed=ids_of(rows))
    rows = dicts(obj(j.call("GET", path, query={"sort[0][field]": "Quantity", "sort[0][direction]": "desc"})).get("records"))
    j.check("sort orders the records by the field", ids_of(rows) == [r1, r2], listed=ids_of(rows))
    page = obj(j.call("GET", path, query={"pageSize": 1}))
    first = dicts(page.get("records"))
    offset = page.get("offset")
    j.check("pageSize 1 returns one record and an offset", len(first) == 1 and isinstance(offset, str) and bool(offset),
            listed=ids_of(first), offset=offset)
    query = {"pageSize": 1}
    if isinstance(offset, str) and offset:
        query["offset"] = offset
    nxt = dicts(obj(j.call("GET", path, query=query)).get("records"))
    j.check("offset reaches the other record", len(nxt) == 1 and same_items(ids_of(first + nxt), [r1, r2]),
            first=ids_of(first), listed=ids_of(nxt))
    j.call("PATCH", f"{path}/{r1}", body={"fields": {f"Colour {tag}": "red"}})
    j.check("an unknown field is refused", j.last["status"] == 422)
    j.call("PUT", f"{path}/{r1}", body={"fields": {"Name": a["Name"], "Status": "Done"}})
    replaced = j.last["status"]
    read = obj(obj(j.call("GET", f"{path}/{r1}")).get("fields"))
    j.check("put clears the fields it was not sent",
            eq(read.get("Name"), a["Name"]) and "Quantity" not in read and "Due date" not in read,
            put_status=replaced, fields=read)
    text = f"Shipped in two boxes, ref {tag}."
    comment = obj(j.call("POST", f"{path}/{r1}/comments", body={"text": text}))
    cid = given(comment.get("id"), made_up(j, "com", 14))
    rows = dicts(obj(j.call("GET", f"{path}/{r1}/comments")).get("comments"))
    j.check("comments list the comment", eq(by_id(rows, cid).get("text"), text), listed=ids_of(rows))
    j.call("DELETE", f"{path}/{r2}")
    deleted = j.last["status"]
    j.call("GET", f"{path}/{r2}")
    j.check("deleted record is gone", j.last["status"] == 404, delete_status=deleted)


def okta_steps(j):
    # Status changes only through the lifecycle operations, so each one is read back.
    # The user has a password, so activation makes it ACTIVE rather than PROVISIONED.
    # A department unique to this run lets search find exactly this user.
    j.headers["authorization"] = "SSWS bench-placeholder"
    tag = j.rng.randrange(10**5, 10**6)

    def status(uid):
        return obj(j.call("GET", f"/api/v1/users/{uid}")).get("status")

    login = f"ana.silva.{tag}@example.org"
    profile = {"firstName": "Ana", "lastName": "Silva", "email": login, "login": login,
               "department": "Finance", "title": "Analyst"}
    user = obj(j.call("POST", "/api/v1/users", query={"activate": "false"}, body={
        "profile": profile, "credentials": {"password": {"value": f"bench-placeholder-{tag}"}}}))
    j.check("a user created without activation is staged", j.last["status"] == 200 and eq(user.get("status"), "STAGED"),
            user_status=user.get("status"))
    j.check("user keeps its profile", same(obj(user.get("profile")), profile, "firstName", "lastName", "email", "login"))
    uid = given(user.get("id"), made_up(j, "00u", 17))
    read = obj(j.call("GET", f"/api/v1/users/{login}"))
    j.check("read by login matches the user", eq(read.get("id"), uid), id=read.get("id"))
    token = obj(j.call("POST", f"/api/v1/users/{uid}/lifecycle/activate", query={"sendEmail": "false"}))
    j.check("activation answers a token",
            j.last["status"] == 200 and isinstance(token.get("activationToken"), str) and bool(token["activationToken"]))
    now = status(uid)
    j.check("activated user is active", eq(now, "ACTIVE"), user_status=now)
    dept = f"Treasury {tag}"
    updated = obj(j.call("POST", f"/api/v1/users/{uid}", body={"profile": {"department": dept}}))
    j.check("partial update changes the field sent", eq(obj(updated.get("profile")).get("department"), dept))
    read = obj(obj(j.call("GET", f"/api/v1/users/{uid}")).get("profile"))
    j.check("partial update keeps the rest of the profile",
            same(read, profile, "firstName", "lastName", "login", "title"), profile=read)
    rows = dicts(j.call("GET", "/api/v1/users", query={"search": f'profile.department eq "{dept}"'}))
    j.check("search finds the user by department",
            bool(by_id(rows, uid)) and all(eq(obj(r.get("profile")).get("department"), dept) for r in rows),
            listed=ids_of(rows))
    j.call("POST", f"/api/v1/users/{uid}/lifecycle/suspend")
    acted = j.last["status"]
    now = status(uid)
    j.check("suspended user reads as suspended", eq(now, "SUSPENDED"), action_status=acted, user_status=now)
    j.call("POST", f"/api/v1/users/{uid}/lifecycle/unsuspend")
    acted = j.last["status"]
    now = status(uid)
    j.check("unsuspended user reads as active", eq(now, "ACTIVE"), action_status=acted, user_status=now)
    group = obj(j.call("POST", "/api/v1/groups", body={
        "profile": {"name": f"Treasury team {tag}", "description": "Everyone in treasury."}}))
    gid = given(group.get("id"), made_up(j, "00g", 17))
    j.call("PUT", f"/api/v1/groups/{gid}/users/{uid}")
    added = j.last["status"]
    rows = j.call("GET", f"/api/v1/groups/{gid}/users")
    j.check("group members list the user", bool(by_id(rows, uid)), add_status=added, listed=ids_of(rows))
    rows = j.call("GET", f"/api/v1/users/{uid}/groups")
    j.check("user's groups list the group", bool(by_id(rows, gid)), listed=ids_of(rows))
    j.call("POST", f"/api/v1/users/{uid}/lifecycle/deactivate")
    acted = j.last["status"]
    now = status(uid)
    j.check("deactivated user is deprovisioned", eq(now, "DEPROVISIONED"), action_status=acted, user_status=now)
    j.call("DELETE", f"/api/v1/users/{uid}")
    deleted = j.last["status"]
    j.call("GET", f"/api/v1/users/{uid}")
    j.check("deleted user is not found", j.last["status"] == 404, delete_status=deleted)


def box_steps(j):
    # Folders are made under the root, "0". Only folder writes carry If-Match. The
    # child folder holds the two web links, so its total_count is 2 while paging.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)

    def own(value):
        return value if isinstance(value, str) and value.isdigit() else str(j.rng.randrange(10**11, 10**12))

    def call(method, path, if_match=None, **kw):
        if if_match is not None:
            j.headers["If-Match"] = if_match
        try:
            return obj(j.call(method, path, **kw))
        finally:
            j.headers.pop("If-Match", None)

    def items(folder, **query):
        return call("GET", f"/2.0/folders/{folder}/items", query=query or None)

    name = f"Projects {tag}"
    top = call("POST", "/2.0/folders", body={"name": name, "parent": {"id": "0"}})
    j.check("folder lives in its parent",
            j.last["status"] == 201 and eq(top.get("name"), name) and eq(obj(top.get("parent")).get("id"), "0"))
    p = own(top.get("id"))
    child = call("POST", "/2.0/folders", body={"name": "Drafts", "parent": {"id": p}})
    path = ids_of(obj(child.get("path_collection")).get("entries"))
    j.check("child folder's path ends at its parent", len(path) >= 2 and path[0] == "0" and path[-1] == p, path=path)
    c = own(child.get("id"))
    etag = given(child.get("etag"), "0")
    call("POST", "/2.0/folders", body={"name": "Drafts", "parent": {"id": p}})
    j.check("a taken name is refused", j.last["status"] == 409)
    url = f"https://example.org/specs/{tag}"
    link = call("POST", "/2.0/web_links", body={"url": url, "parent": {"id": c}, "name": f"Spec {tag}",
                                                 "description": "The launch spec."})
    j.check("web link keeps its url and parent",
            j.last["status"] == 201 and eq(link.get("url"), url) and eq(obj(link.get("parent")).get("id"), c))
    l1 = own(link.get("id"))
    read = call("GET", f"/2.0/web_links/{l1}")
    j.check("read matches the created web link",
            eq(read.get("id"), l1) and eq(read.get("url"), url) and eq(read.get("name"), f"Spec {tag}"))
    other = call("POST", "/2.0/web_links", body={"url": f"https://example.org/notes/{tag}", "parent": {"id": c},
                                                  "name": f"Notes {tag}"})
    l2 = own(other.get("id"))
    page = items(c, offset=0, limit=1)
    first = ids_of(page.get("entries"))
    j.check("limit 1 returns one item and the total", len(first) == 1 and eq(page.get("total_count"), 2),
            listed=first, total_count=page.get("total_count"))
    nxt = ids_of(items(c, offset=1, limit=1).get("entries"))
    j.check("offset 1 reaches the other item", len(nxt) == 1 and same_items(first + nxt, [l1, l2]),
            first=first, listed=nxt)
    renamed = f"Drafts {tag}"
    answer = call("PUT", f"/2.0/folders/{c}", if_match=etag, body={"name": renamed})
    j.check("a matching etag renames the folder", j.last["status"] == 200 and eq(answer.get("name"), renamed),
            sent_etag=etag, etag=answer.get("etag"))
    call("PUT", f"/2.0/folders/{c}", if_match=etag, body={"name": f"Drafts {tag} old"})
    j.check("a stale etag is refused", j.last["status"] == 412, sent_etag=etag)
    read = call("GET", f"/2.0/folders/{c}")
    j.check("a refused update changes nothing", eq(read.get("name"), renamed), folder_name=read.get("name"))
    call("PUT", f"/2.0/web_links/{l2}", body={"parent": {"id": p}})
    moved = j.last["status"]
    read = call("GET", f"/2.0/web_links/{l2}")
    j.check("moved web link names its new parent", eq(obj(read.get("parent")).get("id"), p),
            move_status=moved, parent=obj(read.get("parent")).get("id"))
    call("DELETE", f"/2.0/web_links/{l1}")
    trashed = j.last["status"]
    read = call("GET", f"/2.0/web_links/{l1}/trash")
    j.check("trashed web link reads from the trash", eq(read.get("id"), l1) and eq(read.get("item_status"), "trashed"),
            delete_status=trashed, item_status=read.get("item_status"))
    listed = ids_of(call("GET", "/2.0/folders/trash/items", query={"limit": 1000}).get("entries"))
    j.check("trash lists the trashed web link", l1 in listed, listed=listed)
    call("POST", f"/2.0/web_links/{l1}", body={})
    restored = j.last["status"]
    listed = ids_of(items(c).get("entries"))
    j.check("restored web link returns to its folder", l1 in listed, restore_status=restored, listed=listed)


def intercom_steps(j):
    # The conversation is started by the contact; its state then changes only through
    # /parts, and each change is read back. The admin is the workspace's first one.
    j.headers.update(HELDOUT_AUTH)
    j.headers["Intercom-Version"] = "2.11"
    tag = j.rng.randrange(10**5, 10**6)

    def conversation(cid):
        return obj(j.call("GET", f"/conversations/{cid}"))

    def manage(conv, admin, kind, **extra):
        return obj(j.call("POST", f"/conversations/{conv}/parts",
                          body=dict({"message_type": kind, "type": "admin", "admin_id": admin}, **extra)))

    admin = given(head(obj(j.call("GET", "/admins")).get("admins")).get("id"), str(j.rng.randrange(10**6, 10**7)))
    email, external = f"ana.{tag}@example.org", f"cust-{tag}"
    contact = obj(j.call("POST", "/contacts", body={"role": "user", "external_id": external, "email": email,
                                                    "name": "Ana Silva"}))
    j.check("contact keeps its email and external id",
            j.last["status"] == 200 and eq(contact.get("email"), email) and eq(contact.get("external_id"), external))
    cid = given(contact.get("id"), made_up(j, "", 24, "0123456789abcdef"))
    j.call("POST", "/contacts", body={"role": "user", "email": email, "name": "Ana S."})
    j.check("a second contact with the same email is refused", j.last["status"] == 409)
    rows = dicts(obj(j.call("POST", "/contacts/search",
                            body={"query": {"field": "email", "operator": "=", "value": email}})).get("data"))
    j.check("contact search finds the contact by email",
            bool(by_id(rows, cid)) and all(eq(r.get("email"), email) for r in rows), listed=ids_of(rows))
    text = f"My order {tag} arrived damaged."
    message = obj(j.call("POST", "/conversations", body={"from": {"type": "user", "id": cid}, "body": text}))
    j.check("message names the conversation it opened",
            j.last["status"] == 200 and isinstance(message.get("conversation_id"), str) and bool(message["conversation_id"]),
            conversation_id=message.get("conversation_id"))
    conv = given(message.get("conversation_id"), str(j.rng.randrange(10**11, 10**12)))
    got = conversation(conv)
    j.check("conversation lists its contact", cid in ids_of(obj(got.get("contacts")).get("contacts")),
            contacts=ids_of(obj(got.get("contacts")).get("contacts")))
    body = obj(got.get("source")).get("body")
    j.check("conversation source is the first message", isinstance(body, str) and text in body, body=body)
    j.check("a new conversation is open", eq(got.get("state"), "open"), state=got.get("state"))
    assigned = manage(conv, admin, "assignment", assignee_id=admin)
    j.check("assignment sets the admin assignee",
            assigned.get("admin_assignee_id") is not None and str(assigned.get("admin_assignee_id")) == admin,
            admin_assignee_id=assigned.get("admin_assignee_id"), admin=admin)
    reply = f"Sorry about order {tag}; a replacement ships today."
    j.call("POST", f"/conversations/{conv}/reply",
           body={"message_type": "comment", "type": "admin", "admin_id": admin, "body": reply})
    replied = j.last["status"]
    parts = dicts(obj(conversation(conv).get("conversation_parts")).get("conversation_parts"))
    j.check("admin reply shows as a conversation part",
            any(eq(p.get("part_type"), "comment") and isinstance(p.get("body"), str) and reply in p["body"]
                and str(obj(p.get("author")).get("id")) == admin for p in parts),
            reply_status=replied, parts=ids_of(parts, "part_type"))
    manage(conv, admin, "close")
    closed = j.last["status"]
    got = conversation(conv)
    j.check("close sets the state to closed", eq(got.get("state"), "closed"), close_status=closed, state=got.get("state"))
    query = {"operator": "AND", "value": [{"field": "state", "operator": "=", "value": "closed"},
                                          {"field": "contact_ids", "operator": "=", "value": cid}]}
    rows = dicts(obj(j.call("POST", "/conversations/search", body={"query": query})).get("conversations"))
    j.check("search by state and contact finds the conversation",
            bool(by_id(rows, conv)) and all(eq(r.get("state"), "closed") for r in rows), listed=ids_of(rows))
    reopened = manage(conv, admin, "open")
    j.check("reopen sets the state to open", eq(reopened.get("state"), "open"), state=reopened.get("state"))
    label = obj(j.call("POST", "/tags", body={"name": f"damaged-{tag}"}))
    tid = given(label.get("id"), str(j.rng.randrange(10**6, 10**7)))
    j.call("POST", f"/conversations/{conv}/tags", body={"id": tid, "admin_id": admin})
    tagged = j.last["status"]
    listed = ids_of(obj(conversation(conv).get("tags")).get("tags"))
    j.check("tag shows on the conversation", tid in listed, tag_status=tagged, listed=listed)
    j.call("DELETE", f"/contacts/{cid}")
    deleted = j.last["status"]
    j.call("GET", f"/contacts/{cid}")
    j.check("deleted contact is gone", j.last["status"] == 404, delete_status=deleted)


def calcom_steps(j):
    # Each endpoint gets the cal-api-version its contract names. The booking takes the
    # first free slot answered, or a weekday morning when there was none, and is made
    # after the length changed, so it lasts the new length.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    event_types, slots_v, bookings, listing = "2024-06-14", "2024-09-04", "2026-02-25", "2026-05-01"

    def call(method, path, version=None, **kw):
        if version:
            j.headers["cal-api-version"] = version
        try:
            return obj(j.call(method, path, **kw))
        finally:
            j.headers.pop("cal-api-version", None)

    def data(value):
        return obj(obj(value).get("data"))

    def utc(moment):
        return moment.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    me = data(call("GET", "/v2/me"))
    sent = {"title": f"Intro call {tag}", "slug": f"intro-{tag}", "lengthInMinutes": 30,
            "description": "A first talk about the project.", "minimumBookingNotice": 0}
    made = data(call("POST", "/v2/event-types", event_types, body=sent))
    j.check("event type keeps its slug and length", j.last["status"] == 201 and same(made, sent, "slug", "lengthInMinutes"))
    j.check("event type is owned by the caller", is_int(me.get("id")) and eq(made.get("ownerId"), me.get("id")),
            ownerId=made.get("ownerId"), me=me.get("id"))
    etid = made.get("id") if is_int(made.get("id")) else j.rng.randrange(10**5, 10**6)
    patched = data(call("PATCH", f"/v2/event-types/{etid}", event_types, body={"lengthInMinutes": 45}))
    j.check("patch changes the length", eq(patched.get("lengthInMinutes"), 45), lengthInMinutes=patched.get("lengthInMinutes"))
    read = data(call("GET", f"/v2/event-types/{etid}", event_types))
    j.check("patch keeps the title and slug", same(read, sent, "title", "slug"))
    days = data(call("GET", "/v2/slots", slots_v, query={"eventTypeId": etid, "start": "2026-12-07",
                                                         "end": "2026-12-11", "timeZone": "UTC"}))
    starts = [s.get("start") for day in days.values() for s in dicts(day)]
    lo, hi = datetime(2026, 12, 7, tzinfo=timezone.utc), datetime(2026, 12, 12, tzinfo=timezone.utc)
    inside = [s for s in starts if instant(s) and instant(s).tzinfo and lo <= instant(s) < hi]
    j.check("slots fall inside the asked range", bool(starts) and len(inside) == len(starts), starts=starts[:5])
    start = utc(instant(inside[0])) if inside else "2026-12-08T10:00:00Z"
    email = f"ana.{tag}@example.org"
    booking = data(call("POST", "/v2/bookings", bookings, body={
        "start": start, "eventTypeId": etid, "attendee": {"name": "Ana Silva", "email": email, "timeZone": "Europe/Lisbon"},
        "metadata": {"ref": f"bench-{tag}"}}))
    j.check("booking keeps its start and event type",
            j.last["status"] == 201 and eq(instant(booking.get("start")), instant(start))
            and eq(booking.get("eventTypeId"), etid), start=booking.get("start"), eventTypeId=booking.get("eventTypeId"))
    began, ended = instant(booking.get("start")), instant(booking.get("end"))
    j.check("booking lasts the event type's new length",
            began is not None and ended is not None and ended - began == timedelta(minutes=45),
            start=booking.get("start"), end=booking.get("end"))
    j.check("booking lists its attendee", email in ids_of(booking.get("attendees"), "email"),
            attendees=ids_of(booking.get("attendees"), "email"))
    uid = given(booking.get("uid"), made_up(j, "", 22))
    read = data(call("GET", f"/v2/bookings/{uid}", bookings))
    j.check("read by uid matches the booking", eq(read.get("uid"), uid) and eq(instant(read.get("start")), instant(start)))
    rows = dicts(call("GET", "/v2/bookings", listing, query={"attendeeEmail": email, "afterStart": "2026-12-07",
                                                              "beforeEnd": "2026-12-12"}).get("data"))
    j.check("date-range list holds the booking", uid in ids_of(rows, "uid"), listed=ids_of(rows, "uid"))
    later = utc(instant(start) + timedelta(days=1))
    moved = data(call("POST", f"/v2/bookings/{uid}/reschedule", bookings,
                      body={"start": later, "reschedulingReason": "A clash with a board meeting."}))
    j.check("reschedule names the original booking",
            j.last["status"] == 201 and eq(moved.get("rescheduledFromUid"), uid)
            and eq(instant(moved.get("start")), instant(later)), rescheduledFromUid=moved.get("rescheduledFromUid"))
    new_uid = given(moved.get("uid"), made_up(j, "", 22))
    old = data(call("GET", f"/v2/bookings/{uid}", bookings))
    j.check("original booking points to its reschedule", eq(old.get("rescheduledToUid"), new_uid),
            rescheduledToUid=old.get("rescheduledToUid"), new_uid=new_uid)
    reason = f"No longer needed, ref {tag}."
    call("POST", f"/v2/bookings/{new_uid}/cancel", bookings, body={"cancellationReason": reason})
    cancelled = j.last["status"]
    got = data(call("GET", f"/v2/bookings/{new_uid}", bookings))
    j.check("cancelled booking reads as cancelled",
            eq(got.get("status"), "cancelled") and eq(got.get("cancellationReason"), reason),
            cancel_status=cancelled, booking_status=got.get("status"))


BLIND5_JOURNEYS = {"airtable": airtable_steps, "okta": okta_steps, "box": box_steps,
                   "intercom": intercom_steps, "calcom": calcom_steps}


def run_journeys_blind5(srv, sink, args):
    run_journey_table(BLIND5_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- sixth blind journeys

# The same rule again, over five more contracts written for this suite from each real
# API's public documentation (scripts/llm-bench/contracts/blind6/). They were picked for
# API styles: RPC calls that are all POSTs with JSON arguments, 409 errors as tagged
# unions and a continue cursor (Dropbox), JSON:API documents with links, relationships,
# filter expressions and page[cursor] (Klaviyo), integer ids, bare arrays paged by page
# number and child notes (WooCommerce), resources nested two levels under a page, with
# wrapped bodies and incident updates (Statuspage), and an invoice state machine driven
# by status action endpoints and payments, inside a response envelope (Zoho Books). No
# check gates the rest: a journey goes on with an id of its own when a call gave none. A
# check asserts only what its contract promises a client. Do not edit a check to suit
# results.
BLIND6_CHECKS = {
    "dropbox": [
        "folder keeps its path", "a taken path is refused", "metadata reads the folder back",
        "metadata by id names the path", "list_folder holds the children", "limit 1 returns one entry and a cursor",
        "continue reaches the other entry", "move answers the new path", "moved folder is gone from its old path",
        "moved folder keeps its id", "tags list the added tag", "a removed tag is gone",
        "deleted folder is not found", "deleting a folder removes its children",
    ],
    "klaviyo": [
        "profile keeps its email and names", "profile links to itself", "a taken email names the profile that has it",
        "read matches the created profile", "patch changes the attribute sent",
        "patch keeps the attributes it was not sent", "email filter finds only the profile",
        "page size 1 answers one profile and a next link", "next link reaches the other profile",
        "list keeps its name", "list profiles hold the added profiles", "profile's lists hold the list",
        "a removed profile leaves the list", "deleted list is not found",
    ],
    "woocommerce": [
        "product keeps its sku and price", "product names its category", "a taken sku is refused",
        "read matches the created product", "update changes the fields sent", "update keeps the fields it was not sent",
        "per_page 1 answers one product", "page 2 answers the other product", "sku filter finds only the product",
        "order line names the product and quantity", "order line costs the price times the quantity",
        "order notes list the note", "completed order reads as completed", "deleted product is not found",
    ],
    "statuspage": [
        "component keeps its page, name and status", "incident keeps its name and status",
        "first update carries the body", "affected component shows the outage",
        "status change adds an incident update", "edited update keeps its new body",
        "incident subscribers list the subscriber", "unresolved list holds the incident",
        "per_page 1 answers one incident", "page 2 reaches the other incident", "an unknown status is refused",
        "resolved incident leaves the unresolved list", "resolving restores the component",
        "deleted incident is not found",
    ],
    "zohobooks": [
        "a new invoice is a draft", "invoice total adds its lines", "a taken invoice number is refused",
        "read matches the created invoice", "update changes the reference and keeps the lines",
        "marked-sent invoice reads as sent", "payment applies to the invoice", "a part payment leaves the rest owed",
        "status filter finds the partly paid invoice", "per_page 1 answers one invoice and more pages",
        "page 2 reaches the other invoice", "voided invoice reads as void", "a void invoice returns to draft",
        "deleted invoice is not found",
    ],
}
JOURNEY_CHECKS.update(BLIND6_CHECKS)
BLIND6_DIR = "scripts/llm-bench/contracts/blind6"
# Contracts alone: no persona.
BLIND6_MOCKS = {name: (f"{BLIND6_DIR}/{name}.openapi.yaml", f"/{name}", None) for name in BLIND6_CHECKS}
LOWER_ALNUM = "abcdefghijklmnopqrstuvwxyz0123456789"


def dropbox_steps(j):
    # Every call is a POST of JSON arguments. The folder name is mixed case, so
    # path_lower and path_display differ. Errors the contract declares answer 409 with
    # an error_summary that starts with the error's tags.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)

    def rpc(route, **args):
        return obj(j.call("POST", f"/2/{route}", body=args))

    def summary(answer):
        return answer.get("error_summary") if isinstance(answer.get("error_summary"), str) else ""

    def own(value):
        return value if isinstance(value, str) and value.startswith("id:") else made_up(j, "id:", 22)

    def tags_of(path):
        rows = dicts(rpc("files/tags/get", paths=[path]).get("paths_to_tags"))
        return [t.get("tag_text") for r in rows for t in dicts(r.get("tags"))]

    name = f"Reports-{tag}"
    top = f"/{name}"
    made = obj(rpc("files/create_folder_v2", path=top, autorename=False).get("metadata"))
    j.check("folder keeps its path",
            j.last["status"] == 200 and eq(made.get("name"), name) and eq(made.get("path_lower"), top.lower()),
            folder_name=made.get("name"), path_lower=made.get("path_lower"))
    fid = own(made.get("id"))
    again = rpc("files/create_folder_v2", path=top, autorename=False)
    j.check("a taken path is refused", j.last["status"] == 409 and summary(again).startswith("path/conflict"),
            error_summary=summary(again))
    read = rpc("files/get_metadata", path=top)
    j.check("metadata reads the folder back",
            eq(read.get(".tag"), "folder") and eq(read.get("id"), fid) and eq(read.get("path_display"), top),
            tag=read.get(".tag"), id=read.get("id"), path_display=read.get("path_display"))
    read = rpc("files/get_metadata", path=fid)
    j.check("metadata by id names the path", eq(read.get("path_lower"), top.lower()), path_lower=read.get("path_lower"))
    a_path, b_path = f"{top}/Drafts", f"{top}/Archive"
    a = own(obj(rpc("files/create_folder_v2", path=a_path).get("metadata")).get("id"))
    b = own(obj(rpc("files/create_folder_v2", path=b_path).get("metadata")).get("id"))
    rows = dicts(rpc("files/list_folder", path=top).get("entries"))
    j.check("list_folder holds the children",
            same_items(ids_of(rows), [a, b])
            and all(str(r.get("path_lower", "")).startswith(top.lower() + "/") for r in rows), listed=ids_of(rows))
    page = rpc("files/list_folder", path=top, limit=1)
    first, cursor = dicts(page.get("entries")), page.get("cursor")
    j.check("limit 1 returns one entry and a cursor",
            len(first) == 1 and page.get("has_more") is True and isinstance(cursor, str) and bool(cursor),
            listed=ids_of(first), has_more=page.get("has_more"))
    nxt = rpc("files/list_folder/continue", cursor=given(cursor, made_up(j, "AAG", 40)))
    rest = dicts(nxt.get("entries"))
    j.check("continue reaches the other entry",
            len(rest) == 1 and same_items(ids_of(first + rest), [a, b]) and nxt.get("has_more") is False,
            first=ids_of(first), listed=ids_of(rest), has_more=nxt.get("has_more"))
    to = f"{a_path}/Archive"
    moved = obj(rpc("files/move_v2", from_path=b_path, to_path=to).get("metadata"))
    j.check("move answers the new path", j.last["status"] == 200 and eq(moved.get("path_lower"), to.lower()),
            path_lower=moved.get("path_lower"))
    gone = rpc("files/get_metadata", path=b_path)
    j.check("moved folder is gone from its old path",
            j.last["status"] == 409 and summary(gone).startswith("path/not_found"), error_summary=summary(gone))
    read = rpc("files/get_metadata", path=to)
    j.check("moved folder keeps its id", eq(read.get("id"), b), id=read.get("id"), was=b)
    word = f"review{tag}"
    rpc("files/tags/add", path=a_path, tag_text=word)
    added = j.last["status"]
    listed = tags_of(a_path)
    j.check("tags list the added tag", word in listed, add_status=added, tags=listed)
    rpc("files/tags/remove", path=a_path, tag_text=word)
    removed = j.last["status"]
    listed = tags_of(a_path)
    j.check("a removed tag is gone", removed == 200 and j.last["status"] == 200 and word not in listed,
            remove_status=removed, tags=listed)
    rpc("files/delete_v2", path=a_path)
    deleted = j.last["status"]
    gone = rpc("files/get_metadata", path=a_path)
    j.check("deleted folder is not found", j.last["status"] == 409 and summary(gone).startswith("path/not_found"),
            delete_status=deleted, error_summary=summary(gone))
    gone = rpc("files/get_metadata", path=to)
    j.check("deleting a folder removes its children",
            j.last["status"] == 409 and summary(gone).startswith("path/not_found"), error_summary=summary(gone))


def klaviyo_steps(j):
    # JSON:API documents throughout, with the revision header on every call. Emails
    # unique to this run make the filters match only this run's profiles, and the list
    # is new, so its members are the ones added here.
    j.headers["authorization"] = "Klaviyo-API-Key pk_bench-placeholder"
    j.headers["revision"] = "2024-10-15"
    j.headers["accept"] = "application/vnd.api+json"
    tag = j.rng.randrange(10**5, 10**6)

    def data(value):
        return obj(obj(value).get("data"))

    def rows_of(value):
        return dicts(obj(value).get("data"))

    def own(value):
        return value if isinstance(value, str) and value else made_up(j, "01", 24, "0123456789ABCDEFGHJKMNPQRSTVWXYZ")

    def cursor(links):
        nxt = obj(links).get("next")
        if not isinstance(nxt, str):
            return None
        values = urllib.parse.parse_qs(urllib.parse.urlsplit(nxt).query).get("page[cursor]")
        return values[0] if values else None

    def profile(attributes, pid=None):
        res = {"type": "profile", "attributes": attributes}
        if pid:
            res["id"] = pid
        return {"data": res}

    e1, e2 = f"ana.{tag}@example.org", f"rui.{tag}@example.org"
    sent = {"email": e1, "first_name": "Ana", "last_name": "Silva", "properties": {"plan": "pro", "ref": f"bench-{tag}"}}
    made = data(j.call("POST", "/api/profiles", body=profile(sent)))
    j.check("profile keeps its email and names",
            j.last["status"] == 201 and eq(made.get("type"), "profile")
            and same(obj(made.get("attributes")), sent, "email", "first_name", "last_name"),
            attributes=made.get("attributes"))
    p1 = own(made.get("id"))
    link = obj(made.get("links")).get("self")
    j.check("profile links to itself", isinstance(link, str) and p1 in link, self_link=link)
    dup = obj(j.call("POST", "/api/profiles", body=profile({"email": e1, "first_name": "Ana"})))
    err = head(dup.get("errors"))
    j.check("a taken email names the profile that has it",
            j.last["status"] == 409 and eq(err.get("code"), "duplicate_profile")
            and eq(obj(err.get("meta")).get("duplicate_profile_id"), p1),
            code=err.get("code"), meta=err.get("meta"))
    read = data(j.call("GET", f"/api/profiles/{p1}"))
    j.check("read matches the created profile",
            eq(read.get("id"), p1) and same(obj(read.get("attributes")), sent, "email", "first_name", "last_name"))
    patched = obj(data(j.call("PATCH", f"/api/profiles/{p1}", body=profile({"last_name": "Costa"}, p1))).get("attributes"))
    j.check("patch changes the attribute sent", eq(patched.get("last_name"), "Costa"), last_name=patched.get("last_name"))
    read = obj(data(j.call("GET", f"/api/profiles/{p1}")).get("attributes"))
    j.check("patch keeps the attributes it was not sent",
            same(read, sent, "email", "first_name") and eq(obj(read.get("properties")).get("plan"), "pro"), attributes=read)
    p2 = own(data(j.call("POST", "/api/profiles",
                         body=profile({"email": e2, "first_name": "Rui", "last_name": "Mendes"}))).get("id"))
    rows = rows_of(j.call("GET", "/api/profiles", query={"filter": f'equals(email,"{e1}")'}))
    j.check("email filter finds only the profile", ids_of(rows) == [p1], listed=ids_of(rows))
    both = f'any(email,["{e1}","{e2}"])'
    page = obj(j.call("GET", "/api/profiles", query={"filter": both, "page[size]": 1}))
    first, cur = rows_of(page), cursor(page.get("links"))
    j.check("page size 1 answers one profile and a next link", len(first) == 1 and bool(cur),
            listed=ids_of(first), next=obj(page.get("links")).get("next"))
    query = {"filter": both, "page[size]": 1}
    if cur:
        query["page[cursor]"] = cur
    rest = rows_of(j.call("GET", "/api/profiles", query=query))
    j.check("next link reaches the other profile", len(rest) == 1 and same_items(ids_of(first + rest), [p1, p2]),
            first=ids_of(first), listed=ids_of(rest))
    name = f"VIP customers {tag}"
    lst = data(j.call("POST", "/api/lists", body={"data": {"type": "list", "attributes": {"name": name}}}))
    j.check("list keeps its name",
            j.last["status"] == 201 and eq(lst.get("type"), "list") and eq(obj(lst.get("attributes")).get("name"), name))
    lid = given(lst.get("id"), made_up(j, "", 6))
    members = {"data": [{"type": "profile", "id": p1}, {"type": "profile", "id": p2}]}
    j.call("POST", f"/api/lists/{lid}/relationships/profiles", body=members)
    added = j.last["status"]
    rows = rows_of(j.call("GET", f"/api/lists/{lid}/profiles"))
    j.check("list profiles hold the added profiles", same_items(ids_of(rows), [p1, p2]),
            add_status=added, listed=ids_of(rows))
    rows = rows_of(j.call("GET", f"/api/profiles/{p1}/lists"))
    j.check("profile's lists hold the list", lid in ids_of(rows), listed=ids_of(rows))
    j.call("DELETE", f"/api/lists/{lid}/relationships/profiles", body={"data": [{"type": "profile", "id": p2}]})
    removed = j.last["status"]
    rows = rows_of(j.call("GET", f"/api/lists/{lid}/profiles"))
    j.check("a removed profile leaves the list", ids_of(rows) == [p1], remove_status=removed, listed=ids_of(rows))
    j.call("DELETE", f"/api/lists/{lid}")
    deleted = j.last["status"]
    j.call("GET", f"/api/lists/{lid}")
    j.check("deleted list is not found", j.last["status"] == 404, delete_status=deleted)


def woocommerce_steps(j):
    # Integer ids, and lists that are bare arrays paged by number. The category is new,
    # so its products are the two made here. The store has no tax or shipping, so an
    # order line costs the product's price times its quantity.
    j.headers["authorization"] = "Basic Y2tfYmVuY2hfcGxhY2Vob2xkZXI6Y3NfYmVuY2hfcGxhY2Vob2xkZXI="
    tag = j.rng.randrange(10**5, 10**6)
    wc = "/wp-json/wc/v3"

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**6, 10**7)

    cid = own(obj(j.call("POST", f"{wc}/products/categories", body={"name": f"Desks {tag}"})).get("id"))
    sku = f"DESK-{tag}"
    sent = {"name": f"Oak desk {tag}", "type": "simple", "regular_price": "24.50", "sku": sku,
            "manage_stock": True, "stock_quantity": 12, "categories": [{"id": cid}]}
    a = obj(j.call("POST", f"{wc}/products", body=sent))
    j.check("product keeps its sku and price",
            j.last["status"] == 201 and is_int(a.get("id")) and same(a, sent, "name", "sku", "regular_price"),
            id=a.get("id"), sku=a.get("sku"), regular_price=a.get("regular_price"))
    j.check("product names its category", cid in ids_of(a.get("categories")), categories=ids_of(a.get("categories")))
    pa = own(a.get("id"))
    dup = obj(j.call("POST", f"{wc}/products", body={"name": f"Pine desk {tag}", "regular_price": "30.00", "sku": sku}))
    j.check("a taken sku is refused", j.last["status"] == 400 and eq(dup.get("code"), "product_invalid_sku"),
            code=dup.get("code"))
    pb = own(obj(j.call("POST", f"{wc}/products", body={
        "name": f"Walnut desk {tag}", "regular_price": "89.00", "sku": f"DESK-{tag}-W", "categories": [{"id": cid}]})).get("id"))
    read = obj(j.call("GET", f"{wc}/products/{pa}"))
    j.check("read matches the created product",
            eq(read.get("id"), pa) and same(read, sent, "name", "sku", "regular_price", "stock_quantity"))
    updated = obj(j.call("PUT", f"{wc}/products/{pa}", body={"regular_price": "19.99", "stock_quantity": 8}))
    j.check("update changes the fields sent",
            eq(updated.get("regular_price"), "19.99") and eq(updated.get("stock_quantity"), 8),
            regular_price=updated.get("regular_price"), stock_quantity=updated.get("stock_quantity"))
    read = obj(j.call("GET", f"{wc}/products/{pa}"))
    j.check("update keeps the fields it was not sent",
            same(read, sent, "name", "sku") and cid in ids_of(read.get("categories")))
    first = ids_of(j.call("GET", f"{wc}/products", query={"category": cid, "per_page": 1, "page": 1}))
    j.check("per_page 1 answers one product", len(first) == 1 and first[0] in (pa, pb), listed=first)
    second = ids_of(j.call("GET", f"{wc}/products", query={"category": cid, "per_page": 1, "page": 2}))
    j.check("page 2 answers the other product", len(second) == 1 and same_items(first + second, [pa, pb]),
            first=first, listed=second)
    rows = ids_of(j.call("GET", f"{wc}/products", query={"sku": sku}))
    j.check("sku filter finds only the product", rows == [pa], listed=rows)
    order = obj(j.call("POST", f"{wc}/orders", body={
        "status": "pending", "currency": "EUR", "set_paid": False,
        "billing": {"first_name": "Ana", "last_name": "Silva", "email": f"ana.{tag}@example.org", "country": "PT"},
        "line_items": [{"product_id": pa, "quantity": 2}]}))
    line = head(order.get("line_items"))
    j.check("order line names the product and quantity",
            j.last["status"] == 201 and eq(line.get("product_id"), pa) and eq(line.get("quantity"), 2),
            product_id=line.get("product_id"), quantity=line.get("quantity"))
    j.check("order line costs the price times the quantity",
            eq(line.get("total"), "39.98") and eq(order.get("total"), "39.98"),
            line_total=line.get("total"), total=order.get("total"))
    oid = own(order.get("id"))
    text = f"Packed in two boxes, ref {tag}."
    nid = own(obj(j.call("POST", f"{wc}/orders/{oid}/notes", body={"note": text, "customer_note": False})).get("id"))
    rows = j.call("GET", f"{wc}/orders/{oid}/notes")
    j.check("order notes list the note", eq(by_id(rows, nid).get("note"), text), listed=ids_of(rows))
    j.call("PUT", f"{wc}/orders/{oid}", body={"status": "completed"})
    put = j.last["status"]
    got = obj(j.call("GET", f"{wc}/orders/{oid}"))
    j.check("completed order reads as completed", eq(got.get("status"), "completed"),
            put_status=put, order_status=got.get("status"))
    j.call("DELETE", f"{wc}/products/{pb}", query={"force": "true"})
    deleted = j.last["status"]
    j.call("GET", f"{wc}/products/{pb}")
    j.check("deleted product is not found", j.last["status"] == 404, delete_status=deleted)


def statuspage_steps(j):
    # Everything is under the key's first page, and bodies wrap the resource in its
    # name. Both incidents carry a word unique to this run, so q matches only them.
    # Incident updates are listed newest first.
    j.headers["authorization"] = "OAuth bench-placeholder"
    tag = j.rng.randrange(10**5, 10**6)

    def own(value):
        return value if isinstance(value, str) and value else made_up(j, "", 12, LOWER_ALNUM)

    page = own(head(j.call("GET", "/v1/pages")).get("id"))
    base = f"/v1/pages/{page}"

    def incident(iid):
        return obj(j.call("GET", f"{base}/incidents/{iid}"))

    def component(cid):
        return obj(j.call("GET", f"{base}/components/{cid}"))

    sent = {"name": f"Checkout API {tag}", "description": "Card payments at checkout.", "status": "operational"}
    comp = obj(j.call("POST", f"{base}/components", body={"component": sent}))
    j.check("component keeps its page, name and status",
            j.last["status"] == 201 and eq(comp.get("page_id"), page) and same(comp, sent, "name", "status"),
            page_id=comp.get("page_id"), component_status=comp.get("status"))
    cid = own(comp.get("id"))
    word = f"kestrel{tag}"
    name, body1 = f"Payments failing {word}", f"We are investigating failed card payments ({word})."
    made = obj(j.call("POST", f"{base}/incidents", body={"incident": {
        "name": name, "status": "investigating", "impact_override": "major", "body": body1,
        "component_ids": [cid], "components": {cid: "major_outage"}, "deliver_notifications": False}}))
    j.check("incident keeps its name and status",
            j.last["status"] == 201 and eq(made.get("name"), name) and eq(made.get("status"), "investigating"),
            incident_status=made.get("status"))
    updates = dicts(made.get("incident_updates"))
    j.check("first update carries the body",
            len(updates) == 1 and eq(updates[0].get("status"), "investigating") and eq(updates[0].get("body"), body1),
            updates=[(u.get("status"), u.get("body")) for u in updates])
    iid = own(made.get("id"))
    now = component(cid).get("status")
    j.check("affected component shows the outage", eq(now, "major_outage"), component_status=now)
    body2 = f"The cause is an expired certificate ({word})."
    j.call("PATCH", f"{base}/incidents/{iid}", body={"incident": {"status": "identified", "body": body2}})
    patched = j.last["status"]
    got = incident(iid)
    updates = dicts(got.get("incident_updates"))
    j.check("status change adds an incident update",
            eq(got.get("status"), "identified") and len(updates) == 2
            and eq(updates[0].get("status"), "identified") and eq(updates[0].get("body"), body2),
            patch_status=patched, incident_status=got.get("status"),
            updates=[(u.get("status"), u.get("body")) for u in updates])
    oldest = own(updates[-1].get("id") if updates else None)
    fixed = f"We are investigating failed card payments in the EU ({word})."
    j.call("PATCH", f"{base}/incidents/{iid}/incident_updates/{oldest}", body={"incident_update": {"body": fixed}})
    edited = j.last["status"]
    update = by_id(incident(iid).get("incident_updates"), oldest)
    j.check("edited update keeps its new body",
            eq(update.get("body"), fixed) and eq(update.get("status"), "investigating"),
            edit_status=edited, body=update.get("body"))
    email = f"ops.{tag}@example.org"
    sid = own(obj(j.call("POST", f"{base}/incidents/{iid}/subscribers",
                         body={"subscriber": {"email": email, "skip_confirmation_notification": True}})).get("id"))
    rows = j.call("GET", f"{base}/incidents/{iid}/subscribers")
    j.check("incident subscribers list the subscriber", eq(by_id(rows, sid).get("email"), email), listed=ids_of(rows))
    rows = j.call("GET", f"{base}/incidents/unresolved")
    j.check("unresolved list holds the incident", bool(by_id(rows, iid)), listed=ids_of(rows))
    i2 = own(obj(j.call("POST", f"{base}/incidents", body={"incident": {
        "name": f"Slow search {word}", "status": "investigating", "body": f"Search is slow ({word}).",
        "deliver_notifications": False}})).get("id"))
    first = ids_of(j.call("GET", f"{base}/incidents", query={"q": word, "per_page": 1, "page": 1}))
    j.check("per_page 1 answers one incident", len(first) == 1 and first[0] in (iid, i2), listed=first)
    second = ids_of(j.call("GET", f"{base}/incidents", query={"q": word, "per_page": 1, "page": 2}))
    j.check("page 2 reaches the other incident", len(second) == 1 and same_items(first + second, [iid, i2]),
            first=first, listed=second)
    j.call("PATCH", f"{base}/incidents/{i2}", body={"incident": {"status": "fixed"}})
    j.check("an unknown status is refused", j.last["status"] == 422)
    j.call("PATCH", f"{base}/incidents/{iid}", body={"incident": {
        "status": "resolved", "body": f"Card payments work again ({word}).", "components": {cid: "operational"}}})
    resolved = j.last["status"]
    got = incident(iid)
    rows = j.call("GET", f"{base}/incidents/unresolved")
    j.check("resolved incident leaves the unresolved list",
            eq(got.get("status"), "resolved") and instant(got.get("resolved_at")) is not None and not by_id(rows, iid),
            resolve_status=resolved, incident_status=got.get("status"), resolved_at=got.get("resolved_at"),
            listed=ids_of(rows))
    now = component(cid).get("status")
    j.check("resolving restores the component", eq(now, "operational"), component_status=now)
    j.call("DELETE", f"{base}/incidents/{i2}")
    deleted = j.last["status"]
    j.call("GET", f"{base}/incidents/{i2}")
    j.check("deleted incident is not found", j.last["status"] == 404, delete_status=deleted)


def zohobooks_steps(j):
    # Every call names the organization. Status changes only through /status/... and a
    # payment, and each change is read back. No line has a tax_id, so the totals are the
    # lines' sums: 3 x 120 + 2 x 45.50 = 451.
    j.headers["authorization"] = "Zoho-oauthtoken bench-placeholder"
    tag = j.rng.randrange(10**5, 10**6)
    org = {"organization_id": "10234695"}

    def call(method, path, query=None, **kw):
        return obj(j.call(method, f"/books/v3{path}", query=dict(org, **(query or {})), **kw))

    def own(value):
        return value if isinstance(value, str) and value.isdigit() else str(j.rng.randrange(10**14, 10**15))

    def invoice(iid):
        return obj(call("GET", f"/invoices/{iid}").get("invoice"))

    def listed(**query):
        page = call("GET", "/invoices", query=dict({"customer_id": cust}, **query))
        return ids_of(page.get("invoices"), "invoice_id"), obj(page.get("page_context")).get("has_more_page")

    contact = obj(call("POST", "/contacts", body={
        "contact_name": f"Silva Lda {tag}", "contact_type": "customer",
        "contact_persons": [{"first_name": "Ana", "last_name": "Silva", "email": f"ana.{tag}@example.org",
                             "is_primary_contact": True}]}).get("contact"))
    cust = own(contact.get("contact_id"))
    number = f"BENCH-{tag}"
    lines = [{"name": "Consulting", "rate": 120, "quantity": 3}, {"name": "Travel", "rate": 45.5, "quantity": 2}]
    numbered = {"ignore_auto_number_generation": "true"}
    made = obj(call("POST", "/invoices", query=numbered, body={
        "customer_id": cust, "invoice_number": number, "date": "2026-11-02", "due_date": "2026-12-02",
        "line_items": lines}).get("invoice"))
    j.check("a new invoice is a draft",
            j.last["status"] == 201 and eq(made.get("status"), "draft") and eq(made.get("customer_id"), cust),
            invoice_status=made.get("status"), customer_id=made.get("customer_id"))
    j.check("invoice total adds its lines",
            near(made.get("sub_total"), 451, 0.005) and near(made.get("total"), 451, 0.005)
            and near(made.get("balance"), 451, 0.005),
            sub_total=made.get("sub_total"), total=made.get("total"), balance=made.get("balance"))
    i1 = own(made.get("invoice_id"))
    dup = call("POST", "/invoices", query=numbered,
               body={"customer_id": cust, "invoice_number": number, "line_items": lines[:1]})
    j.check("a taken invoice number is refused", j.last["status"] == 400 and eq(dup.get("code"), 1001),
            code=dup.get("code"))
    read = invoice(i1)
    j.check("read matches the created invoice",
            eq(read.get("invoice_id"), i1) and eq(read.get("invoice_number"), number)
            and near(read.get("total"), 451, 0.005) and len(dicts(read.get("line_items"))) == 2)
    ref = f"PO-{tag}"
    call("PUT", f"/invoices/{i1}", body={"reference_number": ref})
    put = j.last["status"]
    read = invoice(i1)
    j.check("update changes the reference and keeps the lines",
            eq(read.get("reference_number"), ref) and eq(read.get("invoice_number"), number)
            and near(read.get("total"), 451, 0.005) and len(dicts(read.get("line_items"))) == 2,
            put_status=put, reference_number=read.get("reference_number"), total=read.get("total"))
    call("POST", f"/invoices/{i1}/status/sent")
    acted = j.last["status"]
    now = invoice(i1).get("status")
    j.check("marked-sent invoice reads as sent", eq(now, "sent"), action_status=acted, invoice_status=now)
    payment = obj(call("POST", "/customerpayments", body={
        "customer_id": cust, "payment_mode": "banktransfer", "amount": 200, "date": "2026-11-03",
        "invoices": [{"invoice_id": i1, "amount_applied": 200}]}).get("payment"))
    applied = head(payment.get("invoices"))
    j.check("payment applies to the invoice",
            j.last["status"] == 201 and eq(applied.get("invoice_id"), i1) and near(applied.get("amount_applied"), 200, 0.005),
            applied=applied)
    read = invoice(i1)
    j.check("a part payment leaves the rest owed",
            eq(read.get("status"), "partially_paid") and near(read.get("balance"), 251, 0.005)
            and near(read.get("payment_made"), 200, 0.005),
            invoice_status=read.get("status"), balance=read.get("balance"), payment_made=read.get("payment_made"))
    rows, _ = listed(status="partially_paid")
    j.check("status filter finds the partly paid invoice", rows == [i1], listed=rows)
    i2 = own(obj(call("POST", "/invoices", body={
        "customer_id": cust, "date": "2026-11-03",
        "line_items": [{"name": "Workshop", "rate": 300, "quantity": 1}]}).get("invoice")).get("invoice_id"))
    first, more = listed(per_page=1, page=1)
    j.check("per_page 1 answers one invoice and more pages", len(first) == 1 and more is True,
            listed=first, has_more_page=more)
    second, more = listed(per_page=1, page=2)
    j.check("page 2 reaches the other invoice",
            len(second) == 1 and same_items(first + second, [i1, i2]) and more is False,
            first=first, listed=second, has_more_page=more)
    call("POST", f"/invoices/{i2}/status/sent")
    call("POST", f"/invoices/{i2}/status/void", body={"reason": f"Issued twice, ref {tag}."})
    acted = j.last["status"]
    now = invoice(i2).get("status")
    j.check("voided invoice reads as void", eq(now, "void"), action_status=acted, invoice_status=now)
    call("POST", f"/invoices/{i2}/status/draft")
    acted = j.last["status"]
    now = invoice(i2).get("status")
    j.check("a void invoice returns to draft", eq(now, "draft"), action_status=acted, invoice_status=now)
    call("DELETE", f"/invoices/{i2}")
    deleted = j.last["status"]
    call("GET", f"/invoices/{i2}")
    j.check("deleted invoice is not found", j.last["status"] == 404, delete_status=deleted)


BLIND6_JOURNEYS = {"dropbox": dropbox_steps, "klaviyo": klaviyo_steps, "woocommerce": woocommerce_steps,
                   "statuspage": statuspage_steps, "zohobooks": zohobooks_steps}


def run_journeys_blind6(srv, sink, args):
    run_journey_table(BLIND6_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- seventh blind journeys

# The same rule again, over five more contracts written for this suite from each real
# API's public documentation (scripts/llm-bench/contracts/blind7/). They were picked for
# API styles: a GraphQL schema with Relay connections, filters and archiving (Linear),
# HAL documents with _links, amounts as currency and decimal strings and from/limit
# paging (Mollie), form-encoded bodies with bracketed keys, money in minor units and
# epoch-second timestamps, inside resource-named wrappers (Chargebee), resources three
# levels deep with a next URL in the body and soft-deleted comments (Bitbucket), and a
# success/errors/result envelope with result_info, numbered error codes and a batch
# endpoint (Cloudflare). No check gates the rest: a journey goes on with an id of its
# own when a call gave none. A check asserts only what its contract promises a client.
# Do not edit a check to suit results.
BLIND7_CHECKS = {
    "linear": [
        "team keeps its name and key", "issue keeps its title and team", "identifier carries the team key",
        "read matches the created issue", "update changes the priority and keeps the title",
        "first 1 answers one issue and a cursor", "after the cursor reaches the other issue",
        "priority filter finds only the urgent issue", "comment keeps its body and issue",
        "issue comments hold the comment", "archived issue reads with archivedAt",
        "archived issue leaves the team's issues", "includeArchived lists the archived issue",
        "deleted comment is not found",
    ],
    "mollie": [
        "customer keeps its name and email", "customer links to itself", "update changes the locale and keeps the email",
        "payment keeps its amount and customer", "a new payment is open", "payment links to its customer",
        "an amount without two decimals is refused", "read matches the created payment",
        "limit 1 answers one payment and a next link", "next link reaches the other payment",
        "payment update changes the description and keeps the amount", "cancel answers the canceled payment",
        "canceled payment reads as canceled", "deleted customer is not found",
    ],
    "chargebee": [
        "customer keeps its id, names and email", "customer keeps its billing address", "created_at counts seconds",
        "a taken id is refused", "read matches the created customer", "update changes the company and keeps the email",
        "added credits raise the balance in minor units", "a deduction closes at the rest",
        "customer reads the remaining credits", "limit 1 answers one customer and a next_offset",
        "next_offset reaches the other customer", "email filter finds only the customer",
        "customer comments list the note", "deleted customer is not found",
    ],
    "bitbucket": [
        "repository keeps its full name", "a taken slug is refused", "pull request opens on its branches",
        "read matches the created pull request", "update changes the title and keeps the branches",
        "comment keeps its text and pull request", "pagelen 1 answers one comment and a next link",
        "next reaches the other comment", "pull request counts its comments", "a deleted comment reads as deleted",
        "comment_count leaves out the deleted comment", "the next pull request takes the next id",
        "declined pull request reads as declined", "open list leaves out the declined one",
    ],
    "cloudflare": [
        "zone keeps its name", "record answers the full name", "an identical record is refused",
        "read matches the created record", "patch changes the content sent", "patch keeps the fields it was not sent",
        "type filter lists only A records", "result_info counts the matching records", "name filter finds only the record",
        "put replaces the record", "batch answers each change", "a batch patch reads back",
        "a batch delete removes the record", "deleted record leaves the zone's records",
    ],
}
JOURNEY_CHECKS.update(BLIND7_CHECKS)
BLIND7_DIR = "scripts/llm-bench/contracts/blind7"
BLIND7_CONTRACTS = {"linear": "linear.graphql"}
# Contracts alone: no persona.
BLIND7_MOCKS = {name: (f"{BLIND7_DIR}/{BLIND7_CONTRACTS.get(name, f'{name}.openapi.yaml')}", f"/{name}", None)
                for name in BLIND7_CHECKS}
UPPER = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"


def query_value(url, name):
    """The first value of query parameter `name` in `url`, or None."""
    if not isinstance(url, str):
        return None
    values = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query).get(name)
    return values[0] if values else None


def linear_steps(j):
    # One POST /graphql per call, each with its selection set. The team is new, so its
    # issues are the two made here; the second one is archived, which takes it out of
    # the team's list until a list asks for archived issues.
    j.headers["authorization"] = "lin_api_bench-placeholder"
    tag = j.rng.randrange(10**5, 10**6)
    fields = "id identifier number title description priority archivedAt team { id key }"

    def gql(query, **variables):
        answer = obj(j.call("POST", "/graphql", body={"query": query, "variables": variables}))
        return obj(answer.get("data")), answer.get("errors")

    def own(value):
        return value if isinstance(value, str) and value else str(uuid.UUID(int=j.rng.getrandbits(128), version=4))

    def issue(iid, selection=fields):
        return obj(gql(f"query($id: String!) {{ issue(id: $id) {{ {selection} }} }}", id=iid)[0].get("issue"))

    def create(sent):
        data, _ = gql(f"mutation($input: IssueCreateInput!) {{ issueCreate(input: $input) {{ success issue {{ {fields} }} }} }}",
                      input=sent)
        return obj(obj(data.get("issueCreate")).get("issue"))

    def listed(filt, **extra):
        args = "".join(f", ${k}: {t}" for k, t in (("first", "Int"), ("after", "String"), ("archived", "Boolean"))
                       if k in extra)
        use = "".join(f", {'includeArchived' if k == 'archived' else k}: ${k}"
                      for k in ("first", "after", "archived") if k in extra)
        data, _ = gql(f"query($filter: IssueFilter{args}) {{ issues(filter: $filter{use}) "
                      "{ nodes { id } pageInfo { hasNextPage endCursor } } }", filter=filt, **extra)
        conn = obj(data.get("issues"))
        return ids_of(conn.get("nodes")), obj(conn.get("pageInfo"))

    key, name = made_up(j, "Q", 3, UPPER), f"Billing {tag}"
    data, _ = gql("mutation($input: TeamCreateInput!) { teamCreate(input: $input) { success team { id key name } } }",
                  input={"name": name, "key": key})
    made = obj(data.get("teamCreate"))
    team = obj(made.get("team"))
    j.check("team keeps its name and key",
            made.get("success") is True and eq(team.get("name"), name) and eq(team.get("key"), key),
            success=made.get("success"), team=team)
    tid = own(team.get("id"))
    sent = {"teamId": tid, "title": f"Invoice totals round wrong {tag}",
            "description": "Totals are off by one cent on multi-line invoices.", "priority": 2}
    one = create(sent)
    j.check("issue keeps its title and team",
            eq(one.get("title"), sent["title"]) and eq(one.get("priority"), 2) and eq(obj(one.get("team")).get("id"), tid),
            title=one.get("title"), priority=one.get("priority"), team=one.get("team"))
    ident, number = one.get("identifier"), one.get("number")
    j.check("identifier carries the team key",
            matches(rf"{key}-[0-9]+", ident) and is_number(number) and ident == f"{key}-{int(number)}",
            identifier=ident, number=number)
    i1 = own(one.get("id"))
    read = issue(i1)
    j.check("read matches the created issue",
            eq(read.get("id"), i1) and same(read, sent, "title", "description", "priority"),
            title=read.get("title"), priority=read.get("priority"))
    data, _ = gql("mutation($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success } }",
                  id=i1, input={"priority": 1})
    updated = obj(data.get("issueUpdate")).get("success")
    read = issue(i1)
    j.check("update changes the priority and keeps the title",
            eq(read.get("priority"), 1) and same(read, sent, "title", "description"),
            update_success=updated, priority=read.get("priority"), title=read.get("title"))
    i2 = own(create({"teamId": tid, "title": f"Export drops the header row {tag}", "priority": 3}).get("id"))
    team_filter = {"team": {"id": {"eq": tid}}}
    first, info = listed(team_filter, first=1)
    cursor = info.get("endCursor")
    j.check("first 1 answers one issue and a cursor",
            len(first) == 1 and info.get("hasNextPage") is True and isinstance(cursor, str) and bool(cursor),
            listed=first, page_info=info)
    rest, info = listed(team_filter, first=1, after=given(cursor, made_up(j, "", 24)))
    j.check("after the cursor reaches the other issue",
            len(rest) == 1 and same_items(first + rest, [i1, i2]) and info.get("hasNextPage") is False,
            first=first, listed=rest, page_info=info)
    rows, _ = listed(dict(team_filter, priority={"eq": 1}))
    j.check("priority filter finds only the urgent issue", rows == [i1], listed=rows)
    body = f"Reproduced on invoice INV-{tag}: 3 lines at 0.335 each."
    data, _ = gql("mutation($input: CommentCreateInput!) { commentCreate(input: $input) "
                  "{ success comment { id body issue { id } } } }", input={"issueId": i1, "body": body})
    comment = obj(obj(data.get("commentCreate")).get("comment"))
    j.check("comment keeps its body and issue",
            eq(comment.get("body"), body) and eq(obj(comment.get("issue")).get("id"), i1),
            body=comment.get("body"), issue=comment.get("issue"))
    cid = own(comment.get("id"))
    nodes = obj(issue(i1, "id comments { nodes { id body } }").get("comments")).get("nodes")
    j.check("issue comments hold the comment", eq(by_id(nodes, cid).get("body"), body), listed=ids_of(nodes))
    data, _ = gql("mutation($id: String!) { issueArchive(id: $id) { success } }", id=i2)
    archived = obj(data.get("issueArchive")).get("success")
    read = issue(i2, "id archivedAt")
    j.check("archived issue reads with archivedAt", eq(read.get("id"), i2) and instant(read.get("archivedAt")) is not None,
            archive_success=archived, archivedAt=read.get("archivedAt"))
    rows, _ = listed(team_filter)
    j.check("archived issue leaves the team's issues", rows == [i1], listed=rows)
    rows, _ = listed(team_filter, archived=True)
    j.check("includeArchived lists the archived issue", same_items(rows, [i1, i2]), listed=rows)
    data, _ = gql("mutation($id: String!) { commentDelete(id: $id) { success entityId } }", id=cid)
    deleted = obj(data.get("commentDelete"))
    data, errors = gql("query($id: String!) { comment(id: $id) { id body } }", id=cid)
    j.check("deleted comment is not found", bool(dicts(errors)) and not data.get("comment"),
            delete_success=deleted.get("success"), errors=errors if isinstance(errors, list) else None)


def mollie_steps(j):
    # HAL throughout: links are objects with href. Payments are made for a new customer,
    # so its list holds the two made here. A bank transfer is cancelable while open.
    j.headers["authorization"] = "Bearer test_bench-placeholder"
    tag = j.rng.randrange(10**5, 10**6)

    def own(value, prefix):
        return value if isinstance(value, str) and value.startswith(prefix) else made_up(j, prefix, 10)

    def link(thing, rel):
        return obj(obj(obj(thing).get("_links")).get(rel)).get("href")

    def payments(**query):
        page = obj(j.call("GET", f"/v2/customers/{cid}/payments", query=query))
        return page, dicts(obj(page.get("_embedded")).get("payments"))

    sent = {"name": f"Ana Silva {tag}", "email": f"ana.{tag}@example.org", "locale": "pt_PT",
            "metadata": {"ref": f"bench-{tag}"}}
    made = obj(j.call("POST", "/v2/customers", body=sent))
    j.check("customer keeps its name and email",
            j.last["status"] == 201 and eq(made.get("resource"), "customer") and same(made, sent, "name", "email"),
            resource=made.get("resource"), customer_name=made.get("name"), email=made.get("email"))
    cid = own(made.get("id"), "cst_")
    self_link = link(made, "self")
    j.check("customer links to itself", isinstance(self_link, str) and cid in self_link, self_link=self_link)
    j.call("PATCH", f"/v2/customers/{cid}", body={"locale": "nl_NL"})
    patched = j.last["status"]
    read = obj(j.call("GET", f"/v2/customers/{cid}"))
    j.check("update changes the locale and keeps the email",
            eq(read.get("locale"), "nl_NL") and same(read, sent, "name", "email"),
            patch_status=patched, locale=read.get("locale"), email=read.get("email"))
    amount = {"currency": "EUR", "value": "24.50"}
    order = {"amount": amount, "description": f"Order {tag}", "redirectUrl": f"https://shop.example.org/orders/{tag}",
             "method": "banktransfer", "metadata": {"order_id": f"{tag}"}}
    pay = obj(j.call("POST", f"/v2/customers/{cid}/payments", body=order))
    j.check("payment keeps its amount and customer",
            j.last["status"] == 201 and eq(pay.get("amount"), amount) and eq(pay.get("customerId"), cid),
            amount=pay.get("amount"), customerId=pay.get("customerId"))
    j.check("a new payment is open", eq(pay.get("status"), "open"), payment_status=pay.get("status"))
    customer_link = link(pay, "customer")
    j.check("payment links to its customer", isinstance(customer_link, str) and cid in customer_link,
            customer_link=customer_link)
    p1 = own(pay.get("id"), "tr_")
    bad = obj(j.call("POST", f"/v2/customers/{cid}/payments",
                     body=dict(order, amount={"currency": "EUR", "value": "24.5"})))
    j.check("an amount without two decimals is refused", j.last["status"] == 422 and eq(bad.get("field"), "amount.value"),
            field=bad.get("field"))
    read = obj(j.call("GET", f"/v2/payments/{p1}"))
    j.check("read matches the created payment",
            eq(read.get("id"), p1) and same(read, order, "description", "amount")
            and eq(obj(read.get("metadata")).get("order_id"), f"{tag}"),
            description=read.get("description"), amount=read.get("amount"), metadata=read.get("metadata"))
    p2 = own(obj(j.call("POST", f"/v2/customers/{cid}/payments", body={
        "amount": {"currency": "EUR", "value": "9.99"}, "description": f"Order {tag}-2", "method": "banktransfer",
        "redirectUrl": f"https://shop.example.org/orders/{tag}-2"})).get("id"), "tr_")
    page, first = payments(limit=1)
    after = query_value(obj(obj(page.get("_links")).get("next")).get("href"), "from")
    j.check("limit 1 answers one payment and a next link", len(first) == 1 and eq(page.get("count"), 1) and bool(after),
            listed=ids_of(first), count=page.get("count"), next=obj(page.get("_links")).get("next"))
    _, rest = payments(limit=1, **{"from": given(after, p1)})
    j.check("next link reaches the other payment", len(rest) == 1 and same_items(ids_of(first + rest), [p1, p2]),
            first=ids_of(first), listed=ids_of(rest))
    text = f"Order {tag} (gift wrapped)"
    j.call("PATCH", f"/v2/payments/{p1}", body={"description": text})
    patched = j.last["status"]
    read = obj(j.call("GET", f"/v2/payments/{p1}"))
    j.check("payment update changes the description and keeps the amount",
            eq(read.get("description"), text) and eq(read.get("amount"), amount),
            patch_status=patched, description=read.get("description"), amount=read.get("amount"))
    canceled = obj(j.call("DELETE", f"/v2/payments/{p2}"))
    j.check("cancel answers the canceled payment", j.last["status"] == 200 and eq(canceled.get("status"), "canceled"),
            payment_status=canceled.get("status"))
    read = obj(j.call("GET", f"/v2/payments/{p2}"))
    j.check("canceled payment reads as canceled",
            eq(read.get("status"), "canceled") and instant(read.get("canceledAt")) is not None,
            payment_status=read.get("status"), canceledAt=read.get("canceledAt"))
    j.call("DELETE", f"/v2/customers/{cid}")
    deleted = j.last["status"]
    j.call("GET", f"/v2/customers/{cid}")
    j.check("deleted customer is not found", j.last["status"] == 404, delete_status=deleted)


def chargebee_steps(j):
    # Writes are form posts with bracketed keys; answers wrap each resource in its name.
    # Credits are in cents: 1250 added, 450 deducted, 800 left. The company is unique to
    # this run, so its filter matches only the two customers made here.
    j.headers["authorization"] = "Basic " + base64.b64encode(b"test_bench-placeholder:").decode()
    tag = j.rng.randrange(10**5, 10**6)
    api = "/api/v2"

    def customer(value):
        return obj(obj(value).get("customer"))

    def listed(**query):
        page = obj(j.call("GET", f"{api}/customers", query=query))
        return [customer(r).get("id") for r in dicts(page.get("list"))], page.get("next_offset")

    def credit(action, amount):
        answer = obj(j.call("POST", f"{api}/promotional_credits/{action}", form={
            "customer_id": c1, "amount": amount, "currency_code": "EUR", "description": f"{action} {tag}"}))
        return obj(answer.get("promotional_credit")), customer(answer)

    c1, email = f"bench_{tag}", f"ana.{tag}@example.org"
    form = {"id": c1, "first_name": "Ana", "last_name": "Silva", "email": email, "company": f"Silva {tag}",
            "preferred_currency_code": "EUR", "billing_address[city]": "Lisbon", "billing_address[country]": "PT"}
    made = customer(j.call("POST", f"{api}/customers", form=form))
    j.check("customer keeps its id, names and email",
            j.last["status"] == 200 and same(made, form, "id", "first_name", "last_name", "email"),
            id=made.get("id"), first_name=made.get("first_name"), email=made.get("email"))
    address = obj(made.get("billing_address"))
    j.check("customer keeps its billing address", eq(address.get("city"), "Lisbon") and eq(address.get("country"), "PT"),
            billing_address=address)
    created = made.get("created_at")
    j.check("created_at counts seconds", is_int(created) and 10**9 <= created < 10**10, created_at=created)
    dup = obj(j.call("POST", f"{api}/customers", form={"id": c1, "first_name": "Rui", "email": f"rui.{tag}@example.org"}))
    j.check("a taken id is refused", j.last["status"] == 400 and eq(dup.get("api_error_code"), "duplicate_entry"),
            api_error_code=dup.get("api_error_code"))
    read = customer(j.call("GET", f"{api}/customers/{c1}"))
    j.check("read matches the created customer", same(read, form, "id", "first_name", "last_name", "email", "company"))
    company = f"Atlas {tag} Lda"
    j.call("POST", f"{api}/customers/{c1}", form={"company": company})
    updated = j.last["status"]
    read = customer(j.call("GET", f"{api}/customers/{c1}"))
    j.check("update changes the company and keeps the email",
            eq(read.get("company"), company) and same(read, form, "email", "first_name"),
            update_status=updated, company=read.get("company"), email=read.get("email"))
    record, after = credit("add", 1250)
    j.check("added credits raise the balance in minor units",
            eq(record.get("amount"), 1250) and eq(record.get("type"), "increment")
            and eq(after.get("promotional_credits"), 1250),
            amount=record.get("amount"), type=record.get("type"), balance=after.get("promotional_credits"))
    record, _ = credit("deduct", 450)
    j.check("a deduction closes at the rest",
            eq(record.get("type"), "decrement") and eq(record.get("closing_balance"), 800),
            type=record.get("type"), closing_balance=record.get("closing_balance"))
    now = customer(j.call("GET", f"{api}/customers/{c1}")).get("promotional_credits")
    j.check("customer reads the remaining credits", eq(now, 800), promotional_credits=now)
    c2 = f"bench_{tag}_b"
    j.call("POST", f"{api}/customers", form={"id": c2, "first_name": "Rui", "last_name": "Mendes",
                                             "email": f"rui.{tag}@example.org", "company": company})
    first, offset = listed(**{"company[is]": company, "limit": 1})
    j.check("limit 1 answers one customer and a next_offset",
            len(first) == 1 and first[0] in (c1, c2) and isinstance(offset, str) and bool(offset),
            listed=first, next_offset=offset)
    rest, more = listed(**{"company[is]": company, "limit": 1, "offset": given(offset, made_up(j, "", 16))})
    j.check("next_offset reaches the other customer",
            len(rest) == 1 and same_items(first + rest, [c1, c2]) and more is None,
            first=first, listed=rest, next_offset=more)
    rows, _ = listed(**{"email[is]": email})
    j.check("email filter finds only the customer", rows == [c1], listed=rows)
    notes = f"Asked for invoices in Portuguese, ref {tag}."
    note = obj(obj(j.call("POST", f"{api}/comments", form={
        "entity_type": "customer", "entity_id": c1, "notes": notes})).get("comment"))
    nid = given(note.get("id"), made_up(j, "cmt_", 16))
    rows = [obj(r.get("comment")) for r in dicts(obj(j.call("GET", f"{api}/comments", query={
        "entity_type": "customer", "entity_id": c1})).get("list"))]
    j.check("customer comments list the note", eq(by_id(rows, nid).get("notes"), notes), listed=ids_of(rows))
    j.call("POST", f"{api}/customers/{c2}/delete", form={})
    deleted = j.last["status"]
    gone = obj(j.call("GET", f"{api}/customers/{c2}"))
    j.check("deleted customer is not found",
            j.last["status"] == 404 and eq(gone.get("api_error_code"), "resource_not_found"),
            delete_status=deleted, api_error_code=gone.get("api_error_code"))


def bitbucket_steps(j):
    # Everything lives under a repository made for this run, so its pull requests and
    # comments are the ones made here, and its first pull request is number 1.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    repo = f"/2.0/repositories/bench-team/ledger-{tag}"

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**3, 10**4)

    def pull(n):
        return obj(j.call("GET", f"{repo}/pullrequests/{n}"))

    def branches(pr):
        return (obj(obj(pr.get("source")).get("branch")).get("name"),
                obj(obj(pr.get("destination")).get("branch")).get("name"))

    made = obj(j.call("POST", repo, body={"scm": "git", "is_private": True, "description": "Bench ledger."}))
    j.check("repository keeps its full name",
            j.last["status"] == 200 and eq(made.get("full_name"), f"bench-team/ledger-{tag}")
            and made.get("is_private") is True,
            full_name=made.get("full_name"), is_private=made.get("is_private"))
    again = obj(j.call("POST", repo, body={"scm": "git", "is_private": True}))
    j.check("a taken slug is refused",
            j.last["status"] == 400 and isinstance(obj(again.get("error")).get("message"), str), error=again.get("error"))
    source = f"feature/rounding-{tag}"
    sent = {"title": f"Round invoice totals once {tag}", "description": "Rounds the total, not each line.",
            "source": {"branch": {"name": source}}, "destination": {"branch": {"name": "main"}}}
    pr = obj(j.call("POST", f"{repo}/pullrequests", body=sent))
    j.check("pull request opens on its branches",
            j.last["status"] == 201 and eq(pr.get("state"), "OPEN") and branches(pr) == (source, "main"),
            pr_state=pr.get("state"), branches=branches(pr))
    n1 = own(pr.get("id"))
    read = pull(n1)
    j.check("read matches the created pull request",
            eq(read.get("id"), n1) and same(read, sent, "title", "description") and branches(read) == (source, "main"))
    title = f"Round invoice totals once, not per line {tag}"
    j.call("PUT", f"{repo}/pullrequests/{n1}", body={"title": title})
    put = j.last["status"]
    read = pull(n1)
    j.check("update changes the title and keeps the branches",
            eq(read.get("title"), title) and eq(read.get("description"), sent["description"])
            and branches(read) == (source, "main"),
            put_status=put, title=read.get("title"), branches=branches(read))
    comments = f"{repo}/pullrequests/{n1}/comments"
    text = f"Totals match the ledger now ({tag})."
    one = obj(j.call("POST", comments, body={"content": {"raw": text}}))
    j.check("comment keeps its text and pull request",
            j.last["status"] == 201 and eq(obj(one.get("content")).get("raw"), text)
            and eq(obj(one.get("pullrequest")).get("id"), n1),
            content=one.get("content"), pullrequest=one.get("pullrequest"))
    c1 = own(one.get("id"))
    c2 = own(obj(j.call("POST", comments, body={"content": {"raw": f"Can we add a test for 0.335? ({tag})"}})).get("id"))
    page = obj(j.call("GET", comments, query={"pagelen": 1}))
    first, nxt = dicts(page.get("values")), page.get("next")
    j.check("pagelen 1 answers one comment and a next link", len(first) == 1 and isinstance(nxt, str) and bool(nxt),
            listed=ids_of(first), next=nxt)
    page = obj(j.call("GET", comments, query={"pagelen": 1, "page": query_value(nxt, "page") or 2}))
    rest = dicts(page.get("values"))
    j.check("next reaches the other comment",
            len(rest) == 1 and same_items(ids_of(first + rest), [c1, c2]) and not page.get("next"),
            first=ids_of(first), listed=ids_of(rest), next=page.get("next"))
    count = pull(n1).get("comment_count")
    j.check("pull request counts its comments", eq(count, 2), comment_count=count)
    j.call("DELETE", f"{comments}/{c2}")
    deleted = j.last["status"]
    gone = obj(j.call("GET", f"{comments}/{c2}"))
    j.check("a deleted comment reads as deleted", deleted == 204 and gone.get("deleted") is True,
            delete_status=deleted, deleted=gone.get("deleted"))
    count = pull(n1).get("comment_count")
    j.check("comment_count leaves out the deleted comment", eq(count, 1), comment_count=count)
    two = obj(j.call("POST", f"{repo}/pullrequests", body={
        "title": f"Export the header row {tag}", "source": {"branch": {"name": f"fix/export-{tag}"}},
        "destination": {"branch": {"name": "main"}}}))
    j.check("the next pull request takes the next id", is_int(pr.get("id")) and eq(two.get("id"), pr.get("id") + 1),
            first_id=pr.get("id"), next_id=two.get("id"))
    n2 = own(two.get("id"))
    j.call("POST", f"{repo}/pullrequests/{n2}/decline")
    declined = j.last["status"]
    now = pull(n2).get("state")
    j.check("declined pull request reads as declined", eq(now, "DECLINED"), decline_status=declined, pr_state=now)
    rows = ids_of(obj(j.call("GET", f"{repo}/pullrequests", query={"state": "OPEN"})).get("values"))
    j.check("open list leaves out the declined one", rows == [n1], listed=rows)


def cloudflare_steps(j):
    # Every answer is an envelope around `result`. The zone is new, so its records are
    # the ones made here; short names are answered in full. The batch deletes the TXT
    # record, patches the api record and adds a CNAME.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    zone_name = f"ledger-{tag}.com"

    def result(value):
        return obj(obj(value).get("result"))

    def rows_of(value):
        return dicts(obj(value).get("result"))

    def own(value):
        return value if matches("[0-9a-f]{32}", value) else hex_id(j, "")

    def code(value):
        return head(obj(value).get("errors")).get("code")

    zone = obj(j.call("POST", "/client/v4/zones", body={
        "name": zone_name, "account": {"id": hex_id(j, "")}, "type": "full", "jump_start": False}))
    z = result(zone)
    j.check("zone keeps its name",
            zone.get("success") is True and eq(z.get("name"), zone_name) and matches("[0-9a-f]{32}", z.get("id")),
            success=zone.get("success"), zone_name=z.get("name"), id=z.get("id"))
    records = f"/client/v4/zones/{own(z.get('id'))}/dns_records"
    www_sent = {"type": "A", "name": "www", "content": "203.0.113.10", "ttl": 3600, "proxied": False,
                "comment": f"web {tag}"}
    www = result(j.call("POST", records, body=www_sent))
    full = f"www.{zone_name}"
    j.check("record answers the full name",
            eq(www.get("name"), full) and same(www, www_sent, "type", "content"),
            record_name=www.get("name"), content=www.get("content"))
    wid = own(www.get("id"))
    dup = obj(j.call("POST", records, body=www_sent))
    j.check("an identical record is refused",
            j.last["status"] == 400 and dup.get("success") is False and eq(code(dup), 81058),
            success=dup.get("success"), code=code(dup))
    read = result(j.call("GET", f"{records}/{wid}"))
    j.check("read matches the created record",
            eq(read.get("id"), wid) and eq(read.get("name"), full) and same(read, www_sent, "type", "content", "ttl", "comment"))
    patched = result(j.call("PATCH", f"{records}/{wid}", body={"content": "203.0.113.20"}))
    j.check("patch changes the content sent", eq(patched.get("content"), "203.0.113.20"), content=patched.get("content"))
    read = result(j.call("GET", f"{records}/{wid}"))
    j.check("patch keeps the fields it was not sent",
            eq(read.get("name"), full) and same(read, www_sent, "type", "ttl", "comment"),
            ttl=read.get("ttl"), comment=read.get("comment"))
    api = own(result(j.call("POST", records, body={
        "type": "A", "name": "api", "content": "203.0.113.30", "ttl": 300, "proxied": False,
        "comment": f"api {tag}"})).get("id"))
    txt = own(result(j.call("POST", records, body={
        "type": "TXT", "name": f"_verify.{zone_name}", "content": f"bench-verify-{tag}", "ttl": 3600})).get("id"))
    page = obj(j.call("GET", records, query={"type": "A"}))
    rows = rows_of(page)
    j.check("type filter lists only A records",
            same_items(ids_of(rows), [wid, api]) and all(r.get("type") == "A" for r in rows), listed=ids_of(rows))
    info = obj(page.get("result_info"))
    j.check("result_info counts the matching records", eq(info.get("count"), 2) and eq(info.get("total_count"), 2),
            result_info=info)
    rows = ids_of(rows_of(j.call("GET", records, query={"name": f"api.{zone_name}"})))
    j.check("name filter finds only the record", rows == [api], listed=rows)
    j.call("PUT", f"{records}/{api}", body={"type": "A", "name": "api", "content": "203.0.113.31"})
    put = j.last["status"]
    read = result(j.call("GET", f"{records}/{api}"))
    j.check("put replaces the record",
            eq(read.get("content"), "203.0.113.31") and eq(read.get("ttl"), 1) and not read.get("comment"),
            put_status=put, content=read.get("content"), ttl=read.get("ttl"), comment=read.get("comment"))
    batch = result(j.call("POST", f"{records}/batch", body={
        "deletes": [{"id": txt}], "patches": [{"id": api, "content": "203.0.113.40"}],
        "posts": [{"type": "CNAME", "name": "docs", "content": full, "ttl": 1, "proxied": False}]}))
    patch, post = head(batch.get("patches")), head(batch.get("posts"))
    j.check("batch answers each change",
            ids_of(batch.get("deletes")) == [txt] and eq(patch.get("id"), api) and eq(patch.get("content"), "203.0.113.40")
            and eq(post.get("type"), "CNAME") and eq(post.get("name"), f"docs.{zone_name}"),
            deletes=ids_of(batch.get("deletes")), patch=patch.get("content"), post=post.get("name"))
    docs = own(post.get("id"))
    now = result(j.call("GET", f"{records}/{api}")).get("content")
    j.check("a batch patch reads back", eq(now, "203.0.113.40"), content=now)
    gone = obj(j.call("GET", f"{records}/{txt}"))
    j.check("a batch delete removes the record", j.last["status"] == 404 and eq(code(gone), 81044), code=code(gone))
    j.call("DELETE", f"{records}/{wid}")
    deleted = j.last["status"]
    rows = ids_of(rows_of(j.call("GET", records)))
    j.check("deleted record leaves the zone's records", same_items(rows, [api, docs]),
            delete_status=deleted, listed=rows)


BLIND7_JOURNEYS = {"linear": linear_steps, "mollie": mollie_steps, "chargebee": chargebee_steps,
                   "bitbucket": bitbucket_steps, "cloudflare": cloudflare_steps}


def run_journeys_blind7(srv, sink, args):
    run_journey_table(BLIND7_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- main

RUNNERS = {
    "consistency": (CONSISTENCY_MOCKS, run_consistency),
    "sse": (SSE_MOCKS, run_sse),
    "import": ({}, run_import),
    "journeys": (JOURNEY_MOCKS, run_journeys),
    "journeys_heldout": (HELDOUT_MOCKS, run_journeys_heldout),
    "journeys_blind": (BLIND_MOCKS, run_journeys_blind),
    "journeys_blind2": (BLIND2_MOCKS, run_journeys_blind2),
    "journeys_blind3": (BLIND3_MOCKS, run_journeys_blind3),
    "journeys_blind4": (BLIND4_MOCKS, run_journeys_blind4),
    "journeys_blind5": (BLIND5_MOCKS, run_journeys_blind5),
    "journeys_blind6": (BLIND6_MOCKS, run_journeys_blind6),
    "journeys_blind7": (BLIND7_MOCKS, run_journeys_blind7),
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
    p.add_argument("--jobs", type=int, default=1,
                   help="independent units (a journey's rep, a consistency or SSE rep) run at once "
                        "against each suite's server; 1 (default) runs them in order. Import stays sequential")
    p.add_argument("--suites-parallel", action="store_true",
                   help="run the suites at once, each on its own server (in flight: suites x --jobs)")
    args = p.parse_args()
    if args.jobs < 1:
        p.error("--jobs must be at least 1")
    if not os.path.exists(args.binary) and not os.path.isabs(args.binary):
        args.binary = os.path.join(REPO, args.binary)
    args.binary = os.path.abspath(args.binary)
    if not os.path.exists(args.binary):
        sys.exit(f"{args.binary} not found: cargo build --release -p mockingbird-cli")
    os.makedirs(args.out, exist_ok=True)
    if args.journey:
        # --journey alone means the journeys suite; each journey runs once.
        args.journey = list(dict.fromkeys(args.journey))
        args.suite = args.suite or ["journeys"]
    suites = list(args.suite or DEFAULT_SUITES)
    if args.suites_parallel:
        # A suite's server log is named by model and suite, so each runs once.
        suites = list(dict.fromkeys(suites))
        failed = any(run_units([functools.partial(run_suite, suite, args) for suite in suites], args,
                               jobs=len(suites)))
    else:
        failed = any([run_suite(suite, args) for suite in suites])
    sys.exit(1 if failed else 0)


def run_suite(suite, args):
    """Runs one suite on its own server; True when it broke."""
    mocks, runner = RUNNERS[suite]
    sink = Sink(args.out, args.model, suite)
    print(f"{args.model} / {suite}", file=sys.stderr)
    failed = False
    try:
        with Server(args, suite, mocks) as srv:
            runner(srv, sink, args)
    except Exception as e:  # one broken suite must not stop the others
        failed = True
        sink.add("suite_error", -1, False, None, "error", {"error": f"{type(e).__name__}: {e}"[:500]})
    n_ok = sum(1 for r in sink.rows if r["ok"])
    n_infra = sum(1 for r in sink.rows if not r["ok"] and obj(r.get("detail")).get("infra"))
    infra = f", {n_infra} failed on provider errors" if n_infra else ""
    print(f"  {suite}: {n_ok}/{len(sink.rows)} ok{infra}", file=sys.stderr)
    return failed


if __name__ == "__main__":
    main()
