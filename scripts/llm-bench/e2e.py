#!/usr/bin/env python3
"""End-to-end eval: one prompt to a coding agent, then the hidden journeys against what it built.

    python3 scripts/llm-bench/e2e.py tasks --out DIR [--suite S ...]
    python3 scripts/llm-bench/e2e.py build --binary B --cache DIR [--suite S ...] [--jobs 3] [--cap 85]
    python3 scripts/llm-bench/e2e.py key --binary B
    python3 scripts/llm-bench/e2e.py report RUN_DIR [--stage1 KEY_DIR] [--paid DIR ...] [--json OUT]

Stage 1 (`build`): one task per contract of the dev suites (every journeys suite and
`consistency`, except SKIP). A task directory holds the contract (`contract.<ext>`), the
API's doc excerpts under `docs/` when the suite has any, and `PROMPT.md`: one fixed line
naming the vendor and the contract. It holds no journeys, no checks and no hint of what
will be tested. A headless Claude Code (`claude -p`, Sonnet by default) runs there with
the Mockingbird plugin built from the binary under test (`--plugin-dir`: the repository's
manifest and the skills `mockingbird agent skills` installs, loaded through the Skill
tool), that binary's MCP server (`mockingbird mcp`) as its only MCP server, and
Read/Grep/Glob confined to the task directory and the plugin (`--restricted`; no web
tools, no user or project settings, skills or plugins). An empty `mockingbird
serve` is already running for it with no LLM profile, so nothing it does spends on
OpenRouter. It may use every MCP tool except the ones that start, stop or reconfigure
servers, open a browser page or test a provider (BLOCKED). Caps: `--budget` dollars
(default 1) and `--max-turns` (default 60) per task.

When the agent stops, the server's state is exported with the product's own `mockingbird
snapshot export --include scenarios,chaos,limits,world` (mock settings, the active
contract, the newest world revision, scenarios, session fixtures). Each task records the
tool calls and their errors, refused world entries, retries (a call to a tool right
after that tool failed), the skill pages read, turns, cost and wall time, and what was
built (mocks, contract match, world, scenarios, sessions).

Results are cached under `<cache>/<key>/<task>/` (`result.json`, `snapshot/`, and
`transcript.jsonl`, which is never committed). The key hashes what the agent meets: the
skills, the MCP tool schemas, the prompt version, the model and the caps. The binary's
hash is recorded with each result and joins the key only with `--key-binary`, so an
engine change reuses the snapshots and only a skill, tool-schema or prompt change forces
a re-run. `--refresh` builds again.

Stage 2 is `server_suites.py --world e2e --e2e-cache <cache>/<key>`: each suite's server
starts with no mocks, and for every mock the suite's journeys use, the stage-1 snapshot of
that contract is imported with the product's `mockingbird snapshot import`, its mock
renamed to the name and route prefix the journeys call (see `load_snapshots`).
`fakegate.py --e2e-cache` adds this as the `e2e` tier.

`report` prints, per suite and tier: the fake pass, the projected rate, the floor and the
missed failures under the attribution rule (see `failure_class`: classifier agents'
`--attribution` first, then the deciding call's source, the curated tier's result on the
same binary, and the paid runs), else under the engine-decided rule (a failing check whose deciding call the engine answered,
`source` state or rejected, counts as floor; with `--paid`, a failing check counts as
model-dependent only if its deciding call reached the model and some paid run passed it,
else every model-reached failure is counted as model-dependent: an upper bound).

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
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import world_author as wa  # noqa: E402

PROMPT_VERSION = "e2e-1"
PROMPT = ("My app integrates with the {vendor} API. The contract is in ./{contract}{docs}. "
          "Set up a Mockingbird mock so I can develop and test the app against it.")
DOCS_CLAUSE = " and docs are in ./docs/"
# Suites the eval never builds: blind14 stays excluded, blind15 is kept for the final run.
SKIP = ("journeys_blind14", "journeys_blind15")
DEFAULT_MODEL = "claude-sonnet-5-5"
# MCP tools the agent may not call: they start, stop or reconfigure servers (the harness
# owns the server the agent works on), open a browser page, or call an LLM provider.
BLOCKED = ("start_server", "stop_server", "init_config", "configure_llm", "test_llm", "open_key_setup",
           "put_llm_profile", "delete_llm_profile")
# The deciding call of a failing check reached the model: only then can a real model change it.
MODEL_REACHED = ("llm", "llm-repaired", "repaired", "patched", "fallback")
# The MCP arguments that read a local file or fetch a URL (from the tools' schemas). Only
# these are audited: payloads (fixtures, world documents, scenarios, request bodies)
# carry URLs and absolute `path`/`url` values as data.
FILE_ARGS = {"import_contract": ("source", "urls"), "validate_contract": ("path",), "doctor": ("config",),
             "server_status": ("config",), "server_logs": ("config",), "proxy_env": ("config",)}
SNAPSHOT_PARTS = "scenarios,chaos,limits,world"
# A deciding call answered by the engine itself (no model call): its result is the same
# on a real model, so a failure there is floor.
ENGINE_DECIDED = ("state", "rejected")
# Read, Grep and Glob (confined to the task directory and the plugin by --restricted and
# --add-dir) and Skill, which loads the plugin's skills as a user's session would.
BUILTIN_TOOLS = "Read,Grep,Glob,Skill"
MCP_PREFIX = wa.MCP_PREFIX
PLUGIN_NAME = "mockingbird"
PLUGIN_MANIFEST = os.path.join(wa.REPO, "plugins", "mockingbird", ".claude-plugin", "plugin.json")


# ---- tasks ------------------------------------------------------------------------------

def dev_suites():
    import server_suites
    return [s for s in server_suites.RUNNERS if (s.startswith("journeys") or s == "consistency") and s not in SKIP]


def tasks(suites):
    """{contract sha256: task} for every contract the suites' journeys use, once each. A
    task's `uses` lists every (suite, mock, route prefix, persona) that runs it."""
    import server_suites
    out = {}
    for suite in suites:
        if suite in SKIP:
            raise SystemExit(f"e2e: {suite} is never built (SKIP)")
        for name, (contract, prefix, persona) in server_suites.RUNNERS[suite][0].items():
            path = os.path.join(wa.REPO, contract)
            sha = wa.sha256_file(path)
            t = out.setdefault(sha, {"id": f"{name}-{sha[:12]}", "mock": name, "contract": path, "sha": sha,
                                     "docs": docs_dir(path), "uses": []})
            t["uses"].append({"suite": suite, "mock": name, "prefix": prefix, "persona": persona})
    return out


