#!/usr/bin/env bash
# The fast local gate: checks only what the branch changed, so it runs in minutes.
# CI runs the full suite on every pull request (about 4 minutes); this gate exists to
# catch the obvious failures before a review, not to repeat CI.
#
#   scripts/gate.sh            # changes since the merge base with origin/main, plus uncommitted ones
#   scripts/gate.sh --full     # everything CI's Rust job runs (the full workspace)
#   BASE=origin/release scripts/gate.sh
#
# What it runs:
#   - `cargo fmt --all --check` (always; it takes seconds)
#   - clippy and nextest for each changed crate and the workspace crates that depend
#     on it directly
#   - the console's typecheck, lint and tests when crates/mb-console/web changed
#   - the bench's Python tests when scripts/llm-bench changed
# A change to the root Cargo.toml, Cargo.lock, rust-toolchain.toml or .config/ runs the
# full workspace.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

BASE="${BASE:-origin/main}"
FULL=0
[[ "${1:-}" == "--full" ]] && FULL=1

changed=$( { git diff --name-only "$(git merge-base HEAD "$BASE")"...HEAD; git diff --name-only HEAD; git ls-files --others --exclude-standard; } | sort -u )
if [[ -z "$changed" && $FULL -eq 0 ]]; then
  echo "gate: nothing changed against $BASE"
  exit 0
fi

if grep -qE '^(Cargo\.toml|Cargo\.lock|rust-toolchain\.toml|\.config/)' <<<"$changed"; then
  FULL=1
fi

step() { printf '\n== %s\n' "$*"; }

if grep -qE '\.rs$|Cargo\.toml$' <<<"$changed" || [[ $FULL -eq 1 ]]; then
  step "cargo fmt --all --check"
  cargo fmt --all --check

  if [[ $FULL -eq 1 ]]; then
    step "full workspace"
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo nextest run --workspace --all-features
  else
    # Packages whose directory holds a changed file, plus their direct workspace dependents.
    pkgs=$(cargo metadata --no-deps --format-version 1 | CHANGED="$changed" python3 -c '
import json, os, sys
m = json.load(sys.stdin)
root = m["workspace_root"].rstrip("/") + "/"
dirs = {p["name"]: os.path.dirname(p["manifest_path"])[len(root):] + "/" for p in m["packages"]}
files = os.environ["CHANGED"].split()
hit = {n for n, d in dirs.items() if any(f.startswith(d) for f in files)}
names = set(dirs)
deps = {p["name"]: {d["name"] for d in p["dependencies"]} & names for p in m["packages"]}
hit |= {n for n, ds in deps.items() if ds & hit}
print(" ".join(sorted(hit)))
')
    if [[ -n "$pkgs" ]]; then
      args=()
      for p in $pkgs; do args+=(-p "$p"); done
      step "clippy: $pkgs"
      cargo clippy "${args[@]}" --all-targets --all-features -- -D warnings
      step "nextest: $pkgs"
      cargo nextest run "${args[@]}" --all-features --no-tests=pass
    fi
  fi
fi

if grep -q '^crates/mb-console/web/' <<<"$changed"; then
  step "console: typecheck, lint, test"
  (cd crates/mb-console/web && npm run typecheck && npm run lint && npm test)
fi

if grep -q '^scripts/llm-bench/' <<<"$changed"; then
  step "bench: python tests"
  python3 -m unittest discover -s scripts/llm-bench -p 'test_*.py'
fi

step "gate passed"
