#!/usr/bin/env bash
# R68 F3 (DECISIONS #28): the pre-grid model check.
#
# One tiny call through the sandbox's omp (the same image and ~/.omp mount
# the cells use), then OpenRouter's generation API. It passes only when
# omp sent exactly the requested id, OpenRouter served that id (its dated
# id starts with it) from the pinned provider, and the bill is within
# 1.25x of the snapshot price for the call.
#
# Usage: scripts/model-check.sh <model>
#        scripts/model-check.sh --slug <model>
#        scripts/model-check.sh --record <model>
#        scripts/model-check.sh --self-test
#
# <model> is a cell model name (`openrouter/z-ai/glm-5.3`) or a snapshot id
# (`z-ai/glm-5.3`). A model with no row in src/cost/model_prices.json has
# no pin and no snapshot price: the provider and bill checks are skipped
# and the record says so.
#
# The record is <git-common-dir>/sdb-gates/model-check-<slug>.txt. It holds
# the sent and served ids, the provider, the ratio, the UTC time and the
# sandbox image digest; grid.sh reads it. The API key is never printed.
#
# Test hooks (used by --self-test; no network, no Docker):
#   SDB_MODEL_CHECK_DIR               record directory
#   SDB_MODEL_CHECK_IMAGE_DIGEST      image digest instead of docker inspect
#   SDB_MODEL_CHECK_SESSION_JSON      a canned omp session file
#   SDB_MODEL_CHECK_GENERATION_JSON   a canned generation response
#   SDB_ENV_FILE                      where to read OPENROUTER_API_KEY
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

model_slug() {
    local m="${1#openrouter/}"
    printf '%s' "$m" | tr '[:upper:]' '[:lower:]' \
        | sed 's#/#-#g; s#[^a-z0-9._-]#-#g'
}

record_dir() {
    if [[ -n "${SDB_MODEL_CHECK_DIR:-}" ]]; then
        printf '%s' "$SDB_MODEL_CHECK_DIR"
    else
        printf '%s/sdb-gates' \
            "$(cd "$ROOT" && git rev-parse --path-format=absolute --git-common-dir)"
    fi
}

record_path() {
    printf '%s/model-check-%s.txt' "$(record_dir)" "$(model_slug "$1")"
}

usage() {
    sed -n '1,32p' "$0" >&2
    exit 2
}

mode=check
model=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --self-test) mode=self-test; shift ;;
        --slug)
            [[ $# -ge 2 ]] || { echo "[model-check] --slug needs a model" >&2; exit 2; }
            mode=slug; model="$2"; shift 2 ;;
        --record)
            [[ $# -ge 2 ]] || { echo "[model-check] --record needs a model" >&2; exit 2; }
            mode=record; model="$2"; shift 2 ;;
        -h|--help) usage ;;
        -*) echo "[model-check] unknown argument: $1" >&2; exit 2 ;;
        *)
            [[ -z "$model" ]] || { echo "[model-check] one model only" >&2; exit 2; }
            model="$1"; shift ;;
    esac
done

case "$mode" in
    slug) model_slug "$model"; echo; exit 0 ;;
    record) record_path "$model"; echo; exit 0 ;;
esac

