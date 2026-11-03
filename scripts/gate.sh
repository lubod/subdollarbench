#!/usr/bin/env bash
# Round gate: fmt, clippy and the niced test suite on a clean commit.
# Writes <git-common-dir>/sdb-gates/<commit>.txt (full),
# <commit>.fast.txt (--fast) or <commit>.ready.txt (--ready), shared by all
# worktrees and never committed, so the reviewer can read the result for
# that exact commit instead of rerunning it. See AGENTS.md "Gates" for which
# mode a `ready` commit needs.
#
# Usage: scripts/gate.sh [--fast|--ready] [--dirty] [--self-test]
#   --fast   skip the serialized live SQL ladder proofs (the tests that hold
#            SQL_PERF_LIVE: ~28 of the full gate's ~33 min). Everything
#            else, including the other Docker tests, still runs.
#   --ready  R61 F6: the fast set, then the core proofs from
#            scripts/ready-proofs.txt and the proofs scripts/proof-map.txt
#            selects for this branch's graded changes.
#   --dirty  allow uncommitted changes; the record is then marked DIRTY.
#   --self-test  check the scope rule (fast / ready / full) on a fixture.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

# R61: one tests.rs rule (graded-filter.sh) and one path -> proofs map
# (proof-map.sh), shared with flake-bar.sh.
. scripts/graded-filter.sh
. scripts/proof-map.sh

fast=0
ready=0
allow_dirty=0
self_test=0
for arg in "$@"; do
    case "$arg" in
        --fast) fast=1 ;;
        --ready) ready=1 ;;
        --dirty) allow_dirty=1 ;;
        --self-test) self_test=1 ;;
        *) echo "[gate] FATAL: unknown argument $arg" >&2; exit 1 ;;
    esac
done
if [[ $fast -eq 1 && $ready -eq 1 ]]; then
    echo "[gate] FATAL: --fast and --ready are different modes" >&2
    exit 1
fi

# R62 F3: a ready selection that is the whole skip list is the full gate.
ready_is_full() {
    local selected="${1:-0}" live="${2:-0}"
    [[ "$live" -gt 0 && "$selected" -eq "$live" ]]
}

scope_self_test() {
    local dir repo out
    dir="$(mktemp -d /tmp/sdb-gate-self-test.XXXXXX)"
    trap "rm -rf '$dir'" EXIT
    repo="$dir/repo"

    # R61 F6: every core proof is in the skip list the fast set uses.
    local proof
    while IFS= read -r proof; do
        [[ -z "$proof" ]] && continue
        scripts/live-proofs.sh | grep -qx "$proof" || {
            echo "[self-test] ready proof $proof is not in live-proofs.sh" >&2
            return 1
        }
    done < <(core_proofs)
    echo "[self-test] every core proof is in live-proofs.sh"

    mkdir -p "$repo/scripts" "$repo/src/pipeline" "$repo/src/report" "$repo/docs"
    cp scripts/gate.sh scripts/graded-filter.sh scripts/proof-map.sh \
        scripts/bar-proofs.sh scripts/graded-paths.txt scripts/proof-map.txt \
        scripts/ready-proofs.txt scripts/live-proofs.sh "$repo/scripts/"
    printf 'pub const RULES_VERSION: &str = "2026-10-19.45";\n' >"$repo/src/report/rules.rs"
    : >"$repo/src/pipeline.rs"
    : >"$repo/src/pipeline/tests.rs"
    printf '# docs\n' >"$repo/docs/readme.md"
    git -C "$repo" init -q -b master
    git -C "$repo" config user.email gate@example.com
    git -C "$repo" config user.name gate
    git -C "$repo" add -A
    git -C "$repo" commit -qm base
    git -C "$repo" checkout -qb round

    scope_of() {
        (cd "$repo" && SDB_GATE_SCOPE_ONLY=1 scripts/gate.sh --ready 2>&1)
    }

    printf '# docs changed\n' >"$repo/docs/readme.md"
    git -C "$repo" add -A
    git -C "$repo" commit -qm docs
    out="$(scope_of)"
    grep -q 'scope:.*fast' <<<"$out" || {
        echo "[self-test] a docs-only change did not scope to fast" >&2
        printf '%s\n' "$out" >&2
        return 1
    }
    echo "[self-test] docs-only scopes to fast"

    mkdir -p "$repo/src/verifier/sql"
    printf 'pub fn x() {}\n' >"$repo/src/verifier/sql/x.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm graded
    out="$(scope_of)"
    grep -q 'scope:.*ready' <<<"$out" || {
        echo "[self-test] a graded change did not scope to ready" >&2
        printf '%s\n' "$out" >&2
        return 1
    }
    echo "[self-test] a graded change scopes to ready"

    printf 'pub const RULES_VERSION: &str = "2026-10-19.46";\n' >"$repo/src/report/rules.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm bump
    out="$(scope_of)"
    grep -q 'scope:.*full' <<<"$out" || {
        echo "[self-test] a RULES_VERSION change did not scope to full" >&2
        printf '%s\n' "$out" >&2
        return 1
    }
    echo "[self-test] a RULES_VERSION change scopes to full"

    # R62 F3: the full selection takes the full path, a partial one keeps
    # the fast set plus its proofs.
    ready_is_full 19 19 || {
        echo "[self-test] a full ready selection did not take the full path" >&2
        return 1
    }
    if ready_is_full 4 19; then
        echo "[self-test] a partial ready selection took the full path" >&2
        return 1
    fi
    echo "[self-test] full and partial ready selections pick the right path"

    echo "RESULT: PASS (gate scope self-test)"
}

