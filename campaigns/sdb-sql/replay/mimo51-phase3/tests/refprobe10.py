#!/usr/bin/env python3
"""Probe the reference server for ticket-10 subquery semantics (raw socket)."""
import socket, sys

HOST = sys.argv[1] if len(sys.argv) > 1 else "ref-sql"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 5480


def run(stmts):
    s = socket.create_connection((HOST, PORT), timeout=10)
    f = s.makefile("rwb")
    out = []
    for st in stmts:
        f.write(st.encode() + b"\n")
        f.flush()
        # read one answer
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


CASES = [
    # (label, setup[], probe[])
    ("scalar over no rows", [], ["SELECT 1"]),
]

SETUP = [
    "CREATE TABLE t1 (k INTEGER, v TEXT)",
    "CREATE TABLE t2 (k INTEGER, w INTEGER)",
    "INSERT INTO t1 VALUES (1, 'a'), (2, 'b'), (3, NULL)",
    "INSERT INTO t2 VALUES (1, 10), (1, 11), (2, 20)",
    "CREATE TABLE empty (k INTEGER, w INTEGER)",
]

PROBES = [
    # scalar basics
    "SELECT (SELECT COUNT(*) FROM t2) AS c",
    "SELECT (SELECT MIN(w) FROM t2 WHERE k = 99) AS c",
    "SELECT (SELECT w FROM t2 WHERE k = 1) AS c",             # 2 rows -> ?
    "SELECT (SELECT k, w FROM t2 WHERE k = 1) AS c",          # 2 cols -> arity?
    "SELECT (SELECT * FROM t2 WHERE k = 1) AS c",             # star, 2 cols
    "SELECT (SELECT * FROM empty) AS c",                      # star over empty
    "SELECT (SELECT COUNT(*)) AS c",                          # no FROM
    "SELECT (SELECT 1) AS c",
    # scalar in comparison
    "SELECT 1 AS x WHERE (SELECT MAX(w) FROM t2) > 15",
    "SELECT 1 AS x WHERE (SELECT MAX(w) FROM t2) > 100",
    "SELECT 1 AS x WHERE 5 = (SELECT MIN(w) FROM t2)",
    # correlated scalar
    "SELECT k, (SELECT COUNT(*) FROM t2 WHERE t2.k = t1.k) AS c FROM t1 ORDER BY k",
    "SELECT k, (SELECT COUNT(*) FROM t2 WHERE k = k) AS c FROM t1 ORDER BY k",
    "SELECT k, (SELECT MAX(w) FROM t2 WHERE t2.k = t1.k) AS c FROM t1 ORDER BY k",
    # correlated unqualified resolves innermost first
    "SELECT k, (SELECT COUNT(*) FROM t2 WHERE t1.k = t2.k) AS c FROM t1 ORDER BY k",
    "SELECT k, (SELECT COUNT(*) FROM t2 WHERE k = t1.k) AS c FROM t1 ORDER BY k",
    # IN subquery
    "SELECT k FROM t1 WHERE k IN (SELECT k FROM t2) ORDER BY k",
    "SELECT k FROM t1 WHERE k NOT IN (SELECT k FROM t2) ORDER BY k",
    "SELECT k FROM t1 WHERE v IN (SELECT w FROM t2) ORDER BY k",
    "SELECT k FROM t1 WHERE k IN (SELECT k FROM empty) ORDER BY k",
    "SELECT k FROM t1 WHERE k NOT IN (SELECT k FROM empty) ORDER BY k",
    # IN list three-valued
    "SELECT 1 AS x WHERE NULL IN (1, 2)",
    "SELECT 1 AS x WHERE NULL IN (1, NULL)",
    "SELECT 1 AS x WHERE NULL NOT IN (1, 2)",
    "SELECT 1 AS x WHERE NULL NOT IN (1, NULL)",
    "SELECT 1 AS x WHERE 5 IN (1, NULL)",
    "SELECT 1 AS x WHERE 5 NOT IN (1, NULL)",
    "SELECT 1 AS x WHERE 5 IN ()",
    "SELECT 1 AS x WHERE 5 NOT IN ()",
    "SELECT 1 AS x WHERE NULL IN ()",
    "SELECT 1 AS x WHERE NULL NOT IN ()",
    # NOT precedence
    "SELECT 1 AS x WHERE NOT 1 IN (2, 3)",
    "SELECT 1 AS x WHERE NOT 1 IN (1, 2)",
    "SELECT 1 AS x WHERE NOT 5 IN (1, NULL)",
    # IN subquery three-valued with NULL left
    "SELECT k FROM t1 WHERE v IN (SELECT k FROM empty) ORDER BY k",
    "SELECT k FROM t1 WHERE v NOT IN (SELECT k FROM empty) ORDER BY k",
    "SELECT k FROM t1 WHERE v IN (SELECT w FROM t2) ORDER BY k",
    "SELECT k FROM t1 WHERE v NOT IN (SELECT w FROM t2) ORDER BY k",
    "SELECT k FROM t1 WHERE v IN (SELECT NULL FROM t2) ORDER BY k",
    "SELECT k FROM t1 WHERE v NOT IN (SELECT NULL FROM t2) ORDER BY k",
    "SELECT k FROM t1 WHERE NULL IN (SELECT k FROM t2) ORDER BY k",
    "SELECT k FROM t1 WHERE NULL NOT IN (SELECT k FROM t2) ORDER BY k",
    # EXISTS
    "SELECT 1 AS x WHERE EXISTS (SELECT 1 FROM t2)",
    "SELECT 1 AS x WHERE EXISTS (SELECT 1 FROM empty)",
    "SELECT 1 AS x WHERE NOT EXISTS (SELECT 1 FROM empty)",
    "SELECT 1 AS x WHERE NOT EXISTS (SELECT 1 FROM t2)",
    "SELECT k FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.k = t1.k) ORDER BY k",
    "SELECT k FROM t1 WHERE NOT EXISTS (SELECT 1 FROM t2 WHERE t2.k = t1.k) ORDER BY k",
    "SELECT k FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE k = t1.k) ORDER BY k",
    "SELECT k FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE k = 7) ORDER BY k",
    # EXISTS placement / precedence
    "SELECT 1 AS x WHERE 1 = 1 AND EXISTS (SELECT 1 FROM t2)",
    "SELECT 1 AS x WHERE EXISTS (SELECT 1 FROM t2) OR 1 = 2",
    "SELECT k FROM t1 WHERE k > 1 AND EXISTS (SELECT 1 FROM t2 WHERE t2.k = t1.k) ORDER BY k",
    # errors inside subquery, no outer row would evaluate
    "SELECT 1 AS x FROM t1 WHERE 0 AND EXISTS (SELECT 1 FROM nope)",
    "SELECT 1 AS x FROM empty WHERE EXISTS (SELECT 1 FROM nope)",
    "SELECT 1 AS x FROM empty WHERE EXISTS (SELECT 1 FROM t2 WHERE zz = 1)",
    "SELECT 1 AS x FROM empty WHERE (SELECT COUNT(zz) FROM t2) > 0",
    "SELECT (SELECT COUNT(zz) FROM t2) AS c FROM empty",
    "SELECT (SELECT zz FROM t2 LIMIT 1) AS c FROM empty",
    "SELECT (SELECT COUNT(*) FROM nope) AS c FROM empty",
    "SELECT (SELECT 1 FROM nope) AS c",
    "SELECT zz AS c FROM empty WHERE EXISTS (SELECT 1 FROM t2)",
    "SELECT 1 AS x FROM empty WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.k = zz)",
    # two-column scalar probe with outer empty
    "SELECT (SELECT k, w FROM t2) AS c FROM empty",
    "SELECT (SELECT k, w FROM empty) AS c",
    # error ordering outer vs inner
    "SELECT zz AS c FROM t1 WHERE EXISTS (SELECT 1 FROM nope)",
    "SELECT 1 AS c FROM t1 WHERE EXISTS (SELECT 1 FROM nope) ORDER BY yy",
    # subquery in select list of aggregate outer
    "SELECT COUNT(*) AS n, (SELECT COUNT(*) FROM t2) AS c FROM t1",
    "SELECT COUNT(*) AS n, (SELECT SUM(w) FROM t2 WHERE t2.k = t1.k) AS c FROM t1 WHERE k = 1",
    # subquery in HAVING / order
    "SELECT k, COUNT(*) AS n FROM t1 GROUP BY k HAVING COUNT(*) >= (SELECT 1) ORDER BY k",
    "SELECT k FROM t1 ORDER BY (SELECT COUNT(*) FROM t2) LIMIT 2",
    # qualified refs to outer from inner when inner has same column name
    "SELECT k FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.k = t1.k AND t1.v = 'a') ORDER BY k",
    "SELECT t1.k FROM t1 WHERE t1.k IN (SELECT t2.k FROM t2 WHERE t2.w >= 10) ORDER BY t1.k",
    # nested subquery
    "SELECT (SELECT COUNT(*) FROM (SELECT 1)) AS c",
    "SELECT 1 AS x WHERE 1 IN (SELECT 1)",
    "SELECT 1 AS x WHERE 1 IN (SELECT k FROM t2)",
    # aggregate-wrapped correlated, no matching outer rows
    "SELECT k FROM t1 WHERE (SELECT COUNT(*) FROM t2 WHERE t2.k = t1.k) > 1 ORDER BY k",
    # IN subquery with expression on left
    "SELECT k FROM t1 WHERE k + 1 IN (SELECT k + 1 FROM t2) ORDER BY k",
    # subquery result in arithmetic
    "SELECT (SELECT COUNT(*) FROM t2) + 1 AS c",
]

if __name__ == "__main__":
    setup = SETUP
    probes = list(PROBES)
    # run setup first (ignore answers)
    run(setup)
    res = run(probes)
    for p, r in zip(probes, res):
        print("SQL> " + p)
        print(r)
        print("---")
