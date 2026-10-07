#!/bin/bash
# sdb-watch: 5-minute wave health tripwire (runs via systemd timer).
# Appends to runs/.watch.log ONLY on problems or full drain. Quiet otherwise.
ROOT=/home/ubuntu/subdollarbench
LOG=$ROOT/runs/.watch.log
S=$(curl -s --max-time 10 http://127.0.0.1:3000/api/status)
I=$(curl -s --max-time 10 http://127.0.0.1:3000/api/integrity)
TS=$(date -u +%FT%TZ)
echo "$I" | grep -q '"ok":false' && echo "$TS PROBLEM integrity: $I" >> "$LOG"
C=$(curl -s --max-time 10 "http://127.0.0.1:3000/api/console?tail=200")
HIT=$(echo "$C" | grep -aE "BILLING|refused-final|ZERO TRIALS" | head -5)
[ -n "$HIT" ] && { echo "$TS PROBLEM console:" >> "$LOG"; echo "$HIT" >> "$LOG"; }
if echo "$S" | grep -q '"is_running":false'; then
  Q=$(curl -s --max-time 10 http://127.0.0.1:3000/api/queue)
  echo "$Q" | grep -q '"Pending"' || echo "$TS DRAINED: wave complete, no pending cells" >> "$LOG"
fi
