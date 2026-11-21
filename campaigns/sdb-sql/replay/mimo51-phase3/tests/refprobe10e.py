#!/usr/bin/env python3
"""Final discriminators: prepass order, shield rules, arity contexts, DML order."""
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
    # (a) item order: plain first vs subquery prepass
    f"SELECT zz, (SELECT 1 FROM nope{S}) FROM a{S}",
    f"SELECT (SELECT 1 FROM nope{S}), zz FROM a{S}",
    # (b) baseline
    f"SELECT 1 FROM a{S} WHERE zz = 1",
    # (c) IN-arity timing vs plain
    f"SELECT zz FROM a{S} WHERE k IN (SELECT k, w FROM b{S})",
    f"SELECT 1 FROM a{S} WHERE zz = 1 AND k IN (SELECT k, w FROM b{S})",
    # (d)(e) OR-shield with literal 1
    f"SELECT 1 FROM a{S} WHERE 1 OR EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT 1 FROM a{S} WHERE EXISTS (SELECT 1 FROM nope{S}) OR 1",
    f"SELECT 1 OR EXISTS (SELECT 1 FROM nope{S}) AS c FROM a{S}",
    f"SELECT 1 FROM a{S} WHERE 1 OR zz = 1",
    # (f) shield in select-list names
    f"SELECT 0 AND zz = 1 AS c FROM a{S}",
    f"SELECT 1 AS c FROM a{S} WHERE 0 AND zz = 1",
    # (g) shield only skips the AND subterm, later clauses still resolve
    f"SELECT k FROM a{S} WHERE 0 AND zz = 1 ORDER BY ww",
    # (h)(i) DML inline order
    f"UPDATE a{S} SET v = 'x' WHERE zz = 1 AND EXISTS (SELECT 1 FROM nope{S})",
    f"DELETE FROM a{S} WHERE EXISTS (SELECT 1 FROM nope{S}) AND zz = 1",
    f"DELETE FROM a{S} WHERE zz = 1 AND EXISTS (SELECT 1 FROM nope{S})",
    f"UPDATE a{S} SET v = (SELECT 1 FROM nope{S}) WHERE zz = 1",
    # (k) shield with right literal
    f"SELECT 1 FROM a{S} WHERE zz = 1 AND 0",
    f"SELECT 1 FROM a{S} WHERE zz = 1 OR 0",
    # (l)(m)(n) two-column scalar contexts
    f"SELECT (SELECT k, w FROM b{S}) = 1 AS c FROM a{S}",
    f"SELECT (SELECT k, w FROM b{S}) + 1 AS c FROM a{S}",
    f"SELECT 1 FROM a{S} WHERE (SELECT k, w FROM b{S})",
    f"SELECT 1 FROM a{S} WHERE NOT (SELECT k, w FROM b{S})",
    f"SELECT 1 FROM a{S} WHERE (SELECT k, w FROM b{S}) IS NULL",
    f"SELECT 1 FROM a{S} WHERE k = (SELECT k, w FROM b{S})",
    f"SELECT COUNT((SELECT k, w FROM b{S})) AS c FROM a{S}",
    f"SELECT 1 FROM a{S} WHERE k IN ((SELECT k, w FROM b{S}))",
    # ORDER BY two-column scalar
    f"SELECT k FROM a{S} ORDER BY (SELECT k, w FROM b{S}) LIMIT 2",
    # LIMIT subquery arity
    f"SELECT k FROM a{S} LIMIT (SELECT k, w FROM b{S})",
    # HAVING scalar arity vs plain
    f"SELECT k FROM a{S} GROUP BY k HAVING (SELECT k, w FROM b{S}) = 1",
    f"SELECT k FROM a{S} GROUP BY k HAVING zz = 1",
    # correlated subquery inside aggregate arg
    f"SELECT SUM((SELECT w FROM b{S} WHERE b{S}.k = a{S}.k)) AS s FROM a{S}",
    # IN (list) containing subquery + plain
    f"SELECT 1 FROM a{S} WHERE k IN ((SELECT MIN(k) FROM b{S}), 3)",
    f"SELECT 1 FROM a{S} WHERE zz IN (1, 2)",
    # update set expr referencing subquery alias columns? skip.
    # EXISTS in select list arithmetic
    f"SELECT EXISTS (SELECT 1 FROM b{S}) + 1 AS c",
    f"SELECT NOT EXISTS (SELECT 1 FROM e{S}) AS c",
    # subquery in INSERT...SELECT where + probe error order
    f"CREATE TABLE t2{S} (k INTEGER, v TEXT)",
    f"INSERT INTO t2{S} SELECT zz, v FROM a{S}",
    f"INSERT INTO t2{S} SELECT k, v FROM a{S} WHERE EXISTS (SELECT 1 FROM nope{S})",
    f"SELECT COUNT(*) AS n FROM t2{S}",
    # distinct + subquery
    f"SELECT DISTINCT (SELECT MAX(k) FROM b{S}) AS c FROM a{S}",
    # text/int comparisons in IN-list across types
    f"SELECT 'abc' IN ('abc', 'def') AS c",
    f"SELECT 1 IN (1, 2) AND NULL IN (1, 2) AS c",
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
