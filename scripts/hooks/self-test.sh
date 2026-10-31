#!/usr/bin/env bash
# R54 F4: exercise both hooks in a throwaway repo.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMMIT_MSG="$HERE/commit-msg"
PRE_PUSH="$HERE/pre-push"

dir="$(mktemp -d "${TMPDIR:-/tmp}/sdb-hooks.XXXXXX")"
trap 'rm -rf "${dir:-}"' EXIT
repo="$dir/repo"
mkdir -p "$repo"
git -C "$repo" init -q
git -C "$repo" config user.email hooks@example.com
git -C "$repo" config user.name hooks
echo clean >"$repo/clean.txt"
git -C "$repo" add -A
git -C "$repo" commit -qm clean
base="$(git -C "$repo" rev-parse HEAD)"

msg() {
    printf '%s\n' "$1" >"$dir/msg"
}
expect_msg() {
    local subject="$1" want="$2" rc=0
    msg "$subject"
    (cd "$repo" && bash "$COMMIT_MSG" "$dir/msg") >/dev/null 2>&1 || rc=$?
    if [[ "$want" == "accept" && "$rc" -ne 0 ]]; then
        echo "[self-test] '$subject' was refused (rc=$rc)" >&2
        return 1
    fi
    if [[ "$want" == "refuse" && "$rc" -eq 0 ]]; then
        echo "[self-test] '$subject' was accepted" >&2
        return 1
    fi
    return 0
}

git -C "$repo" checkout -qb round54
expect_msg 'R54 F1: x' accept || exit 1
expect_msg 'R54 F1/F2: x' refuse || exit 1
expect_msg 'R54 F1 F2: x' refuse || exit 1
expect_msg 'R54 F1,F2: x' refuse || exit 1
expect_msg 'R54: ready abc' accept || exit 1
expect_msg 'Merge master into round54' accept || exit 1
expect_msg 'R53 F1: x' refuse || exit 1
git -C "$repo" checkout -q master
expect_msg 'Merge round54: tooling' accept || exit 1
expect_msg 'Round 54: open' accept || exit 1
echo '[self-test] commit-msg: one item accepted, two IDs refused, status accepted'

# A pushed range holding a credential: refused, and the value is not
# printed.
secret='sk-or-v1-FIXTURE-DO-NOT-PRINT'
echo "OPENROUTER_API_KEY=$secret" >"$repo/leak.txt"
git -C "$repo" add -A
git -C "$repo" commit -qm leak
leak="$(git -C "$repo" rev-parse HEAD)"
out="$dir/pre-push.out"
rc=0
(cd "$repo" && printf 'refs/heads/x %s refs/heads/x %s\n' "$leak" "$base" \
    | bash "$PRE_PUSH" origin "$dir/remote") >"$out" 2>&1 || rc=$?
[[ "$rc" -eq 1 ]] || { echo "[self-test] a secret was accepted (rc=$rc)" >&2; exit 1; }
grep -q 'leak.txt' "$out" || { echo '[self-test] the secret hit named no file' >&2; exit 1; }
if grep -qF "$secret" "$out"; then
    echo '[self-test] the secret value was printed' >&2
    exit 1
fi
echo '[self-test] secret range refused without the value'

# A tracked .env: refused; its content is not printed.
echo 'SDB_UI_TOKEN=fixture-value' >"$repo/.env"
git -C "$repo" add -A
git -C "$repo" commit -qm env
env_commit="$(git -C "$repo" rev-parse HEAD)"
rc=0
(cd "$repo" && printf 'refs/heads/x %s refs/heads/x %s\n' "$env_commit" "$leak" \
    | bash "$PRE_PUSH" origin "$dir/remote") >"$out" 2>&1 || rc=$?
[[ "$rc" -eq 1 ]] || { echo "[self-test] a tracked .env was accepted (rc=$rc)" >&2; exit 1; }
grep -q '\.env' "$out" || { echo '[self-test] the .env hit named no file' >&2; exit 1; }
if grep -qF 'fixture-value' "$out"; then
    echo '[self-test] the .env value was printed' >&2
    exit 1
fi
echo '[self-test] tracked .env refused without the value'

# A new branch is scanned too (the remote sha is all zeros).
rc=0
(cd "$repo" && printf 'refs/heads/new %s refs/heads/new %s\n' "$env_commit" \
    "0000000000000000000000000000000000000000" \
    | bash "$PRE_PUSH" origin "$dir/remote") >"$out" 2>&1 || rc=$?
