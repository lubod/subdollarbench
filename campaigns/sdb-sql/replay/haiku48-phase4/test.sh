#!/bin/bash
# Offline test gate. Builds the server, runs the cargo suite (golden protocol
# scripts in-process and over the wire, plus concurrency, durability and
# observability checks), then checks the launch contract: ./start.sh must serve
# on --port/--data and keep the store across a restart.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")" && pwd)
cd "$ROOT"

cargo build --release --offline --locked
cargo test --release --offline --locked --tests

PORT=$((61000 + $$ % 4000))
DATA=$(mktemp -d "${TMPDIR:-/tmp}/sdb-gate.XXXXXX")
PID=""
ANSWER=""

stop_server() {
  exec 3>&-
  if [ -n "$PID" ]; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
    PID=""
  fi
}

cleanup() {
  stop_server
  rm -rf "$DATA"
}
trap cleanup EXIT

start_server() {
  ./start.sh --port "$PORT" --data "$DATA" &
  PID=$!
  local i
  for i in $(seq 1 400); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
      exec 3<>"/dev/tcp/127.0.0.1/$PORT"
      return 0
    fi
    if ! kill -0 "$PID" 2>/dev/null; then
      echo "FAIL launch: server exited during start" >&2
      exit 1
    fi
    sleep 0.05
  done
  echo "FAIL launch: server did not listen on port $PORT" >&2
  exit 1
}

# exchange <statement>: sends one statement and stores its answer frame in ANSWER.
exchange() {
  local line
  ANSWER=""
  printf '%s\n' "$1" >&3
  IFS= read -r -t 10 line <&3 || return 1
  ANSWER="$line"$'\n'
  if [[ $line == ROWS\ * ]]; then
    while IFS= read -r -t 10 line <&3; do
      ANSWER+="$line"$'\n'
      [[ $line == END\ * ]] && break
    done
  fi
}

# expect <label> <want> <statement>...: every answer frame, concatenated, must equal <want>.
expect() {
  local label="$1" want="$2" stmt got=""
  shift 2
  for stmt in "$@"; do
    exchange "$stmt" || { echo "FAIL $label: no answer to: $stmt" >&2; exit 1; }
    got+="$ANSWER"
  done
  if [ "$got" != "$want" ]; then
    printf 'FAIL %s\nwant:\n%s\ngot:\n%s\n' "$label" "$want" "$got" >&2
    exit 1
  fi
  echo "ok   $label"
}

start_server
expect "launch: create, insert, select" \
  $'OK 0\nOK 2\nROWS 2\nid\ts\n1\ta\n2\t\\N\nEND 2\n' \
  "CREATE TABLE smoke (id INTEGER, s TEXT)" \
  "INSERT INTO smoke VALUES (1, 'a'), (2, NULL)" \
  "SELECT id AS id, s AS s FROM smoke ORDER BY id"

exchange "SELECT nowhere FROM smoke"
if [[ $ANSWER != "ERR no_column "* ]]; then
  echo "FAIL launch: unknown column answered: $ANSWER" >&2
  exit 1
fi
echo "ok   launch: error keeps the connection usable"

stop_server
start_server
expect "launch: rows survive a restart" \
  $'ROWS 2\nid\ts\n1\ta\n2\t\\N\nEND 2\n' \
  "SELECT id AS id, s AS s FROM smoke ORDER BY id"
stop_server

echo "gate passed"
