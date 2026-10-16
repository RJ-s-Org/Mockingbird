#!/usr/bin/env bash
# Checks the public API of `mb-ext` against the last release tag (`v*`) with
# cargo-semver-checks, so a breaking change cannot ship in a minor release.
#
#   scripts/semver-check.sh
#
# Before the first release there is nothing to compare against: the check
# prints `skipped: no release tag yet` and exits 0.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if ! tag=$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null); then
    echo "skipped: no release tag yet"
    exit 0
fi

echo "checking mb-ext against $tag"
exec cargo semver-checks check-release -p mb-ext --baseline-rev "$tag"
