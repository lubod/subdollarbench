#!/usr/bin/env bash
# R68 F3 (DECISIONS #28): the pre-grid model check.
#
# One tiny call through the sandbox's omp (the same image and ~/.omp mount
# the cells use), then OpenRouter's generation API. It passes only when
# omp sent exactly the requested id, OpenRouter served the model's
# canonical_slug exactly (not a prefix), the record's provider is the pin,
# and the bill is within 1.25x of the snapshot price for the record's own
# native token counts (prompt, cached, completion and cache writes) with
# all four snapshot rates.
#
# Usage: scripts/model-check.sh [--harness omp|opencode] <model>
#        scripts/model-check.sh [--harness omp|opencode] --slug <model>
#        scripts/model-check.sh [--harness omp|opencode] --record <model>
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
#   SDB_MODEL_CHECK_MODELS_JSON       a canned /api/v1/models response
#   SDB_MODEL_CHECK_OPENCODE_DB       a canned opencode session db
#   SDB_MODEL_CHECK_USAGE_BEFORE/AFTER the OpenRouter key usage around a run
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
    local prefix=""
    [[ "${harness:-omp}" == "opencode" ]] && prefix="opencode-"
    printf '%s/model-check-%s%s.txt' "$(record_dir)" "$prefix" "$(model_slug "$1")"
}

# R79-18: a settled canned usage series for the self-test: six equal
# reads, the 30 s settle window at the live 5 s interval.
settled_series() {
    local v="$1"
    printf '%s,%s,%s,%s,%s,%s' "$v" "$v" "$v" "$v" "$v" "$v"
}

# R79-4: the per-run opencode config content: the model's effort variants
# and the snapshot provider pin (the same shape the runner sends). The pin
# is per model: a provider-wide body would pin opencode's title model too.
opencode_config_json() {
    python3 - "$1" "$2" <<'PY'
import json, sys
model_id, pin = sys.argv[1], sys.argv[2]
cfg = {
    "model": "openrouter/" + model_id,
    "small_model": "openrouter/" + model_id,
    "providers": {"openrouter": {"models": {model_id: {
        "variants": [{"id": "low", "settings": {"reasoning": {"effort": "low"}}}]}}}},
}
if pin:
    cfg["providers"]["openrouter"]["models"][model_id]["body"] = {
        "provider": {"only": [pin], "order": [pin]}}
print(json.dumps(cfg))
PY
}

usage() {
    sed -n '1,32p' "$0" >&2
    exit 2
}

mode=check
model=""
harness="${SDB_MODEL_CHECK_HARNESS:-opencode}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --self-test) mode=self-test; shift ;;
        --harness)
            [[ $# -ge 2 ]] || { echo "[model-check] --harness needs omp|opencode" >&2; exit 2; }
            harness="$2"; shift 2 ;;
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

case "$harness" in
    omp|opencode) ;;
    *) echo "[model-check] --harness must be omp or opencode" >&2; exit 2 ;;
esac
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
        # R82 F1: the default harness is opencode; the omp cases below are
        # explicit, and record_path follows this local.
        harness=omp
        mkdir -p "$SDB_MODEL_CHECK_DIR"

        # The snapshot rates for a fixture model (all four).
        snapshot_rates() {
            python3 - "$ROOT/src/cost/model_prices.json" "$1" <<'PY'
import json, sys
snap = json.load(open(sys.argv[1]))
want = sys.argv[2].lower()
for e in snap["models"]:
    if e["id"].lower() == want:
        print(e["prompt_per_million"], e["cache_read_per_million"],
              e["cache_write_per_million"], e["completion_per_million"])
        break
else:
    raise SystemExit("missing from the snapshot")
PY
        }
        read -r rate_prompt rate_cache_read rate_cache_write rate_completion \
            < <(snapshot_rates "openai/gpt-6-luna-pro")

        # R75 F4: the canonical slugs (a canned /api/v1/models response).
        cat > "$dir/models.json" <<'JSON'
{"data":[
 {"id":"openai/gpt-6-luna-pro","canonical_slug":"openai/gpt-6-luna-pro-20260922"},
 {"id":"tencent/hy4","canonical_slug":"tencent/hy4-20261104"},
 {"id":"z-ai/glm-5.3","canonical_slug":"z-ai/glm-5.3-20260101"},
 {"id":"deepseek/deepseek-v4.1-flash-1017","canonical_slug":"deepseek/deepseek-v4.1-flash-20261017"}
]}
JSON
        export SDB_MODEL_CHECK_MODELS_JSON="$dir/models.json"

        fixture() {
            local sent="$1" served="$2" provider="$3" bill="$4" fallback="$5"
            local tokens_prompt="${6:-100}" tokens_cached="${7:-0}"
            local tokens_completion="${8:-10}" tokens_write="${9:-0}"
            python3 - "$dir/session.jsonl" "$dir/generation.json" \
                "$sent" "$served" "$provider" "$bill" "$fallback" \
                "$tokens_prompt" "$tokens_cached" "$tokens_completion" \
                "$tokens_write" <<'PY'
import json, sys
(session, generation, sent, served, provider, bill, fallback,
 tokens_prompt, tokens_cached, tokens_completion,
 tokens_write) = sys.argv[1:12]
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
        "tokens_prompt": int(tokens_prompt) * 3 // 5,
        "tokens_completion": 1,
        "native_tokens_prompt": int(tokens_prompt),
        "native_tokens_cached": int(tokens_cached),
        "native_tokens_completion": int(tokens_completion),
        "native_tokens_cache_write": int(tokens_write),
    }}, f)
