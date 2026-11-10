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
REAL=0
REHEARSAL=0
DRY_RUN=0
WORKER_BASE=1
MAX_PHASE=4
BUDGET_SET=0
LEDGER_SET=0
STOP_SET=0

log() { echo "[grid] $*" >&2; }

usage() {
    cat >&2 <<'EOF'
usage: grid.sh --careers <file> --lanes <n> [--cap <usd>] [--budget <usd>]
               [--wall <min>] [--ledger <file>] [--hold <file>] [--stop <file>]
               [--cell <path>] [--dry-run] [--max-phase <n>]
       grid.sh --rehearsal --careers <file> --lanes <n> [--cap <usd>]
       grid.sh --self-test [--real]
EOF
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --careers) CAREERS_FILE="$2"; shift 2 ;;
        --lanes) LANES="$2"; shift 2 ;;
        --cap) CAP="$2"; shift 2 ;;
        --budget) PHASE_BUDGET="$2"; BUDGET_SET=1; shift 2 ;;
        --wall) PHASE_WALL="$2"; shift 2 ;;
        --ledger) LEDGER="$2"; LEDGER_SET=1; shift 2 ;;
        --hold) HOLD="$2"; shift 2 ;;
        --stop) STOP="$2"; STOP_SET=1; shift 2 ;;
        --cell) CELL="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        --real) REAL=1; shift ;;
        --rehearsal) REHEARSAL=1; shift ;;
        --max-phase) MAX_PHASE="$2"; shift 2 ;;
        -h|--help) usage ;;
        *) log "unknown argument: $1"; usage ;;
    esac
done

[[ "$LANES" =~ ^[1-9][0-9]*$ ]] || { log "--lanes must be a positive integer"; exit 2; }
[[ "$CAP" =~ ^[0-9]+([.][0-9]+)?$ ]] || { log "--cap must be a non-negative number"; exit 2; }
[[ "$MAX_PHASE" =~ ^[1-4]$ ]] || { log "--max-phase must be 1-4"; exit 2; }
if [[ "$REAL" == "1" && "$SELF_TEST" != "1" ]]; then
    log "--real needs --self-test"
    exit 2
fi
if [[ "$REHEARSAL" == "1" && "$BUDGET_SET" != "1" ]]; then
    # A rehearsal spends nothing; the ledger and the cap stay at $0.
    PHASE_BUDGET=0
fi
if [[ "$REHEARSAL" == "1" ]]; then
    # R54-1: a rehearsal must never feed the paid grid's resume table.
    # The hold file stays shared so `gap` pauses both.
    [[ "$LEDGER_SET" == "1" ]] || LEDGER="$RUNS/.grid-rehearsal-ledger.tsv"
    [[ "$STOP_SET" == "1" ]] || STOP="$RUNS/.grid-rehearsal-stop"
fi
LEDGER="${LEDGER:-$RUNS/.grid-ledger.tsv}"
HOLD="${HOLD:-$RUNS/.grid-hold}"
STOP="${STOP:-$RUNS/.grid-stop}"
RUNNING_DIR="${LEDGER}.running"

ledger_total() {
    awk -F'\t' 'NR > 1 { s += $7 } END { printf "%.6f", s }' "$LEDGER" 2>/dev/null || echo 0
}

# R54-1: rows whose campaign rollup says rehearsal/smoke, as
# `<mark>TAB<row>`. A missing rollup is not a mark (stub ledgers and old
# rows stay readable).
ledger_marked_rows() {
    [[ -f "$LEDGER" ]] || return 0
    local row id file mark
    while IFS= read -r row; do
        id="$(printf '%s\n' "$row" | awk -F'\t' '{print $5}')"
        [[ -n "$id" && "$id" != "—" && "$id" != "-" ]] || continue
        file="$RUNS/$id/campaign.json"
        [[ -f "$file" ]] || continue
        mark="$(python3 - "$file" <<'PYEOF'
import json, sys
try:
    r = json.load(open(sys.argv[1]))
except Exception:
    raise SystemExit(0)
for key in ("rehearsal", "smoke"):
    if r.get(key):
        print(key)
        break
PYEOF
)"
        if [[ -n "$mark" ]]; then
            printf '%s\t%s\n' "$mark" "$row"
        fi
    done < <(tail -n +2 "$LEDGER" 2>/dev/null || true)
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

# R65 F3: the account balance, in dollars, from OpenRouter's credits
# endpoint (total_credits - total_usage). SDB_GRID_BALANCE_CMD is the test
# seam: it prints a number or fails. The key goes through curl's stdin
# config, never argv, and is never printed.
account_balance() {
    if [[ -n "${SDB_GRID_BALANCE_CMD:-}" ]]; then
        "$SDB_GRID_BALANCE_CMD"
        return
    fi
    local key
    # The repo's .env is the source of truth (a stale shell key burned a
    # cell on 2026-10-18); fall back to the environment.
    key=""
    if [[ -f "$ROOT/.env" ]]; then
        key="$(grep -m1 '^OPENROUTER_API_KEY=' "$ROOT/.env" | cut -d= -f2- | tr -d '"' | tr -d "'")"
    fi
    if [[ -z "$key" ]]; then
        key="${OPENROUTER_API_KEY:-}"
    fi
    if [[ -z "$key" ]]; then
        log "balance: OPENROUTER_API_KEY is not set"
        return 1
    fi
    local body
    body="$(printf 'header = "Authorization: Bearer %s"\n' "$key" \
        | curl -sS --max-time 20 -K - https://openrouter.ai/api/v1/credits)" || {
        log "balance: the credits endpoint failed"
        return 1
    }
    printf '%s' "$body" | python3 -c 'import json, sys
try:
    d = json.load(sys.stdin)["data"]
    print(float(d["total_credits"]) - float(d["total_usage"]))
except Exception:
    raise SystemExit(1)'
}

# R65 F3: print the hold reason and return 1 when the phase may not start.
# `running` in the spec is the other lanes; the reserved set already holds
# this phase when CAP > 0, and the whole lane count is the worst case at
# the start. A rehearsal and a dry run spend nothing and skip.
balance_gate() {
    [[ "$REHEARSAL" == "1" || "$DRY_RUN" == "1" ]] && return 0
    local reserved other concurrent need balance
    reserved=$(find "$RUNNING_DIR" -maxdepth 1 -type f -name 'res-*' 2>/dev/null | wc -l)
    if [[ "$CAP" == "0" || -z "$CAP" ]]; then
        other=0
    else
        other=$((reserved > 0 ? reserved - 1 : 0))
    fi
    concurrent=$other
    (( concurrent < LANES - 1 )) && concurrent=$((LANES - 1))
    need="$(awk -v b="$PHASE_BUDGET" -v r="$concurrent" 'BEGIN { printf "%.2f", b * (r + 1) + 1.00 }')"
    if ! balance="$(account_balance)"; then
        printf 'balance unknown\n'
        return 1
    fi
    if awk -v b="$balance" -v n="$need" 'BEGIN { exit !(b < n) }'; then
        printf 'balance $%s below $%s\n' "$balance" "$need"
        return 1
    fi
    return 0
}

# R68 F2: the rehearsal record lives next to the bar records, keyed by the
# full content key, and names the exact commit it ran on.
rehearsal_dir() {
    if [[ -n "${SDB_REHEARSAL_DIR:-}" ]]; then
        printf '%s' "$SDB_REHEARSAL_DIR"
    else
        printf '%s/sdb-gates' \
            "$(cd "$ROOT" && git rev-parse --path-format=absolute --git-common-dir)"
    fi
}

rehearsal_record_path() {
    printf '%s/rehearsal-%s.txt' "$(rehearsal_dir)" "$1"
}

rehearsal_gate() {
    local full record commit head
    full="$(cd "$ROOT" && scripts/flake-bar.sh --key)" || {
        log "preflight: could not compute the full key"
        return 1
    }
    record="$(rehearsal_record_path "$full")"
    if [[ ! -f "$record" ]]; then
        log "preflight: no rehearsal record for full key $full ($record); run scripts/grid.sh --rehearsal"
        return 1
    fi
    if ! grep -q '^RESULT: PASS$' "$record"; then
        log "preflight: the rehearsal record for full key $full is not a PASS ($record)"
        return 1
    fi
    # Reviewer fix (2026-11-09): valid for the graded content (the full
    # key), not one commit hash; a docs-only commit keeps the key.
    if ! grep -q "^key: *rehearsal-$full\$" "$record"; then
        log "preflight: the rehearsal record does not name full key $full ($record); rerun scripts/grid.sh --rehearsal"
        return 1
    fi
    return 0
}

