#!/usr/bin/env python3
"""Rough perf sanity: point lookups and GROUP BY on a 50k-row table."""
import os, socket, sys, time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 5580
tbl = "pf_" + os.urandom(4).hex()
s = socket.create_connection(("127.0.0.1", PORT), timeout=60)
f = s.makefile("rwb")

def ask(st):
    f.write((st if st.endswith("\n") else st + "\n").encode())
    f.flush()
    first = f.readline().decode()
    if first.startswith("ROWS "):
        while True:
            l = f.readline()
            if not l or l.startswith(b"END"):
                break
        return first.strip()
    return first.strip()

t0 = time.time()
print(ask("CREATE TABLE %s (id INTEGER, v INTEGER)" % tbl))
for b in range(50):
    vals = ",".join("(%d,%d)" % (i, (i * 7) % 10007) for i in range(b * 1000, b * 1000 + 1000))
    ask("INSERT INTO %s VALUES %s" % (tbl, vals))
print("load %.2fs" % (time.time() - t0))

def timed(label, st, n=20):
    ask(st)  # warm
    t0 = time.time()
    for _ in range(n):
        ask(st)
    dt = (time.time() - t0) / n * 1000
    print("%-46s %8.2f ms" % (label, dt))

timed("point lookup", "SELECT v FROM %s WHERE id = 43210" % tbl, 100)
timed("scalar aggregates", "SELECT COUNT(*), SUM(v), MIN(v), MAX(v) FROM %s" % tbl)
timed("group by 977 keys", "SELECT id %% 977 AS k, COUNT(*), SUM(v) FROM %s GROUP BY id %% 977 ORDER BY k" % tbl, 5)
timed("group by + having", "SELECT id %% 977 AS k, COUNT(*) AS n FROM %s GROUP BY id %% 977 HAVING COUNT(*) > 50 ORDER BY k" % tbl, 5)
timed("range order limit", "SELECT id, v FROM %s WHERE id > 40000 ORDER BY id DESC LIMIT 20" % tbl, 50)
print("done")
