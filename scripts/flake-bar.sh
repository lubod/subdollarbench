#!/usr/bin/env bash
# R27 F5: resumable flake bar for the live SQL ladder proofs.
#
# Usage: scripts/flake-bar.sh [--passes K] [--only <test>] [--load L] [--self-test]
#
# The record is <git-common-dir>/sdb-gates/flake-<key>.txt. Each finished
# test run appends one line immediately, so an interrupted chunk is kept;
# the summary at the end is recomputed over every line so far.
set -euo pipefail
BAR_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$(git rev-parse --show-toplevel)" || exit 1

# R27-3/R28 F2: the record is keyed by graded content, not by commit, so
# a docs-only commit keeps the same resumable record. The paths are the
# same list scripts/gate.sh reads.
. "$BAR_ROOT/scripts/graded-filter.sh"
. "$BAR_ROOT/scripts/bar-proofs.sh"
mapfile -t graded_paths < <(graded_path_entries)
flake_key() {
    local repo="$1" tree="$2"
    ( cd "$repo" && git ls-tree -r "$tree" -- "${graded_paths[@]}" \
        | graded_filter \
        | sha256sum | cut -c1-12 )
}

passes=1
only=""
self_test=0
key_only=0
list_only=0
load=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --passes)
            [[ $# -ge 2 ]] || { echo "[flake] --passes needs a value" >&2; exit 2; }
            passes="$2"; shift 2 ;;
        --only)
            [[ $# -ge 2 ]] || { echo "[flake] --only needs a test name" >&2; exit 2; }
            only="$2"; shift 2 ;;
        --key)
            key_only=1; shift ;;
        --self-test)
            self_test=1; shift ;;
        --list)
            list_only=1; shift ;;
        --load)
            [[ $# -ge 2 ]] || { echo "[flake] --load needs a value" >&2; exit 2; }
            load="$2"; shift 2 ;;
        *)
            echo "[flake] unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ "$passes" =~ ^[0-9]+$ ]] && [[ "$passes" -gt 0 ]] || {
    echo "[flake] --passes must be a positive integer" >&2; exit 2
}
[[ "$load" =~ ^[0-9]+$ ]] || {
    echo "[flake] --load must be a non-negative integer" >&2; exit 2
}

# R52 F4 (round 40 F2): the one key computation; sql-cell.sh and grid.sh
# call this instead of copying the graded-path hash.
if [[ "$key_only" -eq 1 ]]; then
    flake_key "$(git rev-parse --show-toplevel)" HEAD
    exit 0
fi

# R61 F7: the list lives in scripts/bar-proofs.sh, shared with the ready
# gate's proof map so the two cannot drift.
tests_from_source() {
    bar_proofs "$(git rev-parse --show-toplevel)"
}

# R61 F7: the ready gate's self-test compares its map against this list.
# `--list` is read-only, like --key, so it stays allowed beside a cell.
if [[ "$list_only" -eq 1 ]]; then
    tests_from_source
    exit 0
fi

# R56 F6: the bar runs the live proofs; it refuses next to a live cell.
# `--key` above is only a hash and stays allowed.
"$BAR_ROOT/scripts/beside-cell.sh" "flake-bar" || exit 1
if [[ "${SDB_BESIDE_CELL_ONLY:-0}" == "1" ]]; then
    exit 0
fi

runs_for() {
    awk -v t="$1" '$1 == "TEST" && $2 == t && $3 != "RERUN" { n++ } END { print n + 0 }' "$2"
}
passes_for() {
    awk -v t="$1" '$1 == "TEST" && $2 == t && $3 == "PASS" { n++ } END { print n + 0 }' "$2"
}
fails_for() {
    awk -v t="$1" '$1 == "TEST" && $2 == t && $3 == "FAIL" { n++ } END { print n + 0 }' "$2"
}
cured_for() {
    awk -v t="$1" '$1 == "TEST" && $2 == t && $3 == "RERUN" && $4 == "PASS" { n++ } END { print n + 0 }' "$2"
}
rerun_fail_for() {
    awk -v t="$1" '$1 == "TEST" && $2 == t && $3 == "RERUN" && $4 == "FAIL" { n++ } END { print n + 0 }' "$2"
}

