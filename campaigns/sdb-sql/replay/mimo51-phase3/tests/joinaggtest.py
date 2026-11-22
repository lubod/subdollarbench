#!/usr/bin/env python3
"""Ticket 7 differential tester for joins: candidate vs reference.

Same compare rules as aggtest: header exact, rows ordered when ORDER BY
is present (and the ordering total), multiset otherwise, ERR by class.
"""
import os
import random
import re
import socket
import sys

CAND = ("127.0.0.1", 5580)
REF = ("ref-sql", 5480)


class Conn:
    def __init__(self, addr):
        self.sock = socket.create_connection(addr, timeout=30)
        self.file = self.sock.makefile("rb")

    def ask(self, stmt):
        data = stmt if stmt.endswith("\n") else stmt + "\n"
        self.sock.sendall(data.encode("utf-8"))
        first = self.file.readline().decode()
        if first.startswith("ERR "):
            return ("ERR", first.split()[1])
        if first.startswith("OK "):
            return ("OK", first.rstrip("\n"))
        if first.startswith("ROWS "):
            hdr = self.file.readline().decode().rstrip("\n")
            rows = []
            while True:
                line = self.file.readline().decode()
                if line.startswith("END "):
                    n = int(line.split()[1])
                    break
                rows.append(line.rstrip("\n"))
            return ("ROWS", hdr, rows, n)
        raise RuntimeError("bad frame: %r" % first)


def has_order_by(stmt):
    parts = re.split(r"'(?:[^']|'')*'", stmt)
    return any(re.search(r"\border\s+by\b", p, re.I) for p in parts)


