#!/usr/bin/env bash
# One sdb-sql campaign cell: rebuild the release binary, preflight, run.
#
# A stale release binary silently runs the old verifier against a model —
# cell 2 (2026-10-16) did exactly that after the shrink fixes landed, so
# this script refuses to start unless the binary carries the current
# markers.
#
# Usage: scripts/sql-cell.sh <model> [budget_usd] [wall_min] [extra args...]
#   e.g. scripts/sql-cell.sh openrouter/thinkingmachines/inkling-small:free
# Environment: CARGO_TARGET_DIR, CELL_DRY_RUN=1 to print the command only.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODEL="${1:?usage: sql-cell.sh <model> [budget_usd] [wall_min] [extra...]}"
shift || true
BUDGET="${1:-0.25}"
shift || true
WALL="${1:-40}"
shift || true
# Harness/effort are cell configuration like the caps: env, not extra argv,
# so the flags can never clash with the ones this script sets.
HARNESS="${SDB_CELL_HARNESS:-omp}"
EFFORT="${SDB_CELL_EFFORT:-high}"

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="$TARGET_DIR/release/subdollarbench"

echo "[cell] building release binary ($BIN)"
(cd "$ROOT" && CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release --bins)

# Guard against the cell-2 mistake: the binary must contain the current
# verifier fixes. Keep this list current when code markers move (a missing
# marker is fail-closed, and cost one cell run on 2026-10-16 when the reset
# message was replaced by the readiness probe).
# (No `strings | grep -q`: pipefail + grep's early exit gives strings a
# SIGPIPE and the guard misfires.)
MARKERS="$(strings "$BIN" || true)"
for marker in "never answered a probe" "-repair" "embedded SQL engine or parser" "server unreachable" "sentinel from stage"; do
    if [[ "$MARKERS" != *"$marker"* ]]; then
        echo "[cell] FATAL: $BIN is stale (missing marker '$marker'); refusing to run" >&2
        exit 1
    fi
done

# Merge hygiene: fmt is fast; clippy stays in CI so the preflight is quick.
if ! (cd "$ROOT" && cargo fmt --check >/dev/null 2>&1); then
    echo "[cell] FATAL: cargo fmt --check fails; run cargo fmt" >&2
    exit 1
fi

# Preflight: campaigns run foreground on worker-0 ports, so no engine wave
# may hold them, and the candidate port must be free.
status="$(curl -fsS -m 3 http://127.0.0.1:3000/api/status 2>/dev/null || true)"
if printf '%s' "$status" | grep -q '"is_running":true'; then
    echo "[cell] FATAL: the engine is running a wave; stop it first" >&2
    exit 1
fi
if command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | grep -q ':5480 '; then
    echo "[cell] FATAL: port 5480 is already in use" >&2
    exit 1
fi

echo "[cell] model=$MODEL budget=\$$BUDGET wall=${WALL}min harness=$HARNESS effort=$EFFORT root=$ROOT"
if [[ "${CELL_DRY_RUN:-0}" == "1" ]]; then
    echo "[cell] dry run: would exec: campaign run --name sdb-sql --model $MODEL" \
        "--effort $EFFORT --harness $HARNESS --budget-usd $BUDGET --wall-min $WALL --timeout-min 20 $*"
    exit 0
fi

exec env SDB_HOME="$ROOT" "$BIN" campaign run \
    --name sdb-sql --model "$MODEL" --effort "$EFFORT" --harness "$HARNESS" \
    --budget-usd "$BUDGET" --wall-min "$WALL" --timeout-min 20 "$@"