summary() {
    local record="$1"
    shift
    local tests=("$@")
    local any_fail=0 min_runs=20 all_full=1 line total_cured=0 cured_names=()
    echo "--- summary ---"
    for t in "${tests[@]}"; do
        local runs passes fails cured rf
        runs=$(runs_for "$t" "$record")
        passes=$(passes_for "$t" "$record")
        fails=$(fails_for "$t" "$record")
        cured=$(cured_for "$t" "$record")
        rf=$(rerun_fail_for "$t" "$record")
        line="${passes}/${runs} ${t} cured=${cured}"
        if [[ "$fails" -gt 0 ]]; then line+=" FAIL=${fails}"; fi
        if [[ "$rf" -gt 0 || "$cured" -gt 1 ]]; then line+=" RERUN=FAIL"; any_fail=1; fi
        if [[ $((fails - cured - rf)) -gt 0 ]]; then any_fail=1; fi
        total_cured=$((total_cured + cured))
        [[ "$cured" -gt 0 ]] && cured_names+=("$t")
        echo "$line"
        if [[ "$runs" -lt 20 ]]; then all_full=0; fi
        if [[ "$runs" -lt "$min_runs" ]]; then min_runs="$runs"; fi
    done
    if [[ "$total_cured" -gt 2 ]]; then any_fail=1; fi
    if [[ "$any_fail" -eq 1 ]]; then
        local failed=()
        for t in "${tests[@]}"; do
            if [[ "$(rerun_fail_for "$t" "$record")" -gt 0 \
                || "$(cured_for "$t" "$record")" -gt 1 \
                || $(( $(fails_for "$t" "$record") - $(cured_for "$t" "$record") - $(rerun_fail_for "$t" "$record") )) -gt 0 ]]; then
                failed+=("$t")
            fi
        done
        echo "RESULT: FAIL (${failed[*]:-too many cured})"
        return 1
    fi
    if [[ "$all_full" -eq 1 ]]; then
        if [[ "$total_cured" -gt 0 ]]; then
            echo "RESULT: PASS (cured: $(IFS=,; echo "${cured_names[*]}" | sed 's/,/, /g'))"
        else
            echo "RESULT: PASS"
        fi
    else
        echo "RESULT: IN PROGRESS (min runs ${min_runs}/20)"
    fi
    return 0
}

run_one() {
    nice -n 10 cargo test --lib "pipeline::tests::$1" -- --exact --nocapture
}

# R28 F3: one failure is recorded and the chunk continues; the caller's
# summary always covers the full test list.
run_chunk() {
    local record="$1" passes="$2"
    shift 2
    local tests=("$@")
    for _ in $(seq 1 "$passes"); do
        for t in "${tests[@]}"; do
            if [[ "$(runs_for "$t" "$record")" -ge 20 ]]; then
                continue
            fi
            local t0=$SECONDS
            if run_one "$t" > "$log" 2>&1; then
                echo "TEST $t PASS $((SECONDS - t0)) $(date -u +%FT%TZ)" >> "$record"
            else
                local utc base full_log
                utc="$(date -u +%Y%m%dT%H%M%SZ)"
                base="${record%.txt}"
                full_log="${base}-${t}-${utc}.log"
                cp "$log" "$full_log"
                echo "TEST $t FAIL $((SECONDS - t0)) $(date -u +%FT%TZ) log=$full_log" >> "$record"
                echo "OUTPUT-BEGIN" >> "$record"
                tail -n 40 "$log" | sed 's/^/  /' >> "$record"
                echo "OUTPUT-END" >> "$record"
                local r0=$SECONDS
                if run_one "$t" > "$log" 2>&1; then
                    echo "TEST $t RERUN PASS $((SECONDS - r0)) $(date -u +%FT%TZ)" >> "$record"
                else
                    cp "$log" "${base}-${t}-rerun-${utc}.log"
                    echo "TEST $t RERUN FAIL $((SECONDS - r0)) $(date -u +%FT%TZ) log=${base}-${t}-rerun-${utc}.log" >> "$record"
                    failed=1
                fi
            fi
        done
    done
}

