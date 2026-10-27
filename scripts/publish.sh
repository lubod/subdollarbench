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
while [[ $# -gt 0 ]]; do
    case "$1" in
        --yes) mode=push; shift ;;
        --self-test) self_test=1; shift ;;
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

deploy() {
    local dir="$1" target="$2" how="$3"
    if [[ "$how" == "push" ]]; then
        rsync -a --delete "$dir/" "$target"
    else
        echo "[publish] dry run (rsync -n):"
        rsync -an --delete --itemize-changes "$dir/" "$target"
    fi
}

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

    deploy "$good" "$target" dry
    [[ -z "$(find "$target" -mindepth 1 -print -quit)" ]] || {
        echo '[self-test] dry run copied files' >&2; exit 1
    }
    echo '[self-test] dry run copied nothing'
    deploy "$good" "$target" push
    [[ -f "$target/index.html" && -f "$target/results.html" ]] || {
        echo '[self-test] --yes did not copy' >&2; exit 1
    }
    echo '[self-test] --yes copied'
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
