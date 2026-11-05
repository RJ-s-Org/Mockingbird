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
SECRET_MARK = re.compile(r"sk-or-[A-Za-z0-9_-]{8,}")

# The Mockingbird MCP tools the author may call: reading the mock and its world,
# writing the world, and calling the mock to check a description. Everything else
# (imports, deletes, servers, LLM profiles, key setup) is refused.
ALLOWED_MCP = ("list_mocks", "get_mock", "list_contract_versions", "get_world", "get_world_schema",
               "patch_world", "put_world", "list_world_revisions", "get_world_revision", "diff_world",
               "restore_world_revision", "call_mock", "list_requests", "get_request")
WORLD_WRITES = ("patch_world", "put_world", "restore_world_revision")
BUILTIN_TOOLS = "Read,Grep,Glob"

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


def load_cached(cache, aid, sha):
    path = cache_path(cache, aid, sha)
    try:
        with open(path) as f:
            entry = json.load(f)
    except (OSError, ValueError):
        return None
    return entry if isinstance(entry, dict) and entry.get("contract_sha256") == sha else None


def scan_secrets(text):
    """True when text holds what looks like an OpenRouter key."""
    return bool(SECRET_MARK.search(text or ""))


def write_json_atomic(path, obj):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    text = json.dumps(obj, indent=2, ensure_ascii=False, sort_keys=True) + "\n"
    if scan_secrets(text):
        raise RuntimeError(f"refusing to write {path}: it holds an sk-or- key")
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".tmp-")
    with os.fdopen(fd, "w") as f:
        f.write(text)
    os.replace(tmp, path)


_JSONL_LOCK = threading.Lock()


def append_jsonl(path, row):
    line = json.dumps(row, ensure_ascii=False, sort_keys=True)
    if scan_secrets(line):
        raise RuntimeError(f"refusing to append to {path}: the row holds an sk-or- key")
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with _JSONL_LOCK, open(path, "a") as f:
        f.write(line + "\n")


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

def free_ports(n):
    import socket
    socks, ports = [], []
    try:
        for _ in range(n):
            s = socket.socket()
            s.bind(("127.0.0.1", 0))
            socks.append(s)
            ports.append(s.getsockname()[1])
    finally:
        for s in socks:
            s.close()
    return ports


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
        env = {k: v for k, v in os.environ.items()
               if not k.startswith("MB_") and k not in ("OPENROUTER_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY")}
        env.setdefault("RUST_LOG", "warn")
        self.log = open(os.path.join(self.dir, "server.log"), "w")
        for attempt in range(3):
            ports = free_ports(3)
            config = os.path.join(self.dir, "mockingbird.yaml")
            with open(config, "w") as f:
                f.write(f"storage:\n  url: sqlite://{self.dir}/mockingbird.db\n"
                        f"server:\n  port: {ports[0]}\n  admin_port: {ports[1]}\n  grpc_port: {ports[2]}\n"
                        f"mocks:\n  {self.mock}:\n    contract: {json.dumps(self.contract)}\n"
                        f"    route_prefix: /{self.mock}\n")
            self.admin = f"http://127.0.0.1:{ports[1]}"
            self.api = f"{self.admin}/api/v1/workspaces/default"
            self.proc = subprocess.Popen([self.binary, "serve", "-c", config], cwd=self.dir,
                                         stdout=self.log, stderr=self.log, env=env)
            try:
                wait_ready(self.admin, self.proc, self.ready_timeout)
                return self
            except RuntimeError:
                self._stop()
                if attempt == 2:
                    self.log.close()
                    raise
        return self

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

def claude_command(args, workdir, admin, contract_name, mock):
    mcp = {"mcpServers": {"mockingbird": {"command": args.binary, "args": ["--admin-url", admin, "mcp"]}}}
    allowed = ",".join([BUILTIN_TOOLS] + [f"mcp__mockingbird__{t}" for t in ALLOWED_MCP])
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
    ]


def stub_command(args, workdir, admin, contract_name, mock):
    return [sys.executable, STUB_AUTHOR, "--binary", args.binary, "--admin-url", admin, "--mock", mock,
            "--contract", contract_name]


def author_env(admin):
    """The agent's environment: no OpenRouter key and no Mockingbird overrides."""
    env = {k: v for k, v in os.environ.items()
           if not k.startswith("MB_") and k not in ("OPENROUTER_API_KEY", "CLAUDECODE")
           and not k.endswith("_SESSION_ID")}
    env["MB_ADMIN_URL"] = admin
    return env


def parse_transcript(lines):
    """From stream-json lines: the final result event, the model, and every world
    write's answer: [{tool, ok, revision?, refused?, problem?}]."""
    result, model, uses, writes = None, None, {}, []
    for line in lines:
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if not isinstance(ev, dict):
            continue
        kind = ev.get("type")
        if kind == "system" and ev.get("subtype") == "init":
            model = ev.get("model") or model
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
    return result, model, writes


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
    else:
        row["ok"] = False
        row["problem"] = answer if isinstance(answer, dict) else text[:2000]
    return row


