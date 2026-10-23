#!/usr/bin/env bash
# F2 (R27): verify the committed snapshot against today's OpenRouter
# catalog. This is a deliberate network check, not part of the gate.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 1
python3 - <<'PY'
import json
import sys
import urllib.request

snapshot = json.load(open("src/cost/model_prices.json"))
try:
    with urllib.request.urlopen("https://openrouter.ai/api/v1/models", timeout=30) as r:
        catalog = json.load(r)
except Exception as exc:
    print(f"catalog fetch failed: {type(exc).__name__}: {exc}")
    sys.exit(2)

by_id = {m["id"]: m.get("pricing", {}) for m in catalog.get("data", [])}
fail = 0
for entry in snapshot["models"]:
    mid = entry["id"]
    pricing = by_id.get(mid)
    if pricing is None:
        print(f"MISSING {mid}")
        fail = 1
        continue

    def per_million(key):
        raw = pricing.get(key)
        return None if raw is None else float(raw) * 1_000_000.0

    prompt = per_million("prompt")
    completion = per_million("completion")
    cache_read = per_million("input_cache_read")
    cache_write = per_million("input_cache_write")
    # The snapshot uses the prompt rate when the catalog has no explicit
    # cache-write price; a missing cache-read field is left to the snapshot.
    if cache_write is None:
        cache_write = prompt

    checks = [
        ("prompt", entry["prompt_per_million"], prompt),
        ("completion", entry["completion_per_million"], completion),
        ("cache_write", entry["cache_write_per_million"], cache_write),
    ]
    if cache_read is not None:
        checks.append(("cache_read", entry["cache_read_per_million"], cache_read))
    diffs = [
        f"{name} snapshot={want} catalog={got}"
        for name, want, got in checks
        if want is None or got is None or abs(float(want) - float(got)) > 1e-9
    ]
    if diffs:
        print(f"DIFF {mid}: " + "; ".join(diffs))
        fail = 1
    else:
        print(f"OK {mid}")

print("RESULT: PASS" if fail == 0 else "RESULT: FAIL")
sys.exit(fail)
PY
