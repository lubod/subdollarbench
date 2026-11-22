#!/usr/bin/env python3
"""Randomised expression / WHERE fuzzer: candidate vs reference.

Generates typed random expression trees (integer / text / boolean) and
serialises them mostly WITHOUT parentheses, so any precedence or
associativity mistake on either side shows up as a differential mismatch.
Raw sockets only: no SQL engine bindings in this workspace.
"""
import os
import random
import socket
import sys

from difftest import CAND, REF, Conn, compare, scramble

INT_LITS = ["0", "1", "2", "3", "5", "7", "-1", "-7", "10", "-3", "42", "100"]
TXT_LITS = ["'a'", "'abc'", "''", "'zz'", "'hello'", "'5'", "'x y'"]
CMPS = ["=", "<>", "!=", "<", "<=", ">", ">="]


def gi(rng, d, cols):
    """Integer-or-NULL expression."""
    if d <= 0:
        r = rng.random()
        if cols and r < 0.35:
            return rng.choice(cols)
        if r < 0.10:
            return "NULL"
        return rng.choice(INT_LITS)
    r = rng.random()
    if r < 0.07:
        return "(%s)" % gi(rng, d - 1, cols)
    if r < 0.12:
        return "-%s" % gi(rng, d - 1, cols)
    if r < 0.17:
        return "abs(%s)" % gi(rng, d - 1, cols)
    if r < 0.21:
        return "length(%s)" % gt(rng, d - 1, cols)
    if r < 0.25:
        return "coalesce(%s, %s)" % (gi(rng, d - 1, cols), gi(rng, d - 1, cols))
    if r < 0.30:
        return "NOT %s" % gb(rng, d - 1, cols)
    if r < 0.75:
        op = rng.choice(["+", "-", "*", "/", "%"])
        return "%s %s %s" % (gi(rng, d - 1, cols), op, gi(rng, d - 1, cols))
    if r < 0.90:
        op = rng.choice(CMPS)
        return "%s %s %s" % (gi(rng, d - 1, cols), op, gi(rng, d - 1, cols))
    if r < 0.95:
        return "%s IS NULL" % gi(rng, d - 1, cols)
    return "%s IS NOT NULL" % gi(rng, d - 1, cols)


def gt(rng, d, cols):
    """Text-or-NULL expression."""
    if d <= 0:
        r = rng.random()
        if cols and r < 0.4:
            return rng.choice(cols)
        if r < 0.10:
            return "NULL"
        return rng.choice(TXT_LITS)
    r = rng.random()
    if r < 0.08:
        return "(%s)" % gt(rng, d - 1, cols)
    if r < 0.14:
        return "upper(%s)" % gt(rng, d - 1, cols)
    if r < 0.20:
        return "lower(%s)" % gt(rng, d - 1, cols)
    if r < 0.26:
        return "coalesce(%s, %s)" % (gt(rng, d - 1, cols), gt(rng, d - 1, cols))
    if r < 0.30:
        return "coalesce(%s, %s)" % (gi(rng, d - 1, []), gt(rng, d - 1, cols))
    if r < 0.85:
        return "%s || %s" % (gt(rng, d - 1, cols), gt(rng, d - 1, cols))
    # text vs text only: cross-type comparisons are out of contract
    tcols = [c for c in cols if c == "s"]
    op = rng.choice(CMPS)
    return "%s %s %s" % (gt(rng, 1, tcols), op, gt(rng, 1, tcols))


def gb(rng, d, cols):
    """Boolean (0 / 1 / NULL) expression."""
    if d <= 0:
        r = rng.random()
        if r < 0.45:
            op = rng.choice(CMPS)
            return "%s %s %s" % (gi(rng, 1, cols), op, gi(rng, 1, cols))
        if r < 0.7:
            # text vs text only: cross-type comparisons are out of contract
            tcols = [c for c in cols if c == "s"]
            op = rng.choice(CMPS)
            return "%s %s %s" % (gt(rng, 1, tcols), op, gt(rng, 1, tcols))
        if r < 0.85:
            return "%s IS NULL" % gi(rng, 1, cols)
        return "%s IS NOT NULL" % gt(rng, 1, cols)
    r = rng.random()
    if r < 0.08:
        return "(%s)" % gb(rng, d - 1, cols)
    if r < 0.42:
        return "%s AND %s" % (gb(rng, d - 1, cols), gb(rng, d - 1, cols))
    if r < 0.76:
        return "%s OR %s" % (gb(rng, d - 1, cols), gb(rng, d - 1, cols))
    if r < 0.9:
        return "NOT %s" % gb(rng, d - 1, cols)
    return "NOT %s" % gi(rng, d - 1, cols)


