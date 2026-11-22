#!/usr/bin/env python3
"""Differential tester: candidate sdb vs the reference server.

Raw sockets only on purpose: the workspace must not contain any SQL engine
binding or parser library, not even in tests.
"""
import os
import random
import re
import socket
import sys

CAND = ("127.0.0.1", 5580)
REF = ("ref-sql", 5480)

KEYWORDS = [
    "select", "from", "where", "order", "by", "as", "and", "or", "not", "is",
    "null", "insert", "into", "values", "create", "table", "integer", "text",
    "limit", "offset", "asc", "desc", "length", "upper", "lower", "abs",
    "coalesce",
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
    # statements in the battery use "ORDER BY" (possibly case/whitespace
    # randomised) only as a clause, never inside a string literal
    parts = re.split(r"'(?:[^']|'')*'", stmt)
    return any(re.search(r"\border\s+by\b", p, re.I) for p in parts)


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
            len(cand[2]), cand[3], len(ref[2]), ref[3])
    if cand[3] != ref[3]:
        return "count: cand=%s ref=%s" % (cand[3], ref[3])
    if has_order_by(stmt):
        if cand[2] != ref[2]:
            return "order: cand=%r ref=%r" % (cand[2], ref[2])
    else:
        if sorted(cand[2]) != sorted(ref[2]):
            return "rows: cand=%r ref=%r" % (sorted(cand[2]), sorted(ref[2]))
    return None


