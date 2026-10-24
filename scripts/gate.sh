#!/usr/bin/env bash
# Round gate: fmt, clippy and the niced test suite on a clean commit.
# Writes <git-common-dir>/sdb-gates/<commit>.txt (full) or
# <commit>.fast.txt (--fast), shared by all worktrees and never committed,
# so the reviewer can read the result for that exact commit instead of
# rerunning it. See AGENTS.md "Gates" for which mode a `ready` commit needs.
#
# Usage: scripts/gate.sh [--fast] [--dirty]
#   --fast   skip the serialized live SQL ladder proofs (the tests that hold
#            SQL_PERF_LIVE: ~28 of the full gate's ~33 min). Everything
#            else, including the other Docker tests, still runs.
#   --dirty  allow uncommitted changes; the record is then marked DIRTY.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

fast=0
allow_dirty=0
for arg in "$@"; do
    case "$arg" in
        --fast) fast=1 ;;
        --dirty) allow_dirty=1 ;;
        *) echo "[gate] FATAL: unknown argument $arg" >&2; exit 1 ;;
    esac
done

dirty=0
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
    if [[ $allow_dirty != 1 ]]; then
        echo "[gate] FATAL: uncommitted changes; commit first (or pass --dirty)" >&2
        exit 1
    fi
    dirty=1
fi

# Paths whose change needs the full gate on a `ready` commit: grading,
# scoring, cost, what the candidate sees, and the harness that runs it.
mapfile -t graded_paths < <(grep -vE '^[[:space:]]*(#|$)' scripts/graded-paths.txt)
base="$(git merge-base HEAD master 2>/dev/null || true)"
touched=""
if [[ -n "$base" ]]; then
    touched="$(git diff --name-only "$base" HEAD -- "${graded_paths[@]}" | head -5 | tr '\n' ' ')"
fi

commit="$(git rev-parse --short=12 HEAD)"
dir="$(git rev-parse --path-format=absolute --git-common-dir)/sdb-gates"
mkdir -p "$dir"
suffix=""
[[ $fast == 1 ]] && suffix=".fast"
[[ $dirty == 1 ]] && suffix="$suffix-DIRTY"
out="$dir/$commit$suffix.txt"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT

{
    echo "commit:  $(git rev-parse HEAD)"
    echo "branch:  $(git rev-parse --abbrev-ref HEAD)"
    echo "subject: $(git log -1 --format=%s)"
    echo "tree:    $([[ $dirty == 1 ]] && echo DIRTY || echo clean)"
    echo "mode:    $([[ $fast == 1 ]] && echo fast || echo full)"
    if [[ -n "$touched" ]]; then
        echo "scope:   graded paths changed since master ($touched...): ready needs the full gate"
    else
        echo "scope:   no graded path changed since master: the fast gate is enough for ready"
    fi
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
if [[ $fast == 1 ]]; then
    # The tests to skip are read from the source, so a new SQL_PERF_LIVE
    # proof is skipped without editing this script.
    skips=()
    while read -r name; do
        skips+=(--skip "$name")
    done < <(scripts/live-proofs.sh)
    echo "skipped: $(( ${#skips[@]} / 2 )) tests (the SQL_PERF_LIVE ladder proofs)" >>"$out"
    step "cargo test (fast: no serialized SQL ladder proofs)" cargo test -- "${skips[@]}"
else
    step "cargo test (full, unfiltered)" cargo test
fi

echo "finished: $(date -u +%FT%TZ)" >>"$out"
if [[ $fail == 0 ]]; then echo "RESULT: PASS" >>"$out"; else echo "RESULT: FAIL" >>"$out"; fi

cat "$out"
echo "[gate] record: $out"
exit $fail
