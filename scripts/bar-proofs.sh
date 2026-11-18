#!/usr/bin/env bash
# R61 F3/F7: the bar's proof list, from the source (one copy for the bar,
# the ready gate's map and their self-tests).
#
# The bar keeps every ladder proof (the old list) and adds the r45/r49 live
# proofs. The fast-gate skip list is scripts/live-proofs.sh, which is the
# SQL_PERF_LIVE holders only; the bar must not shrink to that list.
bar_proofs_from_source() {
    { grep -oE 'fn (test_pipeline_eval_only_sql_stage[0-9a-z_]*_live|r4[59]_[a-z0-9_]*|r89_f5_[a-z0-9_]*|r91_b4_[a-z0-9_]*)' || true; } \
        | sed 's/^fn //' | sort -u
}

bar_proofs() {
    local root="${1:-$(git rev-parse --show-toplevel)}"
    cat "$root/src/pipeline.rs" "$root/src/pipeline/tests.rs" 2>/dev/null \
        | bar_proofs_from_source
}

# R62 F2: the same list at a commit, so a proof added since the merge base
# can be selected as a new proof.
bar_proofs_at() {
    local repo="$1" ref="$2"
    {
        git -C "$repo" show "$ref:src/pipeline.rs" 2>/dev/null || true
        git -C "$repo" show "$ref:src/pipeline/tests.rs" 2>/dev/null || true
    } | bar_proofs_from_source
}

# R63 F4: the bar's per-proof run target. r45_f1's W5 is about 5 min of a
# 6 min run; five runs keep its production-shaped self-check in every bar
# without 20 x 6 min.
bar_target() {
    case "$1" in
        # R63 F4 / R91 spec answer 1: five runs in every bar (a 6-minute
        # production-shaped self-check); B2's 20/5 rule is for the rest.
        r45_f1_reference_scores_one) echo 5 ;;
        r91_b4_*) echo 3 ;;
        *) echo 20 ;;
    esac
}

# R91 B3: the proofs that check answers, never timing (the scheduler's
# table; a proof not listed is timing, the safe default).
bar_untimed_here() {
    case "$1" in
        test_pipeline_eval_only_sql_stages_0_3_live|\
        test_pipeline_eval_only_sql_stage4_persistence_live|\
        test_pipeline_eval_only_sql_stage7_aggregates_live|\
        test_pipeline_eval_only_sql_stage8_joins_live|\
        test_pipeline_eval_only_sql_stage8_lite_live|\
        test_pipeline_eval_only_sql_stage9_constraints_live|\
        test_pipeline_eval_only_sql_stage10_subqueries_live|\
        test_pipeline_eval_only_sql_stage13_schema_change_live|\
        test_pipeline_eval_only_sql_stage14_observability_live)
            return 0 ;;
    esac
    return 1
}
