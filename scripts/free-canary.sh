#!/usr/bin/env bash
# R96 Z4: one free-model phase on the current code, before any paid model
# runs on a new rules version. The record is keyed by the full graded
# content key, so a code change needs a new canary.
#
# Usage: scripts/free-canary.sh [model]
#   SDB_CANARY_MODEL, SDB_CANARY_WORKER (default 7) and SDB_CANARY_DIR
#   (self-tests only) override the defaults.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODEL="${1:-${SDB_CANARY_MODEL:-openrouter/thinkingmachines/inkling-small:free}}"
WORKER="${SDB_CANARY_WORKER:-7}"
RUNS="${SDB_RUNS_DIR:-$ROOT/runs}"
FULL="$(cd "$ROOT" && scripts/flake-bar.sh --key)"
if [[ -n "${SDB_CANARY_DIR:-}" ]]; then
    OUT_DIR="$SDB_CANARY_DIR"
else
    OUT_DIR="$(cd "$ROOT" && git rev-parse --path-format=absolute --git-common-dir)/sdb-gates"
fi
RECORD="$OUT_DIR/canary-$FULL.txt"
mkdir -p "$OUT_DIR"

log() { echo "[canary] $*" >&2; }
write_record() {
    {
        echo "commit:  $(cd "$ROOT" && git rev-parse HEAD)"
        echo "key:     $FULL"
        echo "model:   $MODEL"
        echo "points:  ${1:-0}"
        echo "time:    $(date -u +%FT%TZ)"
        echo "RESULT: $2"
    } > "$RECORD"
    log "wrote $RECORD ($2)"
}

RESULT="$(mktemp)"
trap 'rm -f "$RESULT"' EXIT
log "free-model phase: $MODEL on worker $WORKER (key $FULL)"
set +e
SDB_CELL_SMOKE=1 SDB_CELL_PHASE=1 SDB_CELL_WORKER="$WORKER" \
SDB_CELL_HARNESS="${SDB_CANARY_HARNESS:-omp}" SDB_CELL_RESULT="$RESULT" \
SDB_RUNS_DIR="$RUNS" "$ROOT/scripts/sql-cell.sh" "$MODEL" 0.50 480
rc=$?
set -e
if [[ "$rc" -ne 0 ]]; then
    write_record 0 "FAIL (the cell exited $rc)"
    exit 1
fi
read -r points complete infra fault wall id <<<"$(python3 - "$RESULT" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception as d:
    d = {}
print(
    d.get("points") or 0,
    "1" if d.get("phase_complete") else "0",
    "1" if d.get("infra_error") else "0",
    "1" if d.get("harness_fault") else "0",
    "1" if d.get("over_wall") else "0",
    d.get("campaign_id") or "",
)
PY
)"
reason=""
awk -v p="$points" 'BEGIN { exit !(p > 0) }' || reason="no points"
[[ "$complete" == "1" ]] || reason="${reason:+$reason; }phase not complete"
[[ "$infra" == "0" ]] || reason="${reason:+$reason; }infra error"
[[ "$fault" == "0" ]] || reason="${reason:+$reason; }harness fault"
[[ "$wall" == "0" ]] || reason="${reason:+$reason; }wall stop"
if [[ -n "$id" && -f "$RUNS/$id/campaign.json" ]]; then
    flags="$(grep -ioE 'measurement failed|no source lines|Timed out waiting for response|panicked' \
        "$RUNS/$id/campaign.json" | sort -u | paste -sd, - || true)"
    [[ -z "$flags" ]] || reason="${reason:+$reason; }checker red flag: $flags"
fi
if [[ -n "$reason" ]]; then
    write_record "$points" "FAIL ($reason)"
    exit 1
fi
write_record "$points" "PASS"
