#!/usr/bin/env python3
"""Ticket 7 differential tester: candidate sdb vs the reference server.

Raw sockets only (no SQL engine bindings anywhere in the workspace).
Every statement goes to both servers; ROWS answers are compared on the
header plus rows (exact when the statement has ORDER BY, as a multiset
otherwise), ERR answers on the class token only.
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
        self.sock = socket.create_connection(addr, timeout=20)
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
        if a[1] != b[1]:
            return "%s: %s vs %s" % (stmt, a[1], b[1])
        return None
    if a[1] != b[1]:
        return "%s: hdr cand=%r ref=%r" % (stmt, a[1], b[1])
    if a[3] != b[3]:
        return "%s: nrows cand=%d ref=%d (%r vs %r)" % (stmt, a[3], b[3], a[2], b[2])
    if has_order_by(stmt):
        if a[2] != b[2]:
            return "%s: order cand=%r ref=%r" % (stmt, a[2], b[2])
    else:
        if sorted(a[2]) != sorted(b[2]):
            return "%s: rows cand=%r ref=%r" % (stmt, a[2], b[2])
    return None


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 7001
    suffix = os.urandom(4).hex()
    rng = random.Random(seed)

    t = "at_" + suffix
    e = "ae_" + suffix
    setup = [
        "CREATE TABLE %s (id INTEGER, s TEXT)" % t,
        # unsorted ids, NULLs, duplicate group keys, both cases, numeric text
        "INSERT INTO %s VALUES (5, 'bb'), (2, 'A'), (9, NULL), (1, 'a'), (7, '10'), "
        "(3, 'bb'), (4, NULL), (8, 'a'), (6, 'Z'), (2, 'q')" % t,
        "INSERT INTO %s VALUES (11, NULL), (10, 'A')" % t,
        "CREATE TABLE %s (id INTEGER, s TEXT)" % e,
    ]

    stmts = []
    A = stmts.append

    # --- scalar aggregates -------------------------------------------------
    A("SELECT COUNT(*) FROM %s" % t)
    A("SELECT count(*) from %s" % t)
    A("SELECT COUNT ( * ) FROM %s" % t)
    A("SELECT COUNT(id) AS n, SUM(id) AS s, MIN(id) AS mn, MAX(id) AS mx FROM %s" % t)
    A("SELECT COUNT(s) AS n FROM %s" % t)
    A("SELECT SUM(id) * 2 AS s FROM %s" % t)
    A("SELECT COUNT(*) + 1 AS n FROM %s" % t)
    A("SELECT coalesce(SUM(id), 0) AS s FROM %s" % t)
    A("SELECT COUNT(*) FROM %s WHERE id > 100" % t)
    A("SELECT COUNT(id), SUM(id), MIN(id), MAX(id) FROM %s" % e)
    A("SELECT COUNT(s), MIN(s), MAX(s) FROM %s" % e)
    A("SELECT COUNT(*)")
    A("SELECT COUNT(*) AS c")
    A("SELECT COUNT(*) WHERE 0")
    A("SELECT COUNT(*) WHERE 1")
    A("SELECT COUNT(*) FROM %s WHERE id > 4 ORDER BY 1" % t)
    A("SELECT MIN(id), MAX(id), id * 100 AS x FROM %s" % t)
    A("SELECT id * 100 AS x, MIN(id), MAX(id) FROM %s" % t)
    A("SELECT id * 100 AS x, MAX(id), MIN(id) FROM %s" % t)
    A("SELECT s, MIN(id) AS m FROM %s" % t)
    A("SELECT s, MAX(id) AS m FROM %s" % t)
    A("SELECT s, COUNT(*) AS c FROM %s" % t)
    A("SELECT s, COUNT(*) AS c FROM %s" % e)
    A("SELECT COUNT(DISTINCT s) AS n FROM %s" % t)
    A("SELECT COUNT(DISTINCT id) AS n FROM %s" % t)
    A("SELECT SUM(DISTINCT id) AS n FROM %s" % t)
    A("SELECT COUNT() AS n FROM %s" % t)
    A("SELECT MIN(s), MAX(s) FROM %s" % t)
    A("SELECT COUNT(*), COUNT(*) AS c2 FROM %s" % t)

    # --- grouped -----------------------------------------------------------
    orders = [
        " ORDER BY k", " ORDER BY k DESC", " ORDER BY 1", " ORDER BY k, n",
        " ORDER BY n, k", "",
    ]
    A("SELECT id %% 2 AS k, COUNT(*) AS n, SUM(id) AS s FROM %s GROUP BY id %% 2 ORDER BY k" % t)
    A("SELECT id %% 3 AS k, COUNT(*) AS n FROM %s GROUP BY id %% 3 ORDER BY k" % t)
    A("SELECT s AS k, COUNT(*) AS n FROM %s GROUP BY s ORDER BY k" % t)
    A("SELECT upper(s) AS k, COUNT(*) AS n FROM %s GROUP BY upper(s) ORDER BY k" % t)
    A("SELECT id + 3 AS k, COUNT(*) AS n FROM %s GROUP BY id + 3 ORDER BY k" % t)
    A("SELECT length(s) AS k, COUNT(*) AS n FROM %s GROUP BY length(s) ORDER BY k" % t)
    A("SELECT id %% 2 AS k, COUNT(*) AS n FROM %s GROUP BY 1 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, COUNT(*) AS n FROM %s GROUP BY k ORDER BY k" % t)
    A("SELECT s AS v, COUNT(*) AS n FROM %s GROUP BY v ORDER BY v" % t)
    A("SELECT id %% 2 AS k, SUM(id) AS s, MIN(id) AS mn, MAX(id) AS mx FROM %s GROUP BY id %% 2 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, s, COUNT(*) AS n FROM %s GROUP BY id %% 2, s ORDER BY k, s" % t)
    # bare column with MIN/MAX in a grouped select (unsorted data: the
    # reference reads it from the min/max-winning row, not the first row)
    A("SELECT id %% 2 AS k, s, MIN(id) AS m FROM %s GROUP BY id %% 2 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, s, MAX(id) AS m FROM %s GROUP BY id %% 2 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, id * 100 AS x, MAX(id) AS m FROM %s GROUP BY id %% 2 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, s, COUNT(*) AS c FROM %s GROUP BY id %% 2 ORDER BY k" % t)
    A("SELECT COUNT(*) FROM %s GROUP BY id ORDER BY id" % t)
    A("SELECT * FROM %s GROUP BY id ORDER BY id" % t)
    A("SELECT id, COUNT(*) AS n FROM %s GROUP BY id HAVING COUNT(*) >= 1 ORDER BY id" % t)
    A("SELECT s, COUNT(*) AS n FROM %s GROUP BY s ORDER BY s" % t)
    A("SELECT id %% 2 AS k, COUNT(*) AS n FROM %s GROUP BY id %% 2" % t)
    A("SELECT id, s, COUNT(*) AS n FROM %s GROUP BY id, s ORDER BY id, s" % t)
    A("SELECT upper(s) AS k, COUNT(*) AS n FROM %s GROUP BY upper(s) HAVING upper(s) IS NOT NULL ORDER BY k" % t)
    A("SELECT id, COUNT(*) AS n FROM %s GROUP BY id ORDER BY id LIMIT 3" % t)
    A("SELECT id, COUNT(*) AS n FROM %s GROUP BY id ORDER BY id DESC LIMIT 2 OFFSET 1" % t)

    # --- HAVING ------------------------------------------------------------
    A("SELECT id %% 2 AS k, COUNT(*) AS n FROM %s GROUP BY id %% 2 HAVING COUNT(*) > 3 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, COUNT(*) AS n FROM %s GROUP BY id %% 2 HAVING n > 3 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, SUM(id) AS s FROM %s GROUP BY id %% 2 HAVING SUM(id) > 25 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, MIN(id) AS mn FROM %s GROUP BY id %% 2 HAVING MIN(id) < 2 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, MAX(id) AS mx FROM %s GROUP BY id %% 2 HAVING MAX(id) > 8 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, COUNT(*) AS n FROM %s GROUP BY id %% 2 HAVING COUNT(*) + 1 > 4 ORDER BY k" % t)
    A("SELECT id %% 2 AS k, COUNT(*) AS n FROM %s GROUP BY id %% 2 HAVING id %% 2 = 1 ORDER BY k" % t)
    A("SELECT length(s) AS L, COUNT(*) AS n FROM %s GROUP BY length(s) HAVING length(s) > 1 ORDER BY L" % t)
    A("SELECT COUNT(*) FROM %s HAVING COUNT(*) > 100" % t)
    A("SELECT COUNT(*) FROM %s HAVING COUNT(*) > 1" % t)
    A("SELECT COUNT(*) FROM %s HAVING 1" % t)
    A("SELECT COUNT(*) FROM %s WHERE id > 100 HAVING COUNT(*) = 0" % t)
    A("SELECT s AS v, COUNT(*) AS n FROM %s GROUP BY s HAVING v = 'a' ORDER BY v" % t)
    A("SELECT id %% 2 AS k, COUNT(*) AS n FROM %s GROUP BY id %% 2 HAVING upper('a') = 'A' ORDER BY k" % t)
    A("SELECT MIN(id) AS mn, SUM(id) AS s FROM %s HAVING SUM(id) > 500" % t)
    A("SELECT MIN(id) AS mn, SUM(id) AS s FROM %s HAVING SUM(id) < 500" % t)

    # --- error probes ------------------------------------------------------
    for bad in [
        "SELECT COUNT(*) FROM %s GROUP BY zz_nocol" % t,
        "SELECT SUM(zz_gone) FROM %s" % t,
        "SELECT COUNT(*) FROM %s WHERE COUNT(*) > 1" % t,
        "SELECT COUNT(*) FROM %s GROUP BY COUNT(*)" % t,
        "SELECT COUNT(*) FROM %s GROUP BY SUM(id)" % t,
        "SELECT COUNT() FROM %s" % t,
        "SELECT COUNT(id, s) FROM %s" % t,
        "SELECT SUM(*) FROM %s" % t,
        "SELECT SUM() FROM %s" % t,
        "SELECT MIN(*) FROM %s" % t,
        "SELECT AVG(id) FROM %s" % t,
        "SELECT AVG(id) FROM %s" % e,
        "SELECT SUM(id + zz) FROM %s" % t,
        "SELECT id FROM %s WHERE COUNT(*) > 0 AND zz > 1" % t,
        "SELECT id FROM %s WHERE zz > 1 AND COUNT(*) > 0" % t,
        "SELECT zz FROM %s GROUP BY COUNT(*)" % t,
        "SELECT COUNT(*) FROM %s ORDER BY COUNT(*)" % t,
        "SELECT id FROM %s HAVING COUNT(*) > 1" % t,
        "SELECT id FROM %s HAVING 1" % t,
        "SELECT id FROM %s HAVING zz > 1" % t,
        "SELECT COUNT(*) FROM %s GROUP BY 9" % t,
        "SELECT COUNT(*) FROM %s GROUP BY 0" % t,
        "SELECT COUNT(DISTINCT *) FROM %s" % t,
        "SELECT sum(sum(id)) FROM %s" % t,
        "SELECT COUNT(*) FROM %s LIMIT COUNT(*)" % t,
        "SELECT COUNT(*) FROM %s ORDER BY zz" % t,
        "SELECT COUNT(*) FROM nosuchtable_%s" % suffix,
        "SELECT SUM(zz) FROM %s GROUP BY zz2" % t,
        "SELECT id FROM %s GROUP BY COUNT(*) HAVING zz > 1" % t,
        "SELECT COUNT(*) FROM %s GROUP BY upper(s) HAVING zz > 1" % t,
        "SELECT id, COUNT(*) FROM %s GROUP BY id HAVING zz > 1" % t,
        "SELECT zz2 FROM %s WHERE COUNT(*) > 1" % t,
        "SELECT UPDATE FROM %s" % t,
        "SELECT COUNT(*) FROM %s GROUP BY" % t,
        "SELECT COUNT(* FROM %s" % t,
        "UPDATE %s SET id = COUNT(*)" % t,
        "DELETE FROM %s WHERE COUNT(*) > 0" % t,
        "SELECT COUNT(*) FROM %s GROUP BY id HAVING COUNT(DISTINCT s) > 1 ORDER BY id" % t,
    ]:
        A(bad)

    # ORDER BY aggregates on aggregate queries (legal there)
    A("SELECT COUNT(*) FROM %s ORDER BY COUNT(*) DESC" % t)
    A("SELECT id, COUNT(*) AS n FROM %s ORDER BY COUNT(*), id" % t)
    A("SELECT id %% 2 AS k, COUNT(*) AS n FROM %s GROUP BY id %% 2 ORDER BY COUNT(*) DESC, k" % t)
    A("SELECT 1 AS x ORDER BY COUNT(*)")
    A("SELECT id FROM %s ORDER BY upper(s), id" % t)
    A("SELECT id FROM %s ORDER BY id + 1" % t)
    A("SELECT id FROM %s ORDER BY id * -1" % t)
    # ... but a non-aggregate query may not sort by one (table present)
    A("SELECT id FROM %s ORDER BY COUNT(*)" % t)
    A("SELECT id FROM %s ORDER BY SUM(id) + 1" % t)
    A("SELECT id FROM %s ORDER BY AVG(id)" % t)
    A("SELECT COUNT(*) FROM %s ORDER BY id" % t)
    # alias inside a GROUP BY expression
    A("SELECT id %% 2 AS k, COUNT(*) AS n FROM %s GROUP BY k + 1 ORDER BY n" % t)
    A("SELECT length(s) AS L, COUNT(*) AS n FROM %s GROUP BY L HAVING n + 1 > 1 ORDER BY L" % t)
    # SUM over text follows the reference's numerify rule (REAL -> error)
    A("SELECT SUM(s) AS x FROM %s" % t)
    A("SELECT MIN(DISTINCT id) AS a, MAX(DISTINCT id) AS b FROM %s" % t)
    # mixed INTEGER/TEXT values in one aggregate (same rules as ORDER BY)
    A("SELECT MIN(coalesce(s, id)) AS mn, MAX(coalesce(s, id)) AS mx FROM %s" % t)
    A("SELECT coalesce(s, id) AS k, COUNT(*) AS n FROM %s GROUP BY coalesce(s, id) ORDER BY k" % t)
    A("SELECT coalesce(s, id) AS k, MIN(id) AS a, MAX(id) AS b FROM %s GROUP BY coalesce(s, id) HAVING COUNT(*) >= 1 ORDER BY k" % t)

    # DISTINCT + aggregates
    A("SELECT DISTINCT COUNT(*) AS n FROM %s" % t)
    A("SELECT DISTINCT s, COUNT(*) AS n FROM %s GROUP BY s ORDER BY s" % t)

    # --- random aggregate statements -------------------------------------
    funcs = ["COUNT(*)", "COUNT({c})", "SUM({c})", "MIN({c})", "MAX({c})",
             "COUNT(DISTINCT {c})", "SUM({c}) + 1", "MIN({c}) + MAX({c})",
             "COUNT({c}) * 2", "abs(MIN({c}))", "COUNT({c}) + COUNT(*)"]
    keys = ["{c}", "upper({s})", "{c} % {m}", "length({s})", "{c} + 7",
            "coalesce({s}, 'z')", "-{c}"]
    havings = [
        "COUNT(*) > {n}", "COUNT(*) >= 0", "SUM({c}) > {n}", "MIN({c}) < {n}",
        "MAX({c}) <= 1000", "COUNT(*) = {n}", "NOT ({c} > 100000)",
        "{c} > 0", "{c} % 2 = {n2}", "length({s}) >= 0",
    ]
    for i in range(200):
        col = "id"
        scol = "s"
        mod = rng.choice([2, 3, 5])
        f = rng.choice(funcs).format(c=col, s=scol)
        grouped = rng.random() < 0.75
        kexpr = rng.choice(keys).format(c=col, s=scol, m=mod)
        sel = "SELECT %s AS a0" % f
        if grouped:
            sel += ", %s AS a1" % kexpr
        q = sel + " FROM " + t
        if rng.random() < 0.6:
            q += " WHERE id >= %d" % rng.choice([1, 3, 5, 7])
        if grouped:
            q += " GROUP BY " + kexpr
            if rng.random() < 0.5:
                # HAVING may only use aggregates and the key (contract).
                hv = rng.choice([
                    "COUNT(*) > {n}",
                    "COUNT(*) >= 0",
                    "COUNT(*) = {n}",
                    "SUM({c}) > {n}",
                    "SUM({c}) <= {n}",
                    "MIN({c}) < {n}",
                    "MAX({c}) <= 1000",
                    "({k}) IS NOT NULL",
                    "({k}) IS NULL OR COUNT(*) >= 0",
                ]).format(c=col, k=kexpr, n=rng.choice([0, 1, 2, 5, 30]))
                q += " HAVING " + hv
            if rng.random() < 0.8:
                q += rng.choice([" ORDER BY a1, a0", " ORDER BY a0, a1",
                                 " ORDER BY a1 DESC, a0"])
        else:
            if rng.random() < 0.5:
                q += " ORDER BY 1"
        stmts.append(q)

    # case/whitespace randomization for a slice of the battery
    kws = ["select", "from", "where", "group", "by", "having", "order", "asc",
           "desc", "count", "sum", "min", "max", "as", "limit", "offset"]
    randomized = []
    for s in stmts:
        if rng.random() < 0.4:
            def up(m):
                return m.group(0).upper() if rng.random() < 0.5 else m.group(0).lower()
            s = re.sub(r"\b(?:%s)\b" % "|".join(kws), up, s)
            s = re.sub(r" +", lambda m: " " * rng.randint(1, 2), s)
        randomized.append(s)
    stmts = setup + randomized

    cand = Conn(CAND)
    ref = Conn(REF)
    fails = 0
    for st in stmts:
        a = cand.ask(st)
        b = ref.ask(st)
        msg = compare(st, a, b)
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
