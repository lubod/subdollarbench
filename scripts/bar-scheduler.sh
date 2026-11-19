#!/usr/bin/env bash
# R91 B3: the bar's plan: correctness-only proofs run up to 3 at a time on
# distinct workers; timing proofs run alone, with nothing beside them.
#
# The table is explicit (a proof not listed is timing, the safe default)
# and the self-test greps each listed proof's `let worker = Nu8` from the
# source, so a drifted table fails instead of silently sharing a worker.
# Distinct workers imply distinct SQL ports (the bases are 100 apart and
# the auxiliary offsets are 50/60/61), and the self-test checks the port
# sets anyway.
#
# Source this file; do not execute it except `--self-test`.
set -uo pipefail

BAR_SCHED_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# R91 B3: the proofs that check answers, never timing. Stage 0-3 are one
# proof (`stages_0_3`); stages 4, 7-10, 13 and 14 follow the round's list.
bar_untimed_proofs() {
    cat <<'EOF'
test_pipeline_eval_only_sql_stages_0_3_live
test_pipeline_eval_only_sql_stage4_persistence_live
test_pipeline_eval_only_sql_stage7_aggregates_live
test_pipeline_eval_only_sql_stage8_joins_live
test_pipeline_eval_only_sql_stage8_lite_live
test_pipeline_eval_only_sql_stage9_constraints_live
test_pipeline_eval_only_sql_stage10_subqueries_live
test_pipeline_eval_only_sql_stage13_schema_change_live
test_pipeline_eval_only_sql_stage14_observability_live
EOF
}

bar_untimed() {
    bar_untimed_proofs | grep -qx "$1"
}

# The worker each untimed proof runs on (its own `let worker`).
bar_proof_worker() {
    case "$1" in
        test_pipeline_eval_only_sql_stages_0_3_live) echo 112 ;;
        test_pipeline_eval_only_sql_stage4_persistence_live) echo 75 ;;
        test_pipeline_eval_only_sql_stage7_aggregates_live) echo 97 ;;
        test_pipeline_eval_only_sql_stage8_joins_live) echo 102 ;;
        test_pipeline_eval_only_sql_stage8_lite_live) echo 104 ;;
        test_pipeline_eval_only_sql_stage9_constraints_live) echo 105 ;;
        test_pipeline_eval_only_sql_stage10_subqueries_live) echo 8 ;;
        test_pipeline_eval_only_sql_stage13_schema_change_live) echo 33 ;;
        test_pipeline_eval_only_sql_stage14_observability_live) echo 42 ;;
        *) echo "" ;;
    esac
}

# The host ports one worker binds (the r37 list: the task bases, the admin
# and dashboard, the proxy pool, and the producer/HA offsets).
bar_worker_ports() {
    local w="$1" base
    base=$((5480 + 100 * w))
    printf '%s\n' \
        "$base" "$((base + 50))" "$((base + 60))" "$((base + 61))" \
        "$((5481 + 100 * w))" "$((8101 + 100 * w))" "$((8102 + 100 * w))" \
        "$((8103 + 100 * w))" "$((7001 + 100 * w))" "$((7002 + 100 * w))" \
        "$((7003 + 100 * w))" | sort
}

# True when two workers' port sets do not intersect.
bar_ports_disjoint() {
    local a b common
    a="$1"; b="$2"
    common="$(comm -12 <(bar_worker_ports "$a") <(bar_worker_ports "$b"))"
    [[ -z "$common" ]]
}

