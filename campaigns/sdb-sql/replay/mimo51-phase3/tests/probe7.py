#!/usr/bin/env python3
"""Probe the reference server for aggregate-dialect behaviour (ticket 7)."""
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
    "CREATE TABLE t (id INTEGER, s TEXT)",
    "INSERT INTO t VALUES (1, 'a'), (2, 'bb'), (3, NULL), (4, 'A'), (5, 10)",
    "CREATE TABLE e (id INTEGER, s TEXT)",
]

probes = [
    # header naming
    "select count(*) from t",
    "SELECT COUNT(*) FROM t",
    "SELECT COUNT ( * ) FROM t",
    "SELECT Sum(id) FROM t",
    "SELECT SUM(id) AS n FROM t",
    "SELECT count(id) + 1 FROM t",
    "SELECT min(s), max(id) FROM t",
    # scalar aggregate, no FROM
    "SELECT COUNT(*)",
    "SELECT SUM(1)",
    "SELECT MIN(1)",
    # empty input
    "SELECT COUNT(*), SUM(id), MIN(id), MAX(id) FROM e",
    "SELECT COUNT(id), COUNT(s) FROM e",
    # grouped
    "SELECT id % 2 AS k, COUNT(*), SUM(id) FROM t GROUP BY id % 2 ORDER BY k",
    "SELECT s, COUNT(*) FROM t GROUP BY s ORDER BY s",
    "SELECT upper(s) AS u, COUNT(*) FROM t GROUP BY upper(s) ORDER BY u",
    "SELECT id + 3 AS p, COUNT(*) FROM t GROUP BY id + 3 ORDER BY p",
    "SELECT length(s) AS L, COUNT(*) FROM t GROUP BY length(s) ORDER BY L",
    # group key by ordinal and alias
    "SELECT id % 2 AS k, COUNT(*) FROM t GROUP BY 1 ORDER BY k",
    "SELECT id % 2 AS k, COUNT(*) FROM t GROUP BY k ORDER BY k",
    "SELECT upper(s) AS u, COUNT(*) FROM t GROUP BY u ORDER BY u",
    # HAVING
    "SELECT id % 2 AS k, COUNT(*) AS n FROM t GROUP BY id % 2 HAVING COUNT(*) > 1 ORDER BY k",
    "SELECT id % 2 AS k, COUNT(*) AS n FROM t GROUP BY id % 2 HAVING n > 1 ORDER BY k",
    "SELECT id % 2 AS k, SUM(id) AS s FROM t GROUP BY id % 2 HAVING SUM(id) > 4 ORDER BY k",
    "SELECT id % 2 AS k, MIN(id) AS mn, MAX(id) AS mx FROM t GROUP BY id % 2 HAVING MIN(id) < 2 ORDER BY k",
    "SELECT COUNT(*) FROM t HAVING COUNT(*) > 100",
    "SELECT COUNT(*) FROM t HAVING COUNT(*) > 1",
    "SELECT COUNT(*) FROM t HAVING COUNT(*) >= 0",
    "SELECT id % 2 AS k, COUNT(*) AS n FROM t GROUP BY id % 2 HAVING id % 2 = 1 ORDER BY k",
    # error probes
    "SELECT COUNT(*) FROM t GROUP BY zz_nocol",
    "SELECT SUM(zz_gone) FROM t",
    "SELECT COUNT(*) FROM t WHERE COUNT(*) > 1",
    "SELECT COUNT(*) FROM t GROUP BY COUNT(*)",
    # arity / misc errors
    "SELECT COUNT() FROM t",
    "SELECT COUNT(id, s) FROM t",
    "SELECT SUM(*) FROM t",
    "SELECT SUM() FROM t",
    "SELECT AVG(id) FROM t",
    "SELECT COUNT(DISTINCT id) FROM t",
    "SELECT COUNT(*) FROM t ORDER BY COUNT(*)",
    "SELECT sum(sum(id)) FROM t",
    # bare column with scalar aggregate
    "SELECT s, COUNT(*) FROM t",
    "SELECT s, MIN(id) FROM t",
    # HAVING without group by, bare col
    "SELECT id, COUNT(*) FROM t GROUP BY id ORDER BY id",
    # aggregates over text
    "SELECT SUM(s) FROM t",
    "SELECT SUM(s) FROM t WHERE s IS NOT NULL",
    # min/max mixed types
    "SELECT MIN(s), MAX(s) FROM t",
    # distinct + aggregate
    "SELECT DISTINCT COUNT(*) FROM t",
    # having referencing non-grouped col
    "SELECT id % 2 AS k, COUNT(*) FROM t GROUP BY id % 2 HAVING s = 'a' ORDER BY k",
    # count(*) with WHERE no table
    "SELECT COUNT(*) WHERE 0",
    "SELECT COUNT(*) WHERE 1",
    # group by on empty table
    "SELECT id, COUNT(*) FROM e GROUP BY id ORDER BY id",
    # nested aggregate inside expression in having
    "SELECT id % 2 AS k, COUNT(*) AS n FROM t GROUP BY id % 2 HAVING COUNT(*) + 1 > 2 ORDER BY k",
    # aggregate arg with expression
    "SELECT SUM(id * 2) AS x FROM t",
    "SELECT COUNT(id + 1) AS x FROM t",
    # group by column also selected bare
    "SELECT s, COUNT(*) AS n FROM t GROUP BY s HAVING COUNT(*) >= 1 ORDER BY s",
]

print("\n".join(run(setup + probes)))
