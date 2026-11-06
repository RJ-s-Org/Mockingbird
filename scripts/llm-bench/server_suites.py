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
  caused (a 5xx, 429 or transport-error fallback; never a timeout) carries the
  label `detail.infra`, which changes no row. Not in the default
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
- journeys_blind8: the same again over five more contracts written for it, in
  `scripts/llm-bench/contracts/blind8/`: Business Central customers and invoices
  (OData), Azure DevOps work items (JSON Patch), commercetools carts, eBay Trading
  listings (SOAP) and Meilisearch documents. Not in the default suites. Same rule;
  no check gates the rest.
- journeys_blind9: the same again over five more contracts written for it, in
  `scripts/llm-bench/contracts/blind9/`: Cloud Resource Manager folders (long-running
  operations, soft delete, moves), Directus items (bracket filters), Cloudinary images
  (multipart uploads), Modern Treasury ledgers (idempotency keys, header cursors) and
  Xero contacts (a tenant header). Not in the default suites. Same rule; no check
  gates the rest.
- journeys_blind10: the same again over five more contracts written for it, in
  `scripts/llm-bench/contracts/blind10/`: Databricks SCIM users and groups (PatchOp
  operations), Google Calendar events (etags, sync tokens), Heroku apps (natural keys,
  Range header paging), Kubernetes config maps (label selectors, resourceVersion
  conflicts, merge patches) and PagerDuty incidents (a From header, de-duplication
  keys, include[]). Not in the default suites. Same rule; no check gates the rest.
- journeys_blind11: the same again over five more contracts written for it, in
  `scripts/llm-bench/contracts/blind11/`: BigCommerce products and orders (V3
  envelopes beside bare V2 arrays, archived orders), Help Scout conversations (HAL,
  Resource-ID headers, JSON Patch), Mux live streams (disable/enable actions, nested
  playback ids), Postmark templates and message streams (aliases, ErrorCode, archive
  and unarchive) and HCP Terraform workspaces (JSON:API, locks, variables). Not in the
  default suites. Same rule; no check gates the rest.
- journeys_blind12: the same again over five more contracts written for it, in
  `scripts/llm-bench/contracts/blind12/`: CouchDB documents (revisions, 409 conflicts,
  tombstones, _all_docs key ranges, Mango selectors), RabbitMQ management (objects named
  by virtual host and name, idempotent declarations, bindings, publish and get), NetBox
  IPAM (DRF lists, choice fields, a VRF's unique addresses, available-ips allocation),
  Harvest time entries (task assignments, running timers, links.next paging) and Lob
  addresses and letters (metadata filters, after cursors, scheduled letters, cancelling,
  Idempotency-Key). Not in the default suites. Same rule; no check gates the rest.
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

World descriptions: `--world none|derived|agent` (default none) sets the description
each suite's mocks run with. `derived` writes Mockingbird's heuristic description back
as a revision; `agent` has a coding agent write one per contract, blind, before the
suites run (cached per contract hash; `--author stub` is free). Rows of a run with a
world carry `world`, its proxy label and server log end in `@<world>`, and each write
is recorded in `<out>/world-applied.jsonl`. See world_author.py and the README.

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
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zlib
from concurrent.futures import FIRST_EXCEPTION, ThreadPoolExecutor, wait
from datetime import datetime, timedelta, timezone
from xml.etree import ElementTree

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import world_author  # noqa: E402
from world_author import WORLDS  # noqa: E402

REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
SUITES = ("consistency", "sse", "import", "journeys", "journeys_heldout", "journeys_blind", "journeys_blind2",
          "journeys_blind3", "journeys_blind4", "journeys_blind5", "journeys_blind6", "journeys_blind7",
          "journeys_blind8", "journeys_blind9", "journeys_blind10", "journeys_blind11",
          "journeys_blind12")
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


def run_label(model, suite, args):
    """The proxy label and server log name of a (model, suite) run: `<slug>~<suite>`, and
    `@<world>` after it when the run writes a world description (`--world`)."""
    world = getattr(args, "world", "none") or "none"
    return f"{slug_of(model)}~{suite}" + ("" if world == "none" else f"@{world}")


def session_slug(model):
    """`slug_of(model)` with every character a header value may not carry
    unquoted (such as the `@` of a variant like `x/y@nojson`) as `-`."""
    return re.sub(r"[^A-Za-z0-9_.-]", "-", slug_of(model))


class Sink:
    """Appends rows to <out>/server.jsonl; safe across concurrent processes
    (flock) and across the threads of --jobs (a lock per sink)."""

    def __init__(self, out, model, suite, world="none"):
        self.path = os.path.join(out, "server.jsonl")
        self.model, self.suite, self.world = model, suite, world
        self.rows = []
        self.lock = threading.Lock()

    def add(self, case, rep, ok, latency_ms, source, detail, ttfe_ms=None):
        row = {"model": self.model, "suite": self.suite, "case": case, "rep": rep, "ok": bool(ok),
               "latency_ms": None if latency_ms is None else round(latency_ms, 1)}
        if ttfe_ms is not None or self.suite == "sse":
            row["ttfe_ms"] = None if ttfe_ms is None else round(ttfe_ms, 1)
        row["source"] = source
        row["detail"] = detail
        if self.world != "none":
            row["world"] = self.world
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


def write_config(directory, model, base_url, ports, mocks, suite, tag_journeys=False):
    """`tag_journeys` (replay.py's record and replay): each mock gets its own copy of the
    profile that sends `x-bench-journey: <mock>`, so the proxy can tell the journeys'
    calls apart, and a fixed `faker_seed`, so a fallback answers the same each run."""
    lines = [
        "llm:\n  default_profile: bench\n  profiles:\n    bench:\n",
        "      api_key: env:OPENROUTER_API_KEY\n",
        f"      base_url: {json.dumps(base_url)}\n",
        f"      model: {json.dumps(model)}\n",
    ]
    if tag_journeys:
        for name in mocks:
            lines += [
                f"    bench_{name}:\n",
                "      api_key: env:OPENROUTER_API_KEY\n",
                f"      base_url: {json.dumps(base_url)}\n",
                f"      model: {json.dumps(model)}\n",
                f"      headers:\n        x-bench-journey: {json.dumps(name)}\n",
            ]
    lines += [
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
            if tag_journeys:
                lines.append(f"    llm_profile: bench_{name}\n    faker_seed: 1\n")
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


# A fallback answer the provider caused, not the model: the provider answered 5xx or
# 429 ("no capacity", rate limited), answered any other status with a structured
# rate-limit signal (`RATE_LIMITED`), or the connection to it failed. Anchored, so a
# reason that starts with the model's own failure (`LLM output truncated: ...; not
# salvaged: LLM returned HTTP 503`) is not one. An LLM timeout is not one either: it is
# often the model being slow (aggregate.py counts timeouts on their own). The bench
# proxy's 424 after repeated upstream 4xx rejections is the request's failure, not the
# provider's (older runs sent it as a 502, hence the text check).
# Known limit: a repair call that gets a 503 after an invalid first answer is reported
# as a bare `LLM returned HTTP 503` and cannot be told apart, so it is labelled infra.
# That is why the label never changes a row and never causes a re-run.
INFRA_REASON = re.compile(r"^LLM returned HTTP (5\d\d|429)\b|^LLM transport error")
# A rate limit under another status counts only on a structured signal: an error
# `"code":429` in the body, or OpenRouter's exact phrase "temporarily rate-limited
# upstream". Free text that merely mentions a rate limit (a `rate_limit` field, "Too
# Many Requests") does not. Only for a reason that is itself an HTTP error from the LLM,
# so the model's own failure that merely ends in one (`LLM output truncated: ...; not
# salvaged: ...`) stays the model's.
RATE_LIMITED = re.compile(r'^LLM returned HTTP \d{3}\b.*("code"\s*:\s*429\b|temporarily rate-limited upstream)',
                          re.S)


def infra_reason(last):
    """The fallback reason of `last` (a `Journey.last`) when the mock's answer was a
    fallback forced by a clear provider error, else None."""
    reason = str(last.get("fallback_reason") or "")
    provider = INFRA_REASON.search(reason) or RATE_LIMITED.search(reason)
    if last.get("source") == "fallback" and provider and "kept rejecting" not in reason:
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
        self.base_url = f"{args.proxy_base.rstrip('/')}/r/{run_label(args.model, suite, args)}/api/v1"
        logs = os.path.join(args.out, "server-logs")
        os.makedirs(logs, exist_ok=True)
        self.log_path = os.path.join(logs, f"{run_label(args.model, suite, args)}.log")
        self.proc = None
        self.log = None

    def use_ports(self, ports):
        self.ports = ports
        self.config = write_config(self.dir, self.args.model, self.base_url, ports, self.mocks, self.suite,
                                   tag_journeys=getattr(self.args, "tag_journeys", False))
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

    A failed check judged on a call the provider failed (`infra_reason`) carries
    `detail.infra` true. That is a label only: the row, its `ok` and every other row
    are what they would be without it, and no run is repeated or dropped. The label
    goes by the latest call, which is the call a check judges in all but a few steps.

    A failed check without that label, when any earlier call of the same run fell back
    on a provider failure, carries `detail.infra_cascade` true instead, whatever its own
    call returned (a timeout too). It is coarse: the session may hold the earlier
    fallback's made-up state, or the model may simply have failed. It is a label only,
    not a discount: the row stays the model's in every score."""

    def __init__(self, name, srv, sink, args, rep):
        self.name, self.srv, self.sink, self.args, self.rep = name, srv, sink, args, rep
        session = f"bench-{session_slug(args.model)}-{int(time.time())}-{name}-{rep}"
        self.headers = {"x-mockingbird-session": session}
        self.rng = random.Random(f"{args.model}|{name}|{rep}")
        self.done = set()
        self.last = {"status": None, "source": "?", "ms": None, "fallback_reason": None}
        self.last_infra = None  # the infra reason the latest check failed on, if any
        self.last_call_infra = False  # the latest call fell back on a provider failure
        self.earlier_infra = False  # a call before the latest one did
        self.raw = b""  # the raw body of the latest answer
        self.answer_headers = {}  # the headers of the latest answer

    def call(self, method, path, query=None, body=None, form=None, data=None):
        """The JSON body of the response, or {} when it is not a JSON object. `data` is a
        raw body, sent with the content-type in `self.headers`; `self.raw` keeps the raw
        body of the response, for answers that are not JSON, and `self.answer_headers`
        its headers, for paging that answers in headers."""
        url = f"{self.srv.data}/{self.name}{path}"
        if query:
            url += "?" + urllib.parse.urlencode(query)
        ms, status, hdrs, parsed, raw = request(url, method, self.headers, json_body=body, form=form, data=data,
                                                timeout=self.args.timeout)
        self.raw = raw
        self.answer_headers = hdrs or {}
        self.earlier_infra = self.earlier_infra or self.last_call_infra
        self.last = {"status": status, "source": source_of(hdrs), "ms": ms,
                     "fallback_reason": fallback_reason(hdrs)}
        self.last_call_infra = bool(infra_reason(self.last))
        if status >= 400 or status == 0:
            self.last["error"] = excerpt(raw)
        return parsed if isinstance(parsed, (dict, list)) else {}

    def check(self, name, ok, infra=None, **detail):
        """`infra`: None labels the row by the latest call; a reason or False sets it."""
        self.done.add(name)
        detail = dict(detail, **{k: v for k, v in self.last.items() if k not in ("source", "ms") and v is not None})
        if infra is None:
            infra = infra_reason(self.last)
        self.last_infra = infra if infra and not ok else None
        if self.last_infra:
            detail["infra"] = True
        elif not ok and self.earlier_infra:
            detail["infra_cascade"] = True
        journey_check(self.sink, self.name, self.rep, name, bool(ok), detail,
                      self.last["source"], self.last["ms"])

    def stop(self, gate):
        self.check(gate, False)
        raise JourneyStop(gate)


def run_one_journey(name, steps, srv, sink, args, rep):
    """Runs a journey once and records every one of its checks. When a gate fails on a
    provider error (`Journey.check` labels it `detail.infra`), the checks it skips carry
    the label too. A journey that breaks on an exception is the model's: `call()` never
    raises, so the exception comes from reading an answer, which may be an earlier one."""
    mark_rep(srv, args, name, rep)
    j = Journey(name, srv, sink, args, rep)
    ended_on_infra = None
    try:
        steps(j)
        reason = None
    except JourneyStop as e:
        reason = f"stopped at {e}"
        ended_on_infra = j.last_infra
    except Exception as e:  # noqa: BLE001 - a journey that breaks is recorded, not raised
        reason = f"{type(e).__name__}: {e}"[:300]
    # A skipped check has no call of its own: every call the run made is an earlier one.
    j.earlier_infra = j.earlier_infra or j.last_call_infra
    for check in JOURNEY_CHECKS[name]:
        if check not in j.done:
            j.last = {"status": None, "source": "skipped", "ms": None, "fallback_reason": None}
            j.check(check, False, infra=ended_on_infra or False, reason=reason or "journey ended early")


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
    names = args.journey or DEFAULT_JOURNEYS
    if getattr(args, "tag_journeys", False):
        run_units([functools.partial(run_reps, JOURNEYS[name], srv, sink, args) for name in names], args)
        return
    run_units([functools.partial(JOURNEYS[name], srv, sink, args, rep)
               for rep in range(args.repeat) for name in names], args)


def run_reps(unit, srv, sink, args):
    """A journey's reps one after another (--tag-journeys): the proxy tells reps apart by
    the mark each one posts, so two reps of one journey must never overlap."""
    for rep in range(args.repeat):
        unit(srv, sink, args, rep)


def mark_rep(srv, args, journey, rep):
    """Tells the bench proxy that `journey`'s calls from now on are rep `rep`
    (--tag-journeys only; replay.py keys recorded answers by it)."""
    if not getattr(args, "tag_journeys", False):
        return
    body = json.dumps({"label": run_label(args.model, srv.suite, args), "journey": journey, "rep": rep}).encode()
    req = urllib.request.Request(f"{args.proxy_base.rstrip('/')}/bench/mark", data=body, method="POST",
                                 headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as r:
        r.read()


def run_journey_table(table, srv, sink, args):
    """Every (rep, journey) of `table` as its own unit. Each `Journey` has its
    own session (`<model>-<stamp>-<journey>-<rep>`, unique within the suite's
    server), its own seeded rng and its own record of checks; the step
    functions keep no module state."""
    if getattr(args, "tag_journeys", False):
        run_units([functools.partial(run_reps, functools.partial(run_one_journey, name, steps), srv, sink, args)
                   for name, steps in table.items()], args)
        return
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
    # DM channel id the bench makes up, as it does the guild's: `create_message` posts to either.
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
    # The contract does not say a new user holds no roles, so other roles may be held
    # before; this one is not held before the assignment, is held after it, and the
    # count goes up by one.
    before = held.get("total")
    j.check("user holds the assigned role",
            isinstance(before, int) and not by_id(held.get("roles"), role["id"]) and assigned == 204
            and eq(after.get("total"), before + 1) and same(by_id(after.get("roles"), role["id"]), role, "id", "name"),
            before=before, after=after.get("total"), assign_status=assigned)
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
        "page_size 1 returns one meeting", "next page token reaches another meeting",
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
    # One check judges the page size and the next judges the token, so a page 1 without
    # a token is one miss. With no token there is nothing to follow: page 1 is not asked
    # for again (a cached repeat of it would be judged as the next page).
    j.check("page_size 1 returns one meeting", len(first) == 1, listed=first)
    if isinstance(token, str) and token:
        nxt = ids_of(obj(j.call("GET", "/v2/users/me/meetings",
                                query={"type": "scheduled", "page_size": 1, "next_page_token": token})).get("meetings"))
        j.check("next page token reaches another meeting", len(nxt) == 1 and nxt[0] not in first, first=first, listed=nxt)
    else:
        j.check("next page token reaches another meeting", False, first=first, next_page_token=token,
                reason="page 1 gave no next_page_token")
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
    # Adding the web links may have changed the folder ("every change to a folder gives
    # it a new etag"), so the etag sent is the one the folder reads with now.
    etag = given(call("GET", f"/2.0/folders/{c}").get("etag"), etag)
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
        "page size 1 answers one profile", "next link reaches the other profile",
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
    # As in Zoom: the page size and the next link are judged apart, and with no next
    # link page 1 is not asked for again.
    j.check("page size 1 answers one profile", len(first) == 1, listed=ids_of(first))
    if cur:
        rest = rows_of(j.call("GET", "/api/profiles", query={"filter": both, "page[size]": 1, "page[cursor]": cur}))
        j.check("next link reaches the other profile", len(rest) == 1 and same_items(ids_of(first + rest), [p1, p2]),
                first=ids_of(first), listed=ids_of(rest))
    else:
        j.check("next link reaches the other profile", False, first=ids_of(first),
                next=obj(page.get("links")).get("next"), reason="page 1 gave no next link with a page[cursor]")
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


# ---------------------------------------------------------------- eighth blind journeys

# The same rule again, over five more contracts written for this suite from each real
# API's public documentation (scripts/llm-bench/contracts/blind8/). They were picked for
# API styles: OData v4 with keys in parentheses, $filter/$top/$skip/$count, server-driven
# paging by @odata.nextLink, @odata.etag with If-Match and a bound action (Business
# Central), JSON Patch documents with a test operation on the revision, WIQL queries in
# POST bodies and a batch read (Azure DevOps), update-action lists guarded by a version
# in the body, money in cent amounts and carts addressed by key (commercetools), SOAP
# envelopes with errors as Ack Failure and money in a currencyID attribute (eBay
# Trading), and asynchronous writes answered with tasks to poll, search in POST bodies
# and batches of documents (Meilisearch). No check gates the rest: a journey goes on
# with an id of its own when a call gave none. A check asserts only what its contract
# promises a client. Do not edit a check to suit results.
BLIND8_CHECKS = {
    "businesscentral": [
        "customer keeps its fields and answers an etag", "read by key matches the created customer",
        "patch with the current etag changes the phone and the etag", "a stale etag is refused",
        "a refused patch changes nothing", "$filter finds only the customer",
        "$top 1 answers one customer and the count", "$skip 1 reaches the other customer",
        "maxpagesize 1 answers a nextLink", "nextLink reaches the other customer",
        "a new invoice is a draft for its customer", "invoice totals its line", "posted invoice reads as open",
        "deleted customer is not found",
    ],
    "azuredevops": [
        "create applies each patch operation", "a new work item is rev 1 in its project",
        "read matches the created work item", "a passing test lets the replace through",
        "a failed test refuses the document", "a refused document changes nothing", "remove takes the field away",
        "child links to its parent", "wiql finds the tagged work items", "wiql $top 1 answers one of them",
        "batch answers the fields asked for", "comment keeps its text and work item", "comment list holds the comment",
        "deleted work item is not found",
    ],
    "commercetools": [
        "cart keeps its key, currency and email", "a new cart is version 1 and active",
        "read by key matches the created cart", "a custom line item totals the cart",
        "an update answers the next version", "a stale version is refused", "a refused update changes nothing",
        "two actions apply in one update", "where finds only the cart", "limit 1 answers one cart and the total",
        "offset 1 reaches the other cart", "an order takes the cart's total", "an ordered cart reads as ordered",
        "deleted cart is not found",
    ],
    "ebay": [
        "a listing answers Success and an ItemID", "listing fees are in the site's currency",
        "read matches the listed item", "a new listing is active", "read by SKU finds the item",
        "a taken SKU is refused", "revise changes the price and keeps the ItemID", "revise keeps what it was not sent",
        "a second listing takes another ItemID", "the active list counts both listings",
        "page 2 reaches the other listing", "ending answers the end time", "an ended listing reads as completed",
        "the active list leaves out the ended listing",
    ],
    "meilisearch": [
        "index creation answers an enqueued task", "the index task succeeds", "settings read back after their task",
        "a batch addition indexes every document", "a document reads by its primary key",
        "the document list counts every document", "offset reaches the last document",
        "a query finds the matching document", "a filter narrows the hits", "sort orders the hits by year",
        "limit and offset page the hits", "a filter on an unfilterable attribute is refused",
        "a partial update keeps the other attributes", "a deleted document is not found",
    ],
}
JOURNEY_CHECKS.update(BLIND8_CHECKS)
BLIND8_DIR = "scripts/llm-bench/contracts/blind8"
BLIND8_CONTRACTS = {"ebay": "ebay-trading.wsdl"}
# Contracts alone: no persona.
BLIND8_MOCKS = {name: (f"{BLIND8_DIR}/{BLIND8_CONTRACTS.get(name, f'{name}.openapi.yaml')}", f"/{name}", None)
                for name in BLIND8_CHECKS}


@contextlib.contextmanager
def sent_headers(j, extra):
    """`extra` added to the journey's headers for the calls inside the block."""
    saved = {k: j.headers.get(k) for k in extra}
    j.headers.update(extra)
    try:
        yield
    finally:
        for k, v in saved.items():
            if v is None:
                j.headers.pop(k, None)
            else:
                j.headers[k] = v


def made_guid(j):
    return str(uuid.UUID(int=j.rng.getrandbits(128), version=4))


def businesscentral_steps(j):
    # OData keys in parentheses. The two customers share a postal code made for this run,
    # so a filter on it matches only them. PATCH and DELETE send the etag last answered;
    # the stale one is the etag from before the first patch.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)

    def own(value):
        return value if isinstance(value, str) and value else made_guid(j)

    def etag(thing):
        value = obj(thing).get("@odata.etag")
        return value if isinstance(value, str) and value else None

    company = own(head(obj(j.call("GET", "/companies")).get("value")).get("id"))
    base = f"/companies({company})"

    def customer(cid):
        return obj(j.call("GET", f"{base}/customers({cid})"))

    def patch(cid, if_match, body):
        with sent_headers(j, {"If-Match": if_match}):
            return obj(j.call("PATCH", f"{base}/customers({cid})", body=body))

    def listed(query, prefer=None):
        with sent_headers(j, {"Prefer": prefer} if prefer else {}):
            page = obj(j.call("GET", f"{base}/customers", query=query))
        return ids_of(page.get("value")), page

    postal = f"{tag}"
    sent = {"displayName": f"Atlas Bikes {tag}", "type": "Company", "addressLine1": "Rua Augusta 100",
            "city": "Lisbon", "country": "PT", "postalCode": postal, "email": f"orders.{tag}@atlasbikes.example",
            "phoneNumber": "+351 21 000 0001"}
    made = obj(j.call("POST", f"{base}/customers", body=sent))
    first_tag = etag(made)
    j.check("customer keeps its fields and answers an etag",
            j.last["status"] == 201 and same(made, sent, "displayName", "city", "postalCode", "email")
            and first_tag is not None,
            displayName=made.get("displayName"), postalCode=made.get("postalCode"), etag=first_tag)
    c1 = own(made.get("id"))
    read = customer(c1)
    j.check("read by key matches the created customer",
            eq(read.get("id"), c1)
            and same(read, sent, "displayName", "addressLine1", "city", "country", "postalCode", "email"),
            id=read.get("id"), displayName=read.get("displayName"))
    current = etag(read) or first_tag
    phone = "+351 21 000 0002"
    patched = patch(c1, current or "*", {"phoneNumber": phone})
    second_tag = etag(patched)
    j.check("patch with the current etag changes the phone and the etag",
            j.last["status"] == 200 and eq(patched.get("phoneNumber"), phone)
            and same(patched, sent, "displayName", "email") and second_tag is not None and second_tag != current,
            phoneNumber=patched.get("phoneNumber"), etag_before=current, etag_after=second_tag)
    refused = patch(c1, current or 'W/"JzQ0O2JlbmNoOzEn"', {"phoneNumber": "+351 21 000 0003"})
    j.check("a stale etag is refused",
            j.last["status"] == 409 and eq(obj(refused.get("error")).get("code"), "Request_EntityChanged"),
            error=refused.get("error"))
    now = customer(c1).get("phoneNumber")
    j.check("a refused patch changes nothing", eq(now, phone), phoneNumber=now)
    c2 = own(obj(j.call("POST", f"{base}/customers", body={
        "displayName": f"Atlas Cycles {tag}", "type": "Company", "city": "Porto", "country": "PT",
        "postalCode": postal, "email": f"hello.{tag}@atlascycles.example"})).get("id"))
    rows, _ = listed({"$filter": f"displayName eq 'Atlas Bikes {tag}'"})
    j.check("$filter finds only the customer", rows == [c1], listed=rows)
    # Sorted by displayName: "Atlas Bikes" comes before "Atlas Cycles".
    by_postal = {"$filter": f"postalCode eq '{postal}'", "$orderby": "displayName"}
    first, page = listed(dict(by_postal, **{"$top": 1, "$count": "true"}))
    j.check("$top 1 answers one customer and the count", first == [c1] and eq(page.get("@odata.count"), 2),
            listed=first, count=page.get("@odata.count"))
    rest, _ = listed(dict(by_postal, **{"$top": 1, "$skip": 1}))
    j.check("$skip 1 reaches the other customer", rest == [c2], first=first, listed=rest)
    first, page = listed(by_postal, prefer="odata.maxpagesize=1")
    nxt = page.get("@odata.nextLink")
    j.check("maxpagesize 1 answers a nextLink",
            first == [c1] and query_value(nxt, "$skiptoken") is not None, listed=first, next_link=nxt)
    follow = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(nxt).query)) if query_value(nxt, "$skiptoken") else dict(
        by_postal, **{"$skiptoken": made_up(j, "", 12)})
    rest, page = listed(follow, prefer="odata.maxpagesize=1")
    j.check("nextLink reaches the other customer", rest == [c2] and not page.get("@odata.nextLink"),
            first=first, listed=rest, next_link=page.get("@odata.nextLink"))
    item = own(obj(j.call("POST", f"{base}/items", body={
        "number": f"BK-{tag}", "displayName": "Commuter helmet", "type": "Inventory", "unitPrice": 12.5,
        "baseUnitOfMeasureCode": "PCS"})).get("id"))
    invoice = obj(j.call("POST", f"{base}/salesInvoices", body={"customerId": c1, "externalDocumentNumber": f"PO-{tag}"}))
    j.check("a new invoice is a draft for its customer",
            j.last["status"] == 201 and eq(invoice.get("status"), "Draft") and eq(invoice.get("customerId"), c1),
            invoice_status=invoice.get("status"), customerId=invoice.get("customerId"))
    inv = own(invoice.get("id"))
    j.call("POST", f"{base}/salesInvoices({inv})/salesInvoiceLines",
           body={"lineType": "Item", "itemId": item, "quantity": 3, "unitPrice": 12.5})
    total = obj(j.call("GET", f"{base}/salesInvoices({inv})")).get("totalAmountExcludingTax")
    j.check("invoice totals its line", near(total, 37.5, 0.005), totalAmountExcludingTax=total)
    j.call("POST", f"{base}/salesInvoices({inv})/Microsoft.NAV.post")
    posted = j.last["status"]
    read = obj(j.call("GET", f"{base}/salesInvoices({inv})"))
    j.check("posted invoice reads as open", eq(read.get("id"), inv) and eq(read.get("status"), "Open"),
            post_status=posted, invoice_status=read.get("status"))
    with sent_headers(j, {"If-Match": etag(customer(c2)) or "*"}):
        j.call("DELETE", f"{base}/customers({c2})")
    deleted = j.last["status"]
    gone = obj(obj(j.call("GET", f"{base}/customers({c2})")).get("error"))
    j.check("deleted customer is not found",
            j.last["status"] == 404 and given(gone.get("code"), None) is not None and isinstance(gone.get("message"), str),
            delete_status=deleted, error=gone)


def azuredevops_steps(j):
    # Writes are JSON Patch documents. The project is named for this run and every work
    # item made here carries its tag, so a WIQL query on the tag finds the two made here.
    j.headers["authorization"] = "Basic " + base64.b64encode(b":bench-placeholder-pat").decode()
    tag = j.rng.randrange(10**5, 10**6)
    project = f"Ledger{tag}"
    root = f"/bench-org/{project}/_apis/wit"
    version = {"api-version": "7.1"}

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**4, 10**6)

    def patch_doc(method, path, ops, **query):
        with sent_headers(j, {"content-type": "application/json-patch+json"}):
            return obj(j.call(method, path, query=dict(version, **query), data=json.dumps(ops).encode()))

    def fields(item):
        return obj(obj(item).get("fields"))

    def work_item(n, **query):
        return obj(j.call("GET", f"{root}/workitems/{n}", query=dict(version, **query)))

    def wiql(**query):
        answer = obj(j.call("POST", f"{root}/wiql", query=dict(version, **query), body={"query": (
            "SELECT [System.Id] FROM WorkItems WHERE [System.TeamProject] = @project "
            f"AND [System.Tags] CONTAINS '{label}'")}))
        return ids_of(answer.get("workItems"))

    label = f"bench-{tag}"
    title = f"Invoice totals round wrong {tag}"
    description = "<div>Totals are off by one cent on multi-line invoices.</div>"
    made = patch_doc("POST", f"{root}/workitems/$Task", [
        {"op": "add", "path": "/fields/System.Title", "value": title},
        {"op": "add", "path": "/fields/System.Description", "value": description},
        {"op": "add", "path": "/fields/Microsoft.VSTS.Common.Priority", "value": 2},
        {"op": "add", "path": "/fields/System.Tags", "value": label}])
    f = fields(made)
    j.check("create applies each patch operation",
            j.last["status"] == 200 and eq(f.get("System.Title"), title) and eq(f.get("System.Description"), description)
            and eq(f.get("Microsoft.VSTS.Common.Priority"), 2) and eq(f.get("System.Tags"), label)
            and eq(f.get("System.WorkItemType"), "Task"),
            title=f.get("System.Title"), priority=f.get("Microsoft.VSTS.Common.Priority"),
            tags=f.get("System.Tags"), work_item_type=f.get("System.WorkItemType"))
    j.check("a new work item is rev 1 in its project",
            eq(made.get("rev"), 1) and eq(f.get("System.TeamProject"), project),
            rev=made.get("rev"), team_project=f.get("System.TeamProject"))
    i1 = own(made.get("id"))
    read = work_item(i1)
    f = fields(read)
    j.check("read matches the created work item",
            eq(read.get("id"), i1) and eq(f.get("System.Title"), title) and eq(f.get("System.Description"), description)
            and eq(f.get("Microsoft.VSTS.Common.Priority"), 2),
            id=read.get("id"), title=f.get("System.Title"))
    rev = read.get("rev") if is_int(read.get("rev")) else 1
    renamed = f"Invoice totals round once, not per line {tag}"
    answer = patch_doc("PATCH", f"{root}/workitems/{i1}", [
        {"op": "test", "path": "/rev", "value": rev},
        {"op": "replace", "path": "/fields/System.Title", "value": renamed}])
    j.check("a passing test lets the replace through",
            j.last["status"] == 200 and eq(fields(answer).get("System.Title"), renamed) and eq(answer.get("rev"), rev + 1),
            title=fields(answer).get("System.Title"), rev_before=rev, rev_after=answer.get("rev"))
    patch_doc("PATCH", f"{root}/workitems/{i1}", [
        {"op": "test", "path": "/rev", "value": rev},
        {"op": "replace", "path": "/fields/System.Title", "value": f"Someone else's title {tag}"}])
    j.check("a failed test refuses the document", j.last["status"] == 412)
    read = work_item(i1)
    j.check("a refused document changes nothing",
            eq(fields(read).get("System.Title"), renamed) and eq(read.get("rev"), rev + 1),
            title=fields(read).get("System.Title"), rev=read.get("rev"))
    patch_doc("PATCH", f"{root}/workitems/{i1}", [{"op": "remove", "path": "/fields/System.Description"}])
    removed = j.last["status"]
    read = work_item(i1)
    j.check("remove takes the field away",
            bool(fields(read)) and "System.Description" not in fields(read) and eq(fields(read).get("System.Title"), renamed),
            remove_status=removed, fields=sorted(fields(read)))
    parent_url = given(made.get("url"), f"https://dev.azure.com/bench-org/_apis/wit/workItems/{i1}")
    child = patch_doc("POST", f"{root}/workitems/$Task", [
        {"op": "add", "path": "/fields/System.Title", "value": f"Add a test for 0.335 {tag}"},
        {"op": "add", "path": "/fields/System.Tags", "value": label},
        {"op": "add", "path": "/relations/-", "value": {
            "rel": "System.LinkTypes.Hierarchy-Reverse", "url": parent_url, "attributes": {"comment": "split out"}}}])
    i2 = own(child.get("id"))
    relations = dicts(work_item(i2, **{"$expand": "relations"}).get("relations"))
    parents = [r.get("url") for r in relations if r.get("rel") == "System.LinkTypes.Hierarchy-Reverse"]
    j.check("child links to its parent",
            len(parents) == 1 and isinstance(parents[0], str) and parents[0].lower().endswith(f"/workitems/{i1}"),
            parents=parents)
    rows = wiql()
    j.check("wiql finds the tagged work items", same_items(rows, [i1, i2]), listed=rows)
    rows = wiql(**{"$top": 1})
    j.check("wiql $top 1 answers one of them", len(rows) == 1 and rows[0] in (i1, i2), listed=rows)
    batch = obj(j.call("POST", f"{root}/workitemsbatch", query=version,
                       body={"ids": [i1, i2], "fields": ["System.Id", "System.Title"]}))
    titles = {v.get("id"): fields(v).get("System.Title") for v in dicts(batch.get("value"))}
    j.check("batch answers the fields asked for",
            eq(batch.get("count"), 2) and titles == {i1: renamed, i2: f"Add a test for 0.335 {tag}"}
            and all(set(fields(v)) <= {"System.Id", "System.Title"} for v in dicts(batch.get("value"))),
            count=batch.get("count"), titles=list(titles.values()))
    comments = f"{root}/workItems/{i1}/comments"
    text = f"Reproduced on invoice INV-{tag}: 3 lines at 0.335 each."
    note = obj(j.call("POST", comments, query={"api-version": "7.1-preview.4"}, body={"text": text}))
    j.check("comment keeps its text and work item",
            j.last["status"] == 200 and eq(note.get("text"), text) and eq(note.get("workItemId"), i1),
            text=note.get("text"), workItemId=note.get("workItemId"))
    nid = own(note.get("id"))
    page = obj(j.call("GET", comments, query={"api-version": "7.1-preview.4"}))
    j.check("comment list holds the comment",
            eq(page.get("totalCount"), 1) and eq(by_id(page.get("comments"), nid).get("text"), text),
            totalCount=page.get("totalCount"), listed=ids_of(page.get("comments")))
    j.call("DELETE", f"{root}/workitems/{i2}", query=version)
    deleted = j.last["status"]
    gone = work_item(i2)
    j.check("deleted work item is not found",
            j.last["status"] == 404 and given(gone.get("message"), None) is not None
            and given(gone.get("typeKey"), None) is not None,
            delete_status=deleted, message=gone.get("message"), typeKey=gone.get("typeKey"))


