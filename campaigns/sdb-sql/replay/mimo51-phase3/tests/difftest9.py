#!/usr/bin/env python3
"""Ticket 9 differential battery: constrained DDL/DML, candidate vs ref.

Every scenario runs against both servers from a fresh random table suffix;
answers must match exactly (rows compared as a multiset without ORDER BY,
exactly with it). Errors are compared by class token.
"""
import os
import random
import socket
import sys

CAND = ("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 5580)
REF = ("ref-sql", 5480)
ROUNDS = int(sys.argv[2]) if len(sys.argv) > 2 else 20


class Conn:
    def __init__(self, addr):
        self.sock = socket.create_connection(addr, timeout=30)
        self.file = self.sock.makefile("rb")

    def ask(self, stmt):
        self.sock.sendall((stmt + "\n").encode())
        first = self.file.readline().decode()
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
        if first.startswith("ERR "):
            return ("ERR", first.split()[1])
        return ("OK", first.rstrip("\n"))


def has_order(stmt):
    return "ORDER BY" in stmt.upper()


def compare(stmt, c, r):
    if c[0] != r[0]:
        return "kind cand=%r ref=%r" % (c, r)
    if c[0] == "ERR":
        if c[1] != r[1]:
            return "class cand=%s ref=%s" % (c[1], r[1])
        return None
    if c[0] == "OK":
        if c[1] != r[1]:
            return "ok cand=%s ref=%s" % (c[1], r[1])
        return None
    if c[1] != r[1]:
        return "header cand=%r ref=%r" % (c[1], r[1])
    if c[3] != r[3]:
        return "count cand=%s ref=%s" % (c[3], r[3])
    if has_order(stmt):
        if c[2] != r[2]:
            return "rows cand=%r ref=%r" % (c[2][:8], r[2][:8])
    else:
        if sorted(c[2]) != sorted(r[2]):
            return "rows cand=%r ref=%r" % (sorted(c[2])[:8], sorted(r[2])[:8])
    return None


def scenarios(sfx):
    T = {}
    S = []

    def add(*stmts):
        S.extend(stmts)

    p, c = sfx + "p", sfx + "c"
    add(
        "CREATE TABLE %s (id INTEGER PRIMARY KEY, v INTEGER UNIQUE, w TEXT NOT NULL)" % p,
        "CREATE TABLE %s (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES %s(id), n INTEGER DEFAULT 7, s TEXT DEFAULT 'hi')" % (c, p),
        "INSERT INTO %s VALUES (1, 10, 'a'), (2, 20, 'b')" % p,
        "INSERT INTO %s (v, w) VALUES (30, 'c')" % p,
        "INSERT INTO %s (id, v, w) VALUES (NULL, 40, 'd')" % p,
        "INSERT INTO %s (v, w) VALUES (NULL, 'e')" % p,
        "INSERT INTO %s VALUES (5, 10, 'f')" % p,
        "INSERT INTO %s VALUES (6, 60, NULL)" % p,
        "SELECT * FROM %s ORDER BY id" % p,
        "INSERT INTO %s (pid) VALUES (1), (2), (NULL)" % c,
        "INSERT INTO %s (pid, n, s) VALUES (99, 1, 'x')" % c,
        "INSERT INTO %s (pid, n) VALUES (1, NULL)" % c,
        "INSERT INTO %s (id, pid) VALUES (7, 7)" % c,
        "SELECT * FROM %s ORDER BY id" % c,
        "UPDATE %s SET v = 20 WHERE id = 1" % p,
        "UPDATE %s SET w = NULL WHERE id = 2" % p,
        "UPDATE %s SET w = 'z' WHERE id = 6" % p,
        "UPDATE %s SET id = 2 WHERE id = 1" % p,
        "UPDATE %s SET id = NULL WHERE id = 3" % p,
        "DELETE FROM %s WHERE id = 1" % p,
        "DELETE FROM %s WHERE id = 2" % p,
        "SELECT * FROM %s ORDER BY id" % p,
        "SELECT * FROM %s ORDER BY id" % c,
        "UPDATE %s SET pid = 4 WHERE id = 1" % c,
        "UPDATE %s SET pid = 4 WHERE id = 3" % c,
        "UPDATE %s SET pid = NULL WHERE id = 3" % c,
        "SELECT * FROM %s ORDER BY id" % c,
        "DELETE FROM %s" % c,
        "DELETE FROM %s" % p,
        "SELECT COUNT(*) FROM %s" % p,
    )
    # table-level constraints + composite keys
    a, b = sfx + "a", sfx + "b"
    add(
        "CREATE TABLE %s (x INTEGER, y TEXT, z INTEGER, PRIMARY KEY (x), UNIQUE (y))" % a,
        "CREATE TABLE %s (k1 INTEGER, k2 TEXT, v INTEGER, PRIMARY KEY (k1, k2))" % b,
        "INSERT INTO %s VALUES (1, 'a', 5), (2, 'b', 6)" % a,
        "INSERT INTO %s (y, z) VALUES ('c', 7)" % a,
        "INSERT INTO %s VALUES (1, 'a', 9)" % a,
        "INSERT INTO %s VALUES (1, 'c', 1)" % b,
        "INSERT INTO %s VALUES (1, 'c', 2)" % b,
        "INSERT INTO %s (k2, v) VALUES ('d', 3)" % b,
        "INSERT INTO %s (k1, k2, v) VALUES (NULL, 'e', 4)" % b,
        "SELECT * FROM %s ORDER BY x" % a,
        "SELECT * FROM %s ORDER BY k1, k2" % b,
        "UPDATE %s SET y = 'b' WHERE x = 1" % a,
        "UPDATE %s SET x = 2 WHERE x = 1" % a,
        "UPDATE %s SET k1 = 1 WHERE k2 = 'b'" % b,
        "SELECT * FROM %s ORDER BY x" % a,
        "SELECT * FROM %s ORDER BY k1, k2" % b,
    )
    # defaults of every flavor + column-list inserts
    d = sfx + "d"
    add(
        "CREATE TABLE %s (a INTEGER, b INTEGER DEFAULT -3, t TEXT DEFAULT 'q', u TEXT DEFAULT NULL, k INTEGER DEFAULT 0)" % d,
        "INSERT INTO %s (a) VALUES (1)" % d,
        "INSERT INTO %s (a, b) VALUES (2, NULL)" % d,
        "INSERT INTO %s (t, u) VALUES (NULL, 'w')" % d,
        "INSERT INTO %s VALUES (4, 1, 'x', 'y', 2)" % d,
        "INSERT INTO %s (a, k) VALUES (5, NULL)" % d,
        "SELECT * FROM %s ORDER BY a" % d,
        "INSERT INTO %s (a, b) VALUES (6, 'txt')" % d,
        "INSERT INTO %s DEFAULT VALUES" % d,
        "INSERT INTO %s (a) DEFAULT VALUES" % d,
        "SELECT * FROM %s ORDER BY a" % d,
    )
    # self reference
    e = sfx + "e"
    add(
        "CREATE TABLE %s (id INTEGER PRIMARY KEY, mgr INTEGER REFERENCES %s(id))" % (e, e),
        "INSERT INTO %s (mgr) VALUES (NULL)" % e,
        "INSERT INTO %s (mgr) VALUES (1)" % e,
        "INSERT INTO %s (id, mgr) VALUES (9, 9)" % e,
        "INSERT INTO %s (mgr) VALUES (9), (8)" % e,
        "SELECT * FROM %s ORDER BY id" % e,
        "UPDATE %s SET id = 20 WHERE id = 1" % e,
        "UPDATE %s SET id = 2 WHERE id = 20" % e,
        "UPDATE %s SET mgr = id" % e,
        "SELECT * FROM %s ORDER BY id" % e,
        "DELETE FROM %s WHERE id = 9" % e,
        "SELECT * FROM %s ORDER BY id" % e,
        "DELETE FROM %s" % e,
    )
    # AUTOINCREMENT
    ai = sfx + "ai"
    add(
        "CREATE TABLE %s (id INTEGER PRIMARY KEY AUTOINCREMENT, s TEXT)" % ai,
        "INSERT INTO %s (s) VALUES ('a')" % ai,
        "INSERT INTO %s VALUES (10, 'b')" % ai,
        "INSERT INTO %s (s) VALUES ('c')" % ai,
        "DELETE FROM %s WHERE id = 10" % ai,
        "INSERT INTO %s (s) VALUES ('d')" % ai,
        "SELECT id, s FROM %s ORDER BY id" % ai,
        "DELETE FROM %s" % ai,
        "INSERT INTO %s (s) VALUES ('e')" % ai,
        "SELECT id, s FROM %s ORDER BY id" % ai,
    )
    # TEXT primary key
    tp = sfx + "tp"
    add(
        "CREATE TABLE %s (k TEXT PRIMARY KEY, v INTEGER NOT NULL DEFAULT 1)" % tp,
        "INSERT INTO %s (k) VALUES ('a')" % tp,
        "INSERT INTO %s VALUES (NULL, 2)" % tp,
        "INSERT INTO %s (k, v) VALUES ('a', 3)" % tp,
        "INSERT INTO %s (v) VALUES (4)" % tp,
        "UPDATE %s SET k = 'a' WHERE k = 'b'" % tp,
        "UPDATE %s SET k = NULL WHERE k = 'a'" % tp,
        "SELECT * FROM %s ORDER BY k" % tp,
    )
    # error ordering probes
    o = sfx + "o"
    add(
        "CREATE TABLE %s (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER NOT NULL)" % o,
        "INSERT INTO %s VALUES (1, 'a', 1)" % o,
        "INSERT INTO %s VALUES (2, 'a', 2), (3, 'b', 'txt')" % o,
        "INSERT INTO %s VALUES (2, 'b', 'txt'), (3, 'a', 3)" % o,
        "INSERT INTO %s VALUES (2, 'b', 'txt'), (4, 'a', 4)" % o,
        "UPDATE %s SET u = 'a', n = 'txt' WHERE id = 1" % o,
        "UPDATE %s SET u = 'a', n = NULL WHERE id = 1" % o,
        "SELECT * FROM %s ORDER BY id" % o,
    )
    # structural probes (broken foreign keys)
    g1, g2 = sfx + "g1", sfx + "g2"
    add(
        "CREATE TABLE %s (v INTEGER)" % g1,
        "CREATE TABLE %s (id INTEGER PRIMARY KEY, pv INTEGER REFERENCES %s(v))" % (g2, g1),
        "INSERT INTO %s VALUES (7)" % g1,
        "INSERT INTO %s VALUES (1, 7)" % g2,
        "UPDATE %s SET v = 8 WHERE v = 99" % g1,
        "DELETE FROM %s WHERE v = 99" % g1,
        "INSERT INTO %s (pv) VALUES (7)" % g2,
        "UPDATE %s SET pv = 7" % g2,
        "DELETE FROM %s" % g2,
        "INSERT INTO %s (id, pv) VALUES ('zz', 7)" % g2,
        "SELECT * FROM %s" % g2,
        "CREATE TABLE %s (id INTEGER PRIMARY KEY, x INTEGER REFERENCES %s(nosuch))" % (sfx + "g3", g1),
        "INSERT INTO %s VALUES (1, 5)" % (sfx + "g3"),
        "UPDATE %s SET x = 5" % (sfx + "g3"),
        "DELETE FROM %s" % (sfx + "g3"),
        "CREATE TABLE %s (id INTEGER PRIMARY KEY, x INTEGER REFERENCES %s)" % (sfx + "g4", g1),
        "INSERT INTO %s VALUES (1, 5)" % (sfx + "g4"),
        "CREATE TABLE %s (id INTEGER PRIMARY KEY, x INTEGER REFERENCES %s(v) ON DELETE CASCADE)" % (sfx + "g5", g1),
        "INSERT INTO %s VALUES (1, 7)" % (sfx + "g5"),
        "DELETE FROM %s WHERE v = 7" % g1,
        "SELECT * FROM %s" % (sfx + "g5"),
    )
    # update gating probes
    u1, u2 = sfx + "u1", sfx + "u2"
    add(
        "CREATE TABLE %s (id INTEGER PRIMARY KEY, own INTEGER REFERENCES %s(id), ref INTEGER)" % (u1, sfx + "u0"),
        "CREATE TABLE %s (v INTEGER)" % (sfx + "u0"),
        "CREATE TABLE %s (x INTEGER REFERENCES %s(ref))" % (u2, u1),
        "INSERT INTO %s VALUES (1)" % (sfx + "u0"),
        "INSERT INTO %s (id, own, ref) VALUES (1, NULL, 7)" % u1,
        "UPDATE %s SET own = NULL" % u1,
        "UPDATE %s SET ref = 8" % u1,
        "UPDATE %s SET id = 5" % u1,
        "UPDATE %s SET id = 5 WHERE ref = 7" % u1,
        "UPDATE %s SET ref = 7 WHERE id = 1" % u1,
        "DELETE FROM %s WHERE id = 9" % u1,
        "SELECT * FROM %s" % u1,
    )
    # multi-row update window + unique shift timing
    sh = sfx + "sh"
    add(
        "CREATE TABLE %s (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER)" % sh,
        "INSERT INTO %s VALUES (1, 'a', 1), (2, 'b', 2), (3, 'c', 3)" % sh,
        "UPDATE %s SET u = 'z' ORDER BY id LIMIT 1" % sh,
        "UPDATE %s SET u = 'q', n = n + 1 WHERE id = 2" % sh,
        "UPDATE %s SET id = id + 1" % sh,
        "UPDATE %s SET n = n + 1 ORDER BY id DESC LIMIT 2" % sh,
        "SELECT * FROM %s ORDER BY id" % sh,
    )
    return S


def main():
    rng = random.Random(9091)
    cand = Conn(CAND)
    ref = Conn(REF)
    fails = 0
    total = 0
    for r in range(ROUNDS):
        sfx = "dt9_%d_%s_" % (r, os.urandom(4).hex())
        stmts = scenarios(sfx)
        for s in stmts:
            total += 1
            # randomize keyword case the way the harness does
            s2 = scramble(s, rng) if rng.random() < 0.5 else s
            try:
                c = cand.ask(s2)
            except Exception as e:
                print("CAND CRASH on %r: %s" % (s2, e))
                return 1
            try:
                rres = ref.ask(s2)
            except Exception as e:
                print("REF CRASH on %r: %s" % (s2, e))
                return 1
            diff = compare(s2, c, rres)
            if diff:
                fails += 1
                print("MISMATCH [%s]\n  stmt: %s\n  cand: %r\n  ref : %r\n  why : %s"
                      % (sfx, s2, c, rres, diff))
                if fails > 15:
                    print("too many failures")
                    return 1
    print("checked %d statements, %d mismatches" % (total, fails))
    return 1 if fails else 0


def scramble(stmt, rng):
    kws = ["select", "from", "where", "insert", "into", "values", "create",
           "table", "integer", "text", "primary", "key", "unique", "not",
           "null", "default", "references", "update", "set", "delete",
           "order", "by", "limit", "offset", "and", "or", "as", "on"]
    out = stmt
    for kw in kws:
        if kw in out.lower() and rng.random() < 0.4:
            idx = out.lower().find(kw)
            repl = kw.upper() if rng.random() < 0.5 else kw
            out = out[:idx] + repl + out[idx + len(kw):]
    return out


if __name__ == "__main__":
    sys.exit(main())
