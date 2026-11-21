#!/usr/bin/env python3
"""Ticket 9 probes: what DDL the reference accepts and how constraints
behave. Prints ref answers only (candidate compared later)."""
import socket
import sys

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
            return "ROWS[%s] n=%d %s" % (hdr, n, rows[:8])
        return first.rstrip("\n")


def run(title, stmts, c=None):
    print("=== %s" % title)
    for s in stmts:
        print("  %-74s => %s" % (s, c.ask(s)))


def main():
    c = Conn(REF)
    S = "p9_"

    run("A. DDL acceptance", [
        f"CREATE TABLE {S}a (id INTEGER PRIMARY KEY, s TEXT UNIQUE, n INTEGER NOT NULL, v INTEGER DEFAULT 3, d TEXT DEFAULT 'x', p INTEGER REFERENCES {S}p(id))",
        f"CREATE TABLE {S}p (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}b (a INTEGER, b TEXT, PRIMARY KEY (a))",
        f"CREATE TABLE {S}c (a INTEGER, UNIQUE (a))",
        f"CREATE TABLE {S}d (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        f"CREATE TABLE {S}e (a INTEGER, FOREIGN KEY (a) REFERENCES {S}p(id))",
        f"CREATE TABLE {S}f (a INTEGER NOT NULL DEFAULT 5 PRIMARY KEY)",
        f"CREATE TABLE {S}g (a INTEGER DEFAULT NULL)",
        f"CREATE TABLE {S}h (a INTEGER DEFAULT -5)",
        f"CREATE TABLE {S}i (a INTEGER DEFAULT +5)",
        f"CREATE TABLE {S}j (a INTEGER REFERENCES {S}p)",
        f"CREATE TABLE {S}k (a INTEGER CONSTRAINT uk UNIQUE)",
        f"CREATE TABLE {S}l (a INTEGER, CONSTRAINT pk PRIMARY KEY (a))",
        f"CREATE TABLE {S}m (a INTEGER PRIMARY KEY AUTOINCREMENT)",
        f"CREATE TABLE {S}n (a INTEGER DEFAULT 'zz')",
        f"CREATE TABLE {S}o (a INTEGER REFERENCES {S}nosuch(id))",
        f"CREATE TABLE {S}q (a INTEGER REFERENCES {S}p(nocol))",
        f"CREATE TABLE {S}r (a INTEGER, PRIMARY KEY (a, b))",
        f"CREATE TABLE {S}s (a INTEGER, CHECK (a > 0))",
        f"CREATE TABLE {S}t (a INTEGER NOT NULL DEFAULT 'zz')",
        f"CREATE TABLE {S}u (a INTEGER REFERENCES {S}p(id) ON DELETE CASCADE)",
        f"CREATE TABLE {S}v (a TEXT PRIMARY KEY, b INTEGER)",
        f"CREATE TABLE {S}w (a INTEGER PRIMARY KEY, b INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}x (a INTEGER, a INTEGER)",
        f"CREATE TABLE {S}y (a INTEGER, PRIMARY KEY ())",
    ], c)

    run("B. INTEGER PK auto-assign", [
        f"CREATE TABLE {S}b1 (id INTEGER PRIMARY KEY, s TEXT)",
        f"INSERT INTO {S}b1 (s) VALUES ('a')",
        f"INSERT INTO {S}b1 (id, s) VALUES (NULL, 'b')",
        f"INSERT INTO {S}b1 VALUES (NULL, 'c')",
        f"INSERT INTO {S}b1 (id, s) VALUES (10, 'd')",
        f"INSERT INTO {S}b1 (s) VALUES ('e')",
        f"SELECT id, s FROM {S}b1 ORDER BY id",
        f"INSERT INTO {S}b1 (id, s) VALUES (10, 'dup')",
        f"INSERT INTO {S}b1 (id, s) VALUES (10, 'dup'), (11, 'x')",
        f"INSERT INTO {S}b1 (id, s) VALUES (NULL, 'f'), (NULL, 'g')",
        f"SELECT id, s FROM {S}b1 ORDER BY id",
        f"DELETE FROM {S}b1 WHERE id = 12",
        f"INSERT INTO {S}b1 (s) VALUES ('h')",
        f"SELECT id, s FROM {S}b1 ORDER BY id",
    ], c)

    run("B2. INTEGER PK with DEFAULT", [
        f"CREATE TABLE {S}b2 (id INTEGER PRIMARY KEY DEFAULT 42, s TEXT)",
        f"INSERT INTO {S}b2 (s) VALUES ('a')",
        f"INSERT INTO {S}b2 (id, s) VALUES (NULL, 'b')",
        f"INSERT INTO {S}b2 (id, s) VALUES (50, 'c')",
        f"SELECT id, s FROM {S}b2 ORDER BY id",
    ], c)

    run("B3. TEXT PK", [
        f"CREATE TABLE {S}b3 (k TEXT PRIMARY KEY, v INTEGER)",
        f"INSERT INTO {S}b3 VALUES ('a', 1)",
        f"INSERT INTO {S}b3 VALUES (NULL, 2)",
        f"INSERT INTO {S}b3 (k, v) VALUES (NULL, 3)",
        f"INSERT INTO {S}b3 (v) VALUES (4)",
        f"INSERT INTO {S}b3 VALUES ('a', 5)",
        f"SELECT * FROM {S}b3",
    ], c)

    run("B4. INTEGER PK explicit NULL in UPDATE", [
        f"CREATE TABLE {S}b4 (id INTEGER PRIMARY KEY, s TEXT)",
        f"INSERT INTO {S}b4 VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        f"UPDATE {S}b4 SET id = NULL WHERE id = 2",
        f"SELECT id, s FROM {S}b4 ORDER BY id",
        f"UPDATE {S}b4 SET id = NULL",
        f"SELECT id, s FROM {S}b4 ORDER BY id",
        f"UPDATE {S}b4 SET id = 3 WHERE id = 1",
        f"SELECT id, s FROM {S}b4 ORDER BY id",
    ], c)

    run("C. UNIQUE / NOT NULL", [
        f"CREATE TABLE {S}c1 (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER NOT NULL)",
        f"INSERT INTO {S}c1 VALUES (1, 'x', 5)",
        f"INSERT INTO {S}c1 VALUES (2, 'x', 6)",
        f"INSERT INTO {S}c1 VALUES (3, NULL, 7)",
        f"INSERT INTO {S}c1 VALUES (4, NULL, 8)",
        f"INSERT INTO {S}c1 VALUES (5, 'y', NULL)",
        f"INSERT INTO {S}c1 VALUES (6, 'y', 9), (7, 'z', 10)",
        f"SELECT * FROM {S}c1 ORDER BY id",
        f"UPDATE {S}c1 SET u = 'x' WHERE id = 4",
        f"UPDATE {S}c1 SET n = NULL WHERE id = 3",
        f"UPDATE {S}c1 SET n = NULL",
        f"UPDATE {S}c1 SET u = NULL WHERE id = 1",
        f"UPDATE {S}c1 SET n = 11, u = 'q' WHERE id = 3",
        f"SELECT * FROM {S}c1 ORDER BY id",
    ], c)

    run("C2. unique duplicate within one UPDATE", [
        f"CREATE TABLE {S}c2 (id INTEGER PRIMARY KEY, u TEXT UNIQUE)",
        f"INSERT INTO {S}c2 VALUES (1, 'a'), (2, 'b')",
        f"UPDATE {S}c2 SET u = 'b' WHERE id = 1",
        f"UPDATE {S}c2 SET u = 'c'",
        f"SELECT * FROM {S}c2 ORDER BY id",
        f"UPDATE {S}c2 SET id = 2 WHERE id = 1",
        f"SELECT * FROM {S}c2 ORDER BY id",
    ], c)

    run("D. DEFAULT behavior", [
        f"CREATE TABLE {S}d1 (a INTEGER, b INTEGER DEFAULT 7, c TEXT DEFAULT 'hi', d INTEGER DEFAULT NULL, e TEXT DEFAULT 'zz')",
        f"INSERT INTO {S}d1 (a) VALUES (1)",
        f"INSERT INTO {S}d1 (a, b) VALUES (2, NULL)",
        f"INSERT INTO {S}d1 (a, b, c, d, e) VALUES (3, 9, 'q', 4, 'w')",
        f"INSERT INTO {S}d1 VALUES (4, 1, 'x', 2, 'y')",
        f"INSERT INTO {S}d1 (a, d) VALUES (5, NULL)",
        f"SELECT * FROM {S}d1 ORDER BY a",
        f"INSERT INTO {S}d1 (a, b) VALUES (6, 'notint')",
    ], c)

    run("D2. DEFAULT into NOT NULL col + explicit NULL", [
        f"CREATE TABLE {S}d2 (a INTEGER, n INTEGER NOT NULL DEFAULT 5)",
        f"INSERT INTO {S}d2 (a) VALUES (1)",
        f"INSERT INTO {S}d2 (a, n) VALUES (2, NULL)",
        f"SELECT * FROM {S}d2 ORDER BY a",
    ], c)

    run("D3. DEFAULT literal types at CREATE", [
        f"CREATE TABLE {S}d3a (a INTEGER DEFAULT 'zz')",
        f"INSERT INTO {S}d3a (a) VALUES (1)",
        f"SELECT * FROM {S}d3a",
        f"CREATE TABLE {S}d3b (a INTEGER DEFAULT 1.5)",
        f"CREATE TABLE {S}d3c (a TEXT DEFAULT 5)",
        f"CREATE TABLE {S}d3d (b INTEGER, a TEXT DEFAULT 5)",
        f"INSERT INTO {S}d3d (b) VALUES (1)",
        f"SELECT * FROM {S}d3d",
        f"CREATE TABLE {S}d3e (b INTEGER, a TEXT DEFAULT NULL)",
        f"INSERT INTO {S}d3e (b) VALUES (2)",
        f"SELECT * FROM {S}d3e",
    ], c)

    run("E. Foreign keys", [
        f"CREATE TABLE {S}e1p (id INTEGER PRIMARY KEY, v INTEGER)",
        f"CREATE TABLE {S}e1c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}e1p(id), s TEXT)",
        f"INSERT INTO {S}e1p VALUES (1, 10), (2, 20)",
        f"INSERT INTO {S}e1c VALUES (1, 1, 'a')",
        f"INSERT INTO {S}e1c VALUES (2, 99, 'b')",
        f"INSERT INTO {S}e1c VALUES (3, NULL, 'c')",
        f"INSERT INTO {S}e1c VALUES (4, 2, 'd'), (5, 99, 'e')",
        f"SELECT * FROM {S}e1c ORDER BY id",
        f"DELETE FROM {S}e1p WHERE id = 1",
        f"DELETE FROM {S}e1p WHERE id = 2",
        f"UPDATE {S}e1p SET id = 5 WHERE id = 1",
        f"UPDATE {S}e1p SET id = 3 WHERE id = 2",
        f"SELECT * FROM {S}e1p ORDER BY id",
        f"DELETE FROM {S}e1p",
        f"SELECT * FROM {S}e1p",
    ], c)

    run("E2. FK update child / missing parent table / no such parent col", [
        f"CREATE TABLE {S}e2p (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}e2c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}e2p(id))",
        f"INSERT INTO {S}e2p VALUES (1), (2)",
        f"INSERT INTO {S}e2c VALUES (1, 1)",
        f"UPDATE {S}e2c SET pid = 2 WHERE id = 1",
        f"UPDATE {S}e2c SET pid = 7 WHERE id = 1",
        f"UPDATE {S}e2c SET pid = NULL WHERE id = 1",
        f"SELECT * FROM {S}e2c",
        f"CREATE TABLE {S}e2x (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}e2nosuch(id))",
        f"INSERT INTO {S}e2x VALUES (1, 5)",
        f"CREATE TABLE {S}e2y (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}e2p(nocol))",
        f"INSERT INTO {S}e2y VALUES (1, 5)",
        f"CREATE TABLE {S}e2z (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}e2p)",
        f"INSERT INTO {S}e2z VALUES (1, 5)",
        f"INSERT INTO {S}e2z VALUES (2, 1)",
        f"INSERT INTO {S}e2z VALUES (3, 9)",
    ], c)

    run("E3. parent not unique (plain column referenced)", [
        f"CREATE TABLE {S}e3p (v INTEGER)",
        f"CREATE TABLE {S}e3c (id INTEGER PRIMARY KEY, pv INTEGER REFERENCES {S}e3p(v))",
        f"INSERT INTO {S}e3p VALUES (7), (7)",
        f"INSERT INTO {S}e3c VALUES (1, 7)",
        f"DELETE FROM {S}e3p WHERE v = 7",
        f"DELETE FROM {S}e3p",
        f"SELECT * FROM {S}e3c",
        f"INSERT INTO {S}e3c VALUES (2, 7)",
    ], c)

    run("E4. self reference", [
        f"CREATE TABLE {S}e4 (id INTEGER PRIMARY KEY, mgr INTEGER REFERENCES {S}e4(id))",
        f"INSERT INTO {S}e4 (mgr) VALUES (NULL)",
        f"INSERT INTO {S}e4 (mgr) VALUES (1)",
        f"INSERT INTO {S}e4 (mgr) VALUES (9)",
        f"INSERT INTO {S}e4 (id, mgr) VALUES (9, 1)",
        f"INSERT INTO {S}e4 (mgr) VALUES (9)",
        f"SELECT * FROM {S}e4 ORDER BY id",
        f"UPDATE {S}e4 SET id = 20 WHERE id = 1",
        f"SELECT * FROM {S}e4 ORDER BY id",
        f"DELETE FROM {S}e4 WHERE id = 20",
        f"UPDATE {S}e4 SET id = NULL WHERE id = 3",
        f"SELECT * FROM {S}e4 ORDER BY id",
    ], c)

    run("F. error classes: type vs constraint ordering", [
        f"CREATE TABLE {S}f1 (id INTEGER PRIMARY KEY, n INTEGER NOT NULL, s TEXT UNIQUE)",
        f"INSERT INTO {S}f1 (id, n, s) VALUES ('zz', NULL, 'dup')",
        f"INSERT INTO {S}f1 (id, n, s) VALUES (1, 'zz', 'dup')",
        f"INSERT INTO {S}f1 (id, n, s) VALUES (1, NULL, 'x')",
        f"INSERT INTO {S}f1 (id, n, s) VALUES ('zz', 5, 'y')",
        f"CREATE TABLE {S}f2 (a INTEGER, b INTEGER)",
        f"INSERT INTO {S}f2 VALUES ('zz', 1)",
        f"CREATE TABLE {S}f3 (a INTEGER PRIMARY KEY, b INTEGER NOT NULL)",
        f"INSERT INTO {S}f3 (a, b) VALUES (1, 'zz')",
    ], c)

    run("G. atomicity of failing multi-row insert + txn", [
        f"CREATE TABLE {S}g1 (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER NOT NULL)",
        f"INSERT INTO {S}g1 VALUES (1, 'a', 1)",
        f"INSERT INTO {S}g1 VALUES (2, 'b', 2), (3, 'a', 3)",
        f"SELECT * FROM {S}g1 ORDER BY id",
        "BEGIN",
        f"INSERT INTO {S}g1 VALUES (4, 'c', 4)",
        f"INSERT INTO {S}g1 VALUES (5, 'a', 5)",
        f"SELECT * FROM {S}g1 ORDER BY id",
        "COMMIT",
        f"SELECT * FROM {S}g1 ORDER BY id",
    ], c)

    run("H. table-level PK rowid alias?", [
        f"CREATE TABLE {S}h1 (a INTEGER, b TEXT, PRIMARY KEY (a))",
        f"INSERT INTO {S}h1 (b) VALUES ('x')",
        f"INSERT INTO {S}h1 (b) VALUES ('y')",
        f"INSERT INTO {S}h1 (a, b) VALUES (5, 'z')",
        f"INSERT INTO {S}h1 (a, b) VALUES (NULL, 'n')",
        f"SELECT * FROM {S}h1 ORDER BY a",
        f"INSERT INTO {S}h1 (a, b) VALUES (5, 'dup')",
        f"CREATE TABLE {S}h2 (a TEXT, PRIMARY KEY (a))",
        f"INSERT INTO {S}h2 VALUES ('k')",
        f"INSERT INTO {S}h2 VALUES (NULL)",
        f"CREATE TABLE {S}h3 (a INTEGER, b TEXT, PRIMARY KEY (a, b))",
        f"INSERT INTO {S}h3 (b) VALUES ('q')",
        f"INSERT INTO {S}h3 VALUES (1, 'q')",
        f"INSERT INTO {S}h3 VALUES (1, 'q')",
        f"SELECT * FROM {S}h3",
        f"CREATE TABLE {S}h4 (a INTEGER, b INTEGER, UNIQUE (a, b))",
        f"INSERT INTO {S}h4 VALUES (1, 1)",
        f"INSERT INTO {S}h4 VALUES (1, 1)",
        f"INSERT INTO {S}h4 VALUES (1, 2)",
        f"INSERT INTO {S}h4 VALUES (NULL, 1)",
        f"INSERT INTO {S}h4 VALUES (NULL, 1)",
        f"SELECT * FROM {S}h4",
    ], c)

    run("I. INSERT arity/columns with defaults + duplicate col", [
        f"CREATE TABLE {S}i1 (a INTEGER, b INTEGER DEFAULT 3, c INTEGER DEFAULT 9)",
        f"INSERT INTO {S}i1 (a, a) VALUES (1, 2)",
        f"SELECT * FROM {S}i1",
        f"INSERT INTO {S}i1 (b, a) VALUES (5, 1)",
        f"SELECT * FROM {S}i1 ORDER BY a",
        f"INSERT INTO {S}i1 (zz) VALUES (1)",
    ], c)

    run("J. PK + FK combined / update parent restrict detail", [
        f"CREATE TABLE {S}j1p (k INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}j1c (id INTEGER PRIMARY KEY, pk INTEGER NOT NULL REFERENCES {S}j1p(k))",
        f"INSERT INTO {S}j1p VALUES (1), (2)",
        f"INSERT INTO {S}j1c VALUES (1, 1)",
        f"UPDATE {S}j1p SET k = 1 WHERE k = 2",
        f"SELECT * FROM {S}j1p ORDER BY k",
        f"UPDATE {S}j1c SET pk = 2",
        f"UPDATE {S}j1p SET k = 3 WHERE k = 2",
        f"SELECT * FROM {S}j1c",
        f"UPDATE {S}j1c SET pk = 3",
        f"UPDATE {S}j1p SET k = 4 WHERE k = 3",
        f"SELECT * FROM {S}j1p ORDER BY k",
        f"SELECT * FROM {S}j1c",
        f"DELETE FROM {S}j1p",
    ], c)

    run("K. TEXT PK dup + update", [
        f"CREATE TABLE {S}k1 (k TEXT PRIMARY KEY, v INTEGER)",
        f"INSERT INTO {S}k1 VALUES ('a', 1)",
        f"INSERT INTO {S}k1 VALUES ('b', 2)",
        f"UPDATE {S}k1 SET k = 'b' WHERE k = 'a'",
        f"UPDATE {S}k1 SET k = NULL WHERE k = 'a'",
        f"UPDATE {S}k1 SET k = 'c' WHERE k = 'a'",
        f"SELECT * FROM {S}k1 ORDER BY k",
    ], c)

    run("L. AUTOINCREMENT + odd DDL", [
        f"CREATE TABLE {S}l1 (id INTEGER PRIMARY KEY AUTOINCREMENT, s TEXT)",
        f"INSERT INTO {S}l1 (s) VALUES ('a')",
        f"INSERT INTO {S}l1 VALUES (5, 'b')",
        f"INSERT INTO {S}l1 (s) VALUES ('c')",
        f"DELETE FROM {S}l1",
        f"INSERT INTO {S}l1 (s) VALUES ('d')",
        f"SELECT * FROM {S}l1",
        f"CREATE TABLE {S}l2 (a INTEGER PRIMARY KEY, b TEXT)",
        f"INSERT INTO {S}l2 VALUES (9223372036854775807, 'max')",
        f"INSERT INTO {S}l2 (b) VALUES ('over')",
        f"SELECT * FROM {S}l2",
    ], c)

    run("M. NOT NULL on UPDATE whole table / zero rows", [
        f"CREATE TABLE {S}m1 (a INTEGER NOT NULL, b TEXT)",
        f"INSERT INTO {S}m1 VALUES (1, 'x')",
        f"UPDATE {S}m1 SET b = NULL WHERE a = 99",
        f"UPDATE {S}m1 SET a = NULL WHERE a = 99",
        f"UPDATE {S}m1 SET a = NULL",
        f"SELECT * FROM {S}m1",
        f"DELETE FROM {S}m1",
        f"SELECT COUNT(*) FROM {S}m1",
    ], c)


if __name__ == "__main__":
    main()