def commercetools_steps(j):
    # Every update sends the version last answered; when an answer gave none the journey
    # goes on with the version the contract says it must be. Both carts share an
    # anonymousId made for this run, so a predicate on it matches only them.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    p = f"/bench-shop-{tag}"
    key, email, anon = f"cart-{tag}", f"ana.{tag}@example.org", f"anon-{tag}"

    def own(value):
        return value if isinstance(value, str) and value else made_guid(j)

    def version(thing, fallback):
        value = obj(thing).get("version")
        return value if is_int(value) else fallback

    def total(thing):
        return obj(obj(thing).get("totalPrice"))

    def euros(money, cents):
        return eq(money.get("currencyCode"), "EUR") and eq(money.get("centAmount"), cents)

    def update(cid, ver, *actions):
        return obj(j.call("POST", f"{p}/carts/{cid}", body={"version": ver, "actions": list(actions)}))

    def error(value):
        return head(obj(value).get("errors"))

    def query(**params):
        page = obj(j.call("GET", f"{p}/carts", query=params))
        return ids_of(page.get("results")), page

    draft = {"currency": "EUR", "key": key, "customerEmail": email, "anonymousId": anon, "country": "PT",
             "taxMode": "Disabled"}
    made = obj(j.call("POST", f"{p}/carts", body=draft))
    j.check("cart keeps its key, currency and email",
            j.last["status"] == 201 and same(made, draft, "key", "customerEmail")
            and eq(total(made).get("currencyCode"), "EUR"),
            key=made.get("key"), customerEmail=made.get("customerEmail"), totalPrice=made.get("totalPrice"))
    j.check("a new cart is version 1 and active", eq(made.get("version"), 1) and eq(made.get("cartState"), "Active"),
            version=made.get("version"), cartState=made.get("cartState"))
    c1 = own(made.get("id"))
    v1 = version(made, 1)
    read = obj(j.call("GET", f"{p}/carts/key={key}"))
    j.check("read by key matches the created cart",
            eq(read.get("id"), c1) and same(read, draft, "key", "customerEmail", "anonymousId"),
            id=read.get("id"), key=read.get("key"))
    added = update(c1, v1, {"action": "addCustomLineItem", "name": {"en": "Gift wrap"}, "quantity": 2,
                            "money": {"currencyCode": "EUR", "centAmount": 1250}, "slug": f"gift-wrap-{tag}"})
    line = head(added.get("customLineItems"))
    j.check("a custom line item totals the cart",
            euros(total(added), 2500) and euros(obj(line.get("totalPrice")), 2500) and eq(line.get("quantity"), 2),
            totalPrice=added.get("totalPrice"), line_total=line.get("totalPrice"))
    j.check("an update answers the next version", eq(added.get("version"), v1 + 1), version=added.get("version"))
    v2 = version(added, v1 + 1)
    stale = update(c1, v1, {"action": "setCustomerEmail", "email": f"rui.{tag}@example.org"})
    j.check("a stale version is refused",
            j.last["status"] == 409 and eq(error(stale).get("code"), "ConcurrentModification")
            and eq(error(stale).get("currentVersion"), v2),
            error=error(stale))
    read = obj(j.call("GET", f"{p}/carts/{c1}"))
    j.check("a refused update changes nothing",
            eq(read.get("customerEmail"), email) and eq(read.get("version"), v2),
            customerEmail=read.get("customerEmail"), version=read.get("version"))
    both = update(c1, v2,
                  {"action": "changeCustomLineItemQuantity", "customLineItemId": given(line.get("id"), made_guid(j)),
                   "quantity": 3},
                  {"action": "setShippingAddress", "address": {"country": "PT", "city": "Lisbon", "postalCode": "1100-148",
                                                               "streetName": "Rua Augusta", "streetNumber": "100"}})
    j.check("two actions apply in one update",
            euros(total(both), 3750) and eq(obj(both.get("shippingAddress")).get("city"), "Lisbon")
            and eq(both.get("version"), v2 + 1),
            totalPrice=both.get("totalPrice"), shippingAddress=both.get("shippingAddress"), version=both.get("version"))
    v3 = version(both, v2 + 1)
    other = obj(j.call("POST", f"{p}/carts", body={"currency": "EUR", "key": f"cart-{tag}-b",
                                                   "customerEmail": f"rui.{tag}@example.org", "anonymousId": anon,
                                                   "taxMode": "Disabled"}))
    c2 = own(other.get("id"))
    rows, _ = query(where=f'customerEmail = "{email}"')
    j.check("where finds only the cart", rows == [c1], listed=rows)
    # Oldest first: the first cart was made before the second.
    first, page = query(where=f'anonymousId = "{anon}"', sort="createdAt asc", limit=1)
    j.check("limit 1 answers one cart and the total",
            first == [c1] and eq(page.get("count"), 1) and eq(page.get("total"), 2),
            listed=first, count=page.get("count"), total=page.get("total"))
    rest, _ = query(where=f'anonymousId = "{anon}"', sort="createdAt asc", limit=1, offset=1)
    j.check("offset 1 reaches the other cart", rest == [c2], first=first, listed=rest)
    order = obj(j.call("POST", f"{p}/orders", body={"cart": {"typeId": "cart", "id": c1}, "version": v3}))
    j.check("an order takes the cart's total",
            j.last["status"] == 201 and eq(order.get("orderState"), "Open") and euros(total(order), 3750)
            and eq(obj(order.get("cart")).get("id"), c1),
            orderState=order.get("orderState"), totalPrice=order.get("totalPrice"), cart=order.get("cart"))
    state = obj(j.call("GET", f"{p}/carts/{c1}")).get("cartState")
    j.check("an ordered cart reads as ordered", eq(state, "Ordered"), cartState=state)
    j.call("DELETE", f"{p}/carts/{c2}", query={"version": version(other, 1)})
    deleted = j.last["status"]
    gone = obj(j.call("GET", f"{p}/carts/{c2}"))
    j.check("deleted cart is not found", j.last["status"] == 404 and eq(error(gone).get("code"), "ResourceNotFound"),
            delete_status=deleted, error=error(gone))


EBAY_NS = "urn:ebay:apis:eBLBaseComponents"


def xml_element(name, value):
    """XML for `value` as element `name`: a dict's `@x` keys are attributes and `#text`
    its text, other keys child elements in order; a list repeats the element."""
    if isinstance(value, list):
        return "".join(xml_element(name, v) for v in value)
    if isinstance(value, dict):
        attrs = "".join(f' {k[1:]}="{xml_escape(str(v))}"' for k, v in value.items() if k.startswith("@"))
        if "#text" in value:
            inner = xml_escape(str(value["#text"]))
        else:
            inner = "".join(xml_element(k, v) for k, v in value.items() if not k.startswith(("@", "#")))
        return f"<{name}{attrs}>{inner}</{name}>"
    if isinstance(value, bool):
        value = "true" if value else "false"
    return f"<{name}>{xml_escape(str(value))}</{name}>"


def xml_escape(text):
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")


def local_name(tag):
    return tag.rsplit("}", 1)[-1] if isinstance(tag, str) else ""


def xml_all(element, path):
    """The elements at `path` (local names joined by `/`) under `element`, any namespace."""
    found = [element] if element is not None else []
    for step in path.split("/"):
        found = [c for e in found for c in e if local_name(c.tag) == step]
    return found


def xml_one(element, path):
    found = xml_all(element, path)
    return found[0] if found else None


def xml_text(element, path):
    found = xml_one(element, path)
    return found.text.strip() if found is not None and isinstance(found.text, str) else None


def soap_answer(raw, name):
    """The SOAP Body's `name` element in `raw`, or an empty element when there is none."""
    try:
        root = ElementTree.fromstring(raw)
    except (ElementTree.ParseError, TypeError, ValueError):
        return ElementTree.Element("none")
    body = next((c for c in root if local_name(c.tag) == "Body"), None)
    return next((c for c in (body if body is not None else []) if local_name(c.tag) == name), ElementTree.Element("none"))


def decimal_of(text):
    try:
        return float(text)
    except (TypeError, ValueError):
        return None


def ebay_steps(j):
    # SOAP 1.1 envelopes with the token in RequesterCredentials. The seller's listings are
    # the two made here; the second is ended, which takes it out of the active list.
    tag = j.rng.randrange(10**5, 10**6)

    def soap(op, **body):
        envelope = (
            '<?xml version="1.0" encoding="utf-8"?>'
            '<soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/">'
            f'<soapenv:Header><RequesterCredentials xmlns="{EBAY_NS}"><eBayAuthToken>bench-placeholder-token'
            '</eBayAuthToken></RequesterCredentials></soapenv:Header><soapenv:Body>'
            + xml_element(f"{op}Request", dict({"@xmlns": EBAY_NS, "Version": "1311"}, **body))
            + "</soapenv:Body></soapenv:Envelope>")
        with sent_headers(j, {"content-type": "text/xml; charset=utf-8", "SOAPAction": '""'}):
            j.call("POST", "/wsapi", data=envelope.encode())
        return soap_answer(j.raw, f"{op}Response")

    def own(value):
        return value if matches("[0-9]+", value) else str(j.rng.randrange(10**11, 10**12))

    def get_item(**which):
        return xml_one(soap("GetItem", **which), "Item")

    def active(per_page, page):
        answer = soap("GetMyeBaySelling", ActiveList={"Include": True, "Sort": "StartTime",
                                                      "Pagination": {"EntriesPerPage": per_page, "PageNumber": page}})
        items = [xml_text(i, "ItemID") for i in xml_all(answer, "ActiveList/ItemArray/Item")]
        return items, xml_text(answer, "ActiveList/PaginationResult/TotalNumberOfEntries")

    def price(item, path="StartPrice"):
        found = xml_one(item, path)
        return (decimal_of(found.text) if found is not None else None), (found.get("currencyID") if found is not None else None)

    sku = f"HLM-{tag}"
    title = f"Commuter bike helmet {tag}"

    def listing(sku_, title_):
        return {"Country": "US", "Currency": "USD", "Description": "Commuter helmet, size M, matte black.",
                "DispatchTimeMax": 3, "ListingDuration": "GTC", "ListingType": "FixedPriceItem",
                "Location": "San Jose, CA", "PrimaryCategory": {"CategoryID": "177831"}, "Quantity": 5,
                "StartPrice": {"@currencyID": "USD", "#text": "19.99"}, "Title": title_, "SKU": sku_,
                "PostalCode": "95125", "ConditionID": 1000, "InventoryTrackingMethod": "SKU"}

    added = soap("AddFixedPriceItem", Item=listing(sku, title))
    ack, item_id = xml_text(added, "Ack"), xml_text(added, "ItemID")
    j.check("a listing answers Success and an ItemID", ack == "Success" and matches("[0-9]+", item_id),
            ack=ack, item_id=item_id)
    fees = xml_all(added, "Fees/Fee")
    currencies = [f.get("currencyID") for f in xml_all(added, "Fees/Fee/Fee")]
    j.check("listing fees are in the site's currency",
            "ListingFee" in [xml_text(f, "Name") for f in fees] and bool(currencies)
            and all(c == "USD" for c in currencies),
            fee_names=[xml_text(f, "Name") for f in fees], currencies=currencies)
    i1 = own(item_id)
    item = get_item(ItemID=i1)
    j.check("read matches the listed item",
            xml_text(item, "ItemID") == i1 and xml_text(item, "Title") == title and xml_text(item, "SKU") == sku
            and xml_text(item, "Quantity") == "5" and price(item) == (19.99, "USD"),
            item_id=xml_text(item, "ItemID"), title=xml_text(item, "Title"), start_price=price(item))
    status = xml_text(item, "SellingStatus/ListingStatus")
    j.check("a new listing is active", status == "Active", listing_status=status)
    found = xml_text(get_item(SKU=sku), "ItemID")
    j.check("read by SKU finds the item", found == i1, item_id=found)
    taken = soap("AddFixedPriceItem", Item=listing(sku, f"Commuter bike helmet, again {tag}"))
    severities = [xml_text(e, "SeverityCode") for e in xml_all(taken, "Errors")]
    j.check("a taken SKU is refused", xml_text(taken, "Ack") == "Failure" and "Error" in severities,
            ack=xml_text(taken, "Ack"), severities=severities)
    revised = soap("ReviseFixedPriceItem", Item={"ItemID": i1, "StartPrice": {"@currencyID": "USD", "#text": "24.99"}})
    revised_ack, revised_id = xml_text(revised, "Ack"), xml_text(revised, "ItemID")
    item = get_item(ItemID=i1)
    j.check("revise changes the price and keeps the ItemID",
            revised_ack == "Success" and revised_id == i1 and price(item) == (24.99, "USD"),
            ack=revised_ack, item_id=revised_id, start_price=price(item))
    j.check("revise keeps what it was not sent",
            xml_text(item, "Title") == title and xml_text(item, "SKU") == sku and xml_text(item, "Quantity") == "5",
            title=xml_text(item, "Title"), sku=xml_text(item, "SKU"), quantity=xml_text(item, "Quantity"))
    second = soap("AddFixedPriceItem", Item=listing(f"HLM-{tag}-K", f"Kids bike helmet {tag}"))
    second_id = xml_text(second, "ItemID")
    j.check("a second listing takes another ItemID",
            xml_text(second, "Ack") == "Success" and matches("[0-9]+", second_id) and second_id != i1,
            ack=xml_text(second, "Ack"), item_id=second_id)
    i2 = second_id if matches("[0-9]+", second_id) and second_id != i1 else own(None)
    # Sorted by StartTime, oldest first: the first listing was made before the second.
    first, entries = active(1, 1)
    j.check("the active list counts both listings", first == [i1] and entries == "2",
            listed=first, total_entries=entries)
    rest, _ = active(1, 2)
    j.check("page 2 reaches the other listing", rest == [i2], first=first, listed=rest)
    ended = soap("EndFixedPriceItem", ItemID=i2, EndingReason="NotAvailable")
    j.check("ending answers the end time",
            xml_text(ended, "Ack") == "Success" and instant(xml_text(ended, "EndTime")) is not None,
            ack=xml_text(ended, "Ack"), end_time=xml_text(ended, "EndTime"))
    status = xml_text(get_item(ItemID=i2), "SellingStatus/ListingStatus")
    j.check("an ended listing reads as completed", status == "Completed", listing_status=status)
    rows, entries = active(10, 1)
    j.check("the active list leaves out the ended listing", rows == [i1] and entries == "1",
            listed=rows, total_entries=entries)


def meilisearch_steps(j):
    # Writes answer tasks; the journey polls each until it has finished (or gives up
    # after a few reads) and goes on either way. The index is new, so its documents are
    # the three added here.
    j.headers["authorization"] = "Bearer bench-placeholder-master-key"
    tag = j.rng.randrange(10**5, 10**6)
    index = f"/indexes/books_{tag}"
    finished = ("succeeded", "failed", "canceled")

    def settle(answer, polls=4):
        uid, task = obj(answer).get("taskUid"), {}
        for _ in range(polls if is_int(uid) else 0):
            task = obj(j.call("GET", f"/tasks/{uid}"))
            if task.get("status") in finished:
                break
        return task

    def search(**body):
        answer = obj(j.call("POST", f"{index}/search", body=body))
        return ids_of(answer.get("hits"), "isbn"), answer

    def code(value):
        return obj(value).get("code")

    books = [{"isbn": f"978{tag}01", "title": "Dune", "author": "Frank Herbert", "genre": "scifi", "year": 1965},
             {"isbn": f"978{tag}02", "title": "Emma", "author": "Jane Austen", "genre": "classic", "year": 1815},
             {"isbn": f"978{tag}03", "title": "Neuromancer", "author": "William Gibson", "genre": "scifi", "year": 1984}]
    d1, d2, d3 = (b["isbn"] for b in books)
    created = obj(j.call("POST", "/indexes", body={"uid": f"books_{tag}", "primaryKey": "isbn"}))
    j.check("index creation answers an enqueued task",
            j.last["status"] == 202 and is_int(created.get("taskUid")) and eq(created.get("status"), "enqueued")
            and eq(created.get("type"), "indexCreation") and eq(created.get("indexUid"), f"books_{tag}"),
            taskUid=created.get("taskUid"), task_status=created.get("status"), type=created.get("type"))
    task = settle(created)
    j.check("the index task succeeds", eq(task.get("status"), "succeeded") and eq(task.get("uid"), created.get("taskUid")),
            task_status=task.get("status"), uid=task.get("uid"))
    settle(obj(j.call("PATCH", f"{index}/settings",
                      body={"filterableAttributes": ["genre", "year"], "sortableAttributes": ["year"]})))
    settings = obj(j.call("GET", f"{index}/settings"))
    j.check("settings read back after their task",
            same_items(settings.get("filterableAttributes"), ["genre", "year"])
            and settings.get("sortableAttributes") == ["year"],
            filterable=settings.get("filterableAttributes"), sortable=settings.get("sortableAttributes"))
    added = obj(j.call("POST", f"{index}/documents", body=books))
    queued = j.last["status"] == 202 and eq(added.get("type"), "documentAdditionOrUpdate")
    task = settle(added)
    details = obj(task.get("details"))
    j.check("a batch addition indexes every document",
            queued and eq(task.get("status"), "succeeded") and eq(details.get("receivedDocuments"), 3)
            and eq(details.get("indexedDocuments"), 3),
            task_status=task.get("status"), details=details)
    doc = obj(j.call("GET", f"{index}/documents/{d1}"))
    j.check("a document reads by its primary key", same(doc, books[0], "isbn", "title", "author", "genre", "year"),
            document=doc)
    page = obj(j.call("GET", f"{index}/documents", query={"limit": 2, "sort": "year:asc"}))
    first = ids_of(page.get("results"), "isbn")
    j.check("the document list counts every document", first == [d2, d1] and eq(page.get("total"), 3),
            listed=first, total=page.get("total"))
    rest = ids_of(obj(j.call("GET", f"{index}/documents",
                             query={"limit": 2, "offset": 2, "sort": "year:asc"})).get("results"), "isbn")
    j.check("offset reaches the last document", rest == [d3], first=first, listed=rest)
    hits, _ = search(q="neuromancer")
    j.check("a query finds the matching document", hits == [d3], hits=hits)
    hits, _ = search(q="", filter="genre = scifi")
    j.check("a filter narrows the hits", same_items(hits, [d1, d3]), hits=hits)
    hits, _ = search(q="", sort=["year:asc"])
    j.check("sort orders the hits by year", hits == [d2, d1, d3], hits=hits)
    hits, answer = search(q="", sort=["year:asc"], limit=1, offset=1)
    j.check("limit and offset page the hits",
            hits == [d1] and eq(answer.get("limit"), 1) and eq(answer.get("offset"), 1)
            and eq(answer.get("estimatedTotalHits"), 3),
            hits=hits, limit=answer.get("limit"), offset=answer.get("offset"),
            estimatedTotalHits=answer.get("estimatedTotalHits"))
    _, answer = search(q="", filter="author = 'Jane Austen'")
    j.check("a filter on an unfilterable attribute is refused",
            j.last["status"] == 400 and eq(code(answer), "invalid_search_filter"), code=code(answer))
    settle(obj(j.call("PUT", f"{index}/documents", body=[{"isbn": d1, "year": 1966}])))
    doc = obj(j.call("GET", f"{index}/documents/{d1}"))
    j.check("a partial update keeps the other attributes",
            eq(doc.get("year"), 1966) and same(doc, books[0], "title", "author", "genre"), document=doc)
    settle(obj(j.call("DELETE", f"{index}/documents/{d2}")))
    gone = obj(j.call("GET", f"{index}/documents/{d2}"))
    j.check("a deleted document is not found", j.last["status"] == 404 and eq(code(gone), "document_not_found"),
            code=code(gone))


BLIND8_JOURNEYS = {"businesscentral": businesscentral_steps, "azuredevops": azuredevops_steps,
                   "commercetools": commercetools_steps, "ebay": ebay_steps, "meilisearch": meilisearch_steps}


