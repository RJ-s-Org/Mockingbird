#!/usr/bin/env python3
"""A free, deterministic stand-in for the world-description author (`--author stub`).

    python3 scripts/llm-bench/stub_author.py --binary target/release/mockingbird \\
        --admin-url http://127.0.0.1:P --mock stripe --contract stripe.openapi.yaml

It goes through the same interface as the real author, the Mockingbird CLI
(`mockingbird world show --json` and `mockingbird world patch --author agent`), and
prints a Claude Code stream-json transcript (init, one tool_use and tool_result per
write, a final result with cost 0) so the bench records it like a real one.

What it writes carries no knowledge of the API: it restates the heuristic
description's type identities as agent entries, then sends one entry naming an
operation the contract lacks, so a refusal is recorded. It is for dry runs and
tests, never for numbers. Standard library only.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time

UNKNOWN_OP = "stubNoSuchOperation"


def emit(event):
    print(json.dumps(event), flush=True)


def cli(args, *rest, stdin=None):
    proc = subprocess.run([args.binary, "--admin-url", args.admin_url, *rest], input=stdin,
                          capture_output=True, text=True)
    return proc.returncode, proc.stdout, proc.stderr


def write(args, n, patch):
    """One `world patch` through the CLI, recorded as an MCP-style tool call."""
    use_id = f"stub-{n}"
    emit({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": use_id, "name": "mcp__mockingbird__patch_world",
         "input": {"mock": args.mock, "author": "agent", "patch": patch}}]}})
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
        json.dump(patch, f)
        path = f.name
    try:
        rc, out, err = cli(args, "world", "patch", args.mock, path, "--author", "agent")
    finally:
        os.unlink(path)
    emit({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": use_id, "is_error": rc != 0,
         "content": [{"type": "text", "text": out if rc == 0 else (out or err)}]}]}})
    return rc


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--binary", required=True)
    p.add_argument("--admin-url", required=True)
    p.add_argument("--mock", required=True)
    p.add_argument("--contract", required=True, help="the contract file in the working directory")
    args = p.parse_args()
    started = time.monotonic()
    emit({"type": "system", "subtype": "init", "model": "stub"})
    if not os.path.isfile(args.contract):
        sys.exit(f"stub author: {args.contract} is not in the working directory")
    rc, out, err = cli(args, "world", "show", args.mock, "--json")
    if rc != 0:
        sys.exit(f"stub author: world show failed: {err or out}")
    world = (json.loads(out) or {}).get("world") or {}
    types = {name: {"identity": t["identity"]} for name, t in sorted((world.get("types") or {}).items())
             if isinstance(t, dict) and isinstance(t.get("identity"), dict)}
    writes = 0
    if types:
        write(args, 1, {"types": types})
        writes += 1
    write(args, 2, {"operations": {UNKNOWN_OP: {"kind": "read"}}})
    writes += 1
    emit({"type": "result", "subtype": "success", "is_error": False, "total_cost_usd": 0.0,
          "duration_ms": round((time.monotonic() - started) * 1000), "num_turns": writes + 1,
          "usage": {"input_tokens": 0, "output_tokens": 0},
          "result": f"stub: restated {len(types)} identities, sent one unknown operation"})


if __name__ == "__main__":
    main()