# R68 F2: a --rehearsal that ran every career's four phases at $0 writes
# the record sql-cell.sh and the paid preflight require. A partial
# --max-phase run writes none.
write_rehearsal_record() {
    local full record image
    if [[ "$MAX_PHASE" != "4" ]]; then
        log "rehearsal: --max-phase $MAX_PHASE is not a full 4-phase rehearsal; no record"
        return 0
    fi
    if ! python3 - "$LEDGER" "${#CAREERS[@]}" <<'PY'
import sys
ledger, careers = sys.argv[1], int(sys.argv[2])
rows = {}
for line in open(ledger):
    parts = line.rstrip("\n").split("\t")
    if len(parts) < 8 or parts[0] in ("career", ""):
        continue
    try:
        idx, phase, cost = int(parts[0]), int(parts[2]), float(parts[6])
    except ValueError:
        sys.exit(f"bad ledger row: {line!r}")
    rows.setdefault(idx, {})[phase] = (parts[7], cost)
for idx in range(careers):
    phases = rows.get(idx, {})
    for phase in range(1, 5):
        stop, cost = phases.get(phase, ("missing", 0.0))
        if stop != "ok":
            sys.exit(f"career {idx} phase {phase} ended {stop}")
        if cost != 0:
            sys.exit(f"career {idx} phase {phase} spent ${cost}")
PY
    then
        log "rehearsal: the ledger is not four ok $0 phases per career; no record"
        return 1
    fi
    full="$(cd "$ROOT" && scripts/flake-bar.sh --key)"
    record="$(rehearsal_record_path "$full")"
    image="${SDB_MODEL_CHECK_IMAGE_DIGEST:-}"
    if [[ -z "$image" ]]; then
        image="$(docker image inspect subdollar-sandbox --format '{{.Id}}' 2>/dev/null || true)"
    fi
    mkdir -p "$(dirname "$record")"
    {
        echo "commit:  $(cd "$ROOT" && git rev-parse HEAD)"
        echo "key:     rehearsal-$full"
        echo "grid:    ${#CAREERS[@]} careers, 4 phases, $LANES lanes"
        echo "image:   ${image:-unknown}"
        echo "time:    $(date -u +%FT%TZ)"
        echo "RESULT: PASS"
    } > "$record"
    log "rehearsal: wrote $record"
    return 0
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
    if [[ "$REHEARSAL" != "1" ]]; then
        local key record
        # R52 F4/R68 F1: one grader-key computation for the bar, the cell
        # and the grid; a plumbing change does not need a new bar.
        key="$(cd "$ROOT" && scripts/flake-bar.sh --grader-key)"
        record="$(cd "$ROOT" && git rev-parse --path-format=absolute --git-common-dir)/sdb-gates/flake-$key.txt"
        if ! grep -q 'RESULT: PASS' "$record" 2>/dev/null; then
            log "preflight: no PASS flake record for grader key $key ($record)"
            return 1
        fi
        # R68 F2: the $0 rehearsal must be on this exact commit.
        if ! rehearsal_gate; then
            return 1
        fi
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

# R68 F3: a paid grid needs a fresh PASS model-check record for every
# career's model, made on the current sandbox image digest.
model_check_gate() {
    local model="$1" record image record_image time_line age
    record="$("$ROOT/scripts/model-check.sh" --record "$model")" || {
        log "preflight: cannot resolve the model-check record for $model"
        return 1
    }
    if [[ ! -f "$record" ]]; then
        log "preflight: no model-check record for $model ($record); run scripts/model-check.sh $model"
        return 1
    fi
    if ! grep -q '^RESULT: PASS$' "$record"; then
        log "preflight: the model-check record for $model is not a PASS ($record)"
        return 1
    fi
    image="${SDB_MODEL_CHECK_IMAGE_DIGEST:-}"
    if [[ -z "$image" ]]; then
        image="$(docker image inspect subdollar-sandbox --format '{{.Id}}' 2>/dev/null || true)"
    fi
    if [[ -z "$image" ]]; then
        log "preflight: cannot read the subdollar-sandbox image digest for $model"
        return 1
    fi
    record_image="$(awk '$1 == "image:" { print $2; exit }' "$record")"
    if [[ "$record_image" != "$image" ]]; then
        log "preflight: the model-check record for $model is from another image ($record_image, want $image)"
        return 1
    fi
    time_line="$(awk '$1 == "time:" { print $2; exit }' "$record")"
    if [[ -z "$time_line" ]]; then
        log "preflight: the model-check record for $model has no time ($record)"
        return 1
    fi
    age="$(python3 - "$time_line" <<'PY'
import datetime, sys
try:
    t = datetime.datetime.fromisoformat(sys.argv[1].replace("Z", "+00:00"))
except ValueError:
    raise SystemExit(1)
now = datetime.datetime.now(datetime.timezone.utc)
print(int((now - t).total_seconds()))
PY
)" || {
        log "preflight: the model-check record for $model has a bad time ($time_line)"
        return 1
    }
    if (( age > 604800 )); then
        log "preflight: the model-check record for $model is older than 7 days ($time_line)"
        return 1
    fi
    if (( age < -300 )); then
        log "preflight: the model-check record for $model has a future time ($time_line)"
        return 1
    fi
    return 0
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

# R68 F5: provider tool-call errors against agent turns in a finished
# phase's live.log. The minimax phases (#29) carried 8-76% errors; the
# published models at most 0.1%. A fault needs both >2% and at least 5
# errors, so a single bad turn in a short phase is not a provider fault.
provider_fault_counts() {
    local log="$1" turns errors
    turns="$(grep -ac '\[BENCH-POLICY\] turn=' "$log" 2>/dev/null || true)"
    errors="$(grep -acE 'provider stream ended with an error|Upstream error from' "$log" 2>/dev/null || true)"
    printf '%s\t%s\n' "${turns:-0}" "${errors:-0}"
}

provider_fault() {
    local id="$1" log turns errors
    log="$RUNS/$id/live.log"
    [[ -f "$log" ]] || return 1
    read -r turns errors < <(provider_fault_counts "$log")
    [[ "$turns" -gt 0 && "$errors" -ge 5 ]] || return 1
    awk -v e="$errors" -v t="$turns" 'BEGIN { exit !(100 * e / t > 2) }'
}

# R74 F2: while a phase runs, read its health.json every poll (2 min in a
# paid grid; SDB_GRID_HEALTH_POLL overrides for tests). After 20 turns,
# tool errors over 2 % (and at least 5) or empty responses over 5 % (and
# at least 5) stop the phase: write .live_stop, the live result for the
# lane, and kill the cell's process group. The first clean 30-turn check
# on the canary lane opens the other lanes. A slow median turn or >10 %
# ref-sql refusals only warn; the console shows the amber health.
live_check() {
    local model="$1" worker="$2" phase="$3" cell_pid="$4" marker="$5" out="$6"
    local tag="${model//\//_}"; tag="${tag//:/_}"
    local poll="${SDB_GRID_HEALTH_POLL:-120}"
    local dir="" vals turns errors empty p50 refr refq rate_limited
    while kill -0 "$cell_pid" 2>/dev/null; do
        if [[ -z "$dir" ]]; then
            dir="$(find "$RUNS" -maxdepth 1 -type d -name "campaign_sdb-sql_${tag}_*_w${worker}" -newer "$marker" -print -quit 2>/dev/null || true)"
        fi
        if [[ -n "$dir" && -f "$dir/health.json" ]]; then
            vals="$(python3 - "$dir/health.json" <<'PY'
import json, sys
try:
    h = json.load(open(sys.argv[1]))
except Exception:
    raise SystemExit(1)
print(int(h.get("turns") or 0), int(h.get("tool_errors") or 0),
      int(h.get("empty_responses") or 0), float(h.get("turn_secs_p50") or 0),
      int(h.get("ref_refused") or 0), int(h.get("ref_queries") or 0),
      int(h.get("rate_limited") or 0))
PY
)" || vals=""
            if [[ -n "$vals" ]]; then
                read -r turns errors empty p50 refr refq rate_limited <<< "$vals"
                if (( turns >= 20 && errors >= 5 )) \
                    && awk -v e="$errors" -v t="$turns" 'BEGIN { exit !(100 * e / t > 2) }'; then
                    printf '%s\t%s\t%s\n' "$(basename "$dir")" "$turns" "$errors" > "$out"
                    printf 'live provider fault: %s errors in %s turns\n' "$errors" "$turns" > "$dir/.live_stop"
                    [[ "$worker" == "$WORKER_BASE" ]] && : > "$RUNS/.canary_failed"
                    kill -- -"$cell_pid" 2>/dev/null || kill "$cell_pid" 2>/dev/null || true
                    return 0
                fi
                if (( turns >= 20 && empty >= 5 )) \
                    && awk -v e="$empty" -v t="$turns" 'BEGIN { exit !(100 * e / t > 5) }'; then
                    printf '%s\t%s\t%s\n' "$(basename "$dir")" "$turns" "$errors" > "$out"
                    printf 'live provider fault: %s empty responses in %s turns\n' "$empty" "$turns" > "$dir/.live_stop"
                    [[ "$worker" == "$WORKER_BASE" ]] && : > "$RUNS/.canary_failed"
                    kill -- -"$cell_pid" 2>/dev/null || kill "$cell_pid" 2>/dev/null || true
                    return 0
                fi
                if (( turns >= 30 )) && [[ "$worker" == "$WORKER_BASE" && ! -f "$RUNS/.canary_failed" ]]; then
                    : > "$RUNS/.canary_ok"
                fi
                if awk -v p="$p50" 'BEGIN { exit !(p > 180) }'; then
                    log "live health warning: worker $worker phase $phase median turn ${p50}s"
                fi
                if (( refq > 0 )) && awk -v r="$refr" -v q="$refq" 'BEGIN { exit !(100 * r / q > 10) }'; then
                    log "live health warning: worker $worker phase $phase ref-sql ${refr}/${refq} refused"
                fi
                # R77 F2: transient 429s warn above 10; they never stop.
                if (( rate_limited > 10 )); then
                    log "live health warning: worker $worker phase $phase $rate_limited rate limits"
                fi
            fi
        fi
        sleep "$poll"
    done
}

