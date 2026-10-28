#!/usr/bin/env python3
"""Latency bench over the field-trial contracts, against a real LLM.

Starts `mockingbird serve` on a throwaway copy of each trial's mocks
(fresh SQLite, no proxy, no webhooks), sends each case RUNS times in fresh
sessions, then once more in the last session (a repeat, which consistency
answers from the cache), and prints a Markdown table of latency, sources
and per-case checks.

    source ~/.zshenv >/dev/null 2>&1   # OPENROUTER_API_KEY, never printed
    scripts/trial-bench.py --trials ../mockingbird-trials --runs 5

The trials directory holds `github-readme-stats/`, `movies/` and
`accept-a-payment/` as the 2026-10 field trials left them. Only the standard
library is used.
"""

import argparse
import json
import os
import re
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

PROFILE = """llm:
  default_profile: openrouter
  profiles:
    openrouter:
      api_key: env:OPENROUTER_API_KEY
      base_url: https://openrouter.ai/api/v1
      headers:
        http-referer: https://github.com/rj-s-org/mockingbird
        x-title: Mockingbird
      model: {model}
"""

PIN_QUERY = """
fragment RepoInfo on Repository {
  name nameWithOwner isPrivate isArchived isTemplate
  stargazers { totalCount }
  description
  primaryLanguage { color id name }
  forkCount
}
query getRepo($login: String!, $repo: String!) {
  user(login: $login) { repository(name: $repo) { ...RepoInfo } }
  organization(login: $login) { repository(name: $repo) { ...RepoInfo } }
}
"""

STATS_QUERY = """
query userInfo($login: String!) {
  user(login: $login) {
    name login
    commits: contributionsCollection { totalCommitContributions }
    pullRequests(first: 1) { totalCount }
    openIssues: issues(states: OPEN) { totalCount }
    followers { totalCount }
  }
}
"""

TMDB_PERSONA = (
    "The Movie Database (TMDB) v3 API. Use real, well-known movies, TV shows and people with "
    "their actual titles, names, years, overviews and plausible TMDB ids. Image paths look like "
    "TMDB's: '/' + 27 alphanumeric chars + '.jpg'. Lists return 10 results per page. Video keys "
    "are 11-char YouTube ids. Dates are YYYY-MM-DD. vote_average is 0-10."
)

STRIPE_PERSONA = (
    "Stripe API in test mode. Return complete Stripe objects with all top-level fields. Ids use "
    "Stripe prefixes (pi_, cus_, pm_) followed by 24 random alphanumeric chars, unique per object. "
    'A PaymentIntent always has client_secret = "<id>_secret_<24 random chars>", livemode false, '
    "created = a recent unix timestamp, and copies amount (integer), currency and "
    "payment_method_types (from payment_method_types[N] form keys), customer and confirm from the "
    "request body."
)

SECRET = re.compile(r"^pi_[A-Za-z0-9]{24}_secret_[A-Za-z0-9]{24}$")


def secret_ok(body):
    secret = body.get("client_secret") if isinstance(body, dict) else None
    return bool(secret and SECRET.match(secret) and secret.startswith(body.get("id", "") + "_secret_"))


def pin_ok(body):
    data = (body or {}).get("data") or {}
    return any((data.get(k) or {}).get("repository") for k in ("user", "organization"))


SUITES = {
    "movies": {
        "mocks": {
            "tmdb": {
                "contract": "movies/mockingbird-contracts/movies-proxy.json",
                "route_prefix": "/tmdb",
                "persona": TMDB_PERSONA,
            }
        },
        "cases": [
            {
                "name": "tmdb movie detail",
                "method": "GET",
                "path": "/tmdb/movie/{n}?append_to_response=videos,credits,images,external_ids,"
                "release_dates,combined_credits&include_image_language=en",
            },
            {"name": "tmdb popular", "method": "GET", "path": "/tmdb/movie/popular?page={n}"},
        ],
    },
    "github": {
        "mocks": {
            "github-graphql": {
                "contract": "github-readme-stats/mockingbird-contracts/schema.docs.graphql",
                "route_prefix": "/graphql",
                "persona": "Realistic public GitHub data for an active open-source developer. Counts "
                "must be plausible and internally consistent (e.g. merged PRs <= total PRs).",
            },
            "github-rest": {
                "contract": "github-readme-stats/mockingbird-contracts/api.github.com.json",
                "route_prefix": "/",
                "persona": "Realistic public GitHub data for an active open-source developer.",
            },
        },
        "cases": [
            {
                "name": "github search/commits",
                "method": "GET",
                "path": "/search/commits?q=author:dev{n}",
            },
            {
                "name": "github graphql stats",
                "method": "POST",
                "path": "/graphql/graphql",
                "json": {"query": STATS_QUERY, "variables": {"login": "dev{n}"}},
            },
            {
                "name": "github graphql pin (2 roots)",
                "method": "POST",
                "path": "/graphql/graphql",
                "json": {"query": PIN_QUERY, "variables": {"login": "dev{n}", "repo": "app"}},
                "check": ("repository present", pin_ok),
            },
        ],
    },
    "stripe": {
        "mocks": {
            "stripe": {
                "contract": "accept-a-payment/custom-payment-flow/server/node/mockingbird-contracts/"
                "stripe-spec3-v2500.json",
                "route_prefix": "/",
                "persona": STRIPE_PERSONA,
            }
        },
        "cases": [
            {
                "name": "stripe create payment_intent",
                "method": "POST",
                "path": "/v1/payment_intents",
                "form": {
                    "amount": "5999",
                    "currency": "usd",
                    "payment_method_types[0]": "card",
                },
                "check": ("client_secret format", secret_ok),
            }
        ],
    },
}


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def write_config(directory, trials, suite, model, ports):
    lines = [PROFILE.format(model=model)]
    lines.append(f"storage:\n  url: sqlite://{directory}/mockingbird.db\n")
    lines.append(
        f"server:\n  port: {ports[0]}\n  admin_port: {ports[1]}\n  grpc_port: {ports[2]}\n"
    )
    lines.append("mocks:\n")
    for name, mock in suite["mocks"].items():
        lines.append(f"  {name}:\n")
        lines.append(f"    contract: {json.dumps(os.path.join(trials, mock['contract']))}\n")
        lines.append(f"    route_prefix: {mock['route_prefix']}\n")
        lines.append(f"    persona: {json.dumps(mock['persona'])}\n")
    path = os.path.join(directory, "mockingbird.yaml")
    with open(path, "w") as f:
        f.write("".join(lines))
    return path


