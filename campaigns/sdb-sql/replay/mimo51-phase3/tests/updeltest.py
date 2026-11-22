#!/usr/bin/env python3
"""Ticket 3 differential tester: UPDATE/DELETE candidate vs reference.

Raw sockets only on purpose: the workspace must not contain any SQL engine
binding or parser library, not even in tests.
"""
import os
import random
import socket
import sys

CAND = ("127.0.0.1", 5580)
REF = ("ref-sql", 5480)

KEYWORDS = [
    "select", "from", "where", "order", "by", "as", "and", "or", "not", "is",
    "null", "insert", "into", "values", "create", "table", "integer", "text",
    "limit", "offset", "asc", "desc", "length", "upper", "lower", "abs",
    "coalesce", "update", "set", "delete", "distinct",
]


class Conn:
    def __init__(self, addr):
        self.sock = socket.create_connection(addr, timeout=20)
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
            hdr = self.file.readline().decode().rstrip("\n")
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

    def close(self):
        try:
            self.sock.close()
        except Exception:  # noqa: BLE001
            pass


def has_order_by(stmt):
    return "order" in [w for w in _words(stmt)]


def _words(stmt):
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
            i = j
        elif c.isalpha() or c == "_":
            j = i
            while j < len(stmt) and (stmt[j].isalnum() or stmt[j] == "_"):
                j += 1
            out.append(stmt[i:j].lower())
            i = j
        else:
            i += 1
    return out


def compare(stmt, cand, ref):
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
            len(cand[2]), cand[3], ref[3], len(ref[2]))
    if cand[3] != ref[3]:
        return "count: cand=%s ref=%s" % (cand[3], ref[3])
    if has_order_by(stmt):
        if cand[2] != ref[2]:
            return "order: cand=%r ref=%r" % (cand[2], ref[2])
    else:
        if sorted(cand[2]) != sorted(ref[2]):
            return "rows: cand=%r ref=%r" % (sorted(cand[2]), sorted(ref[2]))
    return None


def scramble(stmt, rng):
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


# ---------------------------------------------------------------------------
# Fixed battery
# ---------------------------------------------------------------------------

