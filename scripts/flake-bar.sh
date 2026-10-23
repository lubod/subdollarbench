#!/usr/bin/env bash
# R27 F5: resumable flake bar for the live SQL ladder proofs.
#
# Usage: scripts/flake-bar.sh [--passes K] [--only <test>] [--self-test]
#
# The record is <git-common-dir>/sdb-gates/<commit>.flake.txt. Each finished
# test run appends one line immediately, so an interrupted chunk is kept;
# the summary at the end is recomputed over every line so far.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 1

passes=1
only=""
self_test=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --passes)
            [[ $# -ge 2 ]] || { echo "[flake] --passes needs a value" >&2; exit 2; }
            passes="$2"; shift 2 ;;
        --only)
            [[ $# -ge 2 ]] || { echo "[flake] --only needs a test name" >&2; exit 2; }
            only="$2"; shift 2 ;;
        --self-test)
            self_test=1; shift ;;
        *)
            echo "[flake] unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ "$passes" =~ ^[0-9]+$ ]] && [[ "$passes" -gt 0 ]] || {
    echo "[flake] --passes must be a positive integer" >&2; exit 2
}

tests_from_source() {
    grep -oE 'fn test_pipeline_eval_only_sql_stage[0-9a-z_]*_live' src/pipeline.rs \
        | sed 's/^fn //' | sort -u
}

runs_for() {
    awk -v t="$1" '$1 == "TEST" && $2 == t { n++ } END { print n + 0 }' "$2"
}
passes_for() {
    awk -v t="$1" '$1 == "TEST" && $2 == t && $3 == "PASS" { n++ } END { print n + 0 }' "$2"
}
fails_for() {
    awk -v t="$1" '$1 == "TEST" && $2 == t && $3 == "FAIL" { n++ } END { print n + 0 }' "$2"
}

summary() {
    local record="$1"
    shift
    local tests=("$@")
    local any_fail=0 min_runs=20 all_full=1 line
    echo "--- summary ---"
    for t in "${tests[@]}"; do
        local runs passes fails
        runs=$(runs_for "$t" "$record")
        passes=$(passes_for "$t" "$record")
        fails=$(fails_for "$t" "$record")
        line="${passes}/${runs} ${t}"
        if [[ "$fails" -gt 0 ]]; then
            line+=" FAIL=${fails}"
            any_fail=1
        fi
        echo "$line"
        if [[ "$runs" -lt 20 ]]; then all_full=0; fi
        if [[ "$runs" -lt "$min_runs" ]]; then min_runs="$runs"; fi
    done
    if [[ "$any_fail" -eq 1 ]]; then
        local failed=()
        for t in "${tests[@]}"; do
            if [[ "$(fails_for "$t" "$record")" -gt 0 ]]; then failed+=("$t"); fi
        done
        echo "RESULT: FAIL (${failed[*]})"
        return 1
    fi
    if [[ "$all_full" -eq 1 ]]; then
        echo "RESULT: PASS"
    else
        echo "RESULT: IN PROGRESS (min runs ${min_runs}/20)"
    fi
    return 0
}

if [[ "$self_test" -eq 1 ]]; then
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' EXIT
    record="$dir/fixture.flake.txt"
    tests=(alpha beta)
    : > "$record"
    for _ in $(seq 1 20); do
        echo "TEST alpha PASS 1 2026-10-23T00:00:00Z" >> "$record"
        echo "TEST beta PASS 1 2026-10-23T00:00:00Z" >> "$record"
    done
    summary "$record" "${tests[@]}" > "$dir/pass.txt" || true
    grep -q 'RESULT: PASS' "$dir/pass.txt" || { cat "$dir/pass.txt"; exit 1; }

    record="$dir/progress.flake.txt"
    : > "$record"
    for _ in $(seq 1 3); do echo "TEST alpha PASS 1 2026-10-23T00:00:00Z" >> "$record"; done
    for _ in $(seq 1 2); do echo "TEST beta PASS 1 2026-10-23T00:00:00Z" >> "$record"; done
    summary "$record" "${tests[@]}" > "$dir/progress.txt" || true
    grep -q 'RESULT: IN PROGRESS (min runs 2/20)' "$dir/progress.txt" || {
        cat "$dir/progress.txt"; exit 1;
    }

    record="$dir/fail.flake.txt"
    : > "$record"
    echo "TEST alpha PASS 1 2026-10-23T00:00:00Z" >> "$record"
    echo "TEST beta FAIL 1 2026-10-23T00:00:00Z" >> "$record"
    summary "$record" "${tests[@]}" > "$dir/fail.txt" || true
    grep -q 'RESULT: FAIL (beta)' "$dir/fail.txt" || { cat "$dir/fail.txt"; exit 1; }
    echo "RESULT: PASS (self-test)"
    exit 0
fi

# A dirty tree makes a commit-keyed record meaningless, as with gate.sh.
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
    echo "[flake] FATAL: uncommitted changes; commit first" >&2
    exit 1
fi

mapfile -t all_tests < <(tests_from_source)
[[ ${#all_tests[@]} -gt 0 ]] || { echo "[flake] no live SQL proofs found" >&2; exit 1; }
if [[ -n "$only" ]]; then
    match=0
    for t in "${all_tests[@]}"; do [[ "$t" == "$only" ]] && match=1; done
    [[ "$match" -eq 1 ]] || { echo "[flake] unknown test: $only" >&2; exit 1; }
    all_tests=("$only")
fi

commit="$(git rev-parse --short=12 HEAD)"
dir="$(git rev-parse --path-format=absolute --git-common-dir)/sdb-gates"
mkdir -p "$dir"
record="$dir/$commit.flake.txt"
if [[ ! -f "$record" ]]; then
    {
        echo "commit:  $(git rev-parse HEAD)"
        echo "branch:  $(git rev-parse --abbrev-ref HEAD)"
        echo "subject: $(git log -1 --format=%s)"
        echo "started: $(date -u +%FT%TZ)"
    } > "$record"
else
    echo "resumed: $(date -u +%FT%TZ) (passes requested: $passes)" >> "$record"
fi
log="$(mktemp)"
trap 'rm -f "$log"' EXIT

failed=0
for _ in $(seq 1 "$passes"); do
    for t in "${all_tests[@]}"; do
        runs=$(runs_for "$t" "$record")
        if [[ "$runs" -ge 20 ]]; then
            continue
        fi
        t0=$SECONDS
        if nice -n 10 cargo test --lib "pipeline::tests::$t" -- --exact --nocapture > "$log" 2>&1; then
            echo "TEST $t PASS $((SECONDS - t0)) $(date -u +%FT%TZ)" >> "$record"
        else
            echo "TEST $t FAIL $((SECONDS - t0)) $(date -u +%FT%TZ)" >> "$record"
            echo "OUTPUT-BEGIN" >> "$record"
            tail -n 40 "$log" | sed 's/^/  /' >> "$record"
            echo "OUTPUT-END" >> "$record"
            failed=1
            break 2
        fi
    done
done

{
    echo "finished: $(date -u +%FT%TZ)"
    summary "$record" "${all_tests[@]}" || true
} >> "$record"
tail -n 80 "$record"
echo "[flake] record: $record"
[[ "$failed" -eq 0 ]] || exit 1