if [[ "$mode" == "self-test" ]]; then
    self_test() {
        local dir out rc case_name
        dir="$(mktemp -d /tmp/sdb-model-check-self-test.XXXXXX)"
        trap 'rm -rf "${dir:-}"' EXIT
        export SDB_MODEL_CHECK_DIR="$dir/gates"
        export SDB_MODEL_CHECK_IMAGE_DIGEST="sha256:selftest"
        mkdir -p "$SDB_MODEL_CHECK_DIR"

        # The snapshot rates for the pinned fixture model.
        read -r rate_prompt rate_completion < <(python3 - "$ROOT/src/cost/model_prices.json" <<'PY'
import json, sys
snap = json.load(open(sys.argv[1]))
for e in snap["models"]:
    if e["id"] == "openai/gpt-6-luna-pro":
        print(e["prompt_per_million"], e["completion_per_million"])
        break
else:
    raise SystemExit("gpt-6-luna-pro is missing from the snapshot")
PY
)

        fixture() {
            local sent="$1" served="$2" provider="$3" bill="$4" fallback="$5"
            python3 - "$dir/session.jsonl" "$dir/generation.json" \
                "$sent" "$served" "$provider" "$bill" "$fallback" <<'PY'
import json, sys
session, generation, sent, served, provider, bill, fallback = sys.argv[1:8]
with open(session, "w") as f:
    f.write(json.dumps({
        "type": "model_change",
        "model": sent,
        "resolvedModelIsFallback": fallback == "1",
    }) + "\n")
    f.write(json.dumps({
        "type": "message",
        "message": {
            "role": "assistant",
            "model": served,
            "responseId": "gen-selftest",
            "content": [{"type": "text", "text": "ok"}],
        },
    }) + "\n")
with open(generation, "w") as f:
    json.dump({"data": {
        "id": "gen-selftest",
        "model": served,
        "provider_name": provider,
        "total_cost": float(bill),
        "tokens_prompt": 100,
        "tokens_completion": 10,
    }}, f)
PY
        }

        expect_fail() {
            local want="$1" name="$2"
            rc=0
            out="$(SDB_MODEL_CHECK_SESSION_JSON="$dir/session.jsonl" \
                SDB_MODEL_CHECK_GENERATION_JSON="$dir/generation.json" \
                "$ROOT/scripts/model-check.sh" "$name" 2>&1)" || rc=$?
            [[ "$rc" -ne 0 ]] || {
                echo "[self-test] $case_name passed unexpectedly: $out" >&2; exit 1; }
            grep -q "RESULT: FAIL" "$(record_path "$name")" || {
                echo "[self-test] $case_name wrote no FAIL record" >&2; exit 1; }
            grep -q "$want" "$(record_path "$name")" || {
                echo "[self-test] $case_name failed for the wrong reason" >&2
                cat "$(record_path "$name")" >&2; exit 1; }
        }

        expect_pass() {
            local name="$1"
            rc=0
            out="$(SDB_MODEL_CHECK_SESSION_JSON="$dir/session.jsonl" \
                SDB_MODEL_CHECK_GENERATION_JSON="$dir/generation.json" \
                "$ROOT/scripts/model-check.sh" "$name" 2>&1)" || rc=$?
            [[ "$rc" -eq 0 ]] || {
                echo "[self-test] $name was refused: $out" >&2; exit 1; }
            grep -q '^RESULT: PASS$' "$(record_path "$name")" || {
                echo "[self-test] $name wrote no PASS record" >&2
                cat "$(record_path "$name")" >&2; exit 1; }
        }

        case_name=fuzzy
        fixture "openrouter/tencent/hy4-preview" "tencent/hy4-preview-20260101" \
            "Tencent" 0.0001 0
        expect_fail "sent tencent/hy4-preview" "openrouter/tencent/hy4"
        echo "[self-test] a fuzzy sent id fails"

        case_name=wrong-provider
        fixture "openrouter/openai/gpt-6-luna-pro" "openai/gpt-6-luna-pro-20260922" \
            "NotOpenAI" \
            "$(awk -v p="$rate_prompt" -v c="$rate_completion" \
                'BEGIN { printf "%.10f", 100 * p / 1000000 + 10 * c / 1000000 }')" 0
        expect_fail "provider NotOpenAI" "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] a wrong provider fails"

        case_name=double-bill
        fixture "openrouter/openai/gpt-6-luna-pro" "openai/gpt-6-luna-pro-20260922" \
            "OpenAI" \
            "$(awk -v p="$rate_prompt" -v c="$rate_completion" \
                'BEGIN { printf "%.10f", 2 * (100 * p / 1000000 + 10 * c / 1000000) }')" 0
        expect_fail "bill" "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] a 2x bill fails"

        case_name=fallback
        fixture "openrouter/openai/gpt-6-luna-pro" "openai/gpt-6-luna-pro-20260922" \
            "OpenAI" \
            "$(awk -v p="$rate_prompt" -v c="$rate_completion" \
                'BEGIN { printf "%.10f", 100 * p / 1000000 + 10 * c / 1000000 }')" 1
        expect_fail "fallback" "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] a fallback resolution fails"

        case_name=exact
        fixture "openrouter/openai/gpt-6-luna-pro" "openai/gpt-6-luna-pro-20260922" \
            "OpenAI" \
            "$(awk -v p="$rate_prompt" -v c="$rate_completion" \
                'BEGIN { printf "%.10f", 100 * p / 1000000 + 10 * c / 1000000 }')" 0
        expect_pass "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] an exact match passes"

        case_name=unpinned
        fixture "openrouter/z-ai/glm-5.3" "z-ai/glm-5.3-20260101" \
            "Z.AI" 0.0001 0
        expect_pass "openrouter/z-ai/glm-5.3"
        grep -q 'provider check skipped' "$(record_path "openrouter/z-ai/glm-5.3")" || {
            echo "[self-test] the unpinned record does not say the provider check was skipped" >&2
            exit 1; }
        grep -q 'bill check skipped' "$(record_path "openrouter/z-ai/glm-5.3")" || {
            echo "[self-test] the unpinned record does not say the bill check was skipped" >&2
            exit 1; }
        echo "[self-test] an unpinned model passes with the checks marked skipped"

        [[ "$("$ROOT/scripts/model-check.sh" --slug openrouter/z-ai/glm-5.3)" == "z-ai-glm-5.3" ]] || {
            echo "[self-test] the slug is not z-ai-glm-5.3" >&2; exit 1; }
        [[ "$("$ROOT/scripts/model-check.sh" --record openrouter/z-ai/glm-5.3)" == \
            "$SDB_MODEL_CHECK_DIR/model-check-z-ai-glm-5.3.txt" ]] || {
            echo "[self-test] the record path is not the model-check slug" >&2; exit 1; }
        echo "[self-test] the slug and record path are stable"

        echo "RESULT: PASS (model-check self-test)"
    }
    self_test
    exit 0
