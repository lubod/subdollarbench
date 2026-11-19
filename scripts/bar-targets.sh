#!/usr/bin/env bash
# R91 B2: the bar's per-proof run targets.
#
# A proof runs 20 times when the change since the last passing bar puts it
# at risk, and 5 times otherwise. "At risk" is the same selection the
# `--ready` gate makes: the core proofs plus every proof `scripts/proof-map.txt`
# maps to a changed grader path (or a new proof, or an unattributed hunk in
# `src/pipeline/tests.rs`). A change to a path every proof depends on, no
# previous passing bar, or a base that is not an ancestor of HEAD makes
# every proof 20.
#
# The bar pins the targets as `TARGET <proof> <runs> <reason>` lines in its
# record at start; a resumed bar reads them back, never recomputing from a
# moved HEAD. `bar_targets_for` is the one computation; the seams
# (`bar_base_commit`, `bar_ancestor`, `bar_changed_paths`,
# `bar_selected_reasons`) are overridden by the self-test.
#
# Source this file; do not execute it except `--self-test`.
set -uo pipefail

BAR_TARGETS_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if ! declare -F changed_graded_paths >/dev/null 2>&1; then
    . "$BAR_TARGETS_ROOT/scripts/proof-map.sh"
fi
if ! declare -F bar_proofs >/dev/null 2>&1; then
    . "$BAR_TARGETS_ROOT/scripts/bar-proofs.sh"
fi

bar_gates_dir() {
    if [[ -n "${SDB_BAR_GATES_DIR:-}" ]]; then
        printf '%s' "$SDB_BAR_GATES_DIR"
    else
        printf '%s/sdb-gates' \
            "$(git rev-parse --path-format=absolute --git-common-dir)"
    fi
}

# Spec question 1: r45_f1's one run is about 6 minutes; its five runs are
# the production-shaped self-check the round keeps. The answer pins this.
bar_fixed_target() {
    case "$1" in
        r45_f1_reference_scores_one) echo 5 ;;
        r91_b4_*) echo 3 ;;
    esac
}

bar_fixed_reason() {
    case "$1" in
        r45_f1_reference_scores_one) echo "R63 F4 / spec answer 1: its 5-run production-shaped self-check, at 5 in every case" ;;
        r91_b4_*) echo "deterministic fixture, 3 runs (B4)" ;;
    esac
}

# The latest bar record that ended PASS, by its `finished:` time. A
# running bar has no `finished:` line; a failed one has no PASS result.
last_passing_bar() {
    local dir="$1" f t best="" best_t=""
    for f in "$dir"/flake-*.txt; do
        [[ -f "$f" ]] || continue
        grep -qE '^RESULT: PASS' "$f" || continue
        t="$(awk '$1 == "finished:" { print $2 }' "$f" | tail -1)"
        [[ -n "$t" ]] || continue
        if [[ -z "$best_t" || "$t" > "$best_t" ]]; then
            best="$f"
            best_t="$t"
        fi
    done
    printf '%s' "$best"
}

# --- the seams the self-test overrides ---
bar_base_commit() {
    local record
    record="$(last_passing_bar "$(bar_gates_dir)")"
    [[ -n "$record" ]] || return 0
    awk '$1 == "commit:" { print $2; exit }' "$record"
}

bar_ancestor() {
    local repo="$1" base="$2"
    git -C "$repo" merge-base --is-ancestor "$base" HEAD 2>/dev/null
}

bar_changed_paths() {
    changed_graded_paths "$1" "$2"
}

bar_selected_reasons() {
    select_ready_proofs "$1" "$2"
}

# A change to any of these puts every proof at risk.
bar_global_change() {
    local want
    for want in "$@"; do
        case "$want" in
            src/pipeline.rs|src/grading.rs|src/verifier/sql/driver.rs|Cargo.toml|Cargo.lock|rust-toolchain.toml|Dockerfile.sandbox|build.rs)
                return 0 ;;
        esac
    done
    return 1
}

