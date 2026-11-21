#!/usr/bin/env python3
"""Ticket 8 differential probe: candidate sdb vs reference.

Fixed edge-case statements first (grammar/error surface), then a randomized
join generator. Comparison: frame kind, ERR class, header exact, rows
ordered when ORDER BY is present, multiset otherwise (count-only when LIMIT
appears without ORDER BY, since row choice is unspecified).
"""
import os
import random
import re
import socket
import sys

CAND = ("127.0.0.1", int(sys.argv[2]) if len(sys.argv) > 2 else 5580)
REF = ("ref-sql", 5480)
SEED = int(sys.argv[1]) if len(sys.argv) > 1 else 8101


class Conn:
    def __init__(self, addr):
        self.sock = socket.create_connection(addr, timeout=60)
        self.file = self.sock.makefile("rb")

    def ask(self, stmt):
        data = stmt if stmt.endswith("\n") else stmt + "\n"
        self.sock.sendall(data.encode("utf-8"))
        first = self.file.readline().decode()
        if not first:
            raise RuntimeError("connection closed")
        if first.startswith("ERR "):
            return ("ERR", first.split()[1], first.rstrip("\n"))
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
        raise RuntimeError("bad frame %r" % first)


def has_order_by(stmt):
    parts = re.split(r"'(?:[^']|'')*'", stmt)
    return any(re.search(r"\border\s+by\b", p, re.I) for p in parts)


def has_limit(stmt):
    return re.search(r"\blimit\b", stmt, re.I) is not None


def compare(stmt, a, b):
    if a[0] != b[0]:
        return "%s: kind cand=%r ref=%r" % (stmt, a[:2], b[:2])
    if a[0] == "ERR":
        if a[1] != b[1]:
            return "%s: class cand=%s ref=%s (%s | %s)" % (stmt, a[1], b[1], a[2], b[2])
        return None
    if a[0] == "OK":
        return None if a[1] == b[1] else "%s: %s vs %s" % (stmt, a[1], b[1])
    if a[1] != b[1]:
        return "%s: hdr cand=%r ref=%r" % (stmt, a[1], b[1])
    if a[3] != b[3]:
        return "%s: nrows cand=%d ref=%d (%r vs %r)" % (stmt, a[3], b[3], a[2][:4], b[2][:4])
    if has_order_by(stmt):
        if a[2] != b[2]:
            return "%s: order cand=%r ref=%r" % (stmt, a[2][:8], b[2][:8])
    elif has_limit(stmt):
        return None  # unspecified row choice; count already matched
    elif sorted(a[2]) != sorted(b[2]):
        return "%s: rows cand=%r ref=%r" % (stmt, a[2][:8], b[2][:8])
    return None


def run(setup, stmts, label):
    cand, ref = Conn(CAND), Conn(REF)
    fails = 0
    for st in setup:
        for c in (cand, ref):
            r = c.ask(st)
            if r[0] != "OK":
                print("SETUP FAIL %s on %s: %r" % (st, "cand" if c is cand else "ref", r))
                return 1
    for st in stmts:
        try:
            ra = cand.ask(st)
        except Exception as e:
            print("CAND CRASH %s: %s" % (st, e))
            return 1
        rb = ref.ask(st)
        msg = compare(st, ra, rb)
        if msg:
            fails += 1
            print("MISMATCH: %s" % msg)
            if fails > 30:
                print("too many failures, stopping")
                break
    if fails:
        print("%s: FAIL (%d)" % (label, fails))
        return 1
    print("%s: PASS (%d statements)" % (label, len(stmts)))
    return 0