PY
        }

        export SDB_MODEL_CHECK_TOOL_SESSION_JSON_1="$dir/tool-1.jsonl"
        export SDB_MODEL_CHECK_TOOL_SESSION_JSON_2="$dir/tool-2.jsonl"
        tool_fixture() {
            local file="$1" result="$2" is_error="$3" include="$4"
            python3 - "$file" "$result" "$is_error" "$include" <<'PY'
import json, sys
path, result, is_error, include = sys.argv[1:5]
lines = [
    {"type": "message", "message": {"role": "user", "content": [
        {"type": "text", "text": "run echo probe-forty-two"}]}},
    {"type": "message", "message": {"role": "assistant", "content": [
        {"type": "toolCall", "name": "bash",
         "arguments": {"command": "echo probe-forty-two"}}]}},
]
if include == "1":
    lines.append({"type": "message", "message": {
        "role": "toolResult", "toolName": "bash",
        "isError": is_error == "1",
        "content": [{"type": "text", "text": result}]}})
with open(path, "w") as f:
    for line in lines:
        f.write(json.dumps(line) + "\n")
PY
        }
        tool_set() {
            tool_fixture "$dir/tool-1.jsonl" "$1" "$2" "$3"
            tool_fixture "$dir/tool-2.jsonl" "$1" "$2" "$3"
        }
        tool_set "probe-forty-two" 0 1

        expect_fail() {
            local want="$1" name="$2"
            rc=0
            out="$(SDB_MODEL_CHECK_SESSION_JSON="$dir/session.jsonl" \
                SDB_MODEL_CHECK_GENERATION_JSON="$dir/generation.json" \
                "$ROOT/scripts/model-check.sh" --harness omp "$name" 2>&1)" || rc=$?
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
                "$ROOT/scripts/model-check.sh" --harness omp "$name" 2>&1)" || rc=$?
            [[ "$rc" -eq 0 ]] || {
                echo "[self-test] $name was refused: $out" >&2; exit 1; }
            grep -q '^RESULT: PASS$' "$(record_path "$name")" || {
                echo "[self-test] $name wrote no PASS record" >&2
                cat "$(record_path "$name")" >&2; exit 1; }
            grep -q 'tool-calls: 2/2 output, 0/2 errors' "$(record_path "$name")" || {
                echo "[self-test] $name did not record both passing tool tries" >&2
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

        case_name=all-rates
        read -r p cr cw c < <(snapshot_rates "openai/gpt-6-luna-pro")
        fixture "openrouter/openai/gpt-6-luna-pro" \
            "openai/gpt-6-luna-pro-20260922" "OpenAI" \
            "$(awk -v p="$p" -v cr="$cr" -v cw="$cw" -v c="$c" \
                'BEGIN { printf "%.10f", (30 * p + 50 * cr + 20 * cw + 10 * c) / 1000000 }')" \
            0 100 50 10 20
        expect_pass "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] the bill uses the fresh, cached, write and completion counts"

        case_name=dated-canonical
        read -r p cr cw c < <(snapshot_rates "deepseek/deepseek-v4.1-flash-1017")
        fixture "openrouter/deepseek/deepseek-v4.1-flash-1017" \
            "deepseek/deepseek-v4.1-flash-20261017" "DeepSeek" \
            "$(awk -v p="$p" -v c="$c" \
                'BEGIN { printf "%.10f", 100 * p / 1000000 + 10 * c / 1000000 }')" 0
        expect_pass "openrouter/deepseek/deepseek-v4.1-flash-1017"
        echo "[self-test] a dated canonical_slug passes"

        case_name=served-prefix
        read -r p cr cw c < <(snapshot_rates "tencent/hy4")
        fixture "openrouter/tencent/hy4" "tencent/hy4-preview-20260101" "Tencent" \
            "$(awk -v p="$p" -v c="$c" \
                'BEGIN { printf "%.10f", 100 * p / 1000000 + 10 * c / 1000000 }')" 0
        expect_fail "want canonical tencent/hy4-20261104" "openrouter/tencent/hy4"
        echo "[self-test] a served id that only shares the prefix fails"

        case_name=gmicloud-tool-error
        tool_set "provider stream ended with an error" 1 1
        expect_fail "tool-call" "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] a provider tool-call error fails"

        case_name=tool-missing-output
        tool_set "" 0 0
        expect_fail "tool-call" "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] a tool round trip with no command output fails"

        case_name=tool-missing-arguments
        tool_set "missing required arguments" 1 1
        expect_fail "tool-call" "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] missing tool arguments fail"

        tool_set "probe-forty-two" 0 1

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

        # R79 F3: the opencode harness's check reads the opencode session
        # db. The canned db carries the session_v2 row and the tool part.
        make_oc_db() {
            local path="$1" provider="$2" cost="$3"
            local tokens_input="${4:-100}" tokens_output="${5:-10}"
            python3 - "$path" "$provider" "$cost" "$tokens_input" "$tokens_output" <<'PY'
import json, sqlite3, sys
path, provider, cost, tokens_input, tokens_output = sys.argv[1:6]
con = sqlite3.connect(path)
con.execute(
    "create table session_v2(id text primary key, model text, cost real, "
    "tokens_input integer, tokens_output integer, tokens_reasoning integer, "
    "tokens_cache_read integer, tokens_cache_write integer, idle_outcome text, "
    "time_created integer)")
con.execute("create table session_message(session_id text, type text, data text)")
model = json.dumps({"id": "openai/gpt-6-luna-pro", "providerID": provider})
con.execute("insert into session_v2 values (?,?,?,?,?,?,?,?,?,?)",
            ("ses_selftest_positive", model, float(cost), int(tokens_input),
             int(tokens_output), 0, 0, 0, "succeeded", 1))
tool = json.dumps({"content": [{"type": "tool", "name": "shell",
    "state": {"status": "completed",
              "content": [{"type": "text", "text": "probe-forty-two\n"}]}}]})
con.execute("insert into session_message values (?, 'assistant', ?)",
            ("ses_selftest_positive", tool))
con.commit()
con.close()
PY
        }
        add_failed_session() {
            python3 - "$1" <<'PY'
import json, sqlite3, sys
con = sqlite3.connect(sys.argv[1])
model = json.dumps({"id": "openai/gpt-6-luna-pro", "providerID": "openrouter"})
con.execute("insert into session_v2 values (?,?,?,?,?,?,?,?,?,?)",
            ("ses_selftest_failed", model, 0.0, 0, 0, 0, 0, 0,
             "failed", 2))
tool = json.dumps({"content": [{"type": "tool", "name": "shell",
    "state": {"status": "failed",
              "content": [{"type": "text", "text": "wrong-session\n"}]}}]})
con.execute("insert into session_message values (?, 'assistant', ?)",
            ("ses_selftest_failed", tool))
con.commit()
con.close()
PY
        }
        oc_record() {
            printf '%s/model-check-opencode-%s.txt' "$SDB_MODEL_CHECK_DIR" \
                "$(model_slug "$1")"
        }
        expect_pass_oc() {
            local name="$1" rc=0 rec
            rec="$(oc_record "$name")"
            out="$(SDB_MODEL_CHECK_OPENCODE_DB="$dir/oc-pass.db" \
                SDB_MODEL_CHECK_USAGE_BEFORE=0 \
                SDB_MODEL_CHECK_USAGE_AFTER="$(settled_series 0.000015)" \
                SDB_MODEL_CHECK_MODELS_JSON="$dir/models.json" \
                "$ROOT/scripts/model-check.sh" --harness opencode "$name" 2>&1)" || rc=$?
            [[ "$rc" -eq 0 ]] || {
                echo "[self-test] opencode $name was refused: $out" >&2; exit 1; }
            grep -q '^RESULT: PASS$' "$rec" || {
                echo "[self-test] opencode $name wrote no PASS" >&2
                cat "$rec" >&2; exit 1; }
            grep -q 'tool-calls: 1 tool part(s)' "$rec" || {
                echo "[self-test] opencode $name did not record the tool part" >&2
                cat "$rec" >&2; exit 1; }
            grep -q 'session:  input 100, output 10, reasoning 0, cache read 0, cache write 0' "$rec" || {
                echo "[self-test] opencode $name did not record the raw session row" >&2
                cat "$rec" >&2; exit 1; }
            grep -q 'provider: openrouter (sent pin: openai)' "$rec" || {
                echo "[self-test] opencode $name did not record the sent pin" >&2
                cat "$rec" >&2; exit 1; }
            grep -q 'session-id: ses_selftest_positive' "$rec" || {
                echo "[self-test] opencode $name did not record the positive session id" >&2
                cat "$rec" >&2; exit 1; }
            grep -q 'negative: pin novita refused by OpenRouter' "$rec" || {
                echo "[self-test] opencode $name did not record the negative probe" >&2
                cat "$rec" >&2; exit 1; }
        }
        expect_fail_oc() {
            local db="$1" want="$2" name="$3" series="${4:-}" rc=0 rec
            rec="$(oc_record "$name")"
            [[ -n "$series" ]] || series="$(settled_series "${OC_BILL-0.000015}")"
            out="$(SDB_MODEL_CHECK_OPENCODE_DB="$db" \
                SDB_MODEL_CHECK_USAGE_BEFORE=0 \
                SDB_MODEL_CHECK_USAGE_AFTER="$series" \
                SDB_MODEL_CHECK_MODELS_JSON="$dir/models.json" \
                "$ROOT/scripts/model-check.sh" --harness opencode "$name" 2>&1)" || rc=$?
            [[ "$rc" -ne 0 ]] || {
                echo "[self-test] opencode $case_name passed unexpectedly: $out" >&2
                exit 1; }
            grep -q "RESULT: FAIL" "$rec" || {
                echo "[self-test] opencode $case_name wrote no FAIL" >&2; exit 1; }
            grep -q "$want" "$rec" || {
                echo "[self-test] opencode $case_name failed for the wrong reason" >&2
                cat "$rec" >&2; exit 1; }
        }

        case_name=opencode-pass
        cat > "$dir/negative-refused.json" <<'JSON'
{"type":"error","error":{"type":"provider.invalid-request","message":"No allowed providers are available for the selected model. Providers serving openai/gpt-6-luna-pro-20260922: openai, but your request's provider.only preference permits only: novita.","status":404}}
JSON
        cat > "$dir/negative-other.json" <<'JSON'
{"type":"error","error":{"type":"provider.rate-limit","message":"rate limited","status":429}}
JSON
        export SDB_MODEL_CHECK_OPENCODE_NEGATIVE_RC=1
        export SDB_MODEL_CHECK_OPENCODE_NEGATIVE_EVENTS="$dir/negative-refused.json"
        export SDB_MODEL_CHECK_OPENCODE_SESSION_ID="ses_selftest_positive"
        make_oc_db "$dir/oc-pass.db" openrouter 0.000015
        # R79-15: a later, failed session in the same db must not be the
        # one the verdict reads.
        add_failed_session "$dir/oc-pass.db"
        expect_pass_oc "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] an opencode session passes (a later failed session is ignored)"

        case_name=opencode-provider
        make_oc_db "$dir/oc-provider.db" Other 0.000015
        expect_fail_oc "$dir/oc-provider.db" "provider Other" \
            "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] an opencode wrong provider fails"

        case_name=opencode-bill
        make_oc_db "$dir/oc-bill.db" openrouter 0.000030
        OC_BILL=0.000030 expect_fail_oc "$dir/oc-bill.db" "bill" \
            "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] an opencode 2x bill fails"

        case_name=opencode-lag
        OC_BILL=0 expect_fail_oc "$dir/oc-pass.db" "usage lags" \
            "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] an opencode zero usage delta fails"

        case_name=opencode-floor
        OC_BILL=0.000001 expect_fail_oc "$dir/oc-pass.db" "floor" \
            "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] an opencode bill below the floor fails"

        case_name=opencode-zero-tokens
        make_oc_db "$dir/oc-zero-tokens.db" openrouter 0.000015 0 0
        expect_fail_oc "$dir/oc-zero-tokens.db" "the positive session has 0 tokens" \
            "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] a positive session with 0 tokens fails"

        case_name=opencode-zero-bill
        OC_BILL=0 expect_fail_oc "$dir/oc-pass.db" \
            "the OpenRouter usage lags or the run was not billed" \
            "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] a positive run with a \$0 bill fails"

        case_name=opencode-settle
        out="$(SDB_MODEL_CHECK_OPENCODE_DB="$dir/oc-pass.db" \
            SDB_MODEL_CHECK_USAGE_BEFORE=0 \
            SDB_MODEL_CHECK_USAGE_AFTER="0,0,0.000005,0,0,0.000015,0.000015,0.000015,0.000015,0.000015,0.000015" \
            SDB_MODEL_CHECK_MODELS_JSON="$dir/models.json" \
            "$ROOT/scripts/model-check.sh" --harness opencode \
            "openrouter/openai/gpt-6-luna-pro" 2>&1)" || rc=$?
        [[ "$rc" -eq 0 ]] || {
            echo "[self-test] the settling series was refused: $out" >&2; exit 1; }
        grep -q 'usage:    settled after 11 reads' \
            "$(oc_record "openrouter/openai/gpt-6-luna-pro")" || {
            echo "[self-test] the settling series did not record 11 reads" >&2
            cat "$(oc_record "openrouter/openai/gpt-6-luna-pro")" >&2; exit 1; }
        grep -q 'bill:     \$0.00001500' \
            "$(oc_record "openrouter/openai/gpt-6-luna-pro")" || {
            echo "[self-test] the settling series did not use the settled bill" >&2
            cat "$(oc_record "openrouter/openai/gpt-6-luna-pro")" >&2; exit 1; }
        echo "[self-test] a rise-fall usage series passes with its settled value"

        case_name=opencode-settle-never
        never="$(python3 -c 'print(",".join(["0","0.000015"] * 18))')"
        expect_fail_oc "$dir/oc-pass.db" "usage not settled after 36 reads" \
            "openrouter/openai/gpt-6-luna-pro" "$never"
        echo "[self-test] a usage series that never settles fails"

        case_name=opencode-negative-pass
        SDB_MODEL_CHECK_OPENCODE_NEGATIVE_RC=0 \
            expect_fail_oc "$dir/oc-pass.db" "negative: pin novita was served" \
            "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] a wrong pin that is served fails the opencode check"

        case_name=opencode-negative-other
        SDB_MODEL_CHECK_OPENCODE_NEGATIVE_RC=1 \
            SDB_MODEL_CHECK_OPENCODE_NEGATIVE_EVENTS="$dir/negative-other.json" \
            expect_fail_oc "$dir/oc-pass.db" "inconclusive" \
            "openrouter/openai/gpt-6-luna-pro"
        echo "[self-test] a negative probe with another error is inconclusive"

        case_name=opencode-pin
        cfg="$(opencode_config_json "openai/gpt-6-luna-pro" "openai")"
        python3 - "$cfg" <<'PY' || { echo "[self-test] the opencode config misses the per-model pin: $cfg" >&2; exit 1; }
import json, sys
cfg = json.loads(sys.argv[1])
body = cfg["providers"]["openrouter"]["models"]["openai/gpt-6-luna-pro"]["body"]
assert body == {"provider": {"only": ["openai"], "order": ["openai"]}}, cfg
assert "body" not in cfg["providers"]["openrouter"], cfg
PY
        cfg="$(opencode_config_json "google/gemini-2.5-flash" "")"
        python3 - "$cfg" <<'PY' || { echo "[self-test] an unpinned opencode config carries a pin: $cfg" >&2; exit 1; }
import json, sys
cfg = json.loads(sys.argv[1])
assert "body" not in cfg["providers"]["openrouter"], cfg
assert "body" not in cfg["providers"]["openrouter"]["models"]["google/gemini-2.5-flash"], cfg
PY
        echo "[self-test] the opencode config carries the per-model pin"

        [[ "$("$ROOT/scripts/model-check.sh" --slug openrouter/z-ai/glm-5.3)" == "z-ai-glm-5.3" ]] || {
            echo "[self-test] the slug is not z-ai-glm-5.3" >&2; exit 1; }
        [[ "$("$ROOT/scripts/model-check.sh" --harness omp --record openrouter/z-ai/glm-5.3)" == \
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
            "cache_read": e["cache_read_per_million"],
            "cache_write": e["cache_write_per_million"],
            "completion": e["completion_per_million"],
        }))
        break