# Print one `TARGET <proof> <runs> <reason>` line per proof.
bar_targets_for() {
    local repo="$1"
    shift
    local -a proofs=("$@")
    local base="" global=0 why="" p reason fixed
    local -a changed=()
    local -A selected=()
    base="$(bar_base_commit)"
    if [[ -z "$base" ]]; then
        global=1
        why="no previous passing bar"
    elif ! bar_ancestor "$repo" "$base"; then
        global=1
        why="the last passing bar's commit is not an ancestor of HEAD"
    else
        mapfile -t changed < <(bar_changed_paths "$repo" "$base")
        if bar_global_change "${changed[@]}"; then
            global=1
            why="a change every proof depends on"
        else
            while IFS=$'\t' read -r p reason; do
                [[ -n "$p" ]] && selected[$p]="$reason"
            done < <(bar_selected_reasons "$repo" "$base")
        fi
    fi
    for p in "${proofs[@]}"; do
        fixed="$(bar_fixed_target "$p")"
        if [[ -n "$fixed" ]]; then
            echo "TARGET $p $fixed fixed ($(bar_fixed_reason "$p"))"
        elif [[ "$global" -eq 1 ]]; then
            echo "TARGET $p 20 $why"
        elif [[ -n "${selected[$p]:-}" ]]; then
            echo "TARGET $p 20 ${selected[$p]}"
        else
            echo "TARGET $p 5 not selected by the change since $base"
        fi
    done
}

# A proof's target as pinned in the bar record (the only source for a
# running/resumed bar); falls back to the fixed target, then 20.
bar_target_for() {
    local record="$1" proof="$2" pinned
    if [[ -f "$record" ]] \
        && pinned="$(awk -v p="$proof" '
            $1 == "TARGET" && $2 == p { print $3; found = 1; exit }
            END { exit !found }' "$record")"; then
        printf '%s' "$pinned"
        return 0
    fi
    if [[ -n "$(bar_fixed_target "$proof")" ]]; then
        bar_fixed_target "$proof"
        return 0
    fi
    echo 20
}

# Append the targets to a fresh record unless it already has them.
bar_write_targets() {
    local record="$1" repo="$2"
    shift 2
    if [[ -f "$record" ]] && grep -q '^TARGET ' "$record"; then
        return 0
    fi
    bar_targets_for "$repo" "$@" >> "$record"
}