run_cell() {
    local career="$1" model="$2" phase="$3" chain="$4" worker="$5" result="$6"
    exec setsid env SDB_CELL_CAREER="$career" \
    SDB_CELL_PHASE="$phase" \
    SDB_CELL_CHAIN="$chain" \
    SDB_CELL_WORKER="$worker" \
    SDB_CELL_REHEARSAL="$REHEARSAL" \
    SDB_CONCURRENT_CELLS="$LANES" \
    SDB_CELL_RESULT="$result" \
        "$CELL" "$model" "$PHASE_BUDGET" "$PHASE_WALL"
}

run_lane() {
    local lane="$1"
    local worker=$((WORKER_BASE + lane))
    local idx model phase chain result id rc token last_phase last_id last_stop start_phase
    local failed=0 provider_turns=0 provider_errors=0 faults=0 first_fault=""
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
        if (( start_phase > MAX_PHASE )); then
            continue
        fi
        for ((phase = start_phase; phase <= MAX_PHASE; phase++)); do
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
            local hold_reason
            if ! hold_reason="$(balance_gate)"; then
                release_phase "$token"
                printf '%s\n' "hold: $hold_reason" > "$HOLD"
                log "hold: $hold_reason (release $HOLD after a top-up)"
                wait_hold
                token="$(reserve_phase "$PHASE_BUDGET")" || {
                    log "lane $lane: cap $CAP reached (reservations count); STOP"
                    touch "$STOP"
                    return "$failed"
                }
            fi
            if ! preflight "$worker"; then
                release_phase "$token"
                log "lane $lane: preflight failed for career $idx $model phase $phase"
                touch "$STOP"
                return 1
            fi
            result="$(mktemp)"
            live_result="$(mktemp)"
            marker="$RUNS/.grid-live-marker-$worker-$phase-$$"
            : > "$marker"
            rc=0
            run_cell "$idx" "$model" "$phase" "$chain" "$worker" "$result" &
            cell_pid=$!
            live_check "$model" "$worker" "$phase" "$cell_pid" "$marker" "$live_result" &
            monitor_pid=$!
            wait "$cell_pid" || rc=$?
            kill "$monitor_pid" 2>/dev/null || true
            wait "$monitor_pid" 2>/dev/null || true
            rm -f "$marker"
            id="$(result_get "$result" campaign_id)"
            local got_phase got_chain points cost stop billed
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
            if [[ -s "$live_result" ]]; then
                read -r live_id provider_turns provider_errors < "$live_result"
                [[ -n "$live_id" ]] && id="$live_id"
                stop="provider_fault"
            fi
            # R68 F5: a finished phase whose live.log carries provider
            # tool-call errors above 2% of its turns is a provider fault,
            # not a model result. The ledger row says so and the run is
            # hidden before anything can publish it.
            if [[ "$stop" == "ok" && -n "$id" && "$id" != "—" ]] && provider_fault "$id"; then
                read -r provider_turns provider_errors < <(provider_fault_counts "$RUNS/$id/live.log")
                stop="provider_fault"
            fi
            billed="$(result_get "$result" billed_usd)"
            ledger_add "$idx" "$model" "$phase" "$worker" "${id:-—}" "${points:-0}" "${cost:-0}" "$stop" "$token"
            rm -f "$result" "$live_result"
            if [[ "$stop" == "provider_fault" ]]; then
                mkdir -p "$RUNS/$id"
                printf 'provider fault: %s errors in %s turns (%.2f%%)\n' \
                    "$provider_errors" "$provider_turns" \
                    "$(awk -v e="$provider_errors" -v t="$provider_turns" \
                        'BEGIN { printf "%.2f", 100 * e / t }')" \
                    > "$RUNS/$id/.portal_hidden"
                printf 'hold: provider fault: %s errors in %s turns\n' \
                    "$provider_errors" "$provider_turns" > "$HOLD"
                log "lane $lane: career $idx $model phase $phase provider fault: $provider_errors errors in $provider_turns turns; hidden; hold at $HOLD"
                wait_hold
                faults=$((faults + 1))
                if [[ "$faults" -ge 2 ]]; then
                    {
                        echo "STOP: career $idx $model phase $phase provider fault twice"
                        echo "fault 1: $first_fault"
                        echo "fault 2: $provider_errors errors in $provider_turns turns"
                    } > "$STOP"
                    log "lane $lane: career $idx $model phase $phase faulted twice; STOP"
                    return 1
                fi
                first_fault="$provider_errors errors in $provider_turns turns"
                phase=$((phase - 1))
                continue
            fi
            # R67 F2: a bill above 1.25x list means the pin did not hold.
            if [[ "$stop" == "ok" && -n "$billed" && "$cost" != "0" ]] \
                && awk -v b="$billed" -v l="$cost" 'BEGIN { exit !(b > 1.25 * l) }'; then
                printf 'hold: billed $%s vs list $%s\n' "$billed" "$cost" > "$HOLD"
                log "hold: billed \$$billed vs list \$$cost"
                wait_hold
            fi
            if [[ "$stop" != "ok" ]]; then
                log "lane $lane: career $idx $model phase $phase stopped: $stop"
                touch "$STOP"
                return "$failed"
            fi
            chain="$id"
            faults=0
            first_fault=""
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
    local marked mark row model
    # R68 F3: no paid phase starts for a model without a fresh PASS
    # model-check record. A rehearsal and a dry run spend nothing.
    if [[ "$REHEARSAL" != "1" && "$DRY_RUN" != "1" \
        && "${SDB_GRID_SKIP_PREFLIGHT:-0}" != "1" \
        && "${SDB_GRID_PREFLIGHT_TEST:-0}" != "1" ]]; then
        for model in "${CAREERS[@]}"; do
            model_check_gate "$model" || return 1
        done
    fi
    # R54-1: a paid grid may not resume from a rehearsal/smoke row: its
    # phase 2+ would chain from the reference workspace and spend on a
    # career the board can never rank.
    if [[ "$REHEARSAL" != "1" ]]; then
        marked="$(ledger_marked_rows)"
        if [[ -n "$marked" ]]; then
            while IFS=$'\t' read -r mark row; do
                [[ -n "$mark" ]] || continue
                log "ledger row names a $mark campaign: $row"
            done <<<"$marked"
            log "refusing a paid grid whose ledger holds a rehearsal/smoke row; start a new ledger"
            return 1
        fi
    fi
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
    local canary="${SDB_GRID_CANARY:-1}"
    if [[ "$REHEARSAL" == "1" || "$DRY_RUN" == "1" ]]; then
        canary=0
    fi
    mkdir -p "$RUNS"
    rm -f "$RUNS/.canary_ok" "$RUNS/.canary_failed"
    if [[ "$canary" == "1" && "$LANES" -gt 1 ]]; then
        # R74 F2: lane 1 starts alone; the other lanes start only after
        # its live check passed 30 turns cleanly. A stopped canary leaves
        # .canary_failed and the other lanes never start.
        run_lane 0 &
        local canary_pid=$!
        pids+=("$canary_pid")
        while kill -0 "$canary_pid" 2>/dev/null; do
            [[ -f "$RUNS/.canary_ok" ]] && break
            sleep 0.2
        done
        local canary_done=0
        kill -0 "$canary_pid" 2>/dev/null || canary_done=1
        if [[ -f "$RUNS/.canary_ok" ]] \
            || [[ "$canary_done" -eq 1 && ! -f "$RUNS/.canary_failed" && ! -f "$STOP" ]]; then
            for ((lane = 1; lane < LANES; lane++)); do
                run_lane "$lane" &
                pids+=("$!")
            done
        fi
    else
        for ((lane = 0; lane < LANES; lane++)); do
            run_lane "$lane" &
            pids+=("$!")
        done
    fi
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
    local dir fake trace scenario runs out got paid_rows paid_rc first_chain chain p
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
prefix="${SDB_FAKE_PREFIX:-fake}"
id="$prefix-c$career-$model-p$phase"
if [[ "$phase" -gt 1 && "$chain" != "$prefix-c$career-$model-p$((phase - 1))" ]]; then
    echo "bad chain for career $career $model phase $phase: '$chain'" >&2
    exit 1
fi
printf '%s\t%s\t%s\t%s\n' "$career" "$model" "$phase" "$chain" >> "${SDB_GRID_TRACE:?}"
# R74 F6: a per-phase fault scenario. The first N attempts of the named
# phase write a provider-fault live.log; later attempts are clean.
fault_phase="${SDB_FAKE_FAULT_PHASE:-0}"
fault_attempts="${SDB_FAKE_FAULT_ATTEMPTS:-0}"
if [[ "$fault_attempts" -gt 0 && "$phase" == "$fault_phase" ]]; then
    count_file="${SDB_FAKE_FAULT_COUNT:-$SDB_RUNS_DIR/.faultcount}"
    n=0
    [[ -f "$count_file" ]] && n="$(cat "$count_file")"
    n=$((n + 1))
    echo "$n" > "$count_file"
    if [[ "$n" -le "$fault_attempts" ]]; then
        SDB_FAKE_TURNS=33
        SDB_FAKE_ERRORS=5
    else
        SDB_FAKE_TURNS=10
        SDB_FAKE_ERRORS=0
    fi
fi
if [[ -n "${SDB_FAKE_HEALTH:-}" ]]; then
    tag="${model//\//_}"; tag="${tag//:/_}"
    hdir="${SDB_RUNS_DIR:?}/campaign_sdb-sql_${tag}_$(date -u +%Y%m%d_%H%M%S)_${RANDOM}_w${worker}"
    mkdir -p "$hdir"
    printf '%s\n' "$SDB_FAKE_HEALTH" > "$hdir/health.json"
    sleep "${SDB_FAKE_LIVE_SLEEP:-1}"
fi
if [[ -n "${SDB_FAKE_TURNS:-}" ]]; then
    log="${SDB_RUNS_DIR:?}/$id/live.log"
    mkdir -p "$(dirname "$log")"
    : > "$log"
    for i in $(seq 1 "$SDB_FAKE_TURNS"); do
        printf '[BENCH-POLICY] turn=%s/100 idle=false\n' "$i" >> "$log"
    done
    for i in $(seq 1 "${SDB_FAKE_ERRORS:-0}"); do
        printf 'provider stream ended with an error\n' >> "$log"
    done
fi
if [[ "${SDB_FAKE_SLEEP_ALL:-0}" == "1" || "${SDB_FAKE_SLEEP_ID:-}" == "$id" ]]; then
    sleep "${SDB_FAKE_SLEEP:-0.3}"
fi
stop=0
[[ "${SDB_FAKE_STOP_ID:-}" == "$id" ]] && stop=1
res_phase="${SDB_FAKE_RESULT_PHASE:-$phase}"
res_chain="${SDB_FAKE_RESULT_CHAIN-$chain}"
cost="${SDB_FAKE_COST:-0.1}"
# R68 F2: a rehearsal spends $0, as a real one does.
[[ "${SDB_CELL_REHEARSAL:-0}" == "1" ]] && cost=0
python3 - "$SDB_CELL_RESULT" "$id" "$phase" "$worker" "$chain" "$stop" "$cost" "$res_phase" "$res_chain" "${SDB_FAKE_HARNESS_FAULT:-}" "${SDB_FAKE_BILLED:-0}" <<'PY'
import json, sys
out, cid, phase, worker, chain, stop, cost, res_phase, res_chain, fault, billed = sys.argv[1:12]
json.dump({
    "campaign_id": cid,
    "phase": int(res_phase),
    "chain_from": res_chain,
    "worker": int(worker),
    "chain": chain,
    "cost_usd": float(cost),
    "billed_usd": float(billed),
    "points": 10.0,
    "over_budget": False,
    "over_wall": stop == "1",
    "harness_fault": fault or None,
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
    SDB_GRID_CANARY=0
    export SDB_GRID_CANARY
    # R65 F3: every self-test case gets a rich balance unless it overrides
    # it; the balance cases below drive the hold path explicitly.
    balance="$dir/balance.sh"
    printf '#!/bin/sh\necho 100.00\n' > "$balance"
    chmod +x "$balance"
    SDB_GRID_BALANCE_CMD="$balance"
    export SDB_GRID_BALANCE_CMD

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


    # 10) R54-1: --rehearsal has its own ledger and stop file; a paid grid
    # on the same careers file starts at phase 1 and never chains from a
    # rehearsal id.
    scenario="$dir/ten"
    runs="$scenario/runs"
    mkdir -p "$runs"
    printf '%s\n' c1 > "$scenario/careers.txt"
    SDB_RUNS_DIR="$runs" SDB_GRID_TRACE="$scenario/reh.trace" SDB_FAKE_PREFIX=reh \
        SDB_REHEARSAL_DIR="$scenario/gates" \
        "$ROOT/scripts/grid.sh" --rehearsal --careers "$scenario/careers.txt" \
        --lanes 1 --cell "$fake" >"$scenario/reh.out" 2>&1
    [[ -f "$runs/.grid-rehearsal-ledger.tsv" ]] || {
        echo "the rehearsal grid did not use its own ledger"
        cat "$scenario/reh.out"
        exit 1
    }
    out="$(SDB_RUNS_DIR="$runs" "$ROOT/scripts/grid.sh" --rehearsal \
        --careers "$scenario/careers.txt" --lanes 1 --cell "$fake" --dry-run 2>&1)"
    grep -q "ledger: $runs/.grid-rehearsal-ledger.tsv" <<<"$out" || {
        echo "rehearsal ledger default wrong: $out"; exit 1; }
    grep -q "stop:   $runs/.grid-rehearsal-stop" <<<"$out" || {
        echo "rehearsal stop default wrong: $out"; exit 1; }
    paid_rc=0
    SDB_RUNS_DIR="$runs" SDB_GRID_TRACE="$scenario/paid.trace" SDB_FAKE_PREFIX=paid \
        "$ROOT/scripts/grid.sh" --careers "$scenario/careers.txt" \
        --lanes 1 --cell "$fake" >"$scenario/paid.out" 2>&1 || paid_rc=$?
    [[ "$paid_rc" -eq 0 ]] || {
        echo "paid grid after a rehearsal exited $paid_rc"
        cat "$scenario/paid.out"
        exit 1
    }
    paid_rows=$(($(wc -l < "$runs/.grid-ledger.tsv") - 1))
    [[ "$paid_rows" -eq 4 ]] || {
        echo "paid grid after a rehearsal ran $paid_rows phases, want 4"
        cat "$scenario/paid.out"
        exit 1
    }
    got="$(awk -F'\t' 'NR > 1 { printf "%s ", $3 }' "$runs/.grid-ledger.tsv")"
    [[ "$got" == "1 2 3 4 " ]] || { echo "paid grid phases: $got"; exit 1; }
    if grep -q 'reh-' "$runs/.grid-ledger.tsv"; then
        echo "the paid ledger names a rehearsal id"; exit 1
    fi
    first_chain="$(awk -F'\t' '$3 == 1 { print $4 }' "$scenario/paid.trace")"
    [[ -z "$first_chain" ]] || { echo "paid phase 1 chains from '$first_chain'"; exit 1; }
    for p in 2 3 4; do
        chain="$(awk -F'\t' -v p="$p" '$3 == p { print $4 }' "$scenario/paid.trace")"
        [[ "$chain" == "paid-c0-c1-p$((p - 1))" ]] || {
            echo "paid phase $p chains from '$chain'"; exit 1; }
    done

    # 11) R54-1: a paid grid refuses a ledger whose campaign rollup is
    # marked, naming the rows; no cell runs.
    scenario="$dir/eleven"
    runs="$scenario/runs"
    mkdir -p "$runs/fake-c0-c1-p1" "$runs/fake-c1-c1-p1"
    printf '{"rehearsal": true}\n' > "$runs/fake-c0-c1-p1/campaign.json"
    printf '{"smoke": true}\n' > "$runs/fake-c1-c1-p1/campaign.json"
    printf '%s\n' c1 c1 > "$scenario/careers.txt"
    {
        printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\n'
        printf '0\tc1\t1\t1\tfake-c0-c1-p1\t1\t0\tok\t2026-10-31T00:00:00Z\n'
        printf '1\tc1\t1\t2\tfake-c1-c1-p1\t1\t0\tok\t2026-10-31T00:00:00Z\n'
    } > "$runs/.grid-ledger.tsv"
    rc=0
    out="$(SDB_RUNS_DIR="$runs" SDB_GRID_TRACE="$scenario/trace" \
        "$ROOT/scripts/grid.sh" --careers "$scenario/careers.txt" \
        --lanes 2 --cell "$fake" 2>&1)" || rc=$?
    [[ "$rc" -eq 1 ]] || {
        echo "a paid grid with marked rows exited $rc, want 1"
        echo "$out"
        exit 1
    }
    for want in fake-c0-c1-p1 fake-c1-c1-p1 rehearsal smoke; do
        grep -q "$want" <<<"$out" || {
            echo "the refusal did not name $want"
            echo "$out"
            exit 1
        }
    done
    [[ ! -s "$scenario/trace" ]] || {
        echo "a cell ran before the marked-row refusal"; exit 1; }
    [[ "$(($(wc -l < "$runs/.grid-ledger.tsv") - 1))" -eq 2 ]] || {
        echo "the refusal changed the ledger"; exit 1; }

    # 12) R65 F2: a provider billing harness fault stops the grid: the
    # ledger says "harness fault", STOP is written, and no later phase runs
    # on either lane.
    scenario="$dir/twelve"
    mkdir -p "$scenario"
    printf '%s\n' c1 c2 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=2
    CAP=0
    PHASE_BUDGET=0.99
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    SDB_FAKE_HARNESS_FAULT="provider billing exhausted (HTTP 402)"
    SDB_FAKE_SLEEP_ID="fake-c1-c2-p1"
    SDB_FAKE_SLEEP=0.3
    export SDB_GRID_TRACE SDB_FAKE_HARNESS_FAULT SDB_FAKE_SLEEP_ID SDB_FAKE_SLEEP
    load_careers "$CAREERS_FILE"
    run_grid || true
    got="$(awk -F'\t' 'NR > 1 { s = $8 } END { print s }' "$LEDGER")"
    [[ "$got" == "harness fault" ]] || { echo "billing fault stop: '$got'"; exit 1; }
    [[ -f "$STOP" ]] || { echo "a billing fault did not STOP"; exit 1; }
    bad="$(awk -F'\t' 'NR > 1 && $3 >= 2 { n++ } END { print n + 0 }' "$LEDGER")"
    [[ "$bad" -eq 0 ]] || { echo "$bad phase(s) started after the billing fault"; exit 1; }
    unset SDB_FAKE_HARNESS_FAULT SDB_FAKE_SLEEP_ID SDB_FAKE_SLEEP
    export -n SDB_FAKE_HARNESS_FAULT SDB_FAKE_SLEEP_ID SDB_FAKE_SLEEP 2>/dev/null || true

    # 13) R65 F3: the balance preflight. 10.00 with 3 lanes at $0.99 runs;
    # 2.00 holds before any phase (3 lanes need $3.97); a failing stub
    # holds with "balance unknown"; a rehearsal with a failing stub runs.
    scenario="$dir/thirteen"
    mkdir -p "$scenario"
    printf '%s\n' c1 c2 c3 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=3
    CAP=0
    PHASE_BUDGET=0.99
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    printf '#!/bin/sh\necho 10.00\n' > "$balance"
    chmod +x "$balance"
    load_careers "$CAREERS_FILE"
    run_grid
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 12 ]] || {
        echo "balance 10: expected 12 rows, got $(($(wc -l < "$LEDGER") - 1))"; exit 1; }
    [[ ! -f "$HOLD" ]] || { echo "balance 10 wrote a hold"; exit 1; }

    scenario="$dir/thirteen-hold"
    mkdir -p "$scenario"
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    printf '#!/bin/sh\necho 2.00\n' > "$balance"
    chmod +x "$balance"
    ( run_grid >"$scenario/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 50); do [[ -f "$HOLD" ]] && break; sleep 0.1; done
    [[ -f "$HOLD" ]] || { kill "$pid" 2>/dev/null || true; echo "balance 2: no hold"; exit 1; }
    grep -q 'hold: balance \$2.00 below \$3.97' "$scenario/out" || {
        echo "balance 2: wrong hold line"; cat "$scenario/out"; kill "$pid" 2>/dev/null || true; exit 1; }
    [[ "$(($(wc -l < "$LEDGER" 2>/dev/null || echo 1) - 1))" -eq 0 ]] || {
        echo "balance 2: a phase started before the hold"; kill "$pid" 2>/dev/null || true; exit 1; }
    printf '#!/bin/sh\necho 100.00\n' > "$balance"
    rm -f "$HOLD"
    wait "$pid" || { echo "balance 2: the grid did not resume"; exit 1; }
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 12 ]] || {
        echo "balance 2: expected 12 rows after the top-up, got $(($(wc -l < "$LEDGER") - 1))"; exit 1; }

    scenario="$dir/thirteen-unknown"
    mkdir -p "$scenario"
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    printf '#!/bin/sh\nexit 1\n' > "$balance"
    chmod +x "$balance"
    export OPENROUTER_API_KEY="sk-r65-secret"
    ( run_grid >"$scenario/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 50); do [[ -f "$HOLD" ]] && break; sleep 0.1; done
    [[ -f "$HOLD" ]] || { kill "$pid" 2>/dev/null || true; echo "balance unknown: no hold"; exit 1; }
    grep -q 'hold: balance unknown' "$scenario/out" || {
        echo "balance unknown: wrong hold line"; cat "$scenario/out"; kill "$pid" 2>/dev/null || true; exit 1; }
    if grep -q 'sk-r65-secret' "$scenario/out"; then
        echo "the API key was printed"; kill "$pid" 2>/dev/null || true; exit 1
    fi
    printf '#!/bin/sh\necho 100.00\n' > "$balance"
    rm -f "$HOLD"
    wait "$pid" || { echo "balance unknown: the grid did not resume"; exit 1; }
    unset OPENROUTER_API_KEY

    scenario="$dir/thirteen-reh"
    mkdir -p "$scenario"
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    printf '#!/bin/sh\nexit 1\n' > "$balance"
    chmod +x "$balance"
    REHEARSAL=1
    PHASE_BUDGET=0
    load_careers "$CAREERS_FILE"
    run_grid
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 12 ]] || {
        echo "rehearsal with a failing balance did not run"; exit 1; }
    [[ ! -f "$HOLD" ]] || { echo "a rehearsal wrote a hold"; exit 1; }
    REHEARSAL=0
    PHASE_BUDGET=0.99

    # The balance cases above leave the failing stub; restore a rich one.
    printf '#!/bin/sh\necho 100.00\n' > "$balance"
    chmod +x "$balance"

    # 14) R67 F2: a bill above 1.25x list holds the grid; 1.1x does not.
    scenario="$dir/fourteen"
    mkdir -p "$scenario"
    printf '%s\n' c1 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=1
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    SDB_FAKE_COST=0.1
    SDB_FAKE_BILLED=0.25
    export SDB_FAKE_COST SDB_FAKE_BILLED
    load_careers "$CAREERS_FILE"
    ( run_grid >"$scenario/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 50); do [[ -f "$HOLD" ]] && break; sleep 0.1; done
    [[ -f "$HOLD" ]] || { kill "$pid" 2>/dev/null || true; echo "billed 2x: no hold"; exit 1; }
    grep -q 'hold: billed \$0.25 vs list \$0.1' "$scenario/out" || {
        echo "billed 2x: wrong hold line"; cat "$scenario/out"; kill "$pid" 2>/dev/null || true; exit 1; }
    rm -f "$HOLD"
    wait "$pid" || { echo "billed 2x: the grid did not resume"; exit 1; }
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 1 ]] || { echo "billed 2x: expected 1 row"; exit 1; }

    scenario="$dir/fifteen"
    mkdir -p "$scenario"
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    SDB_FAKE_BILLED=0.11
    export SDB_FAKE_BILLED
    load_careers "$CAREERS_FILE"
    run_grid
    [[ ! -f "$HOLD" ]] || { echo "billed 1.1x wrote a hold"; exit 1; }
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 1 ]] || { echo "billed 1.1x: expected 1 row"; exit 1; }
    unset SDB_FAKE_COST SDB_FAKE_BILLED
    export -n SDB_FAKE_COST SDB_FAKE_BILLED 2>/dev/null || true
    MAX_PHASE=4

    # 16) R67 F4 (R66-3): three lanes probe the long-test lock at the same
    # instant with no holder; the retry keeps a collision from looking like
    # a holder, so all three start and no STOP is written. Case 8 keeps the
    # real-holder refusal.
    scenario="$dir/sixteen"
    mkdir -p "$scenario"
    printf '%s\n' c1 c2 c3 > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=3
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=1
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    load_careers "$CAREERS_FILE"
    ( export SDB_GRID_SKIP_PREFLIGHT=0 SDB_GRID_PREFLIGHT_TEST=1 \
        SDB_LONG_TEST_LOCK="$scenario/long-test.lock"; run_grid ) \
        >"$scenario/out" 2>&1 || {
        echo "probe race: the grid failed"; cat "$scenario/out"; exit 1; }
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 3 ]] || {
        echo "probe race: expected 3 rows, got $(($(wc -l < "$LEDGER") - 1))"
        cat "$scenario/out"; exit 1; }
    [[ ! -f "$STOP" ]] || { echo "probe race: a false STOP"; exit 1; }
    MAX_PHASE=4

    # 17) R68 F3: a paid grid refuses a model with no fresh PASS
    # model-check record on the current sandbox image.
    scenario="$dir/eleven"
    mkdir -p "$scenario/gates"
    printf '%s\n' test-model > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=1
    CAP=0
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    load_careers "$CAREERS_FILE"
    export SDB_MODEL_CHECK_DIR="$scenario/gates"
    export SDB_MODEL_CHECK_IMAGE_DIGEST="sha256:fresh"
    record="$scenario/gates/model-check-test-model.txt"
    fresh="$(date -u +%FT%TZ)"
    old="$(date -u -d '8 days ago' +%FT%TZ)"

    rm -f "$record"
    rc=0
    ( export SDB_GRID_SKIP_PREFLIGHT=0 SDB_GRID_PREFLIGHT_TEST=0; run_grid ) \
        >/dev/null 2>&1 || rc=$?
    [[ "$rc" -eq 1 ]] || { echo "a grid without a model-check record was accepted"; exit 1; }
    [[ ! -f "$LEDGER" ]] || { echo "a grid without a model-check record wrote a ledger"; exit 1; }
    echo "[self-test] no model-check record refuses the grid"

    printf 'time: %s\nimage: sha256:fresh\nRESULT: PASS\n' "$old" > "$record"
    if model_check_gate test-model 2>/dev/null; then
        echo "an old model-check record was accepted"; exit 1
    fi
    echo "[self-test] an old model-check record refuses"

    printf 'time: %s\nimage: sha256:other\nRESULT: PASS\n' "$fresh" > "$record"
    if model_check_gate test-model 2>/dev/null; then
        echo "another image's model-check record was accepted"; exit 1
    fi
    echo "[self-test] another image's model-check record refuses"

    printf 'time: %s\nimage: sha256:fresh\nRESULT: FAIL (x)\n' "$fresh" > "$record"
    if model_check_gate test-model 2>/dev/null; then
        echo "a failed model-check record was accepted"; exit 1
    fi
    echo "[self-test] a failed model-check record refuses"

    printf 'time: %s\nimage: sha256:fresh\nRESULT: PASS\n' "$fresh" > "$record"
    model_check_gate test-model 2>/dev/null || {
        echo "a fresh matching model-check record was refused"; exit 1; }
    echo "[self-test] a fresh matching model-check record allows"
    unset SDB_MODEL_CHECK_DIR SDB_MODEL_CHECK_IMAGE_DIGEST

    # 18) R68 F2: a full 4-phase $0 rehearsal writes the full-key record;
    # a stopped phase or a partial run writes none.
    scenario="$dir/twelve"
    mkdir -p "$scenario"
    export SDB_REHEARSAL_DIR="$scenario/gates"
    export SDB_MODEL_CHECK_IMAGE_DIGEST="sha256:rehearsal"
    ledger="$scenario/ledger.tsv"
    printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\n' > "$ledger"
    for c in 0 1; do
        for p in 1 2 3 4; do
            printf '%s\tm\t%s\t1\tid\t1\t0\tok\tx\n' "$c" "$p" >> "$ledger"
        done
    done
    LEDGER="$ledger"
    CAREERS=(c1 c2)
    MAX_PHASE=4
    LANES=1
    write_rehearsal_record || {
        echo "a full 4-phase rehearsal wrote no record"; exit 1; }
    full="$(scripts/flake-bar.sh --key)"
    record="$scenario/gates/rehearsal-$full.txt"
    [[ -f "$record" ]] || { echo "the rehearsal record is missing"; exit 1; }
    grep -q "^commit:  $(git rev-parse HEAD)$" "$record" || {
        echo "the rehearsal record does not name the commit"; exit 1; }
    grep -q '^RESULT: PASS$' "$record" || {
        echo "the rehearsal record is not a PASS"; exit 1; }
    rehearsal_gate || { echo "the rehearsal gate refused a fresh record"; exit 1; }
    cp "$record" "$record.good"
    # A docs-only commit keeps the full key: the record still holds.
    sed -i 's/^commit:  .*/commit:  deadbeef/' "$record"
    rehearsal_gate 2>/dev/null || {
        echo "a rehearsal record for the same key but another commit was refused"; exit 1; }
    # A record whose key line names other content is refused.
    sed -i 's/^key: .*/key:     rehearsal-0000deadbeef/' "$record"
    if rehearsal_gate 2>/dev/null; then
        echo "a rehearsal record naming another key was accepted"; exit 1
    fi
    mv "$record.good" "$record"
    rm -f "$record"
    awk -F'\t' 'BEGIN { OFS = "\t" } NR == 2 { $8 = "stopped" } { print }' \
        "$ledger" > "$ledger.tmp"
    mv "$ledger.tmp" "$ledger"
    if write_rehearsal_record 2>/dev/null; then
        echo "a stopped rehearsal phase wrote a record"; exit 1
    fi
    [[ ! -f "$record" ]] || { echo "a stopped rehearsal wrote a record"; exit 1; }
    awk -F'\t' 'BEGIN { OFS = "\t" } NR == 2 { $8 = "ok" } { print }' \
        "$ledger" > "$ledger.tmp"
    mv "$ledger.tmp" "$ledger"
    MAX_PHASE=1
    write_rehearsal_record 2>/dev/null || true
    [[ ! -f "$record" ]] || { echo "a partial rehearsal wrote a record"; exit 1; }
    MAX_PHASE=4
    unset SDB_REHEARSAL_DIR SDB_MODEL_CHECK_IMAGE_DIGEST
    echo "[self-test] the rehearsal record is full, commit-keyed and \$0"

    # 19) R68 F5: 5 provider errors in 33 turns (15.15%) is a provider
    # fault: the ledger row says provider_fault, the run is hidden with
    # the counts, and the grid holds until the hold is released.
    scenario="$dir/nineteen"
    runs="$scenario/runs"
    mkdir -p "$runs"
    printf 'c1\n' > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=1
    CAP=0
    PHASE_BUDGET=0
    MAX_PHASE=1
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    RUNS="$runs"
    export SDB_RUNS_DIR="$runs"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    export SDB_FAKE_PREFIX=f5
    export SDB_FAKE_TURNS=33
    export SDB_FAKE_ERRORS=5
    export SDB_FAKE_FAULT_PHASE=1
    export SDB_FAKE_FAULT_ATTEMPTS=1
    export SDB_FAKE_FAULT_COUNT="$scenario/faultcount"
    rm -f "$SDB_FAKE_FAULT_COUNT"
    load_careers "$CAREERS_FILE"
    ( run_grid ) &
    pid=$!
    for _ in $(seq 1 100); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    if [[ ! -f "$HOLD" ]]; then
        echo "a 15% provider fault did not hold the grid"
        kill "$pid" 2>/dev/null || true
        exit 1
    fi
    stop="$(awk -F'\t' 'NR > 1 { print $8 }' "$LEDGER")"
    [[ "$stop" == "provider_fault" ]] || {
        echo "a 15% provider fault wrote stop '$stop'"
        kill "$pid" 2>/dev/null || true
        exit 1
    }
    hidden="$runs/f5-c0-c1-p1/.portal_hidden"
    [[ -f "$hidden" ]] || {
        echo "a provider fault did not hide the run"
        kill "$pid" 2>/dev/null || true
        exit 1
    }
    grep -q '5 errors in 33 turns' "$hidden" || {
        echo "the hidden reason does not name the counts: $(cat "$hidden")"
        kill "$pid" 2>/dev/null || true
        exit 1
    }
    rm -f "$HOLD"
    wait "$pid" || { echo "the held grid failed"; exit 1; }
    [[ ! -f "$STOP" ]] || { echo "a provider fault touched STOP"; exit 1; }
    got="$(awk -F'\t' 'NR > 1 { printf "%s:%s ", $3, $8 }' "$LEDGER")"
    [[ "$got" == "1:provider_fault 1:ok " ]] || {
        echo "the faulted phase did not rerun cleanly: $got"; exit 1; }
    unset SDB_FAKE_FAULT_PHASE SDB_FAKE_FAULT_ATTEMPTS SDB_FAKE_FAULT_COUNT
    echo "[self-test] a 15% provider fault holds and hides the run"

    # 20) 1 error in 1000 turns (0.1%) is not a provider fault.
    scenario="$dir/twenty"
    runs="$scenario/runs"
    mkdir -p "$runs"
    printf 'c1\n' > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    RUNS="$runs"
    export SDB_RUNS_DIR="$runs"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    export SDB_FAKE_PREFIX=f5b
    export SDB_FAKE_TURNS=1000
    export SDB_FAKE_ERRORS=1
    load_careers "$CAREERS_FILE"
    run_grid || { echo "a 0.1% phase failed the grid"; exit 1; }
    stop="$(awk -F'\t' 'NR > 1 { print $8 }' "$LEDGER")"
    [[ "$stop" == "ok" ]] || { echo "a 0.1% phase wrote stop '$stop'"; exit 1; }
    [[ ! -f "$runs/f5b-c0-c1-p1/.portal_hidden" ]] || {
        echo "a 0.1% phase was hidden"; exit 1; }
    echo "[self-test] a 0.1% phase passes"

    # 21) 3 errors in 20 turns is 15%, but under the 5-error floor.
    scenario="$dir/twentyone"
    runs="$scenario/runs"
    mkdir -p "$runs"
    printf 'c1\n' > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    RUNS="$runs"
    export SDB_RUNS_DIR="$runs"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    export SDB_FAKE_PREFIX=f5c
    export SDB_FAKE_TURNS=20
    export SDB_FAKE_ERRORS=3
    load_careers "$CAREERS_FILE"
    run_grid || { echo "a 3-error phase failed the grid"; exit 1; }
    stop="$(awk -F'\t' 'NR > 1 { print $8 }' "$LEDGER")"
    [[ "$stop" == "ok" ]] || { echo "a 3-error phase wrote stop '$stop'"; exit 1; }
    [[ ! -f "$runs/f5c-c0-c1-p1/.portal_hidden" ]] || {
        echo "a 3-error phase was hidden"; exit 1; }
    echo "[self-test] 3 errors in 20 turns pass (under the 5-error floor)"
    unset SDB_FAKE_TURNS SDB_FAKE_ERRORS SDB_FAKE_PREFIX SDB_RUNS_DIR

    # 22) R74 F6: a faulted phase 2 is hidden, the hold is released, and
    # the same phase reruns from phase 1's end; the ledger keeps both rows.
    scenario="$dir/twentytwo"
    runs="$scenario/runs"
    mkdir -p "$runs"
    printf 'c1\n' > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=1
    CAP=0
    PHASE_BUDGET=0
    MAX_PHASE=3
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    RUNS="$runs"
    export SDB_RUNS_DIR="$runs"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    export SDB_FAKE_PREFIX=f6
    export SDB_FAKE_FAULT_PHASE=2
    export SDB_FAKE_FAULT_ATTEMPTS=1
    export SDB_FAKE_FAULT_COUNT="$scenario/faultcount"
    rm -f "$SDB_FAKE_FAULT_COUNT"
    load_careers "$CAREERS_FILE"
    ( run_grid ) &
    pid=$!
    for _ in $(seq 1 100); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    if [[ ! -f "$HOLD" ]]; then
        echo "a faulted phase did not hold the grid"
        kill "$pid" 2>/dev/null || true
        exit 1
    fi
    rm -f "$HOLD"
    wait "$pid" || { echo "the rerun grid failed"; exit 1; }
    [[ ! -f "$STOP" ]] || { echo "a single fault touched STOP"; exit 1; }
    got="$(awk -F'\t' 'NR > 1 { printf "%s:%s ", $3, $8 }' "$LEDGER")"
    [[ "$got" == "1:ok 2:provider_fault 2:ok 3:ok " ]] || {
        echo "F6 ledger: $got"; exit 1; }
    chains="$(awk -F'\t' '$3 == 2 { printf "%s ", $4 }' "$SDB_GRID_TRACE")"
    [[ "$chains" == "f6-c0-c1-p1 f6-c0-c1-p1 " ]] || {
        echo "F6 rerun chain: $chains"; exit 1; }
    [[ -f "$runs/f6-c0-c1-p2/.portal_hidden" ]] || {
        echo "the faulted run is not hidden"; exit 1; }
    total="$(ledger_total)"
    awk -v t="$total" 'BEGIN { exit !(t > 0.39 && t < 0.41) }' || {
        echo "F6 spend: $total"; exit 1; }
    echo "[self-test] a faulted phase reruns from the same chain and the spend adds up"

    # 23) A phase that faults twice stops the career and names both faults.
    scenario="$dir/twentythree"
    runs="$scenario/runs"
    mkdir -p "$runs"
    printf 'c1\n' > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    RUNS="$runs"
    export SDB_RUNS_DIR="$runs"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    export SDB_FAKE_PREFIX=f6b
    export SDB_FAKE_FAULT_PHASE=2
    export SDB_FAKE_FAULT_ATTEMPTS=2
    export SDB_FAKE_FAULT_COUNT="$scenario/faultcount"
    rm -f "$SDB_FAKE_FAULT_COUNT"
    load_careers "$CAREERS_FILE"
    ( run_grid ) &
    pid=$!
    for round in 1 2; do
        for _ in $(seq 1 100); do
            [[ -f "$HOLD" ]] && break
            sleep 0.1
        done
        if [[ ! -f "$HOLD" ]]; then
            echo "fault hold $round never appeared"
            kill "$pid" 2>/dev/null || true
            exit 1
        fi
        rm -f "$HOLD"
    done
    rc=0
    wait "$pid" || rc=$?
    [[ "$rc" -eq 1 ]] || { echo "a double fault did not fail the grid (rc=$rc)"; exit 1; }
    got="$(awk -F'\t' 'NR > 1 { printf "%s:%s ", $3, $8 }' "$LEDGER")"
    [[ "$got" == "1:ok 2:provider_fault 2:provider_fault " ]] || {
        echo "double-fault ledger: $got"; exit 1; }
    grep -q 'provider fault twice' "$STOP" || {
        echo "the STOP line does not name the double fault"; exit 1; }
    grep -q 'fault 1:' "$STOP" && grep -q 'fault 2:' "$STOP" || {
        echo "the STOP line misses a fault"; exit 1; }
    echo "[self-test] a second fault stops the career and names both faults"
    unset SDB_FAKE_FAULT_PHASE SDB_FAKE_FAULT_ATTEMPTS SDB_FAKE_FAULT_COUNT
    unset SDB_FAKE_TURNS SDB_FAKE_ERRORS SDB_FAKE_PREFIX SDB_RUNS_DIR

    # 24) R74 F2: a live minimax-shaped health (20 turns, 5 errors) stops
    # the phase within its first 30 turns; the run is hidden and the grid
    # holds. The F6 rerun faults again and the career STOPs.
    scenario="$dir/twentyfour"
    runs="$scenario/runs"
    mkdir -p "$runs"
    printf 'c1\n' > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=1
    CAP=0
    PHASE_BUDGET=0
    MAX_PHASE=1
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    RUNS="$runs"
    export SDB_RUNS_DIR="$runs"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    export SDB_FAKE_PREFIX=f2a
    export SDB_FAKE_HEALTH='{"turns":20,"tool_errors":5,"ref_queries":0,"ref_refused":0,"turn_secs_p50":5.0,"turn_secs_max":20.0,"empty_responses":0,"billed_ratio":1.0}'
    export SDB_FAKE_LIVE_SLEEP=1
    export SDB_GRID_HEALTH_POLL=0.1
    load_careers "$CAREERS_FILE"
    ( run_grid ) &
    pid=$!
    for _ in $(seq 1 100); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    if [[ ! -f "$HOLD" ]]; then
        echo "the live minimax health did not hold the grid"
        kill "$pid" 2>/dev/null || true
        exit 1
    fi
    hdir="$(find "$runs" -maxdepth 1 -type d -name 'campaign_sdb-sql_c1_*_w1' | head -1)"
    [[ -f "$hdir/.live_stop" ]] || {
        echo "the live check wrote no .live_stop"; kill "$pid" 2>/dev/null || true; exit 1; }
    grep -q '5 errors in 20 turns' "$hdir/.live_stop" || {
        echo "the live stop counts are wrong: $(cat "$hdir/.live_stop")"
        kill "$pid" 2>/dev/null || true; exit 1; }
    [[ -f "$hdir/.portal_hidden" ]] || {
        echo "the live-stopped run is not hidden"; kill "$pid" 2>/dev/null || true; exit 1; }
    rm -f "$HOLD"
    for _ in $(seq 1 100); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    if [[ ! -f "$HOLD" ]]; then
        echo "the live fault did not rerun into a second hold"
        kill "$pid" 2>/dev/null || true
        exit 1
    fi
    rm -f "$HOLD"
    rc=0
    wait "$pid" || rc=$?
    [[ "$rc" -eq 1 ]] || { echo "the double live fault did not STOP (rc=$rc)"; exit 1; }
    got="$(awk -F'\t' 'NR > 1 { printf "%s:%s ", $3, $8 }' "$LEDGER")"
    [[ "$got" == "1:provider_fault 1:provider_fault " ]] || {
        echo "live-fault ledger: $got"; exit 1; }
    echo "[self-test] the live check stops a minimax-shaped phase within 30 turns"
    unset SDB_FAKE_HEALTH SDB_FAKE_LIVE_SLEEP SDB_GRID_HEALTH_POLL

    # 25) A clean live health is not stopped.
    scenario="$dir/twentyfive"
    runs="$scenario/runs"
    mkdir -p "$runs"
    printf 'c1\n' > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    RUNS="$runs"
    export SDB_RUNS_DIR="$runs"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    export SDB_FAKE_PREFIX=f2b
    export SDB_FAKE_HEALTH='{"turns":100,"tool_errors":0,"ref_queries":10,"ref_refused":0,"turn_secs_p50":10.0,"turn_secs_max":40.0,"empty_responses":0,"rate_limited":11,"billed_ratio":1.0}'
    export SDB_FAKE_LIVE_SLEEP=1
    export SDB_GRID_HEALTH_POLL=0.1
    load_careers "$CAREERS_FILE"
    out="$(run_grid 2>&1)" || { echo "a clean live health failed the grid: $out"; exit 1; }
    grep -q '11 rate limits' <<<"$out" || {
        echo "the live check did not warn about 11 rate limits: $out"; exit 1; }
    stop="$(awk -F'\t' 'NR > 1 { print $8 }' "$LEDGER")"
    [[ "$stop" == "ok" ]] || { echo "a clean live health wrote '$stop'"; exit 1; }
    if find "$runs" -maxdepth 2 -name .live_stop -print -quit | grep -q .; then
        echo "a clean live health was stopped"; exit 1
    fi
    echo "[self-test] a clean live health is not stopped"
    unset SDB_FAKE_HEALTH SDB_FAKE_LIVE_SLEEP SDB_GRID_HEALTH_POLL

    # 26) A clean canary opens lanes 2 and 3 after 30 turns.
    scenario="$dir/twentysix"
    runs="$scenario/runs"
    mkdir -p "$runs"
    printf 'c1\nc2\nc3\n' > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=3
    MAX_PHASE=1
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    RUNS="$runs"
    export SDB_RUNS_DIR="$runs"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    export SDB_FAKE_PREFIX=f2c
    export SDB_FAKE_HEALTH='{"turns":30,"tool_errors":0,"ref_queries":10,"ref_refused":0,"turn_secs_p50":10.0,"turn_secs_max":40.0,"empty_responses":0,"billed_ratio":1.0}'
    export SDB_FAKE_LIVE_SLEEP=2
    export SDB_GRID_HEALTH_POLL=0.1
    export SDB_GRID_CANARY=1
    load_careers "$CAREERS_FILE"
    run_grid || { echo "the clean-canary grid failed"; exit 1; }
    workers="$(awk -F'\t' 'NR > 1 { print $4 }' "$LEDGER" | sort -u | paste -sd, -)"
    [[ "$workers" == "1,2,3" ]] || { echo "clean canary workers: $workers"; exit 1; }
    [[ -f "$runs/.canary_ok" ]] || { echo "the canary never passed 30 turns"; exit 1; }
    echo "[self-test] a clean canary opens the other lanes after 30 turns"
    unset SDB_FAKE_HEALTH SDB_FAKE_LIVE_SLEEP SDB_GRID_HEALTH_POLL SDB_GRID_CANARY

    # 27) A stopped canary never opens the other lanes.
    scenario="$dir/twentyseven"
    runs="$scenario/runs"
    mkdir -p "$runs"
    printf 'c1\nc2\nc3\n' > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    RUNS="$runs"
    export SDB_RUNS_DIR="$runs"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    export SDB_GRID_TRACE
    export SDB_FAKE_PREFIX=f2d
    export SDB_FAKE_HEALTH='{"turns":20,"tool_errors":5,"ref_queries":0,"ref_refused":0,"turn_secs_p50":5.0,"turn_secs_max":20.0,"empty_responses":0,"billed_ratio":1.0}'
    export SDB_FAKE_LIVE_SLEEP=1
    export SDB_GRID_HEALTH_POLL=0.1
    export SDB_GRID_CANARY=1
    load_careers "$CAREERS_FILE"
    ( run_grid ) &
    pid=$!
    for _ in $(seq 1 100); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    if [[ ! -f "$HOLD" ]]; then
        echo "the stopped canary did not hold"; kill "$pid" 2>/dev/null || true; exit 1
    fi
    [[ -f "$runs/.canary_failed" ]] || {
        echo "the canary did not record its failure"; kill "$pid" 2>/dev/null || true; exit 1; }
    rm -f "$HOLD"
    for _ in $(seq 1 100); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    if [[ ! -f "$HOLD" ]]; then
        echo "the stopped canary did not rerun into a second hold"
        kill "$pid" 2>/dev/null || true; exit 1
    fi
    rm -f "$HOLD"
    rc=0
    wait "$pid" || rc=$?
    [[ "$rc" -eq 1 ]] || { echo "the stopped canary did not STOP (rc=$rc)"; exit 1; }
    workers="$(awk -F'\t' 'NR > 1 { print $4 }' "$LEDGER" | sort -u | paste -sd, -)"
    [[ "$workers" == "1" ]] || { echo "lanes started behind a stopped canary: $workers"; exit 1; }
    echo "[self-test] a stopped canary never opens the other lanes"
    unset SDB_FAKE_HEALTH SDB_FAKE_LIVE_SLEEP SDB_GRID_HEALTH_POLL SDB_GRID_CANARY
    unset SDB_FAKE_PREFIX SDB_RUNS_DIR

    echo "RESULT: PASS (grid self-test)"
}