def run_journeys_blind8(srv, sink, args):
    run_journey_table(BLIND8_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- ninth blind journeys

# The same rule again, over five more contracts written for this suite from each real
# API's public documentation (scripts/llm-bench/contracts/blind9/). They were picked for
# API styles: long-running operations polled until done, custom methods (`:move`,
# `:undelete`), soft deletes that `showDeleted` lists, a tree whose folders move between
# parents, and partial responses by `fields` (Cloud Resource Manager), filters in
# bracket notation with `_and`/`_or` and rules through relations, dotted `fields`, `-`
# sorts and `meta` counts (Directus), signed multipart uploads carrying an asset's
# metadata, cursors in the body, search expressions, renames, a bulk delete by
# `public_ids[]` and a restore from backup (Cloudinary), idempotency keys on creates,
# double-entry balances, bare-array lists paged by a response header and `metadata[key]`
# filters (Modern Treasury), and the organisation named by a `Xero-tenant-id` header,
# PascalCase envelopes, `where`/`order` expressions, .NET JSON dates and archiving with
# a restore (Xero). No check gates the rest: a journey goes on with an id of its own
# when a call gave none. A check asserts only what its contract promises a client. Do
# not edit a check to suit results.
BLIND9_CHECKS = {
    "resourcemanager": [
        "create answers an operation with its metadata", "the create operation finishes with the folder",
        "a folder reads as active under its parent", "fields answers only the fields asked for",
        "a taken display name is refused among siblings", "list answers the parent's folders by display name",
        "pageSize 1 answers one folder", "the page token reaches the next folder",
        "move finishes under the new parent", "a moved folder leaves its old parent's list",
        "search finds the folder by display name", "delete marks the folder for deletion",
        "showDeleted lists a folder marked for deletion", "undelete makes the folder active again",
    ],
    "directus": [
        "a created product keeps its fields", "a batch create answers every product in order",
        "read by id matches the created product", "fields answers only the fields asked for",
        "a dotted field reads through the category", "a taken sku is refused",
        "_eq and _gte filters narrow the list", "an _or filter joins its branches",
        "a filter reaches through the category", "sort -price orders the products",
        "limit and offset page the sorted products", "meta counts the filtered items",
        "a batch update changes every key", "a deleted product answers FORBIDDEN",
    ],
    "cloudinary": [
        "an upload keeps its public id, tags and context", "an upload answers the file's format and name",
        "details read back the upload", "overwrite false answers the stored asset",
        "the tag list answers the newest first", "max_results 1 answers one asset and a cursor",
        "next_cursor reaches the other asset", "an update replaces the tags",
        "search matches a tag and the context", "rename keeps the tags under the new public id",
        "the old public id is not found after a rename", "a delete reports each public id",
        "a deleted asset leaves the tag list", "restore brings the deleted asset back",
    ],
    "moderntreasury": [
        "a new ledger account has zero balances", "a pending transaction keeps its entries",
        "a repeated idempotency key answers the same transaction", "a repeated key makes no second transaction",
        "pending entries move only the pending balance", "a credit-normal account counts credits as positive",
        "unbalanced entries are refused", "posting sets posted_at", "posted entries reach the posted balance",
        "a posted transaction cannot be archived", "a posted refund nets the balance",
        "metadata filters the transactions", "per_page 1 answers the newest and a cursor header",
        "after_cursor reaches the older transaction",
    ],
    "xero": [
        "a created contact keeps its fields", "dates are .NET JSON dates", "read by ContactID matches the created contact",
        "a taken name is refused as a ValidationException", "an update keeps what it was not sent",
        "where matches the contact by name", "order sorts the matching contacts",
        "page and pageSize page the ordered contacts", "an archived contact leaves the list",
        "includeArchived lists the archived contact", "restoring makes the contact active again",
        "a contact group holds the contacts added to it", "a contact removed from a group leaves it",
        "another organisation does not see the contacts",
    ],
}
JOURNEY_CHECKS.update(BLIND9_CHECKS)
BLIND9_DIR = "scripts/llm-bench/contracts/blind9"
# Contracts alone: no persona.
BLIND9_MOCKS = {name: (f"{BLIND9_DIR}/{name}.openapi.yaml", f"/{name}", None) for name in BLIND9_CHECKS}


def resourcemanager_steps(j):
    # Changes answer operations; the journey polls each until it is done (or gives up
    # after a few reads) and goes on either way. The organization is made up for this
    # run, so the folders under it are the ones made here.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    org = f"organizations/{j.rng.randrange(10**11, 10**12)}"
    folder_name = "folders/[0-9]+"

    def own(name):
        return name if matches(folder_name, name) else f"folders/{j.rng.randrange(10**11, 10**12)}"

    def settle(op, polls=4):
        op = obj(op)
        name = op.get("name")
        for _ in range(0 if op.get("done") is True or not matches("operations/[^/]+", name) else polls):
            op = obj(j.call("GET", f"/v3/{name}"))
            if op.get("done") is True:
                break
        return op

    def create(parent, display):
        return obj(j.call("POST", "/v3/folders", body={"parent": parent, "displayName": display}))

    def listed(parent, **query):
        page = obj(j.call("GET", "/v3/folders", query=dict({"parent": parent}, **query)))
        return [f.get("name") for f in dicts(page.get("folders"))], page

    def finished(op):
        return op.get("done") is True and not op.get("error")

    finance, engineering, payroll = f"Finance-{tag}", f"Engineering-{tag}", f"Payroll-{tag}"
    op = create(org, finance)
    meta = obj(op.get("metadata"))
    j.check("create answers an operation with its metadata",
            j.last["status"] == 200 and matches("operations/.+", op.get("name"))
            and eq(meta.get("displayName"), finance) and eq(meta.get("parent"), org),
            operation=op.get("name"), metadata=meta)
    done = settle(op)
    made = obj(done.get("response"))
    j.check("the create operation finishes with the folder",
            finished(done) and matches(folder_name, made.get("name")) and eq(made.get("displayName"), finance)
            and eq(made.get("parent"), org),
            done=done.get("done"), error=done.get("error"), response=made)
    a = own(made.get("name"))
    read = obj(j.call("GET", f"/v3/{a}"))
    j.check("a folder reads as active under its parent",
            eq(read.get("name"), a) and eq(read.get("displayName"), finance) and eq(read.get("parent"), org)
            and eq(read.get("state"), "ACTIVE"),
            folder=read.get("name"), parent=read.get("parent"), state=read.get("state"))
    part = obj(j.call("GET", f"/v3/{a}", query={"fields": "name,displayName"}))
    j.check("fields answers only the fields asked for", part == {"name": a, "displayName": finance}, answer=part)
    taken = obj(obj(j.call("POST", "/v3/folders", body={"parent": org, "displayName": finance})).get("error"))
    j.check("a taken display name is refused among siblings",
            j.last["status"] == 400 and eq(taken.get("status"), "FAILED_PRECONDITION") and eq(taken.get("code"), 400),
            error=taken)
    b = own(obj(settle(create(org, engineering)).get("response")).get("name"))
    c = own(obj(settle(create(a, payroll)).get("response")).get("name"))
    # Sorted by displayName: "Engineering" comes before "Finance".
    rows, _ = listed(org)
    j.check("list answers the parent's folders by display name", rows == [b, a], listed=rows)
    first, page = listed(org, pageSize=1)
    token = given(page.get("nextPageToken"), None)
    # One check judges the page and the next judges the token, so a page without a token
    # is one miss. With no token there is nothing to follow, and no made-up token is sent.
    j.check("pageSize 1 answers one folder", first == [b], listed=first)
    if token is not None:
        rest, page = listed(org, pageSize=1, pageToken=token)
        j.check("the page token reaches the next folder", rest == [a] and not page.get("nextPageToken"),
                first=first, listed=rest, next_page_token=page.get("nextPageToken"))
    else:
        j.check("the page token reaches the next folder", False, first=first,
                next_page_token=page.get("nextPageToken"), reason="page 1 gave no nextPageToken")
    moved = settle(obj(j.call("POST", f"/v3/{c}:move", body={"destinationParent": b})))
    response = obj(moved.get("response"))
    j.check("move finishes under the new parent",
            finished(moved) and eq(response.get("name"), c) and eq(response.get("parent"), b),
            done=moved.get("done"), response=response)
    under_b, _ = listed(b)
    under_a, _ = listed(a)
    j.check("a moved folder leaves its old parent's list",
            under_b == [c] and under_a == [] and j.last["status"] == 200,
            new_parent=under_b, old_parent=under_a)
    found = obj(j.call("GET", "/v3/folders:search", query={"query": f'displayName="{payroll}"'}))
    hits = [f.get("name") for f in dicts(found.get("folders"))]
    j.check("search finds the folder by display name", hits == [c], hits=hits)
    settle(obj(j.call("DELETE", f"/v3/{c}")))
    read = obj(j.call("GET", f"/v3/{c}"))
    j.check("delete marks the folder for deletion",
            eq(read.get("name"), c) and eq(read.get("state"), "DELETE_REQUESTED")
            and instant(read.get("deleteTime")) is not None,
            state=read.get("state"), deleteTime=read.get("deleteTime"))
    hidden, _ = listed(b)
    shown, _ = listed(b, showDeleted="true")
    j.check("showDeleted lists a folder marked for deletion", hidden == [] and shown == [c],
            listed=hidden, with_deleted=shown)
    settle(obj(j.call("POST", f"/v3/{c}:undelete", body={})))
    state = obj(j.call("GET", f"/v3/{c}")).get("state")
    back, _ = listed(b)
    j.check("undelete makes the folder active again", eq(state, "ACTIVE") and back == [c], state=state, listed=back)


def directus_steps(j):
    # Every list filters on a sku prefix made for this run, so it holds only the products
    # made here. Filters are sent in bracket notation.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    prefix = f"BN{tag}-"
    scoped = {"filter[sku][_starts_with]": prefix}

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**4, 10**6)

    def data(answer):
        return obj(answer).get("data")

    def listed(query, scope=True):
        page = obj(j.call("GET", "/items/products", query=dict(scoped, **query) if scope else query))
        return ids_of(page.get("data")), page

    def code(answer):
        return obj(head(obj(answer).get("errors")).get("extensions")).get("code")

    lighting = own(obj(data(j.call("POST", "/items/categories", body={"name": f"Lighting {tag}"}))).get("id"))
    furniture = own(obj(data(j.call("POST", "/items/categories", body={"name": f"Furniture {tag}"}))).get("id"))
    fields = ("sku", "name", "price", "stock", "status", "category")
    lamp = {"sku": f"{prefix}1", "name": "Desk lamp", "price": 49.0, "stock": 12, "status": "published",
            "category": lighting}
    made = obj(data(j.call("POST", "/items/products", body=lamp)))
    j.check("a created product keeps its fields",
            j.last["status"] == 200 and is_int(made.get("id")) and same(made, lamp, *fields),
            id=made.get("id"), product={f: made.get(f) for f in fields})
    p1 = own(made.get("id"))
    batch = [{"sku": f"{prefix}2", "name": "Floor lamp", "price": 129.0, "stock": 3, "status": "published",
              "category": lighting},
             {"sku": f"{prefix}3", "name": "Oak shelf", "price": 89.0, "stock": 0, "status": "draft",
              "category": furniture},
             {"sku": f"{prefix}4", "name": "Wall hook", "price": 9.5, "stock": 40, "status": "published",
              "category": furniture}]
    answered = dicts(data(j.call("POST", "/items/products", body=batch)))
    ids = ids_of(answered)
    j.check("a batch create answers every product in order",
            [p.get("sku") for p in answered] == [p["sku"] for p in batch]
            and all(is_int(i) for i in ids) and len(set(ids)) == 3 and p1 not in ids,
            skus=[p.get("sku") for p in answered], ids=ids)
    p2, p3, p4 = (own(ids[n]) if len(ids) == 3 else own(None) for n in range(3))
    read = obj(data(j.call("GET", f"/items/products/{p1}")))
    j.check("read by id matches the created product", eq(read.get("id"), p1) and same(read, lamp, *fields),
            id=read.get("id"), product={f: read.get(f) for f in fields})
    part = data(j.call("GET", f"/items/products/{p1}", query={"fields": "id,name"}))
    j.check("fields answers only the fields asked for", part == {"id": p1, "name": "Desk lamp"}, answer=part)
    part = data(j.call("GET", f"/items/products/{p1}", query={"fields": "sku,category.name"}))
    j.check("a dotted field reads through the category",
            part == {"sku": lamp["sku"], "category": {"name": f"Lighting {tag}"}}, answer=part)
    taken = j.call("POST", "/items/products", body={"sku": lamp["sku"], "name": "Desk lamp, again", "price": 1.0})
    j.check("a taken sku is refused", j.last["status"] == 400 and eq(code(taken), "RECORD_NOT_UNIQUE"),
            code=code(taken))
    rows, _ = listed({"filter[status][_eq]": "published", "filter[price][_gte]": 40})
    j.check("_eq and _gte filters narrow the list", same_items(rows, [p1, p2]), listed=rows)
    rows, _ = listed({"filter[_and][0][sku][_starts_with]": prefix,
                      "filter[_and][1][_or][0][stock][_lt]": 5,
                      "filter[_and][1][_or][1][status][_eq]": "draft"}, scope=False)
    j.check("an _or filter joins its branches", same_items(rows, [p2, p3]), listed=rows)
    rows, _ = listed({"filter[category][name][_eq]": f"Furniture {tag}"})
    j.check("a filter reaches through the category", same_items(rows, [p3, p4]), listed=rows)
    # By price, highest first: 129, 89, 49, 9.5.
    rows, _ = listed({"sort": "-price"})
    j.check("sort -price orders the products", rows == [p2, p3, p1, p4], listed=rows)
    rows, _ = listed({"sort": "-price", "limit": 2, "offset": 2})
    j.check("limit and offset page the sorted products", rows == [p1, p4], listed=rows)
    rows, page = listed({"filter[status][_eq]": "published", "meta": "filter_count,total_count", "limit": 1})
    meta = obj(page.get("meta"))
    total = meta.get("total_count")
    j.check("meta counts the filtered items",
            len(rows) == 1 and eq(meta.get("filter_count"), 3) and is_int(total) and total >= 4,
            listed=rows, meta=meta)
    changed = dicts(data(j.call("PATCH", "/items/products", body={"keys": [p3, p4], "data": {"status": "archived"}})))
    rows, _ = listed({"filter[status][_eq]": "archived"})
    j.check("a batch update changes every key",
            same_items(ids_of(changed), [p3, p4]) and all(eq(p.get("status"), "archived") for p in changed)
            and same_items(rows, [p3, p4]),
            answered=ids_of(changed), listed=rows)
    j.call("DELETE", f"/items/products/{p2}")
    deleted = j.last["status"]
    gone = j.call("GET", f"/items/products/{p2}")
    j.check("a deleted product answers FORBIDDEN", j.last["status"] == 403 and eq(code(gone), "FORBIDDEN"),
            delete_status=deleted, code=code(gone))


def tiny_png():
    """A valid 1x1 PNG."""
    def chunk(kind, payload):
        return (struct.pack(">I", len(payload)) + kind + payload
                + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(b"\x00\xff\x00\x00")) + chunk(b"IEND", b""))


def multipart_body(j, fields, files):
    """(content-type, body) of a multipart/form-data request: `fields` as text parts in
    order, then `files` ({name: (filename, content-type, bytes)}) as file parts."""
    boundary = "----bench" + "".join(j.rng.choice("0123456789abcdef") for _ in range(24))
    parts = [f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()
             for name, value in fields.items()]
    for name, (filename, kind, content) in files.items():
        parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"; filename="{filename}"\r\n'
                     f"Content-Type: {kind}\r\n\r\n".encode() + content + b"\r\n")
    parts.append(f"--{boundary}--\r\n".encode())
    return f"multipart/form-data; boundary={boundary}", b"".join(parts)


CLOUDINARY_KEY, CLOUDINARY_SECRET = "614335564976464", "bench-placeholder-secret"


def cloudinary_signed(params, key=CLOUDINARY_KEY, secret=CLOUDINARY_SECRET):
    """`params` with `api_key` and the `signature` Cloudinary's Upload API checks: the hex
    SHA-1 of the signed parameters, sorted and joined as `name=value&...`, then the secret."""
    unsigned = ("file", "api_key", "resource_type", "cloud_name")
    text = "&".join(f"{k}={params[k]}" for k in sorted(params) if k not in unsigned and params[k] != "")
    return dict(params, api_key=key, signature=hashlib.sha1((text + secret).encode()).hexdigest())


def cloudinary_steps(j):
    # Uploads are signed multipart bodies; Admin API calls use Basic authentication. The
    # tags are made for this run, so a tag's list holds only the assets uploaded here.
    j.headers["authorization"] = "Basic " + base64.b64encode(
        f"{CLOUDINARY_KEY}:{CLOUDINARY_SECRET}".encode()).decode()
    tag = j.rng.randrange(10**5, 10**6)
    base = f"/v1_1/bench-{tag}"
    label, helmets = f"bench{tag}", f"helmets{tag}"
    bike, helmet, renamed = f"bike_{tag}", f"helmet_{tag}", f"bike_red_{tag}"
    png = tiny_png()

    def upload(public_id, filename, tags, context, **extra):
        params = cloudinary_signed(dict({"public_id": public_id, "tags": tags, "context": context,
                                         "timestamp": str(int(time.time()))}, **extra))
        kind, body = multipart_body(j, params, {"file": (filename, "image/png", png)})
        with sent_headers(j, {"content-type": kind}):
            return obj(j.call("POST", f"{base}/image/upload", data=body))

    def details(public_id):
        return obj(j.call("GET", f"{base}/resources/image/upload/{public_id}"))

    def by_tag(name, **query):
        page = obj(j.call("GET", f"{base}/resources/image/tags/{name}", query=dict({"tags": "true"}, **query)))
        return ids_of(page.get("resources"), "public_id"), page

    def custom(asset):
        return obj(obj(obj(asset).get("context")).get("custom"))

    def same_asset(asset, asset_id):
        # The asset id the upload answered, or any asset id when it answered none.
        if given(asset_id, None) is not None:
            return eq(asset.get("asset_id"), asset_id)
        return given(asset.get("asset_id"), None) is not None

    bike_context = {"alt": "Red bike", "caption": "Front view"}
    up = upload(bike, "bike.png", f"{label},bikes", "alt=Red bike|caption=Front view")
    j.check("an upload keeps its public id, tags and context",
            j.last["status"] == 200 and eq(up.get("public_id"), bike) and same_items(up.get("tags"), [label, "bikes"])
            and custom(up) == bike_context,
            public_id=up.get("public_id"), tags=up.get("tags"), context=up.get("context"))
    j.check("an upload answers the file's format and name",
            eq(up.get("format"), "png") and eq(up.get("original_filename"), "bike")
            and eq(up.get("resource_type"), "image"),
            format=up.get("format"), original_filename=up.get("original_filename"))
    asset_id, version = up.get("asset_id"), up.get("version")
    read = details(bike)
    j.check("details read back the upload",
            eq(read.get("public_id"), bike) and same_asset(read, asset_id)
            and same_items(read.get("tags"), [label, "bikes"]) and custom(read) == bike_context,
            public_id=read.get("public_id"), asset_id=read.get("asset_id"), tags=read.get("tags"))
    again = upload(bike, "bike-again.png", label, "alt=Another bike", overwrite="false")
    j.check("overwrite false answers the stored asset",
            again.get("existing") is True and eq(again.get("public_id"), bike) and same_asset(again, asset_id)
            and (eq(again.get("version"), version) if is_int(version) else is_int(again.get("version")))
            and same_items(again.get("tags"), [label, "bikes"]),
            existing=again.get("existing"), version=again.get("version"), tags=again.get("tags"))
    upload(helmet, "helmet.png", label, "alt=Blue helmet")
    # Newest first: the helmet was uploaded after the bike.
    rows, _ = by_tag(label)
    j.check("the tag list answers the newest first", rows == [helmet, bike], listed=rows)
    first, page = by_tag(label, max_results=1)
    cursor = given(page.get("next_cursor"), None)
    j.check("max_results 1 answers one asset and a cursor", first == [helmet] and cursor is not None,
            listed=first, next_cursor=cursor)
    rest, page = by_tag(label, max_results=1, next_cursor=cursor or made_up(j, "", 32, LOWER_ALNUM))
    j.check("next_cursor reaches the other asset", rest == [bike] and not page.get("next_cursor"),
            first=first, listed=rest, next_cursor=page.get("next_cursor"))
    updated = obj(j.call("POST", f"{base}/resources/image/upload/{helmet}", body={"tags": f"{label},{helmets}"}))
    rows, _ = by_tag(helmets)
    j.check("an update replaces the tags", same_items(updated.get("tags"), [label, helmets]) and rows == [helmet],
            tags=updated.get("tags"), listed=rows)
    found = obj(j.call("POST", f"{base}/resources/search", body={
        "expression": f'tags={label} AND context.alt="Red bike"', "with_field": ["tags", "context"]}))
    hits = ids_of(found.get("resources"), "public_id")
    j.check("search matches a tag and the context", hits == [bike] and eq(found.get("total_count"), 1),
            hits=hits, total_count=found.get("total_count"))
    j.call("POST", f"{base}/image/rename", form=cloudinary_signed(
        {"from_public_id": bike, "to_public_id": renamed, "timestamp": str(int(time.time()))}))
    rename_status = j.last["status"]
    read = details(renamed)
    j.check("rename keeps the tags under the new public id",
            eq(read.get("public_id"), renamed) and same_asset(read, asset_id)
            and same_items(read.get("tags"), [label, "bikes"]) and custom(read) == bike_context,
            rename_status=rename_status, public_id=read.get("public_id"), tags=read.get("tags"))
    old = obj(details(bike).get("error"))
    j.check("the old public id is not found after a rename",
            j.last["status"] == 404 and given(old.get("message"), None) is not None, error=old)
    nothing = f"nothing_{tag}"
    answer = obj(j.call("DELETE", f"{base}/resources/image/upload",
                        query=[("public_ids[]", helmet), ("public_ids[]", nothing)]))
    j.check("a delete reports each public id", answer.get("deleted") == {helmet: "deleted", nothing: "not_found"},
            deleted=answer.get("deleted"))
    rows, _ = by_tag(label)
    j.check("a deleted asset leaves the tag list", rows == [renamed], listed=rows)
    restored = obj(obj(j.call("POST", f"{base}/resources/image/upload/restore",
                              body={"public_ids": [helmet]})).get(helmet))
    back = details(helmet)
    rows, _ = by_tag(label)
    j.check("restore brings the deleted asset back",
            eq(restored.get("public_id"), helmet) and eq(back.get("public_id"), helmet)
            and same_items(rows, [helmet, renamed]),
            restored=restored.get("public_id"), read=back.get("public_id"), listed=rows)


def moderntreasury_steps(j):
    # Every create sends an Idempotency-Key; the replay sends the first one again. The
    # ledger is new, and lists filter on metadata made for this run, so they hold only the
    # transactions made here (the refused one carries other metadata).
    j.headers["authorization"] = "Basic " + base64.b64encode(b"bench-org-id:bench-placeholder-key").decode()
    tag = j.rng.randrange(10**5, 10**6)
    run = {"run": str(tag)}

    def own(value):
        return value if isinstance(value, str) and value else made_guid(j)

    def post(path, body, key=None):
        with sent_headers(j, {"Idempotency-Key": key or made_guid(j)}):
            return obj(j.call("POST", path, body=body))

    def account(aid):
        return obj(obj(j.call("GET", f"/api/ledger_accounts/{aid}")).get("balances"))

    def balance(balances, which):
        return obj(obj(balances).get(which))

    def amounts(bal, amount, debits, credits):
        return eq(bal.get("amount"), amount) and eq(bal.get("debits"), debits) and eq(bal.get("credits"), credits)

    def entries(thing):
        return any_order([(e.get("direction"), e.get("amount"), e.get("ledger_account_id"))
                          for e in dicts(obj(thing).get("ledger_entries"))])

    def transactions(**query):
        rows = j.call("GET", "/api/ledger_transactions",
                      query=dict({"ledger_id": ledger, "metadata[run]": str(tag)}, **query))
        return ids_of(rows if isinstance(rows, list) else []), j.answer_headers.get("X-After-Cursor")

    def entry(amount, direction, aid):
        return {"amount": amount, "direction": direction, "ledger_account_id": aid}

    ledger = own(post("/api/ledgers", {"name": f"Bench books {tag}", "metadata": run}).get("id"))
    made = post("/api/ledger_accounts", {"name": f"Cash {tag}", "normal_balance": "debit", "currency": "USD",
                                         "ledger_id": ledger})
    balances = obj(made.get("balances"))
    j.check("a new ledger account has zero balances",
            j.last["status"] == 201 and eq(made.get("normal_balance"), "debit") and eq(made.get("ledger_id"), ledger)
            and all(amounts(balance(balances, w), 0, 0, 0) and eq(balance(balances, w).get("currency"), "USD")
                    for w in ("pending_balance", "posted_balance", "available_balance")),
            normal_balance=made.get("normal_balance"), balances=balances)
    cash = own(made.get("id"))
    revenue = own(post("/api/ledger_accounts", {"name": f"Revenue {tag}", "normal_balance": "credit",
                                                "currency": "USD", "ledger_id": ledger}).get("id"))
    key = made_guid(j)
    sale = {"description": f"Order {tag}", "status": "pending", "metadata": dict(run, kind="sale"),
            "ledger_entries": [entry(10000, "debit", cash), entry(10000, "credit", revenue)]}
    first = post("/api/ledger_transactions", sale, key)
    j.check("a pending transaction keeps its entries",
            j.last["status"] == 201 and eq(first.get("status"), "pending") and eq(first.get("ledger_id"), ledger)
            and entries(first) == entries(sale) and first.get("metadata") == sale["metadata"],
            transaction_status=first.get("status"), entries=entries(first), metadata=first.get("metadata"))
    t1 = own(first.get("id"))
    replay = post("/api/ledger_transactions", sale, key)
    j.check("a repeated idempotency key answers the same transaction", eq(replay.get("id"), first.get("id")),
            first=first.get("id"), replay=replay.get("id"))
    rows, _ = transactions()
    j.check("a repeated key makes no second transaction", rows == [t1], listed=rows)
    balances = account(cash)
    j.check("pending entries move only the pending balance",
            amounts(balance(balances, "pending_balance"), 10000, 10000, 0)
            and amounts(balance(balances, "posted_balance"), 0, 0, 0),
            pending=balance(balances, "pending_balance"), posted=balance(balances, "posted_balance"))
    balances = account(revenue)
    j.check("a credit-normal account counts credits as positive",
            amounts(balance(balances, "pending_balance"), 10000, 0, 10000),
            pending=balance(balances, "pending_balance"))
    bad = post("/api/ledger_transactions", {"description": f"Off by one {tag}", "metadata": {"run": f"bad-{tag}"},
                                            "ledger_entries": [entry(500, "debit", cash), entry(400, "credit", revenue)]})
    j.check("unbalanced entries are refused",
            j.last["status"] == 422 and eq(obj(bad.get("errors")).get("code"), "parameter_invalid"),
            errors=bad.get("errors"))
    posted = obj(j.call("PATCH", f"/api/ledger_transactions/{t1}", body={"status": "posted"}))
    patched = j.last["status"]
    read = obj(j.call("GET", f"/api/ledger_transactions/{t1}"))
    j.check("posting sets posted_at",
            patched == 200 and eq(posted.get("id"), t1) and eq(posted.get("status"), "posted")
            and instant(posted.get("posted_at")) is not None
            and eq(read.get("status"), "posted")
            and eq(instant(read.get("posted_at")), instant(posted.get("posted_at"))),
            patch_status=patched, transaction_status=posted.get("status"), posted_at=posted.get("posted_at"),
            read_status=read.get("status"), read_posted_at=read.get("posted_at"))
    balances = account(cash)
    j.check("posted entries reach the posted balance",
            amounts(balance(balances, "posted_balance"), 10000, 10000, 0)
            and amounts(balance(balances, "pending_balance"), 10000, 10000, 0),
            posted=balance(balances, "posted_balance"), pending=balance(balances, "pending_balance"))
    j.call("PATCH", f"/api/ledger_transactions/{t1}", body={"status": "archived"})
    j.check("a posted transaction cannot be archived", j.last["status"] == 422)
    refund = post("/api/ledger_transactions", {
        "description": f"Refund {tag}", "status": "posted", "metadata": dict(run, kind="refund"),
        "ledger_entries": [entry(2500, "debit", revenue), entry(2500, "credit", cash)]})
    t2 = own(refund.get("id"))
    balances = account(cash)
    j.check("a posted refund nets the balance", amounts(balance(balances, "posted_balance"), 7500, 10000, 2500),
            posted=balance(balances, "posted_balance"))
    rows, _ = transactions(**{"metadata[kind]": "refund"})
    j.check("metadata filters the transactions", rows == [t2], listed=rows)
    # Newest first: the refund was made after the sale.
    rows, cursor = transactions(per_page=1)
    cursor = given(cursor, None)
    j.check("per_page 1 answers the newest and a cursor header", rows == [t2] and cursor is not None,
            listed=rows, after_cursor=cursor)
    rest, more = transactions(per_page=1, after_cursor=cursor or made_up(j, "", 24))
    j.check("after_cursor reaches the older transaction", rest == [t1] and not more,
            first=rows, listed=rest, after_cursor=more)


XERO_DATE = r"/Date\(-?[0-9]+([+-][0-9]{4})?\)/"


def xero_steps(j):
    # Every call names the organisation in Xero-tenant-id. Lists filter on an account
    # number prefix made for this run, so they hold only the contacts made here.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    j.headers["Xero-tenant-id"] = made_guid(j)
    root = "/api.xro/2.0"
    scope = f'AccountNumber.StartsWith("AT{tag}")'

    def own(value):
        return value if isinstance(value, str) and value else made_guid(j)

    def one(answer):
        return head(obj(answer).get("Contacts"))

    def contact(cid):
        return one(j.call("GET", f"{root}/Contacts/{cid}"))

    def update(cid, **fields):
        return one(j.call("POST", f"{root}/Contacts/{cid}", body={"Contacts": [dict({"ContactID": cid}, **fields)]}))

    def listed(**query):
        where = query.pop("where", scope)
        answer = obj(j.call("GET", f"{root}/Contacts", query=dict({"where": where}, **query)))
        return ids_of(answer.get("Contacts"), "ContactID"), answer

    def members(gid):
        group = head(obj(j.call("GET", f"{root}/ContactGroups/{gid}")).get("ContactGroups"))
        return ids_of(group.get("Contacts"), "ContactID")

    bikes = {"Name": f"Atlas Bikes {tag}", "FirstName": "Ana", "LastName": "Silva",
             "EmailAddress": f"ana.{tag}@atlasbikes.example", "AccountNumber": f"AT{tag}-1",
             "Phones": [{"PhoneType": "DEFAULT", "PhoneNumber": "555 0101", "PhoneAreaCode": "09",
                         "PhoneCountryCode": "64"}],
             "Addresses": [{"AddressType": "STREET", "AddressLine1": "12 Queen Street", "City": "Auckland",
                            "PostalCode": "1010", "Country": "New Zealand"}]}
    names = ("Name", "FirstName", "LastName", "EmailAddress", "AccountNumber")
    answer = obj(j.call("PUT", f"{root}/Contacts", body={"Contacts": [bikes]}))
    made = one(answer)
    j.check("a created contact keeps its fields",
            j.last["status"] == 200 and same(made, bikes, *names) and eq(made.get("ContactStatus"), "ACTIVE")
            and given(made.get("ContactID"), None) is not None,
            contact={f: made.get(f) for f in names + ("ContactID", "ContactStatus")})
    j.check("dates are .NET JSON dates",
            matches(XERO_DATE, made.get("UpdatedDateUTC")) and matches(XERO_DATE, answer.get("DateTimeUTC")),
            UpdatedDateUTC=made.get("UpdatedDateUTC"), DateTimeUTC=answer.get("DateTimeUTC"))
    c1 = own(made.get("ContactID"))
    read = contact(c1)
    j.check("read by ContactID matches the created contact",
            eq(read.get("ContactID"), c1) and same(read, bikes, *names)
            and eq(head(read.get("Addresses")).get("City"), "Auckland"),
            ContactID=read.get("ContactID"), Name=read.get("Name"), Addresses=read.get("Addresses"))
    refused = obj(j.call("PUT", f"{root}/Contacts",
                         body={"Contacts": [{"Name": bikes["Name"], "AccountNumber": f"DUP{tag}"}]}))
    errors = [e for el in dicts(refused.get("Elements")) for e in dicts(el.get("ValidationErrors"))]
    j.check("a taken name is refused as a ValidationException",
            j.last["status"] == 400 and eq(refused.get("ErrorNumber"), 10)
            and eq(refused.get("Type"), "ValidationException")
            and bool(errors) and all(given(e.get("Message"), None) is not None for e in errors),
            ErrorNumber=refused.get("ErrorNumber"), Type=refused.get("Type"), errors=errors)
    email = f"accounts.{tag}@atlasbikes.example"
    update(c1, EmailAddress=email)
    read = contact(c1)
    j.check("an update keeps what it was not sent",
            eq(read.get("EmailAddress"), email)
            and same(read, bikes, "Name", "FirstName", "LastName", "AccountNumber"),
            contact={f: read.get(f) for f in names})
    c2 = own(one(j.call("PUT", f"{root}/Contacts", body={"Contacts": [{
        "Name": f"Atlas Cycles {tag}", "EmailAddress": f"hello.{tag}@atlascycles.example",
        "AccountNumber": f"AT{tag}-2"}]})).get("ContactID"))
    rows, _ = listed(where=f'Name=="Atlas Bikes {tag}"')
    j.check("where matches the contact by name", rows == [c1], listed=rows)
    # By name, descending: "Atlas Cycles" comes before "Atlas Bikes".
    rows, _ = listed(order="Name DESC")
    j.check("order sorts the matching contacts", rows == [c2, c1], listed=rows)
    rows, answer = listed(order="Name DESC", page=2, pageSize=1)
    paging = obj(answer.get("pagination"))
    j.check("page and pageSize page the ordered contacts",
            rows == [c1] and eq(paging.get("page"), 2) and eq(paging.get("itemCount"), 2)
            and eq(paging.get("pageCount"), 2),
            listed=rows, pagination=paging)
    archived = update(c2, ContactStatus="ARCHIVED")
    rows, _ = listed(order="Name ASC")
    j.check("an archived contact leaves the list", eq(archived.get("ContactStatus"), "ARCHIVED") and rows == [c1],
            ContactStatus=archived.get("ContactStatus"), listed=rows)
    rows, answer = listed(order="Name ASC", includeArchived="true")
    states = {c.get("ContactID"): c.get("ContactStatus") for c in dicts(answer.get("Contacts"))}
    j.check("includeArchived lists the archived contact", rows == [c1, c2] and eq(states.get(c2), "ARCHIVED"),
            listed=rows, states=list(states.values()))
    update(c2, ContactStatus="ACTIVE")
    state = contact(c2).get("ContactStatus")
    rows, _ = listed(order="Name ASC")
    j.check("restoring makes the contact active again", eq(state, "ACTIVE") and rows == [c1, c2],
            ContactStatus=state, listed=rows)
    group = head(obj(j.call("PUT", f"{root}/ContactGroups",
                            body={"ContactGroups": [{"Name": f"Wholesale {tag}"}]})).get("ContactGroups"))
    gid = own(group.get("ContactGroupID"))
    j.call("PUT", f"{root}/ContactGroups/{gid}/Contacts", body={"Contacts": [{"ContactID": c1}, {"ContactID": c2}]})
    held = members(gid)
    j.check("a contact group holds the contacts added to it", same_items(held, [c1, c2]), members=held)
    j.call("DELETE", f"{root}/ContactGroups/{gid}/Contacts/{c2}")
    removed = j.last["status"]
    held = members(gid)
    j.check("a contact removed from a group leaves it", held == [c1], delete_status=removed, members=held)
    with sent_headers(j, {"Xero-tenant-id": made_guid(j)}):
        rows, answer = listed(includeArchived="true")
    j.check("another organisation does not see the contacts",
            j.last["status"] == 200 and isinstance(answer.get("Contacts"), list) and rows == [],
            listed=rows)