def build_fixed(sfx):
    t = "f%sa" % sfx
    u = "f%sb" % sfx
    v = "f%sc" % sfx
    e = "f%sd" % sfx  # stays empty
    s = []
    A = s.append

    A("CREATE TABLE %s (a INTEGER, b INTEGER, s TEXT)" % t)
    A("CREATE TABLE %s (a INTEGER, b TEXT)" % u)
    A("CREATE TABLE %s (x INTEGER, y TEXT)" % v)
    A("CREATE TABLE %s (q INTEGER)" % e)

    # inserts
    A("INSERT INTO %s (a, b, s) VALUES (1, 2, 'one'), (3, 4, NULL), (5, 6, 'x')" % t)
    A("INSERT INTO %s (a, b) VALUES (7, 8), (9, 10)" % t)
    A("INSERT INTO %s VALUES (1, 'x'), (2, NULL), (3, 'y')" % u)
    A("INSERT INTO %s (x, y) VALUES (1, 'a'), (2, 'b'), (3, NULL)" % v)

    # --- UPDATE: swap reads the old row -----------------------------------
    A("UPDATE %s SET a = b, b = a WHERE a = 1" % t)          # OK 1, swaps 1<->2
    A("SELECT * FROM %s ORDER BY a" % t)
    A("UPDATE %s SET a = b, b = a" % t)                       # every row
    A("SELECT * FROM %s ORDER BY a" % t)

    # --- UPDATE: matched count even when nothing changes -------------------
    A("UPDATE %s SET a = a WHERE a = b" % t)                  # OK n
    A("UPDATE %s SET a = a WHERE a = 1000" % t)               # OK 0
    A("UPDATE %s SET a = a WHERE 0" % t)                      # OK 0
    A("UPDATE %s SET a = a WHERE NULL" % t)                   # OK 0
    A("UPDATE %s SET a = a WHERE NOT NULL" % t)               # OK 0

    # --- UPDATE: expressions of ticket 1 ----------------------------------
    A("UPDATE %s SET a = a + 1, s = upper(coalesce(s, 'z')) WHERE b %% 2 = 0" % t)
    A("SELECT * FROM %s ORDER BY a" % t)
    A("UPDATE %s SET s = s || '!' WHERE s IS NOT NULL" % t)
    A("UPDATE %s SET a = abs(0 - a) WHERE a > 4" % t)
    A("UPDATE %s SET b = b / 2, a = a * 2 WHERE a < 5" % t)
    A("SELECT * FROM %s ORDER BY a" % t)

    # --- UPDATE: text/int coercions ---------------------------------------
    A("UPDATE %s SET b = 42 WHERE a = 1" % u)                 # int -> TEXT col
    A("SELECT b FROM %s WHERE a = 1" % u)
    A("UPDATE %s SET a = '77' WHERE a = 2" % u)               # text -> INTEGER
    A("UPDATE %s SET a = '  88  '" % u)
    A("UPDATE %s SET a = '1.5'" % u)                          # ERR type (REAL)
    A("UPDATE %s SET a = 'zz'" % u)                           # ERR type (TEXT)
    A("UPDATE %s SET a = upper(b)" % u)                       # '42'/'NULL' -> ok?
    A("SELECT * FROM %s ORDER BY a" % u)
    A("UPDATE %s SET a = '42.0'" % u)                         # -> 42
    A("SELECT * FROM %s ORDER BY a" % u)

    # --- UPDATE: zero-row and empty-table ---------------------------------
    A("UPDATE %s SET a = 'zz' WHERE a = 999" % u)             # OK 0, no type err
    A("UPDATE %s SET a = 'zz'" % e)                           # OK 0 (empty table)
    A("UPDATE %s SET q = 'zz' WHERE q = 1" % e)               # OK 0
    A("UPDATE %s SET nosuch = 1" % e)                         # no_column (up front)
    A("UPDATE %s SET q = nosuchcol" % e)                      # no_column (up front)
    A("UPDATE %s SET q = length(1, 2)" % e)                   # arity (up front)
    A("DELETE FROM %s WHERE length(1, 2) = 1" % e)            # arity
    A("DELETE FROM %s WHERE nosuch = 1" % e)                  # no_column
    A("SELECT * FROM %s" % e)

    # --- UPDATE: errors change nothing ------------------------------------
    A("UPDATE %s SET a = 'zz', s = 'boom'" % u)               # type: atomic
    A("SELECT * FROM %s ORDER BY a" % u)
    A("UPDATE %s SET s = 'boom', a = 'zz'" % u)               # type: atomic
    A("SELECT * FROM %s ORDER BY a" % u)
    A("UPDATE %s SET a = b, b = a WHERE a = 'zz'" % u)        # type via WHERE
    A("SELECT * FROM %s ORDER BY a" % u)
    A("UPDATE nosuch SET a = 1")                              # no_table
    A("UPDATE nosuch SET nosuchcol = 1")                      # no_table first
    A("UPDATE nosuch SET a = length(1, 2)")                   # no_table first
    A("UPDATE %s SET nosuch = length(1, 2)" % u)              # arity before target
    A("UPDATE %s SET nosuch1 = 1, a = length(1, 2)" % u)      # target1 before rhs2
    A("UPDATE %s SET a = length(1, 2), nosuch1 = 1" % u)      # rhs1 before target2
    A("UPDATE %s SET nosuch1 = 1 WHERE length(1, 2) = 1" % u) # target before where
    A("UPDATE %s SET a = 1 WHERE nosuchcol = 2" % u)          # no_column
    A("UPDATE %s SET a = 1, a = 2" % u)                       # duplicate target
    A("SELECT * FROM %s ORDER BY a" % u)
    A("UPDATE %s SET" % u)                                    # syntax
    A("UPDATE %s SET a = 1," % u)                             # syntax
    A("UPDATE %s SET a = 1 WHERE" % u)                        # syntax
    A("UPDATE %s" % u)                                        # syntax
    A("UPDATE")                                               # syntax
    A("UPDATE %s SET a = 1 RETURNING *" % u)                  # syntax
    A("UPDATE %s SET a = 1 ORDER BY a" % u)                   # syntax
    A("UPDATE %s SET table = 1" % u)                          # syntax (reserved)
    A("UPDATE %s SET a = 1 + " % u)                           # syntax

    # --- DELETE ------------------------------------------------------------
    A("DELETE FROM %s WHERE a = 1000" % t)                    # OK 0
    A("DELETE FROM %s WHERE s IS NULL" % t)                   # OK n
    A("SELECT * FROM %s ORDER BY a" % t)
    A("DELETE FROM %s WHERE 1" % t)                           # OK n
    A("SELECT * FROM %s ORDER BY a" % t)
    A("DELETE FROM %s" % t)                                   # empties
    A("SELECT * FROM %s ORDER BY a" % t)
    A("DELETE FROM %s" % t)                                   # OK 0 now
    A("DELETE FROM nosuch")                                    # no_table
    A("DELETE FROM nosuch WHERE nosuchcol = 1")                # no_table first
    A("DELETE FROM %s WHERE nosuchcol = 1" % u)               # no_column
    A("DELETE FROM %s WHERE" % u)                             # syntax
    A("DELETE %s" % u)                                        # syntax (no FROM)
    A("DELETE FROM")                                          # syntax

    # --- DDL after DML: OK 0, never the previous count --------------------
    A("INSERT INTO %s (q) VALUES (1)" % e)
    A("DELETE FROM %s" % e)
    A("CREATE TABLE %se (z INTEGER)" % e)                     # OK 0
    A("CREATE TABLE %se (z INTEGER)" % e)                     # exists
    A("INSERT INTO %s (q) VALUES (2)" % e)
    A("CREATE TABLE %sf (z INTEGER)" % e)                     # OK 0 again
    A("SELECT * FROM %s ORDER BY q" % e)

    # --- interleaved reads after writes -----------------------------------
    A("SELECT a, b, s FROM %s WHERE a > 1 ORDER BY a, b, s" % t)
    A("SELECT DISTINCT y FROM %s ORDER BY y" % v)
    A("SELECT x, y FROM %s ORDER BY x DESC LIMIT 2" % v)
    A("SELECT count_like AS c FROM (1)" if False else
       "SELECT a * 2 AS d FROM %s WHERE s = 'x' ORDER BY d" % t)
    return s


