#!/usr/bin/env python3
"""Ticket-2 differential tester: ORDER BY / LIMIT / OFFSET / DISTINCT.

Raw sockets only on purpose: the workspace must not contain any SQL engine
binding or parser library, not even in tests.

Statements with ORDER BY are compared row-by-row (exact order), statements
without it as multisets, errors by class token only.
"""
import os
import random
import re
import socket
import sys

CAND = ("127.0.0.1", 5580)
REF = ("ref-sql", 5480)

KEYWORDS = [
    "select", "distinct", "all", "from", "where", "order", "by", "as", "and",
    "or", "not", "is", "null", "insert", "into", "values", "create", "table",
    "integer", "text", "limit", "offset", "asc", "desc", "length", "upper",
    "lower", "abs", "coalesce",
]

COLPOOL = [
    "a", "b", "c", "d", "x", "y", "s", "n", "v", "w", "k", "m",
    "p", "q", "r", "t", "u", "z", "g", "h",
]

INT_POOL = [-5, -3, -1, 0, 1, 2, 3, 4, 5, 5, 7, 9, 12]
TXT_POOL = ["a", "b", "c", "", "x y", "zz", "hello", "hi", "mid"]


class Conn:
    def __init__(self, addr):
        self.sock = socket.create_connection(addr, timeout=30)
        self.file = self.sock.makefile("rb")

    def ask(self, stmt):
        data = stmt if stmt.endswith("\n") else stmt + "\n"
        self.sock.sendall(data.encode("utf-8"))
        first = self.file.readline()
        if not first:
            raise RuntimeError("connection closed by peer")
        first = first.decode("utf-8")
        if first.startswith("ERR "):
            return ("ERR", first.split()[1])
        if first.startswith("OK "):
            return ("OK", first.rstrip("\n"))
        if first.startswith("ROWS "):
            hdr = self.file.readline().decode("utf-8").rstrip("\n")
            rows = []
            while True:
                line = self.file.readline()
                if not line:
                    raise RuntimeError("eof while reading rows")
                line = line.decode("utf-8")
                if line.startswith("END "):
                    n = int(line.split()[1])
                    break
                rows.append(line.rstrip("\n"))
            return ("ROWS", hdr, rows, n)
        raise RuntimeError("bad frame: %r" % first)


def has_order_by(stmt):
    parts = re.split(r"'(?:[^']|'')*'", stmt)
    return any(re.search(r"\border\s+by\b", p, re.I) for p in parts)


def compare(cand, ref, ordered):
    if cand[0] != ref[0]:
        return "kind: cand=%r ref=%r" % (cand, ref)
    if cand[0] == "ERR":
        if cand[1] != ref[1]:
            return "class: cand=%s ref=%s" % (cand[1], ref[1])
        return None
    if cand[0] == "OK":
        if cand[1] != ref[1]:
            return "ok: cand=%s ref=%s" % (cand[1], ref[1])
        return None
    if cand[1] != ref[1]:
        return "header: cand=%r ref=%r" % (cand[1], ref[1])
    if len(cand[2]) != cand[3] or ref[3] != len(ref[2]):
        return "self-count: cand=%s/%s ref=%s/%s" % (
            len(cand[2]), cand[3], len(ref[2]), ref[3])
    if cand[3] != ref[3]:
        return "count: cand=%s ref=%s" % (cand[3], ref[3])
    if ordered:
        if cand[2] != ref[2]:
            return "order: cand=%r ref=%r" % (cand[2], ref[2])
    else:
        if sorted(cand[2]) != sorted(ref[2]):
            return "rows: cand=%r ref=%r" % (sorted(cand[2]), sorted(ref[2]))
    return None


def qtext(v):
    return "'%s'" % v.replace("'", "''")


def gen_schema(rng, suffix):
    tables = []
    for ti in range(rng.randint(3, 5)):
        name = "q%s%d" % (suffix, ti)
        ncols = rng.randint(2, 6)
        cols = []
        used = set()
        for _ in range(ncols):
            cname = rng.choice(COLPOOL)
            while cname in used:
                cname = rng.choice(COLPOOL)
            used.add(cname)
            cols.append((cname, rng.choice(["INTEGER", "TEXT"])))
        tables.append((name, cols))
    return tables


def gen_rows(rng, cols, n):
    """Rows drawn from a small pool so duplicates (and ties) really happen."""
    k = rng.randint(1, max(1, min(4, n)))
    pool = []
    for _ in range(k):
        vals = []
        for _, ctype in cols:
            if rng.random() < 0.25:
                vals.append("NULL")
            elif ctype == "INTEGER":
                vals.append(str(rng.choice(INT_POOL)))
            else:
                vals.append(qtext(rng.choice(TXT_POOL)))
        pool.append(vals)
    return [list(rng.choice(pool)) for _ in range(n)]


