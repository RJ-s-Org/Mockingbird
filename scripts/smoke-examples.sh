#!/usr/bin/env bash
# Smoke-tests the API examples catalog (examples/apis): for each example,
# `mockingbird validate` on its contracts, then `serve --offline` and the one
# request its example.json names under "smoke".
#
#   scripts/smoke-examples.sh [mockingbird binary] [example ...]
#
# Needs jq and curl; gRPC examples also need grpcurl. Uses ports 47080/47090/
# 47051/47888 (SMOKE_*_PORT) so it does not collide with a server on the defaults.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-$root/target/debug/mockingbird}
shift || true
bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")

if [ "$#" -gt 0 ]; then
  names=("$@")
else
  names=()
  for meta in "$root"/examples/apis/*/example.json; do
    names+=("$(basename "$(dirname "$meta")")")
  done
fi

port=${SMOKE_PORT:-47080} admin=${SMOKE_ADMIN_PORT:-47090} grpc=${SMOKE_GRPC_PORT:-47051} proxy=${SMOKE_PROXY_PORT:-47888}
work=$(mktemp -d)
pid=
cleanup() {
  if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  rm -rf "$work"
}
trap cleanup EXIT

failed=()
for name in "${names[@]}"; do
  dir=$root/examples/apis/$name
  meta=$dir/example.json
  echo "::group::$name"
  ok=1
  start=$(date +%s)

  while read -r contract; do
    if ! "$bin" validate "$dir/$contract" >"$work/validate.log" 2>&1; then
      echo "validate $contract failed:"; cat "$work/validate.log"; ok=0
    else
      head -1 "$work/validate.log"
    fi
  done < <(jq -r '.contracts[]' "$meta")

  if [ "$ok" = 1 ]; then
    # Each example in its own state; the proxy CA goes to a scratch dir.
    MB_CA_DIR=$work/ca "$bin" serve -c "$dir/mockingbird.yaml" --offline \
      --port $port --admin-port $admin --grpc-port $grpc --proxy-port $proxy \
      >"$work/serve.log" 2>&1 &
    pid=$!
    ready=0
    for _ in $(seq 1 100); do
      if curl -fsS "http://127.0.0.1:$admin/readyz" >/dev/null 2>&1; then ready=1; break; fi
      if ! kill -0 "$pid" 2>/dev/null; then break; fi
      sleep 0.1
    done
    if [ "$ready" != 1 ]; then
      echo "not ready:"; curl -sS "http://127.0.0.1:$admin/readyz" || true; tail -40 "$work/serve.log"; ok=0
    fi
  fi

  if [ "$ok" = 1 ]; then
    kind=$(jq -r '.smoke.kind // "http"' "$meta")
    case "$kind" in
      http)
        method=$(jq -r '.smoke.method // "GET"' "$meta")
        path=$(jq -r '.smoke.path' "$meta")
        host=$(jq -r '.smoke.host // empty' "$meta")
        ctype=$(jq -r '.smoke.content_type // "application/json"' "$meta")
        want=$(jq -r '.smoke.status // 200' "$meta")
        expect=$(jq -r '.smoke.expect // empty' "$meta")
        args=(-sS -o "$work/body" -w '%{http_code}' -X "$method" "http://127.0.0.1:$port$path")
        if [ -n "$host" ]; then args+=(-H "Host: $host"); fi
        while read -r header; do
          [ -n "$header" ] && args+=(-H "$header")
        done < <(jq -r '.smoke.headers // {} | to_entries[] | "\(.key): \(.value)"' "$meta")
        if jq -e '.smoke | has("body")' "$meta" >/dev/null; then
          jq -j '.smoke.body | if type == "string" then . else tojson end' "$meta" >"$work/req"
          args+=(-H "content-type: $ctype" --data-binary "@$work/req")
        fi
        got=$(curl "${args[@]}" || echo 000)
        echo "$method $path -> $got"
        head -c 400 "$work/body"; echo
        if [ "$got" != "$want" ]; then echo "expected HTTP $want"; ok=0; fi
        if [ -n "$expect" ] && ! grep -qF -- "$expect" "$work/body"; then echo "body lacks $expect"; ok=0; fi
        ;;
      grpc)
        method=$(jq -r '.smoke.method' "$meta")
        data=$(jq -c '.smoke.body // {}' "$meta")
        if grpcurl -plaintext -max-time 10 -d "$data" "127.0.0.1:$grpc" "$method" >"$work/body" 2>&1; then
          echo "$method ->"; head -c 400 "$work/body"; echo
        else
          echo "grpcurl $method failed:"; cat "$work/body"; ok=0
        fi
        ;;
      channel)
        mock=$(jq -r '.smoke.mock' "$meta")
        channel=$(jq -r '.smoke.channel' "$meta")
        if "$bin" --admin-url "http://127.0.0.1:$admin" channels publish "$mock" "$channel" --count 1 >"$work/body" 2>&1; then
          echo "published on $mock/$channel:"; head -c 400 "$work/body"; echo
        else
          echo "publish on $mock/$channel failed:"; cat "$work/body"; ok=0
        fi
        ;;
      *) echo "unknown smoke kind $kind"; ok=0 ;;
    esac
  fi

  if [ -n "$pid" ]; then
    if [ "$ok" != 1 ]; then tail -20 "$work/serve.log"; fi
    kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; pid=
  fi
  echo "$name: $([ "$ok" = 1 ] && echo ok || echo FAILED) in $(( $(date +%s) - start ))s"
  echo "::endgroup::"
  [ "$ok" = 1 ] || failed+=("$name")
done

if [ "${#failed[@]}" -gt 0 ]; then
  echo "failed: ${failed[*]}" >&2
  exit 1
fi
echo "all ${#names[@]} examples passed"
