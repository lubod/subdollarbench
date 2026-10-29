#!/usr/bin/env bash
# R48 F4: the grid driver, in-repo.
#
# `--lanes N` runs N careers at once, lane i on worker i+1. A lane runs
# each of its careers' phases 1-4 in order (phases chain). All lanes share
# one ledger and one spend cap; a STOP on any lane stops new phases on
# every lane and lets the running phases finish. The hold file pauses
# every lane at its next phase boundary.
#
# The cell is `scripts/sql-cell.sh` by default (or `SDB_GRID_CELL`). It is
# called with `SDB_CELL_PHASE`, `SDB_CELL_CHAIN`, `SDB_CELL_WORKER` and
# `SDB_CELL_RESULT`; the result JSON is the phase's outcome (the real
# script writes it after `campaign run`, the self-test's fake cell writes
# it directly).
#
# Usage:
#   scripts/grid.sh --careers careers.txt --lanes 3 [--cap 14.85]
#   scripts/grid.sh --self-test
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNS="${SDB_RUNS_DIR:-$ROOT/runs}"
LANES=1
CAREERS_FILE=""
CAP=0
PHASE_BUDGET="${SDB_GRID_PHASE_BUDGET:-0.99}"
PHASE_WALL="${SDB_GRID_PHASE_WALL:-480}"
LEDGER=""
HOLD=""
STOP=""
CELL="${SDB_GRID_CELL:-$ROOT/scripts/sql-cell.sh}"
SELF_TEST=0
DRY_RUN=0
WORKER_BASE=1

log() { echo "[grid] $*" >&2; }

usage() {
    cat >&2 <<'EOF'
usage: grid.sh --careers <file> --lanes <n> [--cap <usd>] [--budget <usd>]
               [--wall <min>] [--ledger <file>] [--hold <file>] [--stop <file>]
               [--cell <path>] [--dry-run]
       grid.sh --self-test
EOF
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --careers) CAREERS_FILE="$2"; shift 2 ;;
        --lanes) LANES="$2"; shift 2 ;;
        --cap) CAP="$2"; shift 2 ;;
        --budget) PHASE_BUDGET="$2"; shift 2 ;;
        --wall) PHASE_WALL="$2"; shift 2 ;;
        --ledger) LEDGER="$2"; shift 2 ;;
        --hold) HOLD="$2"; shift 2 ;;
        --stop) STOP="$2"; shift 2 ;;
        --cell) CELL="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        -h|--help) usage ;;
        *) log "unknown argument: $1"; usage ;;
    esac
done

[[ "$LANES" =~ ^[1-9][0-9]*$ ]] || { log "--lanes must be a positive integer"; exit 2; }
[[ "$CAP" =~ ^[0-9]+([.][0-9]+)?$ ]] || { log "--cap must be a non-negative number"; exit 2; }
LEDGER="${LEDGER:-$RUNS/.grid-ledger.tsv}"
HOLD="${HOLD:-$RUNS/.grid-hold}"
STOP="${STOP:-$RUNS/.grid-stop}"

ledger_add() {
    # career phase worker campaign_id points cost stop
    local career="$1" phase="$2" worker="$3" id="$4" points="$5" cost="$6" stop="$7"
    (
        flock 200
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$career" "$phase" "$worker" "$id" "$points" "$cost" "$stop" \
            "$(date -u +%FT%TZ)" >> "$LEDGER"
    ) 200>>"$LEDGER.lock"
}

ledger_total() {
    awk -F'\t' 'NR > 1 { s += $6 } END { printf "%.6f", s }' "$LEDGER" 2>/dev/null || echo 0
}

wait_hold() {
    if [[ -f "$HOLD" ]]; then
        log "hold at $HOLD; waiting"
        while [[ -f "$HOLD" ]]; do sleep 2; done
        log "hold released"
    fi
}

port_busy() {
    local port="$1"
    command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | grep -q ":$port "
}

preflight() {
    local worker="$1"
    if [[ "${SDB_GRID_SKIP_PREFLIGHT:-0}" == "1" ]]; then
        return 0
    fi
    if [[ -n "$(cd "$ROOT" && git status --porcelain --untracked-files=no)" ]]; then
        log "preflight: the tracked tree is dirty"
        return 1
    fi
    local key record
    mapfile -t graded < <(grep -vE '^[[:space:]]*(#|$)' "$ROOT/scripts/graded-paths.txt")
    key="$(cd "$ROOT" && git ls-tree -r HEAD -- "${graded[@]}" | sha256sum | cut -c1-12)"
    record="$(cd "$ROOT" && git rev-parse --path-format=absolute --git-common-dir)/sdb-gates/flake-$key.txt"
    if ! grep -q 'RESULT: PASS' "$record" 2>/dev/null; then
        log "preflight: no PASS flake record for key $key ($record)"
        return 1
    fi
    local base port
    for base in 5480 5481 8101 8102 8103; do
        port=$((base + 100 * worker))
        if port_busy "$port"; then
            log "preflight: worker $worker port $port is in use"
            return 1
        fi
    done
}

