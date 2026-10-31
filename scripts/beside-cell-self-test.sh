#!/usr/bin/env bash
# R56 F6: the four Docker-running tools refuse beside a live cell.
#
# The lock is held by the hidden `campaign lock-hold` command in a temp
# cells' runs dir (`SDB_CELL_RUNS_DIR`); each tool is run in
# SDB_BESIDE_CELL_ONLY=1 mode, which stops after the beside-cell check.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dir="$(mktemp -d "${TMPDIR:-/tmp}/sdb-beside-self.XXXXXX")"
runs="$dir/runs"
mkdir -p "$runs"
export SDB_CELL_RUNS_DIR="$runs"
export SDB_BESIDE_CELL_ONLY=1
holder=""

cleanup() {
    if [[ -n "$holder" ]]; then
        kill "$holder" 2>/dev/null || true
        wait "$holder" 2>/dev/null || true
    fi
    rm -rf "${dir:-}"
}
trap cleanup EXIT

BIN="${SDB_BESIDE_BIN:-$ROOT/target/debug/subdollarbench}"
if [[ ! -x "$BIN" ]]; then
    (cd "$ROOT" && cargo build --bin subdollarbench >/dev/null 2>&1)
fi

refuse() {
    local name="$1"
    shift
    local out rc=0
    out="$("$@" 2>&1)" || rc=$?
    if [[ "$rc" -eq 0 ]]; then
        echo "[self-test] $name accepted a live lock" >&2
        printf '%s\n' "$out" >&2
        return 1
    fi
    if ! grep -q 'campaign_beside' <<<"$out"; then
        echo "[self-test] $name did not name the campaign" >&2
        printf '%s\n' "$out" >&2
        return 1
    fi
    echo "[self-test] $name refused"
}

start() {
    local name="$1"
    shift
    local out
    if ! out="$("$@" 2>&1)"; then
        echo "[self-test] $name refused without a live lock" >&2
        printf '%s\n' "$out" >&2
        return 1
    fi
    if ! grep -q 'clear' <<<"$out"; then
        echo "[self-test] $name did not report clear" >&2
        printf '%s\n' "$out" >&2
        return 1
    fi
    echo "[self-test] $name starts"
}

# Hold a worker-0 lock in the cells' runs dir.
SDB_RUNS_DIR="$runs" "$BIN" campaign lock-hold \
    --worker 0 --id campaign_beside --seconds 60 >/dev/null 2>&1 &
holder=$!
for _ in $(seq 1 100); do
    [[ -f "$runs/.campaign.lock.w0" ]] && break
    sleep 0.1
done
[[ -f "$runs/.campaign.lock.w0" ]] || {
    echo '[self-test] the lock-hold command wrote no lock' >&2
    exit 1
}

refuse 'seen-failing' "$ROOT/scripts/seen-failing.sh" \
    test_pipeline_eval_only_sql_stage11_indexes_live /dev/null
refuse 'grid --self-test --real' "$ROOT/scripts/grid.sh" --self-test --real
refuse 'flake-bar' "$ROOT/scripts/flake-bar.sh" --passes 1
refuse 'gate (full)' "$ROOT/scripts/gate.sh"

kill "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true
holder=""
rm -f "$runs/.campaign.lock.w0" "$runs/.campaign.lock"

start 'seen-failing' "$ROOT/scripts/seen-failing.sh" \
    test_pipeline_eval_only_sql_stage11_indexes_live /dev/null
start 'grid --self-test --real' "$ROOT/scripts/grid.sh" --self-test --real
start 'flake-bar' "$ROOT/scripts/flake-bar.sh" --passes 1
start 'gate (full)' "$ROOT/scripts/gate.sh"

echo 'RESULT: PASS (beside-cell self-test)'
