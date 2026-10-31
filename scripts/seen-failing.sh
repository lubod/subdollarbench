#!/usr/bin/env bash
# R54 F3: leave a record that a new test was seen failing without its fix.
#
# Usage: scripts/seen-failing.sh <test> <patch>
#        scripts/seen-failing.sh --replay <record>
#        scripts/seen-failing.sh --self-test
#
# <test> is the cargo test filter (the full test path), <patch> is a patch
# that removes the fix (it may touch test and code alike). The script
# creates a temporary worktree at HEAD, applies the patch, and runs the one
# test under the shared cargo-test lock. A failing test writes
# <git-common-dir>/sdb-gates/seen-<HEAD>-<test>.txt (commit, patch sha256,
# command, first failing assertion line) and copies the patch next to it.
# A passing test writes nothing and exits 1.
set -euo pipefail

usage() {
    sed -n '4,7p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

slug() {
    printf '%s' "$1" | sed 's/[^A-Za-z0-9._-]/_/g'
}

first_failure() {
    local log="$1" line
    line="$(grep -m1 -E 'assertion .* failed' "$log" || true)"
    if [[ -z "$line" ]]; then
        line="$(grep -m1 -E 'panicked at|^error(\[|:)' "$log" || true)"
    fi
    if [[ -z "$line" ]]; then
        line="$(grep -m1 'FAILED' "$log" || true)"
    fi
    printf '%s' "$line"
}

# Run the test in a fresh worktree at `commit` with `patch` applied.
# Sets RUN_LOG, RUN_RC, RUN_WT and RUN_TMP.
run_patched() {
    local repo="$1" commit="$2" patch="$3" test="$4"
    local common
    common="$(git -C "$repo" rev-parse --path-format=absolute --git-common-dir)"
    RUN_TMP="$(mktemp -d "${TMPDIR:-/tmp}/sdb-seen.XXXXXX")"
    RUN_WT="$RUN_TMP/wt"
    RUN_LOG="$RUN_TMP/test.log"
    if ! git -C "$repo" worktree add --detach "$RUN_WT" "$commit" >/dev/null 2>&1; then
        echo "[seen] cannot create a worktree at $commit" >&2
        return 2
    fi
    if ! git -C "$RUN_WT" apply --whitespace=nowarn "$patch" 2>"$RUN_TMP/apply.err"; then
        echo "[seen] the patch does not apply at $commit:" >&2
        sed 's/^/[seen] /' "$RUN_TMP/apply.err" >&2
        return 2
    fi
    mkdir -p "$common/sdb-gates"
    RUN_RC=0
    (
        cd "$RUN_WT"
        CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target}" \
            flock "$common/sdb-gates/cargo-test.lock" \
            nice -n 19 cargo test "$test" -- --exact
    ) >"$RUN_LOG" 2>&1 || RUN_RC=$?
    return 0
}

cleanup_run() {
    if [[ -n "${RUN_WT:-}" && -d "${RUN_WT:-}" ]]; then
        local repo="${RUN_REPO:-}"
        if [[ -n "$repo" ]]; then
            git -C "$repo" worktree remove --force "$RUN_WT" >/dev/null 2>&1 \
                || rm -rf "$RUN_WT"
        else
            rm -rf "$RUN_WT"
        fi
    fi
    [[ -n "${RUN_TMP:-}" ]] && rm -rf "$RUN_TMP"
    return 0
}

record_case() {
    local test="$1" patch="$2"
    local repo head common dir base slugged record patch_copy sha
    repo="$(git rev-parse --show-toplevel)"
    head="$(git rev-parse HEAD)"
    common="$(git rev-parse --path-format=absolute --git-common-dir)"
    dir="$common/sdb-gates"
    mkdir -p "$dir"
    slugged="$(slug "$test")"
    base="seen-$head-$slugged"
    record="$dir/$base.txt"
    patch_copy="$dir/$base.patch"
    sha="$(sha256sum "$patch" | awk '{print $1}')"

    RUN_REPO="$repo"
    trap cleanup_run EXIT
    if ! run_patched "$repo" "$head" "$patch" "$test"; then
        cleanup_run
        trap - EXIT
        return 2
    fi
    if [[ "$RUN_RC" -eq 0 ]]; then
        # A test that passes without its fix proves nothing: no record.
        rm -f "$record" "$patch_copy"
        echo "[seen] $test passes with the patch; no record written" >&2
        cleanup_run
        trap - EXIT
        return 1
    fi
    local line
    line="$(first_failure "$RUN_LOG")"
    [[ -n "$line" ]] || line="(no assertion line; see the test output)"
    cp "$patch" "$patch_copy"
    {
        printf 'test: %s\n' "$test"
        printf 'commit: %s\n' "$head"
        printf 'patch_sha256: %s\n' "$sha"
        printf 'patch_file: %s\n' "$(basename "$patch_copy")"
        printf 'command: cargo test %s -- --exact\n' "$test"
        printf 'result: FAIL\n'
        printf 'failing: %s\n' "$line"
    } >"$record"
    echo "[seen] record: $record"
    echo "[seen] failing: $line"
    cleanup_run
    trap - EXIT
    return 0
}