# R54 F2: an opt-in real grid. Two lanes on workers 8 and 9, each one
# career of two chained rehearsal phases, in a temp runs dir. This is the
# check the stub self-test cannot make: two real launches hold two worker
# locks at once. On round-53-open's single runs/.campaign.lock the second
# lane dies at "campaign ... is already running".
real_self_test() {
    # R56 F6: a real grid runs Docker next to a live cell.
    "$ROOT/scripts/beside-cell.sh" "grid --self-test --real" || return 1
    if [[ "${SDB_BESIDE_CELL_ONLY:-0}" == "1" ]]; then
        return 0
    fi
    if ! command -v docker >/dev/null 2>&1; then
        log "real self-test: Docker is not available"
        return 1
    fi
    local dir scenario rows got c w ids total id rc=0
    dir="$(mktemp -d /tmp/sdb-grid-real.XXXXXX)"
    trap 'rm -rf "${dir:-}"' EXIT
    scenario="$dir"
    printf 'test-model\ntest-model\n' > "$scenario/careers.txt"
    CAREERS_FILE="$scenario/careers.txt"
    LANES=2
    CAP=0
    PHASE_BUDGET=0
    MAX_PHASE=2
    WORKER_BASE=8
    REHEARSAL=1
    LEDGER="$scenario/ledger.tsv"
    HOLD="$scenario/hold"
    STOP="$scenario/stop"
    export SDB_RUNS_DIR="$scenario/runs"
    mkdir -p "$SDB_RUNS_DIR"
    export SDB_CELL_REHEARSAL=1
    export SDB_CAMPAIGN_TEST_WORKER_MAX=99
    # The cell still checks the tree; the self-test may run on a commit
    # the developer just built, so skip only the grid preflight.
    export SDB_GRID_SKIP_PREFLIGHT=1
    CELL="$ROOT/scripts/sql-cell.sh"
    load_careers "$CAREERS_FILE"
    log "real self-test: 2 lanes x 2 phases on workers 8/9, runs $SDB_RUNS_DIR"
    run_grid || rc=$?
    if [[ "$rc" -ne 0 ]]; then
        log "real self-test: the grid failed (rc=$rc)"
        [[ -f "$LEDGER" ]] && cat "$LEDGER" >&2
        return 1
    fi
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 4 ]] || {
        log "real self-test: expected 4 ledger rows, got $rows"
        cat "$LEDGER" >&2
        return 1
    }
    for c in 0 1; do
        got="$(awk -F'\t' -v c="$c" 'NR > 1 && $1 == c { printf "%s ", $3 }' "$LEDGER")"
        [[ "$got" == "1 2 " ]] || {
            log "real self-test: career $c phases '$got'"
            return 1
        }
    done
    for w in 8 9; do
        ids="$(awk -F'\t' -v w="$w" 'NR > 1 && $4 == w { n++ } END { print n + 0 }' "$LEDGER")"
        [[ "$ids" -gt 0 ]] || {
            log "real self-test: worker $w never ran"
            return 1
        }
    done
    got="$(awk -F'\t' 'NR > 1 && $8 != "ok" { printf "%s ", $8 }' "$LEDGER")"
    [[ -z "$got" ]] || {
        log "real self-test: a phase stopped: $got"
        return 1
    }
    ids="$(awk -F'\t' 'NR > 1 { print $5 }' "$LEDGER" | sort -u | wc -l)"
    [[ "$ids" -eq 4 ]] || {
        log "real self-test: expected 4 distinct campaign ids, got $ids"
        return 1
    }
    total="$(ledger_total)"
    awk -v t="$total" 'BEGIN { exit !(t == 0) }' || {
        log "real self-test: a rehearsal spent \$$total"
        return 1
    }
    for id in $(awk -F'\t' 'NR > 1 { print $5 }' "$LEDGER"); do
        python3 - "$SDB_RUNS_DIR/$id/campaign.json" "$id" <<'PYEOF' || return 1
import json, sys
path, cid = sys.argv[1:3]
r = json.load(open(path))
if not r.get("rehearsal") or r.get("total_cost_usd", 0):
    sys.exit(f"real self-test: {cid} is not a $0 rehearsal: {r}")
PYEOF
    done
    echo "RESULT: PASS (grid real self-test: 2 lanes x 2 phases, workers 8/9)"
    return 0
}

if [[ "$SELF_TEST" -eq 1 ]]; then
    if [[ "$REAL" -eq 1 ]]; then
        real_self_test
        exit $?
    fi
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
mkdir -p "$RUNS"
: > "$RUNS/.grid-audit-marker"
run_grid
if [[ "$REHEARSAL" == "1" ]]; then
    write_rehearsal_record
fi
# R74 F3: audit the runs this grid produced and put the summary in the
# grid log. The audit's published set is the new, non-hidden campaigns;
# the worse-rerun comparison still sees every earlier attempt.
scripts/run-audit.sh --summary --runs-dir "$RUNS" --newer "$RUNS/.grid-audit-marker" 2>&1 \
    | while IFS= read -r line; do log "$line"; done || true
