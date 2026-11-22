#!/usr/bin/env python3
"""Probe round 5: HAVING-on-non-aggregate rules; check order."""
import socket, sys

HOST = sys.argv[1] if len(sys.argv) > 1 else "ref-sql"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 5480

def run(stmts, host=HOST, port=PORT):
    s = socket.create_connection((host, port), timeout=10)
    f = s.makefile("rwb")
    out = []
    for st in stmts:
        f.write(st.encode() + b"\n")
        f.flush()
        while True:
            line = f.readline()
            if not line:
                out.append("<EOF>")
                break
            l = line.decode().rstrip("\n")
            if l.startswith("ROWS"):
                out.append(l)
                while True:
                    nxt = f.readline().decode().rstrip("\n")
                    out.append(nxt)
                    if nxt.startswith("END") or not nxt:
                        break
                break
            out.append(l)
            if l.startswith(("OK", "ERR", "END")):
                break
    s.close()
    return out

setup = [
    "CREATE TABLE m (id INTEGER)",
    "INSERT INTO m VALUES (3), (9), (7), (1), (5)",
]

probes = [
    "SELECT id FROM m HAVING COUNT(*) > 1",
    "SELECT id FROM m HAVING COUNT(*) > 100",
    "SELECT id FROM m HAVING SUM(id) > 4",
    "SELECT id FROM m GROUP BY id HAVING id > 3",
    "SELECT id FROM m GROUP BY id HAVING COUNT(*) > 100",
    "SELECT id FROM m HAVING 1",
    "SELECT zz FROM m HAVING 1",
    "SELECT id FROM m GROUP BY id HAVING zz > 1",
    # scalar agg with group-by key only in order by
    "SELECT COUNT(*) FROM m ORDER BY id",
    # empty table scalar + having
    "SELECT id FROM m WHERE id > 100 HAVING COUNT(*) = 0",
]

print("\n".join(run(setup + probes)))