def docs_dir(contract):
    """The API's doc excerpts next to the contract (`<stem>.docs/`), or None."""
    d = os.path.join(os.path.dirname(contract), os.path.basename(contract).split(".", 1)[0] + ".docs")
    return d if os.path.isdir(d) else None


def contract_name(path):
    """`contract.<ext>`, keeping every extension (`openapi.yaml`, `graphql`, `proto`)."""
    base = os.path.basename(path)
    return "contract." + (base.split(".", 1)[1] if "." in base else "txt")


def vendor_of(path, fallback):
    """The API's name for the prompt: the contract's `info.title` without a trailing "API",
    else the mock name capitalised."""
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            text = f.read(200_000)
    except OSError:
        text = ""
    title = None
    if text.lstrip().startswith("{"):
        try:
            title = (json.loads(text).get("info") or {}).get("title")
        except (ValueError, AttributeError):
            title = None
    else:
        m = re.search(r"(?m)^info:[ \t]*\n((?:[ \t]+.*\n|[ \t]*\n)*)", text)
        if m:
            t = re.search(r"(?m)^[ \t]+title:[ \t]*(.+?)[ \t]*$", m.group(1))
            title = t.group(1).strip().strip("'\"") if t else None
    title = re.sub(r"\s*\(.*\)\s*$", "", str(title or "")).strip()
    title = re.sub(r"^the\s+", "", title, flags=re.I)
    while True:  # trailing versions, "REST", "API(s)" and dashes: "GitLab REST API v4" -> "GitLab"
        shorter = re.sub(r"[\s\-–]+(v?\d+(\.\d+)*|REST|APIs?|-)$", "", title, flags=re.I).strip()
        if shorter == title:
            break
        title = shorter
    if not title:
        return fallback[:1].upper() + fallback[1:]
    words = [w.lower() for w in re.findall(r"[A-Za-z0-9]+", title)]
    compact = "".join(words)
    brand = fallback.lower()
    for w in words:  # "paypalpayments" titled "Payments" -> brand "paypal"
        if len(w) > 2 and brand.endswith(w) and brand != w:
            brand = brand[: -len(w)]

    def stem(a, b):
        n = 0
        while n < min(len(a), len(b)) and a[n] == b[n]:
            n += 1
        return n
    shares_stem = any(stem(w, fallback.lower()) >= 5 for w in words)
    if brand and brand not in compact and not shares_stem:
        title = f"{brand[:1].upper()}{brand[1:]} {title}"
    return title


def prompt_for(task):
    return PROMPT.format(vendor=vendor_of(task["contract"], task["mock"]), contract=contract_name(task["contract"]),
                         docs=DOCS_CLAUSE if task.get("docs") else "")


def write_task(task, dest):
    """The task directory: the contract, the docs if any, and PROMPT.md. Nothing else."""
    os.makedirs(dest, exist_ok=True)
    shutil.copyfile(task["contract"], os.path.join(dest, contract_name(task["contract"])))
    if task.get("docs"):
        shutil.copytree(task["docs"], os.path.join(dest, "docs"), dirs_exist_ok=True)
    with open(os.path.join(dest, "PROMPT.md"), "w") as f:
        f.write(prompt_for(task) + "\n")
    return dest


# ---- the cache key ----------------------------------------------------------------------

def mcp_tools(binary):
    """The full `tools/list` answer of `mockingbird mcp` (names, descriptions, schemas)."""
    msgs = [{"jsonrpc": "2.0", "id": 1, "method": "initialize",
             "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": {"name": "llm-bench", "version": "1"}}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}]
    proc = subprocess.run([binary, "--admin-url", "http://127.0.0.1:9", "mcp"], env=wa.scrubbed_env(),
                          input="".join(json.dumps(m) + "\n" for m in msgs), capture_output=True, text=True,
                          timeout=60)
    for line in proc.stdout.splitlines():
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if isinstance(msg, dict) and msg.get("id") == 2:
            return [t for t in (msg.get("result") or {}).get("tools") or [] if isinstance(t, dict)]
    raise RuntimeError(f"mockingbird mcp listed no tools: {proc.stderr[-500:]}")


def tree_sha256(d):
    h = hashlib.sha256()
    for root, dirs, files in os.walk(d):
        dirs.sort()
        for name in sorted(files):
            path = os.path.join(root, name)
            h.update(os.path.relpath(path, d).encode() + b"\0")
            with open(path, "rb") as f:
                h.update(f.read() + b"\0")
    return h.hexdigest()


def skills_sha256(binary):
    d = tempfile.mkdtemp(prefix="mb-e2e-skills-")
    try:
        subprocess.run([binary, "agent", "skills", d], check=True, capture_output=True, env=wa.scrubbed_env())
        return tree_sha256(d)
    finally:
        shutil.rmtree(d, ignore_errors=True)


def surface(binary):
    """What the agent meets, by hash: the skills, the MCP tool schemas, and the binary."""
    tools = mcp_tools(binary)
    return {"skills_sha256": skills_sha256(binary),
            "mcp_sha256": hashlib.sha256(json.dumps(tools, sort_keys=True).encode()).hexdigest(),
            "binary_sha256": wa.sha256_file(binary), "tools": [t.get("name") for t in tools]}


def cache_key(surf, model, budget, max_turns, key_binary=False):
    parts = {"skills": surf["skills_sha256"], "mcp": surf["mcp_sha256"], "prompt": PROMPT_VERSION,
             "model": model, "budget": budget, "max_turns": max_turns, "blocked": list(BLOCKED)}
    if key_binary:
        parts["binary"] = surf["binary_sha256"]
    return hashlib.sha256(json.dumps(parts, sort_keys=True).encode()).hexdigest()[:16]


