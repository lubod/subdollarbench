#!/usr/bin/env bash
# Publish the sdb-sql career grid.
#
#   scripts/publish.sh              # export + checked dry run
#   scripts/publish.sh --yes        # export + checked rsync
#   scripts/publish.sh --self-test  # fixture checks, no network
#
# The target and public URL come from SDB_PUBLISH_TARGET and
# SDB_PUBLISH_URL (environment or .env). Only those two values are ever
# printed; the rest of .env is never sourced or echoed.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 1

mode=dry
self_test=0
key_only=0
bar_only=0
check_dir=""
check_bar=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --yes) mode=push; shift ;;
        --self-test) self_test=1; shift ;;
        --key) key_only=1; shift ;;
        --bar) bar_only=1; shift ;;
        --check)
            [[ $# -ge 3 ]] || { echo '[publish] --check needs a directory and a bar record' >&2; exit 2; }
            check_dir="$2"; check_bar="$3"; shift 3 ;;
        -h|--help)
            sed -n '1,12p' "$0"; exit 0 ;;
        *) echo "[publish] unknown argument: $1" >&2; exit 2 ;;
    esac
done

env_value() {
    local key="$1"
    if [[ -n "${!key:-}" ]]; then
        printf '%s' "${!key}"
    elif [[ -f .env ]]; then
        sed -n "s/^[[:space:]]*${key}=//p" .env | tail -1 | tr -d '\r'
    fi
}

# R65 F4: one key computation for the bar, the cell, the grid and the
# publish. This used to hash `graded-paths.txt` itself, including its
# `exclude-name`/`keep` data lines and `tests.rs`, so every publish asked
# for a record no bar ever wrote.
#
# R68 F1: the bar record is keyed by the grader paths only; `--key` stays
# the full key. A plumbing change moves the full key but needs no bar.
flake_key() {
    "$PWD/scripts/flake-bar.sh" --key
}

grader_key() {
    "$PWD/scripts/flake-bar.sh" --grader-key
}

bar_record() {
    if [[ -n "${SDB_BAR_RECORD:-}" ]]; then
        printf '%s' "$SDB_BAR_RECORD"
    else
        printf '%s/sdb-gates/flake-%s.txt' \
            "$(git rev-parse --path-format=absolute --git-common-dir)" "$(grader_key)"
    fi
}

# R51 F2 (R44-10): the binary owns the secret list. In a self-test or a
# fresh checkout without a release build, build the debug binary once.
secret_scan_bin() {
    if [[ -n "${SDB_BIN:-}" && -x "$SDB_BIN" ]]; then
        printf '%s' "$SDB_BIN"
        return 0
    fi
    if [[ -x "$PWD/target/release/subdollarbench" ]]; then
        printf '%s' "$PWD/target/release/subdollarbench"
        return 0
    fi
    if cargo build --quiet --bin subdollarbench >/dev/null 2>&1; then
        printf '%s' "$PWD/target/debug/subdollarbench"
        return 0
    fi
    return 1
}

