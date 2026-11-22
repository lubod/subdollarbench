#!/usr/bin/env python3
"""Round-2 probes: ORDER BY resolution, error precedence, DML aliases,
dup aliases, forward ON references. Prints cand vs ref frames."""
import os
import socket
import sys

CAND = ("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 5580)
REF = ("ref-sql", 5480)


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
            return "ROWS[%s] n=%d %s" % (hdr, n, rows[:7])
        return first.rstrip("\n")


def main():
    sfx = "pc_" + os.urandom(3).hex() + "_"
    T1, T2, T3 = sfx + "1", sfx + "2", sfx + "3"
    setup = [
        "CREATE TABLE %s (id INTEGER, k INTEGER, s TEXT)" % T1,
        "INSERT INTO %s VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, NULL)" % T1,
        "CREATE TABLE %s (k INTEGER, v INTEGER, id INTEGER)" % T2,
        "INSERT INTO %s VALUES (10, 100, 1), (20, 200, 2), (50, NULL, 5)" % T2,
        "CREATE TABLE %s (k INTEGER, w TEXT)" % T3,
        "INSERT INTO %s VALUES (10, 'p'), (20, 'q')" % T3,
    ]
    P = []
    A = P.append
    # ---- ORDER BY resolution ----
    A("SELECT * FROM %s, %s ORDER BY id" % (T1, T2))
    A("SELECT * FROM %s, %s WHERE %s.k = %s.k ORDER BY id" % (T1, T2, T1, T2))
    A("SELECT %s.v, %s.id FROM %s, %s ORDER BY id" % (T2, T2, T1, T2))
    A("SELECT %s.s, %s.id FROM %s, %s ORDER BY id" % (T1, T2, T1, T2))
    A("SELECT %s.s, %s.id FROM %s, %s ORDER BY id" % (T1, T1, T1, T2))
    A("SELECT DISTINCT %s.s FROM %s, %s ORDER BY id" % (T1, T1, T2))
    A("SELECT %s.id AS q, %s.s FROM %s, %s ORDER BY q" % (T1, T1, T1, T2))
    A("SELECT %s.id AS q, %s.s FROM %s, %s ORDER BY id" % (T1, T1, T1, T2))
    A("SELECT %s.s FROM %s, %s ORDER BY id + 1" % (T1, T1, T2))
    A("SELECT %s.s FROM %s, %s ORDER BY 1" % (T1, T1, T2))
    A("SELECT COUNT(*) FROM %s, %s ORDER BY id" % (T1, T2))
    A("SELECT * FROM %s, %s ORDER BY %s.id" % (T1, T2, T2))
    A("SELECT %s.s, %s.v FROM %s, %s ORDER BY id" % (T1, T2, T1, T2))
    A("SELECT * FROM %s, %s ORDER BY k" % (T1, T2))
    A("SELECT %s.s AS id, %s.k FROM %s, %s ORDER BY id" % (T1, T1, T1, T2))
    A("SELECT * FROM %s, %s ORDER BY s" % (T1, T2))
    A("SELECT %s.* FROM %s, %s ORDER BY id" % (T2, T1, T2))
    A("SELECT %s.s, * FROM %s, %s ORDER BY id" % (T1, T1, T2))
    A("SELECT * FROM %s, %s ORDER BY id + 1" % (T1, T2))
    A("SELECT * FROM %s, %s GROUP BY id" % (T1, T2))
    A("SELECT * FROM %s, %s ORDER BY zz" % (T1, T2))
    A("SELECT * FROM %s, %s ORDER BY v" % (T1, T2))
    A("SELECT * FROM %s LEFT JOIN %s ON %s.k = %s.k ORDER BY v" % (T1, T2, T1, T2))
    A("SELECT * FROM %s, %s ORDER BY id, k, v" % (T1, T2))
    # ---- error precedence ----
    A("SELECT %s.s FROM %s JOIN %s ON %s.zz = %s.k ORDER BY id" % (T1, T1, T2, T1, T2))
    A("SELECT k FROM %s JOIN %s ON %s.zz = %s.k" % (T1, T2, T1, T2))
    A("SELECT k FROM %s JOIN %s ON %s.k = %s.k" % (T1, T2, T1, T2))
    A("SELECT %s.s FROM %s JOIN %s ON %s.zz = %s.k WHERE k = 1" % (T1, T1, T2, T1, T2))
    A("SELECT %s.s FROM %s, %s WHERE %s.zz = 1 ORDER BY id" % (T1, T1, T2, T1))
    A("SELECT zz FROM %s, %s ORDER BY id" % (T1, T2))
    A("SELECT zz FROM %s JOIN %s ON %s.k = %s.k ORDER BY id" % (T1, T2, T1, T2))
    A("SELECT %s.s FROM %s, %s ORDER BY id, zz" % (T1, T1, T2))
    A("SELECT %s.s FROM %s, %s ON %s.zz = %s.k" % (T1, T1, T2, T1, T2))
    A("SELECT zz FROM nosuch_%s, %s" % (sfx, T1))
    A("SELECT %s.s FROM %s, %s WHERE zz = 1 GROUP BY zz" % (T1, T1, T2))
    # ---- dup aliases / duplicate tables ----
    A("SELECT COUNT(*) FROM %s a, %s a" % (T1, T2))
    A("SELECT %s.s, %s.v FROM %s a, %s a ORDER BY 1, 2" % (T1, T2, T1, T2))
    A("SELECT a.s FROM %s a, %s a" % (T1, T2))
    A("SELECT k FROM %s a, %s a" % (T1, T2))
    A("SELECT * FROM %s %s, %s" % (T1, T2, T2))
    A("SELECT %s.id FROM %s %s, %s" % (T2, T1, T2, T2))
    A("SELECT * FROM %s a, %s a" % (T1, T1))
    A("SELECT %s.* FROM %s, %s" % (T1, T1, T2))
    # ---- forward ON references ----
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.k = %s.k JOIN %s ON %s.k = %s.k" % (T1, T2, T2, T3, T3, T1, T3))
    A("SELECT * FROM %s LEFT JOIN %s ON %s.k = %s.k JOIN %s ON %s.k = %s.k ORDER BY 1, 2, 3, 4, 5, 6" % (T1, T2, T2, T3, T3, T1, T3))
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.k = %s.k" % (T1, T2, T2, T3))
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.k = %s.k WHERE %s.k = %s.k" % (T1, T2, T2, T3, T1, T3))
    # ---- DML aliases (mutating: keep last) ----
    A("UPDATE %s AS a SET id = 9 WHERE a.k = 10" % T1)
    A("SELECT * FROM %s ORDER BY id, k, s" % T1)
    A("UPDATE %s AS a SET id = 8 WHERE %s.k = 20" % (T1, T1))
    A("SELECT * FROM %s ORDER BY id, k, s" % T1)
    A("DELETE FROM %s AS a WHERE %s.k = 30" % (T1, T1))
    A("SELECT * FROM %s ORDER BY id, k, s" % T1)
    A("UPDATE %s AS a SET a.id = 7 WHERE a.k = 20" % T1)
    A("DELETE FROM %s AS a ORDER BY a.k LIMIT 1" % T1)
    A("SELECT * FROM %s ORDER BY id, k, s" % T1)
    A("DELETE FROM %s AS a ORDER BY k LIMIT 1" % T1)
    A("SELECT * FROM %s ORDER BY id, k, s" % T1)
    A("DELETE FROM %s AS a" % T1)
    A("SELECT COUNT(*) FROM %s" % T1)

    c, r = Conn(CAND), Conn(REF)
    for st in setup:
        a, b = c.ask(st), r.ask(st)
        if a != b:
            print("SETUP DIFF %s: %s | %s" % (st, a, b))
    same = diff = 0
    for st in P:
        a, b = c.ask(st), r.ask(st)
        if a == b:
            same += 1
            print("   %s\n     -> %s" % (st, a))
        else:
            diff += 1
            print("** %s\n     cand=%s\n     ref =%s" % (st, a, b))
    print("same=%d diff=%d" % (same, diff))


if __name__ == "__main__":
    main()
