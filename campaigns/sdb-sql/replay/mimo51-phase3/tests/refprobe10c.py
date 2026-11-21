#!/usr/bin/env python3
"""Fresh probe: semantics + error precedence (unique table names per run)."""
import random, socket, sys

HOST = sys.argv[1] if len(sys.argv) > 1 else "ref-sql"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 5480
S = str(random.randint(10000, 99999))


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
    f"CREATE TABLE a{S} (k INTEGER, v TEXT)",
    f"CREATE TABLE b{S} (k INTEGER, w INTEGER)",
    f"INSERT INTO a{S} VALUES (1, 'a'), (2, 'b'), (3, NULL)",
    f"INSERT INTO b{S} VALUES (1, 10), (1, 11), (2, 20)",
    f"CREATE TABLE e{S} (k INTEGER, w INTEGER)",
    f"CREATE TABLE n{S} (k INTEGER)",
    f"CREATE TABLE u{S} (k INTEGER UNIQUE, v TEXT NOT NULL)",
    f"INSERT INTO u{S} VALUES (1, 'x')",
]

P = [
    # ---- error precedence (single bad things) ----
    f"SELECT zz FROM a{S} WHERE 1 = 2",
    f"SELECT zz FROM a{S} WHERE ww = 1",
    f"SELECT zz FROM a{S} WHERE EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT (SELECT 1 FROM nope{S}) FROM a{S} WHERE ww = 1",
    f"SELECT zz FROM a{S} ORDER BY ww",
    f"SELECT zz FROM a{S} WHERE 1 = 2 ORDER BY ww",
    f"SELECT 1 FROM nope{S} WHERE EXISTS (SELECT 1 FROM nopeb{S})",
    f"SELECT (SELECT zz, ww FROM b{S}) FROM a{S}",
    f"SELECT (SELECT zz FROM b{S}) FROM a{S}",
    f"SELECT (SELECT zz, ww FROM nope{S}) FROM a{S}",
    f"SELECT zz FROM a{S} JOIN b{S} ON qq = 1",
    f"SELECT 1 FROM a{S} WHERE EXISTS (SELECT 1 FROM nope{S}) ORDER BY zz",
    f"SELECT 1 FROM a{S} WHERE zz = 1 AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE EXISTS (SELECT 1 FROM nope{S}) AND zz = 1",
    f"SELECT 1 FROM e{S} WHERE EXISTS (SELECT 1 FROM nope{S})",
    # ---- short-circuit / plan-time behaviour ----
    f"SELECT 1 FROM a{S} WHERE 0 AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE 1 = 2 AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE EXISTS (SELECT 1 FROM nope{S}) AND 0",
    f"SELECT 1 FROM a{S} WHERE 0 OR EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE NOT 0 AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE zz = 999 AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM e{S} WHERE 0 AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE 0 AND (SELECT zz FROM b{S}) = 1",
    f"SELECT 1 FROM a{S} WHERE 0 AND (SELECT zz, ww FROM b{S}) = 1",
    f"SELECT (SELECT zz FROM b{S}) FROM e{S}",
    f"SELECT 1 FROM a{S} WHERE 0 AND zz = (SELECT zz FROM b{S})",
    # ---- NOT EXISTS / EXISTS in select item ----
    f"SELECT k, NOT EXISTS (SELECT 1 FROM b{S} WHERE b{S}.k = a{S}.k) AS c FROM a{S} ORDER BY k",
    f"SELECT k, EXISTS (SELECT 1 FROM b{S} WHERE b{S}.k = a{S}.k) AS c FROM a{S} ORDER BY k",
    f"SELECT 1 AS x WHERE EXISTS SELECT 1 FROM b{S}",
    f"SELECT 1 AS x WHERE (1) IN (SELECT k FROM b{S})",
    f"SELECT 1 AS x WHERE 1 NOT IN (SELECT k FROM b{S})",
    # ---- subquery ORDER BY / LIMIT / DISTINCT ----
    f"SELECT (SELECT k FROM b{S} ORDER BY w DESC LIMIT 1) AS c",
    f"SELECT (SELECT DISTINCT k FROM b{S}) AS c",
    f"SELECT (SELECT COUNT(*) FROM b{S} LIMIT 1) AS c",
    f"SELECT (SELECT SUM(w) FROM b{S} GROUP BY k) AS c",
    f"SELECT (SELECT COUNT(*) FROM b{S} GROUP BY k LIMIT 1) AS c",
    # ---- HAVING / ORDER BY with subqueries ----
    f"SELECT k FROM a{S} GROUP BY k HAVING EXISTS (SELECT 1 FROM b{S} WHERE b{S}.k = a{S}.k) ORDER BY k",
    f"SELECT k FROM a{S} GROUP BY k HAVING k IN (SELECT k FROM b{S}) ORDER BY k",
    f"SELECT k FROM a{S} ORDER BY (SELECT COUNT(*) FROM b{S}) LIMIT 2",
    f"SELECT k FROM a{S} ORDER BY k + (SELECT MIN(w) FROM b{S}) LIMIT 2",
    f"SELECT k FROM a{S} WHERE (SELECT COUNT(*) FROM b{S} WHERE b{S}.k = a{S}.k) > 0 ORDER BY k",
    f"SELECT k FROM a{S} LIMIT (SELECT COUNT(*) FROM b{S})",
    # ---- IN with subquery in select item ----
    f"SELECT k IN (SELECT k FROM b{S}) AS c FROM a{S} ORDER BY k",
    f"SELECT k NOT IN (SELECT k FROM b{S}) AS c FROM a{S} ORDER BY k",
    f"SELECT NULL IN (SELECT k FROM b{S}) AS c",
    f"SELECT NULL NOT IN (SELECT k FROM b{S}) AS c",
    f"SELECT 1 IN (SELECT k FROM e{S}) AS c",
    f"SELECT 1 NOT IN (SELECT k FROM e{S}) AS c",
    f"SELECT NULL IN (1, NULL) AS c",
    f"SELECT NULL NOT IN (1, NULL) AS c",
    f"SELECT 5 IN (1, NULL) AS c",
    f"SELECT 5 NOT IN (1, NULL) AS c",
    f"SELECT 5 IN (1, 5) AS c",
    f"SELECT 5 NOT IN (1, 5) AS c",
    f"SELECT NULL IN (1, 5) AS c",
    f"SELECT NULL NOT IN (1, 5) AS c",
    f"SELECT 'a' IN ('a', 'b') AS c",
    f"SELECT 1 IN ('1') AS c",
    # ---- DML with subqueries ----
    f"UPDATE u{S} SET v = 'q' WHERE k IN (SELECT k FROM b{S})",
    f"SELECT * FROM u{S}",
    f"DELETE FROM u{S} WHERE k = (SELECT MAX(k) FROM b{S})",
    f"SELECT COUNT(*) AS n FROM u{S}",
    f"UPDATE u{S} SET v = (SELECT 'z') WHERE k = 1",
    f"SELECT * FROM u{S}",
    f"UPDATE u{S} SET k = NULL WHERE k = 1",
    f"UPDATE u{S} SET v = 'y' WHERE k = (SELECT zz FROM b{S})",
    # ---- FROM subquery (probably unsupported) ----
    f"SELECT * FROM (SELECT 1 AS x)",
    # ---- INSERT SELECT constraints ----
    f"INSERT INTO u{S} SELECT k, v FROM a{S}",
    f"INSERT INTO u{S} SELECT k, NULL FROM a{S} WHERE k = 3",
    f"SELECT * FROM u{S} ORDER BY k",
    f"CREATE TABLE d{S} (k INTEGER, v TEXT DEFAULT 'dd')",
    f"INSERT INTO d{S} (k) SELECT k FROM a{S}",
    f"SELECT * FROM d{S} ORDER BY k",
    f"INSERT INTO d{S} SELECT k, v FROM a{S} ORDER BY k LIMIT 1",
    # ---- aggregate outer with empty group / correlated in empty-group context ----
    f"SELECT COUNT(*), (SELECT COUNT(*) FROM b{S} WHERE b{S}.k = a{S}.k) FROM a{S} WHERE k = 99",
    f"SELECT k FROM a{S} GROUP BY k HAVING (SELECT COUNT(*) FROM b{S} WHERE b{S}.k = a{S}.k) > 1 ORDER BY k",
    # ---- scalar subquery multi-row in comparison (first row?) ----
    f"SELECT 1 AS x WHERE (SELECT k FROM b{S}) = 1",
    f"SELECT 1 AS x WHERE (SELECT k FROM b{S}) = 2",
    f"SELECT 1 AS x WHERE (SELECT w FROM b{S}) = 20",
    # ---- nested correlated ----
    f"SELECT k FROM a{S} WHERE EXISTS (SELECT 1 FROM b{S} WHERE b{S}.k = a{S}.k AND EXISTS (SELECT 1 FROM a{S} x2 WHERE x2.k = a{S}.k)) ORDER BY k",
    # ---- subquery in ON ----
    f"SELECT * FROM a{S} JOIN b{S} ON b{S}.k = a{S}.k AND EXISTS (SELECT 1 FROM e{S})",
    f"SELECT * FROM a{S} JOIN b{S} ON b{S}.k = (SELECT MIN(k) FROM b{S})",
]

if __name__ == "__main__":
    r = run(SETUP)
    for x in r:
        if not x.startswith("OK"):
            print("SETUP:", x)
    res = run(P)
    for p, ans in zip(P, res):
        print("SQL> " + p)
        print(ans)
        print("---")