# ---- stage 1 ----------------------------------------------------------------------------

class EmptyServer:
    """`mockingbird serve` with no mocks and no LLM profile, run from the task directory
    (where a user runs it), its config and database outside it."""

    def __init__(self, binary, workdir, cwd, ready_timeout=120):
        self.binary, self.dir, self.cwd, self.ready_timeout = binary, workdir, cwd, ready_timeout
        self.proc = self.log = None

    def __enter__(self):
        import server_suites
        env = {k: v for k, v in wa.scrubbed_env().items() if k != "ANTHROPIC_API_KEY"}
        env.setdefault("RUST_LOG", "warn")
        self.log = open(os.path.join(self.dir, "server.log"), "w")
        try:
            for attempt in range(3):
                with server_suites.port_reservation(self.ready_timeout + 30):
                    ports = server_suites.free_ports(3)
                    config = os.path.join(self.dir, "mockingbird.yaml")
                    with open(config, "w") as f:
                        f.write(f"storage:\n  url: sqlite://{self.dir}/mockingbird.db\n"
                                f"server:\n  port: {ports[0]}\n  admin_port: {ports[1]}\n"
                                f"  grpc_port: {ports[2]}\n")
                    self.data = f"http://127.0.0.1:{ports[0]}"
                    self.admin = f"http://127.0.0.1:{ports[1]}"
                    self.api = f"{self.admin}/api/v1/workspaces/default"
                    self.proc = subprocess.Popen([self.binary, "serve", "-c", config], cwd=self.cwd,
                                                 stdout=self.log, stderr=self.log, env=env)
                    try:
                        wa.wait_ready(self.admin, self.proc, self.ready_timeout)
                    except RuntimeError:
                        self._stop()
                        if attempt == 2:
                            raise
                        continue
                status, mocks = wa.admin_call(self.api, "GET", "/mocks")
                if status == 200 and mocks == []:
                    return self
                self._stop()
            raise RuntimeError("the e2e server's ports answered for another server")
        except BaseException:
            self.log.close()
            raise

    def _stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()

    def __exit__(self, *exc):
        self._stop()
        if self.log:
            self.log.close()


def make_plugin(binary, dest):
    """The Mockingbird plugin from the build under test, for this session only: the
    repository's Claude Code manifest and the skills the binary installs. Its MCP server
    is the harness's own `--mcp-config` entry (the same binary, pointed at the task's
    server), so the plugin carries none."""
    os.makedirs(os.path.join(dest, ".claude-plugin"))
    shutil.copyfile(PLUGIN_MANIFEST, os.path.join(dest, ".claude-plugin", "plugin.json"))
    subprocess.run([binary, "agent", "skills", os.path.join(dest, "skills")], check=True, capture_output=True,
                   env=wa.scrubbed_env())
    return dest


def isolation_problems(init, plugin_dir):
    """What the session offers beyond the task directory, Read/Grep/Glob/Skill, the
    Mockingbird MCP server and plugin, and Claude Code's built-in plugins: [] when nothing."""
    if not isinstance(init, dict):
        return ["no init event in the transcript"]
    problems = []
    builtin = set(BUILTIN_TOOLS.split(","))
    extra = [t for t in init.get("tools") or [] if t not in builtin and not str(t).startswith(MCP_PREFIX)]
    if extra:
        problems.append(f"tools beyond {BUILTIN_TOOLS} and Mockingbird's: {sorted(extra)}")
    servers = [x.get("name") if isinstance(x, dict) else x for x in init.get("mcp_servers") or []]
    if [x for x in servers if x != "mockingbird"]:
        problems.append(f"MCP servers beyond mockingbird: {servers}")
    ours = [p for p in init.get("plugins") or [] if isinstance(p, dict) and p.get("name") == PLUGIN_NAME
            and os.path.realpath(str(p.get("path") or "")) == os.path.realpath(plugin_dir)]
    if not ours:
        problems.append("the Mockingbird plugin did not load")
    others = [p for p in init.get("plugins") or [] if not wa.is_builtin_plugin(p) and p not in ours]
    if others:
        problems.append(f"plugins beyond mockingbird: {others}")
    return problems


def input_problems(lines, roots):
    """Tool inputs that reach outside the task: an http(s) URL to anything but this machine,
    or a file path (absolute, or with `..`) outside `roots` (the task and plugin
    directories). --restricted confines Read, Grep and Glob; this audits the MCP tools
    (`import_contract` reads local paths and URLs), so a run that escaped is not scored."""
    roots = [os.path.realpath(r) for r in roots]
    problems = []

    def check(tool, key, v):
        if isinstance(v, list):
            for x in v:
                check(tool, key, x)
            return
        if not isinstance(v, str):
            return
        m = re.match(r"(?i)^\s*([a-z][a-z0-9+.-]*)://([^/:?#]*)", v)
        if m:
            if not (m.group(1).lower() in ("http", "https") and m.group(2).lower() in ("127.0.0.1", "localhost", "[::1]")):
                problems.append(f"{tool}: {key}={v[:120]}")
            return
        path = os.path.expanduser(v.strip())
        real = os.path.realpath(path if os.path.isabs(path) else os.path.join(roots[0], path))
        if not any(real == r or real.startswith(r + os.sep) for r in roots):
            problems.append(f"{tool}: {key}={v[:120]}")

    def walk(tool, _key, inp):
        for key in FILE_ARGS.get(tool, ()):
            if isinstance(inp, dict) and key in inp:
                check(tool, key, inp[key])

    for line in lines:
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if not isinstance(ev, dict) or ev.get("type") != "assistant":
            continue
        for b in (ev.get("message") or {}).get("content") or []:
            if isinstance(b, dict) and b.get("type") == "tool_use" and str(b.get("name") or "").startswith(MCP_PREFIX):
                walk(str(b["name"])[len(MCP_PREFIX):], "", b.get("input") or {})
    return [f"tool input outside the task: {p}" for p in problems]


