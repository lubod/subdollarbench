#!/usr/bin/env python3
"""Probe round 2: aggregate edge cases against the reference."""
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
    "CREATE TABLE b (id INTEGER, s TEXT)",
]

probes = [
    # which row feeds bare columns for MIN/MAX scalar aggregates
    "SELECT s, MAX(id) FROM a",
    "SELECT s, MIN(id) FROM a",
    "SELECT s, COUNT(*) FROM a",
    # empty input with bare column
    "SELECT s, COUNT(*) FROM b",
    # sum over text
    "SELECT SUM(s) FROM a WHERE s = 'a'",
    "SELECT SUM('abc')",
    "SELECT SUM('10')",
    "SELECT SUM(NULL)",
    # avg
    "SELECT AVG(id) FROM b",
    "SELECT AVG(id) FROM a WHERE id > 100",
    "SELECT AVG(NULL)",
    # group by ordinals out of range
    "SELECT COUNT(*) FROM a GROUP BY 9",
    "SELECT COUNT(*) FROM a GROUP BY 0",
    # star variants
    "SELECT COUNT(DISTINCT *) FROM a",
    "SELECT SUM(DISTINCT id) FROM a",
    "SELECT MIN(DISTINCT id) FROM a",
    # ORDER BY expressions
    "SELECT id FROM a ORDER BY upper(s)",
    "SELECT id FROM a ORDER BY id + 1",
    "SELECT COUNT(*) FROM a ORDER BY COUNT(*) DESC",
    "SELECT id % 2 AS k, COUNT(*) FROM a GROUP BY id % 2 ORDER BY upper(s)",
    "SELECT COUNT(*) FROM a GROUP BY id ORDER BY id",
    # no FROM + group by
    "SELECT COUNT(*) GROUP BY 1",
    "SELECT COUNT(*) AS n GROUP BY n",
    # aggregate in LIMIT
    "SELECT COUNT(*) FROM a LIMIT COUNT(*)",
    # HAVING with alias in expression / no aggregate at all
    "SELECT id % 2 AS k, COUNT(*) AS n FROM a GROUP BY id % 2 HAVING n + 1 > 2 ORDER BY k",
    "SELECT id % 2 AS k, COUNT(*) AS n FROM a GROUP BY id % 2 HAVING upper('A') = 'A' ORDER BY k",
    # scalar aggregate: where drops everything + having
    "SELECT COUNT(*) FROM a WHERE id > 100 HAVING COUNT(*) > 0",
    "SELECT COUNT(*) FROM a WHERE id > 100 HAVING COUNT(*) = 0",
    # multiple group keys
    "SELECT id % 2 AS k, s, COUNT(*) FROM a GROUP BY id % 2, s ORDER BY k, s",
    # group by alias of bare column + order by alias
    "SELECT s AS v, COUNT(*) FROM a GROUP BY v ORDER BY v",
    # having with key expression
    "SELECT length(s) AS L, COUNT(*) FROM a GROUP BY length(s) HAVING length(s) > 1 ORDER BY L",
    # nested scalar inside aggregate arg
    "SELECT SUM(abs(id)) FROM a",
    "SELECT COUNT(upper(s)) FROM a",
    # min/max ties -> bare col row
    "SELECT id % 2 AS k, s, MIN(id) FROM a GROUP BY id % 2 ORDER BY k",
    # aggregate over expression yielding text
    "SELECT SUM(upper(s)) FROM a",
    # count of expression with nulls
    "SELECT COUNT(s) FROM a",
    "SELECT COUNT(id * 0) FROM a",
    # group by with distinct
    "SELECT DISTINCT s, COUNT(*) FROM a GROUP BY s ORDER BY s",
    # order by alias for aggregate output
    "SELECT COUNT(*) AS n FROM a ORDER BY n",
    # where clause aggregate after valid where
    "SELECT id FROM a WHERE id > 1 AND COUNT(*) > 0",
    # group by missing column + aggregate in select
    "SELECT COUNT(*) FROM a GROUP BY zz_nocol",
    # sum on missing column inside expression
    "SELECT SUM(id + zz) FROM a",
    # order by expression not resolvable
    "SELECT id FROM a ORDER BY zz + 1",
    # table-less order by aggregate
    "SELECT 1 AS x ORDER BY COUNT(*)",
]

print("\n".join(run(setup + probes)))
