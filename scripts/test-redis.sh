#!/usr/bin/env bash
# Runs the workspace tests against a throwaway local Redis server.
#
# Usage: scripts/test-redis.sh [cargo test args...]
# Example: scripts/test-redis.sh -p mockingbird-state --test it redis::
#
# Starts `redis-server` (no persistence) in a temp dir on a free 127.0.0.1
# port, exports MB_TEST_REDIS_URL and runs
# `cargo test --workspace --all-features "$@"`. The server and its directory
# are removed on exit. Combine with Postgres by running
# `scripts/test-redis.sh` with MB_TEST_POSTGRES_URL already set, or by
# setting MB_TEST_RUNNER=scripts/test-postgres.sh.
set -euo pipefail

REDIS_SERVER=${REDIS_SERVER:-$(command -v redis-server 2>/dev/null || echo /usr/bin/redis-server)}
[[ -x "$REDIS_SERVER" ]] || { echo "test-redis: redis-server not found on PATH or at /usr/bin/redis-server" >&2; exit 1; }

port_in_use() {
    (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

PORT=""
for _ in $(seq 1 50); do
    candidate=$((20000 + RANDOM % 40000))
    if ! port_in_use "$candidate"; then
        PORT=$candidate
        break
    fi
done
[[ -n "$PORT" ]] || { echo "test-redis: no free port found" >&2; exit 1; }

DATA_ROOT=$(mktemp -d)
SERVER_PID=""

cleanup() {
    if [[ -n "$SERVER_PID" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$DATA_ROOT"
}
trap cleanup EXIT

"$REDIS_SERVER" --port "$PORT" --bind 127.0.0.1 --save "" --appendonly no \
    --dir "$DATA_ROOT" --logfile "$DATA_ROOT/server.log" &
SERVER_PID=$!

ready=0
for _ in $(seq 1 100); do
    if port_in_use "$PORT"; then
        ready=1
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if [[ $ready -ne 1 ]]; then
    echo "test-redis: redis-server did not start" >&2
    cat "$DATA_ROOT/server.log" >&2 2>/dev/null || true
    exit 1
fi

export MB_TEST_REDIS_URL="redis://127.0.0.1:$PORT"
echo "test-redis: MB_TEST_REDIS_URL=$MB_TEST_REDIS_URL"

if [[ -n "${MB_TEST_RUNNER:-}" ]]; then
    "$MB_TEST_RUNNER" "$@"
else
    cargo test --workspace --all-features "$@"
fi
