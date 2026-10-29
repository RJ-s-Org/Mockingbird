#!/usr/bin/env bash
# Runs the workspace tests against a throwaway local Postgres server.
#
# Usage: scripts/test-postgres.sh [cargo test args...]
# Example: scripts/test-postgres.sh -p mockingbird-store --test it postgres::
#
# Starts initdb'd Postgres in a temp dir on a free 127.0.0.1 port with trust
# auth, creates database `mb_test`, exports MB_TEST_POSTGRES_URL and runs
# `cargo test --workspace --all-features "$@"`. The server and its directory
# are removed on exit.
set -euo pipefail

find_pg_bin() {
    local name=$1 found
    if found=$(command -v "$name" 2>/dev/null); then
        echo "$found"
        return 0
    fi
    found=$(ls -d /usr/lib/postgresql/*/bin/"$name" 2>/dev/null | sort -V | tail -n 1 || true)
    if [[ -n "$found" ]]; then
        echo "$found"
        return 0
    fi
    return 1
}

INITDB=$(find_pg_bin initdb) || { echo "test-postgres: initdb not found on PATH or in /usr/lib/postgresql/*/bin" >&2; exit 1; }
PG_CTL=$(find_pg_bin pg_ctl) || { echo "test-postgres: pg_ctl not found on PATH or in /usr/lib/postgresql/*/bin" >&2; exit 1; }
PG_BIN=$(dirname "$INITDB")
PSQL="$PG_BIN/psql"
[[ -x "$PSQL" ]] || PSQL=$(command -v psql) || { echo "test-postgres: psql not found" >&2; exit 1; }

# Postgres refuses to run as root: use the `postgres` user when we are root.
RUN_AS=()
if [[ $(id -u) -eq 0 ]]; then
    if id postgres >/dev/null 2>&1; then
        RUN_AS=(su postgres -s /bin/sh -c)
    else
        echo "test-postgres: running as root and no 'postgres' user exists; run as a non-root user or create a 'postgres' user" >&2
        exit 1
    fi
fi

# Runs a command (given as separate words) as the server user.
as_server_user() {
    if [[ ${#RUN_AS[@]} -eq 0 ]]; then
        "$@"
    else
        "${RUN_AS[@]}" "$(printf '%q ' "$@")"
    fi
}

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
[[ -n "$PORT" ]] || { echo "test-postgres: no free port found" >&2; exit 1; }

DATA_ROOT=$(mktemp -d)
PGDATA="$DATA_ROOT/data"
STARTED=0

cleanup() {
    if [[ $STARTED -eq 1 ]]; then
        as_server_user "$PG_CTL" -D "$PGDATA" -m fast -w stop >/dev/null 2>&1 || true
    fi
    rm -rf "$DATA_ROOT"
}
trap cleanup EXIT

if [[ ${#RUN_AS[@]} -ne 0 ]]; then
    chown postgres "$DATA_ROOT"
fi

as_server_user "$INITDB" -D "$PGDATA" -U postgres -A trust -E UTF8 --no-locale >"$DATA_ROOT/initdb.log" 2>&1 \
    || { cat "$DATA_ROOT/initdb.log" >&2; exit 1; }

STARTED=1
as_server_user "$PG_CTL" -D "$PGDATA" -l "$DATA_ROOT/server.log" -w \
    -o "-c listen_addresses=127.0.0.1 -p $PORT -c unix_socket_directories=$DATA_ROOT -c fsync=off" start >/dev/null \
    || { cat "$DATA_ROOT/server.log" >&2; exit 1; }

"$PSQL" -q -h 127.0.0.1 -p "$PORT" -U postgres -d postgres -v ON_ERROR_STOP=1 -c "CREATE DATABASE mb_test"

# A full workspace test build needs several GB under target/. When the disk
# fills mid-link, ld dies with SIGBUS instead of reporting ENOSPC, so say so up
# front.
TARGET_DIR=${CARGO_TARGET_DIR:-$(cd "$(dirname "$0")/.." && pwd)/target}
mkdir -p "$TARGET_DIR"
FREE_KB=$(df -Pk "$TARGET_DIR" | awk 'NR==2 {print $4}')
if [[ -n "$FREE_KB" && "$FREE_KB" -lt 4194304 ]]; then
    echo "test-postgres: warning: only $((FREE_KB / 1024)) MiB free under $TARGET_DIR; linking may fail (ld: Bus error). Run 'cargo clean' first." >&2
fi

export MB_TEST_POSTGRES_URL="postgres://postgres@127.0.0.1:$PORT/mb_test"
echo "test-postgres: MB_TEST_POSTGRES_URL=$MB_TEST_POSTGRES_URL"

cargo test --workspace --all-features "$@"