# Print the execution plan: `BATCH t1 t2 t3` (untimed, distinct workers)
# and `ALONE t` (timing, nothing beside it).
bar_plan() {
    local -a untimed=() timing=() batch=()
    local t b can
    for t in "$@"; do
        if bar_untimed "$t"; then untimed+=("$t"); else timing+=("$t"); fi
    done
    for t in "${untimed[@]}"; do
        can=1
        [[ ${#batch[@]} -ge 3 ]] && can=0
        if [[ "$can" -eq 1 ]]; then
            for b in "${batch[@]}"; do
                if ! bar_ports_disjoint "$(bar_proof_worker "$b")" "$(bar_proof_worker "$t")"; then
                    can=0
                fi
            done
        fi
        if [[ "$can" -eq 1 ]]; then
            batch+=("$t")
        else
            printf 'BATCH %s\n' "${batch[*]}"
            batch=("$t")
        fi
    done
    [[ ${#batch[@]} -gt 0 ]] && printf 'BATCH %s\n' "${batch[*]}"
    for t in "${timing[@]}"; do
        printf 'ALONE %s\n' "$t"
    done
}

self_test() {
    local failed=0 t line w src
    local -a untimed timing
    mapfile -t untimed < <(bar_untimed_proofs)
    timing=(test_pipeline_eval_only_sql_stage5_crash_live
            test_pipeline_eval_only_sql_stage6_transactions_live
            test_pipeline_eval_only_sql_stage11_indexes_live
            test_pipeline_eval_only_sql_stage20_pgwire_live
            r45_f1_reference_scores_one
            r49_f2_unsynced_candidate_floors_live
            r89_f5_compact_reference_passes_stage5
            r89_f5_reference_without_ticket2_passes_its_branch)
    # The table's workers must match each proof's own `let worker = Nu8`.
    src="$BAR_SCHED_ROOT/src/pipeline/tests.rs"
    for t in "${untimed[@]}"; do
        w="$(bar_proof_worker "$t")"
        [[ -n "$w" ]] || { echo "no worker for $t"; failed=1; continue; }
        grep -A8 "fn $t()" "$src" | grep -q "worker = ${w}u8" || {
            echo "the table's worker $w for $t is not the proof's"; failed=1; }
    done
    echo "[self-test] the untimed table matches the proofs' workers"

    # The plan: only untimed members in a batch, at most 3, distinct
    # workers and ports; every timing proof alone.
    mapfile -t plan < <(bar_plan "${untimed[@]}" "${timing[@]}")
    local members n
    for line in "${plan[@]}"; do
        case "$line" in
            BATCH\ *)
                members=(${line#BATCH })
                n=${#members[@]}
                [[ "$n" -le 3 ]] || { echo "a batch of $n proofs: $line"; failed=1; }
                local i b ports
                for ((i = 0; i < n; i++)); do
                    bar_untimed "${members[$i]}" || {
                        echo "a timing proof in a batch: ${members[$i]}"; failed=1; }
                    for ((b = i + 1; b < n; b++)); do
                        bar_ports_disjoint "$(bar_proof_worker "${members[$i]}")" \
                            "$(bar_proof_worker "${members[$b]}")" || {
                            echo "workers share a port: ${members[$i]} ${members[$b]}"
                            failed=1; }
                    done
                done
                ;;
            ALONE\ *)
                t="${line#ALONE }"
                for members in "${untimed[@]}"; do
                    [[ "$members" == "$t" ]] && { echo "$t is alone but untimed"; failed=1; }
                done
                ;;
            *) echo "bad plan line: $line"; failed=1 ;;
        esac
    done
    echo "[self-test] batches are untimed, at most 3, port-disjoint; timing proofs run alone"

    # An unlisted proof is a timing proof and runs alone.
    [[ "$(bar_plan test_pipeline_eval_only_sql_stage9_lite_live)" == "ALONE test_pipeline_eval_only_sql_stage9_lite_live" ]] || {
        echo "an unlisted proof was not alone"; failed=1; }
    echo "[self-test] an unlisted proof runs alone"

    [[ "$failed" -eq 0 ]] || return 1
    echo "RESULT: PASS (bar-scheduler self-test)"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "${1:-}" in
        --self-test) self_test ;;
        *) echo "usage: bar-scheduler.sh --self-test" >&2; exit 2 ;;
    esac
fi
