#!/usr/bin/env bash
# R79-2: capture opencode 2.0.24's lines for the stub cases.
#
# Starts scripts/opencode-stub.py, runs the sandbox's opencode against it
# through the production plural `providers` schema's `settings.baseURL`
# (the shape the runner's config uses; round 79's path), and writes the
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
    cfg="{\"providers\":{\"openrouter\":{\"settings\":{\"baseURL\":\"http://127.0.0.1:$PORT\"}}}}"
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
    # R82 F4: the raw events stay beside the fixture; the lines come
    # from the runner's own mapping (`subdollarbench opencode-map`), not
    # a Python reimplementation.
    cp "$work/out.json" "$OUT/opencode-$case.events.jsonl"
    python3 - "$work/err.log" >> "$OUT/opencode-$case.events.jsonl" <<'PY'
import json
import sys

for line in open(sys.argv[1], errors="replace").read().splitlines():
    if line.strip():
        print(json.dumps({"type": "stderr", "line": line[:1500]}))
PY
    cargo run --quiet --manifest-path "$ROOT/Cargo.toml" -- opencode-map \
        "$OUT/opencode-$case.events.jsonl" "$MODEL" > "$OUT/opencode-$case.txt"
    echo "[capture] $case: rc=$rc -> $OUT/opencode-$case.txt"
done