def claude_command(args, prompt, srv, tool_names, plugin_dir=None):
    mcp = {"mcpServers": {"mockingbird": {"command": args.binary, "args": ["--admin-url", srv.admin, "mcp"],
                                          "env": {"MB_DATA_URL": srv.data}}}}
    allowed = [BUILTIN_TOOLS] + [MCP_PREFIX + t for t in tool_names if t not in BLOCKED]
    blocked = [MCP_PREFIX + t for t in tool_names if t in BLOCKED]
    return [args.claude, "-p", prompt, "--model", args.model,
            "--output-format", "stream-json", "--verbose",
            "--restricted", "--strict-mcp-config", "--mcp-config", json.dumps(mcp),
            "--tools", BUILTIN_TOOLS, "--allowedTools", ",".join(allowed),
            "--permission-prompts", "none", "--no-session-persistence",
            "--max-budget-usd", str(args.budget), "--max-turns", str(args.max_turns)] + \
        (["--plugin-dir", plugin_dir, "--add-dir", plugin_dir] if plugin_dir else []) + \
        (["--disallowedTools", ",".join(blocked)] if blocked else [])


def text_of(block):
    c = block.get("content")
    if isinstance(c, list):
        c = "".join(str(x.get("text") or "") for x in c if isinstance(x, dict))
    return str(c or "")


def transcript_metrics(lines):
    """From stream-json lines: per-tool calls and errors, each failed call (tool and the
    start of its error), retries (a call to the tool that failed just before), the skill
    pages read, and the world entries refused in world-write answers."""
    calls, results = [], {}
    for line in lines:
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if not isinstance(ev, dict) or ev.get("type") not in ("assistant", "user"):
            continue
        for b in (ev.get("message") or {}).get("content") or []:
            if not isinstance(b, dict):
                continue
            if b.get("type") == "tool_use":
                calls.append((b.get("id"), str(b.get("name") or ""), b.get("input") or {}))
            elif b.get("type") == "tool_result":
                results[b.get("tool_use_id")] = b
    tools, errors, refused, skills = {}, [], [], []
    retries, last_failed = 0, None
    for cid, name, inp in calls:
        short = name[len(MCP_PREFIX):] if name.startswith(MCP_PREFIX) else name
        st = tools.setdefault(short, {"calls": 0, "errors": 0})
        st["calls"] += 1
        res = results.get(cid) or {}
        text = text_of(res)
        failed = bool(res.get("is_error"))
        if last_failed == short:
            retries += 1
        if failed:
            st["errors"] += 1
            errors.append({"tool": short, "error": text[:300]})
        last_failed = short if failed else None
        if short in wa.WORLD_WRITES and not failed:
            try:
                answer = json.loads(text)
            except ValueError:
                answer = None
            for r in (answer or {}).get("refused") or [] if isinstance(answer, dict) else []:
                if isinstance(r, dict):
                    refused.append({k: r.get(k) for k in ("path", "code", "message")})
        if short == "Skill":
            skills.append(str(inp.get("skill") or inp.get("name") or ""))
        elif short == "Read":
            m = re.search(r"/skills/(.+)$", str(inp.get("file_path") or ""))
            if m:
                skills.append(m.group(1))
    return {"tools": tools, "tool_calls": len(calls), "tool_errors": errors, "retries": retries,
            "skill_reads": skills, "refused_in_writes": refused}


def built_state(api):
    """What the agent left on the server: each mock with its active version, world
    (revisions, refused, gaps), scenarios and sessions."""
    status, mocks = wa.admin_call(api, "GET", "/mocks")
    out = []
    for m in mocks if status == 200 and isinstance(mocks, list) else []:
        name = m.get("name")
        q = "/mocks/" + wa.urllib.parse.quote(str(name))
        w = wa.read_world(api, name) if m.get("active_version") is not None else {}
        _, revs = wa.admin_call(api, "GET", wa.world_path(name) + "/revisions")
        _, scen = wa.admin_call(api, "GET", q + "/scenarios")
        _, sess = wa.admin_call(api, "GET", q + "/sessions")
        out.append({"name": name, "active_version": m.get("active_version"), "route_prefix": m.get("route_prefix"),
                    "world_revisions": len(revs) if isinstance(revs, list) else 0,
                    "world_refused": len(w.get("refused") or []), "world_gaps": len(w.get("gaps") or []),
                    "scenarios": len(scen) if isinstance(scen, list) else 0,
                    "sessions": len(sess) if isinstance(sess, list) else 0})
    return out


def usable(result):
    """Whether a stage-1 result is a build to score: no error (a session the caps ended
    is not an error) and a snapshot. An errored build (an infra failure, a timeout, an
    isolation problem, a failed export) is built again and never loaded."""
    return isinstance(result, dict) and not result.get("is_error") and result.get("snapshot") == "snapshot"


def cached_result(tdir):
    try:
        with open(os.path.join(tdir, "result.json")) as f:
            r = json.load(f)
    except (OSError, ValueError):
        return None
    return r if usable(r) and os.path.isdir(os.path.join(tdir, "snapshot")) else None


