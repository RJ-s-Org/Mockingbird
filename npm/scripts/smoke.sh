#!/usr/bin/env bash
# Builds the npm packages around a local mockingbird binary, packs them,
# installs the tarballs into a fresh project and runs `npx mockingbird`
# there, as a user of the published packages would.
#
# Usage: npm/scripts/smoke.sh <path to a mockingbird binary for this machine>
set -euo pipefail

if [[ $# -ne 1 || ! -x "$1" ]]; then
    echo "usage: $0 <mockingbird binary>" >&2
    exit 2
fi
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BINARY=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
VERSION=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml")
KEY=$(node -p 'process.platform + "-" + process.arch')
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "--- build the packages ($VERSION, $KEY)"
node "$ROOT/npm/scripts/build-packages.mjs" --version "$VERSION" --out "$WORK/packages" \
    --binary "$KEY=$BINARY"

echo "--- pack them"
mkdir "$WORK/tarballs"
for dir in "$WORK"/packages/*/; do
    (cd "$dir" && npm pack --silent --pack-destination "$WORK/tarballs" > /dev/null)
done
ls -l "$WORK/tarballs"

echo "--- install the tarballs into a new project"
mkdir "$WORK/project"
cd "$WORK/project"
npm init -y > /dev/null
npm install --no-audit --no-fund "$WORK"/tarballs/*.tgz
# The example's mock of api.github.com, mapped through the proxy.
cp "$ROOT/examples/apis/github-rest/mockingbird.yaml" "$ROOT/examples/apis/github-rest/github-rest.openapi.yaml" .

# The platform package must be what runs, not a download.
export MOCKINGBIRD_NO_DOWNLOAD=1

echo "--- npx mockingbird --version"
out=$(npx --no-install mockingbird --version)
echo "$out"
[[ $out == "mockingbird $VERSION" ]] || { echo "expected 'mockingbird $VERSION'" >&2; exit 1; }

echo "--- npx mockingbird run --offline -- node -e …"
npx --no-install mockingbird run --offline -- node -e '
fetch("https://api.github.com/repos/fernwood-labs/trellis", { headers: { authorization: "Bearer ghp_test123" } })
  .then(async (res) => {
    const body = await res.json();
    console.log(res.status, res.headers.get("x-mockingbird-source"), JSON.stringify(body).slice(0, 120));
    process.exit(res.status === 200 && typeof body.full_name === "string" ? 0 : 3);
  })
  .catch((err) => { console.error(err); process.exit(4); });
'

echo "--- the exit code of the command comes back through npx"
set +e
npx --no-install mockingbird run --offline -- node -e 'process.exit(5)'
code=$?
set -e
[[ $code -eq 5 ]] || { echo "expected exit code 5, got $code" >&2; exit 1; }

echo "npm smoke test passed"
