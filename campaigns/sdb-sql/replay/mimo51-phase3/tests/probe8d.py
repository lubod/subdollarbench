#!/usr/bin/env python3
"""Round-3 probes: star over duplicate tables, forward-ON variants, DML
alias/qualification surface. Prints cand vs ref frames."""
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
            return "ROWS[%s] n=%d %s" % (hdr, n, rows[:6])
        return first.rstrip("\n")


def main():
    sfx = "pd_" + os.urandom(3).hex() + "_"
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
    # star over duplicate table / qualifier collisions
    A("SELECT * FROM %s, %s ORDER BY id" % (T1, T1))
    A("SELECT * FROM %s, %s" % (T1, T1))
    A("SELECT %s.* FROM %s, %s" % (T1, T1, T2))
    A("SELECT %s.s FROM %s, %s" % (T1, T1, T2))
    A("SELECT %s.id FROM %s, %s ORDER BY 1" % (T1, T1, T2))
    A("SELECT a.s, a.k FROM %s a, %s a ORDER BY 1, 2" % (T1, T2))
    A("SELECT a.k FROM %s a, %s a" % (T1, T2))
    A("SELECT a.v FROM %s a, %s a ORDER BY 1" % (T1, T2))
    A("SELECT %s.* FROM %s a, %s a" % (T1, T1, T2))
    # forward ON variants
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.k = %s.k LEFT JOIN %s ON %s.k = %s.k" % (T1, T2, T2, T3, T3, T1, T3))
    A("SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k JOIN %s ON %s.k = %s.k" % (T1, T2, T2, T3, T3, T1, T3))
    A("SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k, %s" % (T1, T2, T2, T3, T3))
    A("SELECT COUNT(*) FROM %s, %s ON %s.k = %s.k, %s" % (T1, T2, T2, T3, T3))
    A("SELECT COUNT(*) FROM %s, %s JOIN %s ON %s.k = %s.k" % (T1, T2, T3, T1, T3))
    A("SELECT COUNT(*) FROM %s JOIN %s ON %s.k = %s.k AND %s.k = %s.k JOIN %s ON %s.k = %s.k" % (T1, T2, T1, T2, T2, T3, T3, T1, T3))
    A("SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k LEFT JOIN %s ON %s.k = %s.k" % (T1, T2, T2, T3, T3, T1, T3))
    # DML alias + qualification (mutating; grouped at end)
    A("DELETE FROM %s ORDER BY %s.k LIMIT 1" % (T1, T1))
    A("DELETE FROM %s AS a ORDER BY %s.k LIMIT 1" % (T1, T1))
    A("DELETE FROM %s ORDER BY zz.k LIMIT 1" % T1)
    A("DELETE FROM %s ORDER BY k + 1 LIMIT 1" % T1)
    A("UPDATE %s AS a SET id = %s.k WHERE a.k = 20" % (T1, T1))
    A("UPDATE %s AS a SET id = a.k WHERE a.k = 10" % T1)
    A("UPDATE %s AS a SET id = 1 WHERE a.zz = 1" % T1)
    A("UPDATE %s AS a SET id = 1 WHERE zz = 1" % T1)
    A("DELETE FROM %s AS a WHERE zz.k = 1" % T1)
    A("UPDATE %s SET id = 3 WHERE %s.k = 30" % (T1, T1))
    A("SELECT * FROM %s ORDER BY id, k, s" % T1)
    A("UPDATE %s AS a SET id = a.k ORDER BY a.k LIMIT 2" % T1)
    A("SELECT * FROM %s ORDER BY id, k, s" % T1)
    A("DELETE FROM %s AS a WHERE a.id = 11 ORDER BY a.k LIMIT 1" % T1)
    A("SELECT * FROM %s ORDER BY id, k, s" % T1)

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
