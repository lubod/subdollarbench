#!/usr/bin/env python3
"""W3-shaped perf sanity: 50k x 5k equi-join + GROUP BY."""
import os, socket, sys, time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 5580
big = "wb_" + os.urandom(4).hex()
sml = "ws_" + os.urandom(4).hex()
s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
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
print(ask("CREATE TABLE %s (id INTEGER, k INTEGER, v INTEGER)" % big))
for b in range(50):
    vals = ",".join("(%d,%d,%d)" % (i, i % 5000, (i * 3) % 97) for i in range(b * 1000, b * 1000 + 1000))
    ask("INSERT INTO %s VALUES %s" % (big, vals))
print(ask("CREATE TABLE %s (id INTEGER, k INTEGER, v INTEGER)" % sml))
for b in range(5):
    vals = ",".join("(%d,%d,%d)" % (i, i, (i * 7) % 53) for i in range(b * 1000, b * 1000 + 1000))
    ask("INSERT INTO %s VALUES %s" % (sml, vals))
print("load %.2fs" % (time.time() - t0))

def timed(label, st, n=3):
    t0 = time.time()
    last = None
    for _ in range(n):
        last = ask(st)
    dt = (time.time() - t0) / n * 1000
    print("%-44s %9.2f ms  (%s)" % (label, dt, last))

timed("50k x 5k equi-join count", "SELECT COUNT(*) FROM %s, %s WHERE %s.k = %s.k" % (big, sml, big, sml))
timed("join + group by", "SELECT %s.k AS g, COUNT(*) AS n FROM %s JOIN %s ON %s.k = %s.k GROUP BY %s.k ORDER BY g" % (sml, big, sml, big, sml, sml), 3)
timed("big group by alone", "SELECT k AS g, COUNT(*) AS n FROM %s GROUP BY k ORDER BY g" % big, 3)
print("done")
