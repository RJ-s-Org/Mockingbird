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
#   - every internal crate requirement in Cargo.toml's
#     [workspace.dependencies] (`mb-* = { path = …, version = "=…" }`) pins
#     <version>, so the published crates depend on each other exactly;
#   - the agent plugin manifests (plugins/mockingbird, gemini-extension.json)
#     have `"version": "<version>"`;
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

# Internal path dependencies pinned with `=`; mb-ext has its own version.
while IFS= read -r line; do
    [[ $line == *"version = \"=$VERSION\""* ]] ||
        fail "Cargo.toml: internal dependency does not pin =$VERSION: $line"
done < <(grep -E '^mb-[a-z]+ = \{ path = "crates/' "$ROOT/Cargo.toml" | grep -v '^mb-ext ')

CHART=$ROOT/deploy/helm/mockingbird/Chart.yaml
if [[ -f $CHART ]]; then
    grep -Eq "^appVersion:[[:space:]]*\"$(printf '%s' "$VERSION" | sed 's/[.]/\\./g')\"[[:space:]]*$" "$CHART" ||
        fail "$CHART does not have appVersion: \"$VERSION\""
else
    echo "check-version: notice: $CHART does not exist; skipping the appVersion check" >&2
fi

for manifest in plugins/mockingbird/.claude-plugin/plugin.json plugins/mockingbird/plugin.json \
    plugins/mockingbird/.cursor-plugin/plugin.json gemini-extension.json; do
    grep -Eq "\"version\":[[:space:]]*\"$(printf '%s' "$VERSION" | sed 's/[.]/\\./g')\"" "$ROOT/$manifest" ||
        fail "$manifest does not have \"version\": \"$VERSION\""
done

bash "$ROOT/scripts/changelog-section.sh" "$VERSION" >/dev/null ||
    fail "CHANGELOG.md has no section for $VERSION"

echo "check-version: $TAG matches the workspace version, chart and changelog"
