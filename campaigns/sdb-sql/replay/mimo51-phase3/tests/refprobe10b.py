#!/usr/bin/env python3
"""Quick recheck of the suspicious duplicates."""
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


SETUP = [
    "CREATE TABLE t1 (k INTEGER, v TEXT)",
    "CREATE TABLE t2 (k INTEGER, w INTEGER)",
    "INSERT INTO t1 VALUES (1, 'a'), (2, 'b'), (3, NULL)",
    "INSERT INTO t2 VALUES (1, 10), (1, 11), (2, 20)",
    "CREATE TABLE empty (k INTEGER, w INTEGER)",
]
PROBES = [
    "SELECT COUNT(*) AS n FROM t1",
    "SELECT COUNT(*) AS n FROM t2",
    "SELECT * FROM t1 ORDER BY k",
    "SELECT k FROM t1 WHERE k NOT IN (SELECT k FROM t2) ORDER BY k",
    "SELECT k FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.k = t1.k) ORDER BY k",
    # error-order: outer item error vs inner table error
    "SELECT zz AS c FROM t1 WHERE EXISTS (SELECT 1 FROM nope)",
    # INSERT ... SELECT
    "CREATE TABLE tgt (k INTEGER, v TEXT)",
    "INSERT INTO tgt SELECT k, v FROM t1",
    "SELECT * FROM tgt ORDER BY k",
    "INSERT INTO tgt SELECT k + 10, upper(v) FROM t1 WHERE k = 1",
    "SELECT * FROM tgt ORDER BY k",
    "INSERT INTO tgt (k) SELECT k FROM t1 WHERE k = 3",
    "SELECT * FROM tgt ORDER BY k",
    # type checks in INSERT SELECT
    "INSERT INTO tgt SELECT v, k FROM t1 WHERE k = 1",
    "INSERT INTO tgt SELECT k, v FROM t1 WHERE k = 99",
    "SELECT COUNT(*) AS n FROM tgt",
    # arity
    "INSERT INTO tgt SELECT k, v, k FROM t1",
    "INSERT INTO tgt SELECT k FROM t1",
    # empty source
    "INSERT INTO tgt SELECT k, v FROM t1 WHERE k = 99",
    # pk autoassign from select
    "CREATE TABLE pk (id INTEGER PRIMARY KEY, v TEXT)",
    "INSERT INTO pk SELECT NULL, v FROM t1",
    "SELECT * FROM pk ORDER BY id",
    # subquery in insert select list
    "INSERT INTO tgt SELECT (SELECT COUNT(*) FROM t2), v FROM t1 WHERE k = 1",
    "SELECT * FROM tgt ORDER BY k LIMIT 3",
    # subquery in UPDATE / DELETE where
    "CREATE TABLE u (k INTEGER, hit INTEGER)",
    "INSERT INTO u VALUES (1, 0), (2, 0), (3, 0)",
    "UPDATE u SET hit = 1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.k = u.k)",
    "SELECT * FROM u ORDER BY k",
    "DELETE FROM u WHERE k IN (SELECT k FROM t2)",
    "SELECT * FROM u ORDER BY k",
    "UPDATE u SET hit = 7 WHERE k = (SELECT MAX(k) FROM t2)",
    "SELECT * FROM u ORDER BY k",
    # aggregate-wrapped scalar: NULL rows
    "SELECT k, (SELECT MAX(w) FROM t2 WHERE t2.k = t1.k + 100) AS c FROM t1 ORDER BY k",
    # scalar subquery >1 row and >1 col combos in comparison
    "SELECT 1 AS x WHERE (SELECT k FROM t2 LIMIT 1) = 1",
    # IN (list) with subquery-mixed? scalar subquery compared to list? skip
    # NOT EXISTS correlated in select item
    "SELECT k, NOT EXISTS (SELECT 1 FROM t2 WHERE t2.k = t1.k) AS c FROM t1 ORDER BY k",
    # EXISTS without parens around select? invalid
    "SELECT 1 AS x WHERE EXISTS SELECT 1 FROM t2",
    # IN with subquery parenthesized left
    "SELECT 1 AS x WHERE (1) IN (SELECT k FROM t2)",
    # ORDER BY inside subquery
    "SELECT (SELECT k FROM t2 ORDER BY w DESC LIMIT 1) AS c",
    # LIMIT inside subquery
    "SELECT (SELECT COUNT(*) FROM t2 LIMIT 1) AS c",
    # HAVING with EXISTS
    "SELECT k FROM t1 GROUP BY k HAVING EXISTS (SELECT 1 FROM t2 WHERE t2.k = t1.k) ORDER BY k",
]

if __name__ == "__main__":
    run(SETUP)
    res = run(PROBES)
    for p, r in zip(PROBES, res):
        print("SQL> " + p)
        print(r)
        print("---")