fi

[[ -n "$model" ]] || usage

id="${model#openrouter/}"
wire="openrouter/$id"

# The snapshot row, if any: pin, prompt and completion rates. A model with
# no row is allowed (the provider and bill checks are skipped).
snapshot="$(
    python3 - "$ROOT/src/cost/model_prices.json" "$id" <<'PY'
import json, sys
snap = json.load(open(sys.argv[1]))
want = sys.argv[2].lower()
for e in snap["models"]:
    if e["id"].lower() == want:
        print(json.dumps({
            "found": True,
            "provider": e.get("provider") or "",
            "prompt": e["prompt_per_million"],
            "completion": e["completion_per_million"],
        }))
        break
else:
    print(json.dumps({"found": False, "provider": "", "prompt": 0, "completion": 0}))
PY
)"
pin="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["provider"])' "$snapshot")"
found="$(python3 -c 'import json,sys; print("1" if json.loads(sys.argv[1])["found"] else "0")' "$snapshot")"
rate_prompt="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["prompt"])' "$snapshot")"
rate_completion="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["completion"])' "$snapshot")"

if [[ -n "$pin" ]]; then
    wire="$wire@$pin"
fi

record="$(record_path "$model")"
mkdir -p "$(dirname "$record")"
time_utc="$(date -u +%FT%TZ)"
image="${SDB_MODEL_CHECK_IMAGE_DIGEST:-}"
if [[ -z "$image" ]]; then
    image="$(docker image inspect subdollar-sandbox --format '{{.Id}}' 2>/dev/null || true)"
fi
if [[ -z "$image" ]]; then
    {
        echo "model:    $id"
        echo "sent:     (not run)"
        echo "served:   (not run)"
        echo "provider: (not run)"
        echo "bill:     (not run)"
        echo "time:     $time_utc"
        echo "image:    (missing)"
        echo "RESULT: FAIL (no subdollar-sandbox image)"
    } > "$record"
    echo "[model-check] FATAL: no subdollar-sandbox image ($record)" >&2
    exit 1
fi

work="$(mktemp -d /tmp/sdb-model-check.XXXXXX)"
log="$(mktemp)"
cleanup() {
    rm -rf "$work"
    rm -f "$log"
}
trap cleanup EXIT

session_file="${SDB_MODEL_CHECK_SESSION_JSON:-}"
generation_file="${SDB_MODEL_CHECK_GENERATION_JSON:-}"
key="${OPENROUTER_API_KEY:-}"

