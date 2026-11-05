#!/usr/bin/env python3
"""World descriptions for the server suites: blind authoring by a coding agent, a
cache per contract hash, and applying a description to a running mock.

    python3 scripts/llm-bench/world_author.py --suite journeys_blind11 [--suite ...] \\
        [--author claude|stub] [--author-model opus] [--cache docs/benchmarks/world-cache] \\
        [--out <run dir>] [--author-jobs 2] [--refresh]

`server_suites.py --world agent` calls the same code before its suites run, so this
command is only needed to author (and pay) ahead of a run, or to inspect the cache.

Authoring one contract (`author_contract`):
1. A private `mockingbird serve` starts with that contract alone, imported as one mock
   (fresh SQLite, free ports, no LLM profile: a `call_mock` the agent makes is answered
   without any LLM, so authoring never spends on OpenRouter).
2. A working directory gets a copy of the contract and the Mockingbird agent skills
   (`mockingbird agent skills .claude/skills`), and nothing else.
3. The author runs there. `claude` (the default) is a headless Claude Code,
   `claude -p --restricted`, with the Mockingbird MCP server (`mockingbird mcp`) as its
   only MCP server, the built-in tools cut to Read, Grep and Glob (confined to the
   working directory by `--restricted`), and only the read and world-writing Mockingbird
   tools allowed. This is the path a user takes: their coding agent, with the plugin's
   MCP server and the `mockingbird-world` skill, reviews the description after an import.
   It sees the contract and the skill; never the journeys, the checks, the engine or a
   past result. `stub` runs `stub_author.py` instead: free, deterministic, through the
   CLI (`mockingbird world show|patch`), for dry runs and tests.
4. Afterwards the bench reads, through the Admin API, the effective description, every
   revision the author wrote (document, author), the refused entries and the gaps, and
   takes every world write's answer from the agent's transcript (including whole-
   document refusals, which store nothing). The newest revision's document is the
   accepted description.

The result is cached as `<cache>/<author id>/<contract sha256>.json`, so a rerun on the
same contract, author model and prompt version reuses it and pays nothing; the cached
`cost_usd` and `duration_ms` stay the first authoring's. `--refresh` authors again.

Applying (`apply_world`): `agent` PUTs the cached document onto the suite's mock as
author `agent`; `derived` reads the mock's tier-0 (heuristic) description and PUTs it
back as an explicit revision (author `user`); `none` writes nothing. Each write's answer
(`revision`, `refused`) goes to `<out>/world-applied.jsonl`.

Standard library only.
"""

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
WORLDS = ("none", "derived", "agent")
AUTHORS = ("claude", "stub")
# Bump when the prompt or the agent's tool set changes: it is part of the cache key.
PROMPT_VERSION = 1
DEFAULT_CACHE = os.path.join("docs", "benchmarks", "world-cache")
STUB_AUTHOR = os.path.join(HERE, "stub_author.py")
DEFAULT_AUTHOR_MODEL = "claude-opus-5-5"
# OpenRouter and Anthropic keys: nothing the bench writes may hold one.
SECRET_MARK = re.compile(r"sk-(?:or|ant)-[A-Za-z0-9_-]{8,}")
# Kept out of the stored transcript: machine paths and session ids, no use for attribution.
TRANSCRIPT_DROP = ("session_id", "cwd", "uuid", "apiKeySource", "memory_paths")

# The Mockingbird MCP tools the author may call: reading the mock and its world,
# writing the world, and calling the mock to check a description. Everything else
# (imports, deletes, servers, LLM profiles, key setup) is refused.
ALLOWED_MCP = ("list_mocks", "get_mock", "list_contract_versions", "get_world", "get_world_schema",
               "patch_world", "put_world", "list_world_revisions", "get_world_revision", "diff_world",
               "restore_world_revision", "call_mock", "list_requests", "get_request")
WORLD_WRITES = ("patch_world", "put_world", "restore_world_revision")
BUILTIN_TOOLS = "Read,Grep,Glob"
MCP_PREFIX = "mcp__mockingbird__"

