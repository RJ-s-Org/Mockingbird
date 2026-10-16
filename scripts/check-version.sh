#!/usr/bin/env bash
# Checks that a release tag matches the repository before anything is built.
#
# Usage: scripts/check-version.sh <tag>
# Example: scripts/check-version.sh v0.1.0
#
# Fails (exit 1) unless:
#   - the tag is `v<version>` and <version> is the workspace `version` in
#     Cargo.toml ([workspace.package]);
#   - deploy/helm/mockingbird/Chart.yaml has `appVersion: "<version>"`
#     (skipped with a notice while the chart does not exist);
#   - scripts/changelog-section.sh <version> finds a CHANGELOG.md section.
# Exits 2 on bad usage.
set -euo pipefail

if [[ $# -ne 1 || -z "$1" ]]; then
    echo "usage: $0 <tag>" >&2
    exit 2
fi
TAG=$1
ROOT=$(cd "$(dirname "$0")/.." && pwd)

fail() {
    echo "check-version: $*" >&2
    exit 1
}

if [[ ! $TAG =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    fail "tag '$TAG' is not of the form vX.Y.Z[-prerelease]"
fi
VERSION=${TAG#v}

# The `version` key of the [workspace.package] table.
WORKSPACE_VERSION=$(awk '
    /^\[/ { in_package = ($0 == "[workspace.package]"); next }
    in_package && $1 == "version" {
        sub(/^[^=]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit
    }
' "$ROOT/Cargo.toml")
[[ -n $WORKSPACE_VERSION ]] || fail "no version in [workspace.package] of Cargo.toml"
[[ $VERSION == "$WORKSPACE_VERSION" ]] ||
    fail "tag $TAG does not match the workspace version $WORKSPACE_VERSION"

CHART=$ROOT/deploy/helm/mockingbird/Chart.yaml
if [[ -f $CHART ]]; then
    grep -Eq "^appVersion:[[:space:]]*\"$(printf '%s' "$VERSION" | sed 's/[.]/\\./g')\"[[:space:]]*$" "$CHART" ||
        fail "$CHART does not have appVersion: \"$VERSION\""
else
    echo "check-version: notice: $CHART does not exist; skipping the appVersion check" >&2
fi

bash "$ROOT/scripts/changelog-section.sh" "$VERSION" >/dev/null ||
    fail "CHANGELOG.md has no section for $VERSION"

echo "check-version: $TAG matches the workspace version, chart and changelog"