if [[ -z "$session_file" ]]; then
    if [[ -z "$key" ]]; then
        env_file="${SDB_ENV_FILE:-$ROOT/.env}"
        if [[ -f "$env_file" ]]; then
            key="$(grep -m1 '^OPENROUTER_API_KEY=' "$env_file" \
                | cut -d= -f2- | tr -d '\r' | tr -d '"' | tr -d "'")"
        fi
    fi
    if [[ -z "$key" ]]; then
        echo "[model-check] FATAL: OPENROUTER_API_KEY is unset and $ROOT/.env has none" >&2
        exit 1
    fi
    export OPENROUTER_API_KEY="$key"

    session_name="model-check-$(model_slug "$model")-$(date -u +%Y%m%dT%H%M%SZ)-$$"
    host_sessions="$HOME/.omp/agent/sessions"
    session_host="$host_sessions/$session_name"
    session_dir="/home/ubuntu/.omp/agent/sessions/$session_name"
    mkdir -p "$session_host"
    container="sdb-model-check-$(model_slug "$model")-$$"
    set +e
    timeout 240 docker run --rm --name "$container" \
        -v "$work:/workspace" \
        -v "$HOME/.omp:/home/ubuntu/.omp" \
        -w /workspace \
        -e PI_NO_PTY=1 \
        -e HOME=/home/ubuntu \
        -e OPENROUTER_API_KEY \
        subdollar-sandbox omp \
        --approval-mode=yolo --tools=read \
        "--model=$wire" --cwd=/workspace \
        "--session-dir=$session_dir" --max-time=120s \
        --thinking=low --print-thoughts -p -- "Reply with exactly: ok" \
        >"$log" 2>&1
    rc=$?
    set -e
    if [[ "$rc" -ne 0 ]]; then
        {
            echo "model:    $id"
            echo "sent:     (omp failed)"
            echo "served:   (not queried)"
            echo "provider: (not queried)"
            echo "bill:     (not queried)"
            echo "time:     $time_utc"
            echo "image:    $image"
            echo "RESULT: FAIL (omp exit $rc)"
        } > "$record"
        echo "[model-check] FATAL: omp exit $rc ($record); output tail:" >&2
        tail -n 20 "$log" >&2 || true
        exit 1
    fi
    session_file="$(find "$session_host" -type f -name '*.jsonl' -print -quit 2>/dev/null || true)"
    if [[ -z "$session_file" ]]; then
        {
            echo "model:    $id"
            echo "sent:     (no session)"
            echo "served:   (not queried)"
            echo "provider: (not queried)"
            echo "bill:     (not queried)"
            echo "time:     $time_utc"
            echo "image:    $image"
            echo "RESULT: FAIL (omp wrote no session file)"
        } > "$record"
        echo "[model-check] FATAL: omp wrote no session file ($record)" >&2
        exit 1
    fi
fi

parsed="$(
    python3 - "$session_file" <<'PY'
import json, sys
sent = ""
response_id = ""
served_session = ""
fallback = False
for line in open(sys.argv[1], errors="replace"):
    try:
        v = json.loads(line)
    except Exception:
        continue
    if v.get("type") == "model_change" and not sent:
        sent = v.get("model") or ""
        fallback = bool(v.get("resolvedModelIsFallback"))
    m = v.get("message") or {}
    if m.get("role") == "assistant" and not response_id:
        response_id = m.get("responseId") or ""
        served_session = m.get("model") or ""
print(json.dumps({
    "sent": sent,
    "responseId": response_id,
    "served_session": served_session,
    "fallback": fallback,
}))
PY
)"
response_id="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["responseId"])' "$parsed")"
if [[ -z "$response_id" ]]; then
    {
        echo "model:    $id"
        echo "sent:     $(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["sent"])' "$parsed")"
        echo "served:   (no response id)"
        echo "provider: (not queried)"
        echo "bill:     (not queried)"
        echo "time:     $time_utc"
        echo "image:    $image"
        echo "RESULT: FAIL (omp session has no responseId)"
    } > "$record"
    echo "[model-check] FATAL: no responseId in $session_file ($record)" >&2
    exit 1
fi

if [[ -z "$generation_file" ]]; then
    generation_file="$(mktemp)"
    set +e
    printf 'header = "Authorization: Bearer %s"\n' "$key" \
        | curl -sS -K - -G https://openrouter.ai/api/v1/generation \
            --data-urlencode "id=$response_id" -o "$generation_file"
    rc=$?
    set -e
    if [[ "$rc" -ne 0 ]]; then
        {
            echo "model:    $id"
            echo "sent:     $(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["sent"])' "$parsed")"
            echo "served:   (generation query failed)"
            echo "provider: (not queried)"
            echo "bill:     (not queried)"
            echo "time:     $time_utc"
            echo "image:    $image"
            echo "RESULT: FAIL (generation API exit $rc)"
        } > "$record"
        echo "[model-check] FATAL: generation API exit $rc ($record)" >&2
        exit 1
    fi
fi

verdict="$(
    python3 - "$id" "$pin" "$found" "$rate_prompt" "$rate_completion" \
        "$parsed" "$generation_file" <<'PY'
import json, re, sys

(model_id, pin, found, rate_prompt, rate_completion,
 parsed_path, generation_path) = sys.argv[1:8]