PROMPT = """\
A Mockingbird server is running with the API contract `{contract}` (in this directory) \
imported as the mock `{mock}`, contract version 1, which is active. Mockingbird wrote a \
heuristic world description for it.

Review and improve that world description from the contract alone, the way the \
`mockingbird-world` skill says (read `.claude/skills/mockingbird-world/SKILL.md` first). \
Use the Mockingbird MCP tools: `get_world` for the description and its gaps, \
`get_world_schema` for the format, and `patch_world` or `put_world` with \
`author: "agent"` to write. Write only what the contract states; leave a key out when \
it does not say. Read `refused` in every answer and fix those entries. You may try \
your entries with `call_mock`.

When you are done, answer with a short summary of what you wrote and what you left open.\
"""


def utc_now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def mock_name_of(contract):
    """A mock name from the contract's file name: `stripe.openapi.yaml` -> `stripe`."""
    stem = os.path.basename(contract).split(".")[0].lower()
    stem = re.sub(r"[^a-z0-9-]+", "-", stem).strip("-")
    return stem or "api"


def author_id(author, model):
    if author == "stub":
        return f"stub~p{PROMPT_VERSION}"
    return f"claude-code~{re.sub(r'[^A-Za-z0-9._-]+', '_', model)}~p{PROMPT_VERSION}"


def cache_path(cache, aid, sha):
    return os.path.join(cache, aid, f"{sha}.json")


def load_cached(cache, aid, sha, inputs=None):
    """The cached entry, or None: when there is none, when it failed (failures are
    kept as `<sha>.failed.json`, never read back), or when the agent's inputs
    (`inputs`: the skills' and the world schema's sha256) changed since."""
    path = cache_path(cache, aid, sha)
    try:
        with open(path) as f:
            entry = json.load(f)
    except (OSError, ValueError):
        return None
    if not isinstance(entry, dict) or entry.get("contract_sha256") != sha or entry.get("is_error"):
        return None
    if inputs and any(entry.get("inputs", {}).get(k) != v for k, v in inputs.items()):
        return None
    return entry


def rel(path):
    return os.path.relpath(path, REPO) if path.startswith(REPO + os.sep) else path


def scan_secrets(text):
    """True when text holds what looks like an OpenRouter key."""
    return bool(SECRET_MARK.search(text or ""))


def write_text_atomic(path, text):
    if scan_secrets(text):
        raise RuntimeError(f"refusing to write {path}: it holds an API key")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".tmp-")
    with os.fdopen(fd, "w") as f:
        f.write(text)
    os.replace(tmp, path)


def write_json_atomic(path, obj):
    write_text_atomic(path, json.dumps(obj, indent=2, ensure_ascii=False, sort_keys=True) + "\n")


_JSONL_LOCK = threading.Lock()


def append_jsonl(path, row):
    """Appends one row; safe across threads and processes (flock, as `Sink` does)."""
    line = (json.dumps(row, ensure_ascii=False, sort_keys=True) + "\n").encode()
    if scan_secrets(line.decode()):
        raise RuntimeError(f"refusing to append to {path}: the row holds an API key")
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with _JSONL_LOCK, open(path, "ab") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        try:
            f.write(line)
            f.flush()
        finally:
            fcntl.flock(f, fcntl.LOCK_UN)