else:
    print(json.dumps({
        "found": False,
        "provider": "",
        "prompt": 0,
        "cache_read": 0,
        "cache_write": 0,
        "completion": 0,
    }))
PY
)"
pin="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["provider"])' "$snapshot")"
found="$(python3 -c 'import json,sys; print("1" if json.loads(sys.argv[1])["found"] else "0")' "$snapshot")"
rate_prompt="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["prompt"])' "$snapshot")"
rate_cache_read="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["cache_read"])' "$snapshot")"
rate_cache_write="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["cache_write"])' "$snapshot")"
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
cleanup() {
    rm -rf "$work"
}
trap cleanup EXIT

# R75 F4: the served id must equal the model's canonical_slug exactly
# (live 2026-11-09: sent deepseek/deepseek-v4.1-flash-1017, served
# deepseek/deepseek-v4.1-flash-20261017). A prefix would let tencent/hy4
# match tencent/hy4-preview-..., so it is compared exactly.
models_json="${SDB_MODEL_CHECK_MODELS_JSON:-}"
if [[ -z "$models_json" ]]; then
    models_json="$work/models.json"
    if ! curl -sS https://openrouter.ai/api/v1/models -o "$models_json"; then
        models_json=""
    fi
fi
canonical=""
if [[ -n "$models_json" && -s "$models_json" ]]; then
    canonical="$(python3 - "$models_json" "$id" <<'PY'
