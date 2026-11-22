#!/bin/sh
# sdb launcher: ./start.sh [--port <p>] [--data <dir>]
# Env fallbacks: SDB_PORT, SDB_DATA_DIR. Defaults: 5580, ./data
PORT="${SDB_PORT:-5580}"
DATA="${SDB_DATA_DIR:-./data}"

while [ $# -gt 0 ]; do
  case "$1" in
    --port)
      if [ $# -ge 2 ]; then PORT="$2"; shift 2; else shift; fi
      ;;
    --port=*)
      PORT="${1#--port=}"
      shift
      ;;
    --data)
      if [ $# -ge 2 ]; then DATA="$2"; shift 2; else shift; fi
      ;;
    --data=*)
      DATA="${1#--data=}"
      shift
      ;;
    *)
      shift
      ;;
  esac
done

cd "$(dirname "$0")" || exit 1
exec ./target/release/sdb --port "$PORT" --data "$DATA"
