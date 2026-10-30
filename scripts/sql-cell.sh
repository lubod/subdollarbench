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
# The defaults are the official cell cap (R23 D4, 2026-10-22): $0.99 / 480 min,
# the same as the console launcher. Pass smaller values for a smoke cell.
# Career phases: SDB_CELL_PHASE=2 SDB_CELL_CHAIN=<phase-1 cell id> (P1);
# phase 1 needs no chain, phases after 1 require one. Extra argv still
# passes through, so --phase/--chain also work there.
# Worker lanes (R48): SDB_CELL_WORKER=0-7; every port is base + 100*worker.
# Environment: CARGO_TARGET_DIR, CELL_DRY_RUN=1 to print the command only.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODEL="${1:?usage: sql-cell.sh <model> [budget_usd] [wall_min] [extra...]}"
shift || true
BUDGET="${1:-0.99}"
shift || true
WALL="${1:-480}"
shift || true
# Harness/effort are cell configuration like the caps: env, not extra argv,
# so the flags can never clash with the ones this script sets.
HARNESS="${SDB_CELL_HARNESS:-omp}"
EFFORT="${SDB_CELL_EFFORT:-high}"
PHASE="${SDB_CELL_PHASE:-}"
CHAIN="${SDB_CELL_CHAIN:-}"
WORKER="${SDB_CELL_WORKER:-0}"
if ! [[ "$WORKER" =~ ^[0-7]$ ]]; then
    echo "[cell] FATAL: SDB_CELL_WORKER must be 0-7 (got '$WORKER')" >&2
    exit 1
fi
CELL_PORT=$((5480 + WORKER * 100))

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="$TARGET_DIR/release/subdollarbench"

# The repo's .env is the source of truth for the cell's API key: a stale
# OPENROUTER_API_KEY exported in the calling shell burned a cell on
# 2026-10-18 ("User not found." 401 at round 0, ~$0 spend, but a poisoned
# fault row). Prefer .env; fall back to the environment.
if [[ -f "$ROOT/.env" ]]; then
    ENV_KEY="$(grep -m1 '^OPENROUTER_API_KEY=' "$ROOT/.env" | cut -d= -f2- | tr -d '\"' | tr -d "'")"
    if [[ -n "${ENV_KEY:-}" ]]; then
        if [[ -n "${OPENROUTER_API_KEY:-}" && "${OPENROUTER_API_KEY}" != "$ENV_KEY" ]]; then
            echo "[cell] note: the shell's OPENROUTER_API_KEY differs from .env; using .env (…${ENV_KEY: -4})"
        fi
        export OPENROUTER_API_KEY="$ENV_KEY"
    fi
fi

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

# Preflight: campaigns run foreground on this worker's ports, so no engine
# wave may hold them, and the worker's candidate port must be free.
status="$(curl -fsS -m 3 http://127.0.0.1:3000/api/status 2>/dev/null || true)"
if printf '%s' "$status" | grep -q '"is_running":true'; then
    echo "[cell] FATAL: the engine is running a wave; stop it first" >&2
    exit 1
fi
if command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | grep -q ":$CELL_PORT "; then
    echo "[cell] FATAL: worker $WORKER port $CELL_PORT is already in use" >&2
    exit 1
fi

echo "[cell] model=$MODEL budget=\$$BUDGET wall=${WALL}min harness=$HARNESS effort=$EFFORT phase=${PHASE:-full} chain=${CHAIN:-none} worker=$WORKER port=$CELL_PORT root=$ROOT"
PHASE_ARGS=()
if [[ -n "$PHASE" ]]; then PHASE_ARGS+=(--phase "$PHASE"); fi
if [[ -n "$CHAIN" ]]; then PHASE_ARGS+=(--chain "$CHAIN"); fi
if [[ "${CELL_DRY_RUN:-0}" == "1" ]]; then
    echo "[cell] dry run: would exec: campaign run --name sdb-sql --model $MODEL" \
        "--effort $EFFORT --harness $HARNESS --budget-usd $BUDGET --wall-min $WALL --timeout-min 120 --worker $WORKER ${PHASE_ARGS[*]} $*"
    exit 0
fi

RESULT_PATH="${SDB_CELL_RESULT:-}"
LOG="$(mktemp)"
set +e
env SDB_HOME="$ROOT" "$BIN" campaign run \
    --name sdb-sql --model "$MODEL" --effort "$EFFORT" --harness "$HARNESS" \
    --budget-usd "$BUDGET" --wall-min "$WALL" --timeout-min 120 --worker "$WORKER" "${PHASE_ARGS[@]}" "$@" \
    2>&1 | tee "$LOG"
rc=${PIPESTATUS[0]}
set -e
# R48 F4: the grid driver reads the phase's outcome from this file. The
# console output stays as it was; a plain operator run has no result path.
if [[ -n "$RESULT_PATH" ]]; then
    python3 - "$LOG" "$RESULT_PATH" "$WORKER" "${PHASE:-1}" "${CHAIN:-}" <<'PY'
import json, os, re, sys

log, out, worker, phase, chain = sys.argv[1:6]
text = open(log, errors="replace").read()
d = {
    "campaign_id": "",
    "phase": int(phase),
    "worker": int(worker),
    "chain": chain,
    "cost_usd": 0.0,
    "points": 0.0,
    "over_budget": False,
    "over_wall": False,
    "harness_fault": None,
    "infra_error": False,
    "wall_stop": False,
    "phase_complete": False,
    "error": "no rollup line",
}
m = re.search(r"^rollup: (.+)$", text, re.M)
if m:
    path = m.group(1).strip()
    try:
        r = json.load(open(path))
        d.update(
            campaign_id=os.path.basename(os.path.dirname(path)),
            cost_usd=float(r.get("total_cost_usd") or 0.0),
            points=float(r.get("points") or 0.0),
            over_budget=bool(r.get("over_budget")),
            over_wall=bool(r.get("over_wall")),
            harness_fault=r.get("harness_fault"),
            infra_error=any(s.get("status") == "fault" for s in r.get("stages", [])),
            wall_stop=bool(r.get("over_wall")),
            phase_complete=not r.get("harness_fault") and not r.get("over_wall"),
            error=None,
        )
    except Exception as e:  # noqa: BLE001 - report the parse failure in the file
        d["error"] = str(e)
json.dump(d, open(out, "w"))
PY
fi
rm -f "$LOG"
exit "$rc"
