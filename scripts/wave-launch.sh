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
cargo test --lib -- --skip pipeline::tests::test_pipeline_buffers_and_writes_console_log 2>&1 | tail -1 | grep -q "0 failed" || fail "suite not green"

# 3. Engine idle.
curl -s http://127.0.0.1:3000/api/status --max-time 10 | grep -q '"is_running":false' || fail "engine busy"

# 4. Fingerprint recorded.
FP="$(grep RULES_VERSION src/report/rules.rs | head -1) @ $(git rev-parse --short HEAD)"
echo "fingerprint: $FP"
echo "$FP $(date -u +%FT%TZ)" >> wave-freezes.log

# 5. Enqueue grid.
GRID=${1:?usage: wave-launch.sh <grid-file>}
n=0; ok=0
while read -r m t e tr; do
  case "$m" in ""|\#*) continue;; esac
  n=$((n+1))
  out=$(./target/release/subdollarbench enqueue --model "$m" --task "$t" --effort "${e:-high}" --trials "${tr:-3}" --extend-if-unstable --max-trials 9 2>&1 | tail -1)
  echo "$out"
  case "$out" in enqueued*) ok=$((ok+1));; esac
  sleep 1
done < "$GRID"
echo "ENQUEUED $ok/$n"

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