def run_task(task, args, keydir, surf):
    """Builds one task (or returns its usable cached result): the agent run, the snapshot
    export, and the result written to `<keydir>/<task id>/`."""
    tdir = os.path.join(keydir, task["id"])
    with wa.file_lock(tdir + ".lock"):
        cached = None if args.refresh else cached_result(tdir)
        if cached:
            cached["cached"] = True
            return cached
        base = tempfile.mkdtemp(prefix="mb-e2e-")
        try:
            work = write_task(task, os.path.join(base, "work"))
            srvdir = os.path.join(base, "server")
            os.makedirs(srvdir)
            plugin = make_plugin(args.binary, os.path.join(base, "plugin"))
            prompt = prompt_for(task)
            stage = tdir + ".tmp"
            shutil.rmtree(stage, ignore_errors=True)
            os.makedirs(stage)
            with EmptyServer(args.binary, srvdir, work) as srv:
                cmd = claude_command(args, prompt, srv, surf["tools"], plugin)
                started = time.monotonic()
                error = None
                try:
                    proc = subprocess.run(cmd, cwd=work, env=wa.author_env(srv), capture_output=True, text=True,
                                          timeout=args.timeout, stdin=subprocess.DEVNULL)
                    stdout, stderr, code = proc.stdout, proc.stderr, proc.returncode
                except subprocess.TimeoutExpired as e:
                    stdout = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
                    stderr = e.stderr.decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or "")
                    code, error = None, f"timed out after {args.timeout:.0f}s"
                wall_ms = round((time.monotonic() - started) * 1000)
                lines = stdout.splitlines()
                result, init, _ = wa.parse_transcript(lines)
                built = built_state(srv.api)
                exp = subprocess.run([args.binary, "snapshot", "export", "--admin-url", srv.admin,
                                      "--dir", os.path.join(stage, "snapshot"), "--include", SNAPSHOT_PARTS],
                                     capture_output=True, text=True, env=wa.scrubbed_env(), timeout=120)
            result = result or {}
            problems = isolation_problems(init, plugin) + input_problems(lines, [work, plugin])
            # The caps end a session with an error subtype; what it built still counts.
            capped = result.get("subtype") in ("error_max_turns", "error_max_budget_usd")
            if error is None and code != 0 and not capped:
                error = f"exit code {code}"
            if error is None and result.get("is_error") and not capped:
                error = f"result {result.get('subtype')}"
            if error is None and not result:
                error = "no result event"
            if error is None and problems:
                error = "isolation: " + "; ".join(problems)
            if exp.returncode != 0:
                error = (error + "; " if error else "") + f"snapshot export failed: {exp.stderr.strip()[-300:]}"
            entry = {
                "task": task["id"], "mock": task["mock"], "contract": wa.rel(task["contract"]),
                "contract_sha256": task["sha"], "uses": task["uses"], "prompt": prompt,
                "prompt_version": PROMPT_VERSION, "model_requested": args.model, "model": (init or {}).get("model"),
                "budget_usd": args.budget, "max_turns": args.max_turns,
                "skills_sha256": surf["skills_sha256"], "mcp_sha256": surf["mcp_sha256"],
                "binary_sha256": surf["binary_sha256"], "built_at": wa.utc_now(),
                "exit_code": code, "subtype": result.get("subtype"), "capped": capped,
                "is_error": error is not None, "error": error,
                "cost_usd": result.get("total_cost_usd"), "num_turns": result.get("num_turns"),
                "duration_ms": result.get("duration_ms"), "wall_ms": wall_ms,
                "usage": result.get("usage"), "session_tools": (init or {}).get("tools"),
                "session_skills": [k for k in (init or {}).get("skills") or [] if PLUGIN_NAME in str(k)],
                "built": built, "snapshot": "snapshot" if exp.returncode == 0 else None,
            }
            entry.update(transcript_metrics(lines))
            wa.write_json_atomic(os.path.join(stage, "result.json"), entry)
            wa.write_text_atomic(os.path.join(stage, "transcript.jsonl"), wa.clean_transcript(lines))
            os.chmod(os.path.join(stage, "transcript.jsonl"), 0o600)
            if stderr.strip():
                wa.write_text_atomic(os.path.join(stage, "stderr.txt"), stderr[-4000:])
            shutil.rmtree(tdir, ignore_errors=True)
            os.replace(stage, tdir)
            entry["cached"] = False
            return entry
        finally:
            shutil.rmtree(base, ignore_errors=True)


def charge(result, budget):
    """What a task run cost: its reported cost, else (a timeout, a crash, no result event)
    its whole budget, since the agent may have spent up to it."""
    if result.get("cached") or result.get("skipped"):
        return 0.0
    try:
        return float(result["cost_usd"])
    except (KeyError, TypeError, ValueError):
        return float(budget)


def build(args):
    args.binary = os.path.abspath(args.binary)
    suites = args.suite or dev_suites()
    ts = tasks(suites)
    if args.task:
        ts = {k: t for k, t in ts.items() if t["id"] in args.task or t["mock"] in args.task}
    surf = surface(args.binary)
    key = cache_key(surf, args.model, args.budget, args.max_turns, args.key_binary)
    root = os.path.realpath(args.cache)
    if root == wa.REPO or root.startswith(wa.REPO + os.sep):
        raise SystemExit(f"e2e: --cache {args.cache} is inside the repository; its transcripts must never be "
                         "committed, so keep the cache outside it")
    keydir = os.path.join(os.path.abspath(args.cache), key)
    os.makedirs(keydir, exist_ok=True)
    wa.write_json_atomic(os.path.join(keydir, "key.json"),
                         {"key": key, "skills_sha256": surf["skills_sha256"], "mcp_sha256": surf["mcp_sha256"],
                          "prompt_version": PROMPT_VERSION, "model": args.model, "budget_usd": args.budget,
                          "max_turns": args.max_turns, "blocked": list(BLOCKED), "key_binary": args.key_binary,
                          "binary_sha256": surf["binary_sha256"] if args.key_binary else None})
    print(f"e2e: {len(ts)} tasks from {len(suites)} suites, cache {keydir}", file=sys.stderr)
    lock, stop = threading.Lock(), threading.Event()
    ledger = {"spent": 0.0, "running": 0}

    def one(task):
        if not args.refresh:
            cached = cached_result(os.path.join(keydir, task["id"]))
            if cached:
                cached["cached"] = True
                return report_task(cached)
        with lock:
            # Room for every task in flight, this one included, at its full budget.
            if stop.is_set() or ledger["spent"] + args.budget * (ledger["running"] + 1) > args.cap:
                stop.set()
                return {"task": task["id"], "skipped": "cap"}
            ledger["running"] += 1
        try:
            r = run_task(task, args, keydir, surf)
        except Exception as e:  # one broken task must not stop the others
            r = {"task": task["id"], "is_error": True, "error": f"{type(e).__name__}: {e}"[:500]}
        with lock:
            ledger["running"] -= 1
            ledger["spent"] += charge(r, args.budget)
        return report_task(r)

    def report_task(r):
        print(f"e2e: {r.get('task')}: {'cached ' if r.get('cached') else ''}"
              f"${float(r.get('cost_usd') or 0):.2f}, {r.get('num_turns')} turns, "
              f"{len(r.get('tool_errors') or [])} tool errors, mocks {[m['name'] for m in r.get('built') or []]}"
              f"{' ERROR ' + str(r.get('error')) if r.get('is_error') else ''}", file=sys.stderr)
        return r

    with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        rows = list(pool.map(one, list(ts.values())))
    skipped = [r["task"] for r in rows if r.get("skipped")]
    print(f"e2e: spent ${ledger['spent']:.2f} this run (cap ${args.cap:.2f}; a run with no reported cost "
          f"counts its whole budget); "
          f"{sum(1 for r in rows if r.get('is_error'))} errors; {len(skipped)} skipped at the cap", file=sys.stderr)
    print(keydir)
    return 3 if skipped else 0