if [[ $self_test -eq 1 ]]; then
    scope_self_test
    exit $?
fi

dirty=0
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
    if [[ $allow_dirty != 1 ]]; then
        echo "[gate] FATAL: uncommitted changes; commit first (or pass --dirty)" >&2
        exit 1
    fi
    dirty=1
fi

# Paths whose change needs more than the fast gate on a `ready` commit:
# grading, scoring, cost, what the candidate sees, and the harness that
# runs it.
mapfile -t graded_paths < <(graded_path_entries)
base="$(git merge-base HEAD master 2>/dev/null || true)"
touched=""
if [[ -n "$base" ]]; then
    touched="$(git diff --name-only "$base" HEAD -- "${graded_paths[@]}" \
        | graded_filter \
        | head -5 | tr '\n' ' ')"
fi
rules_bumped=0
if [[ -n "$base" ]]; then
    rules_diff="$(git diff -U0 "$base" HEAD -- src/report/rules.rs 2>/dev/null || true)"
    if grep -qE '^[+-][^+-].*const RULES_VERSION' <<<"$rules_diff"; then
        rules_bumped=1
    fi
fi
if [[ $rules_bumped -eq 1 ]]; then
    scope="full (RULES_VERSION changed: a bump)"
elif [[ -n "$touched" ]]; then
    scope="ready (--ready: the core proofs and the changed paths' proofs)"
else
    scope="fast (no graded path changed since master)"
fi

# The self-test above reads the scope without running anything, and the
# long-test checks below need the tools a fixture does not copy.
if [[ "${SDB_GATE_SCOPE_ONLY:-0}" == "1" ]]; then
    echo "scope:   $scope"
    exit 0
fi
if [[ $ready -eq 1 && $rules_bumped -eq 1 ]]; then
    echo "[gate] FATAL: RULES_VERSION changed; --ready cannot substitute for the full gate" >&2
    exit 1
fi

# R62 F3: when --ready selects every proof the fast set skipped, the full
# gate's single cargo test is cheaper. Decide before the record header so
# the mode line says which path ran.
ready_full=0
if [[ $ready -eq 1 ]]; then
    mapfile -t live < <(scripts/live-proofs.sh)
    mapfile -t ready_list < <(select_ready_proofs "$PWD" "$base")
    if ready_is_full "${#ready_list[@]}" "${#live[@]}"; then
        ready_full=1
    fi
fi

# R56 F6: a full gate builds and runs Docker-heavy tests; a --ready gate
# runs the serialized live proofs too. Both refuse next to a live cell.
# `--fast` stays allowed (AGENTS.md).
long=0
[[ $fast -eq 0 ]] && long=1
beside_override=""
if [[ $long -eq 1 ]]; then
    scripts/beside-cell.sh "gate ($([[ $ready -eq 1 ]] && echo ready || echo full))" || exit 1
    if [[ "${SDB_BESIDE_CELL_ONLY:-0}" == "1" ]]; then
        exit 0
    fi
    if [[ "${SDB_ALLOW_BESIDE_CELL:-0}" == "1" ]]; then
        beside_override="SDB_ALLOW_BESIDE_CELL=1"
    fi
fi

# R48-5: a full or ready gate is a long test. Hold the shared long-test
# lock for the whole run, so the grid's preflight refuses to start a phase
# next to it. `--fast` and plain unit tests stay allowed while a cell runs.
if [[ $long -eq 1 ]]; then
    . scripts/long-test-lock.sh
    hold_long_test_lock
fi

commit="$(git rev-parse --short=12 HEAD)"
dir="$(git rev-parse --path-format=absolute --git-common-dir)/sdb-gates"
mkdir -p "$dir"
suffix=""
[[ $fast == 1 ]] && suffix=".fast"
[[ $ready == 1 ]] && suffix=".ready"
[[ $dirty == 1 ]] && suffix="$suffix-DIRTY"
out="$dir/$commit$suffix.txt"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT

mode="full"
[[ $fast == 1 ]] && mode="fast"
[[ $ready == 1 ]] && mode="ready (fast set + core and mapped proofs)"
[[ $ready == 1 && $ready_full == 1 ]] && mode="full (ready selected every proof)"
{
    echo "commit:  $(git rev-parse HEAD)"
    echo "branch:  $(git rev-parse --abbrev-ref HEAD)"
    echo "subject: $(git log -1 --format=%s)"
    echo "tree:    $([[ $dirty == 1 ]] && echo DIRTY || echo clean)"
    echo "mode:    $mode"
    echo "scope:   $scope"
    echo "started: $(date -u +%FT%TZ)"
    echo "rustc:   $(rustc --version)"
} >"$out"
if [[ -n "$beside_override" ]]; then
    echo "beside-cell: override $beside_override" >>"$out"
fi

fail=0
step() {
    local name="$1"
    shift
    local t0=$SECONDS
    if nice -n 10 "$@" >"$log" 2>&1; then
        echo "PASS  $name ($((SECONDS - t0))s)" >>"$out"
    else
        echo "FAIL  $name ($((SECONDS - t0))s)" >>"$out"
        tail -n 40 "$log" | sed 's/^/      /' >>"$out"
        fail=1
    fi
    # Keep the test summaries, which are the evidence a reviewer reads.
    grep -E '^test result:|^failures:|^    [a-z_:]+$' "$log" | sed 's/^/      /' >>"$out" || true
}

fast_set() {
    # The tests to skip are read from the source, so a new SQL_PERF_LIVE
    # proof is skipped without editing this script.
    local skips=()
    while read -r name; do
        skips+=(--skip "$name")
    done < <(scripts/live-proofs.sh)
    echo "skipped: $(( ${#skips[@]} / 2 )) tests (the SQL_PERF_LIVE ladder proofs)" >>"$out"
    step "cargo test (fast: no serialized SQL ladder proofs)" cargo test -- "${skips[@]}"
}

# R61 F6: one proof, with the reason the ready set chose it. A filter that
# matches nothing would exit 0 with "0 passed", so require exactly one.
run_ready_proof() {
    local proof="$1" reason="$2" t0=$SECONDS
    local target="--lib" filter="pipeline::tests::$proof"
    # R61 F6: r54 is a bin test (tests::r54_...), the bar proofs are lib.
    if grep -qE "fn[[:space:]]+$proof\b" src/main.rs 2>/dev/null; then
        target="--bins"
        filter="tests::$proof"
    fi
    if nice -n 10 cargo test "$target" "$filter" -- --exact --nocapture \
        >"$log" 2>&1 \
        && grep -qE '^test result: ok\. 1 passed' "$log"; then
        echo "PASS  $proof ($((SECONDS - t0))s) [$reason]" >>"$out"
    else
        echo "FAIL  $proof ($((SECONDS - t0))s) [$reason]" >>"$out"
        tail -n 40 "$log" | sed 's/^/      /' >>"$out"
        fail=1
    fi
}

step "cargo fmt --check" cargo fmt --check
step "cargo clippy --all-targets -D warnings" cargo clippy --all-targets -- -D warnings

# R54 F5: citation check on the round files this branch changes. A warning
# only: a drifted cite must be seen, but it does not block the gate.
if [[ -n "$base" ]]; then
    while IFS= read -r round_file; do
        [[ -f "$round_file" ]] || continue
        if ! cite_out="$(scripts/check-cites.sh "$round_file" 2>&1)"; then
            {
                echo "WARN  citation check: $round_file"
                printf '%s\n' "$cite_out" | sed 's/^/      /'
            } >>"$out"
        fi
    done < <(git diff --name-only "$base" HEAD -- 'docs/rounds/*.md' 2>/dev/null || true)
fi

if [[ $ready -eq 1 ]]; then
    if [[ $ready_full -eq 1 ]]; then
        echo "proofs:  all ${#live[@]} (ready selected every proof; full gate)" >>"$out"
        step "cargo test (full, unfiltered)" cargo test
    else
        fast_set
        echo "proofs:  ${#ready_list[@]} selected by scripts/ready-proofs.txt and scripts/proof-map.txt" >>"$out"
        while IFS=$'\t' read -r proof reason; do
            [[ -z "$proof" ]] && continue
            if ! printf '%s\n' "${live[@]}" | grep -qx "$proof"; then
                echo "FAIL  $proof: not in scripts/live-proofs.sh; --ready must not run a proof outside it" >>"$out"
                fail=1
                continue
            fi
            run_ready_proof "$proof" "$reason"
        done < <(printf '%s\n' "${ready_list[@]}")
    fi
elif [[ $fast -eq 1 ]]; then
    fast_set
else
    step "cargo test (full, unfiltered)" cargo test
fi

echo "finished: $(date -u +%FT%TZ)" >>"$out"
if [[ $fail == 0 ]]; then echo "RESULT: PASS" >>"$out"; else echo "RESULT: FAIL" >>"$out"; fi

cat "$out"
echo "[gate] record: $out"
exit $fail
