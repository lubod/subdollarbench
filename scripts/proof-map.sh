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
                full) bar_proofs "$repo" ;;
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
        done < <(bar_proofs "$repo")
        return 0
    fi
    local path header name proof full
    while IFS= read -r path; do
        [[ -z "$path" ]] && continue
        if [[ "$path" == "src/pipeline/tests.rs" ]]; then
            # R61 F7: attribute each hunk by the fn in git's Rust hunk
            # header. A shared helper (or a whole-file add) is unattributed,
            # so the full set runs.
            full=0
            while IFS= read -r header; do
                name="$(sed -nE 's/^fn ([a-z0-9_]+).*/\1/p' <<<"$header")"
                if [[ -z "$name" ]] || ! bar_proofs "$repo" | grep -qx "$name"; then
                    full=1
                    break
                fi
                printf '%s\tmapped: %s (hunk)\n' "$name" "$path"
            done < <(git -C "$repo" diff -U0 "$base" HEAD -- "$path" \
                | sed -n 's/^@@[^@]*@@ *//p')
            if [[ "$full" -eq 1 ]]; then
                while IFS= read -r proof; do
                    printf '%s\tunattributed hunk: full set\n' "$proof"
                done < <(bar_proofs "$repo")
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
    local -A mapped=() unattributed=() core=()
    while IFS=$'\t' read -r proof reason; do
        [[ -z "$proof" ]] && continue
        case "$reason" in
            core) core[$proof]=1 ;;
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
        for proof in "${!unattributed[@]}"; do
            [[ -n "${core[$proof]:-}" ]] && continue
            printf '%s\tunattributed hunk: full set\n' "$proof"
        done
        for proof in "${!mapped[@]}"; do
            [[ -n "${core[$proof]:-}" || -n "${unattributed[$proof]:-}" ]] && continue
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

    # 2) Every proof named in the map is in the bar's list.
    mapfile -t bar < <(bar_proofs "$PROOF_MAP_ROOT")
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
                    } ;;
            esac
        done
        IFS="$oldifs"
    done < <(proof_map_entries)
    [[ $failed -eq 0 ]] || return 1
    echo "[self-test] every mapped proof is in the bar list"

    # 3) Every core proof is in the live-proofs.sh skip list.
    mapfile -t live < <(cd "$PROOF_MAP_ROOT" && scripts/live-proofs.sh)
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
        "$PROOF_MAP_ROOT/scripts/proof-map.sh" "$repo/scripts/"
    printf '*.rs diff=rust\n' >"$repo/.gitattributes"
    printf '# docs\n' >"$repo/docs/readme.md"
    printf 'pub fn concurrency() {}\n' >"$repo/src/verifier/sql/concurrency.rs"
    : >"$repo/src/pipeline.rs"
    {
        while IFS= read -r proof; do
            printf 'fn %s() {\n    let x = 1;\n}\n\n' "$proof"
        done < <(bar_proofs "$PROOF_MAP_ROOT")
        printf 'fn helper() {\n    let x = 1;\n}\n'
    } >"$repo/src/pipeline/tests.rs"
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

    # 4b) a hunk inside a proof body selects that proof.
    git -C "$repo" reset -q --hard master
    sed -i '/fn r45_f1_reference_scores_one()/,/^}/ s/let x = 1;/let x = 2;/' \
        "$repo/src/pipeline/tests.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm proof-body
    mapfile -t got < <(mapped_ready_proofs "$repo" master)
    [[ ${#got[@]} -eq 1 && "${got[0]%%$'\t'*}" == "r45_f1_reference_scores_one" ]] || {
        echo "[self-test] a proof-body hunk did not select just that proof" >&2
        printf '%s\n' "${got[@]}" >&2
        return 1
    }
    echo "[self-test] a proof-body hunk selects its proof"

    # 4c) a helper outside any proof selects the full set.
    git -C "$repo" reset -q --hard master
    sed -i '/fn helper()/,/^}/ s/let x = 1;/let x = 2;/' "$repo/src/pipeline/tests.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -qm helper
    mapfile -t got < <(mapped_ready_proofs "$repo" master)
    [[ ${#got[@]} -eq ${#bar[@]} ]] || {
        echo "[self-test] a helper hunk selected ${#got[@]} proofs, want ${#bar[@]}" >&2
        return 1
    }
    echo "[self-test] a helper hunk selects the full set"

    # 4d) a docs-only change selects no mapped proof.
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

    echo "RESULT: PASS (proof-map self-test)"
}

if [[ "${BASH_SOURCE[0]}" == "$0" && "${1:-}" == "--self-test" ]]; then
    self_test
    exit $?
fi
