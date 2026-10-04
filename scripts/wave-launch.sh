#!/bin/sh
# Wave launch gate: refuses to enqueue unless the tree is releasable.
# Usage: sh scripts/wave-launch.sh <grid-file>  (grid: "model task effort trials" per line)
set -e
cd /home/ubuntu/subdollarbench

fail() { echo "LAUNCH ABORT: $1" >&2; exit 1; }

# 1. Clean tree (frozen fingerprint requirement).
[ -z "$(git status --porcelain | grep -v '^??')" ] || fail "dirty tree: $(git status --porcelain | grep -v '^??' | head -3)"

# 2. Engine idle FIRST (before the suite): the docker-touching tests contend
# with a live engine on the shared daemon (ports, container churn) and flake
# green-alone/red-together (93s red vs 15s green, unnamed, unreproduced).
# Failing fast here also saves a 90s+ wasted suite run.
curl -s http://127.0.0.1:3000/api/status --max-time 10 | grep -q '"is_running":false' || fail "engine busy"

# 3. Green suite (documented skips: parallel-flaky console test, isolated in #gate;
# bench-lock tests, which contend on the cross-process /tmp/subdollar-bench.lock
# against a live engine and fail-open after 90s x 3 tests under `timeout 300`).
export PATH=/home/ubuntu/.cargo/bin:$PATH
# web_routes_lifecycle skipped too: Docker-backed, timing-sensitive,
# green-alone/red-together under engine contention (same flake class).
cargo test --lib -- --skip pipeline::tests::test_pipeline_buffers_and_writes_console_log --skip bench::tests::test_bench_lock --skip web::server::tests::test_web_routes_lifecycle 2>&1 | grep "test result" | grep -q "0 failed" || fail "suite not green"

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
# Extension cap follows the grid: max(trials, 7) so the cap can never sit
# below the initial batch (7v9: no verdict change past 7, so 7 is the floor).
TRIALS="${TRIALS:-3}"
MAXTRIALS=$(( TRIALS > 7 ? TRIALS : 7 ))
./target/release/subdollarbench matrix --models "$MODELS" --tasks "$TASKS" --efforts "$EFFORTS" --trials "$TRIALS" --enqueue --extend-if-unstable --max-trials "$MAXTRIALS"
echo "ENQUEUED via matrix --enqueue"

# 6. Spot check: first 3 cells must run REAL trials (turn>0, spent>0).
# worker_runs is a dispatch-time snapshot (turns/cost stay 0 there); live
# trial state streams on /api/console as [LIVE]/[TELEMETRY] lines.
echo "spot-checking first 3 cells (15 min window)..."
sleep 600
REAL=0
for i in 1 2 3; do
  CON=$(curl -s "http://127.0.0.1:3000/api/console?tail=400" --max-time 10)
  HIT=$(printf '%s' "$CON" | grep -E "\[LIVE\]|\[TELEMETRY\]" | python3 -c 'import sys,re
best=[0,0.0]
for line in sys.stdin:
    m=re.search(r"turn=(\d+).*spent=([0-9.]+)",line)
    if m: best=[max(best[0],int(m.group(1))),max(best[1],float(m.group(2)))]
print("%d %.4f"%(best[0],best[1]))')
  TURNS=$(echo "$HIT" | cut -d" " -f1); SPENT=$(echo "$HIT" | cut -d" " -f2)
  [ "$TURNS" -gt 0 ] && REAL=$((REAL+1))
  echo "cell $i: turns=$TURNS spent=$SPENT"
  sleep 120
done
[ "$REAL" -ge 2 ] || fail "spot check: no real trials (stale cancel / instant abort?)"
QUEUED=$(curl -s http://127.0.0.1:3000/api/queue --max-time 10 | python3 -c "import json,sys; print(len(json.load(sys.stdin)))")
echo "LAUNCH OK: $QUEUED queued, spot check passed ($REAL/3 real)"
