#!/usr/bin/env bash
# R48-5: the long-test lock, shared by the full gate, the flake bar and the
# grid's preflight. Source this file; do not execute it.
#
# A full `gate.sh` or a `flake-bar.sh` run holds a shared lock
# (`flock -s`) for its whole run. `grid.sh` probes it with `flock -n -x`:
# an exclusive probe fails while a shared holder lives, so no phase starts
# next to a long test. `gate.sh --fast` and plain unit tests never hold it.
#
# `SDB_LONG_TEST_LOCK` overrides the path (the self-tests build a private
# lock); otherwise it is <git-common-dir>/sdb-gates/long-test.lock.

long_test_lock_path() {
    local common
    if [[ -n "${SDB_LONG_TEST_LOCK:-}" ]]; then
        printf '%s\n' "$SDB_LONG_TEST_LOCK"
        return 0
    fi
    common="${SDB_GIT_COMMON_DIR:-$(git rev-parse --path-format=absolute --git-common-dir)}"
    printf '%s\n' "$common/sdb-gates/long-test.lock"
}

# Hold the shared lock on fd 9 for the rest of this shell's life. Blocking:
# another long test can queue behind an exclusive holder.
hold_long_test_lock() {
    local path
    path="$(long_test_lock_path)"
    mkdir -p "$(dirname "$path")"
    exec 9>"$path"
    flock -s 9
}

# True (exit 0) when another process holds the lock. The probe takes an
# exclusive lock, so any shared holder (a running gate or bar) wins.
long_test_lock_busy() {
    local path
    path="$(long_test_lock_path)"
    mkdir -p "$(dirname "$path")"
    ! flock -n -x "$path" true 2>/dev/null
}