BLIND9_JOURNEYS = {"resourcemanager": resourcemanager_steps, "directus": directus_steps,
                   "cloudinary": cloudinary_steps, "moderntreasury": moderntreasury_steps, "xero": xero_steps}


def run_journeys_blind9(srv, sink, args):
    run_journey_table(BLIND9_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- tenth blind journeys

# The same rule again, over five more contracts written for this suite from each real
# API's public documentation (scripts/llm-bench/contracts/blind10/). They were picked for
# API styles: SCIM 2.0 provisioning with PatchOp operations, filtered member paths,
# 1-based `startIndex` pages and `attributes` (Databricks), etags read back in
# `If-None-Match` (304) and `If-Match` (412), sync tokens that answer only the changes
# since, deleted events as `cancelled`, and time windows (Google Calendar), natural keys
# in the path (an app by id or name, a release by version), `Range` header paging with
# 206 and `Next-Range`, config vars removed by null, releases and a rollback (Heroku),
# namespaced objects named by namespace and name, label selectors, `limit`/`continue`
# pages, `resourceVersion` conflicts, JSON merge patches and immutable objects
# (Kubernetes), and a required `From` header, wrapped bodies, routing through an
# escalation policy, de-duplication keys, a status machine, bracketed array filters,
# `offset`/`limit`/`more`/`total` and `include[]` side-loading (PagerDuty). No check gates
# the rest: a journey goes on with an id of its own when a call gave none. A check
# asserts only what its contract promises a client. Do not edit a check to suit results.
BLIND10_CHECKS = {
    "databricks": [
        "a created user keeps its userName and is active", "read by id matches the created user",
        "a taken userName is refused for uniqueness, ignoring case", "an eq filter finds the user",
        "a sorted page answers one user and the total", "startIndex 2 reaches the next user",
        "attributes answers only the attributes asked for", "a replace patch changes only its path",
        "a deactivated user lists under active eq false", "a group is created with its members",
        "a taken group name is refused for uniqueness", "an add patch adds a member and keeps the others",
        "a filtered remove takes out only that member", "a deleted user is not found and leaves its group",
    ],
    "googlecalendar": [
        "a created calendar keeps its summary and time zone", "an event keeps its id, times and summary",
        "a taken event id is refused as duplicate", "read matches the created event",
        "If-None-Match with the current etag answers 304", "a patch changes only the fields sent and the etag",
        "If-None-Match with an old etag answers the changed event", "a stale If-Match is refused with 412",
        "timeMin and timeMax keep the events in the window", "q finds the event by its description",
        "maxResults 1 by start time answers the first event",
        "the page token reaches the next event by start time",
        "a deleted event is listed only with showDeleted, as cancelled",
        "a sync token answers only the changes since",
    ],
    "heroku": [
        "a created app keeps its name, region and stack", "the app reads the same by id and by name",
        "a taken name is refused as invalid_params", "config vars set the vars sent",
        "null removes a var and keeps the rest", "each config change adds the current release",
        "a release reads by its version", "a rollback restores that release's config vars",
        "Range max 1 answers 206 and one app", "Next-Range reaches the next app",
        "order desc reverses the range", "a custom domain is added and listed",
        "a renamed app keeps its id and frees the old name", "a deleted app is not found by id or name",
    ],
    "kubernetes": [
        "a namespace is created active", "a created config map keeps its data and labels",
        "read by namespace and name matches the created config map", "a taken name is refused as AlreadyExists",
        "the same name is free in another namespace", "generateName names the config map from the prefix",
        "an equality selector keeps the namespace's matches", "a set selector keeps the matching tiers",
        "limit answers a page of three", "continue reaches the rest of the list",
        "a put with the read resourceVersion replaces the data", "a stale resourceVersion is refused as a Conflict",
        "a merge patch sets, removes and keeps keys", "an immutable config map refuses a data change",
    ],
    "pagerduty": [
        "a created user keeps its name, email and role", "an escalation policy targets the user",
        "a service names its escalation policy", "a new incident is triggered on its service",
        "the incident is assigned to the policy's first target", "an open incident's key is refused",
        "a second incident is numbered after the first", "acknowledging records the acknowledger",
        "a note keeps its content and the From user", "statuses[] filters the service's incidents",
        "limit 1 answers one incident, more and the total", "offset 1 reaches the other incident",
        "include services side-loads the service", "a resolved incident frees its key",
    ],
}
JOURNEY_CHECKS.update(BLIND10_CHECKS)
BLIND10_DIR = "scripts/llm-bench/contracts/blind10"
# Contracts alone: no persona.
BLIND10_MOCKS = {name: (f"{BLIND10_DIR}/{name}.openapi.yaml", f"/{name}", None) for name in BLIND10_CHECKS}
SCIM_USER = "urn:ietf:params:scim:schemas:core:2.0:User"
SCIM_GROUP = "urn:ietf:params:scim:schemas:core:2.0:Group"
SCIM_PATCH = "urn:ietf:params:scim:api:messages:2.0:PatchOp"
SCIM_ERROR = "urn:ietf:params:scim:api:messages:2.0:Error"


def databricks_steps(j):
    # The users' names hold a tag made for this run, and every list filters on it, so the
    # lists hold only the users made here.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    root = "/api/2.0/preview/scim/v2"
    scope = f".{tag}@bench.example"
    ana, ben = f"ana{scope}", f"ben{scope}"
    team = f"Data Eng {tag}"

    def own(value):
        return value if isinstance(value, str) and value else str(j.rng.randrange(10**15, 10**16))

    def user(uid, **query):
        return obj(j.call("GET", f"{root}/Users/{uid}", query=query or None))

    def users(**query):
        page = obj(j.call("GET", f"{root}/Users", query=query))
        return ids_of(page.get("Resources")), page

    def group(gid):
        return obj(j.call("GET", f"{root}/Groups/{gid}"))

    def patch(kind, rid, *operations):
        j.call("PATCH", f"{root}/{kind}/{rid}", body={"schemas": [SCIM_PATCH], "Operations": list(operations)})
        return j.last["status"]

    def values(items):
        return [m.get("value") for m in dicts(items)]

    def refused(answer, status):
        answer = obj(answer)
        return (j.last["status"] == status and SCIM_ERROR in (answer.get("schemas") or [])
                and eq(str(answer.get("status")), str(status)))

    new = {"schemas": [SCIM_USER], "userName": ana, "displayName": "Ana Silva",
           "name": {"givenName": "Ana", "familyName": "Silva"},
           "emails": [{"value": ana, "type": "work", "primary": True}]}
    made = obj(j.call("POST", f"{root}/Users", body=new))
    j.check("a created user keeps its userName and is active",
            j.last["status"] == 201 and eq(made.get("userName"), ana) and eq(made.get("displayName"), "Ana Silva")
            and made.get("active") is True and given(made.get("id"), None) is not None
            and SCIM_USER in (made.get("schemas") or []),
            id=made.get("id"), userName=made.get("userName"), active=made.get("active"), schemas=made.get("schemas"))
    u1 = own(made.get("id"))
    read = user(u1)
    j.check("read by id matches the created user",
            eq(read.get("id"), u1) and eq(read.get("userName"), ana) and eq(read.get("displayName"), "Ana Silva")
            and eq(obj(read.get("name")).get("givenName"), "Ana")
            and eq(obj(read.get("name")).get("familyName"), "Silva"),
            id=read.get("id"), userName=read.get("userName"), user_name=read.get("name"))
    taken = j.call("POST", f"{root}/Users", body={"schemas": [SCIM_USER], "userName": ana.upper(),
                                                  "displayName": "Ana again"})
    j.check("a taken userName is refused for uniqueness, ignoring case",
            refused(taken, 409) and eq(obj(taken).get("scimType"), "uniqueness"), error=taken)
    u2 = own(obj(j.call("POST", f"{root}/Users", body={
        "schemas": [SCIM_USER], "userName": ben, "displayName": "Ben Okafor",
        "name": {"givenName": "Ben", "familyName": "Okafor"}})).get("id"))
    rows, page = users(filter=f'userName eq "{ana}"')
    j.check("an eq filter finds the user", rows == [u1] and eq(page.get("totalResults"), 1),
            listed=rows, totalResults=page.get("totalResults"))
    # Sorted by userName: "ana..." comes before "ben...".
    paged = {"filter": f'userName co "{scope}"', "sortBy": "userName", "count": 1}
    rows, page = users(startIndex=1, **paged)
    j.check("a sorted page answers one user and the total",
            rows == [u1] and eq(page.get("totalResults"), 2) and eq(page.get("itemsPerPage"), 1)
            and eq(page.get("startIndex"), 1),
            listed=rows, totalResults=page.get("totalResults"), itemsPerPage=page.get("itemsPerPage"),
            startIndex=page.get("startIndex"))
    rows, page = users(startIndex=2, **paged)
    j.check("startIndex 2 reaches the next user",
            rows == [u2] and eq(page.get("startIndex"), 2) and eq(page.get("totalResults"), 2),
            listed=rows, startIndex=page.get("startIndex"), totalResults=page.get("totalResults"))
    part = user(u1, attributes="userName")
    j.check("attributes answers only the attributes asked for",
            eq(part.get("id"), u1) and eq(part.get("userName"), ana) and set(part) <= {"id", "schemas", "userName"},
            answer=part)
    patched = patch("Users", u1, {"op": "replace", "path": "displayName", "value": "Ana Costa"})
    read = user(u1)
    j.check("a replace patch changes only its path",
            eq(read.get("displayName"), "Ana Costa") and eq(read.get("userName"), ana)
            and eq(obj(read.get("name")).get("givenName"), "Ana")
            and eq(obj(read.get("name")).get("familyName"), "Silva") and read.get("active") is True,
            patch_status=patched, displayName=read.get("displayName"), user_name=read.get("name"))
    patch("Users", u2, {"op": "replace", "path": "active", "value": False})
    state = user(u2).get("active")
    rows, _ = users(filter=f'active eq false and userName co "{scope}"')
    j.check("a deactivated user lists under active eq false", state is False and rows == [u2],
            active=state, listed=rows)
    made = obj(j.call("POST", f"{root}/Groups", body={"schemas": [SCIM_GROUP], "displayName": team,
                                                      "members": [{"value": u1}]}))
    j.check("a group is created with its members",
            j.last["status"] == 201 and eq(made.get("displayName"), team) and values(made.get("members")) == [u1]
            and given(made.get("id"), None) is not None,
            id=made.get("id"), displayName=made.get("displayName"), members=values(made.get("members")))
    gid = own(made.get("id"))
    taken = j.call("POST", f"{root}/Groups", body={"schemas": [SCIM_GROUP], "displayName": team})
    j.check("a taken group name is refused for uniqueness",
            refused(taken, 409) and eq(obj(taken).get("scimType"), "uniqueness"), error=taken)
    patch("Groups", gid, {"op": "add", "path": "members", "value": [{"value": u2}]})
    held = values(group(gid).get("members"))
    joined = values(user(u2).get("groups"))
    j.check("an add patch adds a member and keeps the others", same_items(held, [u1, u2]) and gid in joined,
            members=held, user_groups=joined)
    patch("Groups", gid, {"op": "remove", "path": f'members[value eq "{u1}"]'})
    held = values(group(gid).get("members"))
    left = user(u1)
    j.check("a filtered remove takes out only that member",
            held == [u2] and eq(left.get("id"), u1) and gid not in values(left.get("groups")),
            members=held, user_groups=values(left.get("groups")))
    j.call("DELETE", f"{root}/Users/{u2}")
    deleted = j.last["status"]
    gone = user(u2)
    missing = refused(gone, 404)
    after = group(gid)
    j.check("a deleted user is not found and leaves its group",
            missing and eq(after.get("id"), gid) and values(after.get("members")) == [],
            delete_status=deleted, read=gone, members=values(after.get("members")))


BASE32HEX = "abcdefghijklmnopqrstuv0123456789"


def googlecalendar_steps(j):
    # The calendar is new, so its events are the three made here. Event ids are made by
    # the client, in base32hex.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    created = obj(j.call("POST", "/calendar/v3/calendars", body={"summary": f"Team {tag}",
                                                                 "timeZone": "Europe/Zurich"}))
    j.check("a created calendar keeps its summary and time zone",
            j.last["status"] == 200 and eq(created.get("summary"), f"Team {tag}")
            and eq(created.get("timeZone"), "Europe/Zurich") and given(created.get("id"), None) is not None,
            id=created.get("id"), summary=created.get("summary"), timeZone=created.get("timeZone"))
    cal = given(created.get("id"), f"c_{made_up(j, '', 16, LOWER_ALNUM)}@group.calendar.google.com")
    events = f"/calendar/v3/calendars/{urllib.parse.quote(cal, safe='@')}/events"
    a, b, c = (made_up(j, "", 20, BASE32HEX) for _ in range(3))

    def at(text):
        return {"dateTime": text}

    def when(slot):
        return instant(obj(slot).get("dateTime"))

    def listed(**query):
        page = obj(j.call("GET", events, query=query or None))
        return ids_of(page.get("items")), page

    def reason(answer):
        return head(obj(obj(answer).get("error")).get("errors")).get("reason")

    planning = {"id": a, "summary": "Sprint planning", "location": "Room 4", "description": f"Plan sprint {tag}",
                "start": at("2026-12-01T09:00:00Z"), "end": at("2026-12-01T10:00:00Z")}
    made = obj(j.call("POST", events, body=planning))
    j.check("an event keeps its id, times and summary",
            j.last["status"] == 200 and eq(made.get("id"), a) and eq(made.get("summary"), "Sprint planning")
            and eq(when(made.get("start")), instant("2026-12-01T09:00:00Z"))
            and eq(when(made.get("end")), instant("2026-12-01T10:00:00Z"))
            and eq(made.get("status"), "confirmed") and given(made.get("etag"), None) is not None,
            id=made.get("id"), start=made.get("start"), end=made.get("end"), event_status=made.get("status"),
            etag=made.get("etag"))
    taken = j.call("POST", events, body=dict(planning, summary="Another planning"))
    j.check("a taken event id is refused as duplicate", j.last["status"] == 409 and eq(reason(taken), "duplicate"),
            reason=reason(taken))
    j.call("POST", events, body={"id": b, "summary": "Design review", "description": "Review the dashboard mockups",
                                 "start": at("2026-12-02T14:00:00Z"), "end": at("2026-12-02T15:00:00Z")})
    j.call("POST", events, body={"id": c, "summary": "Retro", "start": at("2026-12-05T11:00:00Z"),
                                 "end": at("2026-12-05T11:30:00Z")})
    read = obj(j.call("GET", f"{events}/{a}"))
    first = given(read.get("etag"), None)
    j.check("read matches the created event",
            eq(read.get("id"), a) and eq(read.get("summary"), "Sprint planning") and eq(read.get("location"), "Room 4")
            and first is not None and (eq(first, made.get("etag")) if given(made.get("etag"), None) else True),
            id=read.get("id"), etag=first, created_etag=made.get("etag"))
    old = first or made_up(j, '"', 16, "0123456789") + '"'
    with sent_headers(j, {"If-None-Match": old}):
        j.call("GET", f"{events}/{a}")
    j.check("If-None-Match with the current etag answers 304", j.last["status"] == 304)
    # The sync token comes from a full list made before any change.
    _, page = listed()
    for _ in range(3):
        if given(page.get("nextSyncToken"), None) or not given(page.get("nextPageToken"), None):
            break
        _, page = listed(pageToken=page.get("nextPageToken"))
    sync = given(page.get("nextSyncToken"), made_up(j, "", 24))
    moved = obj(j.call("PATCH", f"{events}/{a}", body={"summary": "Sprint planning (moved)"}))
    j.check("a patch changes only the fields sent and the etag",
            eq(moved.get("summary"), "Sprint planning (moved)") and eq(moved.get("location"), "Room 4")
            and eq(moved.get("description"), f"Plan sprint {tag}")
            and given(moved.get("etag"), None) is not None and moved.get("etag") != old,
            summary=moved.get("summary"), location=moved.get("location"), etag=moved.get("etag"), old_etag=old)
    with sent_headers(j, {"If-None-Match": old}):
        fresh = obj(j.call("GET", f"{events}/{a}"))
    j.check("If-None-Match with an old etag answers the changed event",
            j.last["status"] == 200 and eq(fresh.get("summary"), "Sprint planning (moved)")
            and given(fresh.get("etag"), None) is not None and fresh.get("etag") != old,
            summary=fresh.get("summary"), etag=fresh.get("etag"))
    with sent_headers(j, {"If-Match": old}):
        stale = j.call("PATCH", f"{events}/{a}", body={"summary": "Stale planning"})
    refused = j.last["status"]
    kept = obj(j.call("GET", f"{events}/{a}")).get("summary")
    j.check("a stale If-Match is refused with 412",
            refused == 412 and eq(reason(stale), "conditionNotMet") and eq(kept, "Sprint planning (moved)"),
            patch_status=refused, reason=reason(stale), summary=kept)
    # The planning ends before the window opens and the retro starts after it closes.
    rows, _ = listed(timeMin="2026-12-01T12:00:00Z", timeMax="2026-12-04T00:00:00Z")
    j.check("timeMin and timeMax keep the events in the window", rows == [b], listed=rows)
    rows, _ = listed(q="mockups")
    j.check("q finds the event by its description", rows == [b], listed=rows)
    ordered = {"singleEvents": "true", "orderBy": "startTime", "maxResults": 1}
    rows, page = listed(**ordered)
    token = given(page.get("nextPageToken"), None)
    # One check judges the page and the next judges the token, so a page without a token
    # is one miss. With no token there is nothing to follow, and no made-up token is sent.
    j.check("maxResults 1 by start time answers the first event", rows == [a], listed=rows)
    if token is not None:
        rest, _ = listed(pageToken=token, **ordered)
        j.check("the page token reaches the next event by start time", rest == [b], first=rows, listed=rest)
    else:
        j.check("the page token reaches the next event by start time", False, first=rows,
                next_page_token=page.get("nextPageToken"), reason="the first page gave no nextPageToken")
    j.call("DELETE", f"{events}/{c}")
    deleted = j.last["status"]
    rows, _ = listed()
    shown, page = listed(showDeleted="true")
    state = by_id(page.get("items"), c).get("status")
    j.check("a deleted event is listed only with showDeleted, as cancelled",
            same_items(rows, [a, b]) and same_items(shown, [a, b, c]) and eq(state, "cancelled"),
            delete_status=deleted, listed=rows, with_deleted=shown, deleted_status=state)
    rows, page = listed(syncToken=sync)
    changes = page.get("items")
    j.check("a sync token answers only the changes since",
            same_items(rows, [a, c]) and eq(by_id(changes, a).get("summary"), "Sprint planning (moved)")
            and eq(by_id(changes, c).get("status"), "cancelled")
            and given(page.get("nextSyncToken"), None) is not None,
            listed=rows, next_sync_token=page.get("nextSyncToken"))


def heroku_steps(j):
    # App names hold a tag made for this run, and app lists are ranged over those names,
    # so they hold only the apps made here.
    j.headers.update({"accept": "application/vnd.heroku+json; version=3",
                      "authorization": "Bearer bench-placeholder"})
    tag = j.rng.randrange(10**5, 10**6)
    alpha, beta, gamma, delta = (f"bench-{tag}-{n}" for n in ("alpha", "beta", "gamma", "delta"))

    def own(value):
        return value if isinstance(value, str) and value else made_guid(j)

    def rows(answer):
        return dicts(answer) if isinstance(answer, list) else []

    def ranged(path, value):
        with sent_headers(j, {"Range": value}):
            answer = j.call("GET", path)
        return rows(answer), j.answer_headers.get("Next-Range")

    def config():
        return obj(j.call("GET", f"/apps/{app}/config-vars"))

    def releases():
        found, _ = ranged(f"/apps/{app}/releases", "version ..; max=1000")
        return found

    def versions(found):
        return [r.get("version") for r in found]

    def not_found(answer):
        return j.last["status"] == 404 and eq(obj(answer).get("id"), "not_found")

    made = obj(j.call("POST", "/apps", body={"name": alpha, "region": "eu", "stack": "heroku-24"}))
    j.check("a created app keeps its name, region and stack",
            j.last["status"] == 201 and eq(made.get("name"), alpha) and eq(obj(made.get("region")).get("name"), "eu")
            and eq(obj(made.get("stack")).get("name"), "heroku-24") and given(made.get("id"), None) is not None,
            id=made.get("id"), app_name=made.get("name"), region=made.get("region"), stack=made.get("stack"))
    app = own(made.get("id"))
    by_id_ = obj(j.call("GET", f"/apps/{app}"))
    by_name = obj(j.call("GET", f"/apps/{alpha}"))
    j.check("the app reads the same by id and by name",
            eq(by_id_.get("id"), app) and eq(by_name.get("id"), app)
            and eq(by_id_.get("name"), alpha) and eq(by_name.get("name"), alpha),
            by_id=[by_id_.get("id"), by_id_.get("name")], by_name=[by_name.get("id"), by_name.get("name")])
    taken = obj(j.call("POST", "/apps", body={"name": alpha}))
    j.check("a taken name is refused as invalid_params",
            j.last["status"] == 422 and eq(taken.get("id"), "invalid_params"), error=taken)
    before = releases()
    latest = max([v for v in versions(before) if is_int(v)], default=0)
    first = obj(j.call("PATCH", f"/apps/{app}/config-vars", body={"DATABASE_POOL": "5", "LOG_LEVEL": "info"}))
    read = config()
    j.check("config vars set the vars sent",
            eq(first.get("DATABASE_POOL"), "5") and eq(first.get("LOG_LEVEL"), "info") and read == first,
            answered=first, read=read)
    second = obj(j.call("PATCH", f"/apps/{app}/config-vars", body={"LOG_LEVEL": None, "FEATURE_X": "on"}))
    read = config()
    j.check("null removes a var and keeps the rest",
            "LOG_LEVEL" not in second and eq(second.get("DATABASE_POOL"), "5") and eq(second.get("FEATURE_X"), "on")
            and read == second,
            answered=second, read=read)
    after = releases()
    newest = after[-2:] if len(after) >= 2 else []
    j.check("each config change adds the current release",
            versions(after) == versions(before) + [latest + 1, latest + 2]
            and [r.get("current") for r in newest] == [False, True]
            and sum(1 for r in after if r.get("current") is True) == 1,
            before=versions(before), after=versions(after), current=[r.get("current") for r in after])
    set_release = by_key(after, "version", latest + 1)
    one = obj(j.call("GET", f"/apps/{app}/releases/{latest + 1}"))
    j.check("a release reads by its version",
            eq(one.get("version"), latest + 1) and given(one.get("id"), None) is not None
            and (eq(one.get("id"), set_release.get("id")) if set_release else True),
            version=one.get("version"), id=one.get("id"), listed_id=set_release.get("id"))
    target = own(given(set_release.get("id"), one.get("id")))
    back = obj(j.call("POST", f"/apps/{app}/releases", body={"release": target}))
    rolled = j.last["status"]
    read = config()
    j.check("a rollback restores that release's config vars",
            rolled == 201 and eq(back.get("version"), latest + 3) and back.get("current") is True
            and eq(back.get("description"), f"Rollback to v{latest + 1}") and bool(first) and read == first,
            status_code=rolled, version=back.get("version"), description=back.get("description"), config=read)
    ids = {alpha: app}
    for name in (beta, gamma):
        ids[name] = own(obj(j.call("POST", "/apps", body={"name": name, "region": "us"})).get("id"))
    # Sorted by name: alpha, beta, gamma. The ascending range is bounded by this run's
    # names; the descending one is not, and is judged on this run's names only.
    found, next_range = ranged("/apps", f"name {alpha}..{gamma}; max=1;")
    page_status = j.last["status"]
    first_names = [a.get("name") for a in found]
    # One check judges the page and the next judges Next-Range, so a page without one is
    # one miss. With no Next-Range there is nothing to follow, and no made-up range is sent.
    j.check("Range max 1 answers 206 and one app", page_status == 206 and first_names == [alpha],
            status_code=page_status, listed=first_names)
    if given(next_range, None) is not None:
        more, _ = ranged("/apps", next_range)
        j.check("Next-Range reaches the next app", [a.get("name") for a in more] == [beta],
                first=first_names, listed=[a.get("name") for a in more], next_range=next_range)
    else:
        j.check("Next-Range reaches the next app", False, first=first_names, next_range=next_range,
                reason="the first page gave no Next-Range header")
    found, _ = ranged("/apps", "name ..; order=desc,max=10;")
    mine = [a.get("name") for a in found if a.get("name") in (alpha, beta, gamma)]
    j.check("order desc reverses the range", j.last["status"] == 200 and mine == [gamma, beta, alpha],
            listed=[a.get("name") for a in found])
    host = f"www.bench{tag}.example.com"
    domain = obj(j.call("POST", f"/apps/{app}/domains", body={"hostname": host, "sni_endpoint": None}))
    added = j.last["status"]
    listed = rows(j.call("GET", f"/apps/{app}/domains"))
    j.check("a custom domain is added and listed",
            added == 201 and eq(domain.get("hostname"), host) and eq(domain.get("kind"), "custom")
            and given(domain.get("cname"), None) is not None
            and eq(by_key(listed, "hostname", host).get("kind"), "custom"),
            status_code=added, domain={k: domain.get(k) for k in ("hostname", "kind", "cname")},
            listed=[d.get("hostname") for d in listed])
    renamed = obj(j.call("PATCH", f"/apps/{beta}", body={"name": delta}))
    old = j.call("GET", f"/apps/{beta}")
    j.check("a renamed app keeps its id and frees the old name",
            eq(renamed.get("id"), ids[beta]) and eq(renamed.get("name"), delta) and not_found(old),
            id=renamed.get("id"), app_name=renamed.get("name"), old_name=old)
    gone = obj(j.call("DELETE", f"/apps/{gamma}"))
    deleted = j.last["status"]
    by_name = j.call("GET", f"/apps/{gamma}")
    name_gone = not_found(by_name)
    by_id_ = j.call("GET", f"/apps/{ids[gamma]}")
    j.check("a deleted app is not found by id or name",
            deleted == 200 and eq(gone.get("name"), gamma) and name_gone and not_found(by_id_),
            delete_status=deleted, by_name=by_name, by_id=by_id_)