import json, sys
try:
    data = json.load(open(sys.argv[1])).get("data") or []
except Exception:
    raise SystemExit(0)
want = sys.argv[2].lower()
for m in data:
    if (m.get("id") or "").lower() == want:
        print(m.get("canonical_slug") or "")
        break
PY
)"
fi
if [[ -z "$canonical" ]]; then
    {
        echo "model:    $id"
        echo "sent:     (not run)"
        echo "served:   (not queried)"
        echo "provider: (not queried)"
        echo "bill:     (not queried)"
        echo "canonical: (missing)"
        echo "time:     $time_utc"
        echo "image:    $image"
        echo "RESULT: FAIL (no canonical_slug for $id in /api/v1/models)"
    } > "$record"
    echo "[model-check] FATAL: no canonical_slug for $id ($record)" >&2
    exit 1
fi

session_file="${SDB_MODEL_CHECK_SESSION_JSON:-}"
generation_file="${SDB_MODEL_CHECK_GENERATION_JSON:-}"
tool_canned_1="${SDB_MODEL_CHECK_TOOL_SESSION_JSON_1:-}"
tool_canned_2="${SDB_MODEL_CHECK_TOOL_SESSION_JSON_2:-}"
key="${OPENROUTER_API_KEY:-}"

if [[ -z "$session_file" ]]; then
    # The repo .env is the source of truth for the cell key. A stale shell
    # key can be inference-only: live 2026-11-09, the generation API
    # answered 401 to the shell's key while .env's key worked. Prefer
    # .env; fall back to the environment.
    env_file="${SDB_ENV_FILE:-$ROOT/.env}"
    if [[ -f "$env_file" ]]; then
        env_key="$(grep -m1 '^OPENROUTER_API_KEY=' "$env_file" \
            | cut -d= -f2- | tr -d '\r' | tr -d '"' | tr -d "'")"
        if [[ -n "$env_key" ]]; then
            if [[ -n "$key" && "$key" != "$env_key" ]]; then
                echo "[model-check] note: the shell's OPENROUTER_API_KEY differs from $env_file; using $env_file" >&2
            fi
            key="$env_key"
        fi
    fi
    if [[ -z "$key" ]]; then
        echo "[model-check] FATAL: OPENROUTER_API_KEY is unset and $env_file has none" >&2
        exit 1
    fi
    export OPENROUTER_API_KEY="$key"
fi

# R68 F3: one omp invocation in the sandbox. SESSION_FILE is the session
# file to inspect; RUN_ERROR says why a failed run produced none.
SESSION_FILE=""
RUN_ERROR=""
run_omp() {
    local label="$1" prompt="$2" tools="$3" canned="$4"
    local stamp session_name host_sessions session_host session_dir container rc log
    SESSION_FILE=""
    RUN_ERROR=""
    if [[ -n "$canned" ]]; then
        SESSION_FILE="$canned"
        return 0
    fi
    stamp="$(date -u +%Y%m%dT%H%M%SZ)-$$"
    session_name="model-check-$(model_slug "$model")-$label-$stamp"
    host_sessions="$HOME/.omp/agent/sessions"
    session_host="$host_sessions/$session_name"
    session_dir="/home/ubuntu/.omp/agent/sessions/$session_name"
    mkdir -p "$session_host"
    container="sdb-model-check-$(model_slug "$model")-$label-$$"
    log="$work/$label.log"
    set +e
    timeout 240 docker run --rm --name "$container" \
        -v "$work:/workspace" \
        -v "$HOME/.omp:/home/ubuntu/.omp" \
        -w /workspace \
        -e PI_NO_PTY=1 \
        -e HOME=/home/ubuntu \
        -e OPENROUTER_API_KEY \
        subdollar-sandbox omp \
        --approval-mode=yolo "--tools=$tools" \
        "--model=$wire" --cwd=/workspace \
        "--session-dir=$session_dir" --max-time=120s \
        --thinking=low --print-thoughts -p -- "$prompt" \
        >"$log" 2>&1
    rc=$?
    set -e
    if [[ "$rc" -ne 0 ]]; then
        RUN_ERROR="omp exit $rc"
        return 1
    fi
    SESSION_FILE="$(find "$session_host" -type f -name '*.jsonl' -print -quit 2>/dev/null || true)"
    if [[ -z "$SESSION_FILE" ]]; then
        RUN_ERROR="omp wrote no session file"
        return 1
    fi
    return 0
}

# R68 F3: the prompt and the tool-call arguments both contain the probe
# string, so a raw grep is a false positive. Require a successful
# toolResult message whose text carries the first command's output.
tool_result_ok() {
    python3 - "$1" <<'PY'
import json, sys
for line in open(sys.argv[1], errors="replace"):
    try:
        v = json.loads(line)
    except Exception:
        continue
    m = v.get("message") or {}
    if m.get("role") != "toolResult" or m.get("isError"):
        continue
    text = ""
    for part in m.get("content") or []:
        if isinstance(part, dict) and part.get("type") == "text":
            text += part.get("text") or ""
    if "probe-forty-two" in text:
        raise SystemExit(0)
raise SystemExit(1)
PY
}

