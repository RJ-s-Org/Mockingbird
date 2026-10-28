#!/usr/bin/env bash
# Starts `mockingbird server start` for the action's inputs, waits until it
# is ready, and exports its URLs (and, with the proxy, the proxy
# environment) to the job's later steps.
set -euo pipefail

mkdir -p "$STATE_DIR/bin"
if [ -n "$BINARY" ]; then
  binary=$(cd "$(dirname "$BINARY")" && pwd)/$(basename "$BINARY")
  ln -sf "$binary" "$STATE_DIR/bin/mockingbird"
  bin_dir="$STATE_DIR/bin"
else
  # BIN_DIR comes from action.yml.
  # shellcheck disable=SC2153
  bin_dir="$BIN_DIR"
fi
export PATH="$bin_dir:$PATH"
echo "$bin_dir" >> "$GITHUB_PATH"
version=$(mockingbird --version)
echo "version=$version" >> "$GITHUB_OUTPUT"

args=(server start --state-dir "$STATE_DIR" --wait "$WAIT")
if [ -n "$CONFIG" ]; then
  args+=(--config "$CONFIG" --no-watch)
fi
[ -n "$PORT" ] && args+=(--port "$PORT")
[ -n "$ADMIN_PORT" ] && args+=(--admin-port "$ADMIN_PORT")
case "$MODE" in
  offline) args+=(--offline) ;;
  replay) args+=(--replay "$SNAPSHOT") ;;
  record) args+=(--record "$SNAPSHOT") ;;
  llm) ;;
  *) echo "::error::mode must be offline, replay, record or llm (got $MODE)"; exit 1 ;;
esac
[ "$PROXY" = true ] && args+=(--proxy)
if [ -z "$CONFIG" ]; then
  if [ -z "$CONTRACTS" ]; then
    echo "::error::set config or contracts"
    exit 1
  fi
  # Space-separated on purpose: an input is a single string.
  # shellcheck disable=SC2206
  args+=($CONTRACTS)
fi

echo "::group::mockingbird ${args[*]}"
if ! mockingbird "${args[@]}" > "$STATE_DIR/start.json"; then
  cat "$STATE_DIR/start.json"
  echo "::endgroup::"
  echo "::error::Mockingbird did not become ready (log: $STATE_DIR/server.log)"
  exit 1
fi
cat "$STATE_DIR/start.json"
echo "::endgroup::"

admin=$(jq -r .admin "$STATE_DIR/start.json")
data=$(jq -r .data_plane "$STATE_DIR/start.json")
{
  echo "admin=$admin"
  echo "data-plane=$data"
} >> "$GITHUB_OUTPUT"
{
  echo "MB_ADMIN_URL=$admin"
  echo "MB_STATE_DIR=$STATE_DIR"
  echo "MOCKINGBIRD_URL=$data"
} >> "$GITHUB_ENV"

if [ "$PROXY" = true ]; then
  env_args=(proxy env --json)
  [ -n "$CONFIG" ] && env_args+=(--config "$CONFIG")
  mockingbird "${env_args[@]}" > "$STATE_DIR/proxy-env.json"
  jq -r '.vars | to_entries[] | "\(.key)=\(.value)"' "$STATE_DIR/proxy-env.json" >> "$GITHUB_ENV"
  jq -r '"proxy=" + .vars.HTTPS_PROXY' "$STATE_DIR/proxy-env.json" >> "$GITHUB_OUTPUT"
  echo "Later steps go through the proxy at $(jq -r .vars.HTTPS_PROXY "$STATE_DIR/proxy-env.json")"
fi
