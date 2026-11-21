#!/usr/bin/env python3
"""Probe SUM-over-text and a few related coercion edges on both servers."""
import socket, sys

def run(addr, stmts):
    s = socket.create_connection(addr, timeout=10)
    f = s.makefile("rwb")
    out = []
    for st in stmts:
        f.write(st.encode() + b"\n")
        f.flush()
        line = f.readline().decode()
        if line.startswith("OK") or line.startswith("ERR"):
            out.append(line.rstrip("\n"))
        elif line.startswith("ROWS"):
            block = [line.rstrip("\n")]
            while True:
                l = f.readline().decode().rstrip("\n")
                block.append(l)
                if l.startswith("END"):
                    break
            out.append("\n".join(block))
        else:
            out.append("? " + line)
    s.close()
    return out

S = "sumx1"
SETUP = [
    f"CREATE TABLE {S}_t (s TEXT, i INTEGER)",
    f"INSERT INTO {S}_t VALUES ('a', 1), ('b', 2), ('', 3), ('7', 4), ('2.5', 5), (NULL, 6)",
]
PROBES = [
    f"SELECT SUM(s) AS x FROM {S}_t",
    f"SELECT SUM(s) FROM {S}_t WHERE i < 3",
    f"SELECT SUM(s) FROM {S}_t WHERE i < 1",
    f"SELECT SUM(i) AS x FROM {S}_t",
    f"SELECT (SELECT SUM(s) FROM {S}_t) AS x",
    f"SELECT (SELECT SUM(s) FROM {S}_t WHERE i < 3) AS x",
    f"SELECT SUM(s + s) AS x FROM {S}_t WHERE i < 3",
    f"SELECT 1 AS x WHERE (SELECT SUM(s) FROM {S}_t) > 0",
    f"SELECT 1 AS x WHERE (SELECT SUM(s) FROM {S}_t WHERE i < 3) <> 1",
    f"SELECT AVG(s) AS x FROM {S}_t",
    f"SELECT SUM(s) FROM {S}_t WHERE i < 3 GROUP BY i",
]
if __name__ == "__main__":
    ref = run(("ref-sql", 5480), SETUP + PROBES)
    cnd = run(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 5590), SETUP + PROBES)
    for st, a, b in zip(SETUP + PROBES, ref, cnd):
        flag = "  " if a == b else "**"
        print(f"{flag} {st}\n   ref: {a!r}\n   cnd: {b!r}")