# R79 F3: the opencode harness check. The opencode CLI's session db
# (session_v2 + session_message) records the model id/provider the harness
# used, the session cost and tokens, and the tool parts. One tool-prompt
# run through the sandbox's opencode is the round trip; the db is the
# evidence. The record is model-check-opencode-<slug>.txt.
# R79-5: the bill is OpenRouter's key-usage delta around the run, never
# opencode's own session cost.
usage_query() {
    curl -sS https://openrouter.ai/api/v1/auth/key \
        -H "Authorization: Bearer $key" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin).get("data",{}).get("usage") or 0)'
}

opencode_check() {
    local db="${SDB_MODEL_CHECK_OPENCODE_DB:-}" run_dir="$work/opencode"
    local events="$work/opencode-events.json" log="$work/opencode.log" rc=0
    local usage_before usage_after bill
    # R79-15: the positive session has its own id and data dir; the
    # negative probe must never write into the db the verdict reads.
    local session_id="${SDB_MODEL_CHECK_OPENCODE_SESSION_ID:-ses_model_check_$(model_slug "$model")_$$}"
    local oc_tool_prompt='Use the shell tool twice: first run `echo probe-forty-two`, then run `ls -1 /etc | head -3`. Then reply with the output of the first command.'
    if [[ -n "${SDB_MODEL_CHECK_USAGE_BEFORE:-}" ]]; then
        usage_before="$SDB_MODEL_CHECK_USAGE_BEFORE"
    else
        usage_before="$(usage_query)" || usage_before=""
    fi
    if [[ -z "$usage_before" ]]; then
        echo "[model-check] FATAL: cannot read the OpenRouter key usage before the run" >&2
        exit 1
    fi
    if [[ -z "$db" ]]; then
        mkdir -p "$run_dir/ws" "$run_dir/cfg" "$run_dir/data"
        local cfg_json
        cfg_json="$(opencode_config_json "$id" "$pin")"
        set +e
        timeout 300 docker run --rm --name "sdb-model-check-oc-$(model_slug "$model")-$$" \
            -v "$run_dir/ws:/workspace" \
            -v "$run_dir/cfg:/home/ubuntu/.config/opencode" \
            -v "$run_dir/data:/home/ubuntu/.local/share/opencode" \
            -w /workspace -e HOME=/home/ubuntu -e PI_NO_PTY=1 \
            -e OPENROUTER_API_KEY="$key" -e OPENCODE_CONFIG_CONTENT="$cfg_json" \
            subdollar-sandbox opencode run --standalone --format json \
            --session "$session_id" \
            -m "openrouter/$id" --auto -- "$oc_tool_prompt" \
            > "$events" 2> "$log"
        rc=$?
        set -e
        db="$run_dir/data/opencode.db"
        if [[ "$rc" -ne 0 || ! -s "$db" ]]; then
            {
                echo "model:    $id"
                echo "sent:     openrouter/$id"
                echo "served:   (opencode failed)"
                echo "provider: (not queried)"
                echo "bill:     (not queried)"
                echo "time:     $time_utc"
                echo "image:    $image"
                echo "RESULT: FAIL (opencode exit $rc; no session db)"
            } > "$record"
            echo "[model-check] FATAL: opencode exit $rc; no session db ($record)" >&2
            tail -n 20 "$log" >&2 || true
            exit 1
        fi
    fi
    # R79-18: the usage counter is not monotonic (stale reads can fall
    # back) and a session's generations land at different times, so a
    # single rise is not the bill. Poll until the delta is unchanged for
    # 30 s (six reads in a row) within the 180 s bound; the record names
    # the read count. A series that never settles fails.
    local usage_settled=0 usage_reads=0 usage_last=""
    local -a usage_deltas=()
    usage_delta() {
        python3 -c 'import sys; print("%.10f" % max(0.0, float(sys.argv[2]) - float(sys.argv[1])))' \
            "$usage_before" "$1"
    }
    usage_settles() {
        local n=${#usage_deltas[@]} first i
        [[ "$n" -ge 6 ]] || return 1
        first="${usage_deltas[$((n - 6))]}"
        # A $0 delta is the lag (or a genuine $0 bill, which R79-16
        # fails); it must not settle the poll.
        python3 -c 'import sys; raise SystemExit(0 if float(sys.argv[1]) > 0 else 1)' \
            "$first" || return 1
        for ((i = n - 5; i < n; i++)); do
            [[ "${usage_deltas[$i]}" == "$first" ]] || return 1
        done
        return 0
    }
    if [[ -n "${SDB_MODEL_CHECK_USAGE_AFTER:-}" ]]; then
        # The self-test's canned comma-separated series; no sleeps.
        local value
        local -a usage_series
        IFS=',' read -r -a usage_series <<<"$SDB_MODEL_CHECK_USAGE_AFTER"
        for value in "${usage_series[@]}"; do
            value="${value// /}"
            [[ -n "$value" ]] || continue
            usage_reads=$((usage_reads + 1))
            usage_last="$value"
            usage_deltas+=("$(usage_delta "$value")")
            if usage_settles; then
                usage_settled=1
                break
            fi
        done
    else
        local value
        for _ in $(seq 1 36); do
            value="$(usage_query)" || value=""
            if [[ -n "$value" ]]; then
                usage_reads=$((usage_reads + 1))
                usage_last="$value"
                usage_deltas+=("$(usage_delta "$value")")
                if usage_settles; then
                    usage_settled=1
                    break
                fi
            fi
            sleep 5
        done
    fi
    if [[ "$usage_reads" -eq 0 || -z "$usage_last" ]]; then
        echo "[model-check] FATAL: cannot read the OpenRouter key usage after the run" >&2
        exit 1
    fi
    usage_after="$usage_last"
    bill="$(python3 -c 'import sys; print(max(0.0, float(sys.argv[2]) - float(sys.argv[1])))' \
        "$usage_before" "$usage_after")"
    local usage_note
    if [[ "$usage_settled" == "1" ]]; then
        usage_note="settled after $usage_reads reads (delta \$$(python3 -c 'import sys; print("%.8f" % float(sys.argv[1]))' "$bill"))"
    else
        usage_note="not settled after $usage_reads reads (last delta \$$(python3 -c 'import sys; print("%.8f" % float(sys.argv[1]))' "$bill"))"
    fi
    # R79-4: the negative probe. A wrong pin must be refused by
    # OpenRouter's routing at $0 (no inference is billed); if the wrong-pin
    # run succeeds, opencode did not forward the pin and the positive run
    # proves nothing. A negative run that fails for another reason is
    # inconclusive, not a pass.
    local wrong_pin="novita" neg_events="$work/opencode-negative.json" neg_rc=0 neg_ok=0 neg_line
    local neg_dir="$work/opencode-negative"
    if [[ "$pin" == "novita" ]]; then
        wrong_pin="baidu"
    fi
    if [[ -z "$pin" ]]; then
        neg_line="negative: no pin (skipped)"
    else
        if [[ -n "${SDB_MODEL_CHECK_OPENCODE_NEGATIVE_RC:-}" ]]; then
            neg_rc="$SDB_MODEL_CHECK_OPENCODE_NEGATIVE_RC"
            if [[ -n "${SDB_MODEL_CHECK_OPENCODE_NEGATIVE_EVENTS:-}" ]]; then
                neg_events="$SDB_MODEL_CHECK_OPENCODE_NEGATIVE_EVENTS"
            fi
        else
            local neg_cfg
            mkdir -p "$neg_dir/ws" "$neg_dir/cfg" "$neg_dir/data"
            neg_cfg="$(opencode_config_json "$id" "$wrong_pin")"
            set +e
            timeout 300 docker run --rm --name "sdb-model-check-oc-neg-$(model_slug "$model")-$$" \
                -v "$neg_dir/ws:/workspace" \
                -v "$neg_dir/cfg:/home/ubuntu/.config/opencode" \
                -v "$neg_dir/data:/home/ubuntu/.local/share/opencode" \
                -w /workspace -e HOME=/home/ubuntu -e PI_NO_PTY=1 \
                -e OPENROUTER_API_KEY="$key" -e OPENCODE_CONFIG_CONTENT="$neg_cfg" \
                subdollar-sandbox opencode run --standalone --format json \
                --session "ses_model_check_neg_$(model_slug "$model")_$$" \
                -m "openrouter/$id" --auto -- "Reply with exactly: ok" \
                > "$neg_events" 2> "$work/opencode-negative.log"
            neg_rc=$?
            set -e
        fi
        if [[ "$neg_rc" -eq 0 ]]; then
            neg_line="negative: pin $wrong_pin was served (opencode did not forward the pin)"
        elif grep -q "No allowed providers are available" "$neg_events" 2>/dev/null \
            && grep -q "permits only: $wrong_pin" "$neg_events" 2>/dev/null; then
            neg_ok=1
            neg_line="negative: pin $wrong_pin refused by OpenRouter's routing (exit $neg_rc)"
        else
            neg_line="negative: pin $wrong_pin was inconclusive (exit $neg_rc; no routing refusal)"
        fi
    fi
    local verdict
    verdict="$(python3 - "$db" "$id" "$rate_prompt" "$rate_cache_read" \
        "$rate_cache_write" "$rate_completion" "$bill" "$session_id" <<'PY'
import json, sqlite3, sys

(db, model_id, rate_prompt, rate_cache_read, rate_cache_write,
 rate_completion, bill_arg, session_id) = sys.argv[1:9]
bill = float(bill_arg)
con = sqlite3.connect(db)
row = con.execute(
    "select model, cost, tokens_input, tokens_output, tokens_reasoning, "
    "tokens_cache_read, tokens_cache_write, idle_outcome "
    "from session_v2 where id = ?", (session_id,)).fetchone()
if not row:
    print(json.dumps({
        "sent": "openrouter/" + model_id, "served": "", "provider": "",
        "bill": bill, "session_cost": 0.0, "expected": 0.0, "ratio": None,
        "bill_ok": None, "tokens_fresh": 0, "tokens_input": 0,
        "tokens_output": 0, "tokens_reasoning": 0, "tokens_cached": 0,
        "tokens_completion": 0, "tokens_write": 0, "tools": [],
        "tool_ok": False, "outcome": "",
        "reasons": ["the positive session %s is not in session_v2" % session_id],
        "ok": False,
    }))
    raise SystemExit(0)
model = json.loads(row[0] or "{}")
served = model.get("id") or ""
provider = model.get("providerID") or ""
session_cost = float(row[1] or 0.0)
tokens_input = int(row[2] or 0)
tokens_output = int(row[3] or 0)
tokens_reasoning = int(row[4] or 0)
tokens_cached = int(row[5] or 0)
tokens_write = int(row[6] or 0)
outcome = row[7] or ""
tools = []
for (data,) in con.execute(
        "select data from session_message where type='assistant' and session_id = ?",
        (session_id,)):
    try:
        d = json.loads(data or "{}")
    except Exception:
        continue
    for part in d.get("content") or []:
        if not isinstance(part, dict) or part.get("type") != "tool":
            continue
        st = part.get("state") or {}
        text = "".join(
            c.get("text") or "" for c in (st.get("content") or [])
            if isinstance(c, dict))
        tools.append({
            "name": part.get("name") or "",
            "status": st.get("status") or "",
            "text": text,
        })
reasons = []
sent = "openrouter/" + model_id
if served.lower() != model_id.lower():
    reasons.append("served %s, want %s" % (served or "(none)", model_id))
if provider != "openrouter":
    reasons.append("provider %s, want openrouter" % (provider or "(none)"))
completion = tokens_output + tokens_reasoning
# R79-16: a positive run with no tokens or a $0 bill is not a pass; the
# outcome check alone would hide it.
total_tokens = (
    tokens_input + tokens_output + tokens_reasoning + tokens_cached + tokens_write
)
if total_tokens == 0:
    reasons.append("the positive session has 0 tokens")
if bill <= 0:
    reasons.append(
        "bill $%.8f is $0 (the OpenRouter usage lags or the run was not billed)"
        % bill)
# R79-5: opencode's session_v2.tokens_input is the fresh input; the
# cached tokens are reported apart (the step's input 125, cache read 5312).
tokens_fresh = tokens_input
expected = (
    tokens_fresh * float(rate_prompt)
    + tokens_cached * float(rate_cache_read)
    + tokens_write * float(rate_cache_write)
    + completion * float(rate_completion)
) / 1_000_000
ratio = None
bill_ok = None
if expected > 0:
    ratio = bill / expected
    # R79-5a: a zero delta or a bill below the floor is the lagging usage,
    # not a cheap call.
    bill_ok = ratio <= 1.25 and ratio >= 0.1
    if not bill_ok:
        if ratio < 0.1:
            reasons.append("bill $%.8f is %.3fx the snapshot $%.8f, below the 0.1x floor (the OpenRouter usage lags?)" % (bill, ratio, expected))
        else:
            reasons.append("bill $%.8f is %.2fx the snapshot $%.8f" % (bill, ratio, expected))
tool_ok = any(
    t["status"] in ("completed", "success") and "probe-forty-two" in t["text"]
    for t in tools)
if not tool_ok:
    reasons.append("tool-call round trip failed")
if outcome and outcome != "succeeded":
    reasons.append("session outcome %s" % outcome)
print(json.dumps({
    "sent": sent, "served": served, "provider": provider, "bill": bill,
    "session_cost": session_cost,
    "expected": expected, "ratio": ratio, "bill_ok": bill_ok,
    "tokens_fresh": tokens_fresh, "tokens_input": tokens_input,
    "tokens_output": tokens_output, "tokens_reasoning": tokens_reasoning,
    "tokens_cached": tokens_cached,
    "tokens_completion": completion, "tokens_write": tokens_write,
    "tools": tools, "tool_ok": tool_ok, "outcome": outcome,
    "reasons": reasons, "ok": not reasons,
}))
PY
)"
    local ok sent served provider bill expected ratio tokens_line reasons
    ok="$(python3 -c 'import json,sys; print("1" if json.loads(sys.argv[1])["ok"] else "0")' "$verdict")"
    sent="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["sent"])' "$verdict")"
    served="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["served"])' "$verdict")"
    provider="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["provider"])' "$verdict")"
    bill="$(python3 -c 'import json,sys; print("%.8f" % json.loads(sys.argv[1])["bill"])' "$verdict")"
    expected="$(python3 -c 'import json,sys; print("%.8f" % json.loads(sys.argv[1])["expected"])' "$verdict")"
    ratio="$(python3 -c 'import json,sys; v=json.loads(sys.argv[1]); print("" if v["ratio"] is None else "%.3f" % v["ratio"])' "$verdict")"
    tokens_line="$(python3 -c 'import json,sys; v=json.loads(sys.argv[1]); print("%d fresh, %d cached, %d completion, %d cache write" % (v["tokens_fresh"], v["tokens_cached"], v["tokens_completion"], v["tokens_write"]))' "$verdict")"
    raw_row="$(python3 -c 'import json,sys; v=json.loads(sys.argv[1]); print("input %d, output %d, reasoning %d, cache read %d, cache write %d, cost %.8f" % (v["tokens_input"], v["tokens_output"], v["tokens_reasoning"], v["tokens_cached"], v["tokens_write"], v["session_cost"]))' "$verdict")"
    reasons="$(python3 -c 'import json,sys; print("; ".join(json.loads(sys.argv[1])["reasons"]))' "$verdict")"
    if [[ -n "$pin" && "$neg_ok" != "1" ]]; then
        ok=0
        if [[ -n "$reasons" ]]; then
            reasons="$reasons; $neg_line"
        else
            reasons="$neg_line"
        fi
    fi
    if [[ "$usage_settled" != "1" ]]; then
        ok=0
        if [[ -n "$reasons" ]]; then
            reasons="$reasons; usage $usage_note"
        else
            reasons="usage $usage_note"
        fi
    fi
    {
        echo "model:    $id"
        echo "sent:     $sent"
        echo "served:   $served"
        echo "session-id: $session_id"
        if [[ -n "$pin" ]]; then
            echo "provider: $provider (sent pin: $pin)"
        else
            echo "provider: $provider (no pin sent)"
        fi
        echo "bill:     \$$bill (OpenRouter key usage; opencode session \$$(python3 -c 'import json,sys; print("%.8f" % json.loads(sys.argv[1])["session_cost"])' "$verdict"); snapshot \$$expected, ratio $ratio)"
        echo "usage:    $usage_note"
        echo "tokens:   $tokens_line (opencode session)"
        echo "session:  $raw_row (session_v2)"
        echo "tool-calls: $(python3 -c 'import json,sys; v=json.loads(sys.argv[1]); print("%d tool part(s)" % len(v["tools"]))' "$verdict")"
        echo "$neg_line"
        echo "time:     $time_utc"
        echo "image:    $image"
        if [[ "$ok" == "1" ]]; then
            echo "RESULT: PASS"
        else
            echo "RESULT: FAIL ($reasons)"
        fi
    } > "$record"
    if [[ "$ok" == "1" ]]; then
        echo "[model-check] PASS (opencode): $id sent exactly, session model $served ($record)"
        return 0
    fi
    echo "[model-check] FATAL (opencode): $reasons ($record)" >&2
    return 1
}