validate_export() {
    local dir="$1" bar="$2" rc=0
    [[ -s "$dir/index.html" ]] || { echo "[publish] refusal: index.html missing" >&2; return 1; }
    [[ -s "$dir/results.html" ]] || { echo "[publish] refusal: results.html missing" >&2; return 1; }
    [[ -s "$dir/grid.json" ]] || { echo "[publish] refusal: grid.json missing" >&2; return 1; }
    # R71 F1 (R70-2): the public pages stay under 150 KB each.
    local page bytes
    for page in index.html results.html; do
        bytes=$(wc -c < "$dir/$page")
        if (( bytes > 150 * 1024 )); then
            echo "[publish] refusal: $page is $bytes bytes, over the 150 KB cap" >&2
            return 1
        fi
    done
    [[ -z "$(find "$dir" -type d -name runs -print -quit)" ]] || {
        echo "[publish] refusal: export contains runs/ logs or workspaces" >&2
        return 1
    }
    python3 - "$dir/grid.json" <<'PY' || return 1
import json, sys
v = json.load(open(sys.argv[1]))
rules = v.get("ranked_rules") or []
current = v.get("current_rules")
if not rules:
    print("[publish] refusal: no ranked careers", file=sys.stderr)
    raise SystemExit(1)
if len(rules) != 1 or rules[0] != current:
    print("[publish] refusal: ranked careers are not all on the current rules: %r" % (rules,), file=sys.stderr)
    raise SystemExit(1)
if not int(v.get("ranked") or 0) > 0:
    print("[publish] refusal: no ranked careers", file=sys.stderr)
    raise SystemExit(1)
if int(v.get("ranked_rehearsal") or 0) > 0:
    print("[publish] refusal: a rehearsal or smoke cell is in the ranked set", file=sys.stderr)
    raise SystemExit(1)
# R75 F5: no empty board. An export with fewer than 3 models whose
# current-rules careers all finished is refused, so the public site keeps
# the last full bundle. An older binary has no ranked_models field; its
# models list is the whole export, and its export is the current board.
models = v.get("ranked_models")
if models is None:
    models = v.get("models") or []
if not isinstance(models, list) or len(models) < 3:
    print("[publish] refusal: %d model(s) with all their careers finished under %s (need 3): %r"
          % (len(models), current, models), file=sys.stderr)
    raise SystemExit(1)
PY
    [[ -f "$bar" ]] || { echo "[publish] refusal: flake record missing ($bar)" >&2; return 1; }
    grep -q '^RESULT: PASS' "$bar" || {
        echo "[publish] refusal: flake record has no RESULT: PASS" >&2
        return 1
    }
    # R74 F3: the run audit gates the export; a waiver line closes a
    # finding and is printed.
    local audit_out audit_rc=0
    audit_out="$(scripts/run-audit.sh --check --export "$dir" 2>&1)" || audit_rc=$?
    printf '%s\n' "$audit_out"
    if [[ "$audit_rc" -ne 0 ]]; then
        echo "[publish] refusal: the run audit has open findings" >&2
        return 1
    fi
    local scanner out
    scanner="$(secret_scan_bin)" || {
        echo "[publish] refusal: no binary for the secret scan" >&2
        return 1
    }
    out="$(mktemp)"
    if ! "$scanner" portal --secret-scan "$dir" >"$out" 2>&1; then
        echo "[publish] refusal: secret scan found a match" >&2
        sed -n '1,8p' "$out" >&2
        rm -f "$out"
        return 1
    fi
    rm -f "$out"
    return "$rc"
}

