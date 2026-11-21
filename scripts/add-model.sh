#!/usr/bin/env bash
# R94 M3: add a model to the run-time registry from OpenRouter's endpoints.
#
#   scripts/add-model.sh <openrouter id> [--provider <tag>] [--dry-run]
#                        [--from-file <endpoints.json>] [--registry <path>]
#   scripts/add-model.sh --self-test
#
# The row stores the endpoint's **base** (undiscounted) prices: scoring is
# at base rates (DECISIONS #32), so a live discount only lowers the real
# bill while it lasts and the script says so. An endpoint without
# tool-call support is refused. An existing id is a reviewed change (a
# round), never an add. No key is read or printed: the endpoints API is
# public.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REGISTRY="$ROOT/models/registry.json"
ID=""
PROVIDER=""
FROM_FILE=""
DRY_RUN=0
SELF_TEST=0

usage() {
    sed -n '2,4p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --provider) [[ $# -ge 2 ]] || usage; PROVIDER="$2"; shift 2 ;;
        --from-file) [[ $# -ge 2 ]] || usage; FROM_FILE="$2"; shift 2 ;;
        --registry) [[ $# -ge 2 ]] || usage; REGISTRY="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        -h|--help) usage ;;
        -*) echo "[add-model] unknown argument: $1" >&2; usage ;;
        *)
            [[ -z "$ID" ]] || { echo "[add-model] one model id only" >&2; exit 2; }
            ID="$1"; shift ;;
    esac
done

# The one row builder: the endpoint selection, the base prices and the row
# JSON on stdout; every refusal on stderr with its exit code.
row_json() {
    python3 - "$FROM_FILE" "$ID" "$PROVIDER" <<'PY'
import datetime, json, sys
path, model, provider = sys.argv[1:4]
try:
    body = json.load(open(path))
except Exception as exc:
    sys.exit("[add-model] cannot read the endpoints file %s: %s" % (path, exc))
data = body.get("data") or {}
eps = data.get("endpoints") or []
if not eps:
    sys.exit("[add-model] %s has no endpoints" % model)
tags = [e.get("tag") or e.get("provider_name") or e.get("name") or "?" for e in eps]
if provider:
    chosen = None
    for e in eps:
        if provider in (e.get("tag"), e.get("provider_name"), e.get("name")):
            chosen = e
            break
    if chosen is None:
        print("[add-model] provider %s is not one of: %s" % (provider, ", ".join(tags)), file=sys.stderr)
        sys.exit(2)
elif len(eps) == 1:
    chosen = eps[0]
else:
    print(
        "[add-model] %s has several providers; pass --provider <tag>: %s"
        % (model, ", ".join(tags)),
        file=sys.stderr,
    )
    sys.exit(2)
if "tools" not in (chosen.get("supported_parameters") or []):
    sys.exit("[add-model] %s (%s) has no tool-call support" % (model, chosen.get("tag") or "?"))
pricing = chosen.get("pricing") or {}
discount = pricing.get("discount") or 0
try:
    discount = float(discount)
except (TypeError, ValueError):
    discount = 0.0
if not 0.0 < discount < 1.0:
    discount = 0.0

def per_million(key):
    raw = pricing.get(key)
    if raw is None:
        return None
    return float(raw) * 1_000_000.0 / (1.0 - discount)

prompt = per_million("prompt")
completion = per_million("completion")
if prompt is None or completion is None:
    sys.exit("[add-model] %s's endpoint has no prompt/completion price" % model)
cache_read = per_million("input_cache_read")
cache_write = per_million("input_cache_write")
if cache_read is None:
    cache_read = prompt
if cache_write is None:
    cache_write = prompt
if discount:
    print(
        "[add-model] warning: %s shows discount %.2f; the row stores the "
        "undiscounted base prices (the real bill will be lower while the "
        "discount lasts; scoring stays at base rates)" % (model, discount),
        file=sys.stderr,
    )
row = {
    "id": model,
    "provider": chosen.get("tag") or chosen.get("provider_name"),
    "prompt_per_million": round(prompt, 10),
    "completion_per_million": round(completion, 10),
    "cache_read_per_million": round(cache_read, 10),
    "cache_write_per_million": round(cache_write, 10),
    "added": datetime.date.today().isoformat(),
    "source": "OpenRouter /api/v1/models/%s/endpoints" % model,
}
print(json.dumps(row, indent=2, ensure_ascii=False))
PY
}

write_registry() {
    python3 - "$REGISTRY" "$1" <<'PY'
import json, sys
path, row_text = sys.argv[1:3]
try:
    reg = json.load(open(path))
except Exception as exc:
    sys.exit("[add-model] cannot read the registry %s: %s" % (path, exc))
row = json.loads(row_text)
models = reg.setdefault("models", [])
if any(m.get("id", "").lower() == row["id"].lower() for m in models):
    sys.exit(
        "[add-model] %s is already in the registry; changing an existing "
        "model's row is a reviewed change (a round), not an add" % row["id"]
    )
models.append(row)
with open(path, "w") as f:
    json.dump(reg, f, indent=2, ensure_ascii=False)
    f.write("\n")
print("[add-model] wrote %s to %s (%d models)" % (row["id"], path, len(models)))
PY
}

self_test() {
    local dir fixture reg out err rc
    dir="$(mktemp -d /tmp/sdb-add-model-self-test.XXXXXX)"
    trap 'rm -rf "${dir:-}"' EXIT
    fixture="$ROOT/scripts/fixtures/add-model-endpoints.json"
    reg="$dir/registry.json"
    printf '{"as_of":"2026-11-19","source":"self-test","models":[]}\n' > "$reg"
    # One provider, discount 0.5: the base price is written and warned.
    "$0" tencent/hy4-flash --from-file "$fixture" --registry "$reg" \
        > "$dir/out" 2> "$dir/err"
    python3 - "$reg" <<'PY' || { echo "[add-model] self-test: base price wrong" >&2; cat "$dir/out" "$dir/err" >&2; exit 1; }
import json, sys
reg = json.load(open(sys.argv[1]))
row = next(m for m in reg["models"] if m["id"] == "tencent/hy4-flash")
assert row["provider"] == "tencent/fp8", row
for key, want in (("prompt_per_million", 0.07),
                  ("completion_per_million", 0.281),
                  ("cache_read_per_million", 0.003),
                  ("cache_write_per_million", 0.07)):
    assert abs(row[key] - want) < 1e-9, (key, row[key], want)
PY
    grep -q 'discount 0.50' "$dir/err" || {
        echo "[add-model] self-test: the discount warning is missing" >&2
        cat "$dir/err" >&2
        exit 1
    }
    # Several providers: no --provider refuses and lists them; with it, writes.
    cat > "$dir/two.json" <<'JSON'
{"data":{"id":"x/y","endpoints":[
 {"tag":"a/x","pricing":{"prompt":"0.000001","completion":"0.000002"},"supported_parameters":["tools"]},
 {"tag":"b/y","pricing":{"prompt":"0.000003","completion":"0.000004"},"supported_parameters":["tools"]}]}}
JSON
    rc=0
    "$0" x/y --from-file "$dir/two.json" --registry "$reg" > "$dir/out" 2> "$dir/err" || rc=$?
    [[ "$rc" -eq 2 ]] || { echo "[add-model] self-test: several providers did not refuse (rc=$rc)" >&2; cat "$dir/err" >&2; exit 1; }
    grep -q 'a/x' "$dir/err" && grep -q 'b/y' "$dir/err" || {
        echo "[add-model] self-test: the provider list is not named" >&2; cat "$dir/err" >&2; exit 1; }
    "$0" x/y --from-file "$dir/two.json" --provider b/y --registry "$reg" > /dev/null 2>&1
    python3 - "$reg" <<'PY' || { echo "[add-model] self-test: --provider row wrong" >&2; exit 1; }
import json, sys
row = next(m for m in json.load(open(sys.argv[1]))["models"] if m["id"] == "x/y")
assert row["provider"] == "b/y", row
PY
    # No tool-call support refuses.
    cat > "$dir/notool.json" <<'JSON'
{"data":{"id":"x/y","endpoints":[{"tag":"a/x","pricing":{"prompt":"0.000001","completion":"0.000002"},"supported_parameters":["response_format"]}]}}
JSON
    rc=0
    "$0" x/y --from-file "$dir/notool.json" --registry "$reg" > "$dir/out" 2> "$dir/err" || rc=$?
    [[ "$rc" -ne 0 ]] || { echo "[add-model] self-test: a no-tool endpoint was accepted" >&2; exit 1; }
    grep -qi 'tool' "$dir/err" || { echo "[add-model] self-test: the no-tool refusal is not named" >&2; cat "$dir/err" >&2; exit 1; }
    echo "RESULT: PASS (add-model self-test)"
}

if [[ "$SELF_TEST" -eq 1 ]]; then
    self_test
    exit 0
fi

[[ -n "$ID" ]] || usage

if [[ -z "$FROM_FILE" ]]; then
    FROM_FILE="$(mktemp /tmp/sdb-add-model-endpoints.XXXXXX)"
    trap 'rm -f "${FROM_FILE:-}"' EXIT
    curl -fsS --max-time 30 "https://openrouter.ai/api/v1/models/$ID/endpoints" -o "$FROM_FILE" || {
        echo "[add-model] cannot fetch the endpoints for $ID" >&2
        exit 1
    }
fi

ROW="$(row_json)" || exit $?
if [[ "$DRY_RUN" -eq 1 ]]; then
    printf '%s\n' "$ROW"
    echo "[add-model] dry run: not written; run scripts/model-check.sh $ID next" >&2
    exit 0
fi
write_registry "$ROW"
echo "[add-model] next: run scripts/model-check.sh $ID before the model's first cell" >&2
