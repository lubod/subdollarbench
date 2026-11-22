#!/usr/bin/env python3
"""Probe round 4: min/max row feeding expressions; error precedence."""
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
    # min row = 4th (1), max row = 2nd (9), first = 300, last = 500
    "SELECT id * 100, MIN(id) FROM m",
    "SELECT id * 100, MAX(id) FROM m",
    "SELECT id * 100, MIN(id), MAX(id) FROM m",
    "SELECT id * 100, MAX(id), MIN(id) FROM m",
    "SELECT MAX(id), id * 100, MIN(id) FROM m",
    # error precedence inside WHERE
    "SELECT id FROM m WHERE COUNT(*) > 0 AND zz > 1",
    "SELECT id FROM m WHERE zz > 1 AND COUNT(*) > 0",
    "SELECT id FROM m HAVING zz > 1",
    "SELECT COUNT(*) FROM m HAVING zz > 1",
    # group by alias inside expression
    "SELECT id % 2 AS k, COUNT(*) FROM m GROUP BY k + 1",
    # order by mixed expr + word
    "SELECT id FROM m ORDER BY id * -1, id",
]

print("\n".join(run(setup + probes)))