# ---- stage 2: loading snapshots ---------------------------------------------------------

def find_result(keydir, sha):
    """The task directory under keydir whose result is for contract `sha`, or None."""
    for d in sorted(os.listdir(keydir)) if os.path.isdir(keydir) else []:
        if d.endswith("-" + sha[:12]) and os.path.exists(os.path.join(keydir, d, "result.json")):
            return os.path.join(keydir, d)
    return None


def choose_mock(snap, sha):
    """The mock the agent built for the contract: the one whose exported contract is the
    task's, else the only mock, else the first (by name) with a world. (name, reason)."""
    mdir = os.path.join(snap, "mocks")
    mocks = {}
    for f in sorted(os.listdir(mdir)) if os.path.isdir(mdir) else []:
        if f.endswith(".json"):
            with open(os.path.join(mdir, f)) as fh:
                m = json.load(fh)
            if m.get("name"):
                mocks[m["name"]] = m
    if not mocks:
        return None, "no mock built"
    same = [n for n, m in mocks.items() if m.get("contract")
            and os.path.exists(os.path.join(snap, m["contract"]))
            and wa.sha256_file(os.path.join(snap, m["contract"])) == sha]
    if len(same) == 1:
        return same[0], "contract matches"
    pool = same or list(mocks)
    if len(pool) == 1:
        return pool[0], "only mock" if not same else "contract matches"
    with_world = [n for n in pool if os.path.exists(os.path.join(snap, "worlds", f"{n}.json"))]
    pick = (with_world or pool)[0]
    return pick, f"first of {len(pool)} mocks" + (" with a world" if with_world else "")


def rewrite(snap, pick, name, prefix, dest, bench=None):
    """A snapshot directory holding only the agent's mock `pick`, renamed to `name` with the
    route prefix the journeys call. Everything else is the agent's (its persona too: the
    suite's would hand the mock facts the journeys check). `bench` adds the harness's
    per-mock settings the other tiers get from the config (`llm_profile`, `faker_seed`)."""
    os.makedirs(os.path.join(dest, "mocks"))
    with open(os.path.join(snap, "mocks", f"{pick}.json")) as f:
        m = json.load(f)
    m["name"], m["route_prefix"] = name, prefix
    m.update(bench or {})
    if m.get("contract"):
        src = os.path.join(snap, m["contract"])
        stem = os.path.basename(src)[len(pick):] if os.path.basename(src).startswith(pick + ".") else None
        ext = stem[1:] if stem else (os.path.basename(src).split(".", 1)[1] if "." in os.path.basename(src) else "yaml")
        os.makedirs(os.path.join(dest, "contracts"), exist_ok=True)
        shutil.copyfile(src, os.path.join(dest, "contracts", f"{name}.{ext}"))
        m["contract"] = f"contracts/{name}.{ext}"
    with open(os.path.join(dest, "mocks", f"{name}.json"), "w") as f:
        json.dump(m, f, indent=2)
    for sub in ("scenarios", "sessions"):
        if os.path.isdir(os.path.join(snap, sub, pick)):
            shutil.copytree(os.path.join(snap, sub, pick), os.path.join(dest, sub, name))
    w = os.path.join(snap, "worlds", f"{pick}.json")
    if os.path.exists(w):
        os.makedirs(os.path.join(dest, "worlds"))
        shutil.copyfile(w, os.path.join(dest, "worlds", f"{name}.json"))
    return dest


LOADED = "imported"


def load_snapshots(srv, binary, keydir, suite, mocks, out=None, tag_journeys=False):
    """Stage 2: imports, for each of the suite's mocks, the stage-1 snapshot of its contract
    into the running server with `mockingbird snapshot import`, renamed to the mock the
    journeys call. Any other outcome leaves that mock unmocked, so every check of its
    journeys runs and fails: `no_snapshot` (no result), `build_error` (the result is not
    usable: rebuild it), `nothing_built` (the agent built no mock: the product gave the
    user nothing) and `import_failed`. Every status but `imported` and `nothing_built` is
    the harness's to fix, and `e2e.py report` lists them. One record per mock goes to
    <out>/world-applied.jsonl."""
    recs = []
    for name, (contract, prefix, _persona) in mocks.items():
        sha = wa.sha256_file(os.path.join(wa.REPO, contract))
        tdir = find_result(keydir, sha)
        rec = {"ts": wa.utc_now(), "suite": suite, "mock": name, "world": "e2e", "contract": contract,
               "contract_sha256": sha, "task": os.path.basename(tdir) if tdir else None}
        snap = os.path.join(tdir, "snapshot") if tdir else None
        if not tdir:
            rec["status"] = "no_snapshot"
        elif not cached_result(tdir):
            rec["status"] = "build_error"
        else:
            pick, why = choose_mock(snap, sha)
            rec.update({"agent_mock": pick, "chosen": why})
            if pick is None:
                rec["status"] = "nothing_built"
            else:
                tmp = tempfile.mkdtemp(prefix="mb-e2e-load-")
                try:
                    bench = {"faker_seed": 1, "llm_profile": f"bench_{name}"} if tag_journeys else None
                    d = rewrite(snap, pick, name, prefix, os.path.join(tmp, "snap"), bench)
                    rec["world_file"] = os.path.exists(os.path.join(d, "worlds", f"{name}.json"))
                    rec["scenarios"] = len(os.listdir(os.path.join(d, "scenarios", name))) \
                        if os.path.isdir(os.path.join(d, "scenarios", name)) else 0
                    rec["sessions"] = len(os.listdir(os.path.join(d, "sessions", name))) \
                        if os.path.isdir(os.path.join(d, "sessions", name)) else 0
                    p = subprocess.run([binary, "snapshot", "import", "--admin-url", srv.admin, "--dir", d],
                                       capture_output=True, text=True, env=wa.scrubbed_env(), timeout=300)
                finally:
                    shutil.rmtree(tmp, ignore_errors=True)
                rec["import_exit"] = p.returncode
                rec["import_output"] = (p.stdout + p.stderr).strip()[-1500:]
                rec["status"] = LOADED if p.returncode == 0 else "import_failed"
                m = re.search(r"\{.*\}", p.stdout, re.S)
                try:
                    summary = json.loads(m.group(0)) if m else {}
                except ValueError:
                    summary = {}
                refused = (summary.get("refused_world_entries") or {}).get(name) if isinstance(summary, dict) else None
                rec["refused"] = refused or []
                rec["refused_n"] = len(rec["refused"])
        if out:
            wa.append_jsonl(os.path.join(out, "world-applied.jsonl"), rec)
        print(f"  e2e {name}: {rec['status']}" + (f" ({rec.get('agent_mock')}, {rec.get('chosen')})"
                                                    if rec.get("agent_mock") else ""), file=sys.stderr)
        recs.append(rec)
    return recs