@contextlib.contextmanager
def file_lock(path):
    """An exclusive lock across processes: two runs (or `run.py --parallel` models)
    that need the same contract author it once; the second waits and reads the cache."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(f, fcntl.LOCK_UN)


def clean_transcript(lines):
    """The transcript as stored: each event without machine paths or session ids."""
    out = []
    for line in lines:
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if isinstance(ev, dict):
            for k in TRANSCRIPT_DROP:
                ev.pop(k, None)
        out.append(json.dumps(ev, ensure_ascii=False))
    return "\n".join(out) + ("\n" if out else "")


# ---------------------------------------------------------------- Admin API

def admin_call(api, method, path, body=None, query=None, timeout=60):
    """(status, parsed body or None) of a workspace Admin API call."""
    url = f"{api}{path}"
    if query:
        url += "?" + urllib.parse.urlencode(query)
    data = None
    headers = {"accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode()
        headers["content-type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            status, raw = r.status, r.read()
    except urllib.error.HTTPError as e:
        status, raw = e.code, e.read()
    try:
        return status, (json.loads(raw) if raw else None)
    except ValueError:
        return status, {"raw": raw[:500].decode(errors="replace")}


def world_path(mock, version="active"):
    return f"/mocks/{urllib.parse.quote(mock)}/contracts/{version}/world"


def read_world(api, mock):
    status, view = admin_call(api, "GET", world_path(mock))
    if status != 200 or not isinstance(view, dict):
        raise RuntimeError(f"get world of {mock}: HTTP {status} {view}")
    return view


def read_revisions(api, mock):
    """Every revision of the active version, oldest first, each with its document."""
    status, listed = admin_call(api, "GET", world_path(mock) + "/revisions")
    if status != 200:
        raise RuntimeError(f"list world revisions of {mock}: HTTP {status} {listed}")
    items = listed if isinstance(listed, list) else (listed or {}).get("revisions") or []
    out = []
    for info in sorted((i for i in items if isinstance(i, dict)), key=lambda i: i.get("revision") or 0):
        n = info.get("revision")
        status, rev = admin_call(api, "GET", world_path(mock) + f"/revisions/{n}")
        if status != 200 or not isinstance(rev, dict):
            raise RuntimeError(f"get world revision {n} of {mock}: HTTP {status} {rev}")
        out.append({"revision": n, "author": rev.get("author"), "created_at": rev.get("created_at"),
                    "document": rev.get("document")})
    return out


def put_world(api, mock, document, author):
    """(status, answer) of a PUT of the whole overlay."""
    return admin_call(api, "PUT", world_path(mock), body=document, query={"author": author})


# ---------------------------------------------------------------- authoring server

def wait_ready(admin, proc, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"authoring server exited with {proc.returncode}")
        try:
            with urllib.request.urlopen(f"{admin}/readyz", timeout=2) as r:
                if r.status == 200:
                    return
        except (urllib.error.URLError, OSError):
            pass
        time.sleep(0.3)
    raise RuntimeError("authoring server not ready")


class AuthoringServer:
    """`mockingbird serve` with one mock and no LLM profile."""

    def __init__(self, binary, contract, mock, workdir, ready_timeout=120):
        self.binary, self.contract, self.mock = binary, contract, mock
        self.dir, self.ready_timeout = workdir, ready_timeout
        self.proc = self.log = None

    def __enter__(self):
        import server_suites  # the bench's port lock and port picker
        env = {k: v for k, v in scrubbed_env().items() if k != "ANTHROPIC_API_KEY"}
        env.setdefault("RUST_LOG", "warn")
        self.log = open(os.path.join(self.dir, "server.log"), "w")
        try:
            for attempt in range(3):
                # Picked and bound under the lock server_suites uses, so no other
                # bench server takes a port between picking and binding it.
                with server_suites.port_reservation(self.ready_timeout + 30):
                    ports = server_suites.free_ports(3)
                    config = os.path.join(self.dir, "mockingbird.yaml")
                    with open(config, "w") as f:
                        f.write(f"storage:\n  url: sqlite://{self.dir}/mockingbird.db\n"
                                f"server:\n  port: {ports[0]}\n  admin_port: {ports[1]}\n"
                                f"  grpc_port: {ports[2]}\n"
                                f"mocks:\n  {self.mock}:\n    contract: {json.dumps(self.contract)}\n"
                                f"    route_prefix: /{self.mock}\n")
                    self.data = f"http://127.0.0.1:{ports[0]}"
                    self.admin = f"http://127.0.0.1:{ports[1]}"
                    self.api = f"{self.admin}/api/v1/workspaces/default"
                    self.proc = subprocess.Popen([self.binary, "serve", "-c", config], cwd=self.dir,
                                                 stdout=self.log, stderr=self.log, env=env)
                    try:
                        wait_ready(self.admin, self.proc, self.ready_timeout)
                    except RuntimeError:
                        self._stop()
                        if attempt == 2:
                            raise
                        continue
                if self._is_ours():
                    return self
                self._stop()
            raise RuntimeError("the authoring server's ports answered for another server")
        except BaseException:
            self.log.close()
            raise

    def _is_ours(self):
        """The admin port answers with exactly our one mock, not another process's server."""
        status, mocks = admin_call(self.api, "GET", "/mocks")
        items = mocks if isinstance(mocks, list) else (mocks or {}).get("mocks") or [] if isinstance(mocks, dict) else []
        names = sorted(m.get("name") for m in items if isinstance(m, dict))
        return status == 200 and names == [self.mock] and self.proc.poll() is None

    def _stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()

    def __exit__(self, *exc):
        self._stop()
        if self.log:
            self.log.close()