def kubernetes_steps(j):
    # The namespaces are new and named for this run, so their config maps are the ones
    # made here.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    ns, other = f"shop-{tag}", f"lab-{tag}"
    base = "/api/v1/namespaces"

    def meta(thing):
        return obj(obj(thing).get("metadata"))

    def config_map(namespace, name, data, labels, **extra):
        body = dict({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "labels": labels},
                     "data": data}, **extra)
        return obj(j.call("POST", f"{base}/{namespace}/configmaps", body=body))

    def read(name, namespace=ns):
        return obj(j.call("GET", f"{base}/{namespace}/configmaps/{name}"))

    def names(**query):
        page = obj(j.call("GET", f"{base}/{ns}/configmaps", query=query or None))
        return [meta(i).get("name") for i in dicts(page.get("items"))], page

    def failure(answer, status, reason):
        answer = obj(answer)
        return (j.last["status"] == status and eq(answer.get("kind"), "Status") and eq(answer.get("reason"), reason)
                and eq(answer.get("code"), status))

    def merge_patch(name, patch):
        with sent_headers(j, {"content-type": "application/merge-patch+json"}):
            return obj(j.call("PATCH", f"{base}/{ns}/configmaps/{name}", data=json.dumps(patch).encode()))

    made = obj(j.call("POST", base, body={"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns}}))
    j.check("a namespace is created active",
            j.last["status"] == 201 and eq(made.get("kind"), "Namespace") and eq(meta(made).get("name"), ns)
            and eq(obj(made.get("status")).get("phase"), "Active"),
            namespace=meta(made).get("name"), phase=obj(made.get("status")).get("phase"))
    j.call("POST", base, body={"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": other}})
    web = {"LOG_LEVEL": "info", "THEME": "dark"}
    web_labels = {"app": "web", "tier": "frontend"}
    made = config_map(ns, "web-settings", web, web_labels)
    uid = meta(made).get("uid")
    j.check("a created config map keeps its data and labels",
            j.last["status"] == 201 and eq(made.get("kind"), "ConfigMap") and eq(meta(made).get("name"), "web-settings")
            and eq(meta(made).get("namespace"), ns) and meta(made).get("labels") == web_labels
            and made.get("data") == web and given(uid, None) is not None
            and given(meta(made).get("resourceVersion"), None) is not None,
            metadata=meta(made), data=made.get("data"))
    got = read("web-settings")
    j.check("read by namespace and name matches the created config map",
            eq(meta(got).get("name"), "web-settings") and eq(meta(got).get("namespace"), ns)
            and (eq(meta(got).get("uid"), uid) if given(uid, None) else True)
            and got.get("data") == web and meta(got).get("labels") == web_labels,
            metadata=meta(got), data=got.get("data"))
    taken = config_map(ns, "web-settings", {"LOG_LEVEL": "warn"}, web_labels)
    j.check("a taken name is refused as AlreadyExists",
            failure(taken, 409, "AlreadyExists") and eq(obj(taken.get("details")).get("name"), "web-settings"),
            status=taken)
    twin = config_map(other, "web-settings", {"LOG_LEVEL": "debug"}, web_labels)
    j.check("the same name is free in another namespace",
            j.last["status"] == 201 and eq(meta(twin).get("name"), "web-settings")
            and eq(meta(twin).get("namespace"), other) and twin.get("data") == {"LOG_LEVEL": "debug"}
            and given(meta(twin).get("uid"), None) is not None and meta(twin).get("uid") != uid,
            metadata=meta(twin))
    job = obj(j.call("POST", f"{base}/{ns}/configmaps", body={
        "apiVersion": "v1", "kind": "ConfigMap", "metadata": {"generateName": "job-",
                                                              "labels": {"app": "worker", "tier": "cache"}},
        "data": {"QUEUE": "default"}}))
    generated = meta(job).get("name")
    j.check("generateName names the config map from the prefix",
            j.last["status"] == 201 and matches("job-[a-z0-9]{5}", generated) and eq(meta(job).get("namespace"), ns),
            generated=generated)
    # Paging below goes by the name the server gave, whatever the generateName check said.
    generated = given(generated, f"job-{made_up(j, '', 5, LOWER_ALNUM)}")
    config_map(ns, "cache-settings", {"MODE": "lru", "SIZE": "512"}, {"app": "web", "tier": "cache"})
    config_map(ns, "db-settings", {"POOL": "10"}, {"app": "api", "tier": "db"})
    everything = ["web-settings", generated, "cache-settings", "db-settings"]
    # The other namespace's web-settings carries app=web too; it must not show here.
    rows, _ = names(labelSelector="app=web")
    j.check("an equality selector keeps the namespace's matches", same_items(rows, ["web-settings", "cache-settings"]),
            listed=rows)
    rows, _ = names(labelSelector="tier in (cache,db),app!=worker")
    j.check("a set selector keeps the matching tiers", same_items(rows, ["cache-settings", "db-settings"]), listed=rows)
    first, page = names(limit=3)
    token = given(meta(page).get("continue"), None)
    # One check judges the page and the next judges the token, so a page without a token
    # is one miss. With no token there is nothing to follow, and no made-up token is sent.
    j.check("limit answers a page of three",
            len(first) == 3 and set(first) <= set(everything) and eq(meta(page).get("remainingItemCount"), 1),
            listed=first, remainingItemCount=meta(page).get("remainingItemCount"))
    if token is not None:
        rest, page = names(limit=3, **{"continue": token})
        j.check("continue reaches the rest of the list",
                len(rest) == 1 and same_items(first + rest, everything) and not meta(page).get("continue"),
                first=first, listed=rest, continue_token=meta(page).get("continue"))
    else:
        j.check("continue reaches the rest of the list", False, first=first,
                continue_token=meta(page).get("continue"), reason="the first page gave no continue token")
    current = read("web-settings")
    version = given(meta(current).get("resourceVersion"), made_up(j, "", 6, "0123456789"))
    replacement = {"apiVersion": "v1", "kind": "ConfigMap",
                   "metadata": {"name": "web-settings", "labels": web_labels, "resourceVersion": version},
                   "data": {"LOG_LEVEL": "debug"}}
    put = obj(j.call("PUT", f"{base}/{ns}/configmaps/web-settings", body=replacement))
    j.check("a put with the read resourceVersion replaces the data",
            j.last["status"] == 200 and put.get("data") == {"LOG_LEVEL": "debug"}
            and given(meta(put).get("resourceVersion"), None) is not None
            and meta(put).get("resourceVersion") != version,
            data=put.get("data"), resourceVersion=meta(put).get("resourceVersion"), sent=version)
    stale = obj(j.call("PUT", f"{base}/{ns}/configmaps/web-settings",
                       body=dict(replacement, data={"LOG_LEVEL": "error"})))
    conflict = failure(stale, 409, "Conflict")
    kept = read("web-settings").get("data")
    j.check("a stale resourceVersion is refused as a Conflict", conflict and kept == {"LOG_LEVEL": "debug"},
            status=stale, data=kept)
    patched = merge_patch("cache-settings", {"data": {"TTL": "60", "MODE": None}})
    kept = read("cache-settings").get("data")
    j.check("a merge patch sets, removes and keeps keys",
            patched.get("data") == {"SIZE": "512", "TTL": "60"} and kept == {"SIZE": "512", "TTL": "60"},
            answered=patched.get("data"), data=kept)
    config_map(ns, "frozen-settings", {"REGION": "eu"}, {"app": "web", "tier": "frontend"}, immutable=True)
    frozen = merge_patch("frozen-settings", {"data": {"REGION": "us"}})
    invalid = failure(frozen, 422, "Invalid")
    kept = read("frozen-settings").get("data")
    j.check("an immutable config map refuses a data change", invalid and kept == {"REGION": "eu"},
            status=frozen, data=kept)


PAGERDUTY_ID = UPPER + "0123456789"


def pagerduty_steps(j):
    # The service is new, and every incident list filters on it, so the lists hold only
    # the incidents made here.
    tag = j.rng.randrange(10**5, 10**6)
    email = f"oncall.{tag}@bench.example"
    j.headers.update({"authorization": "Token token=bench-placeholder",
                      "accept": "application/vnd.pagerduty+json;version=2"})

    def own(value):
        return value if isinstance(value, str) and value else made_up(j, "P", 6, PAGERDUTY_ID)

    def ref(rid, kind):
        return {"id": rid, "type": kind}

    def incident(answer):
        return obj(obj(answer).get("incident"))

    def trigger(title, key, urgency):
        return incident(j.call("POST", "/incidents", body={"incident": {
            "type": "incident", "title": title, "service": ref(service, "service_reference"), "urgency": urgency,
            "incident_key": key, "body": {"type": "incident_body", "details": f"{title} ({tag})"}}}))

    def move(iid, status):
        return incident(j.call("PUT", f"/incidents/{iid}",
                               body={"incident": {"type": "incident_reference", "status": status}}))

    def listed(*pairs):
        page = obj(j.call("GET", "/incidents", query=[("service_ids[]", service)] + list(pairs)))
        return ids_of(page.get("incidents")), page

    def code(answer):
        return obj(obj(answer).get("error")).get("code")

    made = obj(obj(j.call("POST", "/users", body={"user": {
        "type": "user", "name": "Ana Silva", "email": email, "role": "user", "time_zone": "Europe/Lisbon"}})).get("user"))
    j.check("a created user keeps its name, email and role",
            j.last["status"] == 201 and eq(made.get("name"), "Ana Silva") and eq(made.get("email"), email)
            and eq(made.get("role"), "user") and given(made.get("id"), None) is not None,
            user={k: made.get(k) for k in ("id", "name", "email", "role")})
    user = own(made.get("id"))
    # From names a valid user, so it is sent only once the user exists.
    j.headers["From"] = email
    policy = obj(obj(j.call("POST", "/escalation_policies", body={"escalation_policy": {
        "type": "escalation_policy", "name": f"Bench policy {tag}",
        "escalation_rules": [{"escalation_delay_in_minutes": 30, "targets": [ref(user, "user_reference")]}]}})
                     ).get("escalation_policy"))
    targets = ids_of(head(policy.get("escalation_rules")).get("targets"))
    j.check("an escalation policy targets the user",
            j.last["status"] == 201 and eq(policy.get("name"), f"Bench policy {tag}") and targets == [user],
            policy_name=policy.get("name"), targets=targets)
    pid = own(policy.get("id"))
    svc = obj(obj(j.call("POST", "/services", body={"service": {
        "type": "service", "name": f"Checkout {tag}", "description": "Checkout API",
        "escalation_policy": ref(pid, "escalation_policy_reference")}})).get("service"))
    j.check("a service names its escalation policy",
            j.last["status"] == 201 and eq(svc.get("name"), f"Checkout {tag}")
            and eq(obj(svc.get("escalation_policy")).get("id"), pid) and eq(svc.get("status"), "active"),
            service_name=svc.get("name"), escalation_policy=svc.get("escalation_policy"), service_status=svc.get("status"))
    service = own(svc.get("id"))
    key1, key2 = f"checkout-latency-{tag}", f"webhook-errors-{tag}"
    first = trigger("Checkout latency above 2s", key1, "high")
    j.check("a new incident is triggered on its service",
            j.last["status"] == 201 and eq(first.get("status"), "triggered")
            and eq(first.get("title"), "Checkout latency above 2s") and eq(obj(first.get("service")).get("id"), service)
            and eq(first.get("incident_key"), key1) and eq(first.get("urgency"), "high")
            and is_int(first.get("incident_number")),
            incident={k: first.get(k) for k in ("id", "status", "incident_key", "urgency", "incident_number")},
            service=first.get("service"))
    assignees = [obj(a.get("assignee")).get("id") for a in dicts(first.get("assignments"))]
    j.check("the incident is assigned to the policy's first target", assignees == [user], assignees=assignees)
    i1 = own(first.get("id"))
    again = j.call("POST", "/incidents", body={"incident": {
        "type": "incident", "title": "Checkout latency, again", "service": ref(service, "service_reference"),
        "incident_key": key1}})
    j.check("an open incident's key is refused", j.last["status"] == 400 and eq(code(again), 2001),
            error=obj(again).get("error"))
    second = trigger("Payment webhook errors", key2, "low")
    number = first.get("incident_number")
    j.check("a second incident is numbered after the first",
            j.last["status"] == 201 and is_int(number) and eq(second.get("incident_number"), number + 1)
            and eq(second.get("urgency"), "low"),
            first=number, second=second.get("incident_number"))
    i2 = own(second.get("id"))
    acked = move(i1, "acknowledged")
    by = [obj(a.get("acknowledger")).get("id") for a in dicts(acked.get("acknowledgements"))]
    state = incident(j.call("GET", f"/incidents/{i1}")).get("status")
    j.check("acknowledging records the acknowledger",
            eq(acked.get("status"), "acknowledged") and by == [user] and eq(state, "acknowledged"),
            answered=acked.get("status"), acknowledgers=by, read=state)
    text = f"Rolled back the {tag} deploy."
    note = obj(obj(j.call("POST", f"/incidents/{i1}/notes", body={"note": {"content": text}})).get("note"))
    added = j.last["status"]
    notes = [n.get("content") for n in dicts(obj(j.call("GET", f"/incidents/{i1}/notes")).get("notes"))]
    j.check("a note keeps its content and the From user",
            added == 201 and eq(note.get("content"), text) and eq(obj(note.get("user")).get("id"), user)
            and notes == [text],
            status_code=added, note_user=note.get("user"), notes=notes)
    triggered, _ = listed(("statuses[]", "triggered"))
    acknowledged, _ = listed(("statuses[]", "acknowledged"))
    j.check("statuses[] filters the service's incidents", triggered == [i2] and acknowledged == [i1],
            triggered=triggered, acknowledged=acknowledged)
    # Oldest first: the first incident was made before the second.
    paged = [("sort_by", "created_at:asc"), ("limit", 1), ("total", "true")]
    rows, page = listed(*paged)
    j.check("limit 1 answers one incident, more and the total",
            rows == [i1] and page.get("more") is True and eq(page.get("total"), 2),
            listed=rows, more=page.get("more"), total=page.get("total"))
    rows, page = listed(*paged, ("offset", 1))
    j.check("offset 1 reaches the other incident", rows == [i2] and page.get("more") is False,
            listed=rows, more=page.get("more"))
    full = obj(incident(j.call("GET", f"/incidents/{i1}", query={"include[]": "services"})).get("service"))
    j.check("include services side-loads the service",
            eq(full.get("id"), service) and eq(full.get("type"), "service") and eq(full.get("name"), f"Checkout {tag}"),
            service=full)
    done = move(i1, "resolved")
    reused = trigger("Checkout latency above 2s, later", key1, "high")
    j.check("a resolved incident frees its key",
            eq(done.get("status"), "resolved") and instant(done.get("resolved_at")) is not None
            and j.last["status"] == 201 and eq(reused.get("status"), "triggered")
            and given(reused.get("id"), None) is not None and reused.get("id") != i1,
            resolved=done.get("status"), resolved_at=done.get("resolved_at"), reused=reused.get("id"))


BLIND10_JOURNEYS = {"databricks": databricks_steps, "googlecalendar": googlecalendar_steps, "heroku": heroku_steps,
                    "kubernetes": kubernetes_steps, "pagerduty": pagerduty_steps}


def run_journeys_blind10(srv, sink, args):
    run_journey_table(BLIND10_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- eleventh blind journeys

# The same rule again, over five more contracts written for this suite from each real
# API's public documentation (scripts/llm-bench/contracts/blind11/). They were picked for
# API styles: two API versions in one store, a V3 `{data, meta.pagination}` envelope
# beside bare V2 arrays, decimal strings for money, order totals computed from catalog
# prices, a status machine by `status_id`, archiving as a soft delete with
# `is_deleted`, and 204 for an empty list (BigCommerce), HAL answers under `_embedded`,
# a 201 that names the new id in a `Resource-ID` header, JSON Patch operations that
# answer 204, tags replaced by PUT, a status filter whose default is `active`, a search
# expression in `query`, and `vnd.error` validation errors (Help Scout), a `data`
# envelope, a stream state machine driven by `disable` and `enable` actions, a secret
# key that is reset, playback ids as a nested resource, `limit`/`page` paging and
# `{error: {type, messages}}` (Mux), PascalCase fields, templates named by id or by a
# client alias, a layout filter, `Count`/`Offset` with `TotalCount`, client-chosen
# stream ids, archiving with a purge date, and 422 errors told apart by `ErrorCode`
# (Postmark), and JSON:API documents with kebab-case attributes, a workspace by id or
# by organization and name, `search[name]`, `page[number]`/`page[size]` with
# `meta.pagination`, a lock that refuses a second lock with 409, nested variables whose
# key is unique per category, sensitive values that answer null, and safe delete
# (HCP Terraform). No check gates the rest: a journey goes on with an id of its own when
# a call gave none. A check asserts only what its contract promises a client. Do not
# edit a check to suit results.
BLIND11_CHECKS = {
    "bigcommerce": [
        "a created product keeps its name, sku and price", "a taken product name is refused with 409",
        "a name filter finds the product", "limit 1 answers one product and the pagination",
        "page 2 reaches the other product", "an order of catalog products totals its lines",
        "the order's products are its lines", "an email filter lists the buyer's orders by id",
        "status_id filters the orders", "a status update moves the order and keeps its lines",
        "min_id answers the orders from that id", "an archived order leaves the list and reads as deleted",
        "is_deleted true lists the archived order", "a list with no orders answers 204",
    ],
    "helpscout": [
        "an inbox is listed and reads by its id", "a created conversation answers 201 with its Resource-ID",
        "read matches the created conversation", "the customer thread holds its text",
        "a note is added as the newest thread", "the default list holds the active conversations, newest first",
        "status pending lists only the pending one", "the page counts every conversation with status all",
        "a subject patch answers 204 and changes only the subject",
        "a closed conversation lists under closed, not active", "a tags put replaces the tags",
        "a query finds the conversation by its subject", "a deleted conversation is not found and leaves the list",
        "a conversation without a subject is refused naming the field",
    ],
    "mux": [
        "a created live stream is idle with a key and a public playback id", "read by id matches the created stream",
        "a signed playback id is added to the stream", "a playback id reads by its id",
        "a deleted playback id leaves the stream", "disable makes the stream disabled",
        "status disabled lists the disabled stream", "enable makes the stream idle again",
        "a reset gives the stream a new key", "stream_key finds the stream by its new key only",
        "limit 1 pages answer one stream each", "a patch changes the fields sent and keeps the rest",
        "an unknown latency mode is refused as invalid_parameters", "a deleted live stream is not found",
    ],
    "postmark": [
        "a created template answers its id, alias and layout", "the template reads the same by id and by alias",
        "a taken alias is refused with ErrorCode 1122", "an edit by alias changes the subject and keeps the body",
        "LayoutTemplate lists the templates that use the layout",
        "Count and Offset page the templates with their TotalCount", "a layout in use is not deleted",
        "a deleted template answers ErrorCode 1101 by id and by alias",
        "a created stream keeps its chosen ID and type", "a taken stream ID is refused with ErrorCode 1230",
        "an edit renames the stream and keeps its type",
        "an archived stream answers its purge date and leaves the list",
        "IncludeArchivedStreams lists it archived, purged 45 days later", "unarchive brings the stream back",
    ],
    "terraform": [
        "a created workspace keeps its name and settings", "the workspace reads the same by id and by name",
        "a taken name is refused with 422", "search[name] finds the workspaces by name",
        "page[size] 2 answers two workspaces and the pagination", "page[number] 2 reaches the last workspace",
        "a lock locks the workspace with its reason", "a locked workspace refuses a second lock with 409",
        "an unlock frees the workspace", "a patch changes only the attributes sent",
        "a variable is created on the workspace", "a taken key in the same category is refused with 422",
        "a sensitive variable answers a null value", "a safe-deleted workspace is not found by id or name",
    ],
}
JOURNEY_CHECKS.update(BLIND11_CHECKS)
BLIND11_DIR = "scripts/llm-bench/contracts/blind11"
# Contracts alone: no persona.
BLIND11_MOCKS = {name: (f"{BLIND11_DIR}/{name}.openapi.yaml", f"/{name}", None) for name in BLIND11_CHECKS}


def bigcommerce_steps(j):
    # Product names, skus and the buyer's email hold a tag made for this run, and every
    # list filters on them, so the lists hold only what was made here.
    j.headers.update({"x-auth-token": "bench-placeholder", "accept": "application/json"})
    tag = j.rng.randrange(10**5, 10**6)
    store = "/stores/bench7f3k2q"
    products, orders = f"{store}/v3/catalog/products", f"{store}/v2/orders"
    mug, poster = f"Bench Mug {tag}", f"Bench Poster {tag}"
    buyer = f"buyer.{tag}@bench.example"

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**4, 10**6)

    def data(answer):
        return obj(obj(answer).get("data"))

    def catalog(**query):
        # `id:in` is a form list with explode false: its commas are sent as they are.
        answer = obj(j.call("GET", f"{products}?{urllib.parse.urlencode(query, safe=',:')}"))
        return ids_of(answer.get("data")), obj(obj(answer.get("meta")).get("pagination"))

    def bought(**query):
        answer = j.call("GET", orders, query=dict({"email": buyer}, **query))
        return ids_of(answer if isinstance(answer, list) else [])

    def lines_of(oid):
        answer = j.call("GET", f"{orders}/{oid}/products")
        return dicts(answer if isinstance(answer, list) else [])

    made = data(j.call("POST", products, body={"name": mug, "type": "physical", "weight": 0.4, "price": 12.5,
                                               "sku": f"MUG-{tag}"}))
    j.check("a created product keeps its name, sku and price",
            j.last["status"] == 200 and is_int(made.get("id")) and eq(made.get("name"), mug)
            and eq(made.get("sku"), f"MUG-{tag}") and near(made.get("price"), 12.5, 0.001),
            product={k: made.get(k) for k in ("id", "name", "sku", "price")})
    p1 = own(made.get("id"))
    taken = obj(j.call("POST", products, body={"name": mug, "type": "physical", "weight": 1, "price": 9}))
    j.check("a taken product name is refused with 409", j.last["status"] == 409 and eq(taken.get("status"), 409),
            error=taken)
    p2 = own(data(j.call("POST", products, body={"name": poster, "type": "physical", "weight": 0.2, "price": 20,
                                                 "sku": f"POSTER-{tag}"})).get("id"))
    rows, pages = catalog(name=mug)
    j.check("a name filter finds the product", rows == [p1] and eq(pages.get("total"), 1),
            listed=rows, total=pages.get("total"))
    # Sorted by name: "Bench Mug" comes before "Bench Poster".
    paged = {"id:in": f"{p1},{p2}", "sort": "name", "direction": "asc", "limit": 1}
    rows, pages = catalog(page=1, **paged)
    j.check("limit 1 answers one product and the pagination",
            rows == [p1] and eq(pages.get("total"), 2) and eq(pages.get("count"), 1)
            and eq(pages.get("total_pages"), 2) and eq(pages.get("current_page"), 1),
            listed=rows, pagination=pages)
    rows, pages = catalog(page=2, **paged)
    j.check("page 2 reaches the other product", rows == [p2] and eq(pages.get("current_page"), 2),
            listed=rows, pagination=pages)
    address = {"first_name": "Ana", "last_name": "Silva", "street_1": "12 Rua Augusta", "city": "Lisbon",
               "state": "Lisboa", "zip": "1100-053", "country": "Portugal", "country_iso2": "PT", "email": buyer}
    first = obj(j.call("POST", orders, body={"status_id": 11, "customer_id": 0, "billing_address": address,
                                             "products": [{"product_id": p1, "quantity": 2},
                                                          {"product_id": p2, "quantity": 1}]}))
    j.check("an order of catalog products totals its lines",
            j.last["status"] in (200, 201) and is_int(first.get("id")) and eq(first.get("status_id"), 11)
            and eq(first.get("status"), "Awaiting Fulfillment") and eq(first.get("items_total"), 3)
            and near(decimal_of(first.get("subtotal_ex_tax")), 45.0, 0.005) and first.get("is_deleted") is False,
            order={k: first.get(k) for k in ("id", "status_id", "status", "items_total", "subtotal_ex_tax",
                                             "is_deleted")})
    o1 = own(first.get("id"))
    lines = lines_of(o1)
    mugs = by_key(lines, "product_id", p1)
    j.check("the order's products are its lines",
            len(lines) == 2 and {line.get("product_id"): line.get("quantity") for line in lines} == {p1: 2, p2: 1}
            and near(decimal_of(mugs.get("base_price")), 12.5, 0.005)
            and near(decimal_of(mugs.get("total_ex_tax")), 25.0, 0.005),
            lines=[{k: line.get(k) for k in ("product_id", "quantity", "base_price", "total_ex_tax")}
                   for line in lines])
    second = obj(j.call("POST", orders, body={"status_id": 1, "billing_address": address, "products": [
        {"name": f"Gift wrap {tag}", "quantity": 1, "price_ex_tax": 3, "price_inc_tax": 3.69}]}))
    o2 = own(second.get("id"))
    # The default sort is by id, lowest first.
    rows = bought()
    j.check("an email filter lists the buyer's orders by id", rows == sorted([o1, o2]), listed=rows)
    rows = bought(status_id=1)
    j.check("status_id filters the orders", rows == [o2], listed=rows)
    moved = obj(j.call("PUT", f"{orders}/{o2}", body={"status_id": 5}))
    read = obj(j.call("GET", f"{orders}/{o2}"))
    j.check("a status update moves the order and keeps its lines",
            eq(moved.get("status_id"), 5) and eq(moved.get("status"), "Cancelled") and eq(read.get("status_id"), 5)
            and eq(read.get("items_total"), 1) and near(decimal_of(read.get("subtotal_ex_tax")), 3.0, 0.005),
            answered=[moved.get("status_id"), moved.get("status")],
            read={k: read.get(k) for k in ("status_id", "status", "items_total", "subtotal_ex_tax")})
    later = max(o1, o2)
    rows = bought(min_id=later)
    j.check("min_id answers the orders from that id", rows == [later], listed=rows, min_id=later)
    j.call("DELETE", f"{orders}/{o1}")
    archived = j.last["status"]
    rows = bought()
    kept = obj(j.call("GET", f"{orders}/{o1}"))
    j.check("an archived order leaves the list and reads as deleted",
            archived == 204 and rows == [o2] and eq(kept.get("id"), o1) and kept.get("is_deleted") is True,
            delete_status=archived, listed=rows, is_deleted=kept.get("is_deleted"))
    rows = bought(is_deleted="true")
    j.check("is_deleted true lists the archived order", rows == [o1], listed=rows)
    j.call("GET", orders, query={"email": f"nobody.{tag}@bench.example"})
    j.check("a list with no orders answers 204", j.last["status"] == 204 and not j.raw.strip(),
            body=excerpt(j.raw))


def helpscout_steps(j):
    # Every conversation carries a tag made for this run, and every list filters on it,
    # so the lists hold only the conversations made here.
    j.headers.update(HELDOUT_AUTH)
    tag = j.rng.randrange(10**5, 10**6)
    mine, vip = f"bench-{tag}", f"vip-{tag}"
    email = f"ana.{tag}@bench.example"

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**6, 10**8)

    def created():
        value = str(j.answer_headers.get("Resource-ID") or "").strip()
        return int(value) if value.isdigit() else None

    def conversation(cid):
        return obj(j.call("GET", f"/v2/conversations/{cid}"))

    def listed(**query):
        page = obj(j.call("GET", "/v2/conversations", query=dict({"tag": mine}, **query)))
        return ids_of(obj(page.get("_embedded")).get("conversations")), obj(page.get("page"))

    def tags_of(thing):
        return [t.get("tag") for t in dicts(thing.get("tags"))]

    def patch(cid, path, value):
        j.call("PATCH", f"/v2/conversations/{cid}", body={"op": "replace", "path": path, "value": value})
        return j.last["status"]

    def threads(cid):
        return dicts(obj(obj(j.call("GET", f"/v2/conversations/{cid}/threads")).get("_embedded")).get("threads"))

    page = obj(j.call("GET", "/v2/mailboxes"))
    inbox = head(obj(page.get("_embedded")).get("mailboxes"))
    box = inbox.get("id")
    read = obj(j.call("GET", f"/v2/mailboxes/{box}")) if is_int(box) else {}
    j.check("an inbox is listed and reads by its id",
            is_int(box) and eq(read.get("id"), box) and eq(read.get("name"), inbox.get("name")),
            listed={k: inbox.get(k) for k in ("id", "name")}, read={k: read.get(k) for k in ("id", "name")})
    box = own(box)

    def start(subject, status, text, tags):
        answer = obj(j.call("POST", "/v2/conversations", body={
            "subject": subject, "customer": {"email": email, "firstName": "Ana", "lastName": "Silva"},
            "mailboxId": box, "type": "email", "status": status, "tags": tags,
            "threads": [{"type": "customer", "customer": {"email": email}, "text": text}]}))
        return created(), answer

    subject = f"Refund for order {tag}"
    a, answer = start(subject, "active", f"My order {tag} arrived broken, I would like a refund.", [mine])
    thread_ids = answer.get("threadIds")
    j.check("a created conversation answers 201 with its Resource-ID",
            j.last["status"] == 201 and a is not None and isinstance(thread_ids, list) and len(thread_ids) == 1
            and is_int(thread_ids[0]),
            resource_id=j.answer_headers.get("Resource-ID"), threadIds=thread_ids)
    a = own(a)
    read = conversation(a)
    j.check("read matches the created conversation",
            eq(read.get("id"), a) and eq(read.get("subject"), subject) and eq(read.get("status"), "active")
            and eq(read.get("mailboxId"), box) and eq(obj(read.get("primaryCustomer")).get("email"), email)
            and tags_of(read) == [mine],
            conversation={k: read.get(k) for k in ("id", "subject", "status", "mailboxId")},
            customer=read.get("primaryCustomer"), tags=tags_of(read))
    found = threads(a)
    first_id = thread_ids[0] if isinstance(thread_ids, list) and thread_ids else None
    j.check("the customer thread holds its text",
            len(found) == 1 and eq(head(found).get("type"), "customer")
            and (eq(head(found).get("id"), first_id) if first_id is not None else True)
            and f"order {tag} arrived broken" in str(head(found).get("body")),
            threads=[{k: t.get(k) for k in ("id", "type", "body")} for t in found])
    text = f"Asked the warehouse about order {tag}."
    j.call("POST", f"/v2/conversations/{a}/notes", body={"text": text})
    added, note = j.last["status"], created()
    found = threads(a)
    j.check("a note is added as the newest thread",
            added == 201 and note is not None and len(found) == 2 and eq(head(found).get("type"), "note")
            and eq(head(found).get("id"), note) and text in str(head(found).get("body")),
            status_code=added, resource_id=note, threads=[{k: t.get(k) for k in ("id", "type")} for t in found])
    b, _ = start(f"Invoice copy {tag}", "pending", f"Please send the invoice for order {tag}.", [mine])
    c, _ = start(f"Address change {tag}", "active", f"Please ship order {tag} to my office.", [mine])
    b, c = own(b), own(c)
    # Newest first: c was made after a, and b is pending.
    rows, _ = listed()
    j.check("the default list holds the active conversations, newest first", rows == [c, a], listed=rows)
    rows, _ = listed(status="pending")
    j.check("status pending lists only the pending one", rows == [b], listed=rows)
    rows, counted = listed(status="all")
    j.check("the page counts every conversation with status all",
            same_items(rows, [a, b, c]) and eq(counted.get("totalElements"), 3) and eq(counted.get("number"), 1),
            listed=rows, page=counted)
    renamed = f"Refund for broken order {tag}"
    status = patch(a, "/subject", renamed)
    read = conversation(a)
    j.check("a subject patch answers 204 and changes only the subject",
            status == 204 and eq(read.get("subject"), renamed) and eq(read.get("status"), "active")
            and eq(obj(read.get("primaryCustomer")).get("email"), email) and tags_of(read) == [mine],
            patch_status=status, subject=read.get("subject"), conversation_status=read.get("status"))
    patch(a, "/status", "closed")
    read = conversation(a)
    closed, _ = listed(status="closed")
    active, _ = listed()
    j.check("a closed conversation lists under closed, not active",
            eq(read.get("status"), "closed") and instant(read.get("closedAt")) is not None and closed == [a]
            and active == [c],
            conversation_status=read.get("status"), closedAt=read.get("closedAt"), closed=closed, active=active)
    j.call("PUT", f"/v2/conversations/{c}/tags", body={"tags": [vip]})
    put = j.last["status"]
    read = conversation(c)
    left, _ = listed(status="all")
    page = obj(j.call("GET", "/v2/conversations", query={"tag": vip, "status": "all"}))
    tagged = ids_of(obj(page.get("_embedded")).get("conversations"))
    j.check("a tags put replaces the tags",
            put == 204 and tags_of(read) == [vip] and tagged == [c] and same_items(left, [a, b]),
            put_status=put, tags=tags_of(read), vip=tagged, run_tag=left)
    rows, _ = listed(status="all", query=f'(subject:"Invoice copy {tag}")')
    j.check("a query finds the conversation by its subject", rows == [b], listed=rows)
    j.call("DELETE", f"/v2/conversations/{b}")
    deleted = j.last["status"]
    conversation(b)
    missing = j.last["status"]
    rows, _ = listed(status="all")
    j.check("a deleted conversation is not found and leaves the list",
            deleted == 204 and missing == 404 and rows == [a], delete_status=deleted, read_status=missing,
            listed=rows)
    bad = obj(j.call("POST", "/v2/conversations", body={
        "subject": "", "customer": {"email": email}, "mailboxId": box, "type": "email", "status": "active",
        "threads": [{"type": "customer", "customer": {"email": email}, "text": "No subject here."}]}))
    fields = [e.get("path") for e in dicts(obj(bad.get("_embedded")).get("errors"))]
    j.check("a conversation without a subject is refused naming the field",
            j.last["status"] == 400 and "subject" in fields, fields=fields, error=bad)


def mux_steps(j):
    # Streams carry a passthrough made for this run, and every list is judged on this
    # run's streams only.
    token = base64.b64encode(b"bench-token-id:bench-placeholder").decode()
    j.headers.update({"authorization": f"Basic {token}"})
    tag = j.rng.randrange(10**5, 10**6)
    root = "/video/v1/live-streams"
    main, backup = f"bench-{tag}-main", f"bench-{tag}-backup"

    def own(value):
        return value if isinstance(value, str) and value else made_up(j, "", 44)

    def data(answer):
        return obj(obj(answer).get("data"))

    def stream(sid):
        return data(j.call("GET", f"{root}/{sid}"))

    def listed(**query):
        return ids_of(obj(j.call("GET", root, query=query)).get("data"))

    def policies(thing):
        return [p.get("policy") for p in dicts(thing.get("playback_ids"))]

    made = data(j.call("POST", root, body={"playback_policies": ["public"], "latency_mode": "standard",
                                           "reconnect_window": 60, "passthrough": main, "test": True,
                                           "new_asset_settings": {"playback_policies": ["public"]}}))
    key = given(made.get("stream_key"), None)
    j.check("a created live stream is idle with a key and a public playback id",
            j.last["status"] == 201 and given(made.get("id"), None) is not None and key is not None
            and eq(made.get("status"), "idle") and policies(made) == ["public"] and eq(made.get("passthrough"), main)
            and eq(made.get("latency_mode"), "standard"),
            stream={k: made.get(k) for k in ("id", "status", "passthrough", "latency_mode")},
            playback_ids=made.get("playback_ids"), has_key=key is not None)
    s1 = own(made.get("id"))
    public = given(head(made.get("playback_ids")).get("id"), None)
    read = stream(s1)
    j.check("read by id matches the created stream",
            eq(read.get("id"), s1) and eq(read.get("passthrough"), main)
            and (eq(read.get("stream_key"), key) if key else True)
            and (ids_of(read.get("playback_ids")) == [public] if public else policies(read) == ["public"]),
            stream={k: read.get(k) for k in ("id", "passthrough", "status")}, playback_ids=read.get("playback_ids"))
    added = data(j.call("POST", f"{root}/{s1}/playback-ids", body={"policy": "signed"}))
    status = j.last["status"]
    signed = given(added.get("id"), None)
    now = stream(s1)
    j.check("a signed playback id is added to the stream",
            status == 201 and eq(added.get("policy"), "signed") and signed is not None
            and same_items(policies(now), ["public", "signed"]) and signed in ids_of(now.get("playback_ids")),
            status_code=status, added=added, playback_ids=now.get("playback_ids"))
    signed = own(signed)
    one = data(j.call("GET", f"{root}/{s1}/playback-ids/{signed}"))
    j.check("a playback id reads by its id", eq(one.get("id"), signed) and eq(one.get("policy"), "signed"),
            playback_id=one)
    j.call("DELETE", f"{root}/{s1}/playback-ids/{signed}")
    removed = j.last["status"]
    now = stream(s1)
    j.check("a deleted playback id leaves the stream",
            removed == 204 and signed not in ids_of(now.get("playback_ids")) and policies(now) == ["public"],
            delete_status=removed, playback_ids=now.get("playback_ids"))
    s2 = own(data(j.call("POST", root, body={"playback_policies": ["public"], "passthrough": backup,
                                             "test": True})).get("id"))
    j.call("PUT", f"{root}/{s1}/disable")
    disabled = j.last["status"]
    state = stream(s1).get("status")
    j.check("disable makes the stream disabled", disabled == 200 and eq(state, "disabled"),
            disable_status=disabled, stream_status=state)
    rows = [s for s in listed(status="disabled", limit=100) if s in (s1, s2)]
    j.check("status disabled lists the disabled stream", rows == [s1], listed=rows)
    j.call("PUT", f"{root}/{s1}/enable")
    enabled = j.last["status"]
    state = stream(s1).get("status")
    rows = [s for s in listed(status="idle", limit=100) if s in (s1, s2)]
    j.check("enable makes the stream idle again", enabled == 200 and eq(state, "idle") and same_items(rows, [s1, s2]),
            enable_status=enabled, stream_status=state, idle=rows)
    fresh = data(j.call("POST", f"{root}/{s1}/reset-stream-key"))
    reset = j.last["status"]
    new_key = given(fresh.get("stream_key"), None)
    kept = stream(s1).get("stream_key")
    j.check("a reset gives the stream a new key",
            reset == 201 and eq(fresh.get("id"), s1) and new_key is not None and new_key != key and eq(kept, new_key),
            status_code=reset, changed=new_key is not None and new_key != key, read_matches=eq(kept, new_key))
    by_new = listed(stream_key=new_key) if new_key else None
    by_old = listed(stream_key=key) if key else None
    j.check("stream_key finds the stream by its new key only",
            by_new == [s1] and isinstance(by_old, list) and s1 not in by_old, by_new=by_new, by_old=by_old)
    first, second = listed(limit=1, page=1), listed(limit=1, page=2)
    j.check("limit 1 pages answer one stream each", len(first) == 1 and len(second) == 1 and first != second,
            first=first, second=second)
    patched = data(j.call("PATCH", f"{root}/{s1}", body={"latency_mode": "low", "reconnect_window": 0}))
    read = stream(s1)
    j.check("a patch changes the fields sent and keeps the rest",
            eq(patched.get("latency_mode"), "low") and eq(read.get("latency_mode"), "low")
            and eq(read.get("reconnect_window"), 0) and eq(read.get("passthrough"), main)
            and (eq(read.get("stream_key"), new_key) if new_key else True),
            answered={k: patched.get(k) for k in ("latency_mode", "reconnect_window")},
            read={k: read.get(k) for k in ("latency_mode", "reconnect_window", "passthrough")})
    bad = obj(j.call("POST", root, body={"playback_policies": ["public"], "latency_mode": "instant"}))
    j.check("an unknown latency mode is refused as invalid_parameters",
            j.last["status"] == 400 and eq(obj(bad.get("error")).get("type"), "invalid_parameters"), error=bad)
    j.call("DELETE", f"{root}/{s2}")
    deleted = j.last["status"]
    gone = obj(j.call("GET", f"{root}/{s2}"))
    j.check("a deleted live stream is not found",
            deleted == 204 and j.last["status"] == 404 and eq(obj(gone.get("error")).get("type"), "not_found"),
            delete_status=deleted, error=gone)


def postmark_steps(j):
    # Aliases and the stream ID hold a tag made for this run, and the template lists the
    # checks judge exactly are filtered on this run's layout.
    j.headers.update({"x-postmark-server-token": "bench-placeholder", "accept": "application/json"})
    tag = j.rng.randrange(10**5, 10**6)
    layout, welcome, receipt = f"layout-{tag}", f"welcome-{tag}", f"receipt-{tag}"
    stream_id = f"news-{tag}"

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**6, 10**8)

    def refused(answer, code):
        return j.last["status"] == 422 and eq(obj(answer).get("ErrorCode"), code)

    def template(key):
        return obj(j.call("GET", f"/templates/{key}"))

    def templates(**query):
        answer = obj(j.call("GET", "/templates", query=query))
        return ids_of(answer.get("Templates"), "TemplateId"), answer.get("TotalCount")

    def streams(**query):
        answer = obj(j.call("GET", "/message-streams", query=query or None))
        return {s.get("ID"): s for s in dicts(answer.get("MessageStreams"))}

    lid = own(obj(j.call("POST", "/templates", body={
        "Name": f"Frame {tag}", "Alias": layout, "TemplateType": "Layout",
        "HtmlBody": "<html><body>{{{@content}}}</body></html>", "TextBody": "{{{@content}}}"})).get("TemplateId"))
    html = "<p>Hello {{name}}</p>"
    made = obj(j.call("POST", "/templates", body={
        "Name": f"Welcome {tag}", "Alias": welcome, "Subject": "Welcome, {{name}}", "HtmlBody": html,
        "TextBody": "Hello {{name}}", "LayoutTemplate": layout}))
    j.check("a created template answers its id, alias and layout",
            j.last["status"] == 200 and is_int(made.get("TemplateId")) and eq(made.get("Alias"), welcome)
            and eq(made.get("Name"), f"Welcome {tag}") and made.get("Active") is True
            and eq(made.get("TemplateType"), "Standard") and eq(made.get("LayoutTemplate"), layout),
            template=made)
    tid = own(made.get("TemplateId"))
    by_number, by_alias = template(tid), template(welcome)
    j.check("the template reads the same by id and by alias",
            eq(by_number.get("TemplateId"), tid) and eq(by_alias.get("TemplateId"), tid)
            and eq(by_number.get("Alias"), welcome) and eq(by_alias.get("Alias"), welcome)
            and eq(by_number.get("Subject"), "Welcome, {{name}}") and eq(by_alias.get("Subject"), "Welcome, {{name}}"),
            by_id={k: by_number.get(k) for k in ("TemplateId", "Alias", "Subject")},
            by_alias={k: by_alias.get(k) for k in ("TemplateId", "Alias", "Subject")})
    taken = obj(j.call("POST", "/templates", body={"Name": "Another welcome", "Alias": welcome, "Subject": "Hi",
                                                    "TextBody": "Hi"}))
    j.check("a taken alias is refused with ErrorCode 1122", refused(taken, 1122), error=taken)
    edited = obj(j.call("PUT", f"/templates/{welcome}", body={"Name": f"Welcome {tag}",
                                                               "Subject": "Welcome aboard, {{name}}"}))
    read = template(tid)
    j.check("an edit by alias changes the subject and keeps the body",
            eq(edited.get("TemplateId"), tid) and eq(read.get("Subject"), "Welcome aboard, {{name}}")
            and eq(read.get("HtmlBody"), html) and eq(read.get("LayoutTemplate"), layout)
            and eq(read.get("Alias"), welcome),
            edited=edited, read={k: read.get(k) for k in ("Subject", "HtmlBody", "LayoutTemplate", "Alias")})
    rid = own(obj(j.call("POST", "/templates", body={"Name": f"Receipt {tag}", "Alias": receipt,
                                                     "Subject": "Your receipt", "TextBody": "Total: {{total}}"})
                  ).get("TemplateId"))
    rows, total = templates(Count=50, Offset=0, LayoutTemplate=layout)
    j.check("LayoutTemplate lists the templates that use the layout", rows == [tid] and eq(total, 1),
            listed=rows, TotalCount=total)
    first, total = templates(Count=1, Offset=0)
    second, again = templates(Count=1, Offset=1)
    j.check("Count and Offset page the templates with their TotalCount",
            len(first) == 1 and len(second) == 1 and first != second and is_int(total) and total >= 3
            and eq(again, total),
            first=first, second=second, TotalCount=[total, again])
    busy = obj(j.call("DELETE", f"/templates/{layout}"))
    refused_busy = refused(busy, 1130)
    kept = template(lid)
    j.check("a layout in use is not deleted", refused_busy and eq(kept.get("TemplateId"), lid),
            error=busy, kept=kept.get("TemplateId"))
    gone = obj(j.call("DELETE", f"/templates/{receipt}"))
    removed = j.last["status"] == 200 and eq(gone.get("ErrorCode"), 0)
    by_number = template(rid)
    number_gone = refused(by_number, 1101)
    by_alias = template(receipt)
    j.check("a deleted template answers ErrorCode 1101 by id and by alias",
            removed and number_gone and refused(by_alias, 1101), deleted=gone, by_id=by_number, by_alias=by_alias)
    made = obj(j.call("POST", "/message-streams", body={"ID": stream_id, "Name": f"Newsletter {tag}",
                                                        "MessageStreamType": "Broadcasts",
                                                        "Description": "Monthly product news"}))
    j.check("a created stream keeps its chosen ID and type",
            j.last["status"] == 200 and eq(made.get("ID"), stream_id) and eq(made.get("Name"), f"Newsletter {tag}")
            and eq(made.get("MessageStreamType"), "Broadcasts") and made.get("ArchivedAt") is None
            and is_int(made.get("ServerID")),
            stream=made)
    taken = obj(j.call("POST", "/message-streams", body={"ID": stream_id, "Name": "Another stream",
                                                         "MessageStreamType": "Transactional"}))
    j.check("a taken stream ID is refused with ErrorCode 1230", refused(taken, 1230), error=taken)
    edited = obj(j.call("PATCH", f"/message-streams/{stream_id}", body={"Name": f"Product news {tag}"}))
    read = obj(j.call("GET", f"/message-streams/{stream_id}"))
    j.check("an edit renames the stream and keeps its type",
            eq(edited.get("Name"), f"Product news {tag}") and eq(read.get("Name"), f"Product news {tag}")
            and eq(read.get("MessageStreamType"), "Broadcasts") and eq(read.get("Description"), "Monthly product news"),
            edited={k: edited.get(k) for k in ("ID", "Name")},
            read={k: read.get(k) for k in ("ID", "Name", "MessageStreamType", "Description")})
    archived = obj(j.call("POST", f"/message-streams/{stream_id}/archive"))
    purge = instant(archived.get("ExpectedPurgeDate"))
    listed = streams()
    j.check("an archived stream answers its purge date and leaves the list",
            eq(archived.get("ID"), stream_id) and purge is not None and stream_id not in listed,
            archived=archived, listed=sorted(str(k) for k in listed))
    shown = obj(streams(IncludeArchivedStreams="true").get(stream_id))
    at = instant(shown.get("ArchivedAt"))
    j.check("IncludeArchivedStreams lists it archived, purged 45 days later",
            at is not None and purge is not None and near((purge - at).total_seconds() / 86400, 45, 1.01),
            stream={k: shown.get(k) for k in ("ID", "ArchivedAt", "ExpectedPurgeDate")},
            purge=archived.get("ExpectedPurgeDate"))
    back = obj(j.call("POST", f"/message-streams/{stream_id}/unarchive"))
    listed = streams()
    j.check("unarchive brings the stream back",
            eq(back.get("ID"), stream_id) and "ArchivedAt" in back and back.get("ArchivedAt") is None
            and stream_id in listed and obj(listed.get(stream_id)).get("ArchivedAt") is None,
            unarchived=back, listed=sorted(str(k) for k in listed))


