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
check_dir=""
check_bar=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --yes) mode=push; shift ;;
        --self-test) self_test=1; shift ;;
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

flake_key() {
    local paths
    mapfile -t paths < <(grep -vE '^[[:space:]]*(#|$)' scripts/graded-paths.txt)
    git ls-tree -r HEAD -- "${paths[@]}" | sha256sum | cut -c1-12
}

bar_record() {
    if [[ -n "${SDB_BAR_RECORD:-}" ]]; then
        printf '%s' "$SDB_BAR_RECORD"
    else
        printf '%s/sdb-gates/flake-%s.txt' \
            "$(git rev-parse --git-common-dir)" "$(flake_key)"
    fi
}

validate_export() {
    local dir="$1" bar="$2" rc=0
    [[ -s "$dir/index.html" ]] || { echo "[publish] refusal: index.html missing" >&2; return 1; }
    [[ -s "$dir/results.html" ]] || { echo "[publish] refusal: results.html missing" >&2; return 1; }
    [[ -s "$dir/grid.json" ]] || { echo "[publish] refusal: grid.json missing" >&2; return 1; }
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
fps = v.get("ranked_fingerprints") or []
if len(fps) != 1 or fps[0] != v.get("fingerprint"):
    print("[publish] refusal: ranked careers do not share one fingerprint", file=sys.stderr)
    raise SystemExit(1)
if not int(v.get("ranked") or 0) > 0:
    print("[publish] refusal: no ranked careers", file=sys.stderr)
    raise SystemExit(1)
PY
    [[ -f "$bar" ]] || { echo "[publish] refusal: flake record missing ($bar)" >&2; return 1; }
    grep -q '^RESULT: PASS' "$bar" || {
        echo "[publish] refusal: flake record has no RESULT: PASS" >&2
        return 1
    }
    if grep -RInE \
        -e 'sk-or-v1-[A-Za-z0-9_-]+' \
        -e 'sk-proj-[A-Za-z0-9_-]+' \
        -e 'sk-ant-[A-Za-z0-9_-]+' \
        -e 'sk-[A-Za-z0-9_-]{8,}' \
        -e 'hf_[A-Za-z0-9_-]{16,}' \
        -e 'Bearer[[:space:]]+[A-Za-z0-9._~+/=-]+' \
        -e '(api_key|auth_token|access_token|token)[[:space:]]*[:=][[:space:]]*[^[:space:]]+' \
        -e '[A-Za-z0-9_]*(KEY|SECRET)[[:space:]]*=[[:space:]]*[^[:space:]]+' \
        -e '--api-key([[:space:]]+|=)[^[:space:]]+' \
        "$dir" >/tmp/sdb-publish-secret-scan.$$ 2>&1; then
        echo "[publish] refusal: secret scan found a match" >&2
        sed -n '1,8p' /tmp/sdb-publish-secret-scan.$$ >&2
        rm -f /tmp/sdb-publish-secret-scan.$$
        return 1
    fi
    rm -f /tmp/sdb-publish-secret-scan.$$
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
    printf '{"careers":3,"ranked":3,"current_rules":"2026-10-19.42","ranked_rules":["2026-10-19.42"],"fingerprint":"fp-one","ranked_fingerprints":["fp-one"]}\n' > "$good/grid.json"
    bar="$root/flake.txt"
    printf 'RESULT: PASS\n' > "$bar"

    validate_export "$good" "$bar" || { echo '[self-test] good export rejected' >&2; exit 1; }
    echo '[self-test] good export accepted'

    mixed="$root/mixed"
    cp -a "$good" "$mixed"
    python3 - "$mixed/grid.json" <<'PY'
import json,sys
p=sys.argv[1]; v=json.load(open(p))
v["ranked_fingerprints"]=["fp-one","fp-two"]
json.dump(v,open(p,"w"))
PY
    if validate_export "$mixed" "$bar" >/dev/null 2>&1; then
        echo '[self-test] mixed fingerprint was accepted' >&2; exit 1
    fi
    echo '[self-test] mixed fingerprint refused'

    if validate_export "$good" "$root/no-such-bar" >/dev/null 2>&1; then
        echo '[self-test] missing bar was accepted' >&2; exit 1
    fi
    echo '[self-test] missing bar refused'

    secret="$root/secret"
    cp -a "$good" "$secret"
    printf 'planted sk-or-v1-abcdefghijklmnop\n' > "$secret/note.txt"
    if validate_export "$secret" "$bar" >/dev/null 2>&1; then
        echo '[self-test] planted secret was accepted' >&2; exit 1
    fi
    echo '[self-test] planted sk-or string refused'

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
