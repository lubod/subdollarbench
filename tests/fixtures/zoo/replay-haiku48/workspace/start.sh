#!/bin/sh
ROOT=$(cd "$(dirname "$0")" && pwd)
BIN="$ROOT/target/release/sdb"
if [ ! -x "$BIN" ]; then
  (cd "$ROOT" && cargo build --release --offline --locked) >&2 || exit 1
fi
: "${SDB_DATA_DIR:=$ROOT/data}"
export SDB_DATA_DIR
exec "$BIN" "$@"
