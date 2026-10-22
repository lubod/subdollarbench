#!/usr/bin/env bash
# Round gate: fmt, clippy and the full niced test suite on a clean commit.
# Writes <git-common-dir>/sdb-gates/<commit>.txt (shared by all worktrees,
# never committed) so the reviewer can read the result for that exact commit
# instead of rerunning it. See AGENTS.md "Gates".
#
# Usage: scripts/gate.sh [--dirty]   (--dirty: allow uncommitted changes;
#                                     the record is then marked DIRTY)
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

dirty=0
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
    if [[ "${1:-}" != "--dirty" ]]; then
        echo "[gate] FATAL: uncommitted changes; commit first (or pass --dirty)" >&2
        exit 1
    fi
    dirty=1
fi

commit="$(git rev-parse --short=12 HEAD)"
dir="$(git rev-parse --path-format=absolute --git-common-dir)/sdb-gates"
mkdir -p "$dir"
out="$dir/$commit.txt"
[[ $dirty == 1 ]] && out="$dir/$commit-DIRTY.txt"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT

{
    echo "commit:  $(git rev-parse HEAD)"
    echo "branch:  $(git rev-parse --abbrev-ref HEAD)"
    echo "subject: $(git log -1 --format=%s)"
    echo "tree:    $([[ $dirty == 1 ]] && echo DIRTY || echo clean)"
    echo "started: $(date -u +%FT%TZ)"
    echo "rustc:   $(rustc --version)"
} >"$out"

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

step "cargo fmt --check" cargo fmt --check
step "cargo clippy --all-targets -D warnings" cargo clippy --all-targets -- -D warnings
step "cargo test (full, unfiltered)" cargo test

echo "finished: $(date -u +%FT%TZ)" >>"$out"
if [[ $fail == 0 ]]; then echo "RESULT: PASS" >>"$out"; else echo "RESULT: FAIL" >>"$out"; fi

cat "$out"
echo "[gate] record: $out"
exit $fail