def build_setup(suffix):
    tr = "t%se" % suffix
    tc = "t%sf" % suffix
    s = []
    s.append("CREATE TABLE %s (id INTEGER, n INTEGER, s TEXT)" % tr)
    s.append("INSERT INTO %s (id, n, s) VALUES (1, -5, 'abc'), (2, 0, NULL), "
             "(3, 7, ''), (4, NULL, 'zz'), (5, -7, '5'), (6, 10, 'hello')"
             % tr)
    s.append("INSERT INTO %s (id) VALUES (7), (8)" % tr)
    s.append("INSERT INTO %s (n, s, id) VALUES (42, 'x y', 9)" % tr)
    s.append("INSERT INTO %s (s, id) VALUES ('', 10)" % tr)
    # column-list inserts: subsets in varying order
    s.append("CREATE TABLE %s (c1 INTEGER, c2 TEXT, c3 INTEGER, c4 TEXT)" % tc)
    s.append("INSERT INTO %s (c4, c1) VALUES ('q', 1)" % tc)
    s.append("INSERT INTO %s (c3, c2) VALUES (3, 'w')" % tc)
    s.append("INSERT INTO %s (c2, c4, c1, c3) VALUES ('e', 'r', 2, 4)" % tc)
    s.append("INSERT INTO %s (c1) VALUES (5), (6)" % tc)
    s.append("INSERT INTO %s (c4, c3) VALUES (NULL, -1)" % tc)
    s.append("INSERT INTO %s VALUES (7, 't', 7, 'y')" % tc)
    return tr, tc, s


def statements(rng, tr, tc, count):
    out = []
    for _ in range(count):
        kind = rng.randint(0, 9)
        if kind < 4:
            e0 = gi(rng, rng.randint(1, 3), ["id", "n"])
            e1 = gb(rng, rng.randint(1, 3), ["id", "n"])
            if rng.random() < 0.5:
                p = gb(rng, rng.randint(1, 2), ["id", "n"])
            else:
                p = rng.choice([
                    "id >= 2", "n IS NOT NULL", "s IS NOT NULL", "id > 0",
                    "n < 0", "id % 2 = 0", "s = 'abc'", "id < 100",
                ])
            out.append("SELECT %s AS e0, %s AS e1 FROM %s WHERE %s "
                       "ORDER BY id" % (e0, e1, tr, p))
        elif kind < 6:
            e0 = gi(rng, rng.randint(1, 3), ["id", "n"])
            e1 = gt(rng, rng.randint(1, 2), ["s"])
            p = gb(rng, rng.randint(1, 2), ["id", "n"])
            out.append("SELECT %s AS e0, %s AS e1 FROM %s WHERE %s "
                       "ORDER BY id" % (e0, e1, tr, p))
        elif kind < 7:
            # no FROM: constants only
            e0 = gi(rng, rng.randint(1, 3), [])
            e1 = gb(rng, rng.randint(1, 3), [])
            out.append("SELECT %s AS e0, %s AS e1" % (e0, e1))
        elif kind < 8:
            # predicate as a select item
            p = gb(rng, rng.randint(1, 3), ["id", "n"])
            out.append("SELECT id AS id, %s AS p FROM %s WHERE id >= 1 "
                       "ORDER BY id" % (p, tr))
        elif kind < 9:
            e0 = gi(rng, rng.randint(1, 2), ["id", "n"])
            out.append("SELECT %s AS e0 FROM %s WHERE %s ORDER BY id"
                       % (e0, tr, rng.choice(["n IS NOT NULL", "id > 3",
                                              "1", "id <= 7"])))
        else:
            out.append("SELECT * FROM %s ORDER BY c1" % tc)
    return out


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 555
    count = int(sys.argv[2]) if len(sys.argv) > 2 else 300
    rng = random.Random(seed)
    # unique table names per run: the reference persists between runs
    suffix = os.urandom(4).hex()
    tr, tc, setup = build_setup(suffix)
    cand = Conn(CAND)
    ref = Conn(REF)
    fails = 0
    for stmt in setup:
        s2 = scramble(stmt, rng)
        a, b = cand.ask(s2), ref.ask(s2)
        err = compare(stmt, a, b)
        if err:
            fails += 1
            print("SETUP MISMATCH %s\n  stmt: %s\n  cand: %r\n  ref:  %r"
                  % (err, s2, a, b))
    stmts = statements(rng, tr, tc, count)
    for stmt in stmts:
        s2 = scramble(stmt, rng)
        try:
            a = cand.ask(s2)
        except Exception as e:  # noqa: BLE001
            print("CANDIDATE CRASH on %r: %s" % (s2, e))
            fails += 1
            break
        b = ref.ask(s2)
        err = compare(stmt, a, b)
        if err:
            fails += 1
            print("MISMATCH %s\n  stmt: %s\n  cand: %r\n  ref:  %r"
                  % (err, s2, a, b))
            if fails >= 15:
                print("too many failures")
                break
    if not fails:
        print("OK (%d setup + %d fuzz)" % (len(setup), len(stmts)))
        sys.exit(0)
    print("FAIL (%d)" % fails)
    sys.exit(1)


if __name__ == "__main__":
    main()
