#!/usr/bin/env bash
# R54 F4: point this clone at the hooks in scripts/hooks.
#
# `core.hooksPath` is clone-local config, shared by every worktree. The
# relative path resolves in each worktree, so a round worktree runs its own
# copy of the hooks (the files are identical across branches anyway).
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"
for hook in commit-msg pre-push; do
    path="scripts/hooks/$hook"
    [[ -f "$path" ]] || { echo "[hooks] missing $path" >&2; exit 1; }
    chmod +x "$path"
done
git config core.hooksPath scripts/hooks
echo "[hooks] core.hooksPath=$(git config --get core.hooksPath)"
echo "[hooks] commit-msg checks the round subject; pre-push runs the secret scan"