# ---------------------------------------------------------------- the agent

def mcp_tool_names(binary):
    """Every tool `mockingbird mcp` offers (tools/list over stdio; no server needed)."""
    msgs = [{"jsonrpc": "2.0", "id": 1, "method": "initialize",
             "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": {"name": "llm-bench", "version": "1"}}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}]
    proc = subprocess.run([binary, "--admin-url", "http://127.0.0.1:9", "mcp"], env=scrubbed_env(),
                          input="".join(json.dumps(m) + "\n" for m in msgs), capture_output=True, text=True,
                          timeout=60)
    for line in proc.stdout.splitlines():
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if isinstance(msg, dict) and msg.get("id") == 2:
            return [t["name"] for t in (msg.get("result") or {}).get("tools") or [] if isinstance(t, dict)]
    raise RuntimeError(f"mockingbird mcp listed no tools: {proc.stderr[-500:]}")


def claude_command(args, workdir, srv, contract_name, mock):
    mcp = {"mcpServers": {"mockingbird": {"command": args.binary,
                                          "args": ["--admin-url", srv.admin, "mcp"],
                                          "env": {"MB_DATA_URL": srv.data}}}}
    allowed = ",".join([BUILTIN_TOOLS] + [MCP_PREFIX + t for t in ALLOWED_MCP])
    others = [MCP_PREFIX + t for t in getattr(args, "mcp_tools", None) or [] if t not in ALLOWED_MCP]
    isolation = ["--bare"] if getattr(args, "author_isolation", "restricted") == "bare" else []
    return [
        args.claude, "-p", PROMPT.format(contract=contract_name, mock=mock),
        "--model", args.author_model,
        "--output-format", "stream-json", "--verbose",
        "--restricted", "--strict-mcp-config", "--mcp-config", json.dumps(mcp),
        "--tools", BUILTIN_TOOLS,
        "--allowedTools", allowed,
        "--permission-prompts", "none",
        "--no-session-persistence",
        "--max-budget-usd", str(args.author_budget),
    ] + (["--disallowedTools", ",".join(others)] if others else []) + isolation


def stub_command(args, workdir, srv, contract_name, mock):
    return [sys.executable, STUB_AUTHOR, "--binary", args.binary, "--admin-url", srv.admin, "--mock", mock,
            "--contract", contract_name]


def scrubbed_env():
    """os.environ without Mockingbird overrides and without any LLM key but
    Anthropic's (the author's own login): OPENROUTER_API_KEY, any variable holding
    its value, and every other *_API_KEY."""
    key = os.environ.get("OPENROUTER_API_KEY")
    return {k: v for k, v in os.environ.items()
            if not k.startswith("MB_") and not (key and v == key)
            and not (k.endswith("_API_KEY") and k != "ANTHROPIC_API_KEY")
            and k not in ("CLAUDECODE",) and not k.endswith("_SESSION_ID")}


def author_env(srv):
    """The agent's environment: the authoring server's admin and data URLs (so
    `call_mock` reaches it, not a server on the default port), no OpenRouter key."""
    env = scrubbed_env()
    env["MB_ADMIN_URL"] = srv.admin
    env["MB_DATA_URL"] = srv.data
    return env