def wait_ready(base, proc, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"server exited with {proc.returncode}")
        try:
            with urllib.request.urlopen(f"{base}/readyz", timeout=2) as r:
                if r.status == 200:
                    return
        except (urllib.error.URLError, OSError):
            pass
        time.sleep(0.5)
    raise RuntimeError("server not ready")


def fill(value, n):
    if isinstance(value, str):
        return value.replace("{n}", str(n))
    if isinstance(value, dict):
        return {k: fill(v, n) for k, v in value.items()}
    return value


def send(base, case, n, session, timeout):
    path = fill(case["path"], n)
    headers = {"x-mockingbird-session": session}
    data = None
    if "json" in case:
        data = json.dumps(fill(case["json"], n)).encode()
        headers["content-type"] = "application/json"
    elif "form" in case:
        data = urllib.parse.urlencode(fill(case["form"], n)).encode()
        headers["content-type"] = "application/x-www-form-urlencoded"
    req = urllib.request.Request(base + path, data=data, headers=headers, method=case["method"])
    started = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            status, raw, src = r.status, r.read(), r.headers.get("x-mockingbird-source", "?")
    except urllib.error.HTTPError as e:
        status, raw, src = e.code, e.read(), e.headers.get("x-mockingbird-source", "?")
    elapsed = time.time() - started
    try:
        body = json.loads(raw)
    except ValueError:
        body = None
    return elapsed, status, src, body


def run_suite(binary, trials, name, suite, args, rows):
    directory = tempfile.mkdtemp(prefix=f"mb-bench-{name}-")
    ports = (free_port(), free_port(), free_port())
    config = write_config(directory, trials, suite, args.model, ports)
    log = open(os.path.join(directory, "server.log"), "w")
    proc = subprocess.Popen(
        [binary, "serve", "-c", config], cwd=directory, stdout=log, stderr=log
    )
    base = f"http://127.0.0.1:{ports[0]}"
    try:
        wait_ready(f"http://127.0.0.1:{ports[1]}", proc, 300)
        for case in suite["cases"]:
            if args.only and not re.search(args.only, case["name"]):
                continue
            samples, sources, passed = [], {}, 0
            stamp = int(time.time())
            for i in range(args.runs):
                session = f"bench-{stamp}-{i}"
                elapsed, status, src, body = send(base, case, 100 + i, session, args.timeout)
                samples.append(elapsed)
                sources[src] = sources.get(src, 0) + 1
                if "check" in case and case["check"][1](body):
                    passed += 1
                print(f"  {case['name']} #{i}: {elapsed:.1f}s {status} {src}", file=sys.stderr)
            repeat, _, repeat_src, _ = send(
                base, case, 100 + args.runs - 1, session, args.timeout
            )
            rows.append(
                {
                    "case": case["name"],
                    "p50": statistics.median(samples),
                    "max": max(samples),
                    "sources": sources,
                    "repeat": (repeat, repeat_src),
                    "check": f"{case['check'][0]}: {passed}/{args.runs}" if "check" in case else "",
                }
            )
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        log.close()
        if args.keep:
            print(f"kept {directory}", file=sys.stderr)
        else:
            shutil.rmtree(directory, ignore_errors=True)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--trials", required=True, help="the mockingbird-trials directory")
    p.add_argument("--binary", default="target/release/mockingbird")
    p.add_argument("--model", default="deepseek/deepseek-v4.1-flash")
    p.add_argument("--runs", type=int, default=3, help="fresh-session calls per case")
    p.add_argument("--timeout", type=float, default=180, help="client timeout per call, seconds")
    p.add_argument("--suite", action="append", choices=sorted(SUITES), help="default: all")
    p.add_argument("--only", help="regex over case names")
    p.add_argument("--keep", action="store_true", help="keep each suite's directory and log")
    args = p.parse_args()
    if not os.environ.get("OPENROUTER_API_KEY"):
        sys.exit("OPENROUTER_API_KEY is not set")
    trials = os.path.abspath(args.trials)
    binary = os.path.abspath(args.binary)
    rows = []
    for name in args.suite or sorted(SUITES):
        run_suite(binary, trials, name, SUITES[name], args, rows)
    print("| Case | p50 | max | Sources | Repeat | Check |")
    print("| --- | ---: | ---: | --- | --- | --- |")
    for r in rows:
        sources = ", ".join(f"{k} {v}" for k, v in sorted(r["sources"].items()))
        repeat = f"{r['repeat'][0]:.2f} s ({r['repeat'][1]})"
        print(f"| {r['case']} | {r['p50']:.1f} s | {r['max']:.1f} s | {sources} | {repeat} | {r['check']} |")


if __name__ == "__main__":
    main()
