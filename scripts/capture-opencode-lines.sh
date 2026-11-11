#!/usr/bin/env bash
# R79-2: capture opencode 2.0.24's lines for the stub cases.
#
# Starts scripts/opencode-stub.py, runs the sandbox's opencode against it
# through the deprecated custom-provider baseURL (the one shape 2.0.24
# honours; the current `providers` schema has no baseURL), and writes the
# runner-shaped lines to src/campaign/fixtures/health/opencode-<case>.txt.
#
# Usage: scripts/capture-opencode-lines.sh [case ...]
#        (default: retry exhausted 402 stream_error tool ok)
# The stub uses a dummy key and is local; nothing is billed.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/src/campaign/fixtures/health"
PORT="${SDB_STUB_PORT:-18089}"
CASES=("$@")
[[ ${#CASES[@]} -gt 0 ]] || CASES=(retry exhausted 402 stream_error tool ok)
MODEL="${SDB_STUB_MODEL:-openrouter/inclusionai/ling-3.1-flash}"
mkdir -p "$OUT"
work="$(mktemp -d /tmp/sdb-oc-capture.XXXXXX)"
trap 'rm -rf "$work"' EXIT

for case in "${CASES[@]}"; do
    rm -rf "$work/ws" "$work/cfg" "$work/data" "$work/requests.jsonl"
    mkdir -p "$work/ws" "$work/cfg" "$work/data"
    python3 "$ROOT/scripts/opencode-stub.py" "$PORT" "$work/requests.jsonl" "$case" &
    stub=$!
    ready=0
    for _ in $(seq 1 50); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
            ready=1
            break
        fi
        sleep 0.1
    done
    if [[ "$ready" != "1" ]]; then
        kill "$stub" 2>/dev/null || true
        echo "[capture] the stub did not start for $case" >&2
        exit 1
    fi
    cfg="{\"provider\":{\"openrouter\":{\"id\":\"openrouter\",\"npm\":\"@openrouter/ai-sdk-provider\",\"options\":{\"baseURL\":\"http://127.0.0.1:$PORT\"}}}}"
    rc=0
    timeout 180 docker run --rm --network=host \
        -v "$work/ws:/workspace" \
        -v "$work/cfg:/home/ubuntu/.config/opencode" \
        -v "$work/data:/home/ubuntu/.local/share/opencode" \
        -w /workspace -e HOME=/home/ubuntu -e OPENROUTER_API_KEY=dummy \
        -e OPENCODE_CONFIG_CONTENT="$cfg" \
        subdollar-sandbox opencode run --standalone --format json \
        --session "ses_capture_$case" -m "$MODEL" --auto -- \
        "Reply with exactly: ok" > "$work/out.json" 2> "$work/err.log" || rc=$?
    kill "$stub" 2>/dev/null || true
    wait "$stub" 2>/dev/null || true
    python3 - "$work/out.json" "$work/err.log" "$OUT/opencode-$case.txt" "$case" "$rc" <<'PY'
import datetime
import json
import sys

out, err, dest, case, rc = sys.argv[1:6]
lines = []
turns = 0


def clock(ms):
    return datetime.datetime.fromtimestamp(ms / 1000, datetime.timezone.utc).strftime("%H:%M:%S")


raw = [l for l in open(out, errors="replace").read().splitlines() if l.strip()]
first = None
for line in raw:
    try:
        v = json.loads(line)
    except Exception:
        lines.append("[LOG] " + line[:1500])
        continue
    if first is None:
        first = v.get("timestamp")
    t = v.get("type")
    ts = clock(v.get("timestamp") or first or 0)
    if t == "step_start":
        turns += 1
        lines.append(f"[{ts}] [LIVE] Turn {turns} started")
    elif t == "tool_use":
        part = v.get("part") or {}
        tool = part.get("tool") or "tool"
        lines.append(f"[{ts}] [ACTION] {tool} {json.dumps(v, separators=(',', ':'))}")
    elif t == "text":
        text = (v.get("part") or {}).get("text") or ""
        lines.append(f"[{ts}] [RESPONSE] {text[:1500]}")
    elif t == "step_finish":
        part = v.get("part") or {}
        tok = part.get("tokens") or {}
        total = sum(int(tok.get(k) or 0) for k in ("input", "output", "reasoning"))
        cache = tok.get("cache") or {}
        total += int(cache.get("read") or 0) + int(cache.get("write") or 0)
        lines.append(f"[{ts}] [TELEMETRY] turn={turns} max_turns=0 spent={float(part.get('cost') or 0):.6f} tokens={total}")
    elif t == "error":
        error = v.get("error") or {}
        status = error.get("status") or error.get("code")
        lines.append(f"[{ts}] [LOG] {json.dumps(v, separators=(',', ':'))[:1500]}")
        if status == 402:
            lines.append(f"[{ts}] [BILLING] provider billing exhausted (HTTP 402)")
        elif status == 429:
            lines.append(f"[{ts}] [BILLING] provider rate-limited (HTTP 429)")
    else:
        lines.append(f"[{ts}] [LOG] {json.dumps(v, separators=(',', ':'))[:1500]}")

head = f"[{clock(first or 0)}] [LIVE] Agent initialized (opencode 2.0.24). Max turns: 0, Budget: $0.00"
for line in open(err, errors="replace").read().splitlines():
    if line.strip():
        lines.append("[STDERR] " + line[:1500])
with open(dest, "w") as f:
    f.write(head + "\n")
    f.write("\n".join(lines) + ("\n" if lines else ""))
print(f"[capture] {case}: rc={rc} -> {dest}")
PY
done
