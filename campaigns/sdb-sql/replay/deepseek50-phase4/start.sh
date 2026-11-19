#!/bin/sh
# Launch the sdb SQL server. Usage: ./start.sh --port <p> --data <dir>
#                                             [--pg-port <p>]
#                                             [--replicaof host:port]
PORT=""
DATA=""
PGPORT=""
REPLICAOF=""
while [ $# -gt 0 ]; do
    case "$1" in
        --port)
            PORT="$2"; shift 2 ;;
        --port=*)
            PORT="${1#*=}"; shift ;;
        --data)
            DATA="$2"; shift 2 ;;
        --data=*)
            DATA="${1#*=}"; shift ;;
        --pg-port)
            PGPORT="$2"; shift 2 ;;
        --pg-port=*)
            PGPORT="${1#*=}"; shift ;;
        --replicaof)
            REPLICAOF="$2"; shift 2 ;;
        --replicaof=*)
            REPLICAOF="${1#*=}"; shift ;;
        *)
            shift ;;
    esac
done
if [ -z "$PORT" ]; then
    PORT="${SDB_PORT:-5680}"
fi
if [ -z "$DATA" ]; then
    DATA="${SDB_DATA_DIR:-./data}"
fi
if [ -z "$PGPORT" ] && [ -n "${SDB_PG_PORT:-}" ]; then
    PGPORT="$SDB_PG_PORT"
fi
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
set -- --port "$PORT" --data "$DATA"
if [ -n "$PGPORT" ]; then
    set -- "$@" --pg-port "$PGPORT"
fi
if [ -n "$REPLICAOF" ]; then
    set -- "$@" --replicaof "$REPLICAOF"
fi
exec "$DIR/target/release/sdb" "$@"
