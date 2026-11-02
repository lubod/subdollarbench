#!/usr/bin/env bash
# R61 F7: the changed-path -> proofs map for `gate.sh --ready`.
#
# Source this file; do not execute it. `scripts/proof-map.txt` is the data,
# `scripts/ready-proofs.txt` the core set, and `scripts/bar-proofs.sh` the
# one bar list this and flake-bar.sh share.
set -uo pipefail

PROOF_MAP_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$PROOF_MAP_ROOT/scripts/graded-filter.sh"
. "$PROOF_MAP_ROOT/scripts/bar-proofs.sh"

proof_map_file() {
    printf '%s\n' "${SDB_PROOF_MAP_FILE:-$PROOF_MAP_ROOT/scripts/proof-map.txt}"
}

ready_proofs_file() {
    printf '%s\n' "${SDB_READY_PROOFS_FILE:-$PROOF_MAP_ROOT/scripts/ready-proofs.txt}"
}

proof_map_entries() {
    grep -vE '^[[:space:]]*(#|$)' "$(proof_map_file)" || true
}

core_proofs() {
    grep -vE '^[[:space:]]*(#|$)' "$(ready_proofs_file)" || true
}

# The proofs --ready can add: exactly scripts/live-proofs.sh, the list the
# fast set skips. A proof outside it already ran in the fast set.
ready_full_set() {
    local repo="$1"
    ( cd "$repo" && scripts/live-proofs.sh )
}

path_is_mapped() {
    local path="$1" line glob
    while IFS= read -r line; do
        glob="${line%% *}"
        [[ "$path" == $glob ]] && return 0
    done < <(proof_map_entries)
    return 1
}

# Print the extra proofs a path selects. "full" expands to the bar list of
# the given repo; "core" selects nothing (the core proofs already run).
proofs_for_path() {
    local repo="$1" path="$2" line glob toks name oldifs
    while IFS= read -r line; do
        glob="${line%% *}"
        toks="${line#* }"
        [[ "$path" == $glob ]] || continue
        oldifs="$IFS"
        IFS=','
        for name in $toks; do
            case "$name" in
                full) ready_full_set "$repo" ;;
                core) ;;
                *) printf '%s\n' "$name" ;;
            esac
        done
        IFS="$oldifs"
    done < <(proof_map_entries)
}

changed_graded_paths() {
    local repo="$1" base="$2"
    (
        cd "$repo" || exit 1
        mapfile -t paths < <(graded_path_entries)
        git diff --name-only "$base" HEAD -- "${paths[@]}" | graded_filter
    )
}

# R62 F1: top-level fn spans in a Rust test file: name, start line and
# end line (1-based). The span starts at the doc comment/attributes and
# ends at the closing brace, so a hunk on either selects the function.
fn_spans() {
    python3 - "$1" <<'PY'
import re
import sys

path = sys.argv[1]
lines = open(path, encoding="utf-8", errors="replace").read().splitlines()
head = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([a-z0-9_]+)")
attr = re.compile(r"^\s*#!?\[")
doc = re.compile(r"^\s*///")
raw = re.compile(r'(?:b|br)?r(#*)"')
normal_str = re.compile(r'b?"')


def end_of_block(start):
    depth = 0
    seen_open = False
    in_block = False
    in_str = False
    raw_hashes = -1
    i = start
    while i < len(lines):
        line = lines[i]
        j = 0
        while j < len(line):
            ch = line[j]
            if in_block:
                if ch == "*" and line[j + 1:j + 2] == "/":
                    in_block = False
                    j += 2
                    continue
                j += 1
                continue
            if raw_hashes >= 0:
                if ch == '"' and line[j + 1:j + 1 + raw_hashes] == "#" * raw_hashes:
                    j += 1 + raw_hashes
                    raw_hashes = -1
                    continue
                j += 1
                continue
            if in_str:
                if ch == "\\":
                    j += 2
                    continue
                if ch == '"':
                    in_str = False
                j += 1
                continue
            if ch == "/" and line[j + 1:j + 2] == "/":
                break
            if ch == "/" and line[j + 1:j + 2] == "*":
                in_block = True
                j += 2
                continue
            m = raw.match(line, j)
            if m:
                raw_hashes = len(m.group(1))
                j = m.end()
                continue
            m = normal_str.match(line, j)
            if m:
                in_str = True
                j = m.end()
                continue
            if ch == "'":
                k = j + 1
                if k < len(line) and line[k] == "\\":
                    k += 2
                else:
                    k += 1
                if k < len(line) and line[k] == "'":
                    j = k + 1
                    continue
                j += 1
                continue
            if ch == "{":
                depth += 1
                seen_open = True
            elif ch == "}":
                depth -= 1
                if seen_open and depth == 0:
                    return i
            j += 1
        i += 1
    return len(lines) - 1


def head_start(i):
    start = i
    j = i - 1
    while j >= 0:
        if doc.match(lines[j]) or attr.match(lines[j]):
            start = j
            j -= 1
            continue
        break
    return start


for i, line in enumerate(lines):
    m = head.match(line)
    if not m:
        continue
    print(f"{m.group(1)}\t{head_start(i) + 1}\t{end_of_block(i) + 1}")
PY
}

