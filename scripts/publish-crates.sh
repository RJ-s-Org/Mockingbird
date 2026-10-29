#!/usr/bin/env bash
# Publishes `mockingbird-cli` and the internal crates it depends on to
# crates.io, skipping every crate whose workspace version is already there.
# When crates.io answers 429 (it limits how many *new* crates an account
# publishes in a short time), it waits and tries again with what is left, so
# a first release or a run that stopped half way resumes where it stopped.
#
# Usage: scripts/publish-crates.sh
# Env:   CARGO_REGISTRY_TOKEN  the crates.io token (not needed with DRY_RUN=1)
#        DRY_RUN=1             print the crates that would be published and stop
#        RETRY_WAIT            seconds to wait after a 429 (default 600)
#        MAX_ATTEMPTS          publish attempts before giving up (default 12)
#
# mb-ext is published on its own (it is versioned separately) and is skipped
# here. Run it from the repository root, with the console and embedded
# catalog already built (see the publish-cli job in release.yml).
set -euo pipefail

RETRY_WAIT=${RETRY_WAIT:-600}
MAX_ATTEMPTS=${MAX_ATTEMPTS:-12}

# name@version of every publishable workspace crate except mb-ext.
mapfile -t crates < <(cargo metadata --no-deps --format-version 1 |
    jq -r '.packages[] | select(.publish == null and .name != "mb-ext") | "\(.name)@\(.version)"')

on_crates_io() {
    local name=${1%@*} version=${1#*@}
    curl -sf -A "mockingbird-release (github.com/RJ-s-Org/Mockingbird)" \
        "https://crates.io/api/v1/crates/$name/$version" > /dev/null
}

log=$(mktemp)
trap 'rm -f "$log"' EXIT

for ((attempt = 1; attempt <= MAX_ATTEMPTS; attempt++)); do
    missing=() excludes=()
    for crate in "${crates[@]}"; do
        if on_crates_io "$crate"; then
            excludes+=(--exclude "${crate%@*}")
        else
            missing+=("$crate")
        fi
    done
    if [[ ${#missing[@]} -eq 0 ]]; then
        echo "every crate is on crates.io"
        exit 0
    fi
    echo "attempt $attempt: publishing ${missing[*]}"
    if [[ ${DRY_RUN:-} == 1 ]]; then
        exit 0
    fi
    # --allow-dirty: dist/ and embedded/ are build output, not in git.
    if cargo publish --workspace --exclude mb-ext "${excludes[@]}" --allow-dirty --locked 2>&1 | tee "$log"; then
        continue
    fi
    if ! grep -q '429 Too Many Requests' "$log"; then
        exit 1
    fi
    echo "::notice::crates.io rate limit; trying again in ${RETRY_WAIT}s"
    sleep "$RETRY_WAIT"
done
echo "::error::crates still missing after $MAX_ATTEMPTS attempts" >&2
exit 1
