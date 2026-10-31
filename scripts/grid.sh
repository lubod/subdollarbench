#!/usr/bin/env bash
# R48 F4: the grid driver, in-repo.
#
# `--lanes N` runs N careers at once, lane i on worker i+1. A lane runs
# each of its careers' phases 1-4 in order (phases chain). All lanes share
# one ledger and one spend cap; a STOP on any lane stops new phases on
# every lane and lets the running phases finish. The hold file pauses
# every lane at its next phase boundary.
#
# R48-1: a phase reserves its budget under the ledger's flock before it
# starts (`spent + budget * (running + 1) <= cap`), so lanes that are
# still running count against the cap. The reservation is released when
# the phase's ledger row lands.
#
# R48-2: the ledger's first column is the career's index in the careers
# file, so the same model can run several careers. A restart reads the
# ledger: a career skips the phases with an `ok` row and chains from the
# last one. A career whose latest row is not `ok` is skipped (and the
# driver says so) until the operator removes or marks that row.
#
# The cell is `scripts/sql-cell.sh` by default (or `SDB_GRID_CELL`). It is
# called with `SDB_CELL_CAREER`, `SDB_CELL_PHASE`, `SDB_CELL_CHAIN`,
# `SDB_CELL_WORKER` and `SDB_CELL_RESULT`; the result JSON is the phase's
# outcome.
#
# Usage:
#   scripts/grid.sh --careers careers.txt --lanes 3 [--cap 14.85]
#   scripts/grid.sh --self-test
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$ROOT/scripts/long-test-lock.sh"
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
RUNNING_DIR="${LEDGER}.running"

ledger_total() {
    awk -F'\t' 'NR > 1 { s += $7 } END { printf "%.6f", s }' "$LEDGER" 2>/dev/null || echo 0
}

ledger_add() {
    # career model phase worker campaign_id points cost stop token
    local career="$1" model="$2" phase="$3" worker="$4" id="$5"
    local points="$6" cost="$7" stop="$8" token="$9"
    (
        flock 200
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$career" "$model" "$phase" "$worker" "$id" "$points" "$cost" \
            "$stop" "$(date -u +%FT%TZ)" >> "$LEDGER"
        if [[ -n "$token" ]]; then rm -f "$token"; fi
    ) 200>>"$LEDGER.lock"
}

# R48-1: reserve a phase's budget atomically with the ledger spend. Prints
# the reservation token, or returns 1 when the cap would be exceeded.
reserve_phase() {
    local budget="$1"
    if [[ "$CAP" == "0" || -z "$CAP" ]]; then
        return 0
    fi
    mkdir -p "$RUNNING_DIR"
    local token
    token="$(
        (
            flock 200
            local running spent
            running=$(find "$RUNNING_DIR" -maxdepth 1 -type f -name 'res-*' | wc -l)
            spent=$(ledger_total)
            if awk -v s="$spent" -v b="$budget" -v n="$running" -v c="$CAP" \
                'BEGIN { exit !(c > 0 && s + b * (n + 1) > c) }'; then
                exit 3
            fi
            mktemp "$RUNNING_DIR/res-$$-XXXXXX"
        ) 200>>"$LEDGER.lock"
    )" || return 1
    printf '%s' "$token"
}