def isolation_problems(init, author):
    """What the agent's session offers beyond the contract, the skills and the
    Mockingbird MCP server, from its `system/init` event: [] when nothing."""
    if author == "stub":
        return []
    if not isinstance(init, dict):
        return ["no init event in the transcript"]
    problems = []
    builtin = set(BUILTIN_TOOLS.split(","))
    extra = [t for t in init.get("tools") or [] if t not in builtin and not str(t).startswith(MCP_PREFIX)]
    if extra:
        problems.append(f"tools beyond Read, Grep, Glob and Mockingbird's: {sorted(extra)}")
    servers = [s.get("name") if isinstance(s, dict) else s for s in init.get("mcp_servers") or []]
    if [s for s in servers if s != "mockingbird"]:
        problems.append(f"MCP servers beyond mockingbird: {servers}")
    # Claude Code's own built-in plugins (source `<name>@builtin`) load in every
    # session; they add no tools (the tool check above holds) and are recorded in the
    # entry. Any other plugin fails the authoring.
    others = [p for p in init.get("plugins") or [] if not is_builtin_plugin(p)]
    if others:
        problems.append(f"plugins loaded: {others}")
    return problems


def is_builtin_plugin(plugin):
    return isinstance(plugin, dict) and str(plugin.get("source") or "").endswith("@builtin")


def builtin_plugins(init):
    """The names of the built-in plugins a session loaded, for the audit trail."""
    return sorted(str(p.get("name") or p.get("source")) for p in (init or {}).get("plugins") or []
                  if is_builtin_plugin(p))


def parse_transcript(lines):
    """From stream-json lines: the final result event, the init event, and every world
    write's answer: [{tool, ok, revision?, refused?, problem?}]."""
    result, init, uses, writes = None, None, {}, []
    for line in lines:
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if not isinstance(ev, dict):
            continue
        kind = ev.get("type")
        if kind == "system" and ev.get("subtype") == "init":
            init = ev
        elif kind == "result":
            result = ev
        content = ((ev.get("message") or {}).get("content")) if kind in ("assistant", "user") else None
        for block in content if isinstance(content, list) else []:
            if not isinstance(block, dict):
                continue
            if block.get("type") == "tool_use":
                name = str(block.get("name") or "")
                tool = name.rsplit("__", 1)[-1]
                if tool in WORLD_WRITES:
                    uses[block.get("id")] = tool
            elif block.get("type") == "tool_result" and block.get("tool_use_id") in uses:
                writes.append(write_answer(uses.pop(block.get("tool_use_id")), block))
    return result, init, writes


def write_answer(tool, block):
    text = block.get("content")
    if isinstance(text, list):
        text = "".join(str(c.get("text") or "") for c in text if isinstance(c, dict))
    text = str(text or "")
    row = {"tool": tool, "ok": not block.get("is_error")}
    try:
        answer = json.loads(text)
    except ValueError:
        answer = None
    if isinstance(answer, dict) and "revision" in answer and not block.get("is_error"):
        row["revision"] = answer.get("revision")
        row["refused"] = answer.get("refused") or []
    elif answer is None and not block.get("is_error"):
        # Not an error, but the answer is not JSON (cut short or saved aside by the
        # client): unknown, not failed. The stored revisions settle what happened.
        row["ok"] = None
        row["unparsed"] = text[:2000]
    else:
        row["ok"] = False
        row["problem"] = answer if isinstance(answer, dict) else text[:2000]
    return row