# Extra proofs (not the core set) selected by the branch's graded changes.
# Output: <proof> TAB <reason>.
mapped_ready_proofs() {
    local repo="$1" base="${2:-}"
    if [[ -z "$base" ]]; then
        base="$(git -C "$repo" merge-base HEAD master 2>/dev/null || true)"
    fi
    if [[ -z "$base" ]]; then
        local proof
        while IFS= read -r proof; do
            printf '%s\tno merge-base: full set\n' "$proof"
        done < <(ready_full_set "$repo")
        return 0
    fi
    local path name proof full start count last matched span s_start s_end
    local -a head_bar base_bar ready_set spans

    # R62 F2: a proof present at HEAD but not at the merge base is new (or
    # renamed). Run it even when the hunk that adds it is not attributed.
    mapfile -t head_bar < <(bar_proofs "$repo")
    mapfile -t base_bar < <(bar_proofs_at "$repo" "$base")
    mapfile -t ready_set < <(ready_full_set "$repo")
    for proof in "${head_bar[@]}"; do
        printf '%s\n' "${base_bar[@]}" | grep -qx "$proof" && continue
        printf '%s\n' "${ready_set[@]}" | grep -qx "$proof" || continue
        printf '%s\tnew proof\n' "$proof"
    done

    while IFS= read -r path; do
        [[ -z "$path" ]] && continue
        if [[ "$path" == "src/pipeline/tests.rs" ]]; then
            # R62 F1: a hunk belongs to every top-level fn whose span (doc
            # comment and attributes through the closing brace) contains
            # its new lines. The hunk header alone is wrong for async fn,
            # for a doc comment or signature, and for a hunk spanning two
            # functions.
            full=0
            mapfile -t spans < <(fn_spans "$repo/src/pipeline/tests.rs")
            while IFS=' ' read -r start count; do
                [[ -n "$start" ]] || continue
                local last="$start"
                [[ "$count" -gt 0 ]] && last=$((start + count - 1))
                matched=0
                for span in "${spans[@]}"; do
                    IFS=$'\t' read -r name s_start s_end <<<"$span"
                    if ((start <= s_end && last >= s_start)); then
                        matched=1
                        if ! bar_proofs "$repo" | grep -qx "$name"; then
                            full=1
                            break 2
                        fi
                        # A proof the fast set already ran needs no extra run.
                        if ! ready_full_set "$repo" | grep -qx "$name"; then
                            continue
                        fi
                        printf '%s\tmapped: %s (hunk)\n' "$name" "$path"
                    fi
                done
                if [[ "$matched" -eq 0 ]]; then
                    full=1
                    break
                fi
            done < <(git -C "$repo" diff -U0 "$base" HEAD -- "$path" \
                | sed -nE 's/^@@ -[0-9]+(,[0-9]+)? \+([0-9]+)(,([0-9]+))? @@.*/\2 \4/p')
            if [[ "$full" -eq 1 ]]; then
                while IFS= read -r proof; do
                    printf '%s\tunattributed hunk: full set\n' "$proof"
                done < <(ready_full_set "$repo")
            fi
        else
            while IFS= read -r proof; do
                [[ -z "$proof" ]] && continue
                printf '%s\tmapped: %s\n' "$proof" "$path"
            done < <(proofs_for_path "$repo" "$path")
        fi
    done < <(changed_graded_paths "$repo" "$base")
}