def gen_expr(rng, cname, ctype):
    if ctype == "INTEGER":
        k = rng.randint(1, 6)
        return rng.choice([
            cname,
            "%s + %d" % (cname, k),
            "%s - %d" % (cname, k),
            "%s * %d" % (cname, k),
            "%s %% %d" % (cname, k),
            "abs(%s)" % cname,
            "-(%s)" % cname,
            "coalesce(%s, %d)" % (cname, k),
        ])
    k = rng.randint(1, 6)
    return rng.choice([
        cname,
        "upper(%s)" % cname,
        "lower(%s)" % cname,
        "length(%s)" % cname,
        "%s || 'x'" % cname,
        "coalesce(%s, 'v%d')" % (cname, k),
    ])


def gen_where(rng, cols):
    cname, ctype = rng.choice(cols)
    if ctype == "INTEGER":
        k = rng.randint(-2, 8)
        return rng.choice([
            "%s > %d" % (cname, k),
            "%s = %d" % (cname, k),
            "%s < %d" % (cname, k),
            "%s IS NULL" % cname,
            "%s IS NOT NULL" % cname,
            "%s %% 2 = 0" % cname,
            "NOT %s > %d" % (cname, k),
            "%s > %d AND %s IS NOT NULL" % (cname, k, cname),
        ])
    lit = qtext(rng.choice(TXT_POOL))
    return rng.choice([
        "%s = %s" % (cname, lit),
        "%s IS NULL" % cname,
        "%s IS NOT NULL" % cname,
        "length(%s) > 1" % cname,
        "upper(%s) = 'A'" % cname,
    ])


def gen_select(rng, tables, nrows):
    tname, cols = rng.choice(tables)
    ncols_out = rng.randint(1, min(3, len(cols)))
    chosen = rng.sample(cols, ncols_out)
    items = []
    names = []
    for idx, (cname, ctype) in enumerate(chosen):
        # bare column, or an expression with an alias (sometimes shadowing a
        # table column name, which must resolve to the output column)
        if rng.random() < 0.45:
            items.append(cname)
            names.append(cname)
        else:
            alias = "e%d" % idx
            if rng.random() < 0.2:
                shadow = rng.choice(cols)[0]
                if shadow not in names:
                    alias = shadow
            items.append("%s AS %s" % (gen_expr(rng, cname, ctype), alias))
            names.append(alias)

    stmt = "SELECT "
    if rng.random() < 0.3:
        stmt += "DISTINCT "
    stmt += ", ".join(items)
    stmt += " FROM %s" % tname
    if rng.random() < 0.5:
        stmt += " WHERE %s" % gen_where(rng, cols)

    ordered = False
    if rng.random() < 0.85 or nrows == 0:
        keys = list(names)
        if rng.random() < 0.3:
            rng.shuffle(keys)
        terms = []
        for nme in keys:
            terms.append("%s %s" % (nme, rng.choice(["ASC", "DESC"])))
        stmt += " ORDER BY %s" % ", ".join(terms)
        ordered = True

    if ordered and rng.random() < 0.5:
        lim = rng.randint(0, nrows + 2)
        stmt += " LIMIT %d" % lim
        if rng.random() < 0.5:
            stmt += " OFFSET %d" % rng.randint(0, nrows + 2)
    return stmt


def gen_star(rng, tables):
    tname, cols = rng.choice(tables)
    stmt = "SELECT "
    if rng.random() < 0.3:
        stmt += "DISTINCT "
    stmt += "* FROM %s" % tname
    if rng.random() < 0.6:
        terms = ["%s %s" % (c[0], rng.choice(["ASC", "DESC"])) for c in cols]
        stmt += " ORDER BY %s" % ", ".join(terms)
    return stmt