result_get() {
    python3 - "$1" "$2" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    print("")
    raise SystemExit(0)
v = d.get(sys.argv[2], "")
if v is None:
    v = ""
print(v)
PY
}

result_true() {
    python3 - "$1" "$2" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    print("0")
    raise SystemExit(0)
print("1" if d.get(sys.argv[2]) else "0")
PY
}

run_cell() {
    local model="$1" phase="$2" chain="$3" worker="$4" result="$5"
    SDB_CELL_PHASE="$phase" \
    SDB_CELL_CHAIN="$chain" \
    SDB_CELL_WORKER="$worker" \
    SDB_CONCURRENT_CELLS="$LANES" \
    SDB_CELL_RESULT="$result" \
        "$CELL" "$model" "$PHASE_BUDGET" "$PHASE_WALL"
}

run_lane() {
    local lane="$1"
    local worker=$((WORKER_BASE + lane))
    local idx model phase chain result id rc
    local failed=0
    for idx in "${!CAREERS[@]}"; do
        model="${CAREERS[$idx]}"
        if (( idx % LANES != lane )); then
            continue
        fi
        chain=""
        for phase in 1 2 3 4; do
            if [[ -f "$STOP" ]]; then
                log "lane $lane worker $worker: STOP before $model phase $phase"
                return "$failed"
            fi
            wait_hold
            if awk -v t="$(ledger_total)" -v c="$CAP" 'BEGIN { exit !(c > 0 && t >= c) }'; then
                log "lane $lane: cap $CAP reached; STOP"
                touch "$STOP"
                return "$failed"
            fi
            if ! preflight "$worker"; then
                log "lane $lane: preflight failed for $model phase $phase"
                touch "$STOP"
                return 1
            fi
            result="$(mktemp)"
            rc=0
            run_cell "$model" "$phase" "$chain" "$worker" "$result" || rc=$?
            id="$(result_get "$result" campaign_id)"
            local got_phase points cost stop
            got_phase="$(result_get "$result" phase)"
            points="$(result_get "$result" points)"
            cost="$(result_get "$result" cost_usd)"
            stop="ok"
            if (( rc != 0 )); then
                stop="cell exit $rc"
            elif [[ "$got_phase" != "$phase" ]]; then
                stop="phase $got_phase"
            elif [[ "$(result_true "$result" harness_fault)" == "1" ]]; then
                stop="harness fault"
            elif [[ "$(result_true "$result" infra_error)" == "1" ]]; then
                stop="infra error"
            elif [[ "$(result_true "$result" over_budget)" == "1" ]]; then
                stop="over budget"
            elif [[ "$(result_true "$result" over_wall)" == "1" ]]; then
                stop="over wall"
            elif [[ "$(result_true "$result" wall_stop)" == "1" ]]; then
                stop="wall stop"
            elif awk -v c="$cost" 'BEGIN { exit !(c > 0.99) }'; then
                stop="cost > 0.99"
            fi
            ledger_add "$model" "$phase" "$worker" "${id:-—}" "${points:-0}" "${cost:-0}" "$stop"
            rm -f "$result"
            if [[ "$stop" != "ok" ]]; then
                log "lane $lane: $model phase $phase stopped: $stop"
                touch "$STOP"
                return "$failed"
            fi
            chain="$id"
            [[ -z "$chain" ]] && { log "lane $lane: $model phase $phase has no campaign id"; touch "$STOP"; return 1; }
        done
    done
    return "$failed"
}

run_grid() {
    mkdir -p "$(dirname "$LEDGER")"
    rm -f "$STOP"
    if [[ ! -f "$LEDGER" ]]; then
        printf 'career\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\n' > "$LEDGER"
    fi
    local pids=() lane
    local rc=0
    for ((lane = 0; lane < LANES; lane++)); do
        run_lane "$lane" &
        pids+=("$!")
    done
    for pid in "${pids[@]}"; do
        if ! wait "$pid"; then
            rc=1
        fi
    done
    return "$rc"
}

