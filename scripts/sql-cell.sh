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

# R52 F4 (round 40 F2): refuse a dirty tree and a missing/failed flake
# record before the build. The key is flake-bar.sh's one computation.
# SDB_CELL_SMOKE=1 skips the record for a sub-$0.99 smoke cell and says so.
CELL_FLAKE_KEY=""
CELL_FLAKE_RESULT=""
cell_gate() {
    local root="$1" budget="$2" smoke="$3" rehearsal="${4:-0}"
    local status tracked p
    CELL_FLAKE_KEY=""
    CELL_FLAKE_RESULT=""
    status="$(cd "$root" && git status --porcelain --untracked-files=all 2>/dev/null || true)"
    tracked="$(printf '%s\n' "$status" | grep -v '^??' || true)"
    if [[ -n "$tracked" ]]; then
        echo "[cell] FATAL: the tracked tree is dirty; commit first" >&2
        return 1
    fi
    while IFS= read -r p; do
        [[ -z "$p" ]] && continue
        case "$p" in
            src/*|campaigns/*|extensions/*|build.rs)
                echo "[cell] FATAL: untracked $p is graded content; commit it" >&2
                return 1 ;;
        esac
    done < <(printf '%s\n' "$status" | sed -n 's/^?? //p')
    if [[ "$rehearsal" == "1" ]]; then
        # R54 F2: a $0 real launch; no flake record and no API key.
        CELL_FLAKE_RESULT="REHEARSAL"
        echo "[cell] REHEARSAL: not a publishable cell"
        return 0
    fi
    if [[ "$smoke" == "1" ]]; then
        if awk -v b="$budget" 'BEGIN { exit !(b >= 0.99) }'; then
            echo "[cell] FATAL: a smoke cell needs a budget below \$0.99 (got $budget)" >&2
            return 1
        fi
        CELL_FLAKE_RESULT="SMOKE"
        echo "[cell] SMOKE: not a publishable cell"
        return 0
    fi
    local key record
    key="$(cd "$root" && scripts/flake-bar.sh --key)" || {
        echo "[cell] FATAL: could not compute the flake key" >&2
        return 1
    }
    record="$(cd "$root" && git rev-parse --path-format=absolute --git-common-dir)/sdb-gates/flake-$key.txt"
    if [[ ! -f "$record" ]]; then
        echo "[cell] FATAL: no flake record for $key ($record); run scripts/flake-bar.sh" >&2
        return 1
    fi
    if ! grep -q '^RESULT: PASS' "$record"; then
        echo "[cell] FATAL: the flake record for $key is not a PASS ($record)" >&2
        return 1
    fi
    CELL_FLAKE_KEY="$key"
    CELL_FLAKE_RESULT="PASS"
    return 0
}

self_test() {
    local dir repo key record out rc
    dir="$(mktemp -d /tmp/sdb-cell-self-test.XXXXXX)"
    trap "rm -rf '$dir'" EXIT
    repo="$dir/repo"
    mkdir -p "$repo/src" "$repo/scripts"
    cp scripts/flake-bar.sh scripts/graded-paths.txt scripts/long-test-lock.sh "$repo/scripts/"
    git -C "$repo" init -q
    git -C "$repo" config user.email cell@example.com
    git -C "$repo" config user.name cell
    echo one > "$repo/src/x.rs"
    echo lock > "$repo/Cargo.lock"
    git -C "$repo" add -A
    git -C "$repo" commit -qm one

    key="$(cd "$repo" && scripts/flake-bar.sh --key)"
    record="$(cd "$repo" && git rev-parse --path-format=absolute --git-common-dir)/sdb-gates/flake-$key.txt"
    mkdir -p "$(dirname "$record")"
    printf 'RESULT: PASS\n' > "$record"

    # 1) A dirty tracked file is refused even with a PASS record.
    echo two > "$repo/src/x.rs"
    if cell_gate "$repo" 0.5 0 >/dev/null 2>&1; then
        echo '[self-test] a dirty tracked tree was accepted' >&2; return 1
    fi
    git -C "$repo" checkout -- src/x.rs
    echo '[self-test] dirty tracked refused'

    # 2) No record is refused.
    rm -f "$record"
    if cell_gate "$repo" 0.5 0 >/dev/null 2>&1; then
        echo '[self-test] a missing record was accepted' >&2; return 1
    fi
    echo '[self-test] missing record refused'

    # 3) IN PROGRESS is refused.
    printf 'RESULT: IN PROGRESS\n' > "$record"
    if cell_gate "$repo" 0.5 0 >/dev/null 2>&1; then
        echo '[self-test] IN PROGRESS was accepted' >&2; return 1
    fi
    echo '[self-test] in-progress record refused'

    # 4) FAIL is refused.
    printf 'RESULT: FAIL (x)\n' > "$record"
    if cell_gate "$repo" 0.5 0 >/dev/null 2>&1; then
        echo '[self-test] FAIL was accepted' >&2; return 1
    fi
    echo '[self-test] failed record refused'

    # 5) PASS is allowed and reports the key.
    printf 'RESULT: PASS\n' > "$record"
    cell_gate "$repo" 0.5 0 || { echo '[self-test] PASS was refused' >&2; return 1; }
    [[ "$CELL_FLAKE_KEY" == "$key" && "$CELL_FLAKE_RESULT" == "PASS" ]] || {
        echo '[self-test] PASS did not report the key' >&2; return 1; }
    echo '[self-test] pass record allowed'

    # 6) SMOKE with no record is allowed and says so.
    rm -f "$record"
    out="$(cell_gate "$repo" 0.5 1 2>&1)" || {
        echo '[self-test] smoke without a record was refused' >&2; return 1; }
    grep -q 'SMOKE: not a publishable cell' <<<"$out" || {
        echo '[self-test] smoke did not print the SMOKE line' >&2; return 1; }
    echo '[self-test] smoke allowed'

    # 7) SMOKE at the official budget is refused.
    if cell_gate "$repo" 0.99 1 >/dev/null 2>&1; then
        echo '[self-test] smoke at $0.99 was accepted' >&2; return 1
    fi
    echo '[self-test] smoke at $0.99 refused'

    # 8) R54 F2: a rehearsal needs no record and says so, even at the
    # official budget. A dirty tree is still refused.
    rm -f "$record"
    if ! cell_gate "$repo" 0.99 0 1 > "$dir/rehearsal.out" 2>&1; then
        echo '[self-test] rehearsal without a record was refused' >&2; return 1
    fi
    out="$(cat "$dir/rehearsal.out")"
    grep -q 'REHEARSAL: not a publishable cell' <<<"$out" || {
        echo '[self-test] rehearsal did not print the REHEARSAL line' >&2; return 1; }
    [[ "$CELL_FLAKE_RESULT" == "REHEARSAL" ]] || {
        echo '[self-test] rehearsal did not report REHEARSAL' >&2; return 1; }
    echo '[self-test] rehearsal allowed'
    echo two > "$repo/src/x.rs"
    if cell_gate "$repo" 0.99 0 1 >/dev/null 2>&1; then
        echo '[self-test] a dirty rehearsal tree was accepted' >&2; return 1
    fi
    git -C "$repo" checkout -- src/x.rs
    echo '[self-test] dirty rehearsal refused'

    echo 'RESULT: PASS (cell self-test)'
    return 0
}

if [[ "${1:-}" == "--self-test" ]]; then
    self_test
    exit $?
fi
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
REHEARSAL="${SDB_CELL_REHEARSAL:-0}"
SMOKE="${SDB_CELL_SMOKE:-0}"
# R54 F2: a rehearsal needs no API key and no flake record. Workers 8-9
# stay reserved for tests (AGENTS.md); production lanes remain 0-7.
if [[ "$REHEARSAL" == "1" ]]; then
    if ! [[ "$WORKER" =~ ^[0-9]$ ]]; then
        echo "[cell] FATAL: SDB_CELL_WORKER must be 0-9 for a rehearsal (got '$WORKER')" >&2
        exit 1
    fi
else
    if ! [[ "$WORKER" =~ ^[0-7]$ ]]; then
        echo "[cell] FATAL: SDB_CELL_WORKER must be 0-7 (got '$WORKER')" >&2
        exit 1
    fi
fi
CELL_PORT=$((5480 + WORKER * 100))

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="$TARGET_DIR/release/subdollarbench"

# The repo's .env is the source of truth for the cell's API key: a stale
# OPENROUTER_API_KEY exported in the calling shell burned a cell on
# 2026-10-18 ("User not found." 401 at round 0, ~$0 spend, but a poisoned
# fault row). Prefer .env; fall back to the environment.
if [[ "$REHEARSAL" != "1" && -f "$ROOT/.env" ]]; then
    ENV_KEY="$(grep -m1 '^OPENROUTER_API_KEY=' "$ROOT/.env" | cut -d= -f2- | tr -d '\"' | tr -d "'")"
    if [[ -n "${ENV_KEY:-}" ]]; then
        if [[ -n "${OPENROUTER_API_KEY:-}" && "${OPENROUTER_API_KEY}" != "$ENV_KEY" ]]; then
            echo "[cell] note: the shell's OPENROUTER_API_KEY differs from .env; using .env (…${ENV_KEY: -4})"
        fi
        export OPENROUTER_API_KEY="$ENV_KEY"
    fi
fi

cell_gate "$ROOT" "$BUDGET" "$SMOKE" "$REHEARSAL" || exit 1

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

# R52 F5: the cell line names the rules the binary will grade with.
CELL_VERSION="$("$BIN" --version 2>/dev/null || true)"

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

echo "[cell] model=$MODEL budget=\$$BUDGET wall=${WALL}min harness=$HARNESS effort=$EFFORT phase=${PHASE:-full} chain=${CHAIN:-none} worker=$WORKER port=$CELL_PORT flake=${CELL_FLAKE_KEY:-none} bar=$CELL_FLAKE_RESULT version=${CELL_VERSION:-unknown} root=$ROOT"
PHASE_ARGS=()
if [[ -n "$PHASE" ]]; then PHASE_ARGS+=(--phase "$PHASE"); fi
if [[ -n "$CHAIN" ]]; then PHASE_ARGS+=(--chain "$CHAIN"); fi
if [[ "$REHEARSAL" == "1" ]]; then PHASE_ARGS+=(--rehearsal); fi
if [[ "$SMOKE" == "1" ]]; then PHASE_ARGS+=(--smoke); fi
if [[ "${CELL_DRY_RUN:-0}" == "1" ]]; then
    echo "[cell] dry run: would exec: campaign run --name sdb-sql --model $MODEL" \
        "--effort $EFFORT --harness $HARNESS --budget-usd $BUDGET --wall-min $WALL --timeout-min 120 --worker $WORKER ${PHASE_ARGS[*]} $*"
    exit 0
fi

RESULT_PATH="${SDB_CELL_RESULT:-}"
LOG="$(mktemp)"
# R54 F2: tests may use workers 8-9; production stays 0-7.
EXTRA_ENV=()
if [[ "$REHEARSAL" == "1" ]]; then EXTRA_ENV+=(SDB_CAMPAIGN_TEST_WORKER_MAX=99); fi
set +e
env SDB_HOME="$ROOT" "${EXTRA_ENV[@]}" "$BIN" campaign run \
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
    "phase": None,
    "chain_from": None,
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
    "rehearsal": False,
    "smoke": False,
    "error": "no rollup line",
}
m = re.search(r"^rollup: (.+)$", text, re.M)
if m:
    path = m.group(1).strip()
    try:
        r = json.load(open(path))
        d.update(
            campaign_id=os.path.basename(os.path.dirname(path)),
            # R48-4: the rollup says what ran; the request only says what was
            # asked for. grid.sh compares the two.
            phase=r.get("phase"),
            chain_from=r.get("chain_from"),
            cost_usd=float(r.get("total_cost_usd") or 0.0),
            points=float(r.get("points") or 0.0),
            over_budget=bool(r.get("over_budget")),
            over_wall=bool(r.get("over_wall")),
            harness_fault=r.get("harness_fault"),
            infra_error=any(s.get("status") == "fault" for s in r.get("stages", [])),
            wall_stop=bool(r.get("over_wall")),
            phase_complete=not r.get("harness_fault") and not r.get("over_wall"),
            rehearsal=bool(r.get("rehearsal")),
            smoke=bool(r.get("smoke")),
            error=None,
        )
    except Exception as e:  # noqa: BLE001 - report the parse failure in the file
        d["error"] = str(e)
json.dump(d, open(out, "w"))
PY
fi
rm -f "$LOG"
exit "$rc"
