#!/usr/bin/env python3
"""Ticket 9 probes, round 3: UPDATE gating rules, error ordering,
DEFAULT storage type, CHECK edge cases."""
import socket

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
        print("  %-76s => %s" % (s, c.ask(s)))


def main():
    c = Conn(REF)
    S = "r9_"

    run("U1. UPDATE gating: SET touches own fk col (own fk broken)", [
        f"CREATE TABLE {S}ox (v INTEGER)",
        f"CREATE TABLE {S}oc (id INTEGER PRIMARY KEY, pv INTEGER REFERENCES {S}ox(v))",
        f"INSERT INTO {S}oc VALUES (1, 5)",
        f"UPDATE {S}oc SET pv = 1",
        f"UPDATE {S}oc SET pv = 1 WHERE id = 1",
        f"UPDATE {S}oc SET id = 9",
        f"UPDATE {S}oc SET id = 9 WHERE pv = 5",
        f"DELETE FROM {S}oc",
        f"SELECT * FROM {S}oc",
    ], c)

    run("U2. UPDATE gating: distinguish both-gated vs direction-specific", [
        f"CREATE TABLE {S}up (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}uc (id INTEGER PRIMARY KEY, own INTEGER REFERENCES {S}up(id), ref INTEGER)",
        f"CREATE TABLE {S}ug (x INTEGER REFERENCES {S}uc(ref))",
        f"INSERT INTO {S}up VALUES (1)",
        f"INSERT INTO {S}uc (id, own, ref) VALUES (1, 1, 7)",
        f"UPDATE {S}uc SET own = 1",
        f"UPDATE {S}uc SET ref = 8",
        f"UPDATE {S}uc SET id = 5",
        f"UPDATE {S}uc SET own = 1 WHERE ref = 7",
        f"UPDATE {S}uc SET id = 5 WHERE ref = 7",
    ], c)

    run("U3. UPDATE WHERE touches referenced col but SET does not", [
        f"CREATE TABLE {S}wp (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}wc (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}wp(id))",
        f"CREATE TABLE {S}wg (id INTEGER PRIMARY KEY, cp INTEGER REFERENCES {S}wc(pid))",
        f"INSERT INTO {S}wp VALUES (1)",
        f"INSERT INTO {S}wc (id, pid) VALUES (1, 1)",
        f"INSERT INTO {S}wg (id, cp) VALUES (1, 1)",
        f"UPDATE {S}wc SET id = 9 WHERE pid = 1",
        f"UPDATE {S}wc SET pid = 1 WHERE id = 1",
    ], c)

    run("U4. DELETE on empty parent with incoming broken fk", [
        f"CREATE TABLE {S}dp (v INTEGER)",
        f"CREATE TABLE {S}dc (id INTEGER PRIMARY KEY, pv INTEGER REFERENCES {S}dp(v))",
        f"DELETE FROM {S}dp",
        f"UPDATE {S}dp SET v = 5",
        f"INSERT INTO {S}dp VALUES (5)",
    ], c)

    run("E1. error ordering: type vs constraint, row-by-row", [
        f"CREATE TABLE {S}t1 (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER NOT NULL)",
        f"INSERT INTO {S}t1 VALUES (1, 'a', 1)",
        f"INSERT INTO {S}t1 VALUES (2, 'a', 2), (3, 'b', 'txt')",
        f"INSERT INTO {S}t1 VALUES (2, 'b', 'txt'), (3, 'a', 3)",
        f"INSERT INTO {S}t1 VALUES (2, 'b', 'txt'), (4, 'a', 4)",
        f"SELECT * FROM {S}t1 ORDER BY id",
        f"UPDATE {S}t1 SET u = 'a', n = 'txt' WHERE id = 1",
        f"UPDATE {S}t1 SET u = 'zz', n = 'txt' WHERE id = 1",
        f"UPDATE {S}t1 SET u = 'a', n = NULL WHERE id = 1",
        f"UPDATE {S}t1 SET n = 'txt', u = 'a' WHERE id = 1",
        f"SELECT * FROM {S}t1 ORDER BY id",
    ], c)

    run("E2. multi-row update: constraint in row1 vs type in row2", [
        f"CREATE TABLE {S}t2 (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER NOT NULL)",
        f"INSERT INTO {S}t2 VALUES (1, 'a', 1), (2, 'b', 2)",
        f"UPDATE {S}t2 SET u = 'b', n = CASE WHEN id = 1 THEN 5 ELSE 'txt' END" if False else f"UPDATE {S}t2 SET n = CASE WHEN 1 THEN 1 END",
        f"UPDATE {S}t2 SET u = 'b' WHERE id = 1, n = 'txt'" if False else f"UPDATE {S}t2 SET u = 'b' WHERE id = 1",
        f"SELECT * FROM {S}t2 ORDER BY id",
    ], c)

    run("D1. TEXT DEFAULT storage type (ORDER BY distinguishes)", [
        f"CREATE TABLE {S}y1 (b INTEGER, a TEXT DEFAULT 5)",
        f"INSERT INTO {S}y1 (b, a) VALUES (2, '0')",
        f"INSERT INTO {S}y1 (b) VALUES (3)",
        f"SELECT a, b FROM {S}y1 ORDER BY a, b",
        f"SELECT a || 'x' FROM {S}y1 WHERE b = 3",
        f"SELECT length(a) FROM {S}y1 WHERE b = 3",
        f"SELECT upper(a) FROM {S}y1 WHERE b = 3",
        f"CREATE TABLE {S}y2 (b INTEGER, a INTEGER DEFAULT '5')",
        f"INSERT INTO {S}y2 (b, a) VALUES (2, 9)",
        f"INSERT INTO {S}y2 (b) VALUES (3)",
        f"SELECT a, b FROM {S}y2 ORDER BY a, b",
    ], c)

    run("C1. CHECK edge cases", [
        f"CREATE TABLE {S}k1 (a INTEGER, CHECK (zz > 0))",
        f"CREATE TABLE {S}k2 (a INTEGER CHECK (a > 0), b INTEGER)",
        f"INSERT INTO {S}k2 VALUES (1, 2)",
        f"INSERT INTO {S}k2 VALUES (-1, 2)",
        f"SELECT * FROM {S}k2",
        f"CREATE TABLE {S}k3 (a INTEGER, CHECK (a > 0 AND length('x') = 1))",
        f"INSERT INTO {S}k3 VALUES (1)",
        f"CREATE TABLE {S}k4 (a INTEGER, CHECK (a IN (1, 2)))",
        f"INSERT INTO {S}k4 VALUES (3)",
    ], c)

    run("I1. DEFAULT VALUES with column list / autoinc+notnull", [
        f"CREATE TABLE {S}z1 (a INTEGER DEFAULT 1, b TEXT DEFAULT 'q')",
        f"INSERT INTO {S}z1 (a) DEFAULT VALUES",
        f"INSERT INTO {S}z1 DEFAULT VALUES",
        f"SELECT * FROM {S}z1",
        f"CREATE TABLE {S}z2 (a INTEGER PRIMARY KEY NOT NULL, s TEXT)",
        f"INSERT INTO {S}z2 (s) VALUES ('x')",
        f"INSERT INTO {S}z2 VALUES (NULL, 'y')",
        f"SELECT * FROM {S}z2",
        f"CREATE TABLE {S}z3 (id INTEGER PRIMARY KEY AUTOINCREMENT, s TEXT)",
        f"INSERT INTO {S}z3 (s) VALUES ('x')",
        f"UPDATE {S}z3 SET id = NULL",
        f"SELECT * FROM {S}z3",
        f"INSERT INTO {S}z3 (id, s) VALUES (NULL, 'y')",
        f"SELECT * FROM {S}z3",
    ], c)

    run("I2. insert with defaults + notnull + fk in one row, order", [
        f"CREATE TABLE {S}f1p (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}f1c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}f1p(id) NOT NULL, n INTEGER NOT NULL DEFAULT 5)",
        f"INSERT INTO {S}f1c (id, n) VALUES (1, NULL)",
        f"INSERT INTO {S}f1c (id) VALUES (1)",
        f"INSERT INTO {S}f1c (id, pid) VALUES (1, 9)",
        f"SELECT * FROM {S}f1c",
        f"INSERT INTO {S}f1p VALUES (1)",
        f"INSERT INTO {S}f1c (id, pid) VALUES (1, 1)",
        f"SELECT * FROM {S}f1c",
        f"DELETE FROM {S}f1p",
    ], c)

    run("I3. multi-col unique with one NULL + update combos", [
        f"CREATE TABLE {S}m1 (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, UNIQUE (a, b))",
        f"INSERT INTO {S}m1 VALUES (1, 1, 1)",
        f"INSERT INTO {S}m1 VALUES (2, 1, NULL)",
        f"INSERT INTO {S}m1 VALUES (3, NULL, 1)",
        f"INSERT INTO {S}m1 VALUES (4, 1, 1)",
        f"UPDATE {S}m1 SET b = 1 WHERE id = 2",
        f"UPDATE {S}m1 SET a = 1 WHERE id = 3",
        f"SELECT * FROM {S}m1 ORDER BY id",
        f"UPDATE {S}m1 SET a = NULL, b = NULL WHERE id = 1",
        f"SELECT * FROM {S}m1 ORDER BY id",
    ], c)


if __name__ == "__main__":
    main()
