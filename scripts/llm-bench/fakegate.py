#!/usr/bin/env python3
"""A free pull-request gate: the server suites on fake_llm.py, a candidate against a base.

    python3 scripts/llm-bench/fakegate.py <branch|sha|path-to-binary> [--base origin/main]
        [--work DIR] [--noise 1] [--suite S ...] [--json OUT]

Both sides run every suite (default: all journeys suites and `consistency`, except blind
sets that have not been diagnosed: see SKIP) at two tiers, with `fake_llm.py` answering
every LLM call, so a run costs no spend:

- `agent`: the world descriptions in `--world-cache` (default: this checkout's
  docs/benchmarks/world-cache, the same for both sides), cache only (nothing is
  authored). Each suite takes the first author model in `--author-models` whose cache
  holds a description for every one of its contracts; a suite with none is left out of
  this tier and named in the report.
- `none`: tier 0, no world description.
- `e2e` (with `--e2e-cache`): each mock is what a coding agent built from one prompt,
  loaded from a stage-1 snapshot (see e2e.py). Both sides load the same snapshots, so the
  tier measures the engine on fixed builds; a suite with a contract that has no snapshot
  is left out of it and named in the report.

A branch or sha is checked out in its own worktree under `--work` and built there
(`cargo build --release -p mockingbird-cli`, one build at a time) into the shared cargo
lane `$CARGO_TARGET_DIR` (default `~/.cache/mb-targets/lane-1`), never a target directory
of its own. When the gate finishes, it removes the worktrees no other gate is using and
keeps only what it reuses: the binaries and the results; `--keep-src` keeps the worktrees
for debugging. A path to an executable is taken as the
candidate binary and run with the base checkout's suites. Each side's results are kept
under `--work` and reused while its binary, checkout, contracts and descriptions are
unchanged, so gating several candidates against one main costs one base run. A check
the base passed that the candidate did not run counts as lost.

The report lists, for each tier, the pass counts and every check that went from pass to
fail (LOST) or from fail to pass (gained). The fake's answers are deterministic but the
engine is not entirely so, and a swing of `--noise` checks (default 1) in either
direction is counted as noise. Exit status: 0 when no tier lost more than `--noise`
checks net, 1 when one did, 2 on a setup error. A check passing on the fake LLM is one
the engine decides; the gate says nothing about checks that need a real model.
"""

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)

import world_author  # noqa: E402

# Blind sets kept unread until their headline run: never gated on.
SKIP = ("journeys_blind14",)
DEFAULT_MODELS = ("claude-sonnet-5-5", world_author.DEFAULT_AUTHOR_MODEL)
TIERS = ("agent", "none", "e2e")
# A worktree with no claim file was made by a fakegate from before claims: left alone while
# it may still be in use.
LEGACY_TREE_AGE_S = 6 * 3600
# A deciding call answered by the engine itself (no model call) gets the same answer on a real
# model, so a failing check with one of these sources is the executor's failure, not junk content.
ENGINE_DECIDED = ("state", "rejected")


def git(*args, cwd=REPO, check=True):
    out = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    if check and out.returncode != 0:
        raise SystemExit(f"git {' '.join(args)}: {out.stderr.strip()}")
    return out.stdout.strip()


def resolve(ref):
    """The full sha of a branch (as `origin/<ref>` first) or commit."""
    for cand in (f"origin/{ref}", ref):
        out = subprocess.run(["git", "rev-parse", "--verify", "--quiet", f"{cand}^{{commit}}"],
                             cwd=REPO, capture_output=True, text=True)
        if out.returncode == 0 and out.stdout.strip():
            return out.stdout.strip()
    raise SystemExit(f"{ref}: not a branch or commit")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def suite_contracts(checkout, suite):
    """The contract paths of a suite, as the checkout's server_suites.py names them."""
    code = ("import json,sys; sys.path.insert(0, 'scripts/llm-bench'); import server_suites as s; "
            f"m,_ = s.RUNNERS[{suite!r}]; print(json.dumps([c for c,_,_ in m.values()]))")
    out = subprocess.run([sys.executable, "-c", code], cwd=checkout, capture_output=True, text=True)
    if out.returncode != 0:
        raise SystemExit(f"{suite}: cannot list its contracts: {out.stderr.strip()[-300:]}")
    return [os.path.join(checkout, c) for c in json.loads(out.stdout)]


