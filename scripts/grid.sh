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
. "$ROOT/scripts/peak-window.sh"
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
# R96 Z1: parallel arrays, one entry per careers-file line. `CAREER_ZOO`
# names a zoo fixture directory (empty for the reference career) and
# `CAREER_START` the phase that career starts at.
declare -a CAREER_ZOO=()
declare -a CAREER_START=()
declare -a ZOO_SEEDS=()
SELF_TEST=0
SELF_TEST_CASE=""
REAL=0
REHEARSAL=0
DRY_RUN=0
WORKER_BASE=1
MAX_PHASE=4
HARNESS="${SDB_GRID_HARNESS:-opencode}"
BUDGET_SET=0
LEDGER_SET=0
STOP_SET=0
CAP_SET=0

log() { echo "[grid] $*" >&2; }

# R93 G5: each grid's markers carry its ledger's name, so two grids (or a
# rehearsal and a paid grid) never clobber each other's canary, audit or
# stop state. The ledger lock is separate from the short-lived
# $LEDGER.lock the ledger writes use, so a grid can hold it for its life.
ledger_slug() {
    local base
    base="$(basename "${LEDGER:-grid-ledger}")"
    base="${base%.tsv}"
    base="${base#.}"
    printf '%s' "$base" | sed 's/[^A-Za-z0-9._-]/_/g'
}
canary_ok_path() { printf '%s/.canary_ok.%s' "$RUNS" "$(ledger_slug)"; }
canary_failed_path() { printf '%s/.canary_failed.%s' "$RUNS" "$(ledger_slug)"; }
audit_marker_path() { printf '%s/.grid-audit-marker.%s' "$RUNS" "$(ledger_slug)"; }

usage() {
    cat >&2 <<'EOF'
usage: grid.sh --careers <file> --lanes <n> [--harness omp|opencode]
               [--cap <usd>] [--budget <usd>] [--wall <min>]
               [--ledger <file>] [--hold <file>] [--stop <file>]
               [--cell <path>] [--dry-run] [--max-phase <n>]
       grid.sh --rehearsal --careers <file> --lanes <n> [--cap <usd>]
               [--harness omp|opencode]
       grid.sh --self-test [--real]
EOF
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --careers) CAREERS_FILE="$2"; shift 2 ;;
        --lanes) LANES="$2"; shift 2 ;;
        --cap) CAP="$2"; CAP_SET=1; shift 2 ;;
        --budget) PHASE_BUDGET="$2"; BUDGET_SET=1; shift 2 ;;
        --wall) PHASE_WALL="$2"; shift 2 ;;
        --ledger) LEDGER="$2"; LEDGER_SET=1; shift 2 ;;
        --hold) HOLD="$2"; shift 2 ;;
        --stop) STOP="$2"; STOP_SET=1; shift 2 ;;
        --cell) CELL="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        --case) SELF_TEST_CASE="$2"; shift 2 ;;
        --real) REAL=1; shift ;;
        --rehearsal) REHEARSAL=1; shift ;;
        --harness) HARNESS="$2"; shift 2 ;;
        --max-phase) MAX_PHASE="$2"; shift 2 ;;
        -h|--help) usage ;;
        *) log "unknown argument: $1"; usage ;;
    esac
done

# R82 F1: opencode is the graded harness; omp stays for smoke and
# rehearsals. Only those two names are accepted.
case "$HARNESS" in
    omp|opencode) ;;
    *) log "--harness must be omp or opencode (got '$HARNESS')"; exit 2 ;;
esac
[[ "$LANES" =~ ^[1-9][0-9]*$ ]] || { log "--lanes must be a positive integer"; exit 2; }
[[ "$CAP" =~ ^[0-9]+([.][0-9]+)?$ ]] || { log "--cap must be a non-negative number"; exit 2; }
[[ "$MAX_PHASE" =~ ^[1-4]$ ]] || { log "--max-phase must be 1-4"; exit 2; }
if [[ "$REAL" == "1" && "$SELF_TEST" != "1" ]]; then
    log "--real needs --self-test"
    exit 2
fi
# R90 M5: a paid grid needs a cap. An explicit `--cap 0` stays an
# informed choice; a rehearsal and a dry run spend nothing.
if [[ "$SELF_TEST" != "1" && "$REHEARSAL" != "1" && "$DRY_RUN" != "1" \
    && "$CAP_SET" != "1" ]]; then
    log "a paid grid needs --cap <usd>; refusing to start uncapped"
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
fi
LEDGER="${LEDGER:-$RUNS/.grid-ledger.tsv}"
HOLD="${HOLD:-$RUNS/.grid-hold}"
if [[ "$STOP_SET" != "1" ]]; then
    if [[ "$REHEARSAL" == "1" ]]; then
        STOP="$RUNS/.grid-rehearsal-stop.$(ledger_slug)"
    else
        STOP="$RUNS/.grid-stop.$(ledger_slug)"
    fi
fi
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
    # career model phase worker campaign_id points cost stop token billed
    # R90 M2: column 7 is what the cap counts (the larger of list and the
    # provider's bill); column 11 records the bill for audit. Old
    # 10-column rows parse with an empty $11.
    local career="$1" model="$2" phase="$3" worker="$4" id="$5"
    local points="$6" cost="$7" stop="$8" token="$9" billed="${10:-0}"
    (
        flock 200
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$career" "$model" "$phase" "$worker" "$id" "$points" "$cost" \
            "$stop" "$(date -u +%FT%TZ)" "$HARNESS" "$billed" >> "$LEDGER"
        if [[ -n "$token" ]]; then rm -f "$token"; fi
    ) 200>>"$LEDGER.lock"
}