def build(suffix):
    t0 = "t%sa" % suffix
    t1 = "t%sb" % suffix
    t2 = "t%sc" % suffix
    s = []
    A = s.append

    # --- schema -----------------------------------------------------------
    A("CREATE TABLE %s (id INTEGER, s TEXT, n INTEGER)" % t0)
    A("CREATE TABLE %s (a INTEGER, b INTEGER, c INTEGER)" % t1)
    A("CREATE TABLE %s (x INTEGER, y TEXT)" % t2)
    A("CREATE TABLE %s (id INTEGER)" % t0)          # exists
    A("CREATE TABLE %sd (q INTEGER, q TEXT)" % t1)  # duplicate column

    # --- inserts: full rows and column lists in any order/subset ----------
    A("INSERT INTO %s (id, s, n) VALUES (1, 'hello', 10), (2, 'world', NULL), "
      "(3, NULL, -5), (4, '', 0), (5, 'x y', 7)" % t0)
    A("INSERT INTO %s (id) VALUES (6), (7)" % t0)             # s,n -> NULL
    A("INSERT INTO %s (n, id) VALUES (99, 8)" % t0)           # reordered
    A("INSERT INTO %s (s, id) VALUES ('tab\\there', 9)" % t0)  # n -> NULL
    A("INSERT INTO %s VALUES (10, 'full', 42)" % t0)          # ticket-0 form
    A("INSERT INTO %s (n, s, id) VALUES (3, 'nine', 11)" % t0)
    A("INSERT INTO %s (id, s, n) VALUES (12, 'twelve', 12)" % t0)
    A("INSERT INTO %s (id, s) VALUES (13)" % t0)              # arity
    A("INSERT INTO %s (id, nope) VALUES (14)" % t0)           # no_column
    A("INSERT INTO %s (id, s, n) VALUES ('zz', 'a', 1)" % t0)  # type
    A("INSERT INTO %s (x, y) VALUES (1, 'a'), (2, NULL), (3, 'zz'), (4, '4')" % t2)
    A("INSERT INTO %s (x) VALUES (5)" % t2)
    A("INSERT INTO %s (b, a) VALUES (2, 1), (4, 3)" % t1)
    A("INSERT INTO %s (a, b, c) VALUES (5, 5, 5)" % t1)
    A("INSERT INTO %s (a) VALUES (6)" % t1)
    A("INSERT INTO %s (c, b, a) VALUES (-1, -2, -3)" % t1)
    A("INSERT INTO %s (a, b, c) VALUES (7, 8, 9), (10, 0, NULL)" % t1)

    # --- constant expressions, no FROM ------------------------------------
    A("SELECT 1 + 2 * 3 AS p")
    A("SELECT 10 - 2 - 3 AS q")
    A("SELECT 100 / 10 / 2 AS r")
    A("SELECT 100 % 7 % 4 AS s")
    A("SELECT -7 / 2 AS a, -7 % 3 AS b, 7 / -2 AS c, -7 / -2 AS d")
    A("SELECT 5 / 0 AS a, 5 % 0 AS b, -5 / 0 AS c, -5 % 0 AS d")
    A("SELECT - -5 AS a, -(2 + 3) AS b, -2 * 3 AS c, 2 * -3 AS d, -2 + 3 AS e")
    A("SELECT 1 + 2 < 4 AS a, 1 < 2 + 3 AS b, 2 * 3 = 6 AS c, 6 = 2 * 3 AS d")
    A("SELECT 1 = 2 < 3 AS a, 1 < 2 = 1 AS b, 3 > 2 > 1 AS c")
    A("SELECT NOT 1 = 0 AS a, NOT 0 = 1 AS b, NOT (1 AND 0) AS c")
    A("SELECT 1 OR 0 AND 0 AS a, 0 AND 1 OR 1 AS b, 1 AND 1 OR 0 AND 0 AS c")
    A("SELECT NOT 1 AND 0 AS a, NOT 0 OR 1 AS b, NOT 1 OR 1 AND 0 AS c")
    A("SELECT NULL AND 1 AS a, NULL AND 0 AS b, NULL OR 1 AS c, NULL OR 0 AS d")
    A("SELECT NOT NULL AS a, NULL AND NULL AS b, NULL OR NULL AS c, "
      "1 AND NULL AS d, 1 OR NULL AS e")
    A("SELECT 1 = 1 AS a, 1 <> 2 AS b, 1 != 2 AS c, 2 < 3 AS d, 3 <= 3 AS e, "
      "4 > 4 AS f, 4 >= 5 AS g")
    A("SELECT NULL = NULL AS a, 1 < NULL AS b, NULL IS NULL AS c, "
      "1 IS NULL AS d, NULL IS NOT NULL AS e, 1 IS NOT NULL AS f")
    A("SELECT 1 IS 1 AS a, 1 IS NOT 2 AS b, 'a' IS 'a' AS c, "
      "'a' IS NOT 'b' AS d, NULL IS 1 AS e, 1 IS NULL AS f")
    A("SELECT 'a' || 'b' AS a, 'a' || 1 AS b, 1 || 2 AS c, NULL || 'x' AS d, "
      "'x' || NULL AS e, '' || '' AS f")
    A("SELECT 'a' || 'b' || 'c' AS a, -1 || 'x' AS b, 1 || NULL || 'z' AS c")
    A("SELECT length('') AS a, length('abc') AS b, upper('aBc') AS c, "
      "lower('AbC') AS d, abs(-9) AS e, abs(9) AS f")
    A("SELECT coalesce(NULL, 5) AS a, coalesce(1, 5) AS b, "
      "coalesce(NULL, NULL) AS c, coalesce(NULL, NULL, 7) AS d")
    A("SELECT length(12345) AS a, upper(42) AS b, abs(NULL) AS c, "
      "length(NULL) AS d, upper(NULL) AS e, coalesce(NULL, 'z') AS f")
    A("SELECT (1 + 2) * 3 AS a, -(1 + 2) * 3 AS b, 2 * (3 + 4) AS c")
    A("SELECT 1 AS e0, 2 AS e1")
    A("SELECT 1 + 1 AS e0, 'x' || 'y' AS e1, NULL AS e2")
    A("SELECT 1 AS x WHERE 0")
    A("SELECT 1 AS x WHERE 1")
    A("SELECT 1 AS x WHERE NULL")
    A("SELECT 1 AS x WHERE NOT NULL")
    A("SELECT nosuch AS x")
    A("SELECT length(1, 2) AS x")
    A("SELECT coalesce() AS x")
    A("SELECT foo(1) AS x")
    A("SELECT foo(1) AS x FROM nosuch")
    A("SELECT length(1, 2) AS x FROM nosuch")
    A("SELECT nosuch FROM nosuch")
    A("SELECT 1 = ")
    A("SELECT")
    A("SELECT FROM %s" % t0)

    # --- WHERE / projection over tables -----------------------------------
    A("SELECT * FROM %s ORDER BY id" % t0)
    A("SELECT id, s FROM %s ORDER BY id" % t0)
    A("SELECT id, s, n FROM %s WHERE id >= 3 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE s IS NULL ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE n IS NULL ORDER BY id" % t0)
    A("SELECT id * 2 AS d FROM %s WHERE id + 1 = 4 ORDER BY id" % t0)
    A("SELECT s FROM %s WHERE s = 'hello' ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE id > 1 AND id < 4 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE NOT id = 3 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE id = 1 OR id = 5 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE s || 'x' = 'hellox' ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE length(s) = 5 ORDER BY id" % t0)
    A("SELECT upper(s) AS u FROM %s WHERE id <= 3 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE id %% 2 = 0 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE NULL ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE NOT NULL ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE id IS NOT NULL ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE coalesce(n, 0) > 0 ORDER BY id" % t0)
    A("SELECT n FROM %s WHERE n IS NULL ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE nosuch > 1 ORDER BY id" % t0)
    A("SELECT nosuch AS x FROM %s ORDER BY id" % t0)
    A("SELECT nosuch AS x FROM %s" % t0)
    A("SELECT id FROM %s ORDER BY id DESC" % t0)
    A("SELECT id, id * 2 AS d FROM %s ORDER BY d DESC" % t0)
    A("SELECT id * 2 AS d FROM %s ORDER BY d, id" % t0)
    A("SELECT id AS ident FROM %s WHERE id > 10 ORDER BY id" % t0)
    A("SELECT *, id + 1 AS inc FROM %s ORDER BY id" % t0)
    A("SELECT id, s FROM %s WHERE s ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE NOT s ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE s AND id > 5 ORDER BY id" % t0)
    A("SELECT id, n FROM %s WHERE n ORDER BY id" % t0)
    A("SELECT id, n FROM %s WHERE n IS NULL AND s IS NULL ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE (id = 1) OR (id = 2 AND n = 10) ORDER BY id" % t0)
    A("SELECT id + n AS s2, id * n AS p FROM %s ORDER BY id" % t0)
    A("SELECT -n AS nn FROM %s WHERE n IS NOT NULL ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE -n = 5 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE n / 2 = 0 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE abs(n) = 5 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE upper(s) = 'HELLO' ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE lower(s) = 'world' ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE length(s) > 3 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE 1 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE 0 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE id %% 3 = 1 AND id < 10 ORDER BY id" % t0)
    A("SELECT id FROM %s WHERE s IS NOT NULL AND n IS NOT NULL ORDER BY id" % t0)
    A("SELECT id FROM %s ORDER BY id LIMIT 3" % t0)
    A("SELECT id FROM %s ORDER BY id LIMIT 2 OFFSET 4" % t0)
    A("SELECT id FROM %s WHERE id > 5 ORDER BY id LIMIT 2" % t0)
    A("SELECT id FROM %s WHERE id > 100 ORDER BY id LIMIT 5" % t0)
    A("SELECT * FROM %s ORDER BY a" % t1)
    A("SELECT a, b, c FROM %s WHERE a > b ORDER BY a" % t1)
    A("SELECT a, b, c FROM %s WHERE c IS NULL ORDER BY a" % t1)
    A("SELECT a + b AS ab, a - b AS d, a * b AS m, a %% b AS r FROM %s ORDER BY a" % t1)
    A("SELECT a FROM %s WHERE b = 0 ORDER BY a" % t1)
    A("SELECT a FROM %s WHERE NOT c ORDER BY a" % t1)
    A("SELECT a FROM %s WHERE a < 0 OR b < 0 ORDER BY a" % t1)
    A("SELECT x, y FROM %s WHERE y IS NOT NULL ORDER BY x" % t2)
    A("SELECT x FROM %s WHERE y = 'zz' ORDER BY x" % t2)
    A("SELECT x FROM %s WHERE y = '4' ORDER BY x" % t2)
    A("SELECT x, y || '!' AS e FROM %s ORDER BY x" % t2)
    A("SELECT x FROM %s WHERE x < 3 ORDER BY x" % t2)
    A("SELECT x FROM %s WHERE y IS NULL ORDER BY x" % t2)
    A("SELECT x FROM %s WHERE length(y) = 1 ORDER BY x" % t2)
    A("SELECT x FROM %s WHERE y > 'b' ORDER BY x" % t2)
    A("SELECT x * x AS sq FROM %s WHERE x * x > 4 ORDER BY x" % t2)

    # --- keyword case + whitespace randomisation --------------------------
    return s


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