# ---------------------------------------------------------------------------
# Randomised loop
# ---------------------------------------------------------------------------

def rand_int(rng):
    return rng.choice([0, 1, 2, 3, 5, 7, 10, 42, -1, -7, 100, 999])


def rand_text(rng):
    return rng.choice(["a", "b", "zz", "x y", "", "42", "one", "NULL-ish"])


def rand_int_expr(rng, cols):
    c = rng.choice(cols)
    k = rng.randint(0, 5)
    if k == 0:
        return c
    if k == 1:
        return "%s + %d" % (c, rand_int(rng))
    if k == 2:
        return "%s * 2" % c
    if k == 3:
        return "abs(%s)" % c
    if k == 4:
        return "coalesce(%s, %d)" % (c, rand_int(rng))
    return "%s %% 3" % c


def rand_text_expr(rng, cols):
    c = rng.choice(cols)
    k = rng.randint(0, 5)
    if k == 0:
        return c
    if k == 1:
        return "upper(%s)" % c
    if k == 2:
        return "lower(%s)" % c
    if k == 3:
        return "%s || '%s'" % (c, rand_text(rng))
    if k == 4:
        return "coalesce(%s, '%s')" % (c, rand_text(rng))
    return "%s || ''" % c


def rand_pred(rng, icols, tcols):
    k = rng.randint(0, 6)
    if k == 0 and icols:
        return "%s = %d" % (rng.choice(icols), rand_int(rng))
    if k == 1 and icols:
        return "%s > %d" % (rng.choice(icols), rand_int(rng))
    if k == 2 and icols:
        return "%s IS NULL" % rng.choice(icols)
    if k == 3 and tcols:
        return "%s = '%s'" % (rng.choice(tcols), rand_text(rng))
    if k == 4 and tcols:
        return "%s IS NOT NULL" % rng.choice(tcols)
    if k == 5 and icols:
        return "%s %% 2 = 0" % rng.choice(icols)
    if icols:
        return "%s < %d" % (rng.choice(icols), rand_int(rng))
    return "1"