def all_suites(checkout):
    code = ("import json,sys; sys.path.insert(0, 'scripts/llm-bench'); import server_suites as s; "
            "print(json.dumps(list(s.SUITES)))")
    out = subprocess.run([sys.executable, "-c", code], cwd=checkout, capture_output=True, text=True)
    if out.returncode != 0:
        raise SystemExit(f"cannot list the suites: {out.stderr.strip()[-300:]}")
    return [s for s in json.loads(out.stdout) if (s.startswith("journeys") or s == "consistency") and s not in SKIP]


def author_for(contracts, cache, models):
    """The first author model whose cache holds a usable description of every contract, or None."""
    for model in models:
        aid = world_author.author_id("claude", model)
        if all(world_author.load_cached(cache, aid, world_author.sha256_file(c)) for c in contracts):
            return model
    return None


def compare(base, cand, noise=1):
    """Per tier: pass counts, LOST and gained checks, and whether the net loss exceeds `noise`.

    `base` and `cand` map (tier, suite, case) to a server.jsonl row."""
    report = {}
    for tier in TIERS:
        b = {k: v for k, v in base.items() if k[0] == tier}
        c = {k: v for k, v in cand.items() if k[0] == tier}
        if not b and not c:
            continue  # a tier neither side ran (e2e without --e2e-cache)
        # A check the base passed that the candidate did not run at all is lost too.
        lost = sorted(k for k, v in b.items() if v.get("ok") and not c.get(k, {}).get("ok"))
        missing = [k for k in lost if k not in c]
        gained = sorted(k for k, v in c.items() if v.get("ok") and not b.get(k, {}).get("ok"))
        net = len(gained) - len(lost)
        # Failing checks the executor decided, counted apart from the net: a junk-LLM fallback that
        # gains a check can hide an executor regression that loses another.
        eb, ec = executor_failed(b), executor_failed(c)
        report[tier] = {"exec_base": len(eb), "exec_cand": len(ec), "exec_new": sorted(ec - eb),
                        "exec_cleared": sorted(eb - ec),"base": sum(1 for v in b.values() if v.get("ok")), "cand": sum(1 for v in c.values() if v.get("ok")),
                        "checks": len(c), "lost": lost, "missing": missing, "gained": gained, "net": net,
                        "fails": net < -noise}
    return report


def executor_failed(rows):
    """Keys of the failing checks whose deciding call the engine answered."""
    return {k for k, v in rows.items() if not v.get("ok") and v.get("source") in ENGINE_DECIDED}


def load(run_dir):
    rows = {}
    for tier in TIERS:
        for root, _, files in os.walk(os.path.join(run_dir, tier)):
            if "server.jsonl" not in files:
                continue
            with open(os.path.join(root, "server.jsonl")) as f:
                for line in f:
                    try:
                        r = json.loads(line)
                    except ValueError:
                        continue
                    rows[(tier, r["suite"], r["case"])] = r
    return rows


def target_dir():
    """The cargo target directory builds go to: a shared lane, never one per checkout."""
    return os.path.abspath(os.environ.get("CARGO_TARGET_DIR") or os.path.expanduser("~/.cache/mb-targets/lane-1"))


def touch_sources(tree):
    """Marks every workspace source of `tree` as just changed. Cargo shares one fingerprint
    per workspace crate across every checkout that builds into a lane and decides staleness
    by mtime, so without this a build after another checkout's build is a no-op that leaves
    that checkout's binary in the lane."""
    now = time.time()
    paths = [os.path.join(tree, "Cargo.toml")]
    for root, dirs, files in os.walk(os.path.join(tree, "crates")):
        dirs[:] = [d for d in dirs if d not in ("target", "node_modules", ".git")]
        paths += [os.path.join(root, f) for f in files]
    for path in paths:
        with contextlib.suppress(FileNotFoundError):
            os.utime(path, (now, now))


