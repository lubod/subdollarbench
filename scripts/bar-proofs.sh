#!/usr/bin/env bash
# R61 F3/F7: the bar's proof list, from the source (one copy for the bar,
# the ready gate's map and their self-tests).
#
# The bar keeps every ladder proof (the old list) and adds the r45/r49 live
# proofs. The fast-gate skip list is scripts/live-proofs.sh, which is the
# SQL_PERF_LIVE holders only; the bar must not shrink to that list.
bar_proofs() {
    local root="${1:-$(git rev-parse --show-toplevel)}"
    {
        grep -ohE 'fn test_pipeline_eval_only_sql_stage[0-9a-z_]*_live' \
            "$root/src/pipeline.rs" "$root/src/pipeline/tests.rs" 2>/dev/null || true
        grep -ohE 'fn r4[59]_[a-z0-9_]*' \
            "$root/src/pipeline.rs" "$root/src/pipeline/tests.rs" 2>/dev/null || true
    } | sed 's/^fn //' | sort -u
}