replay_case() {
    local record="$1"
    [[ -f "$record" ]] || { echo "[seen] no record: $record" >&2; exit 1; }
    local dir test commit patch_file want_sha patch got_sha
    dir="$(dirname "$record")"
    test="$(sed -n 's/^test: //p' "$record" | head -1)"
    commit="$(sed -n 's/^commit: //p' "$record" | head -1)"
    patch_file="$(sed -n 's/^patch_file: //p' "$record" | head -1)"
    want_sha="$(sed -n 's/^patch_sha256: //p' "$record" | head -1)"
    [[ -n "$test" && -n "$commit" && -n "$patch_file" ]] || {
        echo "[seen] malformed record: $record" >&2; exit 1; }
    patch="$dir/$patch_file"
    [[ -f "$patch" ]] || { echo "[seen] missing patch: $patch" >&2; exit 1; }
    got_sha="$(sha256sum "$patch" | awk '{print $1}')"
    [[ "$got_sha" == "$want_sha" ]] || {
        echo "[seen] patch sha256 does not match the record" >&2; exit 1; }
    local repo
    repo="$(git rev-parse --show-toplevel)"
    RUN_REPO="$repo"
    trap cleanup_run EXIT
    if ! run_patched "$repo" "$commit" "$patch" "$test"; then
        cleanup_run
        trap - EXIT
        exit 1
    fi
    local line
    line="$(first_failure "$RUN_LOG")"
    if [[ "$RUN_RC" -eq 0 ]]; then
        echo "[seen] replay: $test passes; the record does not reproduce" >&2
        cleanup_run
        trap - EXIT
        exit 1
    fi
    echo "[seen] replay: $test still fails at $commit"
    echo "[seen] failing: $line"
    cleanup_run
    trap - EXIT
    exit 0
}

self_test() {
    local dir repo test fail_patch noop_patch record rc=0
    dir="$(mktemp -d "${TMPDIR:-/tmp}/sdb-seen-self.XXXXXX")"
    trap 'rm -rf "${dir:-}"' EXIT
    repo="$dir/repo"
    mkdir -p "$repo/src"
    cat >"$repo/Cargo.toml" <<'EOF'
[package]
name = "seen-fixture"
version = "0.1.0"
edition = "2021"
EOF
    cat >"$repo/src/lib.rs" <<'EOF'
pub fn answer() -> i32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_is_one() {
        assert_eq!(answer(), 1);
    }
}
EOF
    git -C "$repo" init -q
    git -C "$repo" config user.email seen@example.com
    git -C "$repo" config user.name seen
    git -C "$repo" add -A
    git -C "$repo" commit -qm fixture

    # The fix removed: the test now expects 2.
    python3 - "$repo/src/lib.rs" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace("assert_eq!(answer(), 1);", "assert_eq!(answer(), 2);")
open(p, "w").write(s)
PY
    fail_patch="$dir/fail.patch"
    git -C "$repo" diff >"$fail_patch"
    git -C "$repo" checkout -- src/lib.rs

    # A patch that leaves the test passing.
    echo '// noop' >>"$repo/src/lib.rs"
    noop_patch="$dir/noop.patch"
    git -C "$repo" diff >"$noop_patch"
    git -C "$repo" checkout -- src/lib.rs

    test="tests::answer_is_one"
    record="$(git -C "$repo" rev-parse --path-format=absolute --git-common-dir)/sdb-gates/seen-$(git -C "$repo" rev-parse HEAD)-$(slug "$test").txt"

    # 1) A failing test leaves a record with the failing line.
    if ! (cd "$repo" && "$SELF" "$test" "$fail_patch" >"$dir/fail.out" 2>&1); then
        echo '[self-test] a failing test did not leave a record' >&2
        cat "$dir/fail.out" >&2
        return 1
    fi
    [[ -f "$record" ]] || { echo "[self-test] no record at $record" >&2; return 1; }
    grep -q '^result: FAIL$' "$record" || {
        echo '[self-test] the record does not say FAIL' >&2; cat "$record" >&2; return 1; }
    grep -q 'assertion' "$record" || {
        echo '[self-test] the record has no failing assertion line' >&2; cat "$record" >&2; return 1; }
    [[ -f "${record%.txt}.patch" ]] || {
        echo '[self-test] the patch was not copied next to the record' >&2; return 1; }
    echo '[self-test] failing test recorded'

    # 2) A passing test writes no record and exits 1.
    if (cd "$repo" && "$SELF" "$test" "$noop_patch" >"$dir/noop.out" 2>&1); then
        echo '[self-test] a passing test was accepted' >&2; return 1
    else
        rc=$?
    fi
    [[ "$rc" -eq 1 ]] || { echo "[self-test] passing test exit $rc, want 1" >&2; return 1; }
    [[ ! -f "$record" ]] || { echo '[self-test] a record survived a passing test' >&2; return 1; }
    echo '[self-test] passing test refused, no record'

    # 3) Replay reproduces the record.
    (cd "$repo" && "$SELF" "$test" "$fail_patch" >/dev/null 2>&1)
    if ! (cd "$repo" && "$SELF" --replay "$record" >"$dir/replay.out" 2>&1); then
        echo '[self-test] replay did not reproduce the failure' >&2
        cat "$dir/replay.out" >&2
        return 1
    fi
    grep -q 'still fails' "$dir/replay.out" || {
        echo '[self-test] replay did not report the failure' >&2; cat "$dir/replay.out" >&2; return 1; }
    echo '[self-test] replay reproduced the failure'

    echo 'RESULT: PASS (seen-failing self-test)'
}

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

case "${1:-}" in
    --self-test) self_test; exit $? ;;
    --replay)
        [[ $# -eq 2 ]] || usage
        replay_case "$2"
        ;;
    -h|--help) usage ;;
esac

[[ $# -eq 2 ]] || usage
if [[ ! -f "$2" ]]; then
    echo "[seen] patch not found: $2" >&2
    exit 1
fi
record_case "$1" "$2"