def author_contract(contract, args, sha=None):
    """Authors one contract's description and returns the cache entry (not written)."""
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
                       check=True, capture_output=True, env=author_env(""))
        with AuthoringServer(args.binary, os.path.join(work, name), mock, srvdir) as srv:
            before = read_world(srv.api, mock)
            make = stub_command if args.author == "stub" else claude_command
            cmd = make(args, work, srv.admin, name, mock)
            started = time.monotonic()
            proc = subprocess.run(cmd, cwd=work, env=author_env(srv.admin), capture_output=True, text=True,
                                  timeout=args.author_timeout)
            elapsed_ms = (time.monotonic() - started) * 1000
            lines = proc.stdout.splitlines()
            result, model, writes = parse_transcript(lines)
            after = read_world(srv.api, mock)
            revisions = read_revisions(srv.api, mock)
        final = revisions[-1] if revisions else None
        result = result or {}
        entry = {
            "contract": os.path.relpath(contract, REPO) if contract.startswith(REPO) else contract,
            "contract_sha256": sha,
            "mock": mock,
            "author": {"kind": args.author, "id": author_id(args.author, args.author_model),
                       "model_requested": None if args.author == "stub" else args.author_model,
                       "model": model, "prompt_version": PROMPT_VERSION},
            "authored_at": utc_now(),
            "exit_code": proc.returncode,
            "is_error": bool(result.get("is_error")) or proc.returncode != 0,
            "cost_usd": result.get("total_cost_usd"),
            "duration_ms": result.get("duration_ms") or round(elapsed_ms),
            "wall_ms": round(elapsed_ms),
            "num_turns": result.get("num_turns"),
            "usage": result.get("usage"),
            "model_usage": result.get("modelUsage"),
            "summary": str(result.get("result") or "")[:4000],
            "stderr_tail": proc.stderr[-2000:],
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
        transcript = "\n".join(lines)
        if scan_secrets(transcript):
            raise RuntimeError(f"the authoring transcript of {contract} holds an sk-or- key; not kept")
        entry["_transcript"] = transcript
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
        "cost_usd": entry.get("cost_usd"),
        "duration_ms": entry.get("duration_ms"),
        "num_turns": entry.get("num_turns"),
        "revision": entry.get("revision"),
        "writes": len(writes),
        "writes_failed": sum(1 for w in writes if not w.get("ok")),
        "entries_refused_in_writes": sum(len(w.get("refused") or []) for w in writes),
        "refused_final": len(entry.get("refused") or []),
        "gaps_before": len(entry.get("heuristic_gaps") or []),
        "gaps_after": len(entry.get("gaps") or []),
        "cache_file": cache_file,
    }


def ensure_authored(contracts, args, out=None):
    """{contract path: cache entry} for every contract, authoring the ones not cached.
    One summary row per contract goes to <out>/world-authoring.jsonl."""
    aid = author_id(args.author, args.author_model)
    cache = args.world_cache
    unique = list(dict.fromkeys(os.path.abspath(c) for c in contracts))
    shas = {c: sha256_file(c) for c in unique}
    results = {}

    def one(contract):
        sha = shas[contract]
        path = cache_path(cache, aid, sha)
        entry = None if args.refresh else load_cached(cache, aid, sha)
        cached = entry is not None
        if not cached:
            print(f"world: authoring {os.path.relpath(contract, REPO)} ({aid})", file=sys.stderr)
            entry = author_contract(contract, args, sha)
            transcript = entry.pop("_transcript", "")
            write_json_atomic(path, entry)
            with open(path[:-len(".json")] + ".transcript.jsonl", "w") as f:
                f.write(transcript + ("\n" if transcript else ""))
        row = summary_row(entry, cached, os.path.relpath(path, REPO) if path.startswith(REPO) else path)
        print(f"world: {row['mock']} revision {row['revision']}, {row['writes']} writes "
              f"({row['writes_failed']} failed, {row['entries_refused_in_writes']} entries refused), gaps {row['gaps_before']} -> {row['gaps_after']}, "
              f"${row['cost_usd'] or 0:.2f}{' (cached)' if cached else ''}", file=sys.stderr)
        if out:
            append_jsonl(os.path.join(out, "world-authoring.jsonl"), row)
        return contract, entry

    with ThreadPoolExecutor(max_workers=max(1, args.author_jobs)) as pool:
        for contract, entry in pool.map(one, unique):
            results[contract] = entry
    return results


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
    p.add_argument("--author-model", default="opus", help="claude --model for the author (default opus)")
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