parsed = json.loads(parsed_path)
gen = json.load(open(generation_path)).get("data") or {}

sent = (parsed.get("sent") or "").removeprefix("openrouter/")
served = gen.get("model") or ""
provider_name = gen.get("provider_name") or ""
response_id = parsed.get("responseId") or ""
gen_id = gen.get("id") or ""
fallback = bool(parsed.get("fallback"))
bill = float(gen.get("total_cost") or 0.0)
tokens_prompt = int(gen.get("tokens_prompt") or 0)
tokens_completion = int(gen.get("tokens_completion") or 0)
priced = found == "1"
pinned = bool(pin)

reasons = []
sent_ok = sent.lower() == model_id.lower()
if not sent_ok:
    reasons.append("sent %s, want %s" % (sent or "(none)", model_id))
if fallback:
    reasons.append("omp resolved a fallback model")
served_ok = served.lower().startswith(model_id.lower())
if not served_ok:
    reasons.append("served %s, want %s*" % (served or "(none)", model_id))
if gen_id and gen_id != response_id:
    reasons.append("generation id %s, want %s" % (gen_id, response_id))

provider_ok = None
if pinned:
    base = re.sub(r"[^a-z0-9]", "", pin.split("/")[0].lower())
    seen = re.sub(r"[^a-z0-9]", "", provider_name.lower())
    provider_ok = bool(base) and (seen == base or seen.startswith(base))
    if not provider_ok:
        reasons.append("provider %s, want pin %s" % (provider_name or "(none)", pin))

bill_ok = None
expected = (tokens_prompt * float(rate_prompt) + tokens_completion * float(rate_completion)) / 1_000_000
ratio = None
if priced and expected > 0:
    ratio = bill / expected
    bill_ok = ratio <= 1.25
    if not bill_ok:
        reasons.append("bill $%.8f is %.2fx the snapshot $%.8f" % (bill, ratio, expected))
elif not priced:
    pass

print(json.dumps({
    "sent": sent,
    "served": served,
    "provider_name": provider_name,
    "pinned": pinned,
    "priced": priced,
    "provider_ok": provider_ok,
    "bill": bill,
    "expected": expected,
    "ratio": ratio,
    "bill_ok": bill_ok,
    "reasons": reasons,
    "ok": not reasons,
}))
PY
)"

ok="$(python3 -c 'import json,sys; print("1" if json.loads(sys.argv[1])["ok"] else "0")' "$verdict")"
sent="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["sent"])' "$verdict")"
served="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["served"])' "$verdict")"
provider_name="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["provider_name"])' "$verdict")"
ratio="$(python3 -c 'import json,sys; v=json.loads(sys.argv[1]); print("" if v["ratio"] is None else "%.3f" % v["ratio"])' "$verdict")"
bill="$(python3 -c 'import json,sys; print("%.8f" % json.loads(sys.argv[1])["bill"])' "$verdict")"
expected="$(python3 -c 'import json,sys; print("%.8f" % json.loads(sys.argv[1])["expected"])' "$verdict")"
pinned="$(python3 -c 'import json,sys; print("1" if json.loads(sys.argv[1])["pinned"] else "0")' "$verdict")"
priced="$(python3 -c 'import json,sys; print("1" if json.loads(sys.argv[1])["priced"] else "0")' "$verdict")"
reasons="$(python3 -c 'import json,sys; print("; ".join(json.loads(sys.argv[1])["reasons"]))' "$verdict")"

{
    echo "model:    $id"
    echo "sent:     $sent"
    echo "served:   $served"
    if [[ "$pinned" == "1" ]]; then
        echo "provider: $provider_name (pin: $pin)"
    else
        echo "provider: $provider_name (no pin; provider check skipped)"
    fi
    if [[ "$priced" == "1" ]]; then
        echo "bill:     \$$bill (snapshot \$$expected, ratio $ratio)"
    else
        echo "bill:     \$$bill (no snapshot row; bill check skipped)"
    fi
    echo "time:     $time_utc"
    echo "image:    $image"
    if [[ "$ok" == "1" ]]; then
        echo "RESULT: PASS"
    else
        echo "RESULT: FAIL ($reasons)"
    fi
} > "$record"

if [[ "$ok" == "1" ]]; then
    echo "[model-check] PASS: $id sent exactly, served $served ($record)"
    exit 0
fi
echo "[model-check] FATAL: $reasons ($record)" >&2
exit 1
