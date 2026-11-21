#!/usr/bin/env python3
"""Probe round 6: ORDER BY aggregate alone; misc."""
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
    "SELECT id FROM m ORDER BY COUNT(*)",
    "SELECT id FROM m ORDER BY COUNT(*) LIMIT 2",
    "SELECT id, COUNT(*) FROM m ORDER BY COUNT(*)",
    "SELECT id FROM m HAVING COUNT(*) > 1 ORDER BY id",
    "SELECT id FROM m GROUP BY id ORDER BY COUNT(*)",
    "SELECT MIN(*) FROM m",
    "SELECT id FROM m ORDER BY AVG(id)",
    "SELECT AVG(DISTINCT id) FROM m",
    "SELECT id FROM m ORDER BY SUM(id) + 1",
    "SELECT id, COUNT(*) FROM m GROUP BY id ORDER BY COUNT(*), id",
]

print("\n".join(run(setup + probes)))