# The full `--ready` set: the core proofs plus the mapped ones, one reason
# each (core wins when both apply).
select_ready_proofs() {
    local repo="$1" base="${2:-}" proof reason
    local -A mapped=() unattributed=() new=() core=()
    while IFS=$'\t' read -r proof reason; do
        [[ -z "$proof" ]] && continue
        case "$reason" in
            core) core[$proof]=1 ;;
            "new proof") new[$proof]=1 ;;
            "unattributed hunk: full set") unattributed[$proof]=1 ;;
            *) [[ -n "${mapped[$proof]:-}" ]] || mapped[$proof]="$reason" ;;
        esac
    done < <(mapped_ready_proofs "$repo" "$base")
    while IFS= read -r proof; do
        [[ -z "$proof" ]] && continue
        core[$proof]=1
    done < <(core_proofs)
    {
        for proof in "${!core[@]}"; do
            printf '%s\tcore\n' "$proof"
        done
        for proof in "${!new[@]}"; do
            [[ -n "${core[$proof]:-}" ]] && continue
            printf '%s\tnew proof\n' "$proof"
        done
        for proof in "${!unattributed[@]}"; do
            [[ -n "${core[$proof]:-}" || -n "${new[$proof]:-}" ]] && continue
            printf '%s\tunattributed hunk: full set\n' "$proof"
        done
        for proof in "${!mapped[@]}"; do
            [[ -n "${core[$proof]:-}" || -n "${unattributed[$proof]:-}" \
                || -n "${new[$proof]:-}" ]] && continue
            printf '%s\t%s\n' "$proof" "${mapped[$proof]}"
        done
    } | sort
}

