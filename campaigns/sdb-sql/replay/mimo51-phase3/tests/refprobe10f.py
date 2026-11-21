#!/usr/bin/env python3
"""Final edge probes: frame ambiguity, DML views, nesting, txn, persistence."""
import random, socket, sys, subprocess, time, os

HOST = sys.argv[1] if len(sys.argv) > 1 else "ref-sql"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 5480
S = str(random.randint(10000, 99999))


def run(addr, stmts):
    s = socket.create_connection(addr, timeout=15)
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
        elif line == "":
            out.append("<EOF>")
            break
        else:
            out.append("? " + line)
    while len(out) < len(stmts):
        out.append("<MISSING>")
    s.close()
    return out


SETUP = [
    f"CREATE TABLE pa_{S} (k INTEGER, v TEXT)",
    f"CREATE TABLE pb_{S} (k INTEGER, w INTEGER)",
    f"CREATE TABLE pe_{S} (q INTEGER)",
    f"INSERT INTO pa_{S} VALUES (1, 'a'), (2, 'b')",
    f"INSERT INTO pb_{S} VALUES (1, 10), (2, 20)",
    f"INSERT INTO pe_{S} VALUES (1)",
    f"CREATE TABLE pc_{S} (k INTEGER, s TEXT)",
    f"INSERT INTO pc_{S} VALUES (1, 'x'), (2, 'y')",
]

P = [
    # 17: unqualified name missing from inner, ambiguous across the JOIN frames
    f"SELECT pa_{S}.k FROM pa_{S} JOIN pb_{S} ON pb_{S}.k = pa_{S}.k WHERE EXISTS (SELECT 1 FROM pe_{S} WHERE k = 1) ORDER BY pa_{S}.k",
    f"SELECT pa_{S}.k FROM pa_{S} JOIN pb_{S} ON pb_{S}.k = pa_{S}.k WHERE EXISTS (SELECT 1 FROM pe_{S} WHERE q = k) ORDER BY pa_{S}.k",
    f"SELECT pa_{S}.k FROM pa_{S} JOIN pb_{S} ON pb_{S}.k = pa_{S}.k WHERE EXISTS (SELECT 1 FROM pe_{S} WHERE q = pa_{S}.k) ORDER BY pa_{S}.k",
    # correlated through a LEFT JOIN pad
    f"SELECT pa_{S}.k FROM pa_{S} LEFT JOIN pb_{S} ON pb_{S}.k = 99 WHERE EXISTS (SELECT 1 FROM pe_{S} WHERE q = pb_{S}.k) ORDER BY pa_{S}.k",
    # 19: UPDATE reading its own table through a subquery (old view)
    f"UPDATE pc_{S} SET k = (SELECT COUNT(*) FROM pc_{S}) WHERE k = 1",
    f"SELECT * FROM pc_{S} ORDER BY k",
    f"UPDATE pc_{S} SET s = (SELECT MAX(s) FROM pc_{S}) WHERE k = 2",
    f"SELECT * FROM pc_{S} ORDER BY k",
    # DML subqueries combined with ORDER BY/LIMIT
    f"UPDATE pc_{S} SET k = 5 WHERE k IN (SELECT k FROM pa_{S}) ORDER BY k LIMIT 1",
    f"SELECT * FROM pc_{S} ORDER BY k",
    f"DELETE FROM pc_{S} WHERE EXISTS (SELECT 1 FROM pb_{S} WHERE pb_{S}.k = pc_{S}.k) AND k <> 5",
    f"SELECT COUNT(*) AS n FROM pc_{S}",
    # 24: empty INSERT...SELECT with a wrong-width list
    f"INSERT INTO pc_{S} SELECT k FROM pa_{S} WHERE k = 999",
    f"INSERT INTO pc_{S} (k) SELECT k FROM pa_{S} WHERE k = 999",
    f"INSERT INTO pc_{S} SELECT k, s, k FROM pa_{S} WHERE k = 999",
    f"INSERT INTO nope_{S} SELECT k FROM pa_{S}",
    f"INSERT INTO pc_{S} SELECT zz FROM pa_{S}",
    # 23: depth-3 nesting + scalar in every position
    f"SELECT (SELECT COUNT(*) FROM pa_{S} WHERE EXISTS (SELECT 1 FROM pb_{S} WHERE pb_{S}.k = pa_{S}.k AND EXISTS (SELECT 1 FROM pe_{S} WHERE q = pb_{S}.k))) AS c",
    f"SELECT 1 AS x WHERE (SELECT COUNT(*) FROM pe_{S}) IN (SELECT COUNT(*) FROM pc_{S})",
    # scalar in IN-list, EXISTS depth in select list arithmetic
    f"SELECT EXISTS (SELECT 1 FROM pe_{S}) AND (SELECT COUNT(*) FROM pa_{S}) > 1 AS c",
    # subquery over the target of INSERT...SELECT
    f"CREATE TABLE pi_{S} (n INTEGER)",
    f"INSERT INTO pi_{S} SELECT COUNT(*) FROM pa_{S} WHERE EXISTS (SELECT 1 FROM pb_{S} WHERE pb_{S}.k = pa_{S}.k)",
    f"SELECT * FROM pi_{S}",
    f"INSERT INTO pi_{S} SELECT n FROM pi_{S}",
    f"SELECT * FROM pi_{S} ORDER BY n",
    # ORDER BY / LIMIT inside the INSERT's select
    f"INSERT INTO pi_{S} SELECT k FROM pa_{S} ORDER BY k DESC LIMIT 1",
    f"SELECT * FROM pi_{S} ORDER BY n",
    # NOT IN subquery where the sub yields NULLs and matches
    f"SELECT 1 AS x WHERE 'a' NOT IN (SELECT v FROM pa_{S} WHERE v IS NOT NULL)",
    f"SELECT 1 AS x WHERE 'z' NOT IN (SELECT v FROM pa_{S})",
    f"SELECT 1 AS x WHERE 'a' IN (SELECT v FROM pa_{S})",
]

if __name__ == "__main__":
    ref = run((HOST, PORT), SETUP + P)
    cnd = run(("127.0.0.1", int(sys.argv[3]) if len(sys.argv) > 3 else 5590), SETUP + P)
    fails = 0
    for st, a, b in zip(SETUP + P, ref, cnd):
        na = a.split(" ", 2)[:2] if a.startswith("ERR") else a
        nb = b.split(" ", 2)[:2] if b.startswith("ERR") else b
        same = (na == nb) if isinstance(na, list) and a.startswith("ERR") else (a == b)
        if a.startswith("ROWS") and b.startswith("ROWS"):
            # compare as multisets (some probes lack ORDER BY)
            same = sorted(a.split("\n")[1:]) == sorted(b.split("\n")[1:])
        if not same:
            fails += 1
            print(f"** {st}\n   ref: {a!r}\n   cnd: {b!r}")
    print(f"\n{len(P) - fails}/{len(P)} match")