load_careers() {
    local file="$1"
    [[ -f "$file" ]] || { log "careers file not found: $file"; exit 2; }
    mapfile -t CAREERS < <(grep -vE '^[[:space:]]*(#|$)' "$file" | awk '{print $1}')
    [[ ${#CAREERS[@]} -gt 0 ]] || { log "no careers in $file"; exit 2; }
}

self_test() {
    local dir fake trace scenario
    dir="$(mktemp -d)"
    trap 'rm -rf "${dir:-}"' EXIT
    fake="$dir/cell.sh"
    cat > "$fake" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
model="$1"
phase="${SDB_CELL_PHASE:?}"
worker="${SDB_CELL_WORKER:?}"
chain="${SDB_CELL_CHAIN:-}"
id="fake-$model-p$phase"
if [[ "$phase" -gt 1 && "$chain" != "fake-$model-p$((phase - 1))" ]]; then
    echo "bad chain for $model phase $phase: '$chain'" >&2
    exit 1
fi
printf '%s\t%s\t%s\t%s\n' "$model" "$phase" "$worker" "$chain" >> "${SDB_GRID_TRACE:?}"
if [[ "${SDB_FAKE_SLEEP_ID:-}" == "$id" ]]; then
    sleep "${SDB_FAKE_SLEEP:-0.3}"
fi
stop=0
[[ "${SDB_FAKE_STOP_ID:-}" == "$id" ]] && stop=1
python3 - "$SDB_CELL_RESULT" "$id" "$phase" "$worker" "$chain" "$stop" "${SDB_FAKE_COST:-0.1}" <<'PY'
import json, sys
out, cid, phase, worker, chain, stop, cost = sys.argv[1:8]
json.dump({
    "campaign_id": cid,
    "phase": int(phase),
    "worker": int(worker),
    "chain": chain,
    "cost_usd": float(cost),
    "points": 10.0,
    "over_budget": False,
    "over_wall": stop == "1",
    "harness_fault": None,
    "infra_error": False,
    "wall_stop": stop == "1",
    "phase_complete": stop != "1",
}, open(out, "w"))
PY
EOF
    chmod +x "$fake"
    CELL="$fake"
    SDB_GRID_SKIP_PREFLIGHT=1
    export SDB_GRID_SKIP_PREFLIGHT

    # 1) three lanes, six careers: 24 rows, phases in order, one worker
    # per lane, no two lanes share a worker.
    scenario="$dir/one"
    mkdir -p "$scenario"
    printf '%s\n' c1 c2 c3 c4 c5 c6 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=3
    CAP=0
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    trace="$scenario/trace.tsv"
    SDB_GRID_TRACE="$trace"
    export SDB_GRID_TRACE
    unset SDB_FAKE_STOP_ID SDB_FAKE_SLEEP_ID || true
    load_careers "$CAREERS_FILE"
    run_grid
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 24 ]] || {
        echo "lane run: expected 24 ledger rows, got $(($(wc -l < "$LEDGER") - 1))"; exit 1; }
    for c in c1 c2 c3 c4 c5 c6; do
        got="$(awk -F'\t' -v c="$c" 'NR > 1 && $1 == c { printf "%s ", $2 }' "$LEDGER")"
        [[ "$got" == "1 2 3 4 " ]] || { echo "$c phases: $got"; exit 1; }
    done
    for w in 1 2 3; do
        n="$(awk -F'\t' -v w="$w" 'NR > 1 && $3 == w { n++ } END { print n + 0 }' "$LEDGER")"
        [[ "$n" -gt 0 ]] || { echo "worker $w never ran"; exit 1; }
    done
    # Every career's chain was checked by the fake cell itself.

    # 2) a STOP on one lane starts no phase on any lane.
    scenario="$dir/two"
    mkdir -p "$scenario"
    printf '%s\n' c1 c2 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=2
    CAP=0
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    trace="$scenario/trace.tsv"
    SDB_GRID_TRACE="$trace"
    export SDB_GRID_TRACE
    SDB_FAKE_STOP_ID="fake-c1-p1"
    SDB_FAKE_SLEEP_ID="fake-c2-p1"
    SDB_FAKE_SLEEP=0.3
    export SDB_FAKE_STOP_ID SDB_FAKE_SLEEP_ID SDB_FAKE_SLEEP
    load_careers "$CAREERS_FILE"
    run_grid
    [[ -f "$STOP" ]] || { echo "the STOP file was not written"; exit 1; }
    bad="$(awk -F'\t' 'NR > 1 && $2 >= 2 { n++ } END { print n + 0 }' "$LEDGER")"
    [[ "$bad" -eq 0 ]] || { echo "$bad phase(s) started after the STOP"; exit 1; }
    unset SDB_FAKE_STOP_ID SDB_FAKE_SLEEP_ID SDB_FAKE_SLEEP

    # 3) the cap counts every lane and stops new phases.
    scenario="$dir/three"
    mkdir -p "$scenario"
    printf '%s\n' c1 c2 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=1
    CAP=0.15
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    trace="$scenario/trace.tsv"
    SDB_GRID_TRACE="$trace"
    export SDB_GRID_TRACE
    SDB_FAKE_COST=0.1
    export SDB_FAKE_COST
    load_careers "$CAREERS_FILE"
    run_grid
    [[ -f "$STOP" ]] || { echo "the cap did not write STOP"; exit 1; }
    total="$(ledger_total)"
    awk -v t="$total" 'BEGIN { exit !(t >= 0.2) }' || {
        echo "the cap ignored the lane spend: $total"; exit 1; }
    echo "RESULT: PASS (grid self-test)"
}

if [[ "$SELF_TEST" -eq 1 ]]; then
    self_test
    exit 0
fi

[[ -n "$CAREERS_FILE" ]] || { log "--careers is required"; usage; }
load_careers "$CAREERS_FILE"
if [[ "$DRY_RUN" -eq 1 ]]; then
    for ((lane = 0; lane < LANES; lane++)); do
        echo "lane $lane worker $((WORKER_BASE + lane)):" \
            "$(for i in "${!CAREERS[@]}"; do (( i % LANES == lane )) && printf '%s ' "${CAREERS[$i]}"; done)"
    done
    echo "ledger: $LEDGER"
    echo "hold:   $HOLD"
    echo "stop:   $STOP"
    exit 0
fi
run_grid