def author_contract(contract, args, sha=None):
    """Authors one contract's description and returns the entry (not yet cached). An
    author that fails (exit code, error result, timeout, a session that offers more
    than it should) gives an entry with `is_error` and `error`, which is never cached."""
    sha = sha or sha256_file(contract)
    mock = mock_name_of(contract)
    base = tempfile.mkdtemp(prefix="mb-world-author-")
    try:
        work = os.path.join(base, "work")
        srvdir = os.path.join(base, "server")
        os.makedirs(work)
        os.makedirs(srvdir)
        name = os.path.basename(contract)
        shutil.copyfile(contract, os.path.join(work, name))
        subprocess.run([args.binary, "agent", "skills", os.path.join(work, ".claude", "skills")],
                       check=True, capture_output=True, env=scrubbed_env())
        with AuthoringServer(args.binary, os.path.join(work, name), mock, srvdir) as srv:
            before = read_world(srv.api, mock)
            make = stub_command if args.author == "stub" else claude_command
            cmd = make(args, work, srv, name, mock)
            started = time.monotonic()
            error = None
            try:
                proc = subprocess.run(cmd, cwd=work, env=author_env(srv), capture_output=True, text=True,
                                      timeout=args.author_timeout)
                stdout, stderr, code = proc.stdout, proc.stderr, proc.returncode
            except subprocess.TimeoutExpired as e:
                stdout = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
                stderr = e.stderr.decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or "")
                code, error = None, f"timed out after {args.author_timeout:.0f}s"
            elapsed_ms = (time.monotonic() - started) * 1000
            lines = stdout.splitlines()
            result, init, writes = parse_transcript(lines)
            after = read_world(srv.api, mock)
            revisions = read_revisions(srv.api, mock)
        final = revisions[-1] if revisions else None
        result = result or {}
        problems = isolation_problems(init, args.author)
        if error is None and code != 0:
            error = f"exit code {code}"
        if error is None and result.get("is_error"):
            error = f"result {result.get('subtype')}"
        if error is None and not result:
            error = "no result event"
        if error is None and problems:
            error = "isolation: " + "; ".join(problems)
        entry = {
            "contract": rel(contract),
            "contract_sha256": sha,
            "mock": mock,
            "author": {"kind": args.author, "id": author_id(args.author, args.author_model),
                       "model_requested": None if args.author == "stub" else args.author_model,
                       "model": (init or {}).get("model"), "prompt_version": PROMPT_VERSION,
                       "isolation": None if args.author == "stub" else getattr(args, "author_isolation", None)},
            "inputs": dict(getattr(args, "author_inputs", None) or {}),
            "session": {k: (init or {}).get(k) for k in ("tools", "mcp_servers", "skills", "plugins",
                                                          "claude_code_version")},
            "builtin_plugins": builtin_plugins(init),
            "authored_at": utc_now(),
            "exit_code": code,
            "is_error": error is not None,
            "error": error,
            "cost_usd": result.get("total_cost_usd"),
            "duration_ms": result.get("duration_ms") or round(elapsed_ms),
            "wall_ms": round(elapsed_ms),
            "num_turns": result.get("num_turns"),
            "usage": result.get("usage"),
            "model_usage": result.get("modelUsage"),
            "summary": str(result.get("result") or "")[:4000],
            "stderr_tail": stderr[-2000:],
            "heuristic_gaps": before.get("gaps") or [],
            "writes": writes,
            "revisions": [{k: r[k] for k in ("revision", "author", "created_at")} for r in revisions],
            "revision": final["revision"] if final else 0,
            "document": final["document"] if final else None,
            "refused": after.get("refused") or [],
            "gaps": after.get("gaps") or [],
            "authors": after.get("authors") or {},
            "world": after.get("world"),
        }
        entry["_transcript"] = clean_transcript(lines)
        return entry
    finally:
        if not getattr(args, "keep", False):
            shutil.rmtree(base, ignore_errors=True)
        else:
            print(f"kept {base}", file=sys.stderr)


def summary_row(entry, cached, cache_file):
    writes = entry.get("writes") or []
    return {
        "ts": utc_now(),
        "contract": entry.get("contract"),
        "contract_sha256": entry.get("contract_sha256"),
        "mock": entry.get("mock"),
        "author_id": (entry.get("author") or {}).get("id"),
        "author_model": (entry.get("author") or {}).get("model"),
        "cached": cached,
        "is_error": entry.get("is_error"),
        "error": entry.get("error"),
        "cost_usd": entry.get("cost_usd"),
        "duration_ms": entry.get("duration_ms"),
        "num_turns": entry.get("num_turns"),
        "revision": entry.get("revision"),
        "writes": len(writes),
        "writes_failed": sum(1 for w in writes if w.get("ok") is False),
        "writes_unparsed": sum(1 for w in writes if w.get("ok") is None),
        "entries_refused_in_writes": sum(len(w.get("refused") or []) for w in writes),
        "refused_final": len(entry.get("refused") or []),
        "gaps_before": len(entry.get("heuristic_gaps") or []),
        "gaps_after": len(entry.get("gaps") or []),
        "cache_file": cache_file,
    }


