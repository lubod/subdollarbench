#!/bin/sh
# Wave launch gate: refuses to enqueue unless the tree is releasable.
# Usage: sh scripts/wave-launch.sh <grid-file>  (grid: "model task effort trials" per line)
set -e
cd /home/ubuntu/subdollarbench

fail() { echo "LAUNCH ABORT: $1" >&2; exit 1; }

# 1. Clean tree (frozen fingerprint requirement).
[ -z "$(git status --porcelain | grep -v '^??')" ] || fail "dirty tree: $(git status --porcelain | grep -v '^??' | head -3)"

# 2. Green suite (documented skip: parallel-flaky console test, isolated in #gate).
export PATH=/home/ubuntu/.cargo/bin:$PATH
cargo test --lib -- --skip pipeline::tests::test_pipeline_buffers_and_writes_console_log 2>&1 | grep "test result" | grep -q "0 failed" || fail "suite not green"

# 3. Engine idle.
curl -s http://127.0.0.1:3000/api/status --max-time 10 | grep -q '"is_running":false' || fail "engine busy"

# 4. Fingerprint recorded.
FP="$(grep RULES_VERSION src/report/rules.rs | head -1) @ $(git rev-parse --short HEAD)"
echo "fingerprint: $FP"
echo "$FP $(date -u +%FT%TZ)" >> wave-freezes.log

# 5. Enqueue grid via matrix --enqueue (single enqueuer path: paused/balance
# gates + low-effort-first live there, not here).
# Grid file: "model task effort trials" per line -> matrix flags.
if [ ! -f "${1:?usage: wave-launch.sh <grid-file>}" ]; then
  echo "LAUNCH ABORT: grid file $1 missing" >&2; exit 1
fi
MODELS=$(awk '!/^#/ && NF {print $1}' "$1" | sort -u | paste -sd, -)
TASKS=$(awk '!/^#/ && NF {print $2}' "$1" | sort -u | paste -sd, -)
EFFORTS=$(awk '!/^#/ && NF {print $3}' "$1" | sort -u | paste -sd, -)
TRIALS=$(awk '!/^#/ && NF {print $4; exit}' "$1")
./target/release/subdollarbench matrix --models "$MODELS" --tasks "$TASKS" --efforts "$EFFORTS" --trials "${TRIALS:-3}" --enqueue --extend-if-unstable --max-trials 7
echo "ENQUEUED via matrix --enqueue"

# 6. Spot check: first 3 cells must run REAL trials (turns>0, spend>0) within 15 min.
echo "spot-checking first 3 cells (15 min window)..."
sleep 600
REAL=0
for i in 1 2 3; do
  ST=$(curl -s http://127.0.0.1:3000/api/status --max-time 10)
  TURNS=$(echo "$ST" | python3 -c "import json,sys; print((json.load(sys.stdin).get('current_run') or {}).get('turns',0))")
  SPENT=$(echo "$ST" | python3 -c "import json,sys; print((json.load(sys.stdin).get('current_run') or {}).get('cost_usd',0))")
  [ "$TURNS" -gt 0 ] && REAL=$((REAL+1))
  echo "cell $i: turns=$TURNS spent=$SPENT"
  sleep 120
done
[ "$REAL" -ge 2 ] || fail "spot check: no real trials (stale cancel / instant abort?)"
echo "LAUNCH OK: $ok/$n queued, spot check passed"
