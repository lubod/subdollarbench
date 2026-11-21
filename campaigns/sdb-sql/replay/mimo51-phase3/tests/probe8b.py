#!/usr/bin/env python3
"""Targeted probes: print candidate and reference frames side by side."""
import socket, sys

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
    import os
    sfx = "pb7_" + os.urandom(3).hex() + "_"
    setup = [
        "CREATE TABLE %s1 (id INTEGER, k INTEGER, s TEXT)" % sfx,
        "INSERT INTO %s1 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, NULL)" % sfx,
        "CREATE TABLE %s2 (k INTEGER, v INTEGER, id INTEGER)" % sfx,
        "INSERT INTO %s2 VALUES (10, 100, 1), (20, 200, 2), (50, NULL, 5)" % sfx,
        "CREATE TABLE %s3 (k INTEGER, w TEXT)" % sfx,
        "INSERT INTO %s3 VALUES (10, 'p'), (20, 'q')" % sfx,
    ]
    T1, T2, T3 = sfx + "1", sfx + "2", sfx + "3"
    probes = [
        # ORDER BY bare name: output columns first?
        "SELECT * FROM %s, %s ORDER BY id, k, v" % (T1, T2),
        "SELECT %s.v, %s.id FROM %s, %s WHERE %s.k = %s.k ORDER BY id" % (T2, T2, T1, T2, T1, T2),
        "SELECT %s.s FROM %s, %s WHERE %s.k = %s.k ORDER BY id" % (T1, T1, T2, T1, T2),
        "SELECT %s.s FROM %s, %s WHERE %s.k = %s.k ORDER BY k" % (T1, T1, T2, T1, T2),
        "SELECT %s.s FROM %s, %s WHERE %s.k = %s.k ORDER BY v" % (T1, T1, T2, T1, T2),
        "SELECT %s.s, %s.v FROM %s LEFT JOIN %s ON %s.k = %s.k ORDER BY v" % (T1, T2, T1, T2, T1, T2),
        # ORDER BY ordinal over join / missing column precedence
        "SELECT %s.id, %s.id FROM %s, %s WHERE %s.k = %s.k ORDER BY 1, 2" % (T1, T2, T1, T2, T1, T2),
        "SELECT %s.id FROM %s WHERE %s.k = 10 ORDER BY 1" % (T1, T1, T1),
        # comma + ON
        "SELECT COUNT(*) FROM %s, %s ON %s.k = %s.k" % (T1, T2, T1, T2),
        "SELECT * FROM %s, %s ON %s.k = %s.k ORDER BY id, k, v" % (T1, T2, T1, T2),
        "SELECT COUNT(*) FROM %s, %s ON %s.k = %s.k, %s ON %s.k = %s.k" % (T1, T2, T1, T2, T3, T2, T3),
        "SELECT COUNT(*) FROM %s JOIN %s USING (k)" % (T1, T2),
        "SELECT COUNT(*) FROM %s, %s USING (k)" % (T1, T2),
        # ON resolution precedence vs other errors
        "SELECT zz FROM %s JOIN %s ON %s.zz = %s.k" % (T1, T2, T1, T2),
        "SELECT %s.zz FROM %s JOIN %s ON %s.k = %s.k" % (T1, T1, T2, T1, T2),
        "SELECT * FROM %s JOIN %s ON %s.zz = %s.k ORDER BY s, v" % (T1, T2, T1, T2),
        "SELECT * FROM %s JOIN %s ON %s.k = %s.k ORDER BY zz" % (T1, T2, T1, T2),
        "SELECT zz FROM %s JOIN %s ON %s.k = %s.k" % (T1, T2, T1, T2),
        "SELECT * FROM %s JOIN %s ON %s.k = %s.k, %s" % (T1, T2, T1, T2, T3),
        # duplicate aliases / self comma
        "SELECT * FROM %s a, %s a" % (T1, T2),
        "SELECT COUNT(*) FROM %s, %s" % (T1, T1),
        "SELECT id FROM %s, %s" % (T1, T1),
        "SELECT %s.id, %s.id FROM %s, %s WHERE %s.k = %s.k ORDER BY 1, 2" % (T1, T1, T1, T1, T1, T1),
        "SELECT COUNT(*) FROM %s a, %s a ON a.k = a.k" % (T1, T2),
        # IS NULL through left join (correctness of cand)
        "SELECT %s.id, %s.v FROM %s LEFT JOIN %s ON %s.k = %s.k WHERE %s.v IS NULL ORDER BY %s.id" % (T1, T2, T1, T2, T1, T2, T2, T1),
        "SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k WHERE %s.v IS NULL" % (T1, T2, T1, T2, T2),
        "SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k WHERE NOT (%s.v > 5)" % (T1, T2, T1, T2, T2),
        "SELECT COUNT(*) FROM %s LEFT JOIN %s ON %s.k = %s.k AND %s.v IS NULL" % (T1, T2, T1, T2, T2),
        "SELECT COUNT(*) FROM %s, %s WHERE %s.s IS NULL" % (T1, T2, T1),
        # DML aliases
        "UPDATE %s AS a SET id = 1" % T1,
        "UPDATE %s a SET id = 9 WHERE a.k = 10" % T1,
        "UPDATE %s a SET a.id = 8 WHERE a.k = 10" % T1,
        "UPDATE %s SET %s.id = 7 WHERE %s.k = 10" % (T1, T1, T1),
        "DELETE FROM %s AS a WHERE a.k = 999" % T1,
        "DELETE FROM %s a WHERE a.k = 999" % T1,
        "DELETE FROM %s WHERE %s.k = 999" % (T1, T1),
        "UPDATE %s, %s SET id = 1" % (T1, T2),
        "DELETE FROM %s, %s" % (T1, T2),
        # qualifier rules in joins
        "SELECT a.id FROM %s AS a, %s b WHERE a.k = b.k ORDER BY a.id" % (T1, T2),
        "SELECT %s.id FROM %s AS a, %s b" % (T1, T1, T2),
        "SELECT b.k FROM %s AS a, %s b ORDER BY b.k" % (T1, T2),
        # unknown qualifier class
        "SELECT zz.k FROM %s, %s" % (T1, T2),
        "SELECT zz.* FROM %s, %s" % (T1, T2),
        "SELECT COUNT(*) FROM %s, %s WHERE zz.k = 1" % (T1, T2),
        # ON referencing a later (not yet joined) table
        "SELECT COUNT(*) FROM %s JOIN %s ON %s.k = %s.k JOIN %s ON %s.k = %s.k" % (T1, T2, T2, T3, T3, T1, T3),
        "SELECT COUNT(*) FROM %s JOIN %s ON %s.k = %s.k JOIN %s ON %s.k = %s.k" % (T1, T2, T2, T3, T3, T3, T1),
        # ORDER BY output-name shadowing: alias equal to a column
        "SELECT %s.id AS v, %s.v FROM %s, %s WHERE %s.k = %s.k ORDER BY v" % (T1, T2, T1, T2, T1, T2),
        "SELECT %s.v AS v FROM %s LEFT JOIN %s ON %s.k = %s.k ORDER BY v" % (T2, T1, T2, T1, T2),
    ]
    c, r = Conn(CAND), Conn(REF)
    for st in setup:
        print("setup %s\n  cand=%s\n  ref =%s" % (st, c.ask(st), r.ask(st)))
    for st in probes:
        a, b = c.ask(st), r.ask(st)
        mark = "  " if a == b else "**"
        print("%s %s\n  cand=%s\n  ref =%s" % (mark, st, a, b))


if __name__ == "__main__":
    main()