def author_inputs(binary):
    """The sha256 of what the agent reads besides the contract: the skills Mockingbird
    installs and the world description's JSON Schema. A cached entry whose inputs
    differ is authored again (the vocabulary or the instructions changed)."""
    d = tempfile.mkdtemp(prefix="mb-world-skills-")
    try:
        subprocess.run([binary, "agent", "skills", d], check=True, capture_output=True, env=scrubbed_env())
        h = hashlib.sha256()
        for root, _, files in sorted(os.walk(d)):
            for name in sorted(files):
                path = os.path.join(root, name)
                h.update(os.path.relpath(path, d).encode() + b"\0")
                with open(path, "rb") as f:
                    h.update(f.read() + b"\0")
    finally:
        shutil.rmtree(d, ignore_errors=True)
    schema = subprocess.run([binary, "world", "schema"], check=True, capture_output=True, env=scrubbed_env())
    return {"skills_sha256": h.hexdigest(), "world_schema_sha256": hashlib.sha256(schema.stdout).hexdigest()}


def ensure_authored(contracts, args, out=None):
    """{contract path: entry} for every contract, authoring the ones not cached. A
    failed authoring is kept as `<sha>.failed.json` and returned (applying it fails the
    suite), never cached. One summary row per contract goes to
    <out>/world-authoring.jsonl."""
    aid = author_id(args.author, args.author_model)
    cache = args.world_cache
    args.author_inputs = author_inputs(args.binary)
    if args.author != "stub" and not getattr(args, "mcp_tools", None):
        args.mcp_tools = mcp_tool_names(args.binary)
    paths = list(dict.fromkeys(os.path.abspath(c) for c in contracts))
    shas = {c: sha256_file(c) for c in paths}
    by_sha = {}
    for c in paths:
        by_sha.setdefault(shas[c], c)

    def one(sha):
        contract = by_sha[sha]
        path = cache_path(cache, aid, sha)
        with file_lock(path[:-len(".json")] + ".lock"):
            entry = None if args.refresh else load_cached(cache, aid, sha, args.author_inputs)
            cached = entry is not None
            if not cached:
                print(f"world: authoring {rel(contract)} ({aid})", file=sys.stderr)
                entry = author_contract(contract, args, sha)
                transcript = entry.pop("_transcript", "")
                stem = path[:-len(".json")] + (".failed" if entry.get("is_error") else "")
                write_json_atomic(stem + ".json", entry)
                write_text_atomic(stem + ".transcript.jsonl", transcript)
        row = summary_row(entry, cached, rel(path))
        print(f"world: {row['mock']} revision {row['revision']}, {row['writes']} writes "
              f"({row['writes_failed']} failed, {row['entries_refused_in_writes']} entries refused), gaps "
              f"{row['gaps_before']} -> {row['gaps_after']}, ${row['cost_usd'] or 0:.2f}"
              f"{' (cached)' if cached else ''}{' ERROR ' + str(row['error']) if row['is_error'] else ''}",
              file=sys.stderr)
        if out:
            append_jsonl(os.path.join(out, "world-authoring.jsonl"), row)
        return sha, entry

    with ThreadPoolExecutor(max_workers=max(1, args.author_jobs)) as pool:
        by_entry = dict(pool.map(one, list(by_sha)))
    return {c: by_entry[shas[c]] for c in paths}


# ---------------------------------------------------------------- applying

