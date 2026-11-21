#!/usr/bin/env python3
"""Ticket 9 probes, round 2: open questions on ordering, AUTOINCREMENT,
DEFAULT coercion, structural FK checks."""
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
    S = "q9_"

    run("P1. AUTOINCREMENT clean", [
        f"CREATE TABLE {S}a1 (id INTEGER PRIMARY KEY AUTOINCREMENT, s TEXT)",
        f"INSERT INTO {S}a1 (s) VALUES ('a')",
        f"INSERT INTO {S}a1 (s) VALUES ('b')",
        f"INSERT INTO {S}a1 VALUES (100, 'c')",
        f"DELETE FROM {S}a1 WHERE id = 100",
        f"INSERT INTO {S}a1 (s) VALUES ('d')",
        f"SELECT id, s FROM {S}a1 ORDER BY id",
        f"DELETE FROM {S}a1",
        f"INSERT INTO {S}a1 (s) VALUES ('e')",
        f"SELECT id, s FROM {S}a1 ORDER BY id",
        f"CREATE TABLE {S}a2 (id INTEGER PRIMARY KEY, s TEXT)",
        f"INSERT INTO {S}a2 (s) VALUES ('a')",
        f"INSERT INTO {S}a2 (s) VALUES ('b')",
        f"INSERT INTO {S}a2 VALUES (100, 'c')",
        f"DELETE FROM {S}a2 WHERE id = 100",
        f"DELETE FROM {S}a2",
        f"INSERT INTO {S}a2 (s) VALUES ('z')",
        f"SELECT id, s FROM {S}a2 ORDER BY id",
    ], c)

    run("P2/P3/P4. DEFAULT coercion", [
        f"CREATE TABLE {S}b1 (b INTEGER, a INTEGER DEFAULT 'zz')",
        f"INSERT INTO {S}b1 (b) VALUES (1)",
        f"SELECT * FROM {S}b1",
        f"CREATE TABLE {S}b2 (b INTEGER, a INTEGER DEFAULT 1.5)",
        f"INSERT INTO {S}b2 (b) VALUES (1)",
        f"SELECT * FROM {S}b2",
        f"CREATE TABLE {S}b3 (b INTEGER, a TEXT DEFAULT 5)",
        f"INSERT INTO {S}b3 (b) VALUES (1)",
        f"SELECT a = 5 FROM {S}b3",
        f"SELECT a = '5' FROM {S}b3",
        f"CREATE TABLE {S}b4 (b INTEGER, a TEXT DEFAULT -7)",
        f"INSERT INTO {S}b4 (b) VALUES (1)",
        f"SELECT a = '-7' FROM {S}b4",
        f"CREATE TABLE {S}b5 (b INTEGER, a INTEGER DEFAULT '5')",
        f"INSERT INTO {S}b5 (b) VALUES (1)",
        f"SELECT a = 5 FROM {S}b5",
        f"CREATE TABLE {S}b6 (b INTEGER, a TEXT DEFAULT NULL, c INTEGER DEFAULT 0)",
        f"INSERT INTO {S}b6 (b) VALUES (1)",
        f"SELECT * FROM {S}b6",
    ], c)

    run("P5. table constraint column existence", [
        f"CREATE TABLE {S}c1 (a INTEGER, UNIQUE (zz))",
        f"CREATE TABLE {S}c2 (a INTEGER, FOREIGN KEY (zz) REFERENCES {S}c1(a))",
        f"CREATE TABLE {S}c3 (a INTEGER, PRIMARY KEY (zz))",
        f"CREATE TABLE {S}c4 (a INTEGER, UNIQUE (a, zz))",
    ], c)

    run("P14/P31. structural vs type order (child insert)", [
        f"CREATE TABLE {S}d1 (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}d1nosuch(id))",
        f"INSERT INTO {S}d1 (id, pid) VALUES ('zz', 5)",
        f"INSERT INTO {S}d1 (id, pid) VALUES (1, 'zz')",
        f"CREATE TABLE {S}d2 (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}d2p(nocol))",
        f"CREATE TABLE {S}d2p (id INTEGER PRIMARY KEY)",
        f"INSERT INTO {S}d2 (id, pid) VALUES ('zz', 5)",
        f"CREATE TABLE {S}d3 (v INTEGER)",
        f"CREATE TABLE {S}d3c (id INTEGER PRIMARY KEY, pv INTEGER REFERENCES {S}d3(v))",
        f"INSERT INTO {S}d3c (id, pv) VALUES ('zz', 5)",
    ], c)

    run("P15/P17/P18. structural vs other error order (parent side)", [
        f"CREATE TABLE {S}e1 (v INTEGER)",
        f"CREATE TABLE {S}e1c (id INTEGER PRIMARY KEY, pv INTEGER REFERENCES {S}e1(v))",
        f"UPDATE {S}e1 SET v = 9 WHERE v = 99",
        f"DELETE FROM {S}e1 WHERE v = 99",
        f"INSERT INTO {S}e1 VALUES (9) WHERE 1" if False else f"INSERT INTO {S}e1 VALUES (9)",
        f"UPDATE {S}e1 SET zz = 1",
        f"INSERT INTO {S}e1c (id) VALUES (1, 2)",
        f"INSERT INTO {S}e1c (zz) VALUES (1)",
        f"SELECT * FROM {S}e1",
        f"SELECT * FROM {S}e1c",
        f"DELETE FROM {S}e1c",
        f"UPDATE {S}e1c SET id = 5",
        f"DELETE FROM {S}e1c WHERE zz = 1",
    ], c)

    run("P19. does child write validate incoming (grandchild) FKs", [
        f"CREATE TABLE {S}f1p (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}f1c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}f1p(id))",
        f"CREATE TABLE {S}f1g (id INTEGER PRIMARY KEY, cp INTEGER REFERENCES {S}f1c(pid))",
        f"INSERT INTO {S}f1p VALUES (1)",
        f"INSERT INTO {S}f1c (id, pid) VALUES (1, 1)",
        f"INSERT INTO {S}f1g (id, cp) VALUES (1, 1)",
        f"INSERT INTO {S}f1c (id, pid) VALUES (2, 1)",
        f"DELETE FROM {S}f1c WHERE id = 2",
        f"UPDATE {S}f1c SET pid = 1 WHERE id = 1",
    ], c)

    run("P19b. write to parent whose OWN fk is broken", [
        f"CREATE TABLE {S}g1 (v INTEGER)",
        f"CREATE TABLE {S}g1c (id INTEGER PRIMARY KEY, pv INTEGER REFERENCES {S}g1(v))",
        f"DELETE FROM {S}g1c",
        f"UPDATE {S}g1c SET id = 5",
        f"INSERT INTO {S}g1c VALUES (5, NULL)",
    ], c)

    run("P22. TEXT PK with DEFAULT", [
        f"CREATE TABLE {S}h1 (k TEXT PRIMARY KEY DEFAULT 'x', v INTEGER)",
        f"INSERT INTO {S}h1 (v) VALUES (1)",
        f"INSERT INTO {S}h1 (k, v) VALUES (NULL, 2)",
        f"SELECT * FROM {S}h1",
        f"CREATE TABLE {S}h2 (k TEXT PRIMARY KEY DEFAULT NULL, v INTEGER)",
        f"INSERT INTO {S}h2 (v) VALUES (1)",
        f"SELECT * FROM {S}h2",
    ], c)

    run("P30. REFERENCES table without PK", [
        f"CREATE TABLE {S}i1 (a INTEGER, b INTEGER)",
        f"CREATE TABLE {S}i1c (id INTEGER PRIMARY KEY, x INTEGER REFERENCES {S}i1)",
        f"INSERT INTO {S}i1c (id, x) VALUES (1, 5)",
        f"CREATE TABLE {S}i1d (id INTEGER PRIMARY KEY, x INTEGER REFERENCES {S}i1(a))",
        f"INSERT INTO {S}i1d (id, x) VALUES (1, 5)",
    ], c)

    run("P33/P38. update window + multi-row atomicity", [
        f"CREATE TABLE {S}j1 (id INTEGER PRIMARY KEY, u TEXT UNIQUE)",
        f"INSERT INTO {S}j1 VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        f"UPDATE {S}j1 SET u = 'z' ORDER BY id LIMIT 1",
        f"SELECT * FROM {S}j1 ORDER BY id",
        f"UPDATE {S}j1 SET u = 'a' ORDER BY id DESC LIMIT 1",
        f"SELECT * FROM {S}j1 ORDER BY id",
        f"UPDATE {S}j1 SET u = 'q', id = 2",
        f"SELECT * FROM {S}j1 ORDER BY id",
        f"UPDATE {S}j1 SET u = 'r' WHERE id = 1, u = 's' WHERE id = 3" if False else f"UPDATE {S}j1 SET u = 'r', id = NULL WHERE id = 3",
    ], c)

    run("P39. INSERT DEFAULT VALUES", [
        f"CREATE TABLE {S}k1 (a INTEGER DEFAULT 1)",
        f"INSERT INTO {S}k1 DEFAULT VALUES",
        f"SELECT * FROM {S}k1",
    ], c)

    run("P40. CHECK enforcement", [
        f"CREATE TABLE {S}l1 (a INTEGER, CHECK (a > 0))",
        f"INSERT INTO {S}l1 VALUES (5)",
        f"INSERT INTO {S}l1 VALUES (-1)",
        f"INSERT INTO {S}l1 VALUES (NULL)",
        f"SELECT * FROM {S}l1",
        f"UPDATE {S}l1 SET a = -9",
        f"CREATE TABLE {S}l2 (a INTEGER, CHECK (a > 0), b INTEGER)",
        f"INSERT INTO {S}l2 VALUES (-1, 3)",
    ], c)

    run("P41. duplicate rowid assignment in one batch + big values", [
        f"CREATE TABLE {S}m1 (id INTEGER PRIMARY KEY, s TEXT)",
        f"INSERT INTO {S}m1 (id, s) VALUES (5, 'a'), (NULL, 'b'), (3, 'c'), (NULL, 'd')",
        f"SELECT id, s FROM {S}m1 ORDER BY id",
        f"INSERT INTO {S}m1 (s) VALUES ('e')",
        f"SELECT id, s FROM {S}m1 ORDER BY id",
        f"UPDATE {S}m1 SET s = 'f' WHERE id = 5",
        f"SELECT id, s FROM {S}m1 ORDER BY id",
    ], c)

    run("P42. ON DELETE/UPDATE action clauses", [
        f"CREATE TABLE {S}n1 (id INTEGER PRIMARY KEY)",
        f"CREATE TABLE {S}n2 (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}n1(id) ON DELETE CASCADE ON UPDATE NO ACTION)",
        f"INSERT INTO {S}n1 VALUES (1)",
        f"INSERT INTO {S}n2 VALUES (1, 1)",
        f"DELETE FROM {S}n1",
        f"SELECT * FROM {S}n2",
        f"CREATE TABLE {S}n3 (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}n1(id) MATCH FULL)",
        f"CREATE TABLE {S}n4 (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}n1(id) DEFERRABLE INITIALLY DEFERRED)",
        f"INSERT INTO {S}n4 VALUES (1, 9)",
        f"SELECT * FROM {S}n4",
    ], c)

    run("P43. constraint vs type in UPDATE + ordering", [
        f"CREATE TABLE {S}o1 (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER NOT NULL)",
        f"INSERT INTO {S}o1 VALUES (1, 'a', 1)",
        f"UPDATE {S}o1 SET u = 'zz', n = NULL",
        f"UPDATE {S}o1 SET n = 'txt'",
        f"UPDATE {S}o1 SET u = NULL",
        f"SELECT * FROM {S}o1",
        f"CREATE TABLE {S}o2 (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES {S}o1nosuch(id))",
        f"UPDATE {S}o2 SET pid = 1",
        f"DELETE FROM {S}o2",
    ], c)


if __name__ == "__main__":
    main()
