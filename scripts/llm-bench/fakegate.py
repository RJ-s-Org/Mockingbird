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

A branch or sha is checked out in its own worktree under `--work` and built there
(`cargo build --release -p mockingbird-cli`, into one target directory under `--work`
shared by its builds, one build at a time). A path to an executable is taken as the
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
TIERS = ("agent", "none")


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
        # A check the base passed that the candidate did not run at all is lost too.
        lost = sorted(k for k, v in b.items() if v.get("ok") and not c.get(k, {}).get("ok"))
        missing = [k for k in lost if k not in c]
        gained = sorted(k for k, v in c.items() if v.get("ok") and not b.get(k, {}).get("ok"))
        net = len(gained) - len(lost)
        report[tier] = {"base": sum(1 for v in b.values() if v.get("ok")), "cand": sum(1 for v in c.values() if v.get("ok")),
                        "checks": len(c), "lost": lost, "missing": missing, "gained": gained, "net": net,
                        "fails": net < -noise}
    return report


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


def build(sha, work):
    """The release binary of `sha`, built in its own worktree. Builds share one target
    directory under `work`, so a second build recompiles only the crates that changed;
    a lock keeps two gates from building into it at once, and the binary is copied out
    before the lock is released."""
    binary = os.path.join(work, "bin", f"mockingbird-{sha[:12]}")
    if os.access(binary, os.X_OK):
        return binary
    os.makedirs(os.path.dirname(binary), exist_ok=True)
    tree = checkout(sha, work)
    target = os.path.join(work, "target")
    env = dict(os.environ, CARGO_TARGET_DIR=target)
    with world_author.file_lock(os.path.join(work, "build.lock")):
        print(f"fakegate: building {sha[:12]}", file=sys.stderr)
        done = subprocess.run(["cargo", "build", "--release", "-q", "-p", "mockingbird-cli"], cwd=tree, env=env)
        if done.returncode != 0:
            raise SystemExit(f"{sha[:12]}: build failed")
        shutil.copy2(os.path.join(target, "release", "mockingbird"), binary + ".tmp")
        os.replace(binary + ".tmp", binary)
    return binary


def checkout(sha, work):
    tree = os.path.join(work, f"src-{sha[:12]}")
    with world_author.file_lock(os.path.join(work, "worktree.lock")):
        if not os.path.isdir(tree):
            git("worktree", "prune")
            git("worktree", "add", "--detach", "-q", tree, sha)
    return tree


def side_meta(tree, binary, suites, models, cache):
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
    return {"binary": world_author.sha256_file(binary), "tree": git("rev-parse", "HEAD", cwd=tree),
            "authors": groups, "agent_uncovered": uncovered, "inputs": inputs}


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
    return sorted(set(want) - have)


def run_side(name, tree, binary, work, suites, models, jobs, cache):
    """Every suite at both tiers on fake_llm.py; results under `run_dir`, reused when the
    run there finished. A lock keeps two gates from running the same side at once."""
    meta = side_meta(tree, binary, suites, models, cache)
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
    print(f"agent tier authors: {meta['authors']}; not on the agent tier (no cached descriptions): {meta['agent_uncovered']}")
    if (bmeta["authors"], bmeta["agent_uncovered"]) != (meta["authors"], meta["agent_uncovered"]):
        print(f"WARNING: the base took other descriptions: {bmeta['authors']}; uncovered {bmeta['agent_uncovered']}")
    for tier, r in report.items():
        print(f"\n[{tier}] {r['base']} -> {r['cand']} of {r['checks']}  (+{len(r['gained'])} / -{len(r['lost'])}, net {r['net']:+d})")
        for k in r["lost"]:
            print(f"  LOST   {k[1]}: {k[2]}{'  (not run by the candidate)' if k in r['missing'] else ''}")
        for k in r["gained"]:
            print(f"  gained {k[1]}: {k[2]}")
    bad = [t for t, r in report.items() if r["fails"]]
    print("\nVERDICT:", f"NET LOSS on {', '.join(bad)}" if bad else "no net loss")
    return 1 if bad else 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("candidate", help="a branch, a commit, or the path of a mockingbird binary")
    p.add_argument("--base", default="origin/main", help="branch or commit to compare with (default origin/main)")
    p.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "mb-fakegate"),
                   help="worktrees, binaries and results, reused between runs")
    p.add_argument("--suite", action="append", help="suites to run (default: every journeys suite and consistency)")
    p.add_argument("--author-models", default=",".join(DEFAULT_MODELS),
                   help="world-cache author models to take descriptions from, in order of preference")
    p.add_argument("--world-cache", default=os.path.join(REPO, world_author.DEFAULT_CACHE),
                   help="world descriptions for the agent tier, used by both sides")
    p.add_argument("--noise", type=int, default=1, help="net checks lost per tier still counted as noise (default 1)")
    p.add_argument("--jobs", type=int, default=4)
    p.add_argument("--json", help="also write the comparison here")
    a = p.parse_args()
    work = os.path.abspath(a.work)
    os.makedirs(work, exist_ok=True)
    try:
        git("fetch", "-q", "origin", check=False)
        base_sha = resolve(a.base)
        print(f"fakegate: base {a.base} = {base_sha[:12]}", file=sys.stderr)
        base_tree = checkout(base_sha, work)
        suites = a.suite or all_suites(base_tree)
        models = [m for m in a.author_models.split(",") if m]
        cache = os.path.abspath(a.world_cache)
        base_out, meta = run_side(base_sha[:12], base_tree, build(base_sha, work), work, suites, models, a.jobs, cache)
        if os.path.isfile(a.candidate) and os.access(a.candidate, os.X_OK):
            binary = os.path.abspath(a.candidate)
            name = "bin-" + world_author.sha256_file(binary)[:12]
            tree = base_tree
        else:
            sha = resolve(a.candidate)
            print(f"fakegate: candidate {a.candidate} = {sha[:12]} (a branch resolves to origin/<branch> first)",
                  file=sys.stderr)
            name, tree, binary = sha[:12], checkout(sha, work), build(sha, work)
        cand_out, cmeta = run_side(name, tree, binary, work, suites, models, a.jobs, cache)
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
            json.dump({"base": base_sha, "candidate": name, "meta": cmeta,
                       "tiers": {t: dict(r, lost=[list(k) for k in r["lost"]], gained=[list(k) for k in r["gained"]],
                                         missing=[list(k) for k in r["missing"]])
                                 for t, r in report.items()}}, f, indent=1)
    return print_report(report, name, base_sha, cmeta, meta, a.noise)


if __name__ == "__main__":
    sys.exit(main())