def apply_world(api, mock, world, entry=None):
    """Writes the run's description onto a suite's mock. Returns the record kept in
    world-applied.jsonl: {mock, world, revision, refused, status, ...}."""
    rec = {"mock": mock, "world": world, "revision": 0, "refused": [], "status": None}
    if world == "none":
        return rec
    if world == "derived":
        document = read_world(api, mock).get("world")
        author = "user"
    else:
        if entry is None:
            raise RuntimeError(f"no authored description for {mock}")
        if entry.get("is_error"):
            raise RuntimeError(f"the description of {mock} failed to author: {entry.get('error')}")
        rec["author_id"] = (entry.get("author") or {}).get("id")
        rec["contract_sha256"] = entry.get("contract_sha256")
        rec["authored_revision"] = entry.get("revision")
        document = entry.get("document")
        author = "agent"
        if not document:
            # The agent wrote nothing: the mock keeps its heuristic description.
            rec["status"] = "nothing_authored"
            return rec
    status, answer = put_world(api, mock, document, author)
    rec["status"] = status
    if 200 <= status < 300 and isinstance(answer, dict):
        rec["revision"] = answer.get("revision")
        rec["refused"] = answer.get("refused") or []
    else:
        rec["problem"] = answer
        raise RuntimeError(f"PUT world of {mock} ({world}): HTTP {status} {json.dumps(answer)[:400]}")
    return rec


# ---------------------------------------------------------------- CLI

def add_author_args(p):
    p.add_argument("--author", choices=AUTHORS, default="claude",
                   help="who writes `--world agent` descriptions: claude (headless Claude Code, default) "
                        "or stub (free, deterministic, for dry runs)")
    p.add_argument("--author-model", default=DEFAULT_AUTHOR_MODEL,
                   help="claude --model for the author; a full id, as it is part of the cache key "
                        "(default %(default)s)")
    p.add_argument("--author-isolation", choices=("restricted", "bare"), default="restricted",
                   help="restricted (default): claude --restricted, which still reads the user's own "
                        "~/.claude/CLAUDE.md; bare: also --bare (no CLAUDE.md, memory or hooks), which needs "
                        "ANTHROPIC_API_KEY")
    p.add_argument("--author-budget", type=float, default=5.0, help="claude --max-budget-usd per contract")
    p.add_argument("--author-timeout", type=float, default=1800, help="seconds per contract")
    p.add_argument("--author-jobs", type=int, default=2, help="contracts authored at once")
    p.add_argument("--world-cache", default=DEFAULT_CACHE,
                   help="authored descriptions, per author id and contract sha256 (default %(default)s)")
    p.add_argument("--refresh", action="store_true", help="author again even when cached")
    p.add_argument("--claude", default="claude", help="the claude executable")


def resolve_paths(args):
    if not os.path.isabs(args.world_cache):
        args.world_cache = os.path.join(REPO, args.world_cache)
    if not os.path.exists(args.binary) and not os.path.isabs(args.binary):
        args.binary = os.path.join(REPO, args.binary)
    args.binary = os.path.abspath(args.binary)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--suite", action="append", default=[], help="server suite whose contracts to author")
    p.add_argument("--contract", action="append", default=[], help="a contract file to author")
    p.add_argument("--binary", default="target/release/mockingbird")
    p.add_argument("--out", help="also append summary rows to <out>/world-authoring.jsonl")
    p.add_argument("--keep", action="store_true", help="keep each authoring temp directory")
    add_author_args(p)
    args = p.parse_args()
    resolve_paths(args)
    if not os.path.exists(args.binary):
        sys.exit(f"{args.binary} not found: cargo build --release -p mockingbird-cli")
    contracts = [os.path.abspath(c) for c in args.contract]
    if args.suite:
        sys.path.insert(0, HERE)
        import server_suites  # noqa: E402
        for suite in args.suite:
            mocks, _ = server_suites.RUNNERS[suite]
            contracts += [os.path.join(REPO, c) for c, _, _ in mocks.values()]
    if not contracts:
        p.error("name a --suite or a --contract")
    ensure_authored(contracts, args, args.out)


if __name__ == "__main__":
    main()
