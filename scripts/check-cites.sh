#!/usr/bin/env bash
# R54 F5: check the `path:N` / `path:N-M` cites in a round file.
#
# Each cited path must exist at HEAD and N (and M) must be within its line
# count. When the same backtick span or the next one names a symbol, the
# cited range (plus five lines either side) must contain that symbol.
# Failing cites are printed; the exit code is 1 if any failed.
#
# Usage: scripts/check-cites.sh <round file>
#        scripts/check-cites.sh --self-test
set -euo pipefail

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

self_test() {
    local dir repo
    dir="$(mktemp -d "${TMPDIR:-/tmp}/sdb-cites.XXXXXX")"
    trap 'rm -rf "${dir:-}"' EXIT
    repo="$dir/repo"
    mkdir -p "$repo/src"
    git -C "$repo" init -q
    git -C "$repo" config user.email cites@example.com
    git -C "$repo" config user.name cites
    cat >"$repo/src/lib.rs" <<'EOF'
pub fn good_symbol() -> i32 {
    1
}

// filler
// filler
// filler
// filler
// filler
pub fn moved_symbol() -> i32 {
    2
}

// filler
// filler
// filler
// filler
// filler
// filler
// filler
EOF
    git -C "$repo" add -A
    git -C "$repo" commit -qm fixture

    cat >"$dir/good.md" <<'EOF'
A good cite: `src/lib.rs:1` `good_symbol`, and a range `src/lib.rs:1-3` `good_symbol`.
EOF
    cat >"$dir/past.md" <<'EOF'
A cite past the end: `src/lib.rs:999`.
EOF
    cat >"$dir/moved.md" <<'EOF'
A cite whose symbol moved: `src/lib.rs:1` `moved_symbol`.
EOF

    local rc=0 out
    if (cd "$repo" && bash "$SELF" "$dir/good.md") >"$dir/good.out" 2>&1; then
        :
    else
        echo '[self-test] a good cite was reported' >&2
        cat "$dir/good.out" >&2
        return 1
    fi
    echo '[self-test] good cite passes'

    rc=0
    (cd "$repo" && bash "$SELF" "$dir/past.md") >"$dir/past.out" 2>&1 || rc=$?
    [[ "$rc" -eq 1 ]] || { echo "[self-test] a past-end cite exit $rc, want 1" >&2; return 1; }
    grep -q '999' "$dir/past.out" || {
        echo '[self-test] the past-end cite was not reported' >&2; cat "$dir/past.out" >&2; return 1; }
    echo '[self-test] past-end cite reported'

    rc=0
    (cd "$repo" && bash "$SELF" "$dir/moved.md") >"$dir/moved.out" 2>&1 || rc=$?
    [[ "$rc" -eq 1 ]] || { echo "[self-test] a moved-symbol cite exit $rc, want 1" >&2; return 1; }
    grep -q 'moved_symbol' "$dir/moved.out" || {
        echo '[self-test] the moved symbol was not reported' >&2; cat "$dir/moved.out" >&2; return 1; }
    echo '[self-test] moved-symbol cite reported'

    echo 'RESULT: PASS (check-cites self-test)'
}

if [[ "${1:-}" == "--self-test" ]]; then
    self_test
    exit $?
fi
[[ $# -eq 1 ]] || { echo "usage: scripts/check-cites.sh <round file>" >&2; exit 2; }
[[ -f "$1" ]] || { echo "[cites] no such round file: $1" >&2; exit 2; }

exec python3 - "$1" <<'PYEOF'
import re
import subprocess
import sys

round_file = sys.argv[1]
text = open(round_file, encoding="utf-8", errors="replace").read()

CITE = re.compile(
    r"^([A-Za-z0-9_./+-]+\.(?:rs|sh|md|toml|json|txt|ya?ml|py|sql|lock|cfg|env|out|log|html))"
    r":([0-9]+)(?:-([0-9]+))?$"
)
SYMBOL = re.compile(r"^(?:fn\s+)?([a-z_][a-z0-9_]*(?:::[a-z_][a-z0-9_]*)*)$")
RESERVED = {
    "true", "false", "none", "null", "ok", "pass", "fail", "cargo", "git",
    "bash", "python3", "test", "open", "yes", "no",
}

problems = []
cache = {}


def file_at_head(path):
    if path in cache:
        return cache[path]
    exists = subprocess.run(
        ["git", "cat-file", "-e", f"HEAD:{path}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode == 0
    lines = []
    if exists:
        body = subprocess.run(
            ["git", "show", f"HEAD:{path}"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        if body.returncode == 0:
            lines = body.stdout.decode("utf-8", "replace").splitlines()
        else:
            exists = False
    cache[path] = (exists, lines)
    return cache[path]


for lineno, line in enumerate(text.splitlines(), 1):
    spans = list(re.finditer(r"`([^`]+)`", line))
    for i, span in enumerate(spans):
        m = CITE.match(span.group(1).strip())
        if not m:
            continue
        path, first, last = m.group(1), int(m.group(2)), m.group(3)
        first_end = int(last) if last else first
        exists, lines = file_at_head(path)
        label = f"{round_file}:{lineno}: `{span.group(1)}`"
        if not exists:
            problems.append(f"{label}: {path} is not at HEAD")
            continue
        count = len(lines)
        if first < 1 or first_end < first or first_end > count:
            problems.append(
                f"{label}: line {first_end} is outside {path} (1-{count})"
            )
            continue
        # The same span may carry `path:N symbol`; otherwise the next
        # span on the line may name a symbol.
        candidate = None
        tail = span.group(1)[span.group(1).find(":") + 1 :]
        tail = re.sub(r"^[0-9]+(?:-[0-9]+)?\s*", "", tail).strip()
        if tail:
            sm = SYMBOL.match(tail)
            if sm:
                candidate = sm.group(1)
        if candidate is None and i + 1 < len(spans):
            nxt = spans[i + 1].group(1).strip()
            sm = SYMBOL.match(nxt)
            if sm and sm.group(1) not in RESERVED:
                candidate = sm.group(1)
        if candidate is None:
            continue
        lo = max(1, first - 5)
        hi = min(count, first_end + 5)
        window = "\n".join(lines[lo - 1 : hi])
        if candidate not in window:
            problems.append(
                f"{label}: `{candidate}` is not in {path}:{lo}-{hi}"
            )

for problem in problems:
    print(f"[cites] {problem}")
if problems:
    sys.exit(1)
PYEOF