def terraform_steps(j):
    # The organization is new and named for this run, so its workspaces are the ones made
    # here. Bodies are JSON:API documents.
    j.headers.update(dict(HELDOUT_AUTH, **{"content-type": "application/vnd.api+json",
                                          "accept": "application/vnd.api+json"}))
    tag = j.rng.randrange(10**5, 10**6)
    org = f"bench-{tag}"
    base = f"/api/v2/organizations/{org}/workspaces"

    def send(method, path, doc=None, query=None):
        return j.call(method, path, query=query, data=None if doc is None else json.dumps(doc).encode())

    def data(answer):
        return obj(obj(answer).get("data"))

    def attrs(thing):
        return obj(obj(thing).get("attributes"))

    def own(value):
        return value if isinstance(value, str) and value else made_up(j, "ws-", 16)

    def failed(answer, status):
        return j.last["status"] == status and eq(str(head(obj(answer).get("errors")).get("status")), str(status))

    def workspace(name, **attributes):
        return data(send("POST", base, {"data": {"type": "workspaces", "attributes": dict({"name": name},
                                                                                        **attributes)}}))

    def read(wid):
        return data(send("GET", f"/api/v2/workspaces/{wid}"))

    def listed(**query):
        page = obj(send("GET", base, query=query))
        return [attrs(w).get("name") for w in dicts(page.get("data"))], obj(obj(page.get("meta")).get("pagination"))

    def variables(wid):
        return dicts(obj(send("GET", f"/api/v2/workspaces/{wid}/vars")).get("data"))

    def variable(wid, **attributes):
        return obj(send("POST", f"/api/v2/workspaces/{wid}/vars", {"data": {"type": "vars", "attributes": attributes}}))

    send("POST", "/api/v2/organizations", {"data": {"type": "organizations", "attributes": {
        "name": org, "email": f"ops.{tag}@bench.example"}}})
    made = workspace("web-prod", **{"auto-apply": True, "terraform-version": "1.9.5", "execution-mode": "remote",
                                    "description": "Production web"})
    settings = attrs(made)
    j.check("a created workspace keeps its name and settings",
            j.last["status"] == 201 and eq(made.get("type"), "workspaces") and matches("ws-[A-Za-z0-9]{16}", made.get("id"))
            and eq(settings.get("name"), "web-prod") and settings.get("auto-apply") is True
            and eq(settings.get("terraform-version"), "1.9.5") and eq(settings.get("execution-mode"), "remote")
            and settings.get("locked") is False,
            id=made.get("id"), attributes={k: settings.get(k) for k in ("name", "auto-apply", "terraform-version",
                                                                         "execution-mode", "locked")})
    ws = own(made.get("id"))
    by_number = read(ws)
    by_name = data(send("GET", f"{base}/web-prod"))
    j.check("the workspace reads the same by id and by name",
            eq(by_number.get("id"), ws) and eq(by_name.get("id"), ws) and eq(attrs(by_number).get("name"), "web-prod")
            and eq(attrs(by_name).get("name"), "web-prod"),
            by_id=[by_number.get("id"), attrs(by_number).get("name")],
            by_name=[by_name.get("id"), attrs(by_name).get("name")])
    taken = obj(send("POST", base, {"data": {"type": "workspaces", "attributes": {"name": "web-prod"}}}))
    j.check("a taken name is refused with 422", failed(taken, 422), errors=taken.get("errors"))
    staging = own(workspace("web-staging").get("id"))
    workspace("api-prod")
    everything = ["web-prod", "web-staging", "api-prod"]
    rows, _ = listed(**{"search[name]": "web"})
    j.check("search[name] finds the workspaces by name", same_items(rows, ["web-prod", "web-staging"]), listed=rows)
    first, pages = listed(**{"page[size]": 2, "page[number]": 1})
    # One check judges the first page and the next judges page 2, so a short first page
    # is one miss.
    j.check("page[size] 2 answers two workspaces and the pagination",
            len(first) == 2 and set(first) <= set(everything) and eq(pages.get("current-page"), 1)
            and eq(pages.get("next-page"), 2) and eq(pages.get("total-pages"), 2) and eq(pages.get("total-count"), 3),
            listed=first, pagination=pages)
    rest, pages = listed(**{"page[size]": 2, "page[number]": 2})
    j.check("page[number] 2 reaches the last workspace",
            len(rest) == 1 and same_items(first + rest, everything) and eq(pages.get("current-page"), 2)
            and "next-page" in pages and pages.get("next-page") is None,
            first=first, listed=rest, pagination=pages)
    reason = f"Maintenance window {tag}"
    locked = data(send("POST", f"/api/v2/workspaces/{ws}/actions/lock", {"reason": reason}))
    status = j.last["status"]
    now = attrs(read(ws))
    j.check("a lock locks the workspace with its reason",
            status == 200 and attrs(locked).get("locked") is True and now.get("locked") is True
            and eq(now.get("locked-reason"), reason),
            lock_status=status, answered=attrs(locked).get("locked"),
            read={k: now.get(k) for k in ("locked", "locked-reason")})
    again = obj(send("POST", f"/api/v2/workspaces/{ws}/actions/lock", {"reason": "Another lock"}))
    conflict = failed(again, 409)
    now = attrs(read(ws))
    j.check("a locked workspace refuses a second lock with 409",
            conflict and now.get("locked") is True and eq(now.get("locked-reason"), reason),
            errors=again.get("errors"), read={k: now.get(k) for k in ("locked", "locked-reason")})
    unlocked = data(send("POST", f"/api/v2/workspaces/{ws}/actions/unlock"))
    now = attrs(read(ws))
    j.check("an unlock frees the workspace", attrs(unlocked).get("locked") is False and now.get("locked") is False,
            answered=attrs(unlocked).get("locked"), read=now.get("locked"))
    patched = data(send("PATCH", f"/api/v2/workspaces/{ws}", {"data": {"type": "workspaces", "attributes": {
        "terraform-version": "1.10.0"}}}))
    now = attrs(read(ws))
    j.check("a patch changes only the attributes sent",
            eq(attrs(patched).get("terraform-version"), "1.10.0") and eq(now.get("terraform-version"), "1.10.0")
            and now.get("auto-apply") is True and eq(now.get("name"), "web-prod")
            and eq(now.get("description"), "Production web") and eq(now.get("execution-mode"), "remote"),
            read={k: now.get(k) for k in ("name", "terraform-version", "auto-apply", "description", "execution-mode")})
    region = data(variable(ws, key="region", value="eu-west-1", category="terraform", hcl=False, sensitive=False,
                           description="Deploy region"))
    created = j.last["status"]
    owner = obj(obj(obj(region.get("relationships")).get("configurable")).get("data")).get("id")
    rows = variables(ws)
    j.check("a variable is created on the workspace",
            created == 201 and matches("var-[A-Za-z0-9]{16}", region.get("id")) and eq(attrs(region).get("key"), "region")
            and eq(attrs(region).get("value"), "eu-west-1") and eq(attrs(region).get("category"), "terraform")
            and eq(owner, ws) and [attrs(r).get("key") for r in rows] == ["region"],
            status_code=created, variable=attrs(region), configurable=owner, listed=[attrs(r).get("key") for r in rows])
    taken = variable(ws, key="region", value="us-east-1", category="terraform")
    j.check("a taken key in the same category is refused with 422", failed(taken, 422), errors=taken.get("errors"))
    secret = attrs(data(variable(ws, key="TF_TOKEN", value=f"s3cr3t-{tag}", category="env", sensitive=True)))
    rows = variables(ws)
    shown = attrs(next((r for r in rows if attrs(r).get("key") == "TF_TOKEN"), {}))
    j.check("a sensitive variable answers a null value",
            secret.get("sensitive") is True and "value" in secret and secret.get("value") is None
            and shown.get("sensitive") is True and "value" in shown and shown.get("value") is None
            and same_items([attrs(r).get("key") for r in rows], ["region", "TF_TOKEN"]),
            created={k: secret.get(k) for k in ("key", "sensitive", "value")},
            listed={k: shown.get(k) for k in ("key", "sensitive", "value")})
    send("POST", f"/api/v2/workspaces/{staging}/actions/safe-delete")
    deleted = j.last["status"]
    gone = obj(send("GET", f"/api/v2/workspaces/{staging}"))
    id_gone = failed(gone, 404)
    by_name = obj(send("GET", f"{base}/web-staging"))
    j.check("a safe-deleted workspace is not found by id or name",
            deleted == 204 and id_gone and failed(by_name, 404),
            delete_status=deleted, by_id=gone.get("errors"), by_name=by_name.get("errors"))


BLIND11_JOURNEYS = {"bigcommerce": bigcommerce_steps, "helpscout": helpscout_steps, "mux": mux_steps,
                    "postmark": postmark_steps, "terraform": terraform_steps}


def run_journeys_blind11(srv, sink, args):
    run_journey_table(BLIND11_JOURNEYS, srv, sink, args)


# ---------------------------------------------------------------- twelfth blind journeys

# The same rule again, over five more contracts written for this suite from each real
# API's public documentation (scripts/llm-bench/contracts/blind12/). They were picked for
# API styles and domains no earlier set has: a document database with client-chosen ids,
# revisions checked on every write (409 `conflict`), tombstones that read 404 `deleted`,
# `_all_docs` key ranges with `limit`/`skip`, Mango selectors and database counters of
# live and deleted documents (CouchDB), a message broker whose objects are named by
# virtual host and name in the path, idempotent `PUT` declarations, bindings whose
# `Location` header names them, `routed` answers, messages got oldest first and put back
# or removed by their ack mode, and queue lists ordered by `sort` (RabbitMQ), Django REST
# framework lists (`count`/`next`/`previous`/`results`, `limit`/`offset`, `ordering`),
# choice fields answered as `{value, label}`, field-keyed 400 errors, tags by slug, a VRF
# that refuses duplicate addresses, and addresses allocated from a prefix's free ones
# (NetBox), clients, projects and task assignments, a running timer stopped and
# restarted, `from`/`to` date ranges and pages reached by following `links.next`
# (Harvest), and prefixed ids, newest-first lists filtered by `metadata[key]`, `after`
# cursors in `next_url`, deletes that answer `{id, deleted: true}`, scheduled letters that
# can be cancelled and an `Idempotency-Key` that creates no second letter (Lob).
#
# No check gates the rest, and one miss stays one miss: a check that uses an earlier
# call's result goes by state the journey reads again (an id from a filtered list, a
# revision or status from a read) or a fallback of its own (an object a create did not
# make is made again), so only the check that judges that call fails. A check asserts
# only what the vendor's public documentation promises a client: no status code, order
# or field it does not give. Do not edit a check to suit results.
BLIND12_CHECKS = {
    "couchdb": [
        "a created database answers ok", "a taken database name is refused with 412 file_exists",
        "a PUT document answers its id and a first revision", "the document reads with its _id, _rev and ETag",
        "a PUT of an existing id without its revision is refused with 409",
        "an update with the current revision answers the next revision",
        "a stale revision is refused and changes nothing",
        "a POST stores a document under an id the server makes",
        "_all_docs lists every document by id with total_rows", "startkey and endkey bound the ids",
        "skip and limit answer the row after the first", "a _find selector finds the matching documents",
        "a deleted document answers a new revision and reads 404 deleted",
        "the database counts live and deleted documents",
    ],
    "rabbitmq": [
        "a created virtual host answers success and reads by name", "a declared queue answers success and reads its arguments",
        "declaring the queue again the same way succeeds and changes nothing",
        "a declaration with other arguments is refused and changes nothing",
        "a binding answers success with a Location", "the binding is listed with its routing key and properties_key",
        "a message a binding matches is routed", "a message no binding matches is not routed",
        "ack_requeue_true answers the oldest message and puts it back",
        "ack_requeue_false answers the oldest message and removes it",
        "sort=name lists the virtual host's queues by name", "sort_reverse lists them in the reverse order",
        "if-empty keeps a queue that holds messages", "a purged queue deletes and is then not found",
    ],
    "netbox": [
        "a created site answers success with its slug and status", "a taken slug is refused naming the field",
        "a tag filter lists the tagged site", "a created prefix keeps its prefix, VRF and status",
        "available-ips lists the six usable addresses of the /29",
        "a POST to available-ips allocates a free address",
        "a list body allocates one address per item",
        "an address the VRF holds is refused naming address", "allocated addresses are no longer available",
        "limit 2 answers two addresses, the count and a next page", "offset 2 reaches the last address",
        "a patch changes the status and keeps the address", "a status filter lists the reserved address",
        "a deleted address is not found and is available again",
    ],
    "harvest": [
        "a created client answers 201, active, with its currency", "a project answers its client and how it bills",
        "the task assignment puts the task on the project at its rate", "an entry with hours is stopped and holds them",
        "the entry reads back with its task assignment", "an entry without hours starts a running timer",
        "is_running true lists only the running entry", "stop ends the timer", "restart runs the entry again",
        "a patch changes the notes and keeps the hours", "from and to keep the entries spent in that range",
        "per_page 1 answers one entry and the page counts", "links.next reaches another entry",
        "a deleted entry is not found and leaves the list",
    ],
    "lob": [
        "a created address answers its adr_ id and fields", "the address reads back by id",
        "a metadata filter lists the run's addresses newest first", "limit 1 answers one address and a next_url",
        "the next_url cursor reaches the older address", "a deleted address answers its id and deleted true",
        "a letter to the address answers its ltr_ id and both addresses",
        "a scheduled letter keeps its send_date and is expected after it",
        "a repeated Idempotency-Key answers the same letter",
        "a metadata filter lists the run's letters newest first", "scheduled true lists only the scheduled letter",
        "a scheduled letter is cancelled with deleted true", "scheduled false lists only the unscheduled letter",
        "a letter with an unknown use_type is refused with 422",
    ],
}
JOURNEY_CHECKS.update(BLIND12_CHECKS)
BLIND12_DIR = "scripts/llm-bench/contracts/blind12"
# Contracts alone: no persona.
BLIND12_MOCKS = {name: (f"{BLIND12_DIR}/{name}.openapi.yaml", f"/{name}", None) for name in BLIND12_CHECKS}
HEX = "0123456789abcdef"


