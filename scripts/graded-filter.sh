#!/usr/bin/env bash
# R61 F3/R61-5: the one tests.rs rule for the graded-path list.
#
# `scripts/graded-paths.txt` holds the path patterns first, then two kinds
# of data lines that are not pathspecs:
#
#   exclude-name tests.rs
#   keep src/pipeline/tests.rs
#
# `graded_path_entries` prints the pathspecs (and the keep path, which the
# patterns above do not match) for `git ls-tree`/`git diff`, and every
# reader filters the resulting paths through `graded_filter`.
graded_path_entries() {
    grep -vE '^[[:space:]]*(#|$)|^(exclude-name|keep)[[:space:]]' \
        "$(git rev-parse --show-toplevel)/scripts/graded-paths.txt"
    graded_keeps
}
graded_exclude_names() {
    awk '$1 == "exclude-name" { print $2 }' \
        "$(git rev-parse --show-toplevel)/scripts/graded-paths.txt"
}
graded_keeps() {
    awk '$1 == "keep" { print $2 }' \
        "$(git rev-parse --show-toplevel)/scripts/graded-paths.txt"
}
# Reads paths on stdin (plain names, or `git ls-tree` lines with the path
# after a tab) and drops a basename equal to an exclude-name, except an
# exact keep path. R61-5: filter on the path field, not the whole line.
graded_filter() {
    local ex keep
    ex="$(graded_exclude_names | paste -sd, -)"
    keep="$(graded_keeps | paste -sd, -)"
    awk -F '\t' -v ex="$ex" -v keep="$keep" '
        BEGIN {
            ne = split(ex, e, ",")
            nk = split(keep, k, ",")
        }
        {
            p = $NF
            n = split(p, a, "/")
            excluded = 0
            for (i = 1; i <= ne; i++) {
                if (e[i] != "" && a[n] == e[i]) excluded = 1
            }
            for (i = 1; i <= nk; i++) {
                if (k[i] != "" && p == k[i]) excluded = 0
            }
            if (!excluded) print
        }'
}
