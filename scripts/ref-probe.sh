#!/usr/bin/env bash
# R74 F4: the ref-sql reachability probe.
#
# While a grid runs, every 5 s for each running campaign, probe the
# reference alias on its run network:
#   - `ref-sql:5480`      the reference server (must stay up)
#   - `ref-sql:<cand>`    the candidate's worker port (the agent's wrong
#                         port when the prompt is mis-rendered; refused)
# A change between up and down is logged with its UTC time and the
# campaign. The probe is read-only and spends nothing.
#
# Usage: scripts/ref-probe.sh [--runs-dir DIR] [--interval 5] [--seconds 300]
#                             [--out FILE]
#        scripts/ref-probe.sh --self-test
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
runs_dir="${SDB_RUNS_DIR:-$ROOT/runs}"
interval=5
seconds=300
out=""

usage() {
    sed -n '2,18p' "$0" >&2
    exit 2
}

worker_of() {
    local id="$1"
    printf '%s' "$id" | sed -n 's/.*_w\([0-9]\)$/\1/p'
}

ports_for() {
    local worker="$1"
    printf '5480 %s\n' "$((5480 + 100 * worker))"
}

probe_port() {
    local net="$1" port="$2"
    timeout 8 docker run --rm --network "$net" subdollar-sandbox \
        sh -c "printf 'SELECT 1\\n' | timeout 2 nc -w 1 ref-sql $port" \
        >/dev/null 2>&1
}

log_change() {
    local id="$1" port="$2" state="$3"
    local line
    line="$(date -u +%FT%TZ) campaign=$id port=$port $state"
    printf '%s\n' "$line"
    if [[ -n "$out" ]]; then
        printf '%s\n' "$line" >> "$out"
    fi
}

self_test() {
    [[ "$(worker_of campaign_sdb-sql_x_y_20261109_000000_w3)" == "3" ]] || {
        echo "[probe] worker_of failed" >&2; exit 1; }
    [[ "$(ports_for 2)" == "5480 5680" ]] || {
        echo "[probe] ports_for failed" >&2; exit 1; }
    # The change detector: the same state is silent, a flip logs.
    local seen=""
    local count=0
    for state in UP UP DOWN DOWN UP; do
        if [[ "$state" != "$seen" ]]; then
            count=$((count + 1))
            seen="$state"
        fi
    done
    [[ "$count" -eq 3 ]] || { echo "[probe] change detection failed" >&2; exit 1; }
    echo "RESULT: PASS (ref-probe self-test)"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs-dir) [[ $# -ge 2 ]] || usage; runs_dir="$2"; shift 2 ;;
        --interval) [[ $# -ge 2 ]] || usage; interval="$2"; shift 2 ;;
        --seconds) [[ $# -ge 2 ]] || usage; seconds="$2"; shift 2 ;;
        --out) [[ $# -ge 2 ]] || usage; out="$2"; shift 2 ;;
        --self-test) self_test; exit 0 ;;
        -h|--help) usage ;;
        *) echo "[probe] unknown argument: $1" >&2; usage ;;
    esac
done
[[ "$interval" =~ ^[0-9]+$ && "$interval" -gt 0 ]] || {
    echo "[probe] --interval must be positive" >&2; exit 2; }
[[ "$seconds" =~ ^[0-9]+$ ]] || {
    echo "[probe] --seconds must be non-negative" >&2; exit 2; }
if [[ -n "$out" ]]; then
    mkdir -p "$(dirname "$out")"
    : > "$out"
fi

declare -A last_state=()
deadline=$((SECONDS + seconds))
while (( SECONDS < deadline )); do
    for status in "$runs_dir"/campaign_*/status.json; do
        [[ -f "$status" ]] || continue
        grep -q '"state": "running"' "$status" 2>/dev/null || continue
        id="$(basename "$(dirname "$status")")"
        worker="$(worker_of "$id")"
        [[ -n "$worker" ]] || continue
        # The reference network is per stage: `subdollar-net-<run_id>`,
        # where the run id is `<campaign>_s<stage>_<variant>`. The live.log
        # names the current one; the campaign id alone is not a network.
        live="$(dirname "$status")/live.log"
        net="$(sed -n "s/.*network '\(subdollar-net-[^']*\)'.*/\1/p" "$live" 2>/dev/null | tail -1)"
        [[ -n "$net" ]] || continue
        for port in $(ports_for "$worker"); do
            key="$id:$port"
            if probe_port "$net" "$port"; then
                state=UP
            else
                state=DOWN
            fi
            if [[ "${last_state[$key]:-}" != "$state" ]]; then
                log_change "$id" "$port" "$state"
                last_state[$key]="$state"
            fi
        done
    done
    sleep "$interval"
done
echo "[probe] done; log: ${out:-stdout}"