def build_random(rng, sfx):
    t0 = "r%sa" % sfx   # a INTEGER, b INTEGER, s TEXT
    t1 = "r%sb" % sfx   # a INTEGER, y TEXT
    t2 = "r%sc" % sfx   # x INTEGER (small)
    s = []
    A = s.append
    A("CREATE TABLE %s (a INTEGER, b INTEGER, s TEXT)" % t0)
    A("CREATE TABLE %s (a INTEGER, y TEXT)" % t1)
    A("CREATE TABLE %s (x INTEGER)" % t2)
    A("INSERT INTO %s (a, b, s) VALUES (1, 2, 'one'), (3, NULL, 'two'), "
      "(5, 6, NULL), (7, 8, 'zz'), (9, 10, 'a b')" % t0)
    A("INSERT INTO %s (a, y) VALUES (1, 'p'), (2, 'q'), (3, NULL), (4, '42')" % t1)
    A("INSERT INTO %s (x) VALUES (1), (2), (3), (4), (5)" % t2)

    plans = [
        (t0, ["a", "b"], ["s"]),
        (t1, ["a"], ["y"]),
        (t2, ["x"], []),
    ]
    for _ in range(120):
        tab, icols, tcols = plans[rng.randrange(len(plans))]
        k = rng.random()
        if k < 0.28:
            # INSERT
            cols = icols + tcols
            rng.shuffle(cols)
            if rng.random() < 0.3:
                cols = cols[: rng.randint(1, len(cols))]
            vals = []
            for c in cols:
                if c in icols:
                    vals.append(str(rand_int(rng)))
                else:
                    vals.append("'%s'" % rand_text(rng))
            A("INSERT INTO %s (%s) VALUES (%s)" % (
                tab, ", ".join(cols), ", ".join(vals)))
        elif k < 0.60:
            # UPDATE
            targets = icols + tcols
            rng.shuffle(targets)
            nset = rng.randint(1, min(2, len(targets)))
            sets = []
            for c in targets[:nset]:
                if c in icols:
                    sets.append("%s = %s" % (c, rand_int_expr(rng, icols)))
                else:
                    sets.append("%s = %s" % (c, rand_text_expr(rng, tcols)))
            stmt = "UPDATE %s SET %s" % (tab, ", ".join(sets))
            if rng.random() < 0.75:
                stmt += " WHERE " + rand_pred(rng, icols, tcols)
            A(stmt)
        elif k < 0.80:
            # DELETE
            stmt = "DELETE FROM %s" % tab
            if rng.random() < 0.7:
                stmt += " WHERE " + rand_pred(rng, icols, tcols)
            A(stmt)
        else:
            # SELECT
            items = []
            for c in icols + tcols:
                if rng.random() < 0.6:
                    items.append(c)
            if not items:
                items = icols[:1] or tcols[:1]
            stmt = "SELECT %s FROM %s" % (", ".join(items), tab)
            if rng.random() < 0.5:
                stmt += " WHERE " + rand_pred(rng, icols, tcols)
            if rng.random() < 0.7:
                stmt += " ORDER BY " + ", ".join(items)
                if rng.random() < 0.4:
                    stmt += " DESC"
            A(stmt)
    # interleaved verification reads
    for tab, icols, tcols in plans:
        A("SELECT * FROM %s ORDER BY %s" % (
            tab, ", ".join(icols + tcols)))
    return s


def run_battery(cand, ref, stmts, rng, label):
    fails = 0
    for stmt in stmts:
        s2 = scramble(stmt, rng)
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
        err = compare(stmt, a, b)
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
    sfx = os.urandom(4).hex()
    rng = random.Random(seed)
    cand = Conn(CAND)
    ref = Conn(REF)
    fails = run_battery(cand, ref, build_fixed(sfx), rng, "fixed")
    fails += run_battery(cand, ref, build_random(rng, sfx), rng, "random")
    if fails:
        print("FAIL (%d)" % fails)
        sys.exit(1)
    print("PASS")


if __name__ == "__main__":
    main()