def basic_auth(user, password):
    return {"authorization": "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()}


def same_text(a, b):
    """Equal strings, ignoring case (an API may answer an address in capitals)."""
    return isinstance(a, str) and isinstance(b, str) and a.casefold() == b.casefold()


def couchdb_steps(j):
    # The database is new and named for this run, so its documents are the ones made here.
    j.headers.update(dict(basic_auth("bench", "bench-placeholder"), accept="application/json"))
    tag = j.rng.randrange(10**5, 10**6)
    db = f"/bench-{tag}"
    customer = f"ana.{tag}@bench.example"

    def revision(rev):
        """N of a revision `N-<32 hex>`, or None."""
        return int(rev.split("-", 1)[0]) if matches("[0-9]+-[0-9a-f]{32}", rev) else None

    def own_rev(rev, n):
        return rev if revision(rev) is not None else f"{n}-{made_up(j, '', 32, HEX)}"

    def written():
        return j.last["status"] in (201, 202)

    def doc(docid):
        return obj(j.call("GET", f"{db}/{docid}"))

    def stored(docid, body):
        """The document as it reads. One the write before stored nothing (that write's
        check's miss) is written again, so the checks after it judge their own calls."""
        read = doc(docid)
        if j.last["status"] == 404:
            j.call("PUT", f"{db}/{docid}", body=body)
            read = doc(docid)
        return read

    def all_docs(**query):
        # Keys are JSON: a string key is sent in double quotes.
        query = {k: json.dumps(v) if k in ("startkey", "endkey") else v for k, v in query.items()}
        answer = obj(j.call("GET", f"{db}/_all_docs", query=query))
        return ids_of(answer.get("rows")), answer

    made = obj(j.call("PUT", db))
    created = written()
    j.check("a created database answers ok", created and made.get("ok") is True, answer=made)
    taken = obj(j.call("PUT", db))
    if not created and written():
        # This PUT made the database the first did not (the check before's miss), so the
        # taken name is judged on one more.
        taken = obj(j.call("PUT", db))
    j.check("a taken database name is refused with 412 file_exists",
            j.last["status"] == 412 and eq(taken.get("error"), "file_exists"), error=taken)
    first = {"type": "order", "customer": customer, "total": 12.5, "status": "open"}
    put = obj(j.call("PUT", f"{db}/order-001", body=first))
    j.check("a PUT document answers its id and a first revision",
            written() and put.get("ok") is True and eq(put.get("id"), "order-001") and revision(put.get("rev")) == 1,
            answer=put)
    read = stored("order-001", first)
    etag = str(j.answer_headers.get("ETag") or "")
    # rev1 is the PUT's answer, or the read's when the PUT answered no revision, so a
    # stored document with a missing rev is the PUT check's miss only.
    rev1 = put.get("rev") if revision(put.get("rev")) is not None else own_rev(read.get("_rev"), 1)
    j.check("the document reads with its _id, _rev and ETag",
            eq(read.get("_id"), "order-001") and eq(read.get("_rev"), rev1) and eq(read.get("customer"), customer)
            and near(read.get("total"), 12.5, 0.001) and etag.strip('"') == read.get("_rev"),
            document=read, etag=etag)
    # The refused PUT is judged against the document as it read, so a read that differed
    # from the PUT's answer is the read check's miss only.
    before = {"_rev": given(read.get("_rev"), rev1), "status": given(read.get("status"), "open")}
    again = obj(j.call("PUT", f"{db}/order-001", body=dict(first, status="cancelled")))
    refused = j.last["status"]
    kept = doc("order-001")
    j.check("a PUT of an existing id without its revision is refused with 409",
            refused == 409 and eq(again.get("error"), "conflict") and eq(kept.get("_rev"), before["_rev"])
            and eq(kept.get("status"), before["status"]),
            status_code=refused, error=again, held=before, read={k: kept.get(k) for k in ("_rev", "status")})
    # The revision the document holds now (rev1 unless the refused PUT was stored), so a
    # stored conflict is that check's miss only.
    current = own_rev(kept.get("_rev"), revision(before["_rev"]) or 1)
    update = obj(j.call("PUT", f"{db}/order-001", body=dict(first, _rev=current, status="paid")))
    updated = written()
    read = doc("order-001")
    j.check("an update with the current revision answers the next revision",
            updated and revision(update.get("rev")) == revision(current) + 1 and eq(read.get("_rev"), update.get("rev"))
            and eq(read.get("status"), "paid") and near(read.get("total"), 12.5, 0.001),
            answer=update, current=current, read={k: read.get(k) for k in ("_rev", "status", "total")})
    rev2 = own_rev(update.get("rev"), revision(current) + 1)
    # The stale check compares with what the read after the update showed, so an update
    # that was refused or answered no rev is the update check's miss only. rev1 is stale
    # once the update was stored; else a made-up revision of the same generation is.
    after = {k: read.get(k) for k in ("_rev", "status")}
    old = rev1 if not eq(after["_rev"], rev1) else own_rev(None, revision(rev1) or 1)
    stale = obj(j.call("PUT", f"{db}/order-001", query={"rev": old}, body=dict(first, status="refunded")))
    refused = j.last["status"]
    kept = doc("order-001")
    j.check("a stale revision is refused and changes nothing",
            refused == 409 and eq(stale.get("error"), "conflict") and eq(kept.get("_rev"), after["_rev"])
            and eq(kept.get("status"), after["status"]),
            status_code=refused, error=stale, sent=old, held=after,
            read={k: kept.get(k) for k in ("_rev", "status")})
    # The revision order-001 holds after that check (rev2 unless the stale PUT was stored),
    # so a stored stale write is that check's miss only.
    held = own_rev(kept.get("_rev"), revision(rev2))
    posted = obj(j.call("POST", db, body={"type": "note", "text": f"Call {customer} about delivery"}))
    saved = written()
    note = given(posted.get("id"), None)
    read = doc(note) if note else {}
    j.check("a POST stores a document under an id the server makes",
            saved and note is not None and posted.get("ok") is True and revision(posted.get("rev")) == 1
            and eq(read.get("_id"), note) and eq(read.get("type"), "note"),
            answer=posted, read={k: read.get(k) for k in ("_id", "_rev", "type")})
    order2 = {"type": "order", "customer": customer, "total": 40, "status": "open"}
    second = obj(j.call("PUT", f"{db}/order-002", body=order2))
    # No check judges this PUT by itself: its revision comes from a read when it answered
    # none, and an order it did not store is stored again, so the lists judge only theirs.
    rev_b = second.get("rev") if revision(second.get("rev")) is not None else own_rev(
        stored("order-002", order2).get("_rev"), 1)
    rows, listed = all_docs(include_docs="true")
    # The lists after this go by the two orders and the one other id _all_docs shows (the
    # note as it was stored), whatever the POST answered, so a note that was not stored or
    # answered no id is the POST check's miss only.
    expected = ["order-001", "order-002"] + [r for r in rows if r not in ("order-001", "order-002")][:1]
    revs = {r.get("id"): obj(r.get("value")).get("rev") for r in dicts(listed.get("rows"))}
    orders = [r for r in rows if r in ("order-001", "order-002")]
    j.check("_all_docs lists every document by id with total_rows",
            same_items(rows, expected) and orders == ["order-001", "order-002"]
            and eq(listed.get("total_rows"), len(expected)) and eq(revs.get("order-001"), held)
            and all(isinstance(r.get("doc"), dict) for r in dicts(listed.get("rows"))),
            listed=rows, expected=expected, total_rows=listed.get("total_rows"), revs=revs)
    rows, _ = all_docs(startkey="order-", endkey="order-￰")
    j.check("startkey and endkey bound the ids", rows == ["order-001", "order-002"], listed=rows)
    rows, listed = all_docs(startkey="order-", endkey="order-￰", skip=1, limit=1)
    j.check("skip and limit answer the row after the first",
            rows == ["order-002"] and eq(listed.get("total_rows"), len(expected)),
            listed=rows, total_rows=listed.get("total_rows"))
    found = obj(j.call("POST", f"{db}/_find", body={"selector": {"type": "order", "total": {"$gte": 20}}}))
    docs = dicts(found.get("docs"))
    j.check("a _find selector finds the matching documents",
            j.last["status"] == 200 and ids_of(docs, "_id") == ["order-002"] and eq(head(docs).get("status"), "open"),
            found=ids_of(docs, "_id"))
    gone = obj(j.call("DELETE", f"{db}/order-002", query={"rev": rev_b}))
    deleted = (j.last["status"] in (200, 202) and gone.get("ok") is True
               and revision(gone.get("rev")) == revision(rev_b) + 1)
    tomb = doc("order-002")
    tomb_status = j.last["status"]
    doc(f"order-{tag}")
    never = j.last["status"]
    # The reference shows reason `deleted` for a deleted document and only 404 for one
    # that never existed.
    j.check("a deleted document answers a new revision and reads 404 deleted",
            deleted and tomb_status == 404 and eq(tomb.get("error"), "not_found") and eq(tomb.get("reason"), "deleted")
            and never == 404,
            answer=gone, deleted_read=tomb, missing_status=never)
    # The counts go by the deletion as the read observed it, so a failed delete is the
    # delete check's miss only.
    removed = tomb_status == 404 and eq(tomb.get("reason"), "deleted")
    live = [d for d in expected if not (removed and d == "order-002")]
    info = obj(j.call("GET", db))
    rows, _ = all_docs()
    j.check("the database counts live and deleted documents",
            eq(info.get("db_name"), db.lstrip("/")) and eq(info.get("doc_count"), len(live))
            and eq(info.get("doc_del_count"), 1 if removed else 0) and same_items(rows, live),
            info={k: info.get(k) for k in ("db_name", "doc_count", "doc_del_count")}, listed=rows, expected=live)


def rabbitmq_steps(j):
    # The virtual host is new and named for this run, so its queues are the ones made here.
    # The reference gives no status code for most calls (its example creates a virtual
    # host with 201 Created and deletes with 204 No Content), so a success is any 2xx.
    j.headers.update(dict(basic_auth("bench", "bench-placeholder"), accept="application/json"))
    tag = j.rng.randrange(10**5, 10**6)
    vhost = f"bench-{tag}"
    queues, created, shipped = f"/api/queues/{vhost}", "orders.created", "orders.shipped"
    between = f"/api/bindings/{vhost}/e/events/q/{created}"

    def success(status):
        return isinstance(status, int) and 200 <= status < 300

    def queue(name):
        return obj(j.call("GET", f"{queues}/{name}"))

    def declare(name, **arguments):
        answer = obj(j.call("PUT", f"{queues}/{name}", body={"durable": True, "auto_delete": False,
                                                              "arguments": arguments}))
        return j.last["status"], answer

    def ensure(name, **arguments):
        """A queue no check judges the declaration of is declared again when it does not
        read, so the checks that use it judge only their own calls."""
        declare(name, **arguments)
        queue(name)
        if j.last["status"] == 404:
            declare(name, **arguments)

    def publish(key, payload, exchange="events"):
        answer = obj(j.call("POST", f"/api/exchanges/{vhost}/{exchange}/publish", body={
            "properties": {"content_type": "text/plain"}, "routing_key": key, "payload": payload,
            "payload_encoding": "string"}))
        return j.last["status"], answer.get("routed")

    def put_on(name, payload):
        # The default exchange routes on the queue's name with no binding, so the queue's
        # messages do not hang on the topic binding the checks before judged. A publish
        # that answered routed false put nothing on the queue, so it is sent once more.
        _, routed = publish(name, payload, exchange="amq.default")
        if routed is False:
            publish(name, payload, exchange="amq.default")

    def get(name, count, ackmode):
        answer = j.call("POST", f"{queues}/{name}/get", body={"count": count, "ackmode": ackmode,
                                                               "encoding": "auto"})
        return dicts(answer if isinstance(answer, list) else [])

    def payloads(messages):
        return [m.get("payload") for m in messages]

    def names(**query):
        answer = j.call("GET", queues, query=query)
        return [q.get("name") for q in dicts(answer if isinstance(answer, list) else [])]

    j.call("PUT", f"/api/vhosts/{vhost}", body={"description": f"Bench run {tag}"})
    made = j.last["status"]
    read = obj(j.call("GET", f"/api/vhosts/{vhost}"))
    j.check("a created virtual host answers success and reads by name",
            success(made) and eq(read.get("name"), vhost) and eq(read.get("description"), f"Bench run {tag}"),
            status_code=made, read={k: read.get(k) for k in ("name", "description")})
    if j.last["status"] == 404:
        # The virtual host was not made (the check before's miss): it is made again so the
        # calls after it judge their own.
        j.call("PUT", f"/api/vhosts/{vhost}", body={"description": f"Bench run {tag}"})
    j.call("PUT", f"/api/permissions/{vhost}/bench", body={"configure": ".*", "write": ".*", "read": ".*"})
    status, _ = declare(created, **{"x-max-length": 1000})
    read = queue(created)
    j.check("a declared queue answers success and reads its arguments",
            success(status) and eq(read.get("name"), created) and eq(read.get("vhost"), vhost)
            and read.get("durable") is True and eq(obj(read.get("arguments")).get("x-max-length"), 1000),
            status_code=status, read={k: read.get(k) for k in ("name", "vhost", "durable", "arguments")})
    if j.last["status"] == 404:
        declare(created, **{"x-max-length": 1000})
        read = queue(created)
    # The declarations after it are judged against the arguments the queue reads with, so a
    # declaration that stored others is that check's miss only.
    held = obj(read.get("arguments")).get("x-max-length")
    held = held if is_number(held) else 1000
    status, _ = declare(created, **{"x-max-length": 1000})
    read = queue(created)
    j.check("declaring the queue again the same way succeeds and changes nothing",
            success(status) and eq(obj(read.get("arguments")).get("x-max-length"), held),
            status_code=status, arguments=read.get("arguments"), held=held)
    now = obj(read.get("arguments")).get("x-max-length")
    held = now if is_number(now) else held
    status, refused = declare(created, **{"x-max-length": 10})
    read = queue(created)
    # AMQP 0-9-1 raises a channel exception for a declaration whose arguments differ from
    # the queue's; the HTTP reference gives no code, so any answer but a success passes.
    j.check("a declaration with other arguments is refused and changes nothing",
            isinstance(status, int) and status >= 400 and eq(obj(read.get("arguments")).get("x-max-length"), held),
            status_code=status, error=refused, arguments=read.get("arguments"), held=held)
    ensure(shipped)
    ensure("audit")
    ensure("ledger")
    j.call("PUT", f"/api/exchanges/{vhost}/events", body={"type": "topic", "durable": True})
    j.call("GET", f"/api/exchanges/{vhost}/events")
    if j.last["status"] == 404:
        j.call("PUT", f"/api/exchanges/{vhost}/events", body={"type": "topic", "durable": True})
    j.call("POST", between, body={"routing_key": "order.created"})
    bound = j.last["status"]
    location = str(j.answer_headers.get("Location") or "")
    j.check("a binding answers success with a Location",
            success(bound) and f"/e/events/q/{created}/" in location,
            status_code=bound, location=location)
    answer = j.call("GET", between)
    rows = dicts(answer if isinstance(answer, list) else [])
    mine = [r for r in rows if eq(r.get("routing_key"), "order.created")]
    if not mine:
        # The binding was not made (the check before's miss): it is made again, so this
        # check judges the listing only.
        j.call("POST", between, body={"routing_key": "order.created"})
        answer = j.call("GET", between)
        rows = dicts(answer if isinstance(answer, list) else [])
        mine = [r for r in rows if eq(r.get("routing_key"), "order.created")]
    # The reference names properties_key as a field of a bindings listing.
    j.check("the binding is listed with its routing key and properties_key",
            success(j.last["status"]) and len(mine) == 1 and isinstance(head(mine).get("properties_key"), str)
            and bool(head(mine).get("properties_key")),
            bindings=rows)
    first, second = f"order {tag}-1 created", f"order {tag}-2 created"
    _, routed = publish("order.created", first)
    j.check("a message a binding matches is routed", routed is True, routed=routed)
    _, routed = publish("invoice.sent", f"invoice {tag} sent")
    j.check("a message no binding matches is not routed", routed is False, routed=routed)
    # A queue hands out its oldest message first. ack_requeue_true puts what it got back,
    # and a message put back is marked redelivered (AMQP basic.get-ok), so the second get
    # sees both messages, the first redelivered.
    put_on("audit", first)
    put_on("audit", second)
    got = get("audit", 1, "ack_requeue_true")
    rest = get("audit", 5, "ack_requeue_true")
    back = [m for m in rest if eq(m.get("payload"), first)]
    j.check("ack_requeue_true answers the oldest message and puts it back",
            payloads(got) == [first] and head(got).get("redelivered") is False
            and same_items(payloads(rest), [first, second]) and len(back) == 1 and back[0].get("redelivered") is True,
            got=got, then=rest)
    # A queue of its own, so the requeue check before is not this one's too.
    one, two = f"ledger {tag}-1", f"ledger {tag}-2"
    put_on("ledger", one)
    put_on("ledger", two)
    got = get("ledger", 1, "ack_requeue_false")
    rest = get("ledger", 5, "ack_requeue_false")
    left = get("ledger", 5, "ack_requeue_false")
    j.check("ack_requeue_false answers the oldest message and removes it",
            success(j.last["status"]) and payloads(got) == [one] and payloads(rest) == [two] and left == [],
            got=got, then=rest, left=left)
    # The queues the listings must hold are the ones that read now, so a declaration that
    # failed is not these checks' miss too.
    present = []
    for name in ("audit", "ledger", created, shipped):
        queue(name)
        if success(j.last["status"]):
            present.append(name)
    present.sort()
    listed = names(sort="name")
    j.check("sort=name lists the virtual host's queues by name", listed == present and len(present) > 0,
            listed=listed, expected=present)
    listed = names(sort="name", sort_reverse="true")
    j.check("sort_reverse lists them in the reverse order", listed == present[::-1] and len(present) > 0,
            listed=listed, expected=present[::-1])
    # A fresh message, so the queue holds one whatever the get checks left in it.
    queue("audit")
    if j.last["status"] == 404:
        declare("audit")
    put_on("audit", f"audit {tag} kept")
    kept = obj(j.call("DELETE", f"{queues}/audit", query={"if-empty": "true"}))
    refused = j.last["status"]
    read = queue("audit")
    read_status = j.last["status"]
    # The reference says only that if-empty prevents the delete from succeeding, so any
    # answer that is not a success passes, with the queue still there.
    j.check("if-empty keeps a queue that holds messages",
            isinstance(refused, int) and refused >= 400 and read_status == 200 and eq(read.get("name"), "audit"),
            status_code=refused, error=kept, read_status=read_status)
    # A queue the if-empty delete removed (or that does not read) is declared and filled
    # again, so that delete is the check before's miss only and the next check judges the
    # purge and the delete.
    if read_status != 200:
        declare("audit")
        put_on("audit", f"audit {tag} refilled")
    j.call("DELETE", f"{queues}/audit/contents")
    purged = j.last["status"]
    j.call("DELETE", f"{queues}/audit", query={"if-empty": "true"})
    deleted = j.last["status"]
    queue("audit")
    j.check("a purged queue deletes and is then not found",
            success(purged) and success(deleted) and j.last["status"] == 404,
            purge_status=purged, delete_status=deleted, read_status=j.last["status"])


def netbox_steps(j):
    # The site, tag and VRF are named for this run and the prefix is in the run's VRF, so
    # every list filters on what was made here. The REST API documentation gives no status
    # code for a create (it does for a delete: 204), so a create succeeds with any 2xx.
    j.headers.update({"authorization": "Bearer nbt_bench.bench-placeholder", "accept": "application/json"})
    tag = j.rng.randrange(10**5, 10**6)
    a, b = j.rng.randrange(1, 255), j.rng.randrange(0, 256)
    net = f"10.{a}.{b}"
    prefix = f"{net}.0/29"
    slug, label = f"bench-dc-{tag}", f"edge-{tag}"
    usable = [f"{net}.{n}/29" for n in range(1, 7)]

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**4, 10**6)

    def made():
        return isinstance(j.last["status"], int) and 200 <= j.last["status"] < 300

    def status_of(thing):
        return obj(obj(thing).get("status")).get("value")

    def vrf_of(thing):
        return obj(obj(thing).get("vrf")).get("id")

    def found(path, **query):
        """The id of the one object a filtered list holds, or None."""
        rows = dicts(obj(j.call("GET", path, query=query)).get("results"))
        return rows[0].get("id") if len(rows) == 1 and is_int(rows[0].get("id")) else None

    def id_of(answer, path, **query):
        """The id a create answered, else the one a list filtered to it shows, so a create
        that answered no id is its own check's miss only."""
        return answer.get("id") if is_int(answer.get("id")) else found(path, **query)

    def free():
        answer = j.call("GET", f"/api/ipam/prefixes/{pid}/available-ips/")
        return [r.get("address") for r in dicts(answer if isinstance(answer, list) else [])]

    def addresses(**query):
        answer = obj(j.call("GET", "/api/ipam/ip-addresses/", query=dict({"vrf_id": vrf}, **query)))
        return dicts(answer.get("results")), answer

    site = obj(j.call("POST", "/api/dcim/sites/", body={"name": f"Bench DC {tag}", "slug": slug, "status": "active"}))
    j.check("a created site answers success with its slug and status",
            made() and is_int(site.get("id")) and eq(site.get("name"), f"Bench DC {tag}")
            and eq(site.get("slug"), slug) and status_of(site) == "active"
            and eq(obj(site.get("status")).get("label"), "Active"),
            site={k: site.get(k) for k in ("id", "name", "slug", "status")})
    sid = id_of(site, "/api/dcim/sites/", slug=slug)
    taken = obj(j.call("POST", "/api/dcim/sites/", body={"name": f"Other DC {tag}", "slug": slug}))
    if sid is None and made():
        # This create made the site the first did not (the check before's miss): the taken
        # slug is judged on one more.
        sid = id_of(taken, "/api/dcim/sites/", slug=slug)
        taken = obj(j.call("POST", "/api/dcim/sites/", body={"name": f"Third DC {tag}", "slug": slug}))
    j.check("a taken slug is refused naming the field", j.last["status"] == 400 and "slug" in taken, error=taken)
    sid = own(sid)
    j.call("POST", "/api/extras/tags/", body={"name": label, "slug": label, "color": "4caf50"})
    if found("/api/extras/tags/", slug=label) is None:
        j.call("POST", "/api/extras/tags/", body={"name": label, "slug": label, "color": "4caf50"})
    patched = obj(j.call("PATCH", f"/api/dcim/sites/{sid}/", body={"tags": [{"name": label}]}))
    rows = obj(j.call("GET", "/api/dcim/sites/", query={"tag": label})).get("results")
    j.check("a tag filter lists the tagged site",
            [t.get("slug") for t in dicts(patched.get("tags"))] == [label] and ids_of(rows) == [sid],
            tags=patched.get("tags"), listed=ids_of(rows))
    vrf_answer = obj(j.call("POST", "/api/ipam/vrfs/", body={"name": f"bench-{tag}", "enforce_unique": True}))
    vrf = own(id_of(vrf_answer, "/api/ipam/vrfs/", name=f"bench-{tag}"))
    body = {"prefix": prefix, "vrf": vrf, "status": "active", "description": f"Bench servers {tag}"}
    answer = obj(j.call("POST", "/api/ipam/prefixes/", body=body))
    j.check("a created prefix keeps its prefix, VRF and status",
            made() and is_int(answer.get("id")) and eq(answer.get("prefix"), prefix)
            and eq(vrf_of(answer), vrf) and status_of(answer) == "active",
            prefix={k: answer.get(k) for k in ("id", "prefix", "vrf", "status")})
    pid = id_of(answer, "/api/ipam/prefixes/", prefix=prefix, vrf_id=vrf)
    if pid is None:
        # The prefix was not made (the check before's miss): it is made again so the
        # checks on its addresses judge their own calls.
        pid = id_of(obj(j.call("POST", "/api/ipam/prefixes/", body=body)), "/api/ipam/prefixes/", prefix=prefix,
                    vrf_id=vrf)
    pid = own(pid)
    # A prefix that is not a pool keeps its first and last addresses (network and
    # broadcast) out of use, so a /29 has six. The order is not promised.
    listed = free()
    j.check("available-ips lists the six usable addresses of the /29", same_items(listed, usable), available=listed)
    gateway = obj(j.call("POST", f"/api/ipam/prefixes/{pid}/available-ips/", body={
        "status": "active", "dns_name": f"gw.{tag}.bench.example", "description": f"Gateway {tag}"}))
    j.check("a POST to available-ips allocates a free address",
            made() and is_int(gateway.get("id")) and gateway.get("address") in usable
            and eq(gateway.get("dns_name"), f"gw.{tag}.bench.example"),
            address={k: gateway.get(k) for k in ("id", "address", "dns_name")})
    answer = j.call("POST", f"/api/ipam/prefixes/{pid}/available-ips/", body=[
        {"status": "active", "description": f"Web 1 {tag}"}, {"status": "active", "description": f"Web 2 {tag}"}])
    pair = dicts(answer if isinstance(answer, list) else [])
    at = [p.get("address") for p in pair]
    # Two addresses, each free, neither the one the gateway allocation answered (that
    # allocation's address is its own check's miss when it is wrong).
    j.check("a list body allocates one address per item",
            made() and len(pair) == 2 and len(set(at)) == 2 and all(x in usable for x in at)
            and gateway.get("address") not in at
            and same_items([p.get("description") for p in pair], [f"Web 1 {tag}", f"Web 2 {tag}"]),
            allocated=[{k: p.get(k) for k in ("id", "address", "description")} for p in pair])
    web = [p for p in pair if eq(p.get("description"), f"Web 2 {tag}") and is_int(p.get("id"))
           and isinstance(p.get("address"), str)]
    allocated = len(web) == 1
    web2, web2_at = (web[0]["id"], web[0]["address"]) if allocated else (None, None)
    answered = [gateway.get("address")] + at
    # The duplicate is an address the prefix lists (else one an allocation answered), so a
    # wrong allocation answer is not this check's miss too.
    rows, _ = addresses(parent=prefix)
    seen = [r.get("address") for r in rows]
    dup = obj(j.call("POST", "/api/ipam/ip-addresses/", body={
        "address": next((x for x in seen + answered if x in usable), usable[0]), "vrf": vrf, "status": "active"}))
    j.check("an address the VRF holds is refused naming address", j.last["status"] == 400 and "address" in dup,
            error=dup)
    if made() and is_int(dup.get("id")) and dup.get("id") not in [gateway.get("id")] + ids_of(pair):
        # A duplicate that was stored is that check's miss: it is removed so the lists
        # after it hold only the allocated addresses.
        j.call("DELETE", f"/api/ipam/ip-addresses/{dup['id']}/")
    # The addresses the prefix holds are those the allocations answered or its list shows,
    # so an allocation that answered the wrong address is its own check's miss only.
    held = [x for x in usable if x in answered or x in seen]
    left = free()
    j.check("allocated addresses are no longer available",
            bool(held) and same_items(left, [x for x in usable if x not in held]), available=left, allocated=held)
    # The paging checks need three addresses: one an allocation did not make is created
    # directly, so a failed allocation is that check's miss, not the paging checks' too.
    for x in [x for x in usable if x not in held][:max(0, 3 - len(held))]:
        extra = obj(j.call("POST", "/api/ipam/ip-addresses/", body={"address": x, "vrf": vrf, "status": "active"}))
        if made() and eq(extra.get("address"), x):
            held = [y for y in usable if y in held + [x]]
            if web2 is None and is_int(extra.get("id")):
                # The list allocation answered no second address: the later checks act on
                # one that exists, so that miss is not counted again.
                web2, web2_at = extra["id"], x
    if web2 is None:
        # Else any address of the prefix other than the gateway.
        rows, _ = addresses(parent=prefix)
        other = [r for r in rows if is_int(r.get("id")) and r.get("id") != gateway.get("id")
                 and isinstance(r.get("address"), str)]
        web2, web2_at = (other[-1]["id"], other[-1]["address"]) if other else (own(None), usable[2])
    # `ordering` sorts the list (the default order is not promised).
    rows, answer = addresses(parent=prefix, limit=2, ordering="address")
    j.check("limit 2 answers two addresses, the count and a next page",
            len(held) > 2 and [r.get("address") for r in rows] == held[:2] and eq(answer.get("count"), len(held))
            and isinstance(answer.get("next"), str) and answer.get("previous") is None,
            listed=[r.get("address") for r in rows], count=answer.get("count"), next=answer.get("next"))
    rows, answer = addresses(parent=prefix, limit=2, offset=2, ordering="address")
    j.check("offset 2 reaches the last address",
            [r.get("address") for r in rows] == held[2:4] and "next" in answer
            and (answer.get("next") is None if len(held) <= 4 else isinstance(answer.get("next"), str))
            and isinstance(answer.get("previous"), str),
            listed=[r.get("address") for r in rows], next=answer.get("next"), previous=answer.get("previous"))
    changed = obj(j.call("PATCH", f"/api/ipam/ip-addresses/{web2}/", body={"status": "reserved"}))
    read = obj(j.call("GET", f"/api/ipam/ip-addresses/{web2}/"))
    j.check("a patch changes the status and keeps the address",
            status_of(changed) == "reserved" and status_of(read) == "reserved" and eq(read.get("address"), web2_at)
            and (not allocated or eq(read.get("description"), f"Web 2 {tag}")) and eq(vrf_of(read), vrf),
            answered=status_of(changed), read={k: read.get(k) for k in ("address", "status", "description")})
    # The address the read shows reserved, so a patch that did not hold is that check's
    # miss; the answer must still be a list page, so an empty answer is no pass.
    rows, answer = addresses(status="reserved")
    want = [web2] if status_of(read) == "reserved" else []
    j.check("a status filter lists the reserved address",
            j.last["status"] == 200 and isinstance(answer.get("results"), list) and ids_of(rows) == want
            and eq(answer.get("count"), len(want)),
            listed=ids_of(rows), count=answer.get("count"), expected=want)
    j.call("DELETE", f"/api/ipam/ip-addresses/{web2}/")
    deleted = j.last["status"]
    j.call("GET", f"/api/ipam/ip-addresses/{web2}/")
    missing = j.last["status"]
    left = free()
    j.check("a deleted address is not found and is available again",
            deleted == 204 and missing == 404 and web2_at in left
            and same_items(left, [x for x in usable if x not in held or x == web2_at]),
            delete_status=deleted, read_status=missing, available=left)