[[ "$rc" -eq 1 ]] || { echo "[self-test] a new branch was not scanned (rc=$rc)" >&2; exit 1; }
grep -q '\.env' "$out" || { echo '[self-test] the new branch scan missed .env' >&2; exit 1; }
echo '[self-test] new branch scanned'

# A clean range passes. Branch from the first commit so the range
# excludes the secret commits: removing a secret still shows it in a diff.
git -C "$repo" checkout -q -b cleanbranch "$base"
echo clean2 >"$repo/clean.txt"
git -C "$repo" add -A
git -C "$repo" commit -qm clean2
clean="$(git -C "$repo" rev-parse HEAD)"
rc=0
(cd "$repo" && printf 'refs/heads/x %s refs/heads/x %s\n' "$clean" "$base" \
    | bash "$PRE_PUSH" origin "$dir/remote") >"$out" 2>&1 || rc=$?
[[ "$rc" -eq 0 ]] || {
    echo "[self-test] a clean range was refused (rc=$rc)" >&2
    cat "$out" >&2
    exit 1
}
echo '[self-test] clean range accepted'

# R55 F5: the hook passes its own source and allowlist; a real .env
# value is refused even on an allowlisted path; a pattern hit outside the
# list is refused. The value is built at run time so this file does not
# contain it.
hooksrepo="$dir/hooksrepo"
mkdir -p "$hooksrepo/scripts/hooks"
git -C "$hooksrepo" init -q
git -C "$hooksrepo" config user.email hooks@example.com
git -C "$hooksrepo" config user.name hooks
cp -a "$HERE/." "$hooksrepo/scripts/hooks/"
cp "$HERE/../secret-scan-fixtures.txt" "$hooksrepo/scripts/secret-scan-fixtures.txt"
git -C "$hooksrepo" add -A
git -C "$hooksrepo" commit -qm hooks
hooks_base="$(git -C "$hooksrepo" rev-parse HEAD)"
env_value="sk-or-v1-$(date +%s)-FIXTURE-ENV"
printf 'OPENROUTER_API_KEY=%s\n' "$env_value" >"$hooksrepo/.env"
push_hooks() {
    (cd "$hooksrepo" && printf 'refs/heads/x %s refs/heads/x %s\n' "$1" "${2:-0000000000000000000000000000000000000000}" \
        | bash "$PRE_PUSH" origin "$dir/remote")
}
rc=0
out="$(push_hooks "$hooks_base" 2>&1)" || rc=$?
[[ "$rc" -eq 0 ]] || {
    echo "[self-test] the hook refused its own source (rc=$rc)" >&2
    printf '%s\n' "$out" >&2
    exit 1
}
echo '[self-test] hook source accepted'

printf '\n# %s\n' "$env_value" >>"$hooksrepo/scripts/hooks/pre-push"
git -C "$hooksrepo" add scripts/hooks/pre-push
git -C "$hooksrepo" commit -qm value
value_commit="$(git -C "$hooksrepo" rev-parse HEAD)"
rc=0
out="$(push_hooks "$value_commit" "$hooks_base" 2>&1)" || rc=$?
[[ "$rc" -eq 1 ]] || { echo "[self-test] a real .env value was accepted (rc=$rc)" >&2; exit 1; }
grep -q 'scripts/hooks/pre-push' <<<"$out" || {
    echo '[self-test] the real-value hit named no file' >&2; printf '%s\n' "$out" >&2; exit 1; }
if grep -qF "$env_value" <<<"$out"; then
    echo '[self-test] the real .env value was printed' >&2
    exit 1
fi
echo '[self-test] real .env value refused on an allowlisted path'

echo 'sk-or-v1-NOT-ALLOWLISTED-FIXTURE' >"$hooksrepo/leak.txt"
git -C "$hooksrepo" add leak.txt
git -C "$hooksrepo" commit -qm leak
leak_commit="$(git -C "$hooksrepo" rev-parse HEAD)"
rc=0
out="$(push_hooks "$leak_commit" "$value_commit" 2>&1)" || rc=$?
[[ "$rc" -eq 1 ]] || { echo "[self-test] a pattern hit outside the list was accepted (rc=$rc)" >&2; exit 1; }
grep -q 'leak.txt' <<<"$out" || {
    echo '[self-test] the pattern hit named no file' >&2; printf '%s\n' "$out" >&2; exit 1; }
echo '[self-test] pattern hit outside the allowlist refused'

echo 'RESULT: PASS (hooks self-test)'