self_test() {
    local dir failed=0
    TEST_DIR="$(mktemp -d /tmp/sdb-bar-targets.XXXXXX)"
    local dir="$TEST_DIR"
    trap 'rm -rf "${TEST_DIR:-}"' EXIT
    FAKE_BASE=""
    FAKE_ANCESTOR=0
    FAKE_CHANGED=()
    FAKE_REASONS=()
    bar_base_commit() { printf '%s' "$FAKE_BASE"; }
    bar_ancestor() { return "$FAKE_ANCESTOR"; }
    bar_changed_paths() { printf '%s\n' "${FAKE_CHANGED[@]}"; }
    bar_selected_reasons() { printf '%s\n' "${FAKE_REASONS[@]}"; }

    local proofs=(test_pipeline_eval_only_sql_stage5_crash_live
                  test_pipeline_eval_only_sql_stage7_aggregates_live
                  test_pipeline_eval_only_sql_stage11_indexes_live
                  r49_f2_unsynced_candidate_floors_live
                  r45_f1_reference_scores_one)
    out="$(bar_targets_for "$PWD" "${proofs[@]}")"

    target_of() { awk -v p="$2" '$1 == "TARGET" && $2 == p { print $3 }' <<<"$1"; }
    reason_of() { awk -v p="$2" '$1 == "TARGET" && $2 == p { $1=""; $2=""; $3=""; sub(/^  */, ""); print }' <<<"$1"; }

    # 1) A crash.rs-only change selects the stage-5 proof and the core
    #    proof; the rest run 5.
    FAKE_BASE="aaaa1111"
    FAKE_CHANGED=(src/verifier/sql/crash.rs)
    FAKE_REASONS=($'test_pipeline_eval_only_sql_stage5_crash_live\tmapped: src/verifier/sql/crash.rs'
                  $'r49_f2_unsynced_candidate_floors_live\tcore')
    out="$(bar_targets_for "$PWD" "${proofs[@]}")"
    [[ "$(target_of "$out" test_pipeline_eval_only_sql_stage5_crash_live)" == 20 ]] || {
        echo "crash.rs: the stage-5 proof is not 20: $out"; failed=1; }
    [[ "$(target_of "$out" r49_f2_unsynced_candidate_floors_live)" == 20 ]] || {
        echo "crash.rs: a core proof is not 20: $out"; failed=1; }
    [[ "$(target_of "$out" test_pipeline_eval_only_sql_stage7_aggregates_live)" == 5 ]] || {
        echo "crash.rs: an unrelated proof is not 5: $out"; failed=1; }
    grep -q 'mapped: src/verifier/sql/crash.rs' <<<"$out" || {
        echo "crash.rs: the record does not name the path: $out"; failed=1; }
    [[ "$(target_of "$out" r45_f1_reference_scores_one)" == 5 ]] || {
        echo "r45_f1 is not the fixed 5: $out"; failed=1; }
    echo '[self-test] a crash.rs change targets the stage-5 and core proofs'

    # 2) A path every proof depends on makes every proof 20.
    FAKE_CHANGED=(src/pipeline.rs)
    FAKE_REASONS=()
    out="$(bar_targets_for "$PWD" "${proofs[@]}")"
    for p in "${proofs[@]}"; do
        [[ "$p" == r45_f1_reference_scores_one ]] && continue
        [[ "$(target_of "$out" "$p")" == 20 ]] || {
            echo "pipeline.rs: $p is not 20: $out"; failed=1; }
    done
    grep -q 'a change every proof depends on' <<<"$out" || {
        echo "pipeline.rs: the reason is not named: $out"; failed=1; }
    echo '[self-test] a pipeline.rs change targets every proof'

    # 3) No previous passing bar: every proof 20.
    FAKE_BASE=""
    out="$(bar_targets_for "$PWD" "${proofs[@]}")"
    for p in "${proofs[@]}"; do
        [[ "$p" == r45_f1_reference_scores_one ]] && continue
        [[ "$(target_of "$out" "$p")" == 20 ]] || {
            echo "no base: $p is not 20: $out"; failed=1; }
    done
    grep -q 'no previous passing bar' <<<"$out" || {
        echo "no base: the reason is not named: $out"; failed=1; }
    echo '[self-test] no previous passing bar targets every proof'

    # 4) A base that is not an ancestor: every proof 20.
    FAKE_BASE="bbbb2222"
    FAKE_ANCESTOR=1
    out="$(bar_targets_for "$PWD" "${proofs[@]}")"
    grep -q 'not an ancestor of HEAD' <<<"$out" || {
        echo "non-ancestor: the reason is not named: $out"; failed=1; }
    FAKE_ANCESTOR=0

    # 5) A resumed bar reads the pinned targets, never recomputes.
    record="$dir/pinned.txt"
    printf 'TARGET test_pipeline_eval_only_sql_stage7_aggregates_live 20 pinned\n' > "$record"
    [[ "$(bar_target_for "$record" test_pipeline_eval_only_sql_stage7_aggregates_live)" == 20 ]] || {
        echo "pinned: the record's target was not read"; failed=1; }
    [[ "$(bar_target_for "$record" test_pipeline_eval_only_sql_stage11_indexes_live)" == 20 ]] || {
        echo "pinned: an unpinned proof is not the fallback 20"; failed=1; }
    [[ "$(bar_target_for "$record" r45_f1_reference_scores_one)" == 5 ]] || {
        echo "pinned: r45_f1 lost its fixed 5"; failed=1; }
    echo '[self-test] a resumed bar reads its pinned targets'

    [[ "$failed" -eq 0 ]] || return 1
    echo "RESULT: PASS (bar-targets self-test)"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "${1:-}" in
        --self-test) self_test ;;
        *) echo "usage: bar-targets.sh --self-test" >&2; exit 2 ;;
    esac
fi