def harvest_steps(j):
    # The client and project are named for this run, and every list of time entries
    # filters on the run's project, so the lists hold only the entries made here.
    j.headers.update(dict(HELDOUT_AUTH, **{"harvest-account-id": "1234567",
                                          "user-agent": "Mockingbird bench (bench@bench.example)"}))
    tag = j.rng.randrange(10**5, 10**6)
    today = datetime.now(timezone.utc).date()

    def day(n):
        return (today - timedelta(days=n)).isoformat()

    def own(value):
        return value if is_int(value) else j.rng.randrange(10**6, 10**9)

    def ref(thing, key):
        return obj(obj(thing).get(key)).get("id")

    def named(path, key, name, **query):
        """The id of the one object of a list named `name`, or None: a create that answered
        no id is its own check's miss, and the calls after it use the object it made."""
        rows = [r for r in dicts(obj(j.call("GET", path, query=query)).get(key)) if eq(r.get("name"), name)]
        return rows[0].get("id") if len(rows) == 1 and is_int(rows[0].get("id")) else None

    def entry(eid):
        return obj(j.call("GET", f"/v2/time_entries/{eid}"))

    def entries(query):
        answer = obj(j.call("GET", "/v2/time_entries", query=dict({"project_id": pid}, **query)))
        return ids_of(answer.get("time_entries")), answer

    def log(spent, **fields):
        return obj(j.call("POST", "/v2/time_entries", body=dict({"project_id": pid, "task_id": tid,
                                                                  "spent_date": spent}, **fields)))

    def logged(answer, spent, known, **fields):
        """The id an entry's create answered; else the one entry of that day the project
        lists that is not already known; else one logged again. So a create that answered
        no id is its own check's miss only."""
        if is_int(answer.get("id")):
            return answer["id"]
        rows, _ = entries({"from": spent, "to": spent})
        other = [r for r in rows if is_int(r) and r not in known]
        if len(other) == 1:
            return other[0]
        return own(log(spent, **fields).get("id"))

    client = obj(j.call("POST", "/v2/clients", body={"name": f"Bench Client {tag}", "currency": "EUR"}))
    j.check("a created client answers 201, active, with its currency",
            j.last["status"] == 201 and is_int(client.get("id")) and eq(client.get("name"), f"Bench Client {tag}")
            and client.get("is_active") is True and eq(client.get("currency"), "EUR"),
            client={k: client.get(k) for k in ("id", "name", "is_active", "currency")})
    cid = own(client.get("id") if is_int(client.get("id")) else named("/v2/clients", "clients", f"Bench Client {tag}"))
    project = obj(j.call("POST", "/v2/projects", body={"client_id": cid, "name": f"Website {tag}", "code": f"W{tag}",
                                                       "is_billable": True, "bill_by": "Tasks", "budget_by": "none"}))
    j.check("a project answers its client and how it bills",
            j.last["status"] == 201 and is_int(project.get("id")) and eq(ref(project, "client"), cid)
            and eq(obj(project.get("client")).get("name"), f"Bench Client {tag}") and eq(project.get("bill_by"), "Tasks")
            and project.get("is_billable") is True,
            project={k: project.get(k) for k in ("id", "name", "client", "bill_by", "is_billable")})
    pid = own(project.get("id") if is_int(project.get("id"))
              else named("/v2/projects", "projects", f"Website {tag}", client_id=cid))
    task = obj(j.call("POST", "/v2/tasks", body={"name": f"Design {tag}", "default_hourly_rate": 90,
                                                 "billable_by_default": True}))
    tid = own(task.get("id") if is_int(task.get("id")) else named("/v2/tasks", "tasks", f"Design {tag}"))

    def assignments():
        rows = dicts(obj(j.call("GET", f"/v2/projects/{pid}/task_assignments")).get("task_assignments"))
        # A task with is_default is added to every new project, so the project may also hold
        # the account's default tasks: only the assignment of this run's task is judged.
        return [r for r in rows if eq(ref(r, "task"), tid)]

    j.call("POST", f"/v2/projects/{pid}/task_assignments", body={"task_id": tid, "hourly_rate": 120,
                                                                  "billable": True})
    assigned = j.last["status"]
    mine = assignments()
    j.check("the task assignment puts the task on the project at its rate",
            assigned == 201 and len(mine) == 1 and near(head(mine).get("hourly_rate"), 120, 0.001)
            and head(mine).get("billable") is True,
            status_code=assigned, assignments=mine)
    if not mine:
        # The task is not on the project (the check before's miss): it is assigned again,
        # since an entry can only be logged to an assigned task.
        j.call("POST", f"/v2/projects/{pid}/task_assignments", body={"task_id": tid, "hourly_rate": 120,
                                                                      "billable": True})
        mine = assignments()
    first = log(day(10), hours=1.5, notes=f"Wireframes {tag}")
    j.check("an entry with hours is stopped and holds them",
            j.last["status"] == 201 and is_int(first.get("id")) and first.get("is_running") is False
            and near(first.get("hours"), 1.5, 0.001) and eq(first.get("spent_date"), day(10))
            and eq(ref(first, "project"), pid) and eq(ref(first, "task"), tid) and eq(ref(first, "client"), cid),
            entry={k: first.get(k) for k in ("id", "spent_date", "hours", "is_running", "project", "task", "client")})
    e1 = logged(first, day(10), [], hours=1.5, notes=f"Wireframes {tag}")
    read = entry(e1)
    # The time entry object names its task assignment; the rate it bills at is not tied to
    # the assignment's rate by the reference, so only the assignment is judged.
    ta = head(mine).get("id")
    j.check("the entry reads back with its task assignment",
            j.last["status"] == 200 and eq(read.get("id"), e1) and is_int(ref(read, "task_assignment"))
            and (not is_int(ta) or eq(ref(read, "task_assignment"), ta)),
            entry={k: read.get(k) for k in ("id", "task_assignment")}, assignment=ta)
    e3 = logged(log(day(3), hours=2.0, notes=f"Mockups {tag}"), day(3), [e1], hours=2.0, notes=f"Mockups {tag}")
    timer = log(day(0), notes=f"Review {tag}")
    j.check("an entry without hours starts a running timer",
            j.last["status"] == 201 and is_int(timer.get("id")) and timer.get("is_running") is True
            and near(timer.get("hours"), 0, 0.001) and instant(timer.get("timer_started_at")) is not None,
            entry={k: timer.get(k) for k in ("id", "hours", "is_running", "timer_started_at")})
    e2 = logged(timer, day(0), [e1, e3], notes=f"Review {tag}")
    mine3 = (e1, e2, e3)

    def reads():
        """Each entry as it reads now, so a list check is judged against what the entries
        hold, not against what their creates answered."""
        out = {}
        for e in mine3:
            body = entry(e)
            out[e] = (j.last["status"], body)
        return out

    now = reads()
    # The running entries are the ones that read running (the timer when its read says
    # nothing), so a timer that did not start is that check's miss only.
    running = [e for e in mine3 if now[e][1].get("is_running") is True
               or (e == e2 and now[e][1].get("is_running") is None)]
    rows, _ = entries({"is_running": "true"})
    j.check("is_running true lists only the running entry", same_items(rows, running) and bool(running),
            listed=rows, expected=running)
    if now[e2][1].get("is_running") is False:
        # A stop is possible only for a running entry: one that is not is restarted first,
        # so the stop is judged on its own.
        j.call("PATCH", f"/v2/time_entries/{e2}/restart")
    stopped = obj(j.call("PATCH", f"/v2/time_entries/{e2}/stop"))
    status = j.last["status"]
    read = entry(e2)
    j.check("stop ends the timer",
            status == 200 and stopped.get("is_running") is False and stopped.get("timer_started_at") is None
            and read.get("is_running") is False,
            status_code=status, answered={k: stopped.get(k) for k in ("is_running", "timer_started_at")},
            read=read.get("is_running"))
    # Restarting a running entry answers 422, so when the stop did not hold the restart is
    # judged on e3, which was logged with hours and so is stopped (it is deleted below).
    again = e2 if read.get("is_running") is False else e3
    started = obj(j.call("PATCH", f"/v2/time_entries/{again}/restart"))
    status = j.last["status"]
    read = entry(again)
    j.check("restart runs the entry again",
            status == 200 and started.get("is_running") is True and instant(started.get("timer_started_at")) is not None
            and read.get("is_running") is True,
            status_code=status, answered={k: started.get(k) for k in ("is_running", "timer_started_at")},
            read=read.get("is_running"), entry=again)
    changed = obj(j.call("PATCH", f"/v2/time_entries/{e1}", body={"notes": f"Wireframes, second pass {tag}"}))
    read = entry(e1)
    # Judged against the hours and day e1 read with before, so an entry stored with others
    # is its own check's miss only.
    hours1 = now[e1][1].get("hours") if is_number(now[e1][1].get("hours")) else 1.5
    spent1 = given(now[e1][1].get("spent_date"), day(10))
    j.check("a patch changes the notes and keeps the hours",
            eq(changed.get("notes"), f"Wireframes, second pass {tag}") and eq(read.get("notes"), changed.get("notes"))
            and near(read.get("hours"), hours1, 0.001) and eq(read.get("spent_date"), spent1),
            read={k: read.get(k) for k in ("notes", "hours", "spent_date")})
    now = reads()
    planned = {e1: day(10), e2: day(0), e3: day(3)}
    spent = {e: given(now[e][1].get("spent_date"), planned[e]) for e in mine3}
    alive = [e for e in mine3 if now[e][0] == 200] or list(mine3)
    inside = [e for e in alive if day(12) <= spent[e] <= day(4)]
    rows, _ = entries({"from": day(12), "to": day(4)})
    j.check("from and to keep the entries spent in that range", same_items(rows, inside) and bool(inside),
            listed=rows, expected=inside)
    # One check judges the first page and the next follows its link, so a short first
    # page is one miss; with no link the second fails without a call.
    first_page, answer = entries({"per_page": 1})
    nxt = obj(answer.get("links")).get("next")
    j.check("per_page 1 answers one entry and the page counts",
            len(first_page) == 1 and set(first_page) <= set(alive) and eq(answer.get("total_entries"), len(alive))
            and eq(answer.get("total_pages"), len(alive)) and eq(answer.get("per_page"), 1) and isinstance(nxt, str),
            listed=first_page, counts={k: answer.get(k) for k in ("per_page", "total_pages", "total_entries", "page")},
            next=nxt, expected=alive)
    if isinstance(nxt, str) and urllib.parse.urlsplit(nxt).query:
        query = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(nxt).query))
        answer = obj(j.call("GET", "/v2/time_entries", query=query))
        rows = ids_of(answer.get("time_entries"))
        j.check("links.next reaches another entry",
                len(rows) == 1 and set(rows) <= set(alive) and rows != first_page, first=first_page, listed=rows,
                next=nxt)
    else:
        j.check("links.next reaches another entry", False, reason="the first page gave no links.next", next=nxt)
    j.call("DELETE", f"/v2/time_entries/{e3}")
    deleted = j.last["status"]
    entry(e3)
    missing = j.last["status"]
    rows, _ = entries({})
    j.check("a deleted entry is not found and leaves the list",
            deleted == 200 and missing == 404 and same_items(rows, [e for e in alive if e != e3]),
            delete_status=deleted, read_status=missing, listed=rows)


def lob_steps(j):
    # Every address and letter carries metadata naming this run, and every list filters on
    # it, so the lists hold only what was made here.
    j.headers.update(dict(basic_auth("test_bench-placeholder", ""), accept="application/json"))
    tag = j.rng.randrange(10**5, 10**6)
    run = f"bench-{tag}"
    send = (datetime.now(timezone.utc).date() + timedelta(days=5)).isoformat()
    html = f"<html><body><p>Your appointment reminder {tag}.</p></body></html>"
    sender = {"name": "Bench Mail Co", "address_line1": "185 Berry St Ste 6100", "address_city": "San Francisco",
              "address_state": "CA", "address_zip": "94107", "address_country": "US"}

    def own(value, prefix):
        return value if matches(f"{prefix}[A-Za-z0-9]+", value) else made_up(j, prefix, 16, HEX)

    def listed(path, **query):
        answer = obj(j.call("GET", path, query=dict({"metadata[run]": run}, **query)))
        return ids_of(answer.get("data")), answer

    def address(fields):
        return obj(j.call("POST", "/v1/addresses", body=dict(fields, address_country="US",
                                                              description=f"Bench {tag}", metadata={"run": run})))

    def made(answer, prefix, path, known, again):
        """The id a create answered; else the one id of the run's list that is not already
        known; else the id of one made again. So a create that answered no id is its own
        check's miss only."""
        if matches(f"{prefix}[A-Za-z0-9]+", answer.get("id")):
            return answer["id"]
        rows, _ = listed(path)
        other = [r for r in rows if matches(f"{prefix}[A-Za-z0-9]+", r) and r not in known]
        if len(other) == 1:
            return other[0]
        return own(again().get("id"), prefix)

    def letter(key=None, **fields):
        if key:
            j.headers["idempotency-key"] = key
        try:
            return obj(j.call("POST", "/v1/letters", body=dict({
                "description": f"Reminder {tag}", "to": to, "from": sender, "file": html, "color": False,
                "use_type": "operational", "metadata": {"run": run}}, **fields)))
        finally:
            j.headers.pop("idempotency-key", None)

    def day_of(value):
        moment = instant(value) if isinstance(value, str) and "T" in value else None
        if moment is not None:
            return moment.date().isoformat()
        return value[:10] if isinstance(value, str) and matches(r"\d{4}-\d{2}-\d{2}", value[:10]) else None

    def scheduled(lid, planned):
        """Whether a letter reads with a send_date later than its date_created (the
        reference's meaning of scheduled), or `planned` when the read does not say."""
        read = obj(j.call("GET", f"/v1/letters/{lid}"))

        def utc(value):
            moment = instant(value) if isinstance(value, str) and "T" in value else None
            return moment.replace(tzinfo=timezone.utc) if moment and moment.tzinfo is None else moment

        sent, made_at = utc(read.get("send_date")), utc(read.get("date_created"))
        return planned if sent is None or made_at is None else sent > made_at

    ana = {"name": "Ana Silva", "address_line1": "210 King St", "address_city": "San Francisco",
           "address_state": "CA", "address_zip": "94107"}
    ben = {"name": "Ben Okafor", "address_line1": "1 Market St", "address_city": "San Francisco",
           "address_state": "CA", "address_zip": "94105"}
    answer = address(ana)
    j.check("a created address answers its adr_ id and fields",
            j.last["status"] == 200 and matches("adr_[A-Za-z0-9]+", answer.get("id"))
            and eq(answer.get("object"), "address") and same_text(answer.get("name"), "Ana Silva")
            and same_text(answer.get("address_city"), "San Francisco") and eq(obj(answer.get("metadata")).get("run"), run),
            address={k: answer.get(k) for k in ("id", "object", "name", "address_city", "metadata")})
    a1 = made(answer, "adr_", "/v1/addresses", [], lambda: address(ana))
    read = obj(j.call("GET", f"/v1/addresses/{a1}"))
    # A US address may read back standardised with a ZIP+4, so only the five digits are judged.
    j.check("the address reads back by id",
            eq(read.get("id"), a1) and same_text(read.get("name"), "Ana Silva")
            and same_text(read.get("address_line1"), "210 King St")
            and matches(r"94107(-\d{4})?", read.get("address_zip")),
            address={k: read.get(k) for k in ("id", "name", "address_line1", "address_zip")})
    # No check judges the second create by itself: an id it did not answer comes from the
    # run's list, and an address it did not make is made again, so the checks after it
    # judge their own calls.
    a2 = made(address(ben), "adr_", "/v1/addresses", [a1], lambda: address(ben))
    to = a2
    rows, _ = listed("/v1/addresses")
    j.check("a metadata filter lists the run's addresses newest first", rows == [a2, a1], listed=rows)
    # One check judges the first page and the next follows its cursor, so a short first
    # page is one miss; with no cursor the second fails without a call.
    rows, answer = listed("/v1/addresses", limit=1)
    after = query_value(answer.get("next_url"), "after")
    j.check("limit 1 answers one address and a next_url",
            rows == [a2] and eq(answer.get("count"), 1) and isinstance(answer.get("next_url"), str),
            listed=rows, count=answer.get("count"), next_url=answer.get("next_url"))
    if after:
        rows, answer = listed("/v1/addresses", limit=1, after=after)
        j.check("the next_url cursor reaches the older address", rows == [a1], listed=rows)
    else:
        j.check("the next_url cursor reaches the older address", False, reason="the first page gave no after cursor",
                next_url=answer.get("next_url"))
    gone = obj(j.call("DELETE", f"/v1/addresses/{a1}"))
    status = j.last["status"]
    # The reference shows `{id, deleted: true}` for a delete; it does not say whether a
    # deleted address still lists.
    j.check("a deleted address answers its id and deleted true",
            status == 200 and eq(gone.get("id"), a1) and gone.get("deleted") is True,
            delete_status=status, answer=gone)
    first = letter()
    j.check("a letter to the address answers its ltr_ id and both addresses",
            j.last["status"] == 200 and matches("ltr_[A-Za-z0-9]+", first.get("id")) and eq(first.get("object"), "letter")
            and eq(obj(first.get("to")).get("id"), a2) and same_text(obj(first.get("to")).get("name"), "Ben Okafor")
            and same_text(obj(first.get("from")).get("name"), "Bench Mail Co")
            and eq(first.get("use_type"), "operational") and first.get("color") is False,
            letter={k: first.get(k) for k in ("id", "object", "use_type", "color")},
            to=obj(first.get("to")).get("id"), sender=obj(first.get("from")).get("name"))
    l1 = made(first, "ltr_", "/v1/letters", [], letter)
    key = f"bench-{tag}-reminder"
    later = letter(key, send_date=send)
    expected = day_of(later.get("expected_delivery_date"))
    # The expected delivery date is "based on" the send_date: it is not before it.
    j.check("a scheduled letter keeps its send_date and is expected after it",
            j.last["status"] == 200 and matches("ltr_[A-Za-z0-9]+", later.get("id"))
            and day_of(later.get("send_date")) == send and expected is not None and expected >= send,
            letter={k: later.get(k) for k in ("id", "send_date", "expected_delivery_date")})
    l2 = made(later, "ltr_", "/v1/letters", [l1], lambda: letter(key, send_date=send))
    again = letter(key, send_date=send)
    # Idempotency keys make sure no mail piece is created more than once, so the repeat
    # answers the letter the key made.
    j.check("a repeated Idempotency-Key answers the same letter", j.last["status"] == 200 and eq(again.get("id"), l2),
            first=l2, again=again.get("id"))
    # A second letter the repeat made is that check's miss, so the lists leave it out.
    extra = [] if eq(again.get("id"), l2) else [again.get("id")]
    rows, _ = listed("/v1/letters")
    rows = [r for r in rows if r not in extra]
    j.check("a metadata filter lists the run's letters newest first", rows == [l2, l1], listed=rows)
    # Which letters are scheduled goes by how they read, so a letter stored without its
    # send_date is the scheduled letter check's miss only.
    later_on = {l1: scheduled(l1, False), l2: scheduled(l2, True)}
    want = [x for x in (l2, l1) if later_on[x]]
    rows, _ = listed("/v1/letters", scheduled="true")
    rows = [r for r in rows if r not in extra]
    j.check("scheduled true lists only the scheduled letter", rows == want and bool(want), listed=rows, expected=want)
    cancelled = obj(j.call("DELETE", f"/v1/letters/{l2}"))
    j.check("a scheduled letter is cancelled with deleted true",
            j.last["status"] == 200 and eq(cancelled.get("id"), l2) and cancelled.get("deleted") is True,
            answer=cancelled)
    # scheduled=false keeps the letters whose send_date is their date_created. A letter's
    # send_date does not change when it is cancelled, so this holds either way and a
    # failed cancel is the check before's miss only.
    want = [x for x in (l2, l1) if not later_on[x]]
    rows, _ = listed("/v1/letters", scheduled="false")
    rows = [r for r in rows if r not in extra]
    j.check("scheduled false lists only the unscheduled letter", rows == want and bool(want), listed=rows,
            expected=want)
    # use_type is marketing or operational; a missing one is allowed only with an account
    # default, so an unknown value is what is always refused. 422 is a body that did not
    # pass validation, and an error carries a message.
    bad = obj(j.call("POST", "/v1/letters", body={"to": to, "from": sender, "file": html, "color": False,
                                                  "use_type": "promotional", "metadata": {"run": run}}))
    message = obj(bad.get("error")).get("message", bad.get("message"))
    j.check("a letter with an unknown use_type is refused with 422",
            j.last["status"] == 422 and isinstance(message, str) and bool(message), error=bad)


BLIND12_JOURNEYS = {"couchdb": couchdb_steps, "rabbitmq": rabbitmq_steps, "netbox": netbox_steps,
                    "harvest": harvest_steps, "lob": lob_steps}


def run_journeys_blind12(srv, sink, args):
    run_journey_table(BLIND12_JOURNEYS, srv, sink, args)


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
    "journeys_blind8": (BLIND8_MOCKS, run_journeys_blind8),
    "journeys_blind9": (BLIND9_MOCKS, run_journeys_blind9),
    "journeys_blind10": (BLIND10_MOCKS, run_journeys_blind10),
    "journeys_blind11": (BLIND11_MOCKS, run_journeys_blind11),
    "journeys_blind12": (BLIND12_MOCKS, run_journeys_blind12),
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
    p.add_argument("--world", choices=WORLDS, default="none",
                   help="the world description each mock runs with: none (no revision; the default), "
                        "derived (Mockingbird's heuristic description written back as a revision) or agent "
                        "(a coding agent's, authored blind from the contract and cached per contract hash; "
                        "see world_author.py)")
    p.add_argument("--tag-journeys", action="store_true",
                   help="for replay.py: each mock's LLM calls carry x-bench-journey, each journey rep is "
                        "marked at the proxy, a journey's reps run one after another, and fallbacks are seeded")
    world_author.add_author_args(p)
    args = p.parse_args()
    if args.jobs < 1:
        p.error("--jobs must be at least 1")
    if not os.path.exists(args.binary) and not os.path.isabs(args.binary):
        args.binary = os.path.join(REPO, args.binary)
    args.binary = os.path.abspath(args.binary)
    if not os.path.exists(args.binary):
        sys.exit(f"{args.binary} not found: cargo build --release -p mockingbird-cli")
    os.makedirs(args.out, exist_ok=True)
    world_author.resolve_paths(args)
    args.authored = {}
    if args.journey:
        # --journey alone means the journeys suite; each journey runs once.
        args.journey = list(dict.fromkeys(args.journey))
        args.suite = args.suite or ["journeys"]
    suites = list(args.suite or DEFAULT_SUITES)
    if args.world == "agent":
        # Authored (or read from the cache) before any suite runs, so a suite never
        # waits on authoring and every suite of the run uses the same descriptions.
        contracts = [os.path.join(REPO, c) for s in suites for c, _, _ in suite_mocks(s, args).values()]
        args.authored = world_author.ensure_authored(contracts, args, args.out)
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
    world = getattr(args, "world", "none") or "none"
    sink = Sink(args.out, args.model, suite, world)
    print(f"{args.model} / {suite}" + ("" if world == "none" else f" / world {world}"), file=sys.stderr)
    failed = False
    try:
        with Server(args, suite, mocks) as srv:
            apply_worlds(srv, args, suite, mocks)
            runner(srv, sink, args)
    except Exception as e:  # one broken suite must not stop the others
        failed = True
        sink.add("suite_error", -1, False, None, "error", {"error": f"{type(e).__name__}: {e}"[:500]})
    n_ok = sum(1 for r in sink.rows if r["ok"])
    n_infra = sum(1 for r in sink.rows if not r["ok"] and obj(r.get("detail")).get("infra"))
    n_cascade = sum(1 for r in sink.rows if not r["ok"] and obj(r.get("detail")).get("infra_cascade"))
    infra = f", {n_infra} failed on provider errors" if n_infra else ""
    if n_cascade:
        infra += f", {n_cascade} failed after a provider failure earlier in the journey"
    print(f"  {suite}: {n_ok}/{len(sink.rows)} ok{infra}", file=sys.stderr)
    mix = provider_mix(args, suite)
    if mix:
        print(f"  {suite}: providers {mix}", file=sys.stderr)
    return failed


def suite_mocks(suite, args):
    """The mocks a suite's run uses: every mock of the suite, except that `journeys`
    under --journey uses only the named journeys' mocks."""
    mocks = RUNNERS[suite][0]
    if suite == "journeys" and getattr(args, "journey", None):
        return {k: v for k, v in mocks.items() if k in args.journey}
    return mocks


def apply_worlds(srv, args, suite, mocks):
    """Writes the run's world description (`--world`) onto every mock of the suite before
    its checks run, and records each write in <out>/world-applied.jsonl. `none` writes
    nothing. A write the server refuses as a whole fails the suite."""
    world = getattr(args, "world", "none") or "none"
    if world == "none" or not mocks:
        return
    import world_author
    if suite == "journeys" and getattr(args, "journey", None):
        mocks = {k: v for k, v in mocks.items() if k in args.journey}
    for name, (contract, _, _) in mocks.items():
        entry = (getattr(args, "authored", None) or {}).get(os.path.abspath(os.path.join(REPO, contract)))
        rec = world_author.apply_world(srv.api, name, world, entry)
        rec.update({"ts": world_author.utc_now(), "model": args.model, "suite": suite,
                    "contract": contract, "refused_n": len(rec.get("refused") or [])})
        world_author.append_jsonl(os.path.join(args.out, "world-applied.jsonl"), rec)
        print(f"  world {world} on {name}: revision {rec.get('revision')}, {rec['refused_n']} refused",
              file=sys.stderr)


def provider_mix(args, suite):
    """The suite's calls per serving provider, from the bench proxy's /stats
    (`Fireworks 412 (98.6%), Darkbloom 6 (1.4%)`); None when the proxy has none."""
    label = run_label(args.model, suite, args)
    try:
        with urllib.request.urlopen(f"{args.proxy_base.rstrip('/')}/stats", timeout=5) as r:
            stats = json.load(r)
    except (OSError, ValueError):
        return None
    entry = (stats.get("per_label") or {}).get(label) if isinstance(stats, dict) else None
    counts = (entry or {}).get("providers") or {}
    total = sum(counts.values())
    if not total:
        return None
    ranked = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))
    return ", ".join(f"{p} {n} ({100.0 * n / total:.1f}%)" for p, n in ranked)


if __name__ == "__main__":
    main()