def build(sha, work, claims):
    """The release binary of `sha`, built in its own worktree into the shared lane and
    copied out under `work`. The touch, the build and the copy hold one lock: a fakegate
    building another checkout between the build and the copy would otherwise leave its
    binary there to be copied. Other builds in the lane do not take the lock; the touch
    makes them see our sources as newer, and the window left is between our build and the
    copy."""
    binary = os.path.join(work, "bin", f"mockingbird-{sha[:12]}")
    if os.access(binary, os.X_OK):
        return binary
    os.makedirs(os.path.dirname(binary), exist_ok=True)
    tree = checkout(sha, work, claims)
    target = target_dir()
    built = os.path.join(target, "release", "mockingbird")
    env = dict(os.environ, CARGO_TARGET_DIR=target)
    with world_author.file_lock(os.path.join(target, "fakegate-build.lock")):
        print(f"fakegate: building {sha[:12]} into {target}", file=sys.stderr)
        touch_sources(tree)
        done = subprocess.run(["cargo", "build", "--release", "-q", "-p", "mockingbird-cli"], cwd=tree, env=env)
        if done.returncode != 0:
            raise SystemExit(f"{sha[:12]}: build failed")
        shutil.copy2(built, binary + ".tmp")
        os.replace(binary + ".tmp", binary)
    print(f"fakegate: {sha[:12]} binary sha256 {world_author.sha256_file(binary)}", file=sys.stderr)
    return binary


def checkout(sha, work, claims):
    """The worktree of `sha` under `work`. The gate holds a shared claim on it (a lock on
    `<tree>.use`, appended to `claims`) until it finishes, so another gate's cleanup leaves
    it in place; the claim is taken under the worktree lock, as the cleanup runs."""
    tree = os.path.join(work, f"src-{sha[:12]}")
    with world_author.file_lock(os.path.join(work, "worktree.lock")):
        if not os.path.isdir(tree):
            git("worktree", "prune")
            git("worktree", "add", "--detach", "-q", tree, sha)
        f = open(tree + ".use", "a")
        fcntl.flock(f, fcntl.LOCK_SH)
        claims.append(f)
    return tree


def cleanup(work, claims):
    """Drop this gate's claims, then remove every worktree under `work` that no gate claims,
    and the target directory older gates built into under `work`. The binaries (`bin/`) and
    the results (`run-*`) stay: later gates reuse them."""
    for f in claims:
        f.close()
    claims.clear()
    removed = []
    with world_author.file_lock(os.path.join(work, "worktree.lock")):
        for name in sorted(os.listdir(work)):
            tree = os.path.join(work, name)
            if not (name.startswith("src-") and os.path.isdir(tree)):
                continue
            if not os.path.exists(tree + ".use") and time.time() - os.path.getmtime(tree) < LEGACY_TREE_AGE_S:
                continue  # an older fakegate, which takes no claims, may still be using it
            with open(tree + ".use", "a") as f:
                try:
                    fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    continue  # another gate is using it
                git("worktree", "remove", "--force", tree, check=False)
                shutil.rmtree(tree, ignore_errors=True)
                os.remove(tree + ".use")
                removed.append(name)
        git("worktree", "prune", check=False)
    legacy = os.path.join(work, "target")
    if os.path.isdir(legacy):
        with open(os.path.join(work, "build.lock"), "a") as f:
            with contextlib.suppress(BlockingIOError):
                fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
                shutil.rmtree(legacy, ignore_errors=True)
                removed.append("target")
    return removed


def side_meta(tree, binary, suites, models, cache, e2e=None):
    """What a side's results depend on: the binary, the checkout whose suites ran, and for
    each suite its contracts and the description each took. Results are reused only when
    all of it is the same."""
    groups, uncovered, inputs = {}, [], {}
    for s in suites:
        contracts = suite_contracts(tree, s)
        model = author_for(contracts, cache, models)
        if model:
            groups.setdefault(model, []).append(s)
        else:
            uncovered.append(s)
        aid = world_author.author_id("claude", model) if model else None
        inputs[s] = {os.path.relpath(c, tree): [world_author.sha256_file(c),
                     world_author.sha256_file(world_author.cache_path(cache, aid, world_author.sha256_file(c))) if aid else None]
                     for c in contracts}
    meta = {"binary_sha256": world_author.sha256_file(binary), "tree": git("rev-parse", "HEAD", cwd=tree),
            "authors": groups, "agent_uncovered": uncovered, "inputs": inputs}
    if e2e:
        meta["e2e"] = e2e_meta(tree, suites, e2e)
    return meta