# ---- report ----------------------------------------------------------------------------

def run_rows(run):
    """{(tier, suite, case): row} of a run directory (server_suites or fakegate layout)."""
    rows = {}
    for root, _, files in os.walk(run):
        if "server.jsonl" not in files:
            continue
        tier_dir = os.path.relpath(root, run).split(os.sep)[0]
        with open(os.path.join(root, "server.jsonl")) as f:
            for line in f:
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                tier = r.get("world") or tier_dir
                rows[(tier, r.get("suite"), r.get("case"))] = r
    return rows


def paid_passes(dirs):
    """{(suite, case)} passed in any of the paid run directories."""
    seen = set()
    for d in dirs:
        for (_, suite, case), r in run_rows(d).items():
            if r.get("ok"):
                seen.add((suite, case))
    return seen


CLASSES = ("model", "missed", "engine", "inexpressible", "journey")


def failure_class(suite, case, r, paid=None, curated=None, attribution=None):
    """Why a failed check failed, and so whether a real model can recover it: `model` (only
    real model content passes it), `missed` (a complete description decides it), or a floor
    reason. In order: the attribution (classifier agents' verdict on the deciding call) when
    it names the check; `engine_decided` when the deciding call never reached the model;
    `missed` when the curated tier passes it; `engine_decided` when the curated tier decides
    it from state and still fails; `no_paid_pass` when no paid run passed it; else `model`,
    marked unattributed."""
    key = (suite, case)
    if attribution and key in attribution:
        return attribution[key], True
    if r.get("source") not in MODEL_REACHED:
        return "engine_decided", True
    if curated is not None:
        c = curated.get(key)
        if c and c.get("ok"):
            return "missed", True
        if c and c.get("source") not in MODEL_REACHED:
            return "engine_decided", True
    if paid is not None and key not in paid:
        return "no_paid_pass", True
    return "model", attribution is None


def summarize(rows, paid=None, curated=None, attribution=None):
    """Per (tier, suite): pass, total, model-dependent and floor. A failure is
    model-dependent only when [`failure_class`] says `model`: its deciding call reached the
    model, and neither the attribution, the curated tier nor the paid runs show it is
    decided otherwise. Every other failure is floor, `missed` ones counted apart.
    `unattributed` counts the model-dependent failures no attribution named (with an
    attribution given): the projection is an upper bound by that many. The curated tier
    (`agent`) is never judged against itself."""
    by = {}
    for (tier, suite, case), r in rows.items():
        s = by.setdefault((tier, suite), {"passed": 0, "total": 0, "model_dependent": [], "floor": [],
                                          "missed": [], "classes": {}, "unattributed": 0})
        s["total"] += 1
        if r.get("ok"):
            s["passed"] += 1
            continue
        # The curated tier is the reference, and attributions judge the other tiers' failures.
        ref, attr = (None, None) if tier == "agent" else (curated, attribution)
        cls, sure = failure_class(suite, case, r, paid, ref, attr)
        s["classes"][cls] = s["classes"].get(cls, 0) + 1
        if cls == "model":
            s["model_dependent"].append(case)
            if attr is not None and not sure:
                s["unattributed"] += 1
            continue
        s["floor"].append(case)
        if cls == "missed":
            s["missed"].append(case)
    for s in by.values():
        n = s["total"] or 1
        s["rate"] = round(100 * s["passed"] / n, 1)
        s["projected"] = round(100 * (s["passed"] + len(s["model_dependent"])) / n, 1)
    return by


def load_statuses(run):
    """{status: [(suite, mock, task)]} of a stage-2 run's world-applied.jsonl e2e records."""
    out = {}
    for root, _, files in os.walk(run):
        if "world-applied.jsonl" not in files:
            continue
        with open(os.path.join(root, "world-applied.jsonl")) as f:
            for line in f:
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                if r.get("world") == "e2e":
                    out.setdefault(r.get("status"), []).append((r.get("suite"), r.get("mock"), r.get("task")))
    return out


def stage1_summary(keydir):
    rows = []
    for d in sorted(os.listdir(keydir)) if keydir and os.path.isdir(keydir) else []:
        p = os.path.join(keydir, d, "result.json")
        if os.path.exists(p):
            with open(p) as f:
                r = json.load(f)
            rows.append({"task": d, "cost_usd": r.get("cost_usd"), "turns": r.get("num_turns"),
                         "wall_s": round((r.get("wall_ms") or 0) / 1000), "tool_calls": r.get("tool_calls"),
                         "tool_errors": len(r.get("tool_errors") or []), "retries": r.get("retries"),
                         "refused": len(r.get("refused_in_writes") or []),
                         "skills": sorted({s.split("/")[0] for s in r.get("skill_reads") or []}),
                         "mocks": [m["name"] for m in r.get("built") or []], "subtype": r.get("subtype"),
                         "error": r.get("error"), "binary_sha256": r.get("binary_sha256")})
    return rows