release_phase() {
    [[ -n "${1:-}" ]] && rm -f "$1"
    return 0
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
    # R48-5: no phase starts next to a full gate or a flake bar.
    if long_test_lock_busy; then
        log "preflight: a long test (full gate or flake bar) holds the long-test lock"
        return 1
    fi
    # The self-test checks the lock alone; the tree, record and ports need
    # the real repo/ledger.
    if [[ "${SDB_GRID_PREFLIGHT_TEST:-0}" == "1" ]]; then
        return 0
    fi
    if [[ -n "$(cd "$ROOT" && git status --porcelain --untracked-files=no)" ]]; then
        log "preflight: the tracked tree is dirty"
        return 1
    fi
    local key record
    # R52 F4: one key computation for the bar, the cell and the grid.
    key="$(cd "$ROOT" && scripts/flake-bar.sh --key)"
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

# R48-2: the career's latest row, as `phase<TAB>campaign_id<TAB>stop`.
career_state() {
    local idx="$1"
    (
        flock 200
        awk -F'\t' -v c="$idx" '
            NR > 1 && $1 == c {
                p = $3 + 0
                if (p >= best) { best = p; id = $5; stop = $8 }
            }
            END { if (best > 0) printf "%s\t%s\t%s\n", best, id, stop }
        ' "$LEDGER" 2>/dev/null || true
    ) 200>>"$LEDGER.lock"
}

run_cell() {
    local career="$1" model="$2" phase="$3" chain="$4" worker="$5" result="$6"
    SDB_CELL_CAREER="$career" \
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
    local idx model phase chain result id rc token last_phase last_id last_stop start_phase
    local failed=0
    for idx in "${!CAREERS[@]}"; do
        model="${CAREERS[$idx]}"
        if (( idx % LANES != lane )); then
            continue
        fi
        last_phase=""
        last_id=""
        last_stop=""
        if ! read -r last_phase last_id last_stop < <(career_state "$idx"); then
            last_phase=""
            last_id=""
            last_stop=""
        fi
        if [[ -n "${last_phase:-}" && "${last_stop:-}" != "ok" ]]; then
            log "lane $lane career $idx ($model): phase $last_phase ended '${last_stop:-?}'; skipped until its row is removed"
            continue
        fi
        if [[ -n "${last_phase:-}" ]]; then
            start_phase=$((last_phase + 1))
            chain="$last_id"
        else
            start_phase=1
            chain=""
        fi
        if (( start_phase > 4 )); then
            continue
        fi
        for ((phase = start_phase; phase <= 4; phase++)); do
            if [[ -f "$STOP" ]]; then
                log "lane $lane worker $worker: STOP before $model phase $phase"
                return "$failed"
            fi
            wait_hold
            token="$(reserve_phase "$PHASE_BUDGET")" || {
                log "lane $lane: cap $CAP reached (reservations count); STOP"
                touch "$STOP"
                return "$failed"
            }
            if ! preflight "$worker"; then
                release_phase "$token"
                log "lane $lane: preflight failed for career $idx $model phase $phase"
                touch "$STOP"
                return 1
            fi
            result="$(mktemp)"
            rc=0
            run_cell "$idx" "$model" "$phase" "$chain" "$worker" "$result" || rc=$?
            id="$(result_get "$result" campaign_id)"
            local got_phase got_chain points cost stop
            got_phase="$(result_get "$result" phase)"
            got_chain="$(result_get "$result" chain_from)"
            points="$(result_get "$result" points)"
            cost="$(result_get "$result" cost_usd)"
            stop="ok"
            if (( rc != 0 )); then
                stop="cell exit $rc"
            elif [[ "$got_phase" != "$phase" ]]; then
                # R48-4: the rollup's phase, not the request's, decides.
                stop="phase ${got_phase:-null}"
            elif [[ "$got_chain" != "$chain" ]]; then
                stop="chain ${got_chain:-null}"
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
            ledger_add "$idx" "$model" "$phase" "$worker" "${id:-—}" "${points:-0}" "${cost:-0}" "$stop" "$token"
            rm -f "$result"
            if [[ "$stop" != "ok" ]]; then
                log "lane $lane: career $idx $model phase $phase stopped: $stop"
                touch "$STOP"
                return "$failed"
            fi
            chain="$id"
            [[ -z "$chain" ]] && {
                log "lane $lane: career $idx $model phase $phase has no campaign id"
                touch "$STOP"
                return 1
            }
        done
    done
    return "$failed"
}

run_grid() {
    RUNNING_DIR="${LEDGER}.running"
    mkdir -p "$(dirname "$LEDGER")"
    # R48-7: the ledger's career column is the line index in the careers
    # file. Freeze that file next to the ledger on the first start; a later
    # start with a different file would give the same index to another
    # career, so refuse before any phase.
    local careers_copy="${LEDGER}.careers"
    if [[ -f "$careers_copy" ]]; then
        if ! cmp -s "$CAREERS_FILE" "$careers_copy"; then
            log "careers file changed since this grid started ($careers_copy); start a new grid with a new --ledger"
            return 1
        fi
    else
        cp "$CAREERS_FILE" "$careers_copy"
    fi
    rm -rf "$RUNNING_DIR"
    mkdir -p "$RUNNING_DIR"
    rm -f "$STOP"
    if [[ ! -f "$LEDGER" ]]; then
        printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\n' > "$LEDGER"
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
    rm -rf "$RUNNING_DIR"
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
career="${SDB_CELL_CAREER:?}"
phase="${SDB_CELL_PHASE:?}"
worker="${SDB_CELL_WORKER:?}"
chain="${SDB_CELL_CHAIN:-}"
id="fake-c$career-$model-p$phase"
if [[ "$phase" -gt 1 && "$chain" != "fake-c$career-$model-p$((phase - 1))" ]]; then
    echo "bad chain for career $career $model phase $phase: '$chain'" >&2
    exit 1
fi
printf '%s\t%s\t%s\t%s\n' "$career" "$model" "$phase" "$chain" >> "${SDB_GRID_TRACE:?}"
if [[ "${SDB_FAKE_SLEEP_ALL:-0}" == "1" || "${SDB_FAKE_SLEEP_ID:-}" == "$id" ]]; then
    sleep "${SDB_FAKE_SLEEP:-0.3}"
fi
stop=0
[[ "${SDB_FAKE_STOP_ID:-}" == "$id" ]] && stop=1
res_phase="${SDB_FAKE_RESULT_PHASE:-$phase}"
res_chain="${SDB_FAKE_RESULT_CHAIN-$chain}"
python3 - "$SDB_CELL_RESULT" "$id" "$phase" "$worker" "$chain" "$stop" "${SDB_FAKE_COST:-0.1}" "$res_phase" "$res_chain" <<'PY'
import json, sys
out, cid, phase, worker, chain, stop, cost, res_phase, res_chain = sys.argv[1:10]
json.dump({
    "campaign_id": cid,
    "phase": int(res_phase),
    "chain_from": res_chain,
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

    # 1) three lanes, six careers: 24 rows, phases in order, one worker per
    # lane.
    scenario="$dir/one"
    mkdir -p "$scenario"
    printf '%s\n' c1 c2 c3 c4 c5 c6 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=3
    CAP=0
    PHASE_BUDGET=0.99
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    unset SDB_FAKE_STOP_ID SDB_FAKE_SLEEP_ID SDB_FAKE_COST || true
    load_careers "$CAREERS_FILE"
    run_grid
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 24 ]] || {
        echo "lane run: expected 24 ledger rows, got $(($(wc -l < "$LEDGER") - 1))"; exit 1; }
    for c in 0 1 2 3 4 5; do
        got="$(awk -F'\t' -v c="$c" 'NR > 1 && $1 == c { printf "%s ", $3 }' "$LEDGER")"
        [[ "$got" == "1 2 3 4 " ]] || { echo "career $c phases: $got"; exit 1; }
    done
    for w in 1 2 3; do
        n="$(awk -F'\t' -v w="$w" 'NR > 1 && $4 == w { n++ } END { print n + 0 }' "$LEDGER")"
        [[ "$n" -gt 0 ]] || { echo "worker $w never ran"; exit 1; }
    done

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
    SDB_GRID_TRACE="$scenario/trace.tsv"
    SDB_FAKE_STOP_ID="fake-c0-c1-p1"
    SDB_FAKE_SLEEP_ID="fake-c1-c2-p1"
    SDB_FAKE_SLEEP=0.3
    export SDB_GRID_TRACE SDB_FAKE_STOP_ID SDB_FAKE_SLEEP_ID SDB_FAKE_SLEEP
    load_careers "$CAREERS_FILE"
    run_grid
    [[ -f "$STOP" ]] || { echo "the STOP file was not written"; exit 1; }
    bad="$(awk -F'\t' 'NR > 1 && $3 >= 2 { n++ } END { print n + 0 }' "$LEDGER")"
    [[ "$bad" -eq 0 ]] || { echo "$bad phase(s) started after the STOP"; exit 1; }
    unset SDB_FAKE_STOP_ID SDB_FAKE_SLEEP_ID SDB_FAKE_SLEEP

    # 3) R48-1: the cap counts running phases. Three lanes at $0.99 with
    # a $2.00 cap and an empty ledger: the first two reserve $0.99 each
    # (1.98 <= 2.00) and sleep 1 s; the third would need 2.97, so it stops
    # before starting. Exactly two rows land, total $1.98. The old
    # spent-only check started all three and recorded $2.97.
    scenario="$dir/three"
    mkdir -p "$scenario"
    printf '%s\n' c1 c2 c3 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=3
    CAP=2.0
    PHASE_BUDGET=0.99
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    SDB_FAKE_COST=0.99
    SDB_FAKE_SLEEP_ALL=1
    SDB_FAKE_SLEEP=1
    export SDB_GRID_TRACE SDB_FAKE_COST SDB_FAKE_SLEEP_ALL SDB_FAKE_SLEEP
    printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\n' > "$LEDGER"
    load_careers "$CAREERS_FILE"
    run_grid
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 2 ]] || { echo "cap run: expected 2 rows, got $rows"; exit 1; }
    total="$(ledger_total)"
    awk -v t="$total" 'BEGIN { exit !(t > 1.979999 && t < 1.980001) }' || {
        echo "cap run: expected 1.98, got $total"; exit 1; }
    [[ -f "$STOP" ]] || { echo "the cap did not write STOP"; exit 1; }
    unset SDB_FAKE_COST SDB_FAKE_SLEEP_ALL SDB_FAKE_SLEEP
    export -n SDB_FAKE_COST SDB_FAKE_SLEEP_ALL SDB_FAKE_SLEEP 2>/dev/null || true

    # 4) R48-2: resume skips ok phases and chains from the career's own
    # last id; two careers of the same model stay separate.
    scenario="$dir/four"
    mkdir -p "$scenario"
    printf '%s\n' c1 c1 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\n' > "$LEDGER"
    printf '0\tc1\t1\t1\tfake-c0-c1-p1\t10\t0.1\tok\t2026-10-29T00:00:00Z\n' >> "$LEDGER"
    printf '0\tc1\t2\t1\tfake-c0-c1-p2\t10\t0.1\tok\t2026-10-29T00:00:00Z\n' >> "$LEDGER"
    printf '1\tc1\t1\t2\tfake-c1-c1-p1\t10\t0.1\tok\t2026-10-29T00:00:00Z\n' >> "$LEDGER"
    load_careers "$CAREERS_FILE"
    run_grid
    [[ "$(awk -F'\t' 'NR > 1 && $1 == 0 && $3 == 1 { n++ } END { print n + 0 }' "$LEDGER")" -eq 1 ]] || {
        echo "resume re-ran career 0 phase 1"; exit 1; }
    [[ "$(awk -F'\t' 'NR > 1 && $1 == 1 && $3 == 1 { n++ } END { print n + 0 }' "$LEDGER")" -eq 1 ]] || {
        echo "resume re-ran career 1 phase 1"; exit 1; }
    got="$(awk -F'\t' 'NR > 1 && $1 == 0 { printf "%s ", $3 }' "$LEDGER")"
    [[ "$got" == "1 2 3 4 " ]] || { echo "resumed career 0 phases: $got"; exit 1; }
    got="$(awk -F'\t' 'NR > 1 && $1 == 1 { printf "%s ", $3 }' "$LEDGER")"
    [[ "$got" == "1 2 3 4 " ]] || { echo "resumed career 1 phases: $got"; exit 1; }

    # 5) R48-2: a career whose last row is not ok is skipped; the other
    # careers run.
    scenario="$dir/five"
    mkdir -p "$scenario"
    printf '%s\n' c1 c1 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=1
    CAP=0
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\n' > "$LEDGER"
    printf '0\tc1\t1\t1\tfake-c0-c1-p1\t10\t0.1\tok\t2026-10-29T00:00:00Z\n' >> "$LEDGER"
    printf '1\tc1\t1\t1\tfake-c1-c1-p1\t10\t0.1\tok\t2026-10-29T00:00:00Z\n' >> "$LEDGER"
    printf '1\tc1\t2\t1\tfake-c1-c1-p2\t10\t0.1\tover wall\t2026-10-29T00:00:00Z\n' >> "$LEDGER"
    load_careers "$CAREERS_FILE"
    run_grid
    got="$(awk -F'\t' 'NR > 1 && $1 == 0 { printf "%s ", $3 }' "$LEDGER")"
    [[ "$got" == "1 2 3 4 " ]] || { echo "career 0 did not resume: $got"; exit 1; }
    got="$(awk -F'\t' 'NR > 1 && $1 == 1 { printf "%s ", $3 }' "$LEDGER")"
    [[ "$got" == "1 2 " ]] || { echo "the blocked career 1 ran: $got"; exit 1; }

    # 6) R48-4: the result's rollup phase, not the request, decides. A
    # phase-1 request whose rollup says phase 2 stops the grid and records
    # the reason in the ledger.
    scenario="$dir/six"
    mkdir -p "$scenario"
    printf '%s\n' c1 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=1
    CAP=0
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    SDB_FAKE_RESULT_PHASE=2
    export SDB_FAKE_RESULT_PHASE
    load_careers "$CAREERS_FILE"
    run_grid || true
    got="$(awk -F'\t' 'NR > 1 { s = $8 } END { print s }' "$LEDGER")"
    [[ "$got" == "phase 2" ]] || { echo "phase mismatch stop: '$got'"; exit 1; }
    [[ -f "$STOP" ]] || { echo "phase mismatch did not STOP"; exit 1; }
    unset SDB_FAKE_RESULT_PHASE
    export -n SDB_FAKE_RESULT_PHASE 2>/dev/null || true

    # 7) R48-4: a rollup chain_from that differs from the chain passed stops
    # the grid too.
    scenario="$dir/seven"
    mkdir -p "$scenario"
    printf '%s\n' c1 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=1
    CAP=0
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    SDB_FAKE_RESULT_CHAIN=bogus
    export SDB_FAKE_RESULT_CHAIN
    load_careers "$CAREERS_FILE"
    run_grid || true
    got="$(awk -F'\t' 'NR > 1 { s = $8 } END { print s }' "$LEDGER")"
    [[ "$got" == "chain bogus" ]] || { echo "chain mismatch stop: '$got'"; exit 1; }
    [[ -f "$STOP" ]] || { echo "chain mismatch did not STOP"; exit 1; }
    unset SDB_FAKE_RESULT_CHAIN

    # 8) R48-5: preflight refuses while a long test holds the lock and
    # passes once it is released. SDB_GRID_PREFLIGHT_TEST keeps the
    # tree/record/port checks out of this scenario.
    scenario="$dir/eight"
    mkdir -p "$scenario"
    lock="$scenario/long-test.lock"
    (
        . "$ROOT/scripts/long-test-lock.sh"
        SDB_LONG_TEST_LOCK="$lock" hold_long_test_lock
        sleep 5
    ) &
    holder=$!
    sleep 0.2
    if ( export SDB_GRID_SKIP_PREFLIGHT=0 SDB_GRID_PREFLIGHT_TEST=1 SDB_LONG_TEST_LOCK="$lock"; preflight 1 ); then
        echo "preflight passed while the long-test lock was held"
        kill "$holder" 2>/dev/null || true
        exit 1
    fi
    kill "$holder" 2>/dev/null || true
    wait "$holder" 2>/dev/null || true
    if ! ( export SDB_GRID_SKIP_PREFLIGHT=0 SDB_GRID_PREFLIGHT_TEST=1 SDB_LONG_TEST_LOCK="$lock"; preflight 1 ); then
        echo "preflight failed while the long-test lock was free"
        exit 1
    fi

    # 9) R48-7: the careers file is frozen with the ledger. A restart with
    # an edited file refuses and records no row; restoring it resumes.
    scenario="$dir/nine"
    mkdir -p "$scenario"
    printf '%s\n' c1 c2 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=2
    CAP=0
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    load_careers "$CAREERS_FILE"
    run_grid
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 8 ]] || { echo "frozen run: expected 8 rows, got $rows"; exit 1; }
    printf '%s\n' c1 c3 > "$scenario/careers.txt"
    rc=0
    run_grid || rc=$?
    [[ "$rc" -eq 1 ]] || { echo "an edited careers file was accepted (rc=$rc)"; exit 1; }
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 8 ]] || {
        echo "an edited careers file recorded a row"; exit 1; }
    printf '%s\n' c1 c2 > "$scenario/careers.txt"
    run_grid
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 8 ]] || {
        echo "a matching restart recorded a row"; exit 1; }

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
            "$(for i in "${!CAREERS[@]}"; do (( i % LANES == lane )) && printf '%s#%s ' "${CAREERS[$i]}" "$i"; done)"
    done
    echo "ledger: $LEDGER"
    echo "hold:   $HOLD"
    echo "stop:   $STOP"
    exit 0
fi
run_grid