def build(suffix, rng):
    stmts = []
    tables = gen_schema(rng, suffix)
    for tname, cols in tables:
        stmts.append("CREATE TABLE %s (%s)" % (
            tname, ", ".join("%s %s" % (c, t) for c, t in cols)))
    for tname, cols in tables:
        n = rng.randint(0, 16)
        rows = gen_rows(rng, cols, n)
        # multi-row inserts, all columns in order; duplicates come from the
        # row pool itself
        i = 0
        while i < len(rows):
            take = rng.randint(1, 4)
            chunk = rows[i:i + take]
            i += take
            stmts.append("INSERT INTO %s VALUES %s" % (
                tname, ", ".join("(%s)" % ", ".join(r) for r in chunk)))
    for _ in range(400):
        stmts.append(gen_select(rng, tables, 16))
    for _ in range(60):
        stmts.append(gen_star(rng, tables))

    # --- error probes (compared by class only) ---------------------------
    tname, cols = tables[0]
    probes = [
        "SELECT %s FROM %s ORDER BY nosuch" % (cols[0][0], tname),
        "SELECT %s FROM %s ORDER BY 0" % (cols[0][0], tname),
        "SELECT %s FROM %s ORDER BY 99" % (cols[0][0], tname),
        "SELECT %s FROM %s ORDER BY %s LIMIT nosuch" % (
            cols[0][0], tname, cols[0][0]),
        "SELECT %s FROM %s ORDER BY %s LIMIT NULL" % (
            cols[0][0], tname, cols[0][0]),
        "SELECT %s FROM %s ORDER BY %s OFFSET NULL" % (
            cols[0][0], tname, cols[0][0]),
        "SELECT %s FROM %s ORDER BY %s LIMIT 'x'" % (
            cols[0][0], tname, cols[0][0]),
        "SELECT %s FROM %s ORDER BY %s OFFSET 3" % (
            cols[0][0], tname, cols[0][0]),
        "SELECT %s FROM %s ORDER" % (cols[0][0], tname),
        "SELECT %s FROM %s ORDER BY" % (cols[0][0], tname),
        "SELECT %s FROM %s ORDER BY %s ASC DESC" % (
            cols[0][0], tname, cols[0][0]),
        "SELECT %s FROM %s LIMIT 1, 2, 3" % (cols[0][0], tname),
        "SELECT DISTINCT",
        "SELECT %s FROM %s ORDER BY %s LIMIT -1" % (
            cols[0][0], tname, cols[0][0]),
        "SELECT %s FROM %s ORDER BY %s LIMIT 100 OFFSET -3" % (
            cols[0][0], tname, cols[0][0]),
        "SELECT %s FROM %s DISTINCT" % (cols[0][0], tname),
    ]
    stmts.extend(probes)
    return stmts


def scramble(stmt, rng):
    """Randomise keyword case and inter-token whitespace (never inside
    string literals, never touching identifiers)."""
    out = []
    i = 0
    while i < len(stmt):
        c = stmt[i]
        if c == "'":
            j = i + 1
            while j < len(stmt):
                if stmt[j] == "'":
                    if j + 1 < len(stmt) and stmt[j + 1] == "'":
                        j += 2
                        continue
                    j += 1
                    break
                j += 1
            out.append(("lit", stmt[i:j]))
            i = j
        elif c.isalpha() or c == "_":
            j = i
            while j < len(stmt) and (stmt[j].isalnum() or stmt[j] == "_"):
                j += 1
            out.append(("word", stmt[i:j]))
            i = j
        elif c == " " or c == "\t":
            j = i
            while j < len(stmt) and stmt[j] in " \t":
                j += 1
            out.append(("ws", " "))
            i = j
        else:
            out.append(("p", c))
            i += 1
    res = []
    for kind, text in out:
        if kind == "word" and text.lower() in KEYWORDS:
            mode = rng.randint(0, 2)
            if mode == 0:
                text = text.upper()
            elif mode == 1:
                text = text.lower()
            else:
                text = "".join(
                    ch.upper() if rng.random() < 0.5 else ch.lower()
                    for ch in text)
            res.append(text)
        elif kind == "ws":
            res.append(rng.choice([" ", "\t", "  ", " \t "]))
        else:
            res.append(text)
    return "".join(res)


def run_battery(cand, ref, stmts, rng, label):
    fails = 0
    for stmt in stmts:
        s2 = scramble(stmt, rng)
        ordered = has_order_by(stmt)
        try:
            a = cand.ask(s2)
        except Exception as e:  # noqa: BLE001
            print("[%s] CANDIDATE CRASH on %r: %s" % (label, s2, e))
            return 1
        try:
            b = ref.ask(s2)
        except Exception as e:  # noqa: BLE001
            print("[%s] REF CRASH on %r: %s" % (label, s2, e))
            return 1
        err = compare(a, b, ordered)
        if err:
            fails += 1
            print("[%s] MISMATCH %s\n    stmt: %s\n    cand: %r\n    ref:  %r"
                  % (label, err, s2, a, b))
            if fails >= 25:
                print("[%s] too many failures, stopping" % label)
                break
    if not fails:
        print("[%s] OK (%d statements)" % (label, len(stmts)))
    return fails


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 20261121
    suffix = os.urandom(4).hex()
    rng = random.Random(seed)
    stmts = build(suffix, rng)
    cand = Conn(CAND)
    ref = Conn(REF)
    fails = run_battery(cand, ref, stmts, rng, "order")
    if fails:
        print("FAIL (%d)" % fails)
        sys.exit(1)
    print("PASS")


if __name__ == "__main__":
    main()