# R79 F3: the opencode harness has its own check and record.
if [[ "$harness" == "opencode" ]]; then
    opencode_check
    exit $?
fi

if ! run_omp reply "Reply with exactly: ok" "read" "$session_file"; then
    {
        echo "model:    $id"
        echo "sent:     (omp failed)"
        echo "served:   (not queried)"
        echo "provider: (not queried)"
        echo "bill:     (not queried)"
        echo "time:     $time_utc"
        echo "image:    $image"
        echo "RESULT: FAIL ($RUN_ERROR)"
    } > "$record"
    echo "[model-check] FATAL: $RUN_ERROR ($record); output tail:" >&2
    tail -n 20 "$work/reply.log" >&2 || true
    exit 1
fi
session_file="$SESSION_FILE"

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

# R75 F4: the generation record is eventually consistent: a query right
# after the call can answer 404 "not found" (live 2026-11-09). Retry for
# up to 60 s; if it is still missing, run the tool-call round trip and
# retry once more for 60 s (the record can take minutes), then FAIL with
# that in the record.
fetch_generation() {
    local attempts="$1" attempt rc
    attempt=0
    while :; do
        attempt=$((attempt + 1))
        set +e
        printf 'header = "Authorization: Bearer %s"\n' "$key" \
            | curl -sS -K - -G https://openrouter.ai/api/v1/generation \
                --data-urlencode "id=$response_id" -o "$generation_file"
        rc=$?
        set -e
        if [[ "${SDB_MODEL_CHECK_DEBUG:-0}" == "1" ]]; then
            echo "[model-check] generation attempt $attempt bytes: $(wc -c < "$generation_file")" >&2
            head -c 240 "$generation_file" >&2 || true
            echo >&2
        fi
        if [[ "$rc" -eq 0 ]] && python3 - "$generation_file" <<'PY'
import json, sys
try:
    v = json.load(open(sys.argv[1]))
except Exception:
    raise SystemExit(1)
data = v.get("data") or {}
raise SystemExit(0 if data.get("id") else 1)
PY
        then
            return 0
        fi
        if [[ "$attempt" -ge "$attempts" ]]; then
            GEN_ERROR="generation API exit $rc"
            return 1
        fi
        sleep 1
    done
}

