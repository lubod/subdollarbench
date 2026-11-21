#!/bin/bash
# Torn tail right after a transaction commit: the committed rows must survive.
set -u
cd /workspace
D=/tmp/torn6
rm -rf "$D"; mkdir -p "$D"
./start.sh --port 5607 --data "$D" >/dev/null 2>&1 &
P=$!
sleep 0.6
printf "CREATE TABLE t (id INTEGER, s TEXT)\nBEGIN\nINSERT INTO t VALUES (1, 'committed')\nINSERT INTO t VALUES (2, 'committed')\nCOMMIT\nINSERT INTO t VALUES (3, 'auto')\n" | timeout 5 nc -q 1 127.0.0.1 5607 || exit 1
sleep 0.3
kill -9 "$P"
wait "$P" 2>/dev/null
python3 - <<'EOF'
import os
p = "/tmp/torn6/sdb.log"
n = os.path.getsize(p)
with open(p, "r+b") as f:
    f.truncate(n - 300)
print("cut %d -> %d" % (n, n - 300))
EOF
./start.sh --port 5607 --data "$D" >/dev/null 2>&1 &
P=$!
sleep 0.8
echo "--- after restart:"
printf "SELECT * FROM t ORDER BY id\nBEGIN\nCOMMIT\n" | timeout 5 nc -q 1 127.0.0.1 5607
kill "$P" 2>/dev/null
wait "$P" 2>/dev/null
echo "done"