self_test() {
    local dir repo failed=0 line glob proof live name
    dir="$(mktemp -d /tmp/sdb-proof-map-self-test.XXXXXX)"
    trap "rm -rf '$dir'" EXIT
    repo="$dir/repo"

    # 1) Every graded file has a deliberate map entry.
    mapfile -t graded_files < <(
        cd "$PROOF_MAP_ROOT" || exit 1
        mapfile -t paths < <(graded_path_entries)
        git ls-files -- "${paths[@]}" | graded_filter
    )
    for name in "${graded_files[@]}"; do
        path_is_mapped "$name" || {
            echo "[proof-map] no entry for $name" >&2
            failed=1
        }
    done
    [[ $failed -eq 0 ]] || return 1
    echo "[self-test] every graded file has an entry"

    # 2) Every proof named in the map is in the bar's list and in
    # live-proofs.sh (the ready gate runs only the latter).
    mapfile -t bar < <(bar_proofs "$PROOF_MAP_ROOT")
    mapfile -t live < <(cd "$PROOF_MAP_ROOT" && scripts/live-proofs.sh)
    while IFS= read -r line; do
        glob="${line%% *}"
        oldifs="$IFS"
        IFS=','
        for proof in ${line#* }; do
            case "$proof" in
                core|full) ;;
                *)
                    printf '%s\n' "${bar[@]}" | grep -qx "$proof" || {
                        echo "[proof-map] $glob names $proof, which the bar does not run" >&2
                        failed=1
                    }
                    printf '%s\n' "${live[@]}" | grep -qx "$proof" || {
                        echo "[proof-map] $glob names $proof, which live-proofs.sh does not run" >&2
                        failed=1
                    } ;;
            esac
        done
        IFS="$oldifs"
    done < <(proof_map_entries)
    [[ $failed -eq 0 ]] || return 1
    echo "[self-test] every mapped proof is in the bar and live-proofs lists"

    # 3) Every core proof is in the live-proofs.sh skip list.
    while IFS= read -r proof; do
        printf '%s\n' "${live[@]}" | grep -qx "$proof" || {
            echo "[proof-map] core proof $proof is not in live-proofs.sh" >&2
            failed=1
        }
    done < <(core_proofs)
    [[ $failed -eq 0 ]] || return 1
    echo "[self-test] every core proof is in live-proofs.sh"

    # 4) Fixture diffs: the map selects what the change puts at risk.
    mkdir -p "$repo/src/pipeline" "$repo/src/verifier/sql" "$repo/docs" "$repo/scripts"
    cp "$PROOF_MAP_ROOT/scripts/graded-paths.txt" \
        "$PROOF_MAP_ROOT/scripts/proof-map.txt" \
        "$PROOF_MAP_ROOT/scripts/ready-proofs.txt" \
        "$PROOF_MAP_ROOT/scripts/graded-filter.sh" \
        "$PROOF_MAP_ROOT/scripts/bar-proofs.sh" \
        "$PROOF_MAP_ROOT/scripts/live-proofs.sh" \
        "$PROOF_MAP_ROOT/scripts/proof-map.sh" "$repo/scripts/"
    printf '*.rs diff=rust\n' >"$repo/.gitattributes"
    printf '# docs\n' >"$repo/docs/readme.md"
    printf 'pub fn concurrency() {}\n' >"$repo/src/verifier/sql/concurrency.rs"
    : >"$repo/src/pipeline.rs"
    : >"$repo/src/main.rs"
    {
        while IFS= read -r proof; do
            case "$proof" in
                r45_f1_reference_scores_one)
                    printf '/// doc for %s\nfn %s() {\n    let _serial = SQL_PERF_LIVE.lock();\n    let x = 1;\n}\n\n' \
                        "$proof" "$proof" ;;
                # R62 F1: the lite proof ends on the line before the live
                # proof's doc comment, so one two-line hunk spans both.
                test_pipeline_eval_only_sql_stage12_concurrency_lite_live)
                    printf '/// doc for %s\nfn %s() {\n    let _serial = SQL_PERF_LIVE.lock();\n    let x = 1;\n}\n' \
                        "$proof" "$proof" ;;
                test_pipeline_eval_only_sql_stage12_concurrency_live)
                    printf '/// doc for %s\nasync fn %s() {\n    let _serial = SQL_PERF_LIVE.lock();\n    let x = 1;\n}\n\n' \
                        "$proof" "$proof" ;;
                *)
                    if printf '%s\n' "${live[@]}" | grep -qx "$proof"; then
                        printf 'fn %s() { let _serial = SQL_PERF_LIVE.lock(); }\n\n' "$proof"
                    else
                        printf 'fn %s() {}\n\n' "$proof"
                    fi ;;
            esac
        done < <(bar_proofs "$PROOF_MAP_ROOT")
        # The two live proofs outside the bar (r54, the tests/CI gate) still
        # belong to the full ready set.
        while IFS= read -r proof; do
            printf '%s\n' "${bar[@]}" | grep -qx "$proof" && continue
            printf 'fn %s() { let _serial = SQL_PERF_LIVE.lock(); }\n\n' "$proof"
        done < <(printf '%s\n' "${live[@]}")
    } >"$repo/src/pipeline/tests.rs"
    printf 'fn helper() {\n    let x = 1;\n}\n' >>"$repo/src/pipeline/tests.rs"
    git -C "$repo" init -q -b master
    git -C "$repo" config user.email proof-map@example.com
    git -C "$repo" config user.name proof-map
    git -C "$repo" add -A
    git -C "$repo" commit -qm base
    git -C "$repo" checkout -qb round

    # 4a) concurrency.rs -> both stage-12 proofs (and, in the full set, core).
    printf 'pub fn concurrency() { let _ = 2; }\n' >"$repo/src/verifier/sql/concurrency.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm concurrency
    mapfile -t got < <(mapped_ready_proofs "$repo" master)
    printf '%s\n' "${got[@]}" | grep -q $'^test_pipeline_eval_only_sql_stage12_concurrency_live\t' || {
        echo "[self-test] concurrency.rs did not select stage12 live" >&2
        printf '%s\n' "${got[@]}" >&2
        failed=1
    }
    printf '%s\n' "${got[@]}" | grep -q $'^test_pipeline_eval_only_sql_stage12_concurrency_lite_live\t' || {
        echo "[self-test] concurrency.rs did not select stage12 lite" >&2
        printf '%s\n' "${got[@]}" >&2
        failed=1
    }
    [[ ${#got[@]} -eq 2 ]] || {
        echo "[self-test] concurrency.rs selected ${#got[@]} proofs, want 2" >&2
        failed=1
    }
    select_ready_proofs "$repo" master | grep -q $'^r49_f2_unsynced_candidate_floors_live\tcore$' || {
        echo "[self-test] the ready set lost its core proofs" >&2
        failed=1
    }
    [[ $failed -eq 0 ]] || return 1
    echo "[self-test] concurrency.rs selects the two stage-12 proofs"

    # 4b) a hunk inside an async fn proof selects that proof (R61-6).
    git -C "$repo" reset -q --hard master
    sed -i '/async fn test_pipeline_eval_only_sql_stage12_concurrency_live()/,/^}/ s/let x = 1;/let x = 2;/' \
        "$repo/src/pipeline/tests.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm async-body
    mapfile -t got < <(mapped_ready_proofs "$repo" master)
    [[ ${#got[@]} -eq 1 && "${got[0]%%$'\t'*}" == "test_pipeline_eval_only_sql_stage12_concurrency_live" ]] || {
        echo "[self-test] an async proof-body hunk did not select just that proof" >&2
        printf '%s\n' "${got[@]}" >&2
        return 1
    }
    echo "[self-test] an async proof-body hunk selects its proof"

    # 4c) a hunk on a proof's doc comment selects that proof, not the one
    # before it.
    git -C "$repo" reset -q --hard master
    sed -i 's|^/// doc for test_pipeline_eval_only_sql_stage12_concurrency_live$|/// changed doc|' \
        "$repo/src/pipeline/tests.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm doc-comment
    mapfile -t got < <(mapped_ready_proofs "$repo" master)
    [[ ${#got[@]} -eq 1 && "${got[0]%%$'\t'*}" == "test_pipeline_eval_only_sql_stage12_concurrency_live" ]] || {
        echo "[self-test] a doc-comment hunk selected the wrong proof" >&2
        printf '%s\n' "${got[@]}" >&2
        return 1
    }
    echo "[self-test] a doc-comment hunk selects its proof"

    # 4d) one hunk spanning two proofs selects both.
    git -C "$repo" reset -q --hard master
    sed -i '/fn test_pipeline_eval_only_sql_stage12_concurrency_lite_live()/,/^}$/ s/^}$/} \/\/ changed/' \
        "$repo/src/pipeline/tests.rs"
    sed -i 's|^/// doc for test_pipeline_eval_only_sql_stage12_concurrency_live$|/// changed doc|' \
        "$repo/src/pipeline/tests.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm two-proofs
    [[ "$(git -C "$repo" diff -U0 master HEAD -- src/pipeline/tests.rs | grep -c '^@@')" -eq 1 ]] || {
        echo "[self-test] the two-proof fixture is not one hunk" >&2
        return 1
    }
    mapfile -t got < <(mapped_ready_proofs "$repo" master)
    [[ ${#got[@]} -eq 2 ]] || {
        echo "[self-test] the two-proof hunk selected ${#got[@]} proofs, want 2" >&2
        printf '%s\n' "${got[@]}" >&2
        return 1
    }
    printf '%s\n' "${got[@]}" | grep -q $'^test_pipeline_eval_only_sql_stage12_concurrency_lite_live\t' || {
        echo "[self-test] the two-proof hunk missed stage12 lite" >&2
        return 1
    }
    printf '%s\n' "${got[@]}" | grep -q $'^test_pipeline_eval_only_sql_stage12_concurrency_live\t' || {
        echo "[self-test] the two-proof hunk missed stage12 live" >&2
        return 1
    }
    echo "[self-test] one hunk selects both proofs it spans"

    # 4e) a helper outside any proof selects the full set.
    git -C "$repo" reset -q --hard master
    sed -i '/fn helper()/,/^}/ s/let x = 1;/let x = 2;/' "$repo/src/pipeline/tests.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm helper
    mapfile -t got < <(mapped_ready_proofs "$repo" master)
    [[ ${#got[@]} -eq ${#live[@]} ]] || {
        echo "[self-test] a helper hunk selected ${#got[@]} proofs, want ${#live[@]}" >&2
        return 1
    }
    echo "[self-test] a helper hunk selects the full set"

    # 4f) a docs-only change selects no mapped proof.
    git -C "$repo" reset -q --hard master
    printf '# docs changed\n' >"$repo/docs/readme.md"
    git -C "$repo" add -A
    git -C "$repo" commit -qm docs
    mapfile -t got < <(mapped_ready_proofs "$repo" master)
    [[ ${#got[@]} -eq 0 ]] || {
        echo "[self-test] a docs-only change selected ${#got[@]} proofs" >&2
        return 1
    }
    echo "[self-test] a docs-only change selects nothing"

    # 4g) a proof added since the merge base is new and runs (R62 F2).
    git -C "$repo" reset -q --hard master
    python3 - "$repo/src/pipeline/tests.rs" <<'PY'
import sys

path = sys.argv[1]
s = open(path).read()
needle = "fn r45_f1_reference_scores_one() {"
start = s.index(needle)
end = s.index("\n}\n", start) + 3
new = (
    "\nasync fn test_pipeline_eval_only_sql_stage12_extra_live() {\n"
    "    let _serial = SQL_PERF_LIVE.lock();\n"
    "}\n"
)
open(path, "w").write(s[:end] + new + s[end:])
PY
    git -C "$repo" add -A
    git -C "$repo" commit -qm new-proof
    mapfile -t got < <(mapped_ready_proofs "$repo" master)
    printf '%s\n' "${got[@]}" | grep -q $'^test_pipeline_eval_only_sql_stage12_extra_live\tnew proof$' || {
        echo "[self-test] a new proof was not selected as new proof" >&2
        printf '%s\n' "${got[@]}" >&2
        return 1
    }
    echo "[self-test] a new proof is selected as new proof"

    echo "RESULT: PASS (proof-map self-test)"
}

if [[ "${BASH_SOURCE[0]}" == "$0" && "${1:-}" == "--self-test" ]]; then
    self_test
    exit $?
fi