if [[ "$self_test" -eq 1 ]]; then
    . "$BAR_ROOT/scripts/long-test-lock.sh"
    # R48 F3: the load helper's own self-test runs with the bar's.
    scripts/flake-load.sh self-test
    # R45-1: the bar is the union of every ladder proof and the r45 live
    # proofs; it must not shrink to the SQL_PERF_LIVE skip list.
    mapfile -t bar_tests < <(tests_from_source)
    while IFS= read -r t; do
        printf '%s\n' "${bar_tests[@]}" | grep -qx "$t" \
            || { echo "[flake] bar list missing $t"; exit 1; }
    done < <(grep -ohE 'fn test_pipeline_eval_only_sql_[a-z0-9_]*_live' src/pipeline.rs src/pipeline/tests.rs \
        | sed 's/^fn //' | sort -u)
    for t in "${bar_tests[@]}"; do
        [[ "$t" =~ ^[a-z0-9_]+$ ]] || { echo "[flake] bad bar name: $t"; exit 1; }
    done
    for t in r45_f1_reference_scores_one r45_f5_perfscore_cli \
        r49_f2_unsynced_candidate_floors_live; do
        printf '%s\n' "${bar_tests[@]}" | grep -qx "$t" \
            || { echo "[flake] bar list missing $t"; exit 1; }
    done
    [[ ${#bar_tests[@]} -eq 28 ]] \
        || { echo "[flake] bar list has ${#bar_tests[@]} tests, expected 28"; exit 1; }

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

    # R28 F3: --only's summary still covers all proofs.
    record="$dir/only.flake.txt"
    : > "$record"
    for _ in $(seq 1 3); do echo "TEST alpha PASS 1 2026-10-23T00:00:00Z" >> "$record"; done
    summary "$record" "${tests[@]}" > "$dir/only.txt" || true
    grep -q 'RESULT: IN PROGRESS' "$dir/only.txt" || { cat "$dir/only.txt"; exit 1; }
    if grep -q 'RESULT: PASS' "$dir/only.txt"; then
        echo "--only summary must not claim PASS while beta has no runs"
        exit 1
    fi

    # R28 F3: a failing proof is recorded and the rest of the chunk runs.
    log="$dir/fake.log"
    failed=0
    run_one() { [[ "$1" != "fail" ]]; }
    record="$dir/chunk.flake.txt"
    : > "$record"
    run_chunk "$record" 1 alpha fail beta
    grep -q 'TEST alpha PASS' "$record" || { cat "$record"; exit 1; }
    grep -q 'TEST fail FAIL' "$record" || { cat "$record"; exit 1; }
    grep -q 'TEST beta PASS' "$record" || { cat "$record"; exit 1; }
    grep -q 'TEST fail FAIL .*log=' "$record" || { cat "$record"; exit 1; }
    ls "$dir"/chunk.flake-fail-*.log >/dev/null 2>&1 || {
        echo "the failed run did not leave its full output file"
        exit 1
    }
    [[ "$failed" -eq 1 ]] || { echo "the fixture failure did not set the exit state"; exit 1; }
    summary "$record" alpha fail beta > "$dir/chunk-summary.txt" || true
    for t in alpha fail beta; do
        grep -q " $t" "$dir/chunk-summary.txt" || { cat "$dir/chunk-summary.txt"; exit 1; }
    done

    # R28 F2: Cargo.lock is in the shared list and moves the content key.
    if ! printf '%s\n' "${graded_paths[@]}" | grep -qx 'Cargo.lock'; then
        echo "graded-paths.txt is missing Cargo.lock"
        exit 1
    fi

    # R27-3: a docs-only commit keeps the content key; a graded edit moves it.
    repo="$dir/repo"
    mkdir -p "$repo/src" "$repo/docs" "$repo/scripts"
    cp scripts/graded-paths.txt "$repo/scripts/graded-paths.txt"
    git -C "$repo" init -q
    git -C "$repo" config user.email flake@example.com
    git -C "$repo" config user.name flake
    echo "pub const X: u32 = 1;" > "$repo/src/scoring.rs"
    echo "version = 1" > "$repo/Cargo.lock"
    echo "one" > "$repo/docs/readme.md"
    git -C "$repo" add -A
    git -C "$repo" commit -qm one
    key1=$(flake_key "$repo" HEAD)
    # R52 F4: --key prints the same key as the record header.
    bar_abs="$PWD/scripts/flake-bar.sh"
    got_key="$(cd "$repo" && "$bar_abs" --key)"
    [[ "$got_key" == "$key1" ]] || {
        echo "--key printed $got_key, expected $key1"; exit 1;
    }
    echo "two" > "$repo/docs/readme.md"
    git -C "$repo" add -A
    git -C "$repo" commit -qm two
    key2=$(flake_key "$repo" HEAD)
    [[ "$key1" == "$key2" ]] || { echo "doc-only commit changed the key"; exit 1; }
    echo "version = 2" > "$repo/Cargo.lock"
    git -C "$repo" add -A
    git -C "$repo" commit -qm three
    key3=$(flake_key "$repo" HEAD)
    [[ "$key1" != "$key3" ]] || { echo "Cargo.lock edit kept the key"; exit 1; }
    echo "pub const X: u32 = 2;" > "$repo/src/scoring.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm four
    key4=$(flake_key "$repo" HEAD)
    [[ "$key3" != "$key4" ]] || { echo "graded edit kept the key"; exit 1; }
    mkdir -p "$repo/src/verifier/x"
    echo "test" > "$repo/src/verifier/x/tests.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm tests-rs
    key5=$(flake_key "$repo" HEAD)
    [[ "$key4" == "$key5" ]] || { echo "tests.rs moved the key"; exit 1; }
    mkdir -p "$repo/src/pipeline"
    echo "proof" > "$repo/src/pipeline/tests.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm pipeline-tests
    key6=$(flake_key "$repo" HEAD)
    [[ "$key5" != "$key6" ]] || { echo "pipeline/tests.rs kept the key"; exit 1; }
    echo "pub fn y() {}" > "$repo/src/verifier/x.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm verifier-edit
    key7=$(flake_key "$repo" HEAD)
    [[ "$key6" != "$key7" ]] || { echo "production edit kept the key"; exit 1; }
    # R48-5: the bar's lock holder blocks the probe, and releases on exit.
    lock="$dir/long-test.lock"
    (
        . scripts/long-test-lock.sh
        SDB_LONG_TEST_LOCK="$lock" hold_long_test_lock
        sleep 5
    ) &
    holder=$!
    sleep 0.2
    if ! SDB_LONG_TEST_LOCK="$lock" long_test_lock_busy; then
        echo "a running bar's long-test lock was not seen"
        kill "$holder" 2>/dev/null || true
        exit 1
    fi
    kill "$holder" 2>/dev/null || true
    wait "$holder" 2>/dev/null || true
    if SDB_LONG_TEST_LOCK="$lock" long_test_lock_busy; then
        echo "the released long-test lock is still held"
        exit 1
    fi

    echo "RESULT: PASS (self-test, key=$key1)"
    exit 0
fi

# A dirty tree makes a commit-keyed record meaningless, as with gate.sh.
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
    echo "[flake] FATAL: uncommitted changes; commit first" >&2
    exit 1
fi

# R48-5: bar runs are long tests; hold the shared lock for the whole run.
. "$BAR_ROOT/scripts/long-test-lock.sh"
hold_long_test_lock

mapfile -t all_tests < <(tests_from_source)
[[ ${#all_tests[@]} -gt 0 ]] || { echo "[flake] no live SQL proofs found" >&2; exit 1; }
summary_tests=("${all_tests[@]}")
run_tests=("${all_tests[@]}")
if [[ -n "$only" ]]; then
    match=0
    for t in "${all_tests[@]}"; do [[ "$t" == "$only" ]] && match=1; done
    [[ "$match" -eq 1 ]] || { echo "[flake] unknown test: $only" >&2; exit 1; }
    run_tests=("$only")
fi

commit="$(git rev-parse --short=12 HEAD)"
key="$(flake_key "$(git rev-parse --show-toplevel)" HEAD)"
dir="$(git rev-parse --path-format=absolute --git-common-dir)/sdb-gates"
mkdir -p "$dir"
load_suffix=""
[[ "$load" -gt 0 ]] && load_suffix="-load$load"
record="$dir/flake-$key$load_suffix.txt"
if [[ ! -f "$record" ]]; then
    {
        echo "commit:  $(git rev-parse HEAD)"
        echo "key:     flake-$key$load_suffix"
        echo "load:    $load"
        echo "branch:  $(git rev-parse --abbrev-ref HEAD)"
        echo "subject: $(git log -1 --format=%s)"
        echo "started: $(date -u +%FT%TZ)"
    } > "$record"
else
    echo "resumed: $(date -u +%FT%TZ) commit $commit (passes requested: $passes)" >> "$record"
fi
log="$(mktemp)"
load_key=""
cleanup() {
    if [[ -n "$load_key" ]]; then
        scripts/flake-load.sh stop "$load_key" || true
    fi
    rm -f "$log"
}
trap cleanup EXIT INT TERM
if [[ "$load" -gt 0 ]]; then
    load_key="bar-$key-$$"
    scripts/flake-load.sh start "$load" "$load_key"
fi

failed=0
run_chunk "$record" "$passes" "${run_tests[@]}"

{
    echo "finished: $(date -u +%FT%TZ)"
    summary "$record" "${summary_tests[@]}" || true
} >> "$record"
tail -n 80 "$record"
echo "[flake] record: $record"
[[ "$failed" -eq 0 ]] || exit 1