# R93 G1: mark the row a guard just classified, without a second row (a
# restart chains from the last row; a duplicate phase row would corrupt
# chain_from).
ledger_mark_last_stop() {
    local career="$1" phase="$2" stop="$3"
    (
        flock 200
        awk -F'\t' -v c="$career" -v p="$phase" -v r="$stop" '
            { line[NR] = $0 }
            $1 == c && $3 == p { last = NR }
            END {
                for (i = 1; i <= NR; i++) {
                    if (i != last) { print line[i]; continue }
                    n = split(line[i], f, "\t")
                    f[8] = r
                    out = f[1]
                    for (j = 2; j <= n; j++) out = out "\t" f[j]
                    print out
                }
            }
        ' "$LEDGER" > "$LEDGER.tmp" && mv "$LEDGER.tmp" "$LEDGER"
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
        local why
        why="$(head -1 "$HOLD" 2>/dev/null || true)"
        log "hold at $HOLD; waiting"
        while [[ -f "$HOLD" ]]; do sleep 2; done
        log "hold released"
        # R93 G5: a balance hold rechecks on release; a top-up that did not
        # land re-holds instead of starting a paid phase.
        if [[ "$why" == *balance* ]]; then
            local again
            if ! again="$(balance_gate)"; then
                printf 'hold: %s\n' "$again" > "$HOLD"
                log "hold: $again (the balance is still low)"
                wait_hold
            fi
        fi
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

# R90 M2: before each phase reservation, an off-peak-only model waits for
# its window (peak_state in scripts/peak-window.sh, the same code the
# model check refuses with). The wait polls STOP so a stopped grid exits.
wait_off_peak() {
    local model="$1" phase="$2" state
    # R96 Z3: a rehearsal uses the same peak window as a paid grid (on a
    # fake clock in tests); a deepseek-named career must wait at 01:30.
    state="$(peak_state "$model")"
    [[ "$state" == "off-peak" ]] && return 0
    log "phase $phase of $model waits: $state"
    while [[ "$state" != "off-peak" ]]; do
        if [[ -f "$STOP" ]]; then
            log "STOP while waiting for $model's off-peak window"
            return 1
        fi
        sleep "${SDB_GRID_PEAK_POLL:-60}"
        state="$(peak_state "$model")"
    done
    log "off-peak for $model; phase $phase starts"
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
    local zoo_args=() i
    for i in "${!CAREERS[@]}"; do
        zoo_args+=("$i" "${CAREER_ZOO[$i]:-}" "${CAREER_START[$i]:-1}")
    done
    if ! python3 - "$LEDGER" "$RUNS" "${REHEARSAL_START_LINE:-0}" "${zoo_args[@]}" <<'PY'
import json, os, sys

ledger, runs, start = sys.argv[1], sys.argv[2], int(sys.argv[3])
triples = sys.argv[4:]
rows = {}
for line in open(ledger).read().splitlines()[start:]:
    parts = line.split("\t")
    if len(parts) < 8 or parts[0] in ("career", ""):
        continue
    try:
        idx, phase, points, cost = (
            int(parts[0]), int(parts[2]), float(parts[5]), float(parts[6]))
    except ValueError:
        sys.exit(f"bad ledger row: {line!r}")
    rows.setdefault(idx, {})[phase] = (parts[7], cost, points, parts[4])
if not rows:
    sys.exit("no rows were written by this run")

def failed_tickets(campaign_id):
    """Name the tickets a career's rollup says failed, for the FAIL line."""
    try:
        rollup = json.load(open(os.path.join(runs, campaign_id, "campaign.json")))
    except Exception:
        return []
    out = []
    for st in rollup.get("stages") or []:
        if not st.get("passed", False):
            out.append(str(st.get("id", "?")))
    return out

for n in range(0, len(triples), 3):
    idx, cand, begin = int(triples[n]), triples[n + 1], int(triples[n + 2])
    phases = rows.get(idx, {})
    if cand:
        meta = json.load(open(os.path.join(cand, "expected.json")))
        name = meta.get("candidate", os.path.basename(cand))
        want = {int(k): v for k, v in meta.get("phases", {}).items()}
    else:
        name, want = f"career {idx}", {p: {} for p in range(1, 5)}
    for phase, exp in sorted(want.items()):
        stop, cost, points, cid = phases.get(phase, ("missing", 0.0, 0.0, ""))
        tickets = failed_tickets(cid)
        ticket = tickets[0] if tickets else "-"
        if stop != "ok":
            sys.exit(f"zoo {name} phase {phase} ticket {ticket}: ended {stop}")
        if cost != 0:
            sys.exit(f"zoo {name} phase {phase} ticket {ticket}: spent ${cost}")
        target = exp.get("points")
        if target is not None and abs(points - float(target)) > 0.001:
            sys.exit(
                f"zoo {name} phase {phase} ticket {ticket}: "
                f"expected {target} points, ledger says {points}")
    extra = sorted(set(phases) - set(want))
    if extra:
        sys.exit(f"zoo {name}: unexpected phase rows {extra} (start {begin})")
PY
    then
        log "rehearsal: a career did not match its expected.json; no record"
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

# R96 Z4: a paid grid on a rules version needs a free-model canary that
# passed on that exact graded-content key. `SDB_CANARY_DIR` only redirects
# the records in self-tests; the real path is the shared gate directory.
canary_dir() {
    if [[ -n "${SDB_CANARY_DIR:-}" ]]; then
        printf '%s' "$SDB_CANARY_DIR"
    else
        printf '%s/sdb-gates' "$(cd "$ROOT" && git rev-parse --path-format=absolute --git-common-dir)"
    fi
}

canary_gate() {
    local full record
    full="$(cd "$ROOT" && scripts/flake-bar.sh --key)" || return 1
    record="$(canary_dir)/canary-$full.txt"
    if [[ ! -f "$record" ]]; then
        log "preflight: no free-model canary for key $full ($record); run scripts/free-canary.sh"
        return 1
    fi
    if ! grep -q '^RESULT: PASS$' "$record"; then
        log "preflight: the canary for $full is not a PASS ($record)"
        return 1
    fi
    if ! grep -q "^key: *$full\$" "$record"; then
        log "preflight: the canary record does not name key $full ($record); rerun scripts/free-canary.sh"
        return 1
    fi
    return 0
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
    # R94 M2: the check must match the model's current price row.
    local row_msg
    if ! row_msg="$("$ROOT/scripts/model-check.sh" --row-check "$model" 2>&1)"; then
        log "preflight: $row_msg"
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

# R90 M1: the run dir of a phase: the result's campaign id, or the newest
# campaign dir for this model and worker created after the marker (a
# killed cell writes no result).
run_dir_for() {
    local id="$1" model="$2" worker="$3" marker="$4" tag dir
    if [[ -n "$id" && -d "$RUNS/$id" ]]; then
        printf '%s' "$RUNS/$id"
        return 0
    fi
    [[ -f "$marker" ]] || return 0
    tag="${model//\//_}"; tag="${tag//:/_}"
    dir="$(find "$RUNS" -maxdepth 1 -type d \
        -name "campaign_sdb-sql_${tag}_*_w${worker}" -newer "$marker" \
        -print 2>/dev/null | xargs -r ls -dt 2>/dev/null | head -1)"
    printf '%s' "$dir"
}

# R90 M1/M2: a phase's spend from its own records: the list-price spend
# (status.json live.campaign_spent, the campaign's own running total) and
# the provider's bill (billed.jsonl's usage.cost lines). Prints
# "<list> <bill>"; missing records are 0.
run_spend() {
    local dir="$1" list=0 bill=0
    [[ -n "$dir" ]] || { printf '0 0\n'; return 0; }
    if [[ -f "$dir/status.json" ]]; then
        list="$(python3 - "$dir/status.json" <<'PY'
import json, sys
try:
    v = json.load(open(sys.argv[1]))
except Exception:
    raise SystemExit(0)
print(float((v.get("live") or {}).get("campaign_spent") or 0.0))
PY
)" || list=0
    fi
    if [[ -f "$dir/billed.jsonl" ]]; then
        bill="$(python3 - "$dir/billed.jsonl" <<'PY'
import json, sys
total = 0.0
try:
    lines = open(sys.argv[1], errors="replace").read().splitlines()
except OSError:
    raise SystemExit(0)
for line in lines:
    try:
        cost = json.loads(line).get("cost")
    except Exception:
        continue
    if isinstance(cost, (int, float)):
        total += cost
print(total)
PY
)" || bill=0
    fi
    printf '%s %s\n' "${list:-0}" "${bill:-0}"
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

# R68 F5/R90 M3: the provider-fault thresholds on a finished phase's
# final health.json (the same numbers the live check uses): 20+ turns and
# 5+ tool errors over 2 %, or 5+ empty responses over 5 %. opencode never
# writes the [BENCH-POLICY] turn= lines, so the live.log count is only a
# fallback for a run with no health file. Prints "<turns>TAB<count>TAB
# <kind>" and returns 0 on a fault.
provider_fault_detail() {
    local id="$1" log turns errors
    if [[ -f "$RUNS/$id/health.json" ]]; then
        python3 - "$RUNS/$id/health.json" <<'PY'
import json, sys
try:
    h = json.load(open(sys.argv[1]))
except Exception:
    raise SystemExit(1)
turns = int(h.get("turns") or 0)
errors = int(h.get("tool_errors") or 0)
empty = int(h.get("empty_responses") or 0)
if turns >= 20 and errors >= 5 and 100.0 * errors / turns > 2:
    print("%d\t%d\terrors" % (turns, errors))
    raise SystemExit(0)
if turns >= 20 and empty >= 5 and 100.0 * empty / turns > 5:
    print("%d\t%d\tempty responses" % (turns, empty))
    raise SystemExit(0)
raise SystemExit(1)
PY
        return
    fi
    log="$RUNS/$id/live.log"
    [[ -f "$log" ]] || return 1
    turns="$(grep -ac '\[BENCH-POLICY\] turn=' "$log" 2>/dev/null || true)"
    errors="$(grep -acE 'provider stream ended with an error|Upstream error from' "$log" 2>/dev/null || true)"
    [[ "${turns:-0}" -gt 0 && "${errors:-0}" -ge 5 ]] || return 1
    awk -v e="$errors" -v t="$turns" 'BEGIN { exit !(100 * e / t > 2) }' || return 1
    printf '%s\t%s\terrors\n' "$turns" "$errors"
}

provider_fault() {
    provider_fault_detail "$1" >/dev/null 2>&1
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
    local dir="" vals turns errors empty p50 refr refq rate_limited ratio
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
      int(h.get("rate_limited") or 0), float(h.get("billed_ratio") or 0))
PY
)" || vals=""
            if [[ -n "$vals" ]]; then
                read -r turns errors empty p50 refr refq rate_limited ratio <<< "$vals"
                if (( turns >= 20 && errors >= 5 )) \
                    && awk -v e="$errors" -v t="$turns" 'BEGIN { exit !(100 * e / t > 2) }'; then
                    printf '%s\t%s\t%s\t%s\n' "$(basename "$dir")" "$turns" "$errors" "errors" > "$out"
                    printf 'live provider fault: %s errors in %s turns\n' "$errors" "$turns" > "$dir/.live_stop"
                    [[ "$worker" == "$WORKER_BASE" ]] && : > "$(canary_failed_path)"
                    kill -- -"$cell_pid" 2>/dev/null || kill "$cell_pid" 2>/dev/null || true
                    return 0
                fi
                if (( turns >= 20 && empty >= 5 )) \
                    && awk -v e="$empty" -v t="$turns" 'BEGIN { exit !(100 * e / t > 5) }'; then
                    printf '%s\t%s\t%s\t%s\n' "$(basename "$dir")" "$turns" "$empty" "empty responses" > "$out"
                    printf 'live provider fault: %s empty responses in %s turns\n' "$empty" "$turns" > "$dir/.live_stop"
                    [[ "$worker" == "$WORKER_BASE" ]] && : > "$(canary_failed_path)"
                    kill -- -"$cell_pid" 2>/dev/null || kill "$cell_pid" 2>/dev/null || true
                    return 0
                fi
                if (( turns >= 30 )) && [[ "$worker" == "$WORKER_BASE" && ! -f "$(canary_failed_path)" ]]; then
                    : > "$(canary_ok_path)"
                fi
                if awk -v p="$p50" 'BEGIN { exit !(p > 180) }'; then
                    log "live health warning: worker $worker phase $phase median turn ${p50}s"
                fi
                if (( refq > 0 )) && awk -v r="$refr" -v q="$refq" 'BEGIN { exit !(100 * r / q > 10) }'; then
                    log "live health warning: worker $worker phase $phase ref-sql ${refr}/${refq} refused"
                fi
                # R90 M2: a running phase billed above 1.25x list means the
                # pin did not hold; hold the grid at its next boundary. The
                # hold file is shared with the operator's `gap`, so an
                # existing hold is never overwritten.
                if awk -v r="$ratio" 'BEGIN { exit !(r > 1.25) }'; then
                    if [[ ! -f "$HOLD" ]]; then
                        printf 'hold: billed ratio %s (worker %s phase %s)\n' \
                            "$ratio" "$worker" "$phase" > "$HOLD"
                    fi
                    log "live health warning: worker $worker phase $phase billed ratio $ratio"
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
    local candidate="${7:-}" seed="${8:-}"
    exec setsid env SDB_CELL_CAREER="$career" \
    SDB_CELL_PHASE="$phase" \
    SDB_CELL_CHAIN="$chain" \
    SDB_CELL_WORKER="$worker" \
    SDB_CELL_REHEARSAL="$REHEARSAL" \
    SDB_CELL_HARNESS="$HARNESS" \
    SDB_CONCURRENT_CELLS="$LANES" \
    SDB_ZOO_CANDIDATE="$candidate" \
    SDB_ZOO_SEED="$seed" \
    SDB_CELL_RESULT="$result" \
        "$CELL" "$model" "$PHASE_BUDGET" "$PHASE_WALL"
}

# R93 G1: the audit's red flags on a finished phase, read here (before the
# next phase) rather than by an external checker after the fact. Prints one
# line naming the reason, or nothing. A missing rollup/health is not a flag
# (a killed cell is R90 M1's spend case, and the grid's own stop already
# covers it).
phase_guard_reason() {
    local dir="$1" points="$2" cost="$3" billed="$4" rehearsal="$5"
    [[ -f "$dir/campaign.json" ]] || return 0
    python3 - "$dir" "$points" "$cost" "$billed" "$rehearsal" <<'PY'
import json, sys
dir, points, cost, billed, rehearsal = sys.argv[1:6]
points = float(points or 0)
cost = float(cost or 0)
billed = float(billed or 0)
try:
    r = json.load(open(dir + "/campaign.json"))
except Exception:
    raise SystemExit(0)
flags = ("measurement failed", "no source lines",
         "Timed out waiting for response", "panicked")
for st in r.get("stages") or []:
    f = st.get("failure") or {}
    if isinstance(f, str):
        f = {"line": f}
    text = " ".join(str(f.get(k) or "") for k in ("line", "detail", "stderr_tail"))
    low = text.lower()
    for flag in flags:
        if flag.lower() in low:
            line = (f.get("line") or f.get("detail") or text).replace("\n", " ")
            print("stage %s: %s: %s" % (st.get("id"), flag, line[:160]))
            raise SystemExit(0)
try:
    h = json.load(open(dir + "/health.json"))
except Exception:
    h = {}
turns = int(h.get("turns") or 0)
errors = int(h.get("tool_errors") or 0)
empty = int(h.get("empty_responses") or 0)
p50 = float(h.get("turn_secs_p50") or 0)
refq = int(h.get("ref_queries") or 0)
refr = int(h.get("ref_refused") or 0)
ratio = float(h.get("billed_ratio") or 0)
if turns >= 20 and errors >= 5 and 100.0 * errors / turns > 2:
    print("health: %d tool errors in %d turns" % (errors, turns))
elif turns >= 20 and empty >= 5 and 100.0 * empty / turns > 5:
    print("health: %d empty responses in %d turns" % (empty, turns))
elif ratio > 1.25:
    print("health: billed ratio %.2f" % ratio)
elif p50 > 180:
    print("health: median turn %.0fs" % p50)
elif refq > 0 and 100.0 * refr / refq > 10:
    print("health: ref-sql %d/%d refused" % (refr, refq))
elif rehearsal != "1" and points == 0:
    print("0 points on an ok phase")
elif cost > 0.01 and billed < 0.5 * cost:
    print("bill $%.4f under half the list $%.4f" % (billed, cost))
PY
}

run_lane() {
    local lane="$1"
    local worker=$((WORKER_BASE + lane))
    local idx model phase chain result id rc token last_phase last_id last_stop start_phase
    local failed=0 provider_turns=0 provider_errors=0 fault_kind="" faults=0 first_fault=""
    local cell_pid=""
    # R93 G5: a TERM to the grid kills the lanes it started; a lane kills
    # the cell's process group (run_cell execs setsid) before it exits.
    trap 'if [[ -n "${cell_pid:-}" ]]; then kill -- -"$cell_pid" 2>/dev/null || kill "$cell_pid" 2>/dev/null || true; fi; exit 143' TERM
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
            start_phase="${CAREER_START[$idx]:-1}"
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
            if ! wait_off_peak "$model" "$phase"; then
                return "$failed"
            fi
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
            run_cell "$idx" "$model" "$phase" "$chain" "$worker" "$result" \
                "${CAREER_ZOO[$idx]:-}" "${ZOO_SEEDS[$idx]:-}" &
            cell_pid=$!
            live_check "$model" "$worker" "$phase" "$cell_pid" "$marker" "$live_result" &
            monitor_pid=$!
            wait "$cell_pid" || rc=$?
            kill "$monitor_pid" 2>/dev/null || true
            wait "$monitor_pid" 2>/dev/null || true
            id="$(result_get "$result" campaign_id)"
            local got_phase got_chain points cost stop billed dir spent_list spent_bill charged
            got_phase="$(result_get "$result" phase)"
            got_chain="$(result_get "$result" chain_from)"
            points="$(result_get "$result" points)"
            cost="$(result_get "$result" cost_usd)"
            billed="$(result_get "$result" billed_usd)"
            # R90 M1: a killed or faulted cell writes no result; read the
            # run's own records so its spend still reaches the ledger and
            # the cap.
            dir="$(run_dir_for "$id" "$model" "$worker" "$marker")"
            read -r spent_list spent_bill < <(run_spend "$dir")
            if [[ -z "$id" && -n "$dir" ]]; then
                id="$(basename "$dir")"
            fi
            # The recovered spend is a floor, not a reformat: the result's
            # own numbers stay as they are (the hold line names them).
            cost="${cost:-0}"
            billed="${billed:-0}"
            if awk -v a="$cost" -v b="${spent_list:-0}" \
                'BEGIN { exit !(b > a) }'; then
                cost="$spent_list"
            fi
            if awk -v a="$billed" -v b="${spent_bill:-0}" \
                'BEGIN { exit !(b > a) }'; then
                billed="$spent_bill"
            fi
            rm -f "$marker"
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
                read -r live_id provider_turns provider_errors fault_kind < "$live_result"
                [[ -n "$live_id" ]] && id="$live_id"
                [[ -n "$fault_kind" ]] || fault_kind="errors"
                stop="provider_fault"
            fi
            # R68 F5: a finished phase whose live.log carries provider
            # tool-call errors above 2% of its turns is a provider fault,
            # not a model result. The ledger row says so and the run is
            # hidden before anything can publish it.
            if [[ "$stop" == "ok" && -n "$id" && "$id" != "—" ]] && provider_fault "$id"; then
                local fault_turns fault_count detail_kind
                read -r fault_turns fault_count detail_kind < <(provider_fault_detail "$id")
                provider_turns="$fault_turns"
                provider_errors="$fault_count"
                fault_kind="$detail_kind"
                stop="provider_fault"
            fi
            # R90 M2: the cap counts the larger of the list spend and the
            # provider's bill; the bill is recorded beside it.
            charged="$(awk -v l="${cost:-0}" -v b="${billed:-0}" \
                'BEGIN { printf "%.8f", (l > b ? l : b) }')"
            ledger_add "$idx" "$model" "$phase" "$worker" "${id:-—}" \
                "${points:-0}" "$charged" "$stop" "$token" "$billed"
            rm -f "$result" "$live_result"
            if [[ "$stop" == "provider_fault" ]]; then
                mkdir -p "$RUNS/$id"
                printf 'provider fault: %s %s in %s turns (%.2f%%)\n' \
                    "$provider_errors" "${fault_kind:-errors}" "$provider_turns" \
                    "$(awk -v e="$provider_errors" -v t="$provider_turns" \
                        'BEGIN { printf "%.2f", 100 * e / t }')" \
                    > "$RUNS/$id/.portal_hidden"
                printf 'hold: provider fault: %s %s in %s turns\n' \
                    "$provider_errors" "${fault_kind:-errors}" "$provider_turns" > "$HOLD"
                log "lane $lane: career $idx $model phase $phase provider fault: $provider_errors ${fault_kind:-errors} in $provider_turns turns; hidden; hold at $HOLD"
                wait_hold
                faults=$((faults + 1))
                if [[ "$faults" -ge 2 ]]; then
                    {
                        echo "STOP: career $idx $model phase $phase provider fault twice"
                        echo "fault 1: $first_fault"
                        echo "fault 2: $provider_errors ${fault_kind:-errors} in $provider_turns turns"
                    } > "$STOP"
                    log "lane $lane: career $idx $model phase $phase faulted twice; STOP"
                    return 1
                fi
                first_fault="$provider_errors ${fault_kind:-errors} in $provider_turns turns"
                phase=$((phase - 1))
                continue
            fi
            # R93 G1: the audit's red flags on the finished phase, after its
            # ledger row and before the next phase starts. The row is
            # marked (not duplicated) so a restart skips the career.
            local guard=""
            if [[ "$stop" == "ok" && -n "$id" && "$id" != "—" && -n "$dir" ]]; then
                guard="$(phase_guard_reason "$dir" "${points:-0}" "${cost:-0}" "${billed:-0}" "$REHEARSAL")"
            fi
            if [[ -n "$guard" ]]; then
                if [[ "$REHEARSAL" == "1" ]]; then
                    # R96 Z1: the guard still runs on a $0 career, but a
                    # replay fixture's flaky coverage reading must not
                    # hold every lane of a rehearsal. Paid phases keep
                    # the hold and the hidden marker below.
                    log "rehearsal: guard note (not held): $guard"
                else
                    stop="guard: $guard"
                fi
            fi
            if [[ "$stop" == guard:* ]]; then
                ledger_mark_last_stop "$idx" "$phase" "$stop"
                mkdir -p "$RUNS/$id"
                printf '%s\n' "$stop" > "$RUNS/$id/.portal_hidden"
                if [[ ! -f "$HOLD" ]]; then
                    printf 'hold: phase %s: %s\n' "$phase" "${stop#guard: }" > "$HOLD"
                fi
                log "lane $lane: career $idx $model phase $phase held: ${stop#guard: }"
                wait_hold
                return "$failed"
            fi
            # R67 F2: a bill above 1.25x list means the pin did not hold.
            if [[ "$stop" == "ok" && -n "$billed" && "$cost" != "0" ]] \
                && awk -v b="$billed" -v l="$cost" 'BEGIN { exit !(b > 1.25 * l) }'; then
                printf 'hold: billed $%s vs list $%s\n' "$billed" "$cost" > "$HOLD"
                log "hold: billed \$$billed vs list \$$cost"
                wait_hold
            fi
            if [[ "$stop" != "ok" ]]; then
                # R93 G1: a non-ok stop is a red flag too: name it in the
                # hold and hide the phase before the existing STOP.
                if [[ -n "${id:-}" && "$id" != "—" ]]; then
                    mkdir -p "$RUNS/$id"
                    printf 'stop: %s\n' "$stop" > "$RUNS/$id/.portal_hidden" 2>/dev/null || true
                fi
                if [[ ! -f "$HOLD" ]]; then
                    printf 'hold: phase %s stopped: %s\n' "$phase" "$stop" > "$HOLD"
                fi
                log "lane $lane: career $idx $model phase $phase stopped: $stop"
                touch "$STOP"
                return "$failed"
            fi
            # R90 M3: a clean phase on the canary lane reopens the lanes a
            # stopped canary closed; the grid's wait loop watches .canary_ok.
            if [[ "$worker" == "$WORKER_BASE" && -f "$(canary_failed_path)" ]]; then
                rm -f "$(canary_failed_path)"
                : > "$(canary_ok_path)"
                log "lane $lane: the canary recovered on $model phase $phase; opening the other lanes"
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

# R93 G5: one grid per ledger. The lifetime lock is separate from the
# short-lived $LEDGER.lock the ledger writes use.
run_grid_locked() {
    exec 9>"$LEDGER.grid.lock"
    if ! flock -n 9; then
        log "another grid holds $LEDGER.grid.lock; refusing to start"
        return 1
    fi
    run_grid
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
    # R96 Z4: one free canary per rules version, before any paid model.
    # The self-test's SDB_GRID_SKIP_PREFLIGHT bypasses model checks, but a
    # case that sets SDB_GRID_CANARY_TEST still exercises this gate.
    if [[ "$REHEARSAL" != "1" && "$DRY_RUN" != "1" ]] \
        && { [[ "${SDB_GRID_SKIP_PREFLIGHT:-0}" != "1" ]] \
             || [[ "${SDB_GRID_CANARY_TEST:-0}" == "1" ]]; }; then
        if [[ "${SDB_GRID_NO_CANARY:-0}" == "1" ]]; then
            log "canary gate bypassed by SDB_GRID_NO_CANARY=1 (operator only)"
        else
            canary_gate || return 1
        fi
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
    # R83 F2: the reservations go on every exit, including a SIGTERM'd
    # grid; the model check reads them as live only while their PID is.
    trap 'rm -rf "$RUNNING_DIR"' EXIT
    trap 'for p in "${pids[@]:-}"; do kill -- -"$p" 2>/dev/null || kill "$p" 2>/dev/null || true; done; rm -rf "$RUNNING_DIR"; exit 143' TERM
    trap 'for p in "${pids[@]:-}"; do kill -- -"$p" 2>/dev/null || kill "$p" 2>/dev/null || true; done; rm -rf "$RUNNING_DIR"; exit 130' INT
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
    # R93 G5: a restart never deletes a stop reason silently: keep it
    # (archived beside the stop file) and say why the last grid stopped.
    if [[ -f "$STOP" ]]; then
        local previous
        previous="$(head -1 "$STOP" 2>/dev/null || true)"
        log "previous stop ($STOP): ${previous:-no reason}"
        mv "$STOP" "$STOP.$(date -u +%Y%m%dT%H%M%SZ)" 2>/dev/null || true
    fi
    if [[ ! -f "$LEDGER" ]]; then
        printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\tharness\tbilled_usd\n' > "$LEDGER"
    fi
    local pids=() lane
    local rc=0
    local lanes_opened=0
    local canary="${SDB_GRID_CANARY:-1}"
    if [[ "$REHEARSAL" == "1" || "$DRY_RUN" == "1" ]]; then
        canary=0
    fi
    mkdir -p "$RUNS"
    rm -f "$(canary_ok_path)" "$(canary_failed_path)"
    if [[ "$canary" == "1" && "$LANES" -gt 1 ]]; then
        # R74 F2: lane 1 starts alone; the other lanes start only after
        # its live check passed 30 turns cleanly. A stopped canary leaves
        # .canary_failed and the other lanes never start.
        run_lane 0 &
        local canary_pid=$!
        pids+=("$canary_pid")
        while kill -0 "$canary_pid" 2>/dev/null; do
            [[ -f "$(canary_ok_path)" ]] && break
            sleep 0.2
        done
        local canary_done=0
        kill -0 "$canary_pid" 2>/dev/null || canary_done=1
        if [[ -f "$(canary_ok_path)" ]] \
            || [[ "$canary_done" -eq 1 && ! -f "$(canary_failed_path)" && ! -f "$STOP" ]]; then
            for ((lane = 1; lane < LANES; lane++)); do
                run_lane "$lane" &
                pids+=("$!")
            done
            lanes_opened=1
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
    if [[ "$canary" == "1" && "$LANES" -gt 1 && "$lanes_opened" -eq 0 ]]; then
        log "the canary never opened the other lanes (see $(canary_failed_path)); lanes 2-$LANES did not start"
    fi
    rm -rf "$RUNNING_DIR"
    return "$rc"
}

load_careers() {
    local file="$1" i tok dir start
    [[ -f "$file" ]] || { log "careers file not found: $file"; exit 2; }
    mapfile -t CAREERS < <(grep -vE '^[[:space:]]*(#|$)' "$file" | awk '{print $1}')
    mapfile -t ZOO_TOKENS < <(grep -vE '^[[:space:]]*(#|$)' "$file" | awk '{print $2}')
    [[ ${#CAREERS[@]} -gt 0 ]] || { log "no careers in $file"; exit 2; }
    CAREER_ZOO=()
    CAREER_START=()
    ZOO_SEEDS=()
    for i in "${!CAREERS[@]}"; do
        tok="${ZOO_TOKENS[$i]:-}"
        if [[ "$tok" == zoo:* ]]; then
            dir="$ROOT/tests/fixtures/zoo/${tok#zoo:}"
            [[ -f "$dir/expected.json" ]] || {
                log "career $i names a zoo candidate without expected.json: $dir"; exit 2; }
            CAREER_ZOO[$i]="$dir"
            start="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("start_phase", 1))' "$dir/expected.json")"
            CAREER_START[$i]="$start"
            ZOO_SEEDS[$i]="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("sentinel_seed", ""))' "$dir/expected.json")"
        else
            CAREER_ZOO[$i]=""
            CAREER_START[$i]=1
        fi
        ZOO_SEEDS[$i]=""
    done
}

# R96 Z1: a $0 rehearsal runs the reference careers named in the file and,
# besides them, every flawed candidate in tests/fixtures/zoo. The zoo
# entries are internal (after the file's careers), so the frozen careers
# copy and a paid grid's resume indices stay untouched.
zoo_add_careers() {
    local dir meta model seed start
    [[ "$REHEARSAL" == "1" && "${SDB_GRID_NO_ZOO:-0}" != "1" ]] || return 0
    [[ -d "$ROOT/tests/fixtures/zoo" ]] || return 0
    for dir in "$ROOT"/tests/fixtures/zoo/*; do
        [[ -f "$dir/expected.json" ]] || continue
        meta="$dir/expected.json"
        model="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["model"])' "$meta")"
        seed="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("sentinel_seed", ""))' "$meta")"
        start="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("start_phase", 1))' "$meta")"
        CAREERS+=("$model")
        CAREER_ZOO+=("$dir")
        CAREER_START+=("$start")
        ZOO_SEEDS+=("$seed")
    done
    log "zoo: ${#CAREER_ZOO[@]} careers, ${#CAREERS[@]} total"
}

# R90 M1-M5: the fake cell of the named acceptance cases. It writes the
# same records a real campaign does (status.json's live.campaign_spent,
# billed.jsonl, health.json), can die before its result (a killed phase),
# and can fault a phase's health on its first attempts.
r90_write_cell() {
    local fake="$1"
    cat > "$fake" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
model="$1"
career="${SDB_CELL_CAREER:?}"
phase="${SDB_CELL_PHASE:?}"
worker="${SDB_CELL_WORKER:?}"
chain="${SDB_CELL_CHAIN:-}"
runs="${SDB_RUNS_DIR:?}"
tag="${model//\//_}"; tag="${tag//:/_}"
id="campaign_sdb-sql_${tag}_$(date -u +%Y%m%d_%H%M%S)_$$_w${worker}"
mkdir -p "$runs/$id"
if [[ -n "${SDB_R90_SPENT:-}" ]]; then
    python3 - "$runs/$id/status.json" "$SDB_R90_SPENT" <<'PY'
import json, sys
json.dump({"live": {"campaign_spent": float(sys.argv[2])}},
          open(sys.argv[1], "w"))
PY
fi
if [[ -n "${SDB_R90_BILLED:-}" ]]; then
    printf '{"id":"call-1","model":"m","cost":%s,"status":200}\n' \
        "$SDB_R90_BILLED" > "$runs/$id/billed.jsonl"
fi
if [[ -n "${SDB_R90_ROLLUP:-}" ]]; then
    printf '%s\n' "$SDB_R90_ROLLUP" > "$runs/$id/campaign.json"
fi
if [[ -n "${SDB_R90_HEALTH:-}" ]]; then
    n=0
    if [[ -n "${SDB_R90_FAULT_ATTEMPTS:-}" ]]; then
        count="$runs/.r90-fault-count"
        [[ -f "$count" ]] && n="$(cat "$count")"
        n=$((n + 1))
        printf '%s' "$n" > "$count"
    fi
    if [[ -z "${SDB_R90_FAULT_ATTEMPTS:-}" || "$n" -le "$SDB_R90_FAULT_ATTEMPTS" ]]; then
        printf '%s\n' "$SDB_R90_HEALTH" > "$runs/$id/health.json"
        [[ -n "${SDB_R90_LIVE_SLEEP:-}" ]] && sleep "$SDB_R90_LIVE_SLEEP"
    fi
fi
if [[ -n "${SDB_R90_PIDFILE:-}" ]]; then
    printf '%s\n' "$$" > "$SDB_R90_PIDFILE"
fi
if [[ -n "${SDB_R90_SLEEP:-}" ]]; then
    sleep "$SDB_R90_SLEEP"
fi
if [[ "${SDB_R90_KILL_PHASE:-0}" == "$phase" ]]; then
    kill -9 $$
fi
python3 - "$SDB_CELL_RESULT" "$id" "$phase" "$worker" "$chain" \
    "${SDB_R90_COST:-0.1}" "${SDB_R90_BILLED:-0}" <<'PY'
import json, sys
out, cid, phase, worker, chain, cost, billed = sys.argv[1:8]
json.dump({
    "campaign_id": cid,
    "phase": int(phase),
    "chain_from": chain,
    "worker": int(worker),
    "chain": chain,
    "cost_usd": float(cost),
    "billed_usd": float(billed),
    "points": 10.0,
    "over_budget": False,
    "over_wall": False,
    "harness_fault": None,
    "infra_error": False,
    "wall_stop": False,
    "phase_complete": True,
}, open(out, "w"))
PY
EOF
    chmod +x "$fake"
}

# The common setup of a named case: a fresh scenario dir, the fake cell,
# and the ledger/hold/stop paths.
r90_case_setup() {
    local name="$1"
    R90_DIR="$(mktemp -d "/tmp/sdb-grid-r90-${name}.XXXXXX")"
    R90_RUNS="$R90_DIR/runs"
    mkdir -p "$R90_RUNS"
    RUNS="$R90_RUNS"
    LEDGER="$R90_DIR/ledger.tsv"
    HOLD="$R90_DIR/hold"
    STOP="$R90_DIR/stop"
    CELL="$R90_DIR/cell.sh"
    r90_write_cell "$CELL"
    printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\tharness\tbilled_usd\n' > "$LEDGER"
    CAREERS_FILE="$R90_DIR/careers.txt"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=1
    WORKER_BASE=1
    REHEARSAL=0
    DRY_RUN=0
    HARNESS=opencode
    export SDB_RUNS_DIR="$RUNS"
    export SDB_GRID_SKIP_PREFLIGHT=1
    export SDB_GRID_CANARY=0
    export SDB_GRID_TRACE="$R90_DIR/trace.tsv"
    # R93 G1: a real phase records the provider's bill beside the list;
    # the clean self-test phases do too, so the guard's lost-bill check
    # does not fire on them.
    export SDB_R90_BILLED=0.1
}

r90_case_done() {
    local name="$1"
    rm -rf "${R90_DIR:-}"
    echo "[self-test] $name"
}

# End a background grid by its stop file and wait for it, so no lane or
# cell outlives the case holding the cargo-test lock.
r90_stop_grid() {
    local pid="$1"
    : > "$STOP"
    for _ in $(seq 1 100); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
    done
    wait "$pid" 2>/dev/null || true
}

# R90 M1: a killed phase's ledger row carries what it spent (status.json
# and billed.jsonl), and the cap's next reservation counts it.
r90_m1_killed_phase_spend_counts() {
    r90_case_setup r90_m1_killed_phase_spend_counts
    printf 'c1\n' > "$CAREERS_FILE"
    export SDB_R90_SPENT=0.186 SDB_R90_BILLED=0.196 SDB_R90_KILL_PHASE=1
    LANES=1
    CAP=0.2
    PHASE_BUDGET=0.15
    MAX_PHASE=2
    load_careers "$CAREERS_FILE"
    run_grid || true
    export -n SDB_R90_SPENT SDB_R90_BILLED SDB_R90_KILL_PHASE
    unset SDB_R90_SPENT SDB_R90_BILLED SDB_R90_KILL_PHASE
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 1 ]] || {
        echo "killed spend: expected one row, got $rows"; return 1; }
    cost="$(awk -F'\t' 'NR == 2 { print $7 }' "$LEDGER")"
    billed="$(awk -F'\t' 'NR == 2 { print $11 }' "$LEDGER")"
    ok="$(awk -v c="$cost" -v b="$billed" 'BEGIN {
        exit !(c > 0.195999 && c < 0.196001 && b > 0.195999 && b < 0.196001) }' && echo yes || true)"
    [[ "$ok" == "yes" ]] || {
        echo "killed spend: the row says cost '$cost' bill '$billed', want 0.196"; return 1; }
    [[ -f "$STOP" ]] || {
        echo "killed spend: the cap did not stop the next phase"; return 1; }
    r90_case_done r90_m1_killed_phase_spend_counts
}

# R90 M2: the cap counts the larger of list and the provider's bill; the
# ledger records both.
r90_m2_cap_counts_the_real_bill() {
    r90_case_setup r90_m2_cap_counts_the_real_bill
    printf 'c1\n' > "$CAREERS_FILE"
    export SDB_R90_COST=0.1 SDB_R90_BILLED=0.25
    LANES=1
    CAP=0.3
    PHASE_BUDGET=0.1
    MAX_PHASE=2
    load_careers "$CAREERS_FILE"
    ( run_grid >"$R90_DIR/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 50); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    [[ -f "$HOLD" ]] || {
        kill "$pid" 2>/dev/null || true
        echo "real bill: the 2.5x bill did not hold the grid"; return 1; }
    grep -q 'hold: billed \$0.25 vs list \$0.1' "$R90_DIR/out" || {
        kill "$pid" 2>/dev/null || true
        echo "real bill: the hold does not name the bill: $(cat "$R90_DIR/out")"; return 1; }
    rm -f "$HOLD"
    # A fix that does not count the bill lets phase 2 start, and its own
    # bill holds the grid again: bound the wait so the test fails instead
    # of hanging.
    for _ in $(seq 1 100); do
        kill -0 "$pid" 2>/dev/null || break
        if [[ -f "$HOLD" ]]; then
            kill "$pid" 2>/dev/null || true
            break
        fi
        sleep 0.1
    done
    wait "$pid" 2>/dev/null || true
    unset SDB_R90_COST SDB_R90_BILLED
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 1 ]] || {
        echo "real bill: expected the cap to stop phase 2 (1 row), got $rows"; return 1; }
    cost="$(awk -F'\t' 'NR == 2 { print $7 }' "$LEDGER")"
    billed="$(awk -F'\t' 'NR == 2 { print $11 }' "$LEDGER")"
    ok="$(awk -v c="$cost" -v b="$billed" 'BEGIN {
        exit !(c > 0.249999 && c < 0.250001 && b > 0.249999 && b < 0.250001) }' && echo yes || true)"
    [[ "$ok" == "yes" ]] || {
        echo "real bill: the row says cost '$cost' bill '$billed', want 0.25"; return 1; }
    r90_case_done r90_m2_cap_counts_the_real_bill
}

# R90 M2: a phase due at peak waits for off-peak; a STOP ends the wait,
# and the same grid starts the phase off-peak.
r90_m2_no_phase_starts_at_peak() {
    r90_case_setup r90_m2_no_phase_starts_at_peak
    printf 'deepseek/deepseek-v4.1-flash-1017\n' > "$CAREERS_FILE"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=1
    load_careers "$CAREERS_FILE"
    export SDB_MODEL_CHECK_NOW="2026-11-16T07:00:00Z" SDB_GRID_PEAK_POLL=0.1
    ( run_grid >"$R90_DIR/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 50); do
        grep -q 'waits: peak' "$R90_DIR/out" 2>/dev/null && break
        sleep 0.1
    done
    if ! grep -q 'waits: peak' "$R90_DIR/out"; then
        # The mutant starts the phase: stop the grid cleanly (a killed
        # subshell would leave its lane and cell behind holding locks).
        : > "$STOP"
        for _ in $(seq 1 100); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        wait "$pid" 2>/dev/null || true
        echo "peak: the grid did not wait"
        return 1
    fi
    if [[ -f "$SDB_GRID_TRACE" ]]; then
        echo "peak: a phase started at peak"
        return 1
    fi
    : > "$STOP"
    wait "$pid" 2>/dev/null || true
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 0 ]] || {
        echo "peak: a phase ran after the STOP during the wait ($rows rows)"; return 1; }
    rm -f "$STOP"
    export SDB_MODEL_CHECK_NOW="2026-11-16T16:00:00Z"
    run_grid
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 1 ]] || {
        echo "off-peak: expected one phase, got $rows"; return 1; }
    unset SDB_MODEL_CHECK_NOW SDB_GRID_PEAK_POLL
    r90_case_done r90_m2_no_phase_starts_at_peak
}

# R93 G0: the grid's model string carries the provider prefix; the peak
# window must match it, and the grid must actually wait at peak.
r93_g0_peak_matches_grid_model_names() {
    r90_case_setup r93_g0_peak_matches_grid_model_names
    printf 'openrouter/deepseek/deepseek-v4.1-flash-1017\n' > "$CAREERS_FILE"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=1
    load_careers "$CAREERS_FILE"
    got="$(SDB_MODEL_CHECK_NOW="2026-11-16T01:30:00Z" peak_state "openrouter/deepseek/deepseek-v4.1-flash-1017")"
    [[ "$got" == peak* ]] || {
        echo "peak: openrouter/deepseek at 01:30 is '$got'"; return 1; }
    export SDB_MODEL_CHECK_NOW="2026-11-16T01:30:00Z" SDB_GRID_PEAK_POLL=0.1
    ( run_grid >"$R90_DIR/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 50); do
        grep -q 'waits: peak' "$R90_DIR/out" 2>/dev/null && break
        sleep 0.1
    done
    if ! grep -q 'waits: peak' "$R90_DIR/out"; then
        r90_stop_grid "$pid"
        echo "peak: the grid did not wait for openrouter/deepseek"
        return 1
    fi
    if [[ -f "$SDB_GRID_TRACE" ]]; then
        r90_stop_grid "$pid"
        echo "peak: a phase started at peak"
        return 1
    fi
    r90_stop_grid "$pid"
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 0 ]] || {
        echo "peak: a phase ran after the STOP during the wait ($rows rows)"; return 1; }
    unset SDB_MODEL_CHECK_NOW SDB_GRID_PEAK_POLL
    r90_case_done r93_g0_peak_matches_grid_model_names
}

# R90 M3: a canary phase that faults on its first attempt is held and
# rerun; the clean rerun reopens the lanes the stopped canary closed.
r90_m3_recovered_canary_opens_lanes() {
    r90_case_setup r90_m3_recovered_canary_opens_lanes
    printf 'c1\nc2\nc3\n' > "$CAREERS_FILE"
    export SDB_R90_HEALTH='{"turns":20,"tool_errors":5,"empty_responses":0,"turn_secs_p50":1.0,"ref_queries":0,"ref_refused":0,"rate_limited":0,"billed_ratio":1.0}'
    export SDB_R90_FAULT_ATTEMPTS=1 SDB_R90_LIVE_SLEEP=0.6
    export SDB_GRID_HEALTH_POLL=0.1 SDB_GRID_CANARY=1
    LANES=3
    CAP=0
    MAX_PHASE=1
    load_careers "$CAREERS_FILE"
    ( run_grid >"$R90_DIR/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 100); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    [[ -f "$HOLD" ]] || {
        r90_stop_grid "$pid"
        echo "canary: the faulted phase did not hold"; return 1; }
    [[ -f "$(canary_failed_path)" ]] || {
        r90_stop_grid "$pid"
        echo "canary: the fault did not record .canary_failed"; return 1; }
    rm -f "$HOLD"
    rc=0
    wait "$pid" || rc=$?
    [[ "$rc" -eq 0 ]] || {
        echo "canary: the grid did not finish after the recovery (rc=$rc)"; return 1; }
    workers="$(awk -F'\t' 'NR > 1 { print $4 }' "$LEDGER" | sort -u | paste -sd, -)"
    [[ "$workers" == "1,2,3" ]] || {
        echo "canary: a recovered canary did not open the lanes (workers $workers)"; return 1; }
    [[ ! -f "$(canary_failed_path)" ]] || {
        echo "canary: .canary_failed survived the clean rerun"; return 1; }
    grep -q 'canary recovered' "$R90_DIR/out" || {
        echo "canary: the recovery was not logged"; return 1; }
    unset SDB_R90_HEALTH SDB_R90_FAULT_ATTEMPTS SDB_R90_LIVE_SLEEP SDB_GRID_HEALTH_POLL SDB_GRID_CANARY
    r90_case_done r90_m3_recovered_canary_opens_lanes
}

# R90 M3: a finished phase's final health.json (not the opencode-less
# [BENCH-POLICY] lines) decides the provider fault.
r90_m3_post_phase_fault_reads_health() {
    r90_case_setup r90_m3_post_phase_fault_reads_health
    printf 'c1\n' > "$CAREERS_FILE"
    export SDB_R90_HEALTH='{"turns":33,"tool_errors":5,"empty_responses":0,"turn_secs_p50":1.0,"ref_queries":0,"ref_refused":0,"rate_limited":0,"billed_ratio":1.0}'
    LANES=1
    CAP=0
    MAX_PHASE=1
    load_careers "$CAREERS_FILE"
    ( run_grid >"$R90_DIR/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 100); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    [[ -f "$HOLD" ]] || {
        r90_stop_grid "$pid"
        echo "health fault: no hold"; return 1; }
    grep -q 'provider fault' "$R90_DIR/out" || {
        r90_stop_grid "$pid"
        echo "health fault: the grid did not name the provider fault"; return 1; }
    id="$(awk -F'\t' 'NR == 2 { print $5 }' "$LEDGER")"
    [[ -f "$RUNS/$id/.portal_hidden" ]] || {
        r90_stop_grid "$pid"
        echo "health fault: the phase was not hidden"; return 1; }
    grep -q '5 errors in 33 turns' "$RUNS/$id/.portal_hidden" || {
        r90_stop_grid "$pid"
        echo "health fault: the hidden file's counts are wrong: $(cat "$RUNS/$id/.portal_hidden")"
        return 1; }
    : > "$STOP"
    rm -f "$HOLD"
    wait "$pid" 2>/dev/null || true
    unset SDB_R90_HEALTH
    r90_case_done r90_m3_post_phase_fault_reads_health
}

# R93 G1: a phase whose rollup carries a grader/infra failure is held
# before the next phase, and the hold names it.
r93_g1_pre_phase_guard() {
    r90_case_setup r93_g1_pre_phase_guard
    printf 'c1\n' > "$CAREERS_FILE"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=2
    export SDB_R90_BILLED=0.1
    export SDB_R90_ROLLUP='{"points":10.0,"stages":[{"id":1,"passed":false,"failure":{"kind":"own_stage","line":"ticket 15: coverage measurement failed","detail":"coverage measurement failed: the measurement did not finish"}}]}'
    load_careers "$CAREERS_FILE"
    ( run_grid >"$R90_DIR/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 100); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    if [[ ! -f "$HOLD" ]]; then
        rm -f "$HOLD"
        r90_stop_grid "$pid"
        echo "guard: the failing phase did not hold"
        return 1
    fi
    grep -q 'coverage measurement failed' "$HOLD" || {
        rm -f "$HOLD"
        r90_stop_grid "$pid"
        echo "guard: the hold does not name the failure: $(cat "$HOLD")"
        return 1
    }
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 1 ]] || {
        rm -f "$HOLD"
        r90_stop_grid "$pid"
        echo "guard: the next phase started ($rows rows)"
        return 1
    }
    id="$(awk -F'\t' 'NR == 2 { print $5 }' "$LEDGER")"
    [[ -f "$RUNS/$id/.portal_hidden" ]] || {
        rm -f "$HOLD"
        r90_stop_grid "$pid"
        echo "guard: the phase was not hidden"
        return 1
    }
    rm -f "$HOLD"
    wait "$pid" 2>/dev/null || true
    unset SDB_R90_ROLLUP SDB_R90_BILLED
    r90_case_done r93_g1_pre_phase_guard
}

# R93 G1: a clean phase (no failure text, ten points, a real bill) does
# not hold.
r93_g1_clean_phase_does_not_hold() {
    r90_case_setup r93_g1_clean_phase_does_not_hold
    printf 'c1\n' > "$CAREERS_FILE"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=2
    export SDB_R90_BILLED=0.1
    export SDB_R90_ROLLUP='{"points":10.0,"stages":[{"id":1,"passed":true,"failure":null}]}'
    load_careers "$CAREERS_FILE"
    rc=0
    run_grid || rc=$?
    [[ "$rc" -eq 0 ]] || {
        echo "clean phase: the grid failed (rc=$rc)"; return 1; }
    [[ ! -f "$HOLD" ]] || {
        echo "clean phase: a clean phase held: $(cat "$HOLD")"; return 1; }
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 2 ]] || {
        echo "clean phase: expected two phases, got $rows"; return 1; }
    unset SDB_R90_ROLLUP SDB_R90_BILLED
    r90_case_done r93_g1_clean_phase_does_not_hold
}

# R87-1: an empty-response stop says "empty responses", not "0 errors".
r93_g1_empty_response_message() {
    r90_case_setup r93_g1_empty_response_message
    printf 'c1\n' > "$CAREERS_FILE"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=1
    export SDB_R90_BILLED=0.1
    export SDB_R90_HEALTH='{"turns":20,"tool_errors":0,"empty_responses":5,"turn_secs_p50":1.0,"ref_queries":0,"ref_refused":0,"rate_limited":0,"billed_ratio":1.0}'
    export SDB_R90_LIVE_SLEEP=0.8 SDB_GRID_HEALTH_POLL=0.1
    load_careers "$CAREERS_FILE"
    ( run_grid >"$R90_DIR/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 100); do
        [[ -f "$HOLD" ]] && break
        sleep 0.1
    done
    if [[ ! -f "$HOLD" ]]; then
        rm -f "$HOLD"
        r90_stop_grid "$pid"
        echo "empty response: the live stop did not hold"
        return 1
    fi
    grep -q 'empty responses' "$HOLD" || {
        rm -f "$HOLD"
        r90_stop_grid "$pid"
        echo "empty response: the hold does not say empty responses: $(cat "$HOLD")"
        return 1
    }
    if grep -q '0 errors' "$HOLD"; then
        rm -f "$HOLD"
        r90_stop_grid "$pid"
        echo "empty response: the hold says 0 errors: $(cat "$HOLD")"
        return 1
    fi
    id="$(awk -F'\t' 'NR == 2 { print $5 }' "$LEDGER")"
    grep -q 'empty responses' "$RUNS/$id/.portal_hidden" 2>/dev/null || {
        rm -f "$HOLD"
        r90_stop_grid "$pid"
        echo "empty response: the hidden file does not say empty responses"
        return 1
    }
    rm -f "$HOLD"
    : > "$STOP"
    wait "$pid" 2>/dev/null || true
    unset SDB_R90_HEALTH SDB_R90_LIVE_SLEEP SDB_GRID_HEALTH_POLL SDB_R90_BILLED
    r90_case_done r93_g1_empty_response_message
}

# R93 G5: two grids on different ledgers don't affect each other's lanes,
# the same ledger admits one grid (the lock), and the marker paths carry
# the ledger's name.
r93_g5_grid_state_is_per_ledger() {
    r90_case_setup r93_g5_grid_state_is_per_ledger
    printf 'c1\n' > "$CAREERS_FILE"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=1
    load_careers "$CAREERS_FILE"
    LEDGER="$R90_DIR/a.tsv"
    [[ "$(canary_ok_path)" == "$R90_RUNS/.canary_ok.a" ]] || {
        echo "state: canary_ok path is $(canary_ok_path)"; return 1; }
    [[ "$(canary_failed_path)" == "$R90_RUNS/.canary_failed.a" ]] || {
        echo "state: canary_failed path is $(canary_failed_path)"; return 1; }
    [[ "$(audit_marker_path)" == "$R90_RUNS/.grid-audit-marker.a" ]] || {
        echo "state: audit marker path is $(audit_marker_path)"; return 1; }
    a_stop="$R90_RUNS/.grid-stop.a"
    b_stop="$R90_RUNS/.grid-stop.b"
    export SDB_R90_SLEEP=30 SDB_R90_PIDFILE="$R90_DIR/a.pid"
    STOP="$a_stop"
    ( run_grid_locked >"$R90_DIR/a.out" 2>&1 ) &
    apid=$!
    for _ in $(seq 1 100); do
        [[ -f "$R90_DIR/a.pid" ]] && break
        sleep 0.1
    done
    if [[ ! -f "$R90_DIR/a.pid" ]]; then
        kill -TERM "$apid" 2>/dev/null || true
        wait "$apid" 2>/dev/null || true
        echo "state: the first grid never started its cell"
        return 1
    fi
    : > "$a_stop"
    # The same ledger is refused while the first grid holds its lock.
    if LEDGER="$R90_DIR/a.tsv" STOP="$a_stop" run_grid_locked >"$R90_DIR/a2.out" 2>&1; then
        kill -TERM "$apid" 2>/dev/null || true
        wait "$apid" 2>/dev/null || true
        echo "state: the same ledger admitted a second grid"
        return 1
    fi
    grep -q 'another grid holds' "$R90_DIR/a2.out" || {
        kill -TERM "$apid" 2>/dev/null || true
        wait "$apid" 2>/dev/null || true
        echo "state: the second grid did not name the lock"
        return 1
    }
    # A different ledger is not stopped by A's stop file.
    unset SDB_R90_SLEEP SDB_R90_PIDFILE
    LEDGER="$R90_DIR/b.tsv"
    STOP="$b_stop"
    run_grid || {
        kill -TERM "$apid" 2>/dev/null || true
        wait "$apid" 2>/dev/null || true
        echo "state: the second ledger's grid failed"
        return 1
    }
    rows_b=$(($(wc -l < "$R90_DIR/b.tsv") - 1))
    [[ "$rows_b" -eq 1 ]] || {
        kill -TERM "$apid" 2>/dev/null || true
        wait "$apid" 2>/dev/null || true
        echo "state: the second ledger ran $rows_b phases"
        return 1
    }
    [[ -f "$a_stop" ]] || {
        kill -TERM "$apid" 2>/dev/null || true
        wait "$apid" 2>/dev/null || true
        echo "state: A's stop file vanished"
        return 1
    }
    kill -TERM "$apid" 2>/dev/null || true
    wait "$apid" 2>/dev/null || true
    cellpid="$(cat "$R90_DIR/a.pid" 2>/dev/null || true)"
    if [[ -n "$cellpid" ]] && kill -0 "$cellpid" 2>/dev/null; then
        kill -- -"$cellpid" 2>/dev/null || kill "$cellpid" 2>/dev/null || true
        echo "state: the first grid's cell survived the TERM"
        return 1
    fi
    unset SDB_R90_SLEEP SDB_R90_PIDFILE
    r90_case_done r93_g5_grid_state_is_per_ledger
}

# R93 G5: a restart keeps the previous stop's reason (archived beside the
# stop file) instead of deleting it silently.
r93_g5_restart_keeps_stop_reason() {
    r90_case_setup r93_g5_restart_keeps_stop_reason
    printf 'c1\n' > "$CAREERS_FILE"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=1
    load_careers "$CAREERS_FILE"
    printf 'STOP: old reason\n' > "$STOP"
    run_grid 2>"$R90_DIR/out"
    grep -q 'previous stop' "$R90_DIR/out" || {
        echo "stop reason: the restart did not log the old stop"; return 1; }
    archive="$(ls "$STOP".* 2>/dev/null | head -1)"
    [[ -n "$archive" ]] || {
        echo "stop reason: the old stop file was deleted"; return 1; }
    grep -q 'STOP: old reason' "$archive" || {
        echo "stop reason: the archive lost the reason"; return 1; }
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 1 ]] || {
        echo "stop reason: expected one phase after the restart, got $rows"; return 1; }
    r90_case_done r93_g5_restart_keeps_stop_reason
}

# R93 G5: a TERM reaps the lanes and their cells, and the run dir stays
# for the audit.
r93_g5_term_kills_the_lanes() {
    r90_case_setup r93_g5_term_kills_the_lanes
    printf 'c1\n' > "$CAREERS_FILE"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=1
    export SDB_R90_SLEEP=30 SDB_R90_PIDFILE="$R90_DIR/cell.pid"
    load_careers "$CAREERS_FILE"
    ( run_grid_locked >"$R90_DIR/out" 2>&1 ) &
    pid=$!
    for _ in $(seq 1 100); do
        [[ -f "$R90_DIR/cell.pid" ]] && break
        sleep 0.1
    done
    if [[ ! -f "$R90_DIR/cell.pid" ]]; then
        kill -TERM "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        echo "term: the cell never started"
        return 1
    fi
    cellpid="$(cat "$R90_DIR/cell.pid")"
    kill -TERM "$pid"
    wait "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
        kill -0 "$cellpid" 2>/dev/null || break
        sleep 0.1
    done
    if kill -0 "$cellpid" 2>/dev/null; then
        kill -- -"$cellpid" 2>/dev/null || kill "$cellpid" 2>/dev/null || true
        echo "term: the cell survived the grid's TERM"
        return 1
    fi
    [[ -d "$R90_RUNS" ]] || {
        echo "term: the runs dir vanished"; return 1; }
    compgen -G "$R90_RUNS/campaign_*" >/dev/null || {
        echo "term: the run dir was deleted with the cell"; return 1; }
    unset SDB_R90_SLEEP SDB_R90_PIDFILE
    r90_case_done r93_g5_term_kills_the_lanes
}

# R93 G5: a released balance hold rechecks the balance and re-holds when
# the top-up did not land.
r93_g5_balance_rechecked_on_release() {
    r90_case_setup r93_g5_balance_rechecked_on_release
    printf 'c1\n' > "$CAREERS_FILE"
    LANES=1
    CAP=0
    PHASE_BUDGET=0.99
    MAX_PHASE=1
    load_careers "$CAREERS_FILE"
    bal="$R90_DIR/balance.sh"
    printf '#!/bin/sh\necho 0.50\n' > "$bal"
    chmod +x "$bal"
    export SDB_GRID_BALANCE_CMD="$bal"
    printf 'hold: balance $0.50 below $1.99\n' > "$HOLD"
    ( wait_hold ) &
    wpid=$!
    sleep 0.3
    rm -f "$HOLD"
    got=0
    for _ in $(seq 1 60); do
        if [[ -f "$HOLD" ]] && grep -q 'balance' "$HOLD"; then
            got=1
            break
        fi
        sleep 0.1
    done
    if [[ "$got" -ne 1 ]]; then
        kill "$wpid" 2>/dev/null || true
        wait "$wpid" 2>/dev/null || true
        echo "balance: the hold was not rechecked on release"
        return 1
    fi
    rm -f "$HOLD"
    kill "$wpid" 2>/dev/null || true
    wait "$wpid" 2>/dev/null || true
    unset SDB_GRID_BALANCE_CMD
    r90_case_done r93_g5_balance_rechecked_on_release
}

# R90 M5: a rehearsal on a ledger whose careers are already complete runs
# nothing and writes no PASS record.
r90_m5_stale_rehearsal_ledger_never_passes() {
    r90_case_setup r90_m5_stale_rehearsal_ledger_never_passes
    printf 'c1\n' > "$CAREERS_FILE"
    for p in 1 2 3 4; do
        printf '0\tc1\t%s\t1\told-p%s\t10\t0\tok\tx\topencode\t0\n' "$p" "$p" >> "$LEDGER"
    done
    LANES=1
    CAP=0
    PHASE_BUDGET=0
    MAX_PHASE=4
    REHEARSAL=1
    export SDB_REHEARSAL_DIR="$R90_DIR/gates"
    export SDB_MODEL_CHECK_IMAGE_DIGEST="sha256:r90"
    load_careers "$CAREERS_FILE"
    REHEARSAL_START_LINE="$(wc -l < "$LEDGER")"
    run_grid
    rows=$(($(wc -l < "$LEDGER") - 1))
    [[ "$rows" -eq 4 ]] || {
        echo "stale ledger: the grid wrote rows for a complete career ($rows)"; return 1; }
    if write_rehearsal_record 2>"$R90_DIR/rec.err"; then
        echo "stale ledger: a PASS record was written for content that did not run"; return 1
    fi
    record="$(rehearsal_record_path "$(cd "$ROOT" && scripts/flake-bar.sh --key)")"
    [[ ! -f "$record" ]] || {
        echo "stale ledger: the record exists at $record"; return 1; }
    grep -q 'no rows were written by this run' "$R90_DIR/rec.err" || {
        echo "stale ledger: the refusal did not say why: $(cat "$R90_DIR/rec.err")"; return 1; }
    unset SDB_REHEARSAL_DIR SDB_MODEL_CHECK_IMAGE_DIGEST
    REHEARSAL=0
    r90_case_done r90_m5_stale_rehearsal_ledger_never_passes
}

# R90 M5: a paid grid without --cap refuses before anything runs.
r90_m5_paid_grid_needs_a_cap() {
    r90_case_setup r90_m5_paid_grid_needs_a_cap
    printf 'c1\n' > "$CAREERS_FILE"
    rc=0
    out="$(SDB_GRID_SKIP_PREFLIGHT=1 SDB_GRID_CELL="$CELL" \
        "$ROOT/scripts/grid.sh" --careers "$CAREERS_FILE" --lanes 1 2>&1)" || rc=$?
    [[ "$rc" -eq 2 ]] || {
        echo "paid cap: the grid exited $rc without --cap, want 2: $out"; return 1; }
    grep -q 'needs --cap' <<<"$out" || {
        echo "paid cap: the refusal did not say why: $out"; return 1; }
    [[ ! -f "$SDB_GRID_TRACE" ]] || {
        echo "paid cap: a phase ran without a cap"; return 1; }
    r90_case_done r90_m5_paid_grid_needs_a_cap
}

# R96 Z1: a zoo career whose ledger points do not match expected.json
# must fail the rehearsal record and name the candidate, phase and ticket.
r96_z1_zoo_mismatch_fails_the_rehearsal() {
    r90_case_setup r96_z1_zoo_mismatch_fails_the_rehearsal
    local zoo="$R90_DIR/zoo" out rc=0
    mkdir -p "$zoo/bad" "$R90_RUNS/campaign_zoo_bad"
    printf '{"candidate":"bad","model":"m","start_phase":1,"phases":{"1":{"points":42}}}\n' \
        > "$zoo/bad/expected.json"
    printf '0\tm\t1\t1\tcampaign_zoo_bad\t10.0\t0\tok\t2026-11-22T00:00:00Z\topencode\t0\n' \
        >> "$LEDGER"
    printf '%s\n' '{"stages":[{"id":7,"passed":false}]}' \
        > "$R90_RUNS/campaign_zoo_bad/campaign.json"
    CAREERS=(m)
    CAREER_ZOO=("$zoo/bad")
    CAREER_START=(1)
    ZOO_SEEDS=("")
    REHEARSAL=1
    MAX_PHASE=4
    out="$(write_rehearsal_record 2>&1)" || rc=$?
    [[ "$rc" -ne 0 ]] || {
        echo "zoo mismatch: a wrong point total was accepted"; return 1; }
    grep -q 'zoo bad phase 1 ticket 7' <<<"$out" || {
        echo "zoo mismatch: the FAIL line does not name candidate/phase/ticket: $out"; return 1; }
    r90_case_done r96_z1_zoo_mismatch_fails_the_rehearsal
}

# R96 Z3: a rehearsal career with a real openrouter/... name waits at the
# peak window on the fake clock; off-peak the simulated key's spend equals
# the phase bills the ledger records.
r96_z3_rehearsal_waits_at_peak() {
    r90_case_setup r96_z3_rehearsal_waits_at_peak
    printf 'openrouter/deepseek/deepseek-v4.1-flash-1017\n' > "$CAREERS_FILE"
    REHEARSAL=1
    MAX_PHASE=1
    CAP=0
    load_careers "$CAREERS_FILE"
    export SDB_MODEL_CHECK_NOW="2026-11-23T01:30:00Z" SDB_GRID_PEAK_POLL=0.1
    ( run_grid >"$R90_DIR/peak.out" 2>&1 ) &
    local pid=$!
    for _ in $(seq 1 50); do
        grep -q 'waits: peak' "$R90_DIR/peak.out" 2>/dev/null && break
        sleep 0.1
    done
    if ! grep -q 'waits: peak' "$R90_DIR/peak.out"; then
        r90_stop_grid "$pid"
        echo "rehearsal peak: the deepseek-named career did not wait"; return 1
    fi
    if [[ -f "$SDB_GRID_TRACE" ]]; then
        r90_stop_grid "$pid"
        echo "rehearsal peak: a phase started at peak"; return 1
    fi
    r90_stop_grid "$pid"
    [[ $(($(wc -l < "$LEDGER") - 1)) -eq 0 ]] || {
        echo "rehearsal peak: a phase ran during the wait"; return 1; }
    unset SDB_MODEL_CHECK_NOW SDB_GRID_PEAK_POLL
    export SDB_R90_COST=0.1 SDB_R90_BILLED=0.1
    run_grid
    local key_spend ledger_billed
    key_spend="$(python3 - "$R90_RUNS" <<'PY'
import glob, json, sys
total = 0.0
for p in glob.glob(sys.argv[1] + "/*/billed.jsonl"):
    for line in open(p):
        try:
            total += float(json.loads(line).get("cost") or 0.0)
        except Exception:
            pass
print("%.6f" % total)
PY
)"
    ledger_billed="$(awk -F'\t' 'NR > 1 { s += $11 } END { printf "%.6f", s }' "$LEDGER")"
    [[ "$key_spend" == "$ledger_billed" ]] || {
        echo "rehearsal key: spend $key_spend != ledger bills $ledger_billed"; return 1; }
    r90_case_done r96_z3_rehearsal_waits_at_peak
}

# R96 Z4: a paid grid refuses to start without a passing free canary on
# the current content key, and starts with one.
r96_z4_paid_grid_needs_a_canary() {
    r90_case_setup r96_z4_paid_grid_needs_a_canary
    printf 'c1\n' > "$CAREERS_FILE"
    MAX_PHASE=1
    CAP=0
    load_careers "$CAREERS_FILE"
    local full rc=0
    full="$(cd "$ROOT" && scripts/flake-bar.sh --key)"
    export SDB_CANARY_DIR="$R90_DIR/gates" SDB_GRID_CANARY_TEST=1
    mkdir -p "$SDB_CANARY_DIR"
    run_grid >"$R90_DIR/out" 2>&1 || rc=$?
    [[ "$rc" -ne 0 ]] || {
        echo "canary: a paid grid without a canary started"; return 1; }
    grep -q 'free-model canary' "$R90_DIR/out" || {
        echo "canary: the refusal does not say why: $(cat "$R90_DIR/out")"; return 1; }
    [[ $(($(wc -l < "$LEDGER") - 1)) -eq 0 ]] || {
        echo "canary: a phase ran without a canary"; return 1; }
    printf 'key: %s\nmodel: openrouter/foo/bar:free\nRESULT: PASS\n' "$full" \
        > "$SDB_CANARY_DIR/canary-$full.txt"
    run_grid
    [[ $(($(wc -l < "$LEDGER") - 1)) -eq 1 ]] || {
        echo "canary: a paid grid with a canary did not run"; return 1; }
    r90_case_done r96_z4_paid_grid_needs_a_canary
}

self_test() {
    local dir fake trace scenario runs out got paid_rows paid_rc first_chain chain p
    # The self-test's fake rehearsals check the driver, not the zoo.
    export SDB_GRID_NO_ZOO=1
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
    if [[ -n "${SELF_TEST_CASE:-}" ]]; then
        case "$SELF_TEST_CASE" in
            r90_m1_*|r90_m2_*|r90_m3_*|r90_m5_*|r93_*|r96_*) "$SELF_TEST_CASE" ;;
            *) echo "unknown self-test case: $SELF_TEST_CASE" >&2; return 2 ;;
        esac
        return 0
    fi

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
    got="$(awk -F'\t' 'NR > 1 { print $10 }' "$LEDGER" | sort -u)"
    [[ "$got" == "opencode" ]] || {
        echo "the default ledger harness is '$got', want opencode"; exit 1; }
    rc=0
    out="$("$ROOT/scripts/grid.sh" --careers "$scenario/careers.txt" --lanes 1 \
        --harness foo --dry-run 2>&1)" || rc=$?
    [[ "$rc" -eq 2 ]] || { echo "--harness foo exited $rc, want 2"; echo "$out"; exit 1; }
    grep -q 'must be omp or opencode' <<<"$out" || {
        echo "the --harness refusal did not say why: $out"; exit 1; }

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
    printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\tharness\n' > "$LEDGER"
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
    printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\tharness\n' > "$LEDGER"
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
    printf 'career\tmodel\tphase\tworker\tcampaign_id\tpoints\tcost_usd\tstop\tts\tharness\n' > "$LEDGER"
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
        --lanes 1 --cell "$fake" --cap 0 >"$scenario/paid.out" 2>&1 || paid_rc=$?
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
        --lanes 2 --cell "$fake" --cap 0 2>&1)" || rc=$?
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
        SDB_GRID_NO_CANARY=1 SDB_LONG_TEST_LOCK="$scenario/long-test.lock"; run_grid ) \
        >"$scenario/out" 2>&1 || {
        echo "probe race: the grid failed"; cat "$scenario/out"; exit 1; }
    [[ "$(($(wc -l < "$LEDGER") - 1))" -eq 3 ]] || {
        echo "probe race: expected 3 rows, got $(($(wc -l < "$LEDGER") - 1))"
        cat "$scenario/out"; exit 1; }
    [[ ! -f "$STOP" ]] || { echo "probe race: a false STOP"; exit 1; }
    MAX_PHASE=4

    # 16b) R83 F2: a grid killed with SIGTERM leaves no res-* files.
    scenario="$dir/r83term"
    mkdir -p "$scenario"
    printf '%s\n' c1 > "$scenario/careers.txt"
    SDB_GRID_TRACE="$scenario/trace.tsv"
    SDB_GRID_SKIP_PREFLIGHT=1
    SDB_FAKE_SLEEP_ALL=1
    SDB_FAKE_SLEEP=30
    export SDB_GRID_TRACE SDB_GRID_SKIP_PREFLIGHT SDB_FAKE_SLEEP_ALL SDB_FAKE_SLEEP
    SDB_RUNS_DIR="$scenario/runs" "$ROOT/scripts/grid.sh" \
        --careers "$scenario/careers.txt" --lanes 1 --cell "$fake" \
        --cap 1.0 --ledger "$scenario/ledger.tsv" >"$scenario/out" 2>&1 &
    grid_pid=$!
    reserved=0
    for _ in $(seq 1 50); do
        if ls "$scenario/ledger.tsv.running"/res-* >/dev/null 2>&1; then
            reserved=1
            break
        fi
        sleep 0.1
    done
    if [[ "$reserved" != "1" ]]; then
        kill -TERM "$grid_pid" 2>/dev/null || true
        echo "sigterm: the grid never reserved"; cat "$scenario/out"; exit 1
    fi
    kill -TERM "$grid_pid"
    wait "$grid_pid" || true
    if ls "$scenario/ledger.tsv.running"/res-* >/dev/null 2>&1; then
        echo "sigterm: the grid left res-* files"; ls "$scenario/ledger.tsv.running"; exit 1
    fi
    unset SDB_FAKE_SLEEP_ALL SDB_FAKE_SLEEP
    export -n SDB_FAKE_SLEEP_ALL SDB_FAKE_SLEEP 2>/dev/null || true
    echo "[self-test] a SIGTERM'd grid leaves no reservations"

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
    # R82 F1: the default harness is opencode, so the record path carries
    # the opencode- prefix.
    record="$scenario/gates/model-check-opencode-test-model.txt"
    fresh="$(date -u +%FT%TZ)"
    old="$(date -u -d '8 days ago' +%FT%TZ)"

    rm -f "$record"
    rc=0
    ( export SDB_GRID_SKIP_PREFLIGHT=0 SDB_GRID_PREFLIGHT_TEST=0 \
        SDB_GRID_NO_CANARY=1; run_grid ) >/dev/null 2>&1 || rc=$?
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
    [[ -f "$(canary_ok_path)" ]] || { echo "the canary never passed 30 turns"; exit 1; }
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
    [[ -f "$(canary_failed_path)" ]] || {
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

    r96_z1_zoo_mismatch_fails_the_rehearsal
    r96_z3_rehearsal_waits_at_peak
    r96_z4_paid_grid_needs_a_canary

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
zoo_add_careers
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
AUDIT_MARKER="$(audit_marker_path)"
: > "$AUDIT_MARKER"
# R90 M5: the rehearsal record counts only the rows this run writes.
REHEARSAL_START_LINE=0
if [[ "$REHEARSAL" == "1" && -f "$LEDGER" ]]; then
    REHEARSAL_START_LINE="$(wc -l < "$LEDGER")"
fi
# R96 Z1: after recording a zoo run's actual points into expected.json,
# regenerate the record from that run's ledger without starting another
# grid. The expectations are still checked by write_rehearsal_record.
if [[ "${SDB_ZOO_RECORD_ONLY:-0}" == "1" ]]; then
    REHEARSAL_START_LINE=0
else
    run_grid_locked
fi
if [[ "$REHEARSAL" == "1" ]]; then
    write_rehearsal_record
fi
# R74 F3: audit the runs this grid produced and put the summary in the
# grid log. The audit's published set is the new, non-hidden campaigns;
# the worse-rerun comparison still sees every earlier attempt.
scripts/run-audit.sh --summary --runs-dir "$RUNS" --newer "$AUDIT_MARKER" 2>&1 \
    | while IFS= read -r line; do log "$line"; done || true
