#!/usr/bin/env bash
# Writes the Markdown report (or the verify result) to the job summary and
# collects the report, the request log and the server log for upload. A
# failed check is reported as the `failed` output, so the upload and the
# server stop still run before the job fails.
set -uo pipefail

if [ -z "${MB_STATE_DIR:-}" ] || ! command -v mockingbird > /dev/null; then
  echo "::warning::no Mockingbird server was started by the Mockingbird action in this job"
  exit 0
fi
out="$MB_STATE_DIR/report"
mkdir -p "$out/requests"

checks=()
[ "$FAIL_ON_FALLBACK" = true ] && checks+=(--no-fallbacks)
# Space-separated flags from one input.
# shellcheck disable=SC2206
checks+=($VERIFY)

if [ ${#checks[@]} -gt 0 ]; then
  mockingbird verify "${checks[@]}" --format markdown -o "$out/summary.md"
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "failed=true" >> "$GITHUB_OUTPUT"
  fi
  if [ "$status" -ne 0 ] && [ ! -s "$out/summary.md" ]; then
    echo "::error::mockingbird verify could not run (exit $status)"
  fi
else
  mockingbird report --format markdown -o "$out/summary.md" ||
    echo "::warning::mockingbird report failed"
fi
[ -s "$out/summary.md" ] && cat "$out/summary.md" >> "$GITHUB_STEP_SUMMARY"
mockingbird report --format json -o "$out/report.json" || true
if [ "$JUNIT" = true ]; then
  mockingbird report --format junit -o "$out/mockingbird.junit.xml" || true
fi

# The request log of every mock (redacted, as the Admin API serves it).
auth=()
[ -n "${MB_ADMIN_TOKEN:-}" ] && auth=(-H "Authorization: Bearer $MB_ADMIN_TOKEN")
ws="${MB_WORKSPACE:-default}"
for mock in $(jq -r '.mocks[].mock' "$out/report.json" 2> /dev/null); do
  curl -fsS ${auth[@]+"${auth[@]}"} "$MB_ADMIN_URL/api/v1/workspaces/$ws/mocks/$mock/requests?limit=500" \
    -o "$out/requests/$mock.json" || true
done
cp "$MB_STATE_DIR/server.log" "$out/server.log" 2> /dev/null || true
# The mocks are done: the artifact upload and every later step reach GitHub
# and registries directly, not through the proxy (which may block them).
# Both cases, which a step's `env:` cannot tell apart.
printf '%s=\n' HTTPS_PROXY HTTP_PROXY https_proxy http_proxy >> "$GITHUB_ENV"
exit 0