def e2e_meta(tree, suites, keydir):
    """The e2e tier's inputs: the stage-1 snapshot (a hash of its files) of every contract of
    each suite. A suite with a contract that has no usable build is left out of the tier."""
    if not os.path.exists(os.path.join(tree, "scripts", "llm-bench", "e2e.py")):
        # A checkout from before the e2e tier: it cannot run it.
        return {"cache": keydir, "suites": [], "uncovered": list(suites), "results": {},
                "missing": "this checkout has no e2e.py"}
    import e2e as e2e_mod
    covered, uncovered, results = [], [], {}
    for s in suites:
        got = {}
        for c in suite_contracts(tree, s):
            d = e2e_mod.find_result(keydir, world_author.sha256_file(c))
            ok = d and e2e_mod.cached_result(d)
            got[os.path.relpath(c, tree)] = e2e_mod.tree_sha256(os.path.join(d, "snapshot")) if ok else None
        (covered if all(got.values()) else uncovered).append(s)
        results[s] = got
    return {"cache": keydir, "suites": covered, "uncovered": uncovered, "results": results}


def reusable(done, meta):
    try:
        with open(done) as f:
            return json.load(f) == meta
    except (OSError, ValueError):
        return False


def wait_listening(port, proc, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return False
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return True
        time.sleep(0.1)
    return False


def run_dir(work, name, meta):
    """Results live in a directory named by the side and a hash of everything they depend
    on, so a run with other inputs (another suite list, another checkout or cache) never
    replaces results another gate is about to read."""
    digest = hashlib.sha256(json.dumps(meta, sort_keys=True).encode()).hexdigest()[:12]
    return os.path.join(work, f"run-{name}-{digest}")


def incomplete(rows, meta):
    """The (tier, suite) pairs a side should have run but has no rows for."""
    have = {(t, s) for t, s, _ in rows}
    want = [("agent", s) for ss in meta["authors"].values() for s in ss] + [("none", s) for s in meta["inputs"]]
    want += [("e2e", s) for s in (meta.get("e2e") or {}).get("suites", [])]
    return sorted(set(want) - have)


def run_side(name, tree, binary, work, suites, models, jobs, cache, e2e=None):
    """Every suite at both tiers on fake_llm.py; results under `run_dir`, reused when the
    run there finished. A lock keeps two gates from running the same side at once."""
    meta = side_meta(tree, binary, suites, models, cache, e2e)
    out = run_dir(work, name, meta)
    with world_author.file_lock(out + ".lock"):
        done = os.path.join(out, "done")
        if reusable(done, meta):
            return out, meta
        return run_fresh(out, done, meta, name, tree, binary, suites, jobs, cache), meta


def run_fresh(out, done, meta, name, tree, binary, suites, jobs, cache):
    groups = meta["authors"]
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    port = free_port()
    fake = subprocess.Popen([sys.executable, os.path.join(tree, "scripts", "llm-bench", "fake_llm.py"), "--port", str(port)],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if not wait_listening(port, fake):
        fake.kill()
        raise SystemExit(f"{name}: fake_llm.py did not start on port {port}")
    env = dict(os.environ, OPENROUTER_API_KEY="bench-dummy")
    common = [sys.executable, "scripts/llm-bench/server_suites.py", "--model", "bench/fake-llm",
              "--proxy-base", f"http://127.0.0.1:{port}", "--repeat", "1", "--jobs", str(jobs), "--suites-parallel",
              "--binary", binary, "--no-provider-check", "--provider-pins="]
    procs, logs = [], []
    try:
        for model, ss in groups.items():
            d = os.path.join(out, "agent", model)
            os.makedirs(d)
            cmd = common + ["--out", d, "--world", "agent", "--world-cache", cache, "--world-cache-only",
                            "--author-model", model] + [f"--suite={s}" for s in ss]
            logs.append(open(os.path.join(d, "run.log"), "w"))
            procs.append(subprocess.Popen(cmd, cwd=tree, env=env, stdout=logs[-1], stderr=subprocess.STDOUT))
        e2e = meta.get("e2e") or {}
        if e2e.get("suites"):
            d = os.path.join(out, "e2e")
            os.makedirs(d)
            cmd = common + ["--out", d, "--world", "e2e", "--e2e-cache", e2e["cache"]] + \
                [f"--suite={s}" for s in e2e["suites"]]
            logs.append(open(os.path.join(d, "run.log"), "w"))
            procs.append(subprocess.Popen(cmd, cwd=tree, env=env, stdout=logs[-1], stderr=subprocess.STDOUT))
        d = os.path.join(out, "none")
        os.makedirs(d)
        cmd = common + ["--out", d, "--world", "none"] + [f"--suite={s}" for s in suites]
        logs.append(open(os.path.join(d, "run.log"), "w"))
        procs.append(subprocess.Popen(cmd, cwd=tree, env=env, stdout=logs[-1], stderr=subprocess.STDOUT))
        codes = [p.wait() for p in procs]
        alive = fake.poll() is None
    finally:
        for p in procs:
            if p.poll() is None:
                p.terminate()
        fake.terminate()
        fake.wait()
        for f in logs:
            f.close()
    if any(codes):
        raise SystemExit(f"{name}: a suite run failed (logs under {out})")
    if not alive:
        raise SystemExit(f"{name}: fake_llm.py stopped during the run, so its results are not kept")
    with open(done, "w") as f:
        json.dump(meta, f)
    return out


def print_report(report, cand_name, base_sha, meta, bmeta, noise):
    print(f"fake-LLM gate: {cand_name} (tree {meta['tree'][:12]}) vs base {base_sha[:12]} (noise ±{noise})")
    print(f"binaries (sha256): candidate {meta['binary_sha256']}, base {bmeta['binary_sha256']}")
    print(f"agent tier authors: {meta['authors']}; not on the agent tier (no cached descriptions): {meta['agent_uncovered']}")
    if (bmeta["authors"], bmeta["agent_uncovered"]) != (meta["authors"], meta["agent_uncovered"]):
        print(f"WARNING: the base took other descriptions: {bmeta['authors']}; uncovered {bmeta['agent_uncovered']}")
    e2e = meta.get("e2e")
    if e2e:
        print(f"e2e tier: snapshots from {e2e['cache']}; not on the e2e tier: {e2e['uncovered']}"
              + (f" ({e2e['missing']})" if e2e.get("missing") else ""))
        if (bmeta.get("e2e") or {}).get("suites") != e2e["suites"]:
            print(f"WARNING: the base ran the e2e tier on other suites: {(bmeta.get('e2e') or {}).get('suites')}")
    for tier, r in report.items():
        print(f"\n[{tier}] {r['base']} -> {r['cand']} of {r['checks']}  (+{len(r['gained'])} / -{len(r['lost'])}, net {r['net']:+d})")
        for k in r["lost"]:
            print(f"  LOST   {k[1]}: {k[2]}{'  (not run by the candidate)' if k in r['missing'] else ''}")
        for k in r["gained"]:
            print(f"  gained {k[1]}: {k[2]}")
        print(f"[{tier}] executor-decided: {r['exec_base']} -> {r['exec_cand']} fail "
              f"(+{len(r['exec_new'])}/-{len(r['exec_cleared'])})")
        for k in r["exec_new"]:
            print(f"  EXEC-FAIL {k[1]}: {k[2]}")
    bad = [t for t, r in report.items() if r["fails"]]
    print("\nVERDICT:", f"NET LOSS on {', '.join(bad)}" if bad else "no net loss")
    return 1 if bad else 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("candidate", help="a branch, a commit, or the path of a mockingbird binary")
    p.add_argument("--base", default="origin/main", help="branch or commit to compare with (default origin/main)")
    p.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "mb-fakegate"),
                   help="worktrees (removed when the gate finishes), binaries and results (reused between runs)")
    p.add_argument("--suite", action="append", help="suites to run (default: every journeys suite and consistency)")
    p.add_argument("--author-models", default=",".join(DEFAULT_MODELS),
                   help="world-cache author models to take descriptions from, in order of preference")
    p.add_argument("--world-cache", default=os.path.join(REPO, world_author.DEFAULT_CACHE),
                   help="world descriptions for the agent tier, used by both sides")
    p.add_argument("--e2e-cache", help="the e2e tier: a stage-1 key directory (e2e.py build prints it); both "
                                        "sides load the same snapshots, so the tier measures the engine")
    p.add_argument("--noise", type=int, default=1, help="net checks lost per tier still counted as noise (default 1)")
    p.add_argument("--jobs", type=int, default=4)
    p.add_argument("--keep-src", action="store_true",
                   help="keep the source worktrees under --work when the gate finishes (for debugging)")
    p.add_argument("--json", help="also write the comparison here")
    a = p.parse_args()
    work = os.path.abspath(a.work)
    os.makedirs(work, exist_ok=True)
    claims = []
    try:
        return gate(a, work, claims)
    finally:
        if a.keep_src:
            for f in claims:
                f.close()
        else:
            try:
                removed = cleanup(work, claims)
            except (OSError, SystemExit) as e:
                print(f"fakegate: cleaning up {work} failed: {e}", file=sys.stderr)
            else:
                if removed:
                    print(f"fakegate: removed {', '.join(removed)} under {work}", file=sys.stderr)