gen_missing=0
gen_note=""
if [[ -z "$generation_file" ]]; then
    generation_file="$work/generation.json"
fi
if [[ -n "${SDB_MODEL_CHECK_GENERATION_JSON:-}" ]]; then
    : # canned by the self-test
elif ! fetch_generation 60; then
    gen_missing=1
    gen_note="missing after 60 s; retried for 60 s after the tool calls"
fi

# R68 F3: the tool-call round trip, twice. The prompt needs two bash calls
# with arguments; the first command's output must appear and no provider
# tool-call error may. This is the minimax shape (#29): a provider that
# returns malformed tool calls still answers the reply-ok prompt.
tool_prompt='Use the bash tool twice: first run `echo probe-forty-two`, then run `ls -1 /etc | head -3`. Then reply with the output of the first command.'
tool_lines=()
tool_outputs=0
tool_errors=0
tool_ok=1
for i in 1 2; do
    canned_var="tool_canned_$i"
    canned="${!canned_var:-}"
    if ! run_omp "tool-$i" "$tool_prompt" "read,bash" "$canned"; then
        tool_lines+=("tool-call $i: output=no error=yes ($RUN_ERROR)")
        tool_errors=$((tool_errors + 1))
        tool_ok=0
        continue
    fi
    tool_session="$SESSION_FILE"
    tool_log="$work/tool-$i.log"
    output=no
    error=no
    if tool_result_ok "$tool_session"; then
        output=yes
    fi
    if grep -qE 'provider stream ended with an error|missing required arguments' \
        "$tool_session" "$tool_log" 2>/dev/null; then
        error=yes
    fi
    tool_lines+=("tool-call $i: output=$output error=$error")
    [[ "$output" == "yes" ]] && tool_outputs=$((tool_outputs + 1))
    [[ "$error" == "yes" ]] && tool_errors=$((tool_errors + 1))
    [[ "$output" == "yes" && "$error" == "no" ]] || tool_ok=0