def load_attribution(paths):
    """{(suite, check): class} from classifier output files: lists of {suite, check,
    reclass | cls}. A later file wins."""
    out = {}
    for p in paths or []:
        with open(p) as f:
            for x in json.load(f):
                cls = x.get("reclass") or x.get("cls")
                if cls in CLASSES:
                    out[(x.get("suite"), x.get("check"))] = cls
    return out or None


def curated_rows(rows, run=None):
    """{(suite, case): row} of the curated (`agent`) tier: from `run` when given, else the
    report's own run."""
    src = run_rows(run) if run else rows
    got = {(s, c): r for (t, s, c), r in src.items() if t == "agent"}
    return got or None


def report(args):
    paid = paid_passes(args.paid) if args.paid else None
    rows = run_rows(args.run)
    curated = curated_rows(rows, args.curated)
    rep = summarize(rows, paid, curated, load_attribution(args.attribution))
    print(f"{'suite@tier':30} {'pass':>9} {'rate':>6} {'proj':>6} {'floor':>6} {'missed':>6} {'model':>6}")
    tot = {}
    for (tier, suite), s in sorted(rep.items(), key=lambda kv: (kv[0][0], kv[0][1])):
        un = f" ({s['unattributed']} unattributed)" if s["unattributed"] else ""
        print(f"{suite + '@' + tier:30} {s['passed']:>4}/{s['total']:<4} {s['rate']:>6} {s['projected']:>6} "
              f"{len(s['floor']):>6} {len(s['missed']):>6} {len(s['model_dependent']):>6}{un}")
        t = tot.setdefault(tier, [0, 0, 0, 0, 0, 0])
        t[0] += s["passed"]; t[1] += s["total"]; t[2] += len(s["model_dependent"]); t[3] += len(s["floor"])
        t[4] += len(s["missed"]); t[5] += s["unattributed"]
    for tier, (p, n, md, fl, mi, un) in sorted(tot.items()):
        print(f"{'ALL@' + tier:30} {p:>4}/{n:<4} {100 * p / max(n, 1):6.1f} {100 * (p + md) / max(n, 1):6.1f} "
              f"{fl:>6} {mi:>6} {md:>6}" + (f" ({un} unattributed: the projection is an upper bound by them)"
                                            if un else ""))
    if curated is None:
        print("no curated (agent) tier: missed failures are not told apart; pass --curated")
    s1 = stage1_summary(args.stage1)
    if s1:
        cost = sum(float(r["cost_usd"] or 0) for r in s1)
        print(f"\nstage 1: {len(s1)} tasks, ${cost:.2f}, "
              f"{sum(r['tool_errors'] for r in s1)} tool errors, {sum(r['retries'] or 0 for r in s1)} retries, "
              f"{sum(r['refused'] for r in s1)} refused world entries, "
              f"{sum(1 for r in s1 if not r['mocks'])} tasks built no mock, "
              f"{sum(1 for r in s1 if r['error'])} errors")
        bins = sorted({str(r["binary_sha256"])[:12] for r in s1})
        print(f"snapshots built on binary {', '.join(bins)}" + (" (MIXED)" if len(bins) > 1 else ""))
    loads = load_statuses(args.run)
    if loads:
        print("\nstage 2 loads: " + ", ".join(f"{k} {len(v)}" for k, v in sorted(loads.items(), key=lambda kv: str(kv[0]))))
        for status, rows in sorted(loads.items(), key=lambda kv: str(kv[0])):
            if status not in (LOADED, "nothing_built"):
                for suite, mock, task in rows:
                    print(f"  NOT SCORED FAIRLY ({status}): {suite}/{mock} {task or ''}")
    if args.json:
        with open(args.json, "w") as f:
            json.dump({"suites": {f"{s}@{t}": v for (t, s), v in rep.items()}, "stage1": s1,
                       "loads": {str(k): v for k, v in loads.items()}}, f, indent=1)
    return 0


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    t = sub.add_parser("tasks", help="write the task directories")
    t.add_argument("--out", required=True)
    t.add_argument("--suite", action="append")
    b = sub.add_parser("build", help="stage 1: the agent builds each task's mock")
    k = sub.add_parser("key", help="print the cache key of a binary's surface")
    for q in (b, k):
        q.add_argument("--binary", required=True)
        q.add_argument("--model", default=DEFAULT_MODEL)
        q.add_argument("--budget", type=float, default=1.0, help="claude --max-budget-usd per task")
        q.add_argument("--max-turns", type=int, default=60)
        q.add_argument("--key-binary", action="store_true", help="the binary's hash joins the cache key")
    b.add_argument("--cache", required=True, help="stage-1 cache root (results land in <cache>/<key>/)")
    b.add_argument("--suite", action="append")
    b.add_argument("--task", action="append", help="only these tasks (task id or mock name)")
    b.add_argument("--jobs", type=int, default=3)
    b.add_argument("--cap", type=float, default=85.0, help="stop starting tasks before this many dollars")
    b.add_argument("--timeout", type=float, default=1800.0, help="seconds per task")
    b.add_argument("--refresh", action="store_true")
    b.add_argument("--claude", default="claude")
    r = sub.add_parser("report", help="pass, projection and floor of a stage-2 run")
    r.add_argument("run")
    r.add_argument("--stage1", help="the stage-1 key directory, for the build summary")
    r.add_argument("--paid", action="append", help="a paid run directory (repeatable)")
    r.add_argument("--curated", help="a run with the curated (agent) tier on the same binary (default: this run's)")
    r.add_argument("--attribution", action="append",
                   help="classifier output, [{suite, check, reclass|cls}], naming each failure's class (repeatable)")
    r.add_argument("--json")
    a = p.parse_args(argv)
    if a.cmd == "tasks":
        for task in tasks(a.suite or dev_suites()).values():
            write_task(task, os.path.join(a.out, task["id"]))
            print(f"{task['id']}: {prompt_for(task)}")
        return 0
    if a.cmd == "key":
        surf = surface(os.path.abspath(a.binary))
        print(cache_key(surf, a.model, a.budget, a.max_turns, a.key_binary))
        return 0
    if a.cmd == "build":
        return build(a)
    return report(a)


if __name__ == "__main__":
    sys.exit(main())