def robustness(cand):
    """Garbage / huge lines / pipelining must not kill the candidate."""
    fails = 0
    try:
        cand.sock.sendall(b"\x00\x01\xff garbage \xfe\n")
        r = cand.file.readline().decode()
        if not r.startswith("ERR "):
            print("[robust] expected ERR for garbage, got %r" % r)
            fails += 1
        cand.sock.sendall(b"SELECT 1\n")
        r = cand.file.readline().decode()
        if not r.startswith("ROWS "):
            print("[robust] server unusable after garbage: %r" % r)
            fails += 1
        else:
            cand.file.readline()
            while True:
                line = cand.file.readline().decode()
                if line.startswith("END "):
                    break
        # pipelining: three statements, three answers in order
        cand.sock.sendall(b"SELECT 1 AS a\nSELECT 2 AS b\nSELECT 3 AS c\n")
        for expect in ("a", "b", "c"):
            frame = cand.file.readline().decode()
            if not frame.startswith("ROWS 1"):
                print("[robust] pipeline frame %r" % frame)
                fails += 1
            hdr = cand.file.readline().decode().rstrip("\n")
            if hdr != expect:
                print("[robust] pipeline header %r != %r" % (hdr, expect))
                fails += 1
            cand.file.readline()
            while True:
                line = cand.file.readline().decode()
                if line.startswith("END "):
                    break
        # 2 MiB single line must not kill the server
        big = b"SELECT '" + b"x" * (2 * 1024 * 1024) + b"'\n"
        cand.sock.sendall(big)
        r = cand.file.readline().decode()
        if not r.startswith("ROWS "):
            print("[robust] big line answer %r" % r)
            fails += 1
        else:
            cand.file.readline()
            while True:
                line = cand.file.readline().decode()
                if line.startswith("END "):
                    break
        cand.sock.sendall(b"SELECT 7 AS ok\n")
        r = cand.file.readline().decode()
        if not r.startswith("ROWS "):
            print("[robust] dead after big line: %r" % r)
            fails += 1
        else:
            cand.file.readline()
            while True:
                line = cand.file.readline().decode()
                if line.startswith("END "):
                    break
    except Exception as e:  # noqa: BLE001
        print("[robust] exception: %s" % e)
        fails += 1
    if not fails:
        print("[robust] OK")
    return fails


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 20261120
    # unique table names per run: the reference server persists between runs
    suffix = os.urandom(4).hex()
    rng = random.Random(seed)
    stmts = build(suffix)
    cand = Conn(CAND)
    ref = Conn(REF)
    fails = run_battery(cand, ref, stmts, rng, "main")
    fails += robustness(cand)
    # a fresh connection must still work
    cand2 = Conn(CAND)
    r = cand2.ask("SELECT 1 AS alive")
    if r[0] != "ROWS":
        print("[fresh] new connection failed: %r" % (r,))
        fails += 1
    if fails:
        print("FAIL (%d)" % fails)
        sys.exit(1)
    print("PASS")


if __name__ == "__main__":
    main()