# R44-9: a mistyped target must never be wiped by --delete. The target
# carries a marker the export writes too, so a normal --delete keeps it.
target_marker() {
    local target="$1"
    if [[ "$target" == *:* && "$target" != /* ]]; then
        local host="${target%%:*}" path="${target#*:}"
        ssh -o BatchMode=yes "$host" "test -f $(printf '%q' "$path")/.sdb-publish" 2>/dev/null
    else
        [[ -f "$target/.sdb-publish" ]]
    fi
}

deploy() {
    local dir="$1" target="$2" how="$3"
    if [[ "$how" == "push" ]]; then
        if ! target_marker "$target"; then
            echo "[publish] refusal: the target has no .sdb-publish marker; refusing to rsync --delete there" >&2
            echo "[publish] create it first: ssh <host> 'touch <path>/.sdb-publish' (local: touch <dir>/.sdb-publish)" >&2
            return 1
        fi
        rsync -a --delete "$dir/" "$target"
    else
        local listing
        listing="$(rsync -an --delete --itemize-changes "$dir/" "$target")"
        printf '[publish] dry run (rsync -n):\n%s\n' "$listing"
        # A file in the export's own set is never deleted; any *deleting
        # line is therefore outside it and means the target is not ours.
        if grep -q '^\*deleting[[:space:]]' <<<"$listing"; then
            echo "[publish] refusal: the target holds files this export would delete:" >&2
            grep '^\*deleting[[:space:]]' <<<"$listing" >&2
            return 1
        fi
    fi
}

if [[ "$key_only" -eq 1 ]]; then
    flake_key
    exit 0
fi
if [[ "$bar_only" -eq 1 ]]; then
    bar_record
    exit 0
fi

if [[ -n "$check_dir" ]]; then
    validate_export "$check_dir" "$check_bar"
    exit $?
fi

if [[ "$self_test" -eq 1 ]]; then
    root="$(mktemp -d /tmp/sdb-publish-self-test.XXXXXX)"
    trap 'rm -rf "$root"' EXIT
    good="$root/good"
    target="$root/target"
    mkdir -p "$good" "$target"
    printf '<html>board</html>\n' > "$good/index.html"
    printf '<html>results</html>\n' > "$good/results.html"
    printf '{"careers":3,"ranked":3,"current_rules":"2026-10-19.42","ranked_rules":["2026-10-19.42"],"ranked_models":["m-one","m-two","m-three"],"fingerprint":"fp-binary","ranked_fingerprints":["fp-one"],"fingerprint_careers":{"fp-one":3}}\n' > "$good/grid.json"
    bar="$root/flake.txt"
    printf 'RESULT: PASS\n' > "$bar"

    validate_export "$good" "$bar" || { echo '[self-test] good export rejected' >&2; exit 1; }
    echo '[self-test] good export accepted'

    # R71 F1: a 151 KB page is refused, naming the page and its size.
    big="$root/big"
    cp -a "$good" "$big"
    python3 - "$big/index.html" <<'PY'
import sys
open(sys.argv[1], "w").write("x" * (151 * 1024))
PY
    rc=0
    out="$(validate_export "$big" "$bar" 2>&1)" || rc=$?
    [[ "$rc" -ne 0 ]] || { echo '[self-test] 151 KB index.html accepted' >&2; exit 1; }
    grep -q 'index.html' <<<"$out" && grep -q '154624' <<<"$out" || {
        echo "[self-test] the size refusal did not name the page and size: $out" >&2
        exit 1
    }
    echo '[self-test] 151 KB index.html refused'

    mixed="$root/mixed"
    cp -a "$good" "$mixed"
    python3 - "$mixed/grid.json" <<'PY'
import json,sys
p=sys.argv[1]; v=json.load(open(p))
v["ranked_fingerprints"]=["fp-one","fp-two"]
json.dump(v,open(p,"w"))
PY
    validate_export "$mixed" "$bar" || {
        echo '[self-test] two fingerprints under one rules version were refused' >&2; exit 1; }
    echo '[self-test] two fingerprints accepted'

    rules="$root/rules"
    cp -a "$good" "$rules"
    python3 - "$rules/grid.json" <<'PY'
import json,sys
p=sys.argv[1]; v=json.load(open(p))
v["ranked_rules"]=["2026-10-19.42","2026-10-19.43"]
json.dump(v,open(p,"w"))
PY
    if validate_export "$rules" "$bar" >/dev/null 2>&1; then
        echo '[self-test] mixed rules versions were accepted' >&2; exit 1
    fi
    echo '[self-test] mixed rules versions refused'

    # R75 F5: 2 complete models refuse, 3 pass (the good export).
    two="$root/two-models"
    cp -a "$good" "$two"
    python3 - "$two/grid.json" <<'PY'
import json,sys
p=sys.argv[1]; v=json.load(open(p))
v["ranked_models"]=["m-one","m-two"]
json.dump(v,open(p,"w"))
PY
    rc=0
    out="$(validate_export "$two" "$bar" 2>&1)" || rc=$?
    [[ "$rc" -ne 0 ]] || { echo '[self-test] 2 complete models were accepted' >&2; exit 1; }
    grep -q '2 model(s) with all their careers finished' <<<"$out" || {
        echo "[self-test] the 2-model refusal did not name the count: $out" >&2
        exit 1
    }
    echo '[self-test] 2 complete models refused, 3 pass'

    rehearsal="$root/rehearsal"
    cp -a "$good" "$rehearsal"
    python3 - "$rehearsal/grid.json" <<'PY'
import json,sys
p=sys.argv[1]; v=json.load(open(p))
v["ranked_rehearsal"]=1
json.dump(v,open(p,"w"))
PY
    if validate_export "$rehearsal" "$bar" >/dev/null 2>&1; then
        echo '[self-test] ranked rehearsal was accepted' >&2; exit 1
    fi
    echo '[self-test] ranked rehearsal refused'

    if validate_export "$good" "$root/no-such-bar" >/dev/null 2>&1; then
        echo '[self-test] missing bar was accepted' >&2; exit 1
    fi
    echo '[self-test] missing bar refused'

    # R74 F3: an open audit finding refuses; a waiver closes it and the
    # waiver line is printed.
    audit_runs="$root/audit-runs"
    audit_career="campaign_audit_bad_20261109_000000_w1"
    mkdir -p "$audit_runs/$audit_career"
    python3 - "$audit_runs/$audit_career/campaign.json" "$audit_career" <<'PY'
import json, sys
path, cid = sys.argv[1:3]
json.dump({
    "campaign_id": cid, "model": "audit", "phase": 1, "points": 8.0,
    "total_cost_usd": 1.0, "billed_usd": 1.0,
    "health": {"turns": 73, "tool_errors": 25, "ref_queries": 0,
               "ref_refused": 0, "empty_responses": 0, "billed_ratio": 1.0},
    "career": {"cells": [cid], "points": 8.0},
}, open(path, "w"))
PY
    audit_export="$root/audit-export"
    cp -a "$good" "$audit_export"
    printf '<html><a href="results.html#%s">x</a></html>\n' "$audit_career" \
        > "$audit_export/index.html"
    if ( export SDB_AUDIT_RUNS_DIR="$audit_runs" SDB_AUDIT_WAIVERS="$root/no-waiver"; \
        validate_export "$audit_export" "$bar" >/dev/null 2>&1 ); then
        echo '[self-test] an open audit finding was accepted' >&2; exit 1
    fi
    echo '[self-test] open audit finding refused'
    printf 'audit | tool-errors | operator | test\n' > "$root/audit-waivers.txt"
    out="$(export SDB_AUDIT_RUNS_DIR="$audit_runs" SDB_AUDIT_WAIVERS="$root/audit-waivers.txt"; \
        validate_export "$audit_export" "$bar" 2>&1)" || {
        echo '[self-test] a waived audit finding was refused' >&2
        printf '%s\n' "$out" >&2
        exit 1
    }
    grep -q '\[audit\] waiver:' <<<"$out" || {
        echo '[self-test] the audit waiver line was not printed' >&2
        printf '%s\n' "$out" >&2
        exit 1
    }
    echo '[self-test] waived audit finding passes and prints the waiver'

    while IFS='|' read -r planted value; do
        secret="$root/secret"
        rm -rf "$secret"
        cp -a "$good" "$secret"
        printf '%s\n' "$planted" > "$secret/note.txt"
        rc=0
        out="$(validate_export "$secret" "$bar" 2>&1)" || rc=$?
        [[ "$rc" -ne 0 ]] || {
            echo "[self-test] planted secret was accepted: $planted" >&2; exit 1
        }
        case "$out" in
            *"$value"*) echo '[self-test] the scan printed the secret value' >&2; exit 1 ;;
        esac
    done <<'EOF'
sk-or-v1-abcdefghijklmnop|abcdefghijklmnop
authorization: bearer bearerabc123XYZ|bearerabc123XYZ
OPENROUTER_API_KEY=sk-live-value|sk-live-value
EOF
    echo '[self-test] planted secrets refused without values'

    # R44-9: the export writes the marker; an unmarked target refuses a
    # push and is left untouched.
    printf 'sdb-publish\n' > "$good/.sdb-publish"
    if deploy "$good" "$target" push >/dev/null 2>&1; then
        echo '[self-test] an unmarked target was pushed' >&2; exit 1
    fi
    [[ -z "$(find "$target" -mindepth 1 -print -quit)" ]] || {
        echo '[self-test] a refused push wrote to the target' >&2; exit 1
    }
    echo '[self-test] unmarked target refused'
    printf 'sdb-publish\n' > "$target/.sdb-publish"
    deploy "$good" "$target" push
    [[ -f "$target/index.html" && -f "$target/results.html" && -f "$target/.sdb-publish" ]] || {
        echo '[self-test] --yes did not copy (or removed the marker)' >&2; exit 1
    }
    echo '[self-test] marked target pushed'
    printf 'stray\n' > "$target/stray.txt"
    rc=0
    if out="$(deploy "$good" "$target" dry 2>&1)"; then
        rc=0
    else
        rc=$?
    fi
    [[ "$rc" -ne 0 ]] || { echo '[self-test] stray target file was accepted' >&2; exit 1; }
    grep -q '\*deleting[[:space:]][[:space:]]*stray.txt' <<<"$out" || {
        echo '[self-test] dry run did not list the stray deletion' >&2; exit 1
    }
    [[ -f "$target/stray.txt" ]] || { echo '[self-test] dry run deleted' >&2; exit 1; }
    echo '[self-test] stray target file listed and refused'

    # R65 F4/R68 F1: a fixture repo. publish --key is the full key; the
    # default bar record is the grader key's. A plumbing edit moves the
    # full key and leaves the bar record where it is; a grader edit moves
    # the bar record too.
    fixture="$root/repo"
    mkdir -p "$fixture/src/cost" "$fixture/src/verifier" \
        "$fixture/src/sandbox" "$fixture/scripts"
    cp scripts/publish.sh scripts/flake-bar.sh scripts/graded-paths.txt \
        scripts/graded-filter.sh scripts/bar-proofs.sh scripts/long-test-lock.sh \
        scripts/run-audit.sh "$fixture/scripts/"
    git -C "$fixture" init -q
    git -C "$fixture" config user.email publish@example.com
    git -C "$fixture" config user.name publish
    echo one > "$fixture/src/x.rs"
    echo "pub const P: u32 = 1;" > "$fixture/src/cost/x.rs"
    echo "pub fn v() {}" > "$fixture/src/verifier/x.rs"
    echo one > "$fixture/src/sandbox/tests.rs"
    echo lock > "$fixture/Cargo.lock"
    git -C "$fixture" add -A
    git -C "$fixture" commit -qm one
    echo two > "$fixture/src/sandbox/tests.rs"
    git -C "$fixture" add -A
    git -C "$fixture" commit -qm "edit tests.rs"
    pubkey="$(cd "$fixture" && scripts/publish.sh --key)"
    flakekey="$(cd "$fixture" && scripts/flake-bar.sh --key)"
    [[ "$pubkey" == "$flakekey" ]] || {
        echo "[self-test] publish key $pubkey != flake key $flakekey" >&2; exit 1; }
    graderkey="$(cd "$fixture" && scripts/flake-bar.sh --grader-key)"
    fixture_gates="$(cd "$fixture" && git rev-parse --path-format=absolute --git-common-dir)/sdb-gates"
    fixture_record="$fixture_gates/flake-$graderkey.txt"
    mkdir -p "$fixture_gates"
    printf 'RESULT: PASS\n' > "$fixture_record"
    fixture_bar="$(cd "$fixture" && scripts/publish.sh --bar)"
    [[ "$fixture_bar" == "$fixture_record" ]] || {
        echo "[self-test] default bar record $fixture_bar != $fixture_record" >&2; exit 1; }
    echo '[self-test] publish key matches the bar and its record validates'

    # A plumbing edit: the full key moves, the grader key and its bar
    # record stay.
    echo "pub const P: u32 = 2;" > "$fixture/src/cost/x.rs"
    git -C "$fixture" add -A
    git -C "$fixture" commit -qm plumbing
    plumbing_key="$(cd "$fixture" && scripts/publish.sh --key)"
    [[ "$plumbing_key" != "$pubkey" ]] || {
        echo '[self-test] a plumbing edit kept the full key' >&2; exit 1; }
    [[ "$(cd "$fixture" && scripts/flake-bar.sh --grader-key)" == "$graderkey" ]] || {
        echo '[self-test] a plumbing edit moved the grader key' >&2; exit 1; }
    [[ "$(cd "$fixture" && scripts/publish.sh --bar)" == "$fixture_record" ]] || {
        echo '[self-test] a plumbing edit moved the bar record' >&2; exit 1; }
    echo '[self-test] a plumbing edit keeps the bar record'

    # A grader edit: the grader key and the bar record move.
    echo "pub fn v() { let _ = 1; }" > "$fixture/src/verifier/x.rs"
    git -C "$fixture" add -A
    git -C "$fixture" commit -qm grader
    graderkey2="$(cd "$fixture" && scripts/flake-bar.sh --grader-key)"
    [[ "$graderkey2" != "$graderkey" ]] || {
        echo '[self-test] a grader edit kept the grader key' >&2; exit 1; }
    fixture_bar="$(cd "$fixture" && scripts/publish.sh --bar)"
    [[ "$fixture_bar" == "$fixture_gates/flake-$graderkey2.txt" ]] || {
        echo "[self-test] a grader edit did not move the bar record ($fixture_bar)" >&2
        exit 1; }
    printf 'RESULT: PASS\n' > "$fixture_bar"
    scan_bin="$(secret_scan_bin)"
    ( cd "$fixture" && SDB_BIN="$scan_bin" SDB_BAR_RECORD= \
        scripts/publish.sh --check "$good" "$fixture_bar" ) || {
        echo '[self-test] the keyed bar record was refused without SDB_BAR_RECORD' >&2; exit 1; }
    echo '[self-test] a grader edit moves the bar record and validates'
    # The pre-push hook lists every file for the secret scan; it never
    # reads graded-paths.txt, so it is not a key reader.
    needle="git ls-""tree"
    if grep -rn "$needle" scripts/ \
        | grep -vE '^scripts/(flake-bar|graded-filter)\.sh:|^scripts/hooks/'; then
        echo '[self-test] a private graded-path key computation remains' >&2; exit 1
    fi
    echo '[self-test] no private key computation remains'

    echo '[self-test] PASS'
    exit 0
fi

target="$(env_value SDB_PUBLISH_TARGET)"
url="$(env_value SDB_PUBLISH_URL)"
[[ -n "$target" ]] || { echo '[publish] refusal: SDB_PUBLISH_TARGET is not set' >&2; exit 1; }

bin="${SDB_BIN:-$PWD/target/release/subdollarbench}"
[[ -x "$bin" ]] || { echo "[publish] refusal: binary not found: $bin" >&2; exit 1; }
export="$(mktemp -d /tmp/sdb-publish.XXXXXX)"
trap 'rm -rf "$export"' EXIT
runs="${SDB_RUNS_DIR:-$PWD/runs}"
"$bin" portal --runs-dir "$runs" --out "$export" >/dev/null
printf 'sdb-publish\n' > "$export/.sdb-publish"
bar="$(bar_record)"
validate_export "$export" "$bar" || exit 1
echo "[publish] export checks passed"
du -sh "$export" | awk '{print "[publish] size: " $1}'
deploy "$export" "$target" "$mode"
if [[ -n "$url" ]]; then
    echo "[publish] $url"
else
    echo "[publish] SDB_PUBLISH_URL is unset"
fi
