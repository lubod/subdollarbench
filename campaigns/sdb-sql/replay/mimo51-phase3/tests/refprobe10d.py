#!/usr/bin/env python3
"""Discriminating probes: subquery check order vs plain names, constant-false WHERE."""
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
]

P = [
    # discriminator: item-subquery vs FROM error
    f"SELECT (SELECT 1 FROM nopeb{S}) FROM nopea{S}",
    f"SELECT 1 FROM nopea{S} WHERE EXISTS (SELECT 1 FROM nopeb{S})",
    # subquery checks before plain errors, in later clauses?
    f"SELECT zz FROM a{S} ORDER BY (SELECT 1 FROM nope{S})",
    f"SELECT zz FROM a{S} LIMIT (SELECT 1 FROM nope{S})",
    f"SELECT zz FROM a{S} WHERE 0 AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE 0 AND zz = 1",
    f"SELECT 1 FROM a{S} WHERE 0 AND EXISTS (SELECT 1 FROM nope{S}) AND zz = 1",
    f"SELECT zz FROM a{S} WHERE 0 AND EXISTS (SELECT 1 FROM nope{S}) ORDER BY ww",
    # constant false in items vs where
    f"SELECT 0 AND EXISTS (SELECT 1 FROM nope{S}) AS c FROM a{S}",
    f"SELECT 1 AS c FROM a{S} WHERE (0 AND EXISTS (SELECT 1 FROM nope{S})) = 0",
    # arity in prepass vs plain errors
    f"SELECT zz FROM a{S} WHERE (SELECT k, w FROM b{S}) = 1",
    f"SELECT (SELECT k, w FROM b{S}) FROM a{S} WHERE zz = 1",
    f"SELECT (SELECT k, w FROM b{S}) FROM nope{S}",
    # NULL / literal variations of constant-false detection
    f"SELECT 1 FROM a{S} WHERE NULL AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE 2 AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE 0 AND EXISTS (SELECT 1 FROM nope{S}) OR 1",
    f"SELECT 1 FROM a{S} WHERE NOT EXISTS (SELECT 1 FROM nope{S}) AND 0",
    f"SELECT 1 FROM a{S} WHERE 0 = 0 AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE 0 AND 0 AND EXISTS (SELECT 1 FROM nope{S})",
    # IN/EXISTS arity
    f"SELECT 1 FROM a{S} WHERE k IN (SELECT k, w FROM b{S})",
    f"SELECT 1 FROM a{S} WHERE EXISTS (SELECT k, w FROM b{S})",
    f"SELECT 1 FROM a{S} WHERE (SELECT k, w FROM b{S}) = 1",
    f"SELECT 1 FROM a{S} WHERE (SELECT w, k FROM b{S}) > 1",
    # HAVING constant false
    f"SELECT k FROM a{S} GROUP BY k HAVING 0 AND EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT k FROM a{S} GROUP BY k HAVING EXISTS (SELECT 1 FROM nope{S})",
    # DML prepass order: SET subq vs WHERE plain
    f"UPDATE a{S} SET v = (SELECT 'x' FROM nope{S}) WHERE zz = 1",
    f"UPDATE a{S} SET zz = 1 WHERE EXISTS (SELECT 1 FROM nope{S})",
    f"DELETE FROM a{S} WHERE zz = 1 AND EXISTS (SELECT 1 FROM nope{S})",
    # outer join + correlated into join columns
    f"SELECT a{S}.k FROM a{S} JOIN b{S} ON b{S}.k = a{S}.k WHERE EXISTS (SELECT 1 FROM e{S} WHERE e{S}.k = b{S}.k) ORDER BY a{S}.k",
    f"SELECT a{S}.k FROM a{S} JOIN b{S} ON b{S}.k = a{S}.k WHERE EXISTS (SELECT 1 FROM e{S} WHERE k = 1) ORDER BY a{S}.k",
    f"SELECT a{S}.k FROM a{S} JOIN b{S} ON b{S}.k = a{S}.k WHERE EXISTS (SELECT 1 FROM e{S} WHERE w = 1) ORDER BY a{S}.k",
    # correlated into LEFT JOIN padded NULLs
    f"SELECT a{S}.k FROM a{S} LEFT JOIN b{S} ON b{S}.k = 99 WHERE EXISTS (SELECT 1 FROM e{S} WHERE e{S}.k = b{S}.k) ORDER BY a{S}.k",
    # subquery referencing outer LIMIT? no. subquery inside subquery inside list?
    f"SELECT 1 AS x WHERE k IN (1, 2) FROM a{S}",
    # IN list with subquery value inside: (SELECT ...) in list
    f"SELECT 1 AS x WHERE 1 IN ((SELECT 1), 2)",
    # scalar subquery with NULL first row / mixed
    f"SELECT (SELECT NULL FROM b{S} LIMIT 1) AS c",
    f"SELECT (SELECT v FROM a{S} WHERE k = 3) AS c",
    # INSERT SELECT failure atomicity (multi-row, bad row in middle)
    f"CREATE TABLE q{S} (k INTEGER UNIQUE)",
    f"INSERT INTO q{S} VALUES (1)",
    f"INSERT INTO q{S} SELECT k FROM a{S}",
    f"SELECT COUNT(*) AS n FROM q{S}",
    # UPDATE SET with correlated subquery
    f"UPDATE a{S} SET v = (SELECT CAST(1 AS TEXT)) WHERE k = 1",
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