def compare(stmt, a, b):
    if a[0] != b[0]:
        return "%s: cand=%r ref=%r" % (stmt, a, b)
    if a[0] == "ERR":
        if a[1] != b[1]:
            return "%s: class cand=%s ref=%s" % (stmt, a[1], b[1])
        return None
    if a[0] == "OK":
        return None if a[1] == b[1] else "%s: %s vs %s" % (stmt, a[1], b[1])
    if a[1] != b[1]:
        return "%s: hdr cand=%r ref=%r" % (stmt, a[1], b[1])
    if a[3] != b[3]:
        return "%s: nrows cand=%d ref=%d (%r vs %r)" % (stmt, a[3], b[3], a[2], b[2])
    if has_order_by(stmt):
        if a[2] != b[2]:
            return "%s: order cand=%r ref=%r" % (stmt, a[2], b[2])
    elif sorted(a[2]) != sorted(b[2]):
        return "%s: rows cand=%r ref=%r" % (stmt, a[2], b[2])
    return None


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 8001
    suffix = os.urandom(4).hex()
    rng = random.Random(seed)
    a = "ja_" + suffix
    b = "jb_" + suffix
    c = "jc_" + suffix

    setup = [
        "CREATE TABLE %s (id INTEGER, s TEXT)" % a,
        "INSERT INTO %s VALUES (1, 'x'), (2, 'y'), (3, NULL), (2, 'z'), (5, 'x')" % a,
        "CREATE TABLE %s (k INTEGER, v INTEGER)" % b,
        "INSERT INTO %s VALUES (1, 10), (2, 20), (2, 21), (4, 40), (5, NULL)" % b,
        "CREATE TABLE %s (k INTEGER, w TEXT)" % c,
        "INSERT INTO %s VALUES (1, 'p'), (2, 'q'), (3, 'r')" % c,
    ]

    stmts = []
    A = stmts.append
    # --- comma joins, qualified columns ---
    A("SELECT %s.id, %s.k, %s.v FROM %s, %s WHERE %s.id = %s.k ORDER BY %s.id, %s.k, %s.v" % (a, b, b, a, b, a, b, a, b, b))
    A("SELECT id, v FROM %s, %s WHERE %s.id = %s.k ORDER BY id, v" % (a, b, a, b))
    A("SELECT s, v FROM %s, %s WHERE %s.id = %s.k ORDER BY s, v" % (a, b, a, b))
    A("SELECT %s.* FROM %s, %s WHERE %s.id = %s.k ORDER BY id, k, v" % (a, a, b, a, b))
    A("SELECT * FROM %s, %s WHERE %s.id = %s.k ORDER BY id, k, v" % (a, b, a, b))
    A("SELECT COUNT(*) FROM %s, %s" % (a, b))
    A("SELECT COUNT(*) FROM %s, %s WHERE %s.id = %s.k" % (a, b, a, b))
    # --- aliases ---
    A("SELECT p.id, q.v FROM %s AS p, %s AS q WHERE p.id = q.k ORDER BY p.id, q.v" % (a, b))
    A("SELECT p.id, q.v FROM %s p, %s q WHERE p.id = q.k ORDER BY p.id, q.v" % (a, b))
    # --- explicit joins ---
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.id = %s.k" % (a, b, a, b))
    A("SELECT COUNT(*) FROM %s INNER JOIN %s ON %s.id = %s.k" % (a, b, a, b))
    A("SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.id = %s.k" % (a, b, a, b))
    A("SELECT COUNT(*) FROM %s LEFT OUTER JOIN %s ON %s.id = %s.k" % (a, b, a, b))
    A("SELECT COUNT(*) FROM %s CROSS JOIN %s" % (a, b))
    A("SELECT COUNT(*) FROM %s JOIN %s" % (a, b))
    A("SELECT %s.s, %s.v FROM %s LEFT JOIN %s ON %s.id = %s.k ORDER BY %s.s, %s.v" % (a, b, a, b, a, b, a, b))
    A("SELECT %s.s, %s.v FROM %s LEFT JOIN %s ON %s.id = %s.k AND %s.v > 20 ORDER BY %s.s, %s.v" % (a, b, a, b, a, b, b, a, b))
    A("SELECT COUNT(*) FROM %s JOIN %s USING (k)" % (c, b))
    A("SELECT COUNT(*) FROM %s JOIN %s USING (k)" % (b, c))
    A("SELECT w, v FROM %s JOIN %s USING (k) ORDER BY w, v" % (c, b))
    A("SELECT * FROM %s JOIN %s USING (k) ORDER BY k, v, w" % (c, b))
    # --- three tables ---
    A("SELECT COUNT(*) FROM %s, %s, %s WHERE %s.id = %s.k AND %s.k = %s.k" % (a, b, c, a, b, b, c))
    A("SELECT %s.id, %s.v, %s.w FROM %s, %s, %s WHERE %s.id = %s.k AND %s.k = %s.k ORDER BY %s.id, %s.v, %s.w" % (a, b, c, a, b, c, a, b, b, c, a, b, c))
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.id = %s.k JOIN %s ON %s.k = %s.k" % (a, b, a, b, c, b, c))
    # --- aggregates over joins ---
    A("SELECT %s.k AS key, COUNT(*) AS n, SUM(%s.v) AS s FROM %s, %s WHERE %s.id = %s.k GROUP BY %s.k ORDER BY key" % (b, b, a, b, a, b, b))
    A("SELECT %s.s AS g, COUNT(*) AS n FROM %s, %s WHERE %s.id = %s.k GROUP BY %s.s ORDER BY g" % (a, a, b, a, b, a))
    A("SELECT %s.k AS key, COUNT(*) AS n FROM %s LEFT JOIN %s ON %s.id = %s.k GROUP BY %s.k HAVING COUNT(*) > 1 ORDER BY key" % (b, a, b, a, b, b))
    A("SELECT COUNT(*), SUM(%s.v), MIN(%s.v), MAX(%s.v) FROM %s, %s WHERE %s.id = %s.k" % (b, b, b, a, b, a, b))
    A("SELECT COUNT(*), SUM(%s.v) FROM %s LEFT JOIN %s ON %s.id = %s.k" % (b, a, b, a, b))
    A("SELECT %s.k AS key, SUM(%s.v) AS s FROM %s, %s WHERE %s.id = %s.k GROUP BY %s.k HAVING SUM(%s.v) > 20 ORDER BY key" % (b, b, a, b, a, b, b, b))
    A("SELECT %s.id AS i, COUNT(DISTINCT %s.k) AS d FROM %s, %s WHERE %s.id = %s.k GROUP BY %s.id ORDER BY i" % (a, b, a, b, a, b, a))
    A("SELECT DISTINCT %s.s FROM %s, %s WHERE %s.id = %s.k ORDER BY %s.s" % (a, a, b, a, b, a))
    A("SELECT %s.id, COUNT(*) AS n FROM %s, %s WHERE %s.id = %s.k GROUP BY %s.id ORDER BY n DESC, %s.id" % (a, a, b, a, b, a, a))
    # --- ORDER BY qualified not in select ---
    A("SELECT %s.id FROM %s, %s WHERE %s.id = %s.k ORDER BY %s.v, %s.id" % (a, a, b, a, b, b, a))
    # reversed equality, extra conjuncts, WHERE over LEFT JOIN, non-equi
    A("SELECT %s.id, %s.k FROM %s, %s WHERE %s.k = %s.id ORDER BY %s.id, %s.k" % (a, b, a, b, b, a, a, b))
    A("SELECT %s.s FROM %s, %s WHERE %s.id = %s.k AND %s.v > 15 ORDER BY %s.s" % (a, a, b, a, b, b, a))
    A("SELECT %s.s FROM %s LEFT JOIN %s ON %s.id = %s.k WHERE %s.v > 15 ORDER BY %s.s" % (a, a, b, a, b, b, a))
    A("SELECT COUNT(*) FROM %s, %s WHERE %s.id >= %s.k" % (a, b, a, b))
    A("SELECT COUNT(*) FROM %s, %s WHERE %s.s <> %s.w" % (a, c, a, c))
    A("SELECT %s.id, COUNT(*) AS n FROM %s, %s WHERE %s.id = %s.k GROUP BY %s.id ORDER BY n DESC, %s.id LIMIT 2" % (a, a, b, a, b, a, a))
    A("SELECT DISTINCT %s.v FROM %s LEFT JOIN %s ON %s.id = %s.k ORDER BY %s.v" % (b, a, b, a, b, b))
    A("SELECT %s.k AS g, COUNT(*) AS n FROM %s JOIN %s ON %s.k = %s.k GROUP BY %s.k HAVING COUNT(*) >= 1 ORDER BY g LIMIT 2" % (b, b, c, b, c, b))
    # --- LIMIT ---
    A("SELECT %s.id, %s.k FROM %s, %s WHERE %s.id = %s.k ORDER BY %s.id, %s.k LIMIT 3" % (a, b, a, b, a, b, a, b))
    # --- error probes ---
    for bad in [
        "SELECT zz FROM %s, %s" % (a, b),
        "SELECT %s.zz FROM %s, %s" % (a, a, b),
        "SELECT %sx.a FROM %s" % (a, a),
        "SELECT id FROM %s, %s" % (a, b),
        "SELECT id FROM %s, %s WHERE %s.id = %s.k" % (a, b, a, b),
        "SELECT %s.id, id FROM %s, %s WHERE %s.id = %s.k ORDER BY 1, 2" % (a, a, b, a, b),
        "SELECT COUNT(*) FROM %s, nosuch_%s" % (a, suffix),
        "SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.zz = %s.k" % (a, b, a, b),
        "SELECT COUNT(*) FROM %s JOIN %s ON" % (a, b),
        "SELECT COUNT(*) FROM %s INNER %s" % (a, b),
        "SELECT s, zz FROM %s LEFT JOIN %s ON %s.id = %s.k" % (a, b, a, b),
        "SELECT %s.id FROM %s, %s WHERE %s.id = %s.k AND COUNT(*) > 1" % (a, a, b, a, b),
        "SELECT %s.id, COUNT(*) FROM %s, %s WHERE %s.id = %s.k GROUP BY COUNT(*)" % (a, a, b, a, b),
        "SELECT %s.id FROM %s JOIN %s ON %s.id = %s.k HAVING %s.zz > 1" % (a, a, b, a, b, a),
        "SELECT %s.id FROM %s JOIN %s ON %s.id = %s.k GROUP BY %s.id ORDER BY zz" % (a, a, b, a, b, a),
        "SELECT %s.id FROM %s JOIN %s ON %s.id = %s.k JOIN %s" % (a, a, b, a, b, c),
        "SELECT %s.* FROM %s WHERE %s.id = 1" % (b, a, a),
    ]:
        A(bad)

    # --- randomized join aggregates (total ORDER BY always) ---
    joins = [
        ("%s, %s WHERE %s.id = %s.k" % (a, b, a, b), a, b),
        ("%s JOIN %s ON %s.id = %s.k" % (a, b, a, b), a, b),
        ("%s LEFT JOIN %s ON %s.id = %s.k" % (a, b, a, b), a, b),
        ("%s p, %s q WHERE p.id = q.k" % (a, b), a, b),
        ("%s, %s, %s WHERE %s.id = %s.k AND %s.k = %s.k" % (a, b, c, a, b, b, c), a, c),
    ]
    for _ in range(120):
        j, ta, tb = rng.choice(joins)
        key = rng.choice([
            "%s.k" % tb, "%s.id" % ta, "%s.v" % tb,
            "%s.id %% 2" % ta, "length(%s.s)" % a, "upper(%s.s)" % a,
        ])
        agg = rng.choice(["COUNT(*)", "SUM(%s.v)" % tb, "MIN(%s.v)" % tb,
                          "MAX(%s.v)" % tb, "COUNT(%s.v)" % tb,
                          "COUNT(DISTINCT %s.s)" % a])
        q = "SELECT %s AS g, %s AS x FROM %s GROUP BY %s" % (key, agg, j, key)
        if rng.random() < 0.4:
            q += " HAVING %s" % rng.choice([
                "COUNT(*) > 0", "COUNT(*) >= 1", "%s IS NOT NULL" % agg,
                "SUM(%s.v) IS NULL OR COUNT(*) >= 0" % tb,
            ])
        q += " ORDER BY g, x"
        stmts.append(q)

    stmts = setup + stmts
    cand = Conn(CAND)
    ref = Conn(REF)
    fails = 0
    for st in stmts:
        ra, rb = cand.ask(st), ref.ask(st)
        msg = compare(st, ra, rb)
        if msg:
            fails += 1
            print("MISMATCH: %s" % msg)
            if fails > 25:
                print("too many failures, stopping")
                break
    if fails:
        print("FAIL (%d)" % fails)
        sys.exit(1)
    print("PASS (%d statements)" % len(stmts))


if __name__ == "__main__":
    main()
