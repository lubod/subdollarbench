#!/usr/bin/env python3
"""Probe round 3: which row feeds expressions; error precedence; misc."""
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
    "CREATE TABLE a (id INTEGER, s TEXT)",
    "INSERT INTO a VALUES (1, 'a'), (2, 'bb'), (3, NULL), (4, 'A'), (5, '10')",
    "CREATE TABLE c (count INTEGER, sum INTEGER)",
    "INSERT INTO c VALUES (7, 8)",
]

probes = [
    "SELECT COUNT(*) FROM a GROUP BY 1",
    "SELECT id * 100, MAX(id) FROM a",
    "SELECT upper(s), MAX(id) FROM a",
    "SELECT id * 100, MIN(id), MAX(id) FROM a",
    "SELECT id * 100, COUNT(*) FROM a",
    "SELECT zz FROM a GROUP BY COUNT(*)",
    "SELECT COUNT() FROM a WHERE id > 2",
    "SELECT COUNT(*) FROM a HAVING 1",
    "SELECT s AS v, COUNT(*) FROM a GROUP BY s HAVING v = 'a' ORDER BY v",
    "SELECT COUNT(*) FROM a GROUP BY upper(s) ORDER BY upper(s)",
    "SELECT MAX(id), MIN(id), id * 100 FROM a",
    "SELECT count FROM c",
    "SELECT count, sum FROM c",
    "SELECT COUNT(*) FROM a GROUP BY id, s ORDER BY id, s",
    # error precedence: unknown col in select + aggregate group key
    "SELECT zz FROM a WHERE COUNT(*) > 1",
    "SELECT COUNT(*) FROM a WHERE zz > 1 AND COUNT(*) > 0",
    # aggregate select + unknown order term
    "SELECT COUNT(*) FROM a ORDER BY zz",
    # unknown col in group key + aggregate in having
    "SELECT COUNT(*) FROM a GROUP BY zz HAVING COUNT(*) > 0",
    # order by alias used elsewhere
    "SELECT id % 2 AS k, COUNT(*) AS n FROM a GROUP BY id % 2 ORDER BY n, k",
    # having on key with alias-less expression key
    "SELECT length(s) AS L, COUNT(*) FROM a GROUP BY length(s) HAVING length(s) IS NOT NULL ORDER BY L",
    # MIN over all-null group
    "SELECT MIN(s), MAX(s), COUNT(*) FROM a WHERE s IS NULL",
    # sum distinct text
    "SELECT SUM(DISTINCT s) FROM a",
    # group by two keys, having agg
    "SELECT id % 2 AS k, s, COUNT(*) AS n FROM a GROUP BY id % 2, s HAVING COUNT(*) > 0 ORDER BY k, s",
    # select star with group by
    "SELECT * FROM a GROUP BY id ORDER BY id",
    "SELECT * FROM a GROUP BY id",
]

print("\n".join(run(setup + probes)))
