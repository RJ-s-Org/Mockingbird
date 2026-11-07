#!/usr/bin/env bash
# The fast local gate: checks only what the branch changed, so it runs in minutes.
# CI runs the full suite on main after every merge (a few minutes); this gate exists to
# catch the obvious failures before a review, not to repeat CI.
#
#   scripts/gate.sh            # changes since the merge base with origin/main, plus uncommitted ones
#   scripts/gate.sh --full     # the whole workspace: fmt, clippy, nextest, doctests
#   BASE=origin/release scripts/gate.sh
#
# What it runs:
#   - `cargo fmt --all --check` (it takes seconds)
#   - clippy, nextest and doctests for each crate with a changed file (any file under
#     its directory, not only .rs) and the workspace crates that depend on it directly;
#     files the CLI embeds or its tests read (plugins/, docs/, examples/, install.sh)
#     count as changes to mockingbird-cli
#   - the console's typecheck, lint and tests when crates/mb-console/web changed
#   - the bench's Python tests when scripts/llm-bench changed, after building the
#     release binary their end-to-end replay test runs (MB_BENCH_BINARY)
#   - the replay gate (scripts/llm-bench/replay.py: the journeys against recorded LLM
#     answers, offline and free) when mb-protocols, mb-engine, mb-import, mb-state,
#     mb-store, mb-llm, scripts/llm-bench or docs/benchmarks/world-cache changed, on that
#     same release binary, on both world tiers (none and agent)
# A change to the root Cargo.toml, Cargo.lock, rust-toolchain.toml, clippy.toml,
# deny.toml, .cargo/ or .config/ runs the full workspace. Transitive dependents are left
# to main's CI, which runs everything (cargo-deny, docs and the slim build included).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

BASE="${BASE:-origin/main}"
FULL=0
PACKAGES=0
[[ "${1:-}" == "--full" ]] && FULL=1
# --packages: print what the Rust step would cover ("all", the package names, or
# nothing) and exit. CI's `check` job uses it on pull requests.
[[ "${1:-}" == "--packages" ]] && PACKAGES=1

git fetch -q origin main 2>/dev/null || true
base=$(git merge-base HEAD "$BASE") || { echo "gate: cannot find the merge base with $BASE" >&2; exit 2; }
changed=$( { git diff --name-only "$base" HEAD; git diff --name-only HEAD; git ls-files --others --exclude-standard; } | sort -u )
if [[ -z "$changed" && $FULL -eq 0 ]]; then
  [[ $PACKAGES -eq 1 ]] && exit 0
  echo "gate: nothing changed against $BASE"
  exit 0
fi

if grep -qE '^(Cargo\.toml|Cargo\.lock|rust-toolchain\.toml|clippy\.toml|deny\.toml|\.cargo/|\.config/)' <<<"$changed"; then
  FULL=1
fi

step() { printf '\n== %s\n' "$*"; }

# Packages with a changed file under their directory, plus their direct workspace dependents.
pkgs=$(cargo metadata --no-deps --format-version 1 | CHANGED="$changed" python3 -c '
import json, os, sys
m = json.load(sys.stdin)
root = m["workspace_root"].rstrip("/") + "/"
dirs = {p["name"]: os.path.dirname(p["manifest_path"])[len(root):] + "/" for p in m["packages"]}
files = [f for f in os.environ["CHANGED"].splitlines() if f]
hit = {n for n, d in dirs.items() if any(f.startswith(d) for f in files)}
# Files outside crates/ that the CLI embeds or its tests read.
if any(f.startswith(("plugins/", "docs/", "examples/")) or f == "install.sh" for f in files):
    hit.add("mockingbird-cli")
names = set(dirs)
deps = {p["name"]: {d["name"] for d in p["dependencies"]} & names for p in m["packages"]}
hit |= {n for n, ds in deps.items() if ds & hit}
print(" ".join(sorted(hit)))
')

if [[ $PACKAGES -eq 1 ]]; then
  if [[ $FULL -eq 1 ]]; then echo all; else echo "$pkgs"; fi
  exit 0
fi

if [[ $FULL -eq 1 ]]; then
  step "full workspace"
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo nextest run --workspace --all-features
  cargo test --doc --workspace --all-features
elif [[ -n "$pkgs" ]]; then
  args=()
  for p in $pkgs; do args+=(-p "$p"); done
  step "cargo fmt --all --check"
  cargo fmt --all --check
  step "clippy: $pkgs"
  cargo clippy "${args[@]}" --all-targets --all-features -- -D warnings
  step "nextest: $pkgs"
  cargo nextest run "${args[@]}" --all-features --no-tests=pass
  step "doctests: $pkgs"
  cargo test --doc "${args[@]}" --all-features
fi

if grep -q '^crates/mb-console/web/' <<<"$changed"; then
  step "console: typecheck, lint, test"
  (cd crates/mb-console/web && npm run typecheck && npm run lint && npm test)
fi

# The bench's Python tests include an end-to-end replay test that runs the release
# binary, so the binary is built first and handed to them: a stale one from an earlier
# build would test the wrong engine.
replay_paths='^(crates/mb-protocols/|crates/mb-engine/|crates/mb-import/|crates/mb-state/|crates/mb-store/|crates/mb-llm/|scripts/llm-bench/|docs/benchmarks/world-cache/)'
target_dir="${CARGO_TARGET_DIR:-target}"
[[ "$target_dir" == /* ]] || target_dir="$PWD/$target_dir"
bench_binary="$target_dir/release/mockingbird"
if grep -qE "$replay_paths" <<<"$changed"; then
  step "release binary for the bench"
  cargo build --release -p mockingbird-cli --locked
fi

if grep -q '^scripts/llm-bench/' <<<"$changed"; then
  step "bench: python tests"
  MB_BENCH_BINARY="$bench_binary" python3 -m unittest discover -s scripts/llm-bench -p 'test_*.py'
fi

if grep -qE "$replay_paths" <<<"$changed"; then
  step "replay gate: journeys against recorded LLM answers"
  python3 scripts/llm-bench/replay.py gate --binary "$bench_binary" --world none --world agent
fi

step "gate passed"
