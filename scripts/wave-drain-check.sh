#!/bin/sh
# Drain QA gate: verify a finished wave before rebuild/publish.
# Usage: sh scripts/wave-drain-check.sh <expected-parents>
set -e
cd /home/ubuntu/subdollarbench

fail() { echo "DRAIN FAIL: $1" >&2; exit 1; }
EXPECTED=${1:?usage: wave-drain-check.sh <expected-parents>}

# 1. Engine idle + queue drained.
curl -s http://127.0.0.1:3000/api/status --max-time 10 | grep -q '"is_running":false' || fail "engine still running"
PENDING=$(curl -s http://127.0.0.1:3000/api/queue --max-time 10 | python3 -c "import json,sys; print(sum(1 for q in json.load(sys.stdin) if q['status']=='Pending'))")
[ "$PENDING" = "0" ] || fail "$PENDING cells still pending"

# 2. Parent count.
PARENTS=$(python3 -c "import sqlite3; print(sqlite3.connect('bench.db').execute('SELECT COUNT(*) FROM parents').fetchone()[0])")
[ "$PARENTS" = "$EXPECTED" ] || fail "parents=$PARENTS, expected $EXPECTED"

# 3. Orphans + junk scan.
python3 - <<'PYEOF' || fail "orphan/junk scan failed"
import sqlite3, json
con = sqlite3.connect("bench.db")
assert con.execute("SELECT COUNT(*) FROM trials WHERE parent_id NOT IN (SELECT id FROM parents)").fetchone()[0] == 0, "orphan trials"
for (pid,) in con.execute("SELECT id FROM parents"):
    if pid.startswith("test_"):
        raise AssertionError("test row published: " + pid)
rows = [json.loads(r[0]) for r in con.execute("SELECT doc FROM parents")]
junk = [r["id"] for r in rows if not r.get("subruns") and not r.get("total_trials")]
assert not junk, "0-trial junk parents: %s" % junk[:5]
print("orphans=0 junk=0 testrows=0")
PYEOF

# 4. Cost sanity (no cell over $2; flags qwen-class outliers for review).
python3 - <<'PYEOF' || fail "cost sanity failed"
import sqlite3, json
con = sqlite3.connect("bench.db")
for (d,) in con.execute("SELECT doc FROM parents"):
    j = json.loads(d)
    c = j.get("total_cost_usd") or 0
    assert c < 2.0, "overspend cell %s: $%.2f" % (j["id"], c)
print("costs sane")
PYEOF

echo "DRAIN OK: $PARENTS parents, 0 orphans, 0 junk, costs sane"