done

if [[ "$gen_missing" == "1" ]]; then
    fetch_generation 60 || true
fi

if [[ ! -s "$generation_file" ]] || ! python3 - "$generation_file" <<'PY'
import json, sys
try:
    v = json.load(open(sys.argv[1]))
except Exception:
    raise SystemExit(1)
data = v.get("data") or {}
raise SystemExit(0 if data.get("id") else 1)
PY
then
    {
        echo "model:    $id"
        echo "sent:     $(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["sent"])' "$parsed")"
        echo "served:   (generation query failed)"
        echo "provider: (not queried)"
        echo "bill:     (not queried)"
        echo "canonical: $canonical"
        if [[ -n "$gen_note" ]]; then
            echo "generation: $gen_note"
        fi
        for line in "${tool_lines[@]}"; do
            echo "$line"
        done
        echo "tool-calls: $tool_outputs/2 output, $tool_errors/2 errors"
        echo "time:     $time_utc"
        echo "image:    $image"
        echo "RESULT: FAIL (${GEN_ERROR:-generation API}; ${gen_note:-no record})"
    } > "$record"
    echo "[model-check] FATAL: ${GEN_ERROR:-generation API}; ${gen_note:-no record} ($record)" >&2
    exit 1
fi

verdict="$(
    python3 - "$id" "$canonical" "$pin" "$found" "$rate_prompt" \
        "$rate_cache_read" "$rate_cache_write" "$rate_completion" \
        "$parsed" "$generation_file" <<'PY'
import json, re, sys

(model_id, canonical, pin, found, rate_prompt, rate_cache_read,
 rate_cache_write, rate_completion, parsed_path, generation_path) = sys.argv[1:11]
parsed = json.loads(parsed_path)
gen = json.load(open(generation_path)).get("data") or {}

sent = (parsed.get("sent") or "").removeprefix("openrouter/")
served = gen.get("model") or ""
provider_name = gen.get("provider_name") or ""
response_id = parsed.get("responseId") or ""
gen_id = gen.get("id") or ""
fallback = bool(parsed.get("fallback"))
bill = float(gen.get("total_cost") or 0.0)


def num(value):
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


# R75 F4: the provider's own counts, the same basis the bill is charged
# on. tokens_prompt/tokens_completion are OpenRouter's tokenizer counts;
# the native counts are what the provider billed (live 2026-11-09: an
# Anthropic reply billed 18 prompt + 4 completion where OpenRouter
# counted 6 + 1).
tokens_prompt = num(gen.get("native_tokens_prompt")) or num(gen.get("tokens_prompt"))
tokens_cached = num(gen.get("native_tokens_cached")) or num(gen.get("tokens_cached"))
tokens_completion = num(gen.get("native_tokens_completion")) or num(gen.get("tokens_completion"))
tokens_write = num(gen.get("native_tokens_cache_write")) or num(gen.get("tokens_cache_write"))
# native_tokens_prompt is the TOTAL prompt, cached and written tokens
# included; the calculator's prompt_tokens is the fresh part. Live
# 2026-11-09, gpt-6-luna-pro: 8456 native prompt, 2528 cached, and
# $0.00064058 = 5928*0.1 + 2528*0.01 + 45*0.5 per million.
tokens_fresh = max(0, tokens_prompt - tokens_cached - tokens_write)
priced = found == "1"
pinned = bool(pin)

reasons = []
sent_ok = sent.lower() == model_id.lower()
if not sent_ok:
    reasons.append("sent %s, want %s" % (sent or "(none)", model_id))
if fallback:
    reasons.append("omp resolved a fallback model")
served_ok = bool(canonical) and served.lower() == canonical.lower()
if not served_ok:
    reasons.append("served %s, want canonical %s" % (served or "(none)", canonical or "(none)"))
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
expected = (
    tokens_fresh * float(rate_prompt)
    + tokens_cached * float(rate_cache_read)
    + tokens_write * float(rate_cache_write)
    + tokens_completion * float(rate_completion)
) / 1_000_000
ratio = None
if priced and expected > 0:
    ratio = bill / expected
    bill_ok = ratio <= 1.25
    if not bill_ok:
        reasons.append("bill $%.8f is %.2fx the snapshot $%.8f" % (bill, ratio, expected))

print(json.dumps({
    "sent": sent,
    "served": served,
    "canonical": canonical,
    "provider_name": provider_name,
    "pinned": pinned,
    "priced": priced,
    "provider_ok": provider_ok,
    "bill": bill,
    "expected": expected,
    "ratio": ratio,
    "bill_ok": bill_ok,
    "tokens_fresh": tokens_fresh,
    "tokens_prompt": tokens_prompt,
    "tokens_cached": tokens_cached,
    "tokens_completion": tokens_completion,
    "tokens_write": tokens_write,
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
tokens_line="$(python3 -c 'import json,sys; v=json.loads(sys.argv[1]); print("%d fresh, %d cached, %d completion, %d cache write" % (v["tokens_fresh"], v["tokens_cached"], v["tokens_completion"], v["tokens_write"]))' "$verdict")"
pinned="$(python3 -c 'import json,sys; print("1" if json.loads(sys.argv[1])["pinned"] else "0")' "$verdict")"
priced="$(python3 -c 'import json,sys; print("1" if json.loads(sys.argv[1])["priced"] else "0")' "$verdict")"
reasons="$(python3 -c 'import json,sys; print("; ".join(json.loads(sys.argv[1])["reasons"]))' "$verdict")"

if [[ "$tool_ok" != "1" ]]; then
    ok=0
    if [[ -n "$reasons" ]]; then
        reasons="$reasons; tool-call round trip failed"
    else
        reasons="tool-call round trip failed"
    fi
fi

{
    echo "model:    $id"
    echo "sent:     $sent"
    echo "served:   $served"
    echo "canonical: $canonical"
    if [[ "$pinned" == "1" ]]; then
        echo "provider: $provider_name (pin: $pin)"
    else
        echo "provider: $provider_name (no pin; provider check skipped)"
    fi
    if [[ "$priced" == "1" ]]; then
        echo "bill:     \$$bill (snapshot \$$expected, ratio $ratio)"
        echo "tokens:   $tokens_line (native)"
    else
        echo "bill:     \$$bill (no snapshot row; bill check skipped)"
        echo "tokens:   $tokens_line (native)"
    fi
    if [[ -n "$gen_note" ]]; then
        echo "generation: $gen_note"
    fi
    for line in "${tool_lines[@]}"; do
        echo "$line"
    done
    echo "tool-calls: $tool_outputs/2 output, $tool_errors/2 errors"
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