def main():
    sfx = os.urandom(4).hex()
    t1, t2, t3 = "f1_" + sfx, "f2_" + sfx, "f3_" + sfx
    empty = "f0_" + sfx

    setup = [
        "CREATE TABLE %s (id INTEGER, k INTEGER, s TEXT)" % t1,
        "INSERT INTO %s VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, NULL), (2, 21, 'a'), (5, 50, 'c')" % t1,
        "CREATE TABLE %s (k INTEGER, v INTEGER, id INTEGER)" % t2,
        "INSERT INTO %s VALUES (10, 100, 1), (20, 200, 2), (20, 201, 9), (40, 400, 4), (50, NULL, 5)" % t2,
        "CREATE TABLE %s (k INTEGER, w TEXT, id INTEGER)" % t3,
        "INSERT INTO %s VALUES (10, 'p', 1), (20, 'q', 2), (30, 'r', 3), (99, 'z', 9)" % t3,
        "CREATE TABLE %s (id INTEGER, v INTEGER)" % empty,
    ]

    S = []
    A = S.append
    # ---- comma joins, cross products, qualification ----
    A("SELECT * FROM %s, %s ORDER BY id, k, v, id" % (t1, t2))
    A("SELECT %s.*, %s.* FROM %s, %s WHERE %s.k = %s.k ORDER BY 1, 2, 3, 4, 5, 6" % (t1, t2, t1, t2, t1, t2))
    A("SELECT COUNT(*) FROM %s, %s" % (t1, empty))
    A("SELECT COUNT(*) FROM %s, %s" % (empty, t1))
    A("SELECT * FROM %s LEFT JOIN %s ON %s.k = %s.k ORDER BY id, k, v" % (t1, empty, t1, empty))
    A("SELECT * FROM %s LEFT JOIN %s ON %s.k = %s.k ORDER BY id, k, v" % (empty, t1, empty, t1))
    A("SELECT %s.id, %s.v FROM %s LEFT JOIN %s ON %s.k = %s.k WHERE %s.v IS NULL ORDER BY %s.id" % (t1, t2, t1, t2, t1, t2, t2, t1))
    A("SELECT DISTINCT %s.s FROM %s, %s WHERE %s.k = %s.k ORDER BY %s.s" % (t1, t1, t2, t1, t2, t1))
    A("SELECT DISTINCT %s.s, %s.w FROM %s JOIN %s ON %s.k = %s.k ORDER BY %s.s, %s.w" % (t1, t3, t1, t3, t1, t3, t1, t3))
    A("SELECT %s.id, %s.v FROM %s, %s WHERE %s.k = %s.k LIMIT 4" % (t1, t2, t1, t2, t1, t2))
    A("SELECT %s.id FROM %s, %s WHERE %s.k = %s.k ORDER BY %s.v, %s.id" % (t1, t1, t2, t1, t2, t2, t1))
    A("SELECT %s.id FROM %s, %s WHERE %s.k = %s.k ORDER BY 1, %s.v" % (t1, t1, t2, t1, t2, t2))
    # ---- explicit joins ----
    A("SELECT * FROM %s INNER JOIN %s ON %s.k = %s.k ORDER BY id, k, v, id" % (t1, t2, t1, t2))
    A("SELECT * FROM %s LEFT OUTER JOIN %s ON %s.k = %s.k AND %s.v > 150 ORDER BY id, k, v, id" % (t1, t2, t1, t2, t2))
    A("SELECT * FROM %s CROSS JOIN %s ORDER BY 1, 2, 3, 4, 5, 6 LIMIT 3" % (t1, t3))
    A("SELECT COUNT(*) FROM %s JOIN %s" % (t1, t3))
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.k = %s.k JOIN %s ON %s.k = %s.k" % (t1, t2, t1, t2, t3, t2, t3))
    A("SELECT * FROM %s JOIN %s ON %s.k = %s.k JOIN %s ON %s.k = %s.k ORDER BY id, k, v, k, w" % (t1, t2, t1, t2, t3, t2, t3))
    A("SELECT * FROM %s LEFT JOIN %s ON %s.k = %s.k JOIN %s ON %s.k = %s.k ORDER BY id, k, v, w" % (t1, t2, t1, t2, t3, t2, t3))
    A("SELECT * FROM %s JOIN %s ON %s.k = %s.k LEFT JOIN %s ON %s.k = %s.k ORDER BY id, k, v, w" % (t1, t2, t1, t2, t3, t1, t3))
    A("SELECT * FROM %s LEFT JOIN %s ON %s.k = %s.k LEFT JOIN %s ON %s.k = %s.k ORDER BY id, k, v, w" % (t1, t2, t1, t2, t3, t1, t3))
    A("SELECT * FROM %s LEFT JOIN %s ON %s.k = %s.k, %s WHERE %s.k = %s.k ORDER BY id, k, v, w" % (t1, t2, t1, t2, t3, t2, t3))
    A("SELECT * FROM %s, %s LEFT JOIN %s ON %s.k = %s.k WHERE %s.k = %s.k ORDER BY id, k, v, w" % (t1, t2, t3, t2, t3, t1, t2))
    A("SELECT * FROM %s LEFT JOIN %s ON %s.id = %s.id ORDER BY id, k, v" % (t2, t3, t2, t3))
    # ON over compound predicates
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.k = %s.k OR %s.id = %s.id" % (t1, t3, t1, t3, t1, t3))
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.k >= %s.k AND %s.s = %s.w" % (t1, t3, t1, t3, t1, t3))
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.s IS NULL OR %s.k = %s.k" % (t1, t3, t1, t1, t3))
    A("SELECT COUNT(*) FROM %s JOIN %s ON NOT (%s.k = %s.k)" % (t1, t3, t1, t3))
    A("SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k WHERE %s.v IS NULL" % (t1, t2, t1, t2, t2))
    A("SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k WHERE %s.v >= 100" % (t1, t2, t1, t2, t2))
    A("SELECT COUNT(*) FROM %s, %s, %s WHERE %s.k = %s.k AND %s.k = %s.k" % (t1, t2, t3, t1, t2, t2, t3))
    A("SELECT %s.id, %s.v, %s.w FROM %s, %s, %s WHERE %s.k = %s.k AND %s.k = %s.k ORDER BY %s.id, %s.v, %s.w" % (t1, t2, t3, t1, t2, t3, t1, t2, t2, t3, t1, t2, t3))
    A("SELECT * FROM %s a, %s b WHERE a.id = b.id ORDER BY a.id, a.k, a.s, b.k, b.v, b.id" % (t1, t2))
    # ---- aggregates over joins ----
    A("SELECT COUNT(*), SUM(%s.v), MIN(%s.v), MAX(%s.v) FROM %s LEFT JOIN %s ON %s.k = %s.k" % (t2, t2, t2, t1, t2, t1, t2))
    A("SELECT %s.k AS g, COUNT(*) AS n FROM %s LEFT JOIN %s ON %s.k = %s.k GROUP BY %s.k ORDER BY g" % (t1, t1, t2, t1, t2, t1))
    A("SELECT %s.k AS g, COUNT(*) AS n FROM %s LEFT JOIN %s ON %s.k = %s.k GROUP BY %s.k HAVING COUNT(*) > 1 ORDER BY g" % (t1, t1, t2, t1, t2, t1))
    A("SELECT DISTINCT %s.id, %s.v FROM %s, %s WHERE %s.k = %s.k ORDER BY 1, 2" % (t1, t2, t1, t2, t1, t2))
    # --- ticket 8 shapes: star + duplicate names, self-join, IS NULL ---
    A("SELECT * FROM %s, %s ORDER BY id, k, v" % (t1, t2))
    A("SELECT * FROM %s, %s ORDER BY k" % (t1, t2))
    A("SELECT * FROM %s, %s WHERE %s.k = %s.k ORDER BY id" % (t1, t2, t1, t2))
    A("SELECT * FROM %s LEFT JOIN %s ON %s.k = %s.k ORDER BY id, v" % (t1, t2, t1, t2))
    A("SELECT %s.* FROM %s, %s ORDER BY id" % (t2, t1, t2))
    A("SELECT %s.s, * FROM %s, %s ORDER BY id" % (t1, t1, t2))
    A("SELECT DISTINCT * FROM %s, %s WHERE %s.k = %s.k ORDER BY id, k, v" % (t1, t2, t1, t2))
    A("SELECT * FROM %s, %s ORDER BY id, k, v LIMIT 5" % (t1, t2))
    # self-join on one table with aliases
    A("SELECT a.id, b.id FROM %s a, %s b WHERE a.k = b.k ORDER BY a.id, b.id" % (t1, t1))
    A("SELECT a.id, b.k FROM %s a, %s b ON a.id = b.id WHERE a.k < b.k ORDER BY a.id, b.k" % (t1, t1))
    A("SELECT a.s, b.s FROM %s a JOIN %s b ON a.k = b.k ORDER BY a.s, b.s" % (t1, t1))
    A("SELECT a.s, b.s FROM %s a LEFT JOIN %s b ON a.k = b.k ORDER BY a.s, b.s" % (t1, t1))
    A("SELECT * FROM %s a, %s b WHERE a.id = b.id ORDER BY a.id, a.k, b.k" % (t1, t1))
    A("SELECT COUNT(*) FROM %s a, %s b WHERE a.id = b.id" % (t1, t1))
    A("SELECT id FROM %s a, %s b" % (t1, t1))
    A("SELECT k FROM %s a, %s b WHERE a.id = 1" % (t1, t1))
    A("SELECT a.id FROM %s a, %s b WHERE id = 1" % (t1, t1))
    A("SELECT a.id FROM %s a, %s b ORDER BY id" % (t1, t1))
    A("SELECT COUNT(*) FROM %s a, %s a" % (t1, t2))
    A("SELECT a.k FROM %s a, %s a" % (t1, t2))
    A("SELECT v FROM %s a, %s a" % (t2, t2))
    A("SELECT * FROM %s, %s" % (t1, t1))
    # IS NULL / compound ON over LEFT JOIN
    A("SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k WHERE %s.v IS NULL" % (t1, t2, t1, t2, t2))
    A("SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k AND %s.v > 100" % (t1, t2, t1, t2, t2))
    A("SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k WHERE %s.v > 100" % (t1, t2, t1, t2, t2))
    A("SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k WHERE %s.id >= 2" % (t1, t2, t1, t2, t1))
    A("SELECT %s.id, %s.v FROM %s LEFT JOIN %s ON %s.k = %s.k WHERE %s.v IS NULL ORDER BY %s.id" % (t1, t2, t1, t2, t1, t2, t2, t1))
    # join error probes from the ticket
    A("SELECT k FROM %s a, %s b WHERE a.id = b.id" % (t1, t1))
    A("SELECT nope.id FROM %s, %s" % (t1, t2))
    A("SELECT COUNT(*) FROM nope_%s, %s" % (sfx, t1))
    A("SELECT COUNT(*) FROM %s LEFT JOIN nope_%s ON %s.k = %s.k" % (t1, sfx, t1, sfx))
    A("SELECT COUNT(*) FROM %s, %s ON %s.nope = %s.k" % (t1, t2, t1, t2))
    A("SELECT COUNT(*) FROM %s, %s USING (nope)" % (t1, t2))
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.id = %s.nope" % (t1, t2, t1, t2))
    A("SELECT %s.id FROM %s, %s WHERE %s.nope = %s.k" % (t1, t1, t2, t1, t2))
    A("SELECT %s.id FROM %s a, %s b WHERE b.nope = 1" % (t1, t1, t2))
    # comma + ON, mixed chains
    A("SELECT COUNT(*) FROM %s, %s ON %s.k = %s.k" % (t1, t2, t1, t2))
    A("SELECT COUNT(*) FROM %s, %s ON %s.k = %s.k, %s ON %s.k = %s.k" % (t1, t2, t1, t2, t3, t2, t3))
    A("SELECT COUNT(*) FROM %s, %s JOIN %s ON %s.k = %s.k" % (t1, t2, t3, t2, t3))
    A("SELECT * FROM %s, %s ON %s.k = %s.k ORDER BY id, k, v" % (t1, t2, t1, t2))
    # ---- error probes (ticket 8 named list) ----
    err = [
        "SELECT k FROM %s a, %s b" % (t1, t1),                       # ambiguous bare in self-join
        "SELECT k FROM %s a, %s b WHERE a.id = b.id" % (t1, t1),
        "SELECT a.id, k FROM %s a, %s b WHERE a.id = b.id" % (t1, t1),
        "SELECT a.id FROM %s a, %s b WHERE k = 1" % (t1, t1),
        "SELECT a.id FROM %s a, %s b WHERE a.id = b.id ORDER BY k" % (t1, t1),
        "SELECT a.id, COUNT(*) FROM %s a, %s b WHERE a.id = b.id GROUP BY a.id, k" % (t1, t1),
        "SELECT zz.k FROM %s, %s" % (t1, t2),                        # unknown qualifier
        "SELECT zz.k FROM %s a, %s b" % (t1, t2),
        "SELECT %s.k FROM %s a, %s b" % (t1, t1, t2),                # table name hidden by alias
        "SELECT zz.* FROM %s, %s" % (t1, t2),
        "SELECT %s.zz FROM %s, %s" % (t1, t1, t2),                   # missing column in select
        "SELECT COUNT(*) FROM %s JOIN %s ON %s.zz = %s.k" % (t1, t2, t1, t2),  # missing column in ON
        "SELECT COUNT(*) FROM %s JOIN %s ON %s.k = %s.zz" % (t1, t2, t1, t2),
        "SELECT COUNT(*) FROM %s, nosuch_%s" % (t1, sfx),            # unknown table
        "SELECT COUNT(*) FROM nosuch_%s, %s" % (sfx, t1),
        "SELECT nosuch_%s.a FROM %s, %s" % (sfx, t1, t2),
        "SELECT COUNT(*) FROM %s, %s WHERE zz = 1" % (t1, t2),
        "SELECT %s.id FROM %s, %s WHERE %s.id = %s.k" % (t1, t1, t2, t1, t2),
        "SELECT %s.id FROM %s a, %s b ON a.id = b.id" % (t1, t1, t2),  # ON after comma
        "SELECT COUNT(*) FROM %s a LEFT JOIN %s b" % (t1, t2),       # no ON -> cross (ok)
        "SELECT COUNT(*) FROM %s a LEFT OUTER %s" % (t1, t2),
        "SELECT COUNT(*) FROM %s a INNER %s" % (t1, t2),
        "DELETE FROM %s, %s" % (t1, t2),
        "UPDATE %s, %s SET id = 1" % (t1, t2),
        "UPDATE %s AS a SET id = 1" % t1,
        "DELETE FROM %s AS a" % t1,
        "SELECT %s.a FROM %s, %s" % (t1, t1, t1),                    # dup alias-free qual
        "SELECT * FROM %s a, %s a" % (t1, t2),                       # duplicate alias
        "SELECT COUNT(*) FROM %s a, %s a ON a.k = a.k" % (t1, t2),
        "SELECT COUNT(*) FROM %s JOIN %s ON" % (t1, t2),
        "SELECT COUNT(*) FROM %s JOIN %s ON %s.k = " % (t1, t2, t1),
        "SELECT id, id FROM %s, %s WHERE %s.k = %s.k ORDER BY 1, 2" % (t1, t2, t1, t2),
        "SELECT %s.id, %s.id FROM %s, %s WHERE %s.k = %s.k ORDER BY 1, 2" % (t1, t2, t1, t2, t1, t2),
        "SELECT %s.k FROM %s, %s WHERE %s.k = %s.k ORDER BY zz" % (t1, t1, t2, t1, t2),
        "SELECT COUNT(*) FROM %s a JOIN %s b ON a.k = b.k WHERE COUNT(*) > 0" % (t1, t2),
    ]
    for st in err:
        A(st)
    rc = run(setup, S, "edge")
    # ---- randomized 2/3-table join generator ----
    rng = random.Random(SEED)
    tables = [
        (t1, ["id", "k", "s"], "s"),
        (t2, ["k", "v", "id"], "v"),
        (t3, ["k", "w", "id"], "w"),
    ]
    R = []
    for n in range(300):
        ntab = rng.choice([2, 2, 3])
        picked = rng.sample(tables, ntab)
        # alias or bare table name per source
        parts = []  # (qualifier, from-text, columns, unique-col)
        for i, (tn, cols, uniq) in enumerate(picked):
            if rng.random() < 0.5:
                al = "al%d" % i
                parts.append((al, "%s %s" % (tn, al), cols, uniq))
            else:
                parts.append((tn, tn, cols, uniq))
        frm = parts[0][1]
        wheres = []
        for i in range(1, ntab):
            k = rng.choice(["comma", "inner", "left", "inner"])
            # ON/WHERE condition over the columns in scope (joined so far)
            left = rng.choice(parts[:i])
            right = parts[i]
            if rng.random() < 0.75:
                cond = "%s.k = %s.k" % (left[0], right[0])
            elif rng.random() < 0.5:
                cond = "%s.id = %s.id" % (left[0], right[0])
            elif rng.random() < 0.5:
                cond = "%s.k <= %s.id" % (left[0], right[0])
            else:
                cond = "%s.k = %s.k AND %s.id >= %s.id" % (
                    left[0], right[0], left[0], right[0])
            if k == "comma":
                frm += ", " + parts[i][1]
                wheres.append(cond)
            elif k == "inner":
                frm += " JOIN " + parts[i][1] + " ON " + cond
            else:
                frm += " LEFT JOIN " + parts[i][1] + " ON " + cond
        # WHERE: filter over in-scope columns (usually the leftmost table)
        if rng.random() < 0.5:
            p = rng.choice(parts)
            wheres.append(rng.choice(["%s.id >= 2" % p[0], "%s.k < 60" % p[0],
                                      "%s.s <> 'zz'" % p[0] if p[2] and p[3] in ("s",) else "%s.id <> 7" % p[0]]))
        where = ""
        if wheres:
            where = " WHERE " + " AND ".join("(%s)" % w for w in wheres)
        # select list
        style = rng.random()
        agg = ""
        if style < 0.2:
            sel = "*"
        elif style < 0.35:
            sel = "%s.*" % parts[0][0]
        else:
            picks = []
            for p in parts:
                c = rng.choice(p[2])
                picks.append("%s.%s" % (p[0], c))
            if rng.random() < 0.35:
                # bare reference: unique column of one table (resolves) or
                # `id`/`k` (shared -> ambiguous)
                picks.append(rng.choice([p[3] for p in parts] + ["id", "k"]))
            sel = ", ".join(picks)
        order = ""
        if rng.random() < 0.85:
            terms = []
            for _ in range(rng.randint(1, 3)):
                if rng.random() < 0.35:
                    terms.append("%d" % (len(terms) + 1))
                elif rng.random() < 0.6:
                    p = rng.choice(parts)
                    terms.append("%s.%s" % (p[0], rng.choice(p[2])))
                else:
                    terms.append(rng.choice([p[3] for p in parts] + ["id", "k"]))
            order = " ORDER BY " + ", ".join(terms)
        limit = ""
        if rng.random() < 0.12:
            limit = " LIMIT %d" % rng.randint(1, 8)
        if rng.random() < 0.25:
            g = rng.choice(parts)
            key = "%s.k" % g[0] if rng.random() < 0.7 else "%s.%s" % (g[0], g[3])
            sel = "%s AS g, COUNT(*) AS n" % key
            agg = " GROUP BY %s" % key
            order = " ORDER BY g, n"
            limit = ""
        R.append("SELECT %s FROM %s%s%s%s%s" % (sel, frm, where, agg, order, limit))
    rc |= run([], R, "fuzz(seed=%d)" % SEED)
    sys.exit(rc)


if __name__ == "__main__":
    main()