def gate(a, work, claims):
    try:
        git("fetch", "-q", "origin", check=False)
        base_sha = resolve(a.base)
        print(f"fakegate: base {a.base} = {base_sha[:12]}", file=sys.stderr)
        base_tree = checkout(base_sha, work, claims)
        suites = a.suite or all_suites(base_tree)
        models = [m for m in a.author_models.split(",") if m]
        cache = os.path.abspath(a.world_cache)
        e2e = os.path.abspath(a.e2e_cache) if a.e2e_cache else None
        if e2e and not os.path.isdir(e2e):
            raise SystemExit(f"--e2e-cache {e2e} is not a directory")
        base_bin = build(base_sha, work, claims)
        base_out, meta = run_side(base_sha[:12], base_tree, base_bin, work, suites, models, a.jobs, cache, e2e)
        if os.path.isfile(a.candidate) and os.access(a.candidate, os.X_OK):
            binary = os.path.abspath(a.candidate)
            name = "bin-" + world_author.sha256_file(binary)[:12]
            tree = base_tree
        else:
            sha = resolve(a.candidate)
            print(f"fakegate: candidate {a.candidate} = {sha[:12]} (a branch resolves to origin/<branch> first)",
                  file=sys.stderr)
            name, tree, binary = sha[:12], checkout(sha, work, claims), build(sha, work, claims)
        cand_out, cmeta = run_side(name, tree, binary, work, suites, models, a.jobs, cache, e2e)
    except SystemExit as e:
        print(f"fakegate: {e}", file=sys.stderr)
        return 2
    base_rows, cand_rows = load(base_out), load(cand_out)
    for side, rows, m, d in (("base", base_rows, meta, base_out), ("candidate", cand_rows, cmeta, cand_out)):
        gaps = incomplete(rows, m)
        if gaps:
            print(f"fakegate: the {side}'s results under {d} have no rows for {gaps}; not comparing", file=sys.stderr)
            return 2
    report = compare(base_rows, cand_rows, a.noise)
    if a.json:
        with open(a.json, "w") as f:
            json.dump({"base": base_sha, "candidate": name, "meta": cmeta, "base_meta": meta,
                       "tiers": {t: dict(r, lost=[list(k) for k in r["lost"]], gained=[list(k) for k in r["gained"]],
                                         missing=[list(k) for k in r["missing"]],
                                         exec_new=[list(k) for k in r["exec_new"]],
                                         exec_cleared=[list(k) for k in r["exec_cleared"]])
                                 for t, r in report.items()}}, f, indent=1)
    return print_report(report, name, base_sha, cmeta, meta, a.noise)


if __name__ == "__main__":
    sys.exit(main())
